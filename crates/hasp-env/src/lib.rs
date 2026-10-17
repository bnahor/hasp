//! Environments: run an agent's tools somewhere isolated — a container today,
//! a microVM behind the same [`backend::Backend`] seam later — with declared
//! limits (cpus, memory, processes, network).
//!
//! How it fits together:
//!   * a [`spec::EnvSpec`] says what the environment is;
//!   * a backend starts an [`backend::Instance`] of it, with the workspace at
//!     its host path and the guest agent (this harness's own static Linux
//!     build) installed;
//!   * the guest agent is `harness --mcp-serve` inside the instance; the host
//!     talks to it over the instance's stdio channel;
//!   * the host's `read`/`write`/`edit`/`bash`/`job`/`grep`/`glob`/
//!     `symbol_search` are replaced by proxies with the same names and
//!     schemas, so the model sees an unchanged tool list. Approvals stay on
//!     the host: a proxy asks the host gate before forwarding, and the guest
//!     then enforces the structural rules (relative paths, confinement).
//!
//! The session journal, the provider connection and the agent bus stay on the
//! host; only tool execution moves.

pub mod backend;
pub mod docker;
pub mod guest;
pub mod spec;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hasp_core::{GateDecision, Tool, ToolCtx, ToolOutput, ToolSpec};
use hasp_mcp::{McpClient, McpToolDef};
use serde_json::Value;

pub use spec::{EnvSpec, Network};

/// Tools that run inside an environment; everything else (agent bus, todo,
/// skills, web_fetch, goals) stays on the host.
pub const ENV_TOOLS: &[&str] = &[
    "bash",
    "edit",
    "glob",
    "grep",
    "job",
    "read",
    "symbol_search",
    "write",
];

/// Read-only tools, run concurrently exactly as their local versions are.
const CONCURRENCY_SAFE: &[&str] = &["glob", "grep", "job", "read", "symbol_search"];

/// How long one call may run inside an environment: bash allows 10 minutes,
/// and a background job's `output` returns at once, so this is a backstop.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);

/// How long a fresh instance may take to answer the MCP handshake. A
/// container answers in milliseconds; the first exec into a just-booted
/// microVM (loading the guest over virtio-fs) can take far longer.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A running environment. Dropping the last handle tears the instance down.
pub struct Environment {
    pub spec: EnvSpec,
    instance: Box<dyn backend::Instance>,
    client: McpClient,
}

impl Environment {
    /// Load `name`, start it for `workspace`, and connect its guest agent.
    pub async fn open(name: &str, workspace: &Path) -> Result<Arc<Self>> {
        let spec = spec::load(workspace, name)?;
        Self::start(spec, workspace).await
    }

    pub async fn start(spec: EnvSpec, workspace: &Path) -> Result<Arc<Self>> {
        let backend = backend::backend_for(&spec)?;
        let instance = backend
            .start(&spec, workspace)
            .await
            .with_context(|| format!("starting environment {}", spec.name))?;
        let client = match McpClient::connect_within(
            &spec.name,
            &instance.guest_command(),
            HANDSHAKE_TIMEOUT,
        )
        .await
        {
            Ok(c) => c.with_call_timeout(CALL_TIMEOUT),
            Err(e) => {
                instance.stop();
                return Err(e).context("connecting to the guest agent");
            }
        };
        Ok(Arc::new(Self {
            spec,
            instance,
            client,
        }))
    }

    pub fn id(&self) -> &str {
        self.instance.id()
    }

    /// Run a shell command inside (checks, diagnostics).
    pub async fn exec(&self, command: &str) -> Result<(i32, String)> {
        self.instance.exec(command).await
    }

    /// Proxies for [`ENV_TOOLS`], built from what the guest advertises.
    pub fn tools(self: &Arc<Self>) -> Vec<Box<dyn Tool>> {
        self.client
            .tools
            .iter()
            .filter(|d| ENV_TOOLS.contains(&d.name.as_str()))
            .map(|d| {
                Box::new(EnvTool {
                    env: self.clone(),
                    def: d.clone(),
                }) as Box<dyn Tool>
            })
            .collect()
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        self.instance.stop();
    }
}

/// `host` with its environment tools replaced by `env`'s proxies, name-sorted
/// (tool order is part of the prompt-cache contract).
pub fn with_environment(host: Vec<Box<dyn Tool>>, env: &Arc<Environment>) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = host
        .into_iter()
        .filter(|t| !ENV_TOOLS.contains(&t.spec().name.as_str()))
        .collect();
    tools.extend(env.tools());
    tools.sort_by_key(|t| t.spec().name);
    tools
}

struct EnvTool {
    env: Arc<Environment>,
    def: McpToolDef,
}

impl EnvTool {
    /// The host's approval for this call, before anything reaches the guest.
    fn approve(&self, args: &Value, ctx: &mut ToolCtx<'_>) -> Option<String> {
        let decision = match self.def.name.as_str() {
            "bash" => ctx
                .gate
                .check_bash(args["command"].as_str().unwrap_or_default()),
            "write" | "edit" => ctx
                .gate
                .check_write(args["path"].as_str().unwrap_or_default()),
            _ => GateDecision::Allow,
        };
        match decision {
            GateDecision::Allow => None,
            GateDecision::Deny(reason) => Some(reason),
        }
    }
}

#[async_trait]
impl Tool for EnvTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.def.name.clone(),
            description: self.def.description.clone(),
            input_schema: self.def.input_schema.clone(),
        }
    }

    fn concurrency_safe(&self) -> bool {
        CONCURRENCY_SAFE.contains(&self.def.name.as_str())
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        if let Some(reason) = self.approve(&args, ctx) {
            let what = if self.def.name == "bash" {
                "command"
            } else {
                &self.def.name
            };
            return ToolOutput::error(format!("{what} not approved: {reason}"));
        }
        match self.env.client.call_tool_full(&self.def.name, args).await {
            Ok((content, is_error, images)) => ToolOutput {
                content,
                is_error,
                images,
            },
            Err(e) => ToolOutput::error(format!(
                "environment {} ({}): {e}",
                self.env.spec.name,
                self.env.id()
            )),
        }
    }
}

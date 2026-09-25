//! The seam between "an environment" and "how it is made".
//!
//! A backend turns an [`EnvSpec`] into a running [`Instance`]: something with
//! the workspace available at its host path, the guest agent installed, the
//! spec's limits applied and its setup done. The one thing the rest of the
//! harness needs from an instance is [`Instance::guest_command`] — a command
//! whose stdin/stdout reach the guest agent. Containers get that from
//! `docker exec -i`; a microVM backend would bridge stdio to vsock the same way.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use harness_mcp::McpServerConfig;

use crate::spec::EnvSpec;

#[async_trait]
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Create and start an instance for `spec` with `workspace` available at
    /// the same path inside. Setup commands have run when this returns.
    async fn start(&self, spec: &EnvSpec, workspace: &Path) -> Result<Box<dyn Instance>>;
}

#[async_trait]
pub trait Instance: Send + Sync {
    /// Backend-specific identifier (container id, VM id).
    fn id(&self) -> &str;

    /// The command whose stdio is the guest agent's MCP channel.
    fn guest_command(&self) -> McpServerConfig;

    /// Run a shell command inside, for checks and diagnostics:
    /// `(exit code, combined output)`.
    async fn exec(&self, command: &str) -> Result<(i32, String)>;

    /// Tear the instance down. Synchronous: it runs from `Drop`.
    fn stop(&self);
}

/// The backend a spec names.
pub fn backend_for(spec: &EnvSpec) -> Result<Box<dyn Backend>> {
    match spec.backend.as_str() {
        "docker" => Ok(Box::new(crate::docker::Docker)),
        other => anyhow::bail!(
            "environment {}: unknown backend {other:?} (available: docker)",
            spec.name
        ),
    }
}

//! Agent-bus tools (model-facing): spawn, message, agents. One level deep —
//! child engines get base tools only, so nested fan-out is impossible by
//! construction.

use async_trait::async_trait;
use harness_core::{ChildStatus, SpawnRequest, Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const SPAWN_TOOL_NAME: &str = "spawn";
pub const MESSAGE_TOOL_NAME: &str = "message";
pub const AGENTS_TOOL_NAME: &str = "agents";
pub const SUBAGENT_TOOL_NAME: &str = "subagent";

pub struct SpawnTool;

#[async_trait]
impl Tool for SpawnTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SPAWN_TOOL_NAME.into(),
            description: "Spawn a child agent with a fresh context to work on a task in parallel. \
                Returns an admission handle immediately — never the result. The child's final \
                report arrives as a [child …] notice between your tool calls; use `message` for \
                follow-ups. The child shares nothing with you: put everything it needs in the task."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete, self-contained task for the child"},
                    "name": {"type": "string", "description": "Short alias (optional)"},
                    "mutation": {"type": "boolean", "description": "The child will edit files — it gets an isolated git worktree (default false: read-only in this checkout)"},
                    "environment": {"type": "string", "description": "Run the child's tools in this named environment (.harness/environments/<name>.toml): its own container with declared cpu, memory and network limits. Default: the parent's environment, if any"}
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(bus) = ctx.bus else {
            return ToolOutput::error("no agent bus in this session");
        };
        let Some(task) = args["task"].as_str() else {
            return ToolOutput::error("missing `task`");
        };
        let req = SpawnRequest {
            task: task.to_string(),
            name: args["name"].as_str().map(String::from),
            mutation: args["mutation"].as_bool().unwrap_or(false),
            environment: args["environment"].as_str().map(String::from),
        };
        match bus.spawn_child(req).await {
            Ok(h) => ToolOutput::text(format!(
                "spawned {} (alias from name, session {}, root {}{}) — watch for its [child …] notice",
                h.child_id,
                h.session_id,
                h.root,
                h.environment
                    .as_deref()
                    .map(|e| format!(", tools in environment {e}"))
                    .unwrap_or_default()
            )),
            Err(e) => ToolOutput::error(e),
        }
    }
}

pub struct MessageTool;

#[async_trait]
impl Tool for MessageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: MESSAGE_TOOL_NAME.into(),
            description: "Send a message to a child agent. Running children receive it at their \
                next tool boundary (queued); idle children run a new turn when trigger is true. \
                Never blocks on the child. Replies arrive as [child … replied] notices."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "child_id": {"type": "string"},
                    "text": {"type": "string"},
                    "trigger": {"type": "boolean", "description": "Start a new turn on an idle child (default true)"}
                },
                "required": ["child_id", "text"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(bus) = ctx.bus else {
            return ToolOutput::error("no agent bus in this session");
        };
        let (Some(child_id), Some(text)) = (args["child_id"].as_str(), args["text"].as_str())
        else {
            return ToolOutput::error("missing `child_id` or `text`");
        };
        let trigger = args["trigger"].as_bool().unwrap_or(true);
        match bus.message_child(child_id, text, trigger).await {
            Ok(r) => ToolOutput::text(format!(
                "{child_id}: {} — reply arrives as a notice",
                r.label()
            )),
            Err(e) => ToolOutput::error(e),
        }
    }
}

pub struct AgentsTool;

#[async_trait]
impl Tool for AgentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: AGENTS_TOOL_NAME.into(),
            description: "List child agents of this session: id, name, status (running/completed/failed), session."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(&self, _args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(bus) = ctx.bus else {
            return ToolOutput::error("no agent bus in this session");
        };
        let list = bus.list_children().await;
        if list.is_empty() {
            return ToolOutput::text("no child agents");
        }
        let lines: Vec<String> = list
            .iter()
            .map(|c| {
                let head = format!(
                    "{}  {:<12} {:<9} {}  {}",
                    c.child_id,
                    c.name,
                    c.status.label(),
                    if c.mutation { "worktree" } else { "shared" },
                    c.session_id
                );
                let head = match &c.environment {
                    Some(e) => format!("{head}  env {e}"),
                    None => head,
                };
                match &c.report {
                    Some(r) => format!("{head}\n    report: {r}"),
                    None => head,
                }
            })
            .collect();
        ToolOutput::text(lines.join("\n"))
    }
}

/// Blocking sibling of `spawn`: runs one child and returns its answer.
///
/// `spawn` is the better default — it keeps the parent working while the child
/// runs — but some questions have to be answered before the next step, and
/// asking the model to poll for that produced exactly the filesystem-hunting
/// behaviour the notice inbox now prevents. This is the direct form.
pub struct SubagentTool {
    /// How long to wait before giving up and falling back to the notice.
    pub wait: std::time::Duration,
}

impl Default for SubagentTool {
    fn default() -> Self {
        Self {
            wait: std::time::Duration::from_secs(300),
        }
    }
}

const SUBAGENT_POLL_MS: u64 = 200;

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SUBAGENT_TOOL_NAME.into(),
            description: "Run one child agent on a task and wait for its answer. Blocks this turn,                 so prefer `spawn` for work that can happen in parallel; use this when the next step                 depends on the result. Isolation rules are the same as `spawn`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete, self-contained task"},
                    "name": {"type": "string", "description": "Short alias (optional)"},
                    "mutation": {"type": "boolean", "description": "The child will edit files — it gets an isolated git worktree (default false)"},
                    "environment": {"type": "string", "description": "Run the child's tools in this named environment (default: the parent's, if any)"}
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(bus) = ctx.bus else {
            return ToolOutput::error("subagents are unavailable in this session");
        };
        let Some(task) = args["task"].as_str() else {
            return ToolOutput::error("missing `task`");
        };
        let handle = match bus
            .spawn_child(SpawnRequest {
                task: task.to_string(),
                name: args["name"].as_str().map(str::to_string),
                mutation: args["mutation"].as_bool().unwrap_or(false),
                environment: args["environment"].as_str().map(str::to_string),
            })
            .await
        {
            Ok(handle) => handle,
            Err(e) => return ToolOutput::error(format!("could not spawn a child: {e}")),
        };

        let deadline = std::time::Instant::now() + self.wait;
        loop {
            if let Some(info) = bus
                .list_children()
                .await
                .into_iter()
                .find(|c| c.child_id == handle.child_id)
            {
                match &info.status {
                    ChildStatus::Running => {}
                    ChildStatus::Completed => {
                        let report = info.report.clone().unwrap_or_default();
                        return ToolOutput::text(format!(
                            "{} finished
{report}",
                            handle.child_id
                        ));
                    }
                    ChildStatus::Failed(why) => {
                        let report = info.report.clone().unwrap_or_default();
                        return ToolOutput::error(format!(
                            "{} failed: {why}
{report}",
                            handle.child_id
                        ));
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return ToolOutput::text(format!(
                    "{} is still running after {}s; its report will arrive as a [child …] notice,                      so carry on with other work",
                    handle.child_id,
                    self.wait.as_secs()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(SUBAGENT_POLL_MS)).await;
        }
    }
}

/// The model's only way to complete a goal. The engine intercepts this name
/// before tool dispatch (goal state lives there), so `execute` is unreachable.
pub struct GoalCompleteTool;

#[async_trait]
impl Tool for GoalCompleteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: harness_core::engine::GOAL_COMPLETE_TOOL.into(),
            description: "Mark the ACTIVE goal complete — it ends the continuation loop. Only \
                call it when a goal is active (the harness tells you so in a continuation \
                message) and its success criteria are actually met. Never call it to signal that \
                an ordinary request is finished. No arguments."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(&self, _args: Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        ToolOutput::error("goal_complete is handled by the engine")
    }
}

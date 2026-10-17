//! Built-in tools. `builtins_base()` (read/write/edit/bash, search, todo) is what child
//! agents get — one level deep; `builtins()` adds the agent-bus tools for
//! root sessions. Name-sorted with stable schemas: tool order is part of the
//! cache contract.

mod agent;
mod bash;
pub mod diff;
mod fs;
pub mod jobs;
pub mod sandbox;
pub mod recall;
mod search;
mod skill;
mod symbols;
mod todo;
mod web;

pub use agent::{AgentsTool, GoalCompleteTool, MessageTool, SpawnTool, SubagentTool};
pub use bash::{BashTool, escapes_cwd, is_simple, read_only_invocation, seed_allowlist, split_words};
pub use fs::{EditTool, ReadTool, WriteTool};
pub use jobs::{JobTable, JobTool};
pub use recall::{MemoryTool, RecallTool, WorkflowProposeTool};
pub use search::{GlobTool, GrepTool};
pub use skill::SkillTool;
pub use symbols::SymbolSearchTool;
pub use todo::TodoTool;
pub use web::WebFetchTool;

use hasp_core::Tool;

pub fn builtins() -> Vec<Box<dyn Tool>> {
    let mut tools = builtins_base();
    tools.push(Box::new(agent::SpawnTool));
    tools.push(Box::new(agent::MessageTool));
    tools.push(Box::new(agent::AgentsTool));
    tools.push(Box::new(agent::GoalCompleteTool));
    tools.push(Box::new(agent::SubagentTool::default()));
    tools.push(Box::new(RecallTool));
    tools.push(Box::new(MemoryTool));
    tools.push(Box::new(WorkflowProposeTool));
    tools.sort_by_key(|t| t.spec().name);
    tools
}

/// `builtins()` plus what this project and user have installed (skills).
pub fn builtins_for(project: &std::path::Path) -> Vec<Box<dyn Tool>> {
    let mut tools = builtins();
    if let Some(skill) = SkillTool::discover(project) {
        tools.push(Box::new(skill));
    }
    tools.sort_by_key(|t| t.spec().name);
    tools
}

pub fn builtins_base() -> Vec<Box<dyn Tool>> {
    // One job table per tool set: `bash` starts jobs, `job` manages them, and
    // dropping the tools (the session) stops whatever is still running.
    let jobs = std::sync::Arc::new(JobTable::default());
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(BashTool::with_jobs(jobs.clone())),
        Box::new(JobTool::new(jobs)),
        // Read-only extras: children and the MCP server get these too.
        Box::new(WebFetchTool),
        Box::new(SymbolSearchTool),
        Box::new(GrepTool),
        Box::new(GlobTool),
        Box::new(TodoTool),
    ];
    tools.sort_by_key(|t| t.spec().name);
    tools
}

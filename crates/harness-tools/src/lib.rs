//! Built-in tools. `builtins_base()` (read/write/edit/bash) is what child
//! agents get — one level deep; `builtins()` adds the agent-bus tools for
//! root sessions. Name-sorted with stable schemas: tool order is part of the
//! cache contract.

mod agent;
mod bash;
pub mod diff;
mod fs;
mod symbols;
mod web;

pub use agent::{AgentsTool, GoalCompleteTool, MessageTool, SpawnTool, SubagentTool};
pub use bash::{BashTool, escapes_cwd, is_simple, seed_allowlist};
pub use fs::{EditTool, ReadTool, WriteTool};
pub use symbols::SymbolSearchTool;
pub use web::WebFetchTool;

use harness_core::Tool;

pub fn builtins() -> Vec<Box<dyn Tool>> {
    let mut tools = builtins_base();
    tools.push(Box::new(agent::SpawnTool));
    tools.push(Box::new(agent::MessageTool));
    tools.push(Box::new(agent::AgentsTool));
    tools.push(Box::new(agent::GoalCompleteTool));
    tools.push(Box::new(agent::SubagentTool::default()));
    tools.sort_by_key(|t| t.spec().name);
    tools
}

pub fn builtins_base() -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(BashTool),
        // Read-only extras: children and the MCP server get these too.
        Box::new(WebFetchTool),
        Box::new(SymbolSearchTool),
    ];
    tools.sort_by_key(|t| t.spec().name);
    tools
}

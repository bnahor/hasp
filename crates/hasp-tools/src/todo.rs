//! `todo`: the model's working plan for a multi-step task. Stateless on
//! purpose — each call carries the whole list, so the latest list is simply
//! the latest call in the transcript: it survives resume, branch and
//! compaction without any state of its own, and the UI shows it as the tool's
//! output.

use async_trait::async_trait;
use hasp_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const TODO_TOOL_NAME: &str = "todo";

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TODO_TOOL_NAME.into(),
            description: "Maintain a checklist for work with several steps. Send the complete \
                          list every time (it replaces the previous one). Keep exactly one item \
                          in_progress while working, and mark items completed as soon as they \
                          are done — not in batches at the end."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(items) = args["todos"].as_array() else {
            return ToolOutput::error("missing `todos` array");
        };
        let mut out = String::new();
        let (mut done, mut active) = (0, 0);
        for item in items {
            let Some(content) = item["content"].as_str() else {
                return ToolOutput::error("every todo needs `content`");
            };
            let mark = match item["status"].as_str() {
                Some("completed") => {
                    done += 1;
                    "[x]"
                }
                Some("in_progress") => {
                    active += 1;
                    "[>]"
                }
                Some("pending") => "[ ]",
                other => {
                    return ToolOutput::error(format!(
                        "status must be pending, in_progress or completed (got {other:?})"
                    ));
                }
            };
            out.push_str(&format!("{mark} {content}\n"));
        }
        out.push_str(&format!("{done}/{} completed", items.len()));
        if active > 1 {
            out.push_str(" — note: more than one item is in_progress; finish one first");
        }
        ToolOutput::text(out)
    }
}

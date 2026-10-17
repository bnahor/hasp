//! `skill`: load a skill's instructions on demand. The tool description names
//! each installed skill with its one-line description — that is all that sits
//! in context until the model decides a skill applies and loads it.
//!
//! Skills in the home directory live outside the workspace, where `read`
//! cannot go; `file` reads a supporting file inside the skill's own directory,
//! under the same rules (relative, no `..`, no symlink escape).

use std::path::{Component, Path};

use async_trait::async_trait;
use hasp_core::extensions::{Skill, split_front_matter};
use hasp_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub const SKILL_TOOL_NAME: &str = "skill";
const MAX_FILE_BYTES: u64 = 256 * 1024;

pub struct SkillTool {
    skills: Vec<Skill>,
}

impl SkillTool {
    /// `None` when there are no skills: no tool beats an empty one.
    pub fn discover(project: &Path) -> Option<Self> {
        let skills = hasp_core::extensions::skills(project);
        (!skills.is_empty()).then_some(Self { skills })
    }

    pub fn with(skills: Vec<Skill>) -> Self {
        Self { skills }
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(dir)
                && rel != Path::new("SKILL.md")
            {
                out.push(rel.to_string_lossy().into_owned());
            }
            if out.len() >= 100 {
                return out;
            }
        }
    }
    out.sort();
    out
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        let mut description = String::from(
            "Load a skill: expert instructions for a kind of task. When a task matches a skill \
             below, load it before starting and follow it. `file` reads one of the skill's \
             supporting files.\n\nAvailable skills:",
        );
        for s in &self.skills {
            description.push_str(&format!("\n- {}: {}", s.name, s.description));
        }
        ToolSpec {
            name: SKILL_TOOL_NAME.into(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "enum": self.skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>()},
                    "file": {"type": "string", "description": "Optional path inside the skill's directory"}
                },
                "required": ["name"]
            }),
        }
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(name) = args["name"].as_str() else {
            return ToolOutput::error("missing `name`");
        };
        let Some(skill) = self.skills.iter().find(|s| s.name == name) else {
            return ToolOutput::error(format!("no skill named {name}"));
        };
        if let Some(file) = args["file"].as_str().filter(|f| !f.is_empty()) {
            let rel = Path::new(file);
            if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
                return ToolOutput::error("`file` must be a relative path inside the skill");
            }
            let (Ok(base), Ok(target)) =
                (skill.dir.canonicalize(), skill.dir.join(rel).canonicalize())
            else {
                return ToolOutput::error(format!("{file} not found in skill {name}"));
            };
            if !target.starts_with(&base) {
                return ToolOutput::error("`file` escapes the skill directory");
            }
            if std::fs::metadata(&target).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
                return ToolOutput::error(format!("{file} is larger than 256 KB"));
            }
            return match std::fs::read(&target) {
                Ok(b) => ToolOutput::text(String::from_utf8_lossy(&b).into_owned()),
                Err(e) => ToolOutput::error(e.to_string()),
            };
        }
        let text = match std::fs::read_to_string(skill.dir.join("SKILL.md")) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let (_, body) = split_front_matter(&text);
        let mut out = format!("# Skill: {name}\n\n{}", body.trim());
        let files = listing(&skill.dir);
        if !files.is_empty() {
            out.push_str(&format!(
                "\n\n---\nSupporting files (load with the skill tool's `file`): {}",
                files.join(", ")
            ));
        }
        ToolOutput::text(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hasp_core::{ArtifactStore, RefuseGate};
    use hasp_workspace::Workspace;

    #[tokio::test]
    async fn loads_body_lists_files_and_confines_file_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills/pdf");
        std::fs::create_dir_all(dir.join("ref")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: PDFs\n---\nUse pypdf.\n",
        )
        .unwrap();
        std::fs::write(dir.join("ref/forms.md"), "fill forms like so").unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "no").unwrap();
        let tool = SkillTool::with(vec![Skill {
            name: "pdf".into(),
            description: "PDFs".into(),
            dir: dir.clone(),
        }]);
        assert!(tool.spec().description.contains("- pdf: PDFs"));

        let ws = Workspace::resolve(tmp.path()).unwrap();
        let artifacts = ArtifactStore::new(tmp.path().join("a"), "t".into()).unwrap();
        let mut gate = RefuseGate;
        let mut ctx = ToolCtx {
            workspace: &ws,
            gate: &mut gate,
            artifacts: &artifacts,
            bus: None,
        };

        let out = tool.execute(json!({"name": "pdf"}), &mut ctx).await;
        assert!(
            out.content.starts_with("# Skill: pdf\n\nUse pypdf."),
            "{}",
            out.content
        );
        assert!(out.content.contains("ref/forms.md"));

        let out = tool
            .execute(json!({"name": "pdf", "file": "ref/forms.md"}), &mut ctx)
            .await;
        assert_eq!(out.content, "fill forms like so");
        let out = tool
            .execute(json!({"name": "pdf", "file": "../../secret.txt"}), &mut ctx)
            .await;
        assert!(out.is_error);
    }
}

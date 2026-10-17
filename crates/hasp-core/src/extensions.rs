//! User extension points that need no code: prompt templates run as slash
//! commands, and skills the model loads on demand.
//!
//! Both are plain Markdown and deliberately compatible with the layouts other
//! agents already use, so an existing collection works unchanged:
//!
//! - commands: `<dir>/<name>.md` (or `<dir>/<ns>/<name>.md` → `/ns:name`) in
//!   `.harness/commands` and `.claude/commands` of the project, then of the
//!   home directory. `$ARGUMENTS` is replaced by everything after the command
//!   name and `$1`…`$9` by its words.
//! - skills: `<dir>/<name>/SKILL.md` with `name:` / `description:` front matter
//!   in `.harness/skills` and `.claude/skills` (project, then home). Only the
//!   names and descriptions are ever in context; the body is loaded when the
//!   model asks for it (progressive disclosure).
//!
//! Earlier locations win on a name clash: project over home, `.harness` over
//! `.claude`. Discovery happens once when a session's engine is built, so the
//! tool list stays byte-stable for the whole session.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct CommandTemplate {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// Search roots in precedence order.
fn roots(project: &Path, kind: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = hasp_workspace::state_dirs(project)
        .into_iter()
        .map(|d| d.join(kind))
        .collect();
    out.push(project.join(".claude").join(kind));
    if let Ok(home) = hasp_workspace::hasp_home() {
        out.push(home.join(kind));
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".claude").join(kind));
    }
    out
}

/// `(front matter pairs, body)`. Front matter is the simple `key: value`
/// subset — enough for `name`, `description` and friends, no YAML dependency.
pub fn split_front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (Vec::new(), text);
    };
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), text);
    };
    let pairs = rest[..end]
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let v = v.trim().trim_matches('"').trim_matches('\'');
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect();
    let body = &rest[end + 4..];
    let body = body.split_once('\n').map(|(_, b)| b).unwrap_or("");
    (pairs, body)
}

fn front(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

fn first_line(body: &str) -> String {
    let line = body
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    line.chars().take(120).collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

fn read_command(path: &Path, name: String) -> Option<CommandTemplate> {
    let text = std::fs::read_to_string(path).ok()?;
    let (pairs, body) = split_front_matter(&text);
    Some(CommandTemplate {
        description: front(&pairs, "description").unwrap_or_else(|| first_line(body)),
        body: body.trim().to_string(),
        name,
        path: path.to_path_buf(),
    })
}

fn md_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Every command template visible from `project`, name-sorted.
pub fn commands(project: &Path) -> Vec<CommandTemplate> {
    let mut out: Vec<CommandTemplate> = Vec::new();
    for root in roots(project, "commands") {
        for path in md_files(&root) {
            let found: Vec<(PathBuf, String)> = if path.is_dir() {
                let ns = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                md_files(&path)
                    .into_iter()
                    .filter(|p| p.extension().is_some_and(|e| e == "md"))
                    .map(|p| {
                        let stem = p
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        (p, format!("{ns}:{stem}"))
                    })
                    .collect()
            } else if path.extension().is_some_and(|e| e == "md") {
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                vec![(path.clone(), stem)]
            } else {
                Vec::new()
            };
            for (p, name) in found {
                if valid_name(&name)
                    && !out.iter().any(|c| c.name == name)
                    && let Some(c) = read_command(&p, name)
                {
                    out.push(c);
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Fill a template: `$ARGUMENTS` gets the whole argument string, `$1`…`$9`
/// its words. A template with no placeholder still receives the arguments,
/// appended, so nothing typed is silently dropped.
pub fn render(body: &str, args: &str) -> String {
    let words: Vec<&str> = args.split_whitespace().collect();
    let has_placeholder =
        body.contains("$ARGUMENTS") || (1..=9).any(|i| body.contains(&format!("${i}")));
    let mut out = body.replace("$ARGUMENTS", args);
    for i in (1..=9).rev() {
        out = out.replace(&format!("${i}"), words.get(i - 1).copied().unwrap_or(""));
    }
    if !has_placeholder && !args.trim().is_empty() {
        out.push_str("\n\nArguments: ");
        out.push_str(args.trim());
    }
    out
}

/// `/name args` → the rendered prompt, when `name` is a template. `None` for
/// anything else, so built-in commands keep their meaning.
pub fn expand_command(project: &Path, input: &str) -> Option<String> {
    let cmd = input.trim().strip_prefix('/')?;
    let (name, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
    commands(project)
        .into_iter()
        .find(|c| c.name == name)
        .map(|c| render(&c.body, args.trim()))
}

/// Every skill visible from `project`, name-sorted.
pub fn skills(project: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in roots(project, "skills") {
        for dir in md_files(&root) {
            let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else {
                continue;
            };
            let (pairs, body) = split_front_matter(&text);
            let name = front(&pairs, "name")
                .unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into());
            if !valid_name(&name) || out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Skill {
                description: front(&pairs, "description").unwrap_or_else(|| first_line(body)),
                name,
                dir,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_body_split() {
        let (pairs, body) =
            split_front_matter("---\nname: x\ndescription: \"does y\"\n---\n# Body\ntext\n");
        assert_eq!(front(&pairs, "name").as_deref(), Some("x"));
        assert_eq!(front(&pairs, "description").as_deref(), Some("does y"));
        assert_eq!(body, "# Body\ntext\n");
        let (pairs, body) = split_front_matter("plain\n");
        assert!(pairs.is_empty());
        assert_eq!(body, "plain\n");
    }

    #[test]
    fn render_fills_placeholders_or_appends() {
        assert_eq!(
            render("Review $ARGUMENTS now", "src/a.rs b"),
            "Review src/a.rs b now"
        );
        assert_eq!(render("from $1 to $2", "a b c"), "from a to b");
        assert_eq!(render("Fix it", "the bug"), "Fix it\n\nArguments: the bug");
        assert_eq!(render("Fix it", ""), "Fix it");
    }

    #[test]
    fn project_commands_and_skills_are_discovered_with_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        std::fs::create_dir_all(p.join(".harness/commands/git")).unwrap();
        std::fs::create_dir_all(p.join(".claude/commands")).unwrap();
        std::fs::write(
            p.join(".harness/commands/review.md"),
            "---\ndescription: review a file\n---\nReview $ARGUMENTS carefully.",
        )
        .unwrap();
        std::fs::write(p.join(".claude/commands/review.md"), "shadowed").unwrap();
        std::fs::write(
            p.join(".claude/commands/plan.md"),
            "# Make a plan\nPlan it.",
        )
        .unwrap();
        std::fs::write(p.join(".harness/commands/git/pr.md"), "Open a PR").unwrap();

        let names: Vec<String> = commands(p).into_iter().map(|c| c.name).collect();
        for want in ["git:pr", "plan", "review"] {
            assert!(names.contains(&want.to_string()), "{names:?}");
        }
        let review = commands(p)
            .into_iter()
            .find(|c| c.name == "review")
            .unwrap();
        assert_eq!(review.description, "review a file");
        let plan = commands(p).into_iter().find(|c| c.name == "plan").unwrap();
        assert_eq!(plan.description, "Make a plan");
        assert_eq!(
            expand_command(p, "/review src/lib.rs").as_deref(),
            Some("Review src/lib.rs carefully.")
        );
        assert_eq!(expand_command(p, "/nope x"), None);
        assert_eq!(expand_command(p, "not a command"), None);

        std::fs::create_dir_all(p.join(".claude/skills/pdf")).unwrap();
        std::fs::write(
            p.join(".claude/skills/pdf/SKILL.md"),
            "---\nname: pdf\ndescription: Work with PDF files\n---\nUse pypdf.",
        )
        .unwrap();
        let skill = skills(p).into_iter().find(|s| s.name == "pdf").unwrap();
        assert_eq!(skill.description, "Work with PDF files");
        assert!(skill.dir.ends_with(".claude/skills/pdf"));
    }
}

//! A small syntax highlighter for fenced code blocks.
//!
//! Dependency-free by design: a full highlighter (syntect and friends) carries
//! megabytes of grammar data and a regex engine to colour a terminal
//! transcript. What reads best at this size is comments, strings, numbers and
//! keywords — so that is what this does, per language, with block-comment state
//! carried across lines.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

#[derive(Clone, Copy)]
struct Lang {
    /// Markers that comment out the rest of a line.
    line_comments: &'static [&'static str],
    /// Open/close pair for block comments.
    block: Option<(&'static str, &'static str)>,
    keywords: &'static [&'static str],
}

const RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const PY: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "self", "True", "try",
    "while", "with", "yield",
];
const JS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "while",
    "yield",
];
const GO: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];
const SH: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in",
    "local", "return", "then", "while",
];
const C_LIKE: &[&str] = &[
    "break", "case", "class", "const", "continue", "default", "do", "else", "enum", "false", "for",
    "if", "int", "new", "null", "private", "public", "return", "static", "struct", "switch",
    "this", "true", "void", "while",
];
const SQL: &[&str] = &[
    "AND", "AS", "BY", "CREATE", "DELETE", "DROP", "FROM", "GROUP", "HAVING", "INSERT", "INTO",
    "JOIN", "LIMIT", "NOT", "NULL", "ON", "OR", "ORDER", "SELECT", "SET", "TABLE", "UPDATE",
    "VALUES", "WHERE",
];

fn lang_for(name: &str) -> Lang {
    let n = name.trim().to_ascii_lowercase();
    let n = n.split([' ', ',']).next().unwrap_or("");
    match n {
        "rust" | "rs" => Lang {
            line_comments: &["//"],
            block: Some(("/*", "*/")),
            keywords: RUST,
        },
        "python" | "py" => Lang {
            line_comments: &["#"],
            block: None,
            keywords: PY,
        },
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "jsonc" => Lang {
            line_comments: &["//"],
            block: Some(("/*", "*/")),
            keywords: JS,
        },
        "go" => Lang {
            line_comments: &["//"],
            block: Some(("/*", "*/")),
            keywords: GO,
        },
        "sh" | "bash" | "zsh" | "shell" | "console" => Lang {
            line_comments: &["#"],
            block: None,
            keywords: SH,
        },
        "c" | "h" | "cc" | "cpp" | "hpp" | "java" | "kt" | "kotlin" | "cs" | "swift" | "zig" => {
            Lang {
                line_comments: &["//"],
                block: Some(("/*", "*/")),
                keywords: C_LIKE,
            }
        }
        "sql" => Lang {
            line_comments: &["--"],
            block: Some(("/*", "*/")),
            keywords: SQL,
        },
        "toml" | "ini" | "yaml" | "yml" => Lang {
            line_comments: &["#"],
            block: None,
            keywords: &[],
        },
        // Unknown fence: still worth colouring comments/strings/numbers.
        _ => Lang {
            line_comments: &["//", "#", "--"],
            block: Some(("/*", "*/")),
            keywords: &[],
        },
    }
}

pub struct Palette {
    pub base: Style,
    pub keyword: Style,
    pub string: Style,
    pub number: Style,
    pub comment: Style,
}

impl Palette {
    pub fn new(base: Style) -> Self {
        let t = crate::theme::current();
        Self {
            base,
            keyword: Style::new().fg(t.keyword),
            string: Style::new().fg(t.string),
            number: Style::new().fg(t.number),
            comment: Style::new().fg(t.comment).add_modifier(Modifier::ITALIC),
        }
    }
}

/// Incremental highlighter for a streaming block: state (block comments) carries
/// from line to line.
pub struct Highlighter {
    lang: Lang,
    in_block: Option<&'static str>,
    palette: Palette,
}

impl Highlighter {
    pub fn new(lang: &str, base: Style) -> Self {
        Self {
            lang: lang_for(lang),
            in_block: None,
            palette: Palette::new(base),
        }
    }

    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        line_spans(text, &self.lang, &self.palette, &mut self.in_block)
    }
}

/// Highlight a whole block at once. The renderer streams line by line through
/// `Highlighter`, so this is the one-shot form (used by the tests).
#[cfg(test)]
pub fn block(lines: &[String], lang: &str, base: Style) -> Vec<Vec<Span<'static>>> {
    let l = lang_for(lang);
    let p = Palette::new(base);
    let mut in_block: Option<&'static str> = None;
    lines
        .iter()
        .map(|line| line_spans(line, &l, &p, &mut in_block))
        .collect()
}

fn line_spans(
    line: &str,
    l: &Lang,
    p: &Palette,
    in_block: &mut Option<&'static str>,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                out.push(Span::styled(std::mem::take(&mut plain), p.base));
            }
        };
    }
    macro_rules! push {
        ($text:expr, $style:expr) => {{
            flush!();
            out.push(Span::styled($text.to_string(), $style));
        }};
    }

    while i < bytes.len() {
        let rest = &line[i..];

        // Inside a block comment: look for the terminator.
        if let Some(close) = *in_block {
            match rest.find(close) {
                Some(end) => {
                    push!(&rest[..end + close.len()], p.comment);
                    i += end + close.len();
                    *in_block = None;
                }
                None => {
                    push!(rest, p.comment);
                    i = bytes.len();
                }
            }
            continue;
        }

        if let Some((open, close)) = l.block
            && rest.starts_with(open)
        {
            match rest[open.len()..].find(close) {
                Some(end) => {
                    let upto = open.len() + end + close.len();
                    push!(&rest[..upto], p.comment);
                    i += upto;
                }
                None => {
                    push!(rest, p.comment);
                    *in_block = Some(close);
                    i = bytes.len();
                }
            }
            continue;
        }

        if l.line_comments.iter().any(|m| rest.starts_with(m)) {
            push!(rest, p.comment);
            break;
        }

        let ch = rest.chars().next().expect("non-empty rest");
        // Strings, with backslash escapes.
        if ch == '"' || ch == '\'' || ch == '`' {
            let mut end = None;
            let mut escaped = false;
            for (off, c) in rest.char_indices().skip(1) {
                if escaped {
                    escaped = false;
                    continue;
                }
                match c {
                    '\\' => escaped = true,
                    c if c == ch => {
                        end = Some(off + c.len_utf8());
                        break;
                    }
                    _ => {}
                }
            }
            match end {
                Some(end) => {
                    push!(&rest[..end], p.string);
                    i += end;
                }
                None => {
                    push!(rest, p.string);
                    i = bytes.len();
                }
            }
            continue;
        }

        // Numbers (including 0x/0b/underscores/exponents, roughly).
        if ch.is_ascii_digit() {
            let len = rest
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
                .last()
                .map(|(off, c)| off + c.len_utf8())
                .unwrap_or(1);
            push!(&rest[..len], p.number);
            i += len;
            continue;
        }

        // Identifiers: keywords get a colour, everything else stays base.
        if ch.is_alphabetic() || ch == '_' {
            let len = rest
                .char_indices()
                .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                .last()
                .map(|(off, c)| off + c.len_utf8())
                .unwrap_or(1);
            let word = &rest[..len];
            if l.keywords.contains(&word) {
                push!(word, p.keyword);
            } else {
                plain.push_str(word);
            }
            i += len;
            continue;
        }

        plain.push(ch);
        i += ch.len_utf8();
    }

    flush!();
    if out.is_empty() {
        out.push(Span::styled(String::new(), p.base));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn spans(line: &str, lang: &str) -> Vec<(String, Style)> {
        let lines = vec![line.to_string()];
        block(&lines, lang, Style::new())[0]
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    #[test]
    fn keywords_strings_numbers_and_comments_are_distinguished() {
        let p = Palette::new(Style::new());
        let s = spans(r#"let x = "hi"; let n = 42; // note"#, "rust");
        let find = |text: &str| s.iter().find(|(t, _)| t == text).map(|(_, st)| *st);
        assert_eq!(find("let"), Some(p.keyword));
        assert_eq!(find("\"hi\""), Some(p.string));
        assert_eq!(find("42"), Some(p.number));
        assert_eq!(find("// note"), Some(p.comment));
        // Text still round-trips exactly.
        assert_eq!(
            s.iter().map(|(t, _)| t.as_str()).collect::<String>(),
            r#"let x = "hi"; let n = 42; // note"#
        );
    }

    #[test]
    fn python_comments_and_keywords() {
        let p = Palette::new(Style::new());
        let s = spans("def f(): return None  # why", "python");
        assert_eq!(s.iter().find(|(t, _)| t == "def").unwrap().1, p.keyword);
        assert_eq!(s.iter().find(|(t, _)| t == "None").unwrap().1, p.keyword);
        assert_eq!(
            s.iter().find(|(t, _)| t.starts_with('#')).unwrap().1,
            p.comment
        );
    }

    #[test]
    fn block_comments_span_lines() {
        let p = Palette::new(Style::new());
        let code = vec![
            "let a = 1; /* start".to_string(),
            "still comment".to_string(),
            "end */ let b = 2;".to_string(),
        ];
        let out = block(&code, "rust", Style::new());
        assert_eq!(out[0].last().unwrap().style, p.comment);
        assert_eq!(out[1][0].style, p.comment);
        // The comment ends mid-line; the rest is code again.
        assert!(
            out[2]
                .iter()
                .any(|s| s.content == "end */" && s.style == p.comment)
        );
        assert!(
            out[2]
                .iter()
                .any(|s| s.content == "let" && s.style == p.keyword)
        );
    }

    #[test]
    fn escapes_do_not_end_a_string_early() {
        let p = Palette::new(Style::new());
        let s = spans(r#"let s = "a\"b"; "#, "rust");
        assert_eq!(
            s.iter().find(|(t, _)| t == r#""a\"b""#).unwrap().1,
            p.string
        );
    }

    #[test]
    fn unknown_and_empty_languages_are_safe() {
        let s = spans("plain text with \"quotes\" and 12", "brainfuck");
        assert!(s.iter().any(|(t, _)| t == "\"quotes\""));
        assert_eq!(block(&["".to_string()], "", Style::new()).len(), 1);
        assert!(block(&[], "rust", Style::new()).is_empty());
    }
}

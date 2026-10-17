//! Markdown → styled terminal lines.
//!
//! Deliberately small: the subset a coding agent actually emits — headings,
//! bold/italic, inline code, fenced code blocks with a language label, bullets,
//! numbered lists, quotes, rules and links. Wrapping is left to ratatui, so
//! spans stay whole and the viewport decides where lines break.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Decorations the renderer needs beyond the caller's base (assistant) style.
struct Styles {
    base: Style,
    dim: Style,
    heading: Style,
    heading1: Style,
    code: Style,
    inline_code: Style,
    quote: Style,
}

impl Styles {
    fn new(base: Style) -> Self {
        let t = crate::theme::current();
        Self {
            base,
            dim: Style::new().fg(Color::DarkGray),
            heading: base.add_modifier(Modifier::BOLD),
            heading1: Style::new().fg(t.heading1).add_modifier(Modifier::BOLD),
            code: Style::new().fg(t.code),
            inline_code: Style::new().fg(t.inline_code),
            quote: Style::new().fg(t.quote).add_modifier(Modifier::ITALIC),
        }
    }
}

/// `ascii` swaps the box/​bullet glyphs for terminals whose font renders them as
/// tofu or substitutes CJK glyphs.
pub fn render(md: &str, base: Style, ascii: bool) -> Vec<Line<'static>> {
    let st = Styles::new(base);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fence: Option<String> = None;
    let mut hl: Option<crate::highlight::Highlighter> = None;
    let mut blank_run = false;

    for raw in md.lines() {
        let trimmed = raw.trim_start();

        // Inside a fence everything is literal until the closing marker.
        if let Some(lang) = &fence {
            if let Some(rest) = trimmed.strip_prefix("```") {
                let lang = if rest.trim().is_empty() {
                    lang.clone()
                } else {
                    rest.trim().to_string()
                };
                let _ = lang;
                out.push(Line::from(Span::styled(
                    if ascii {
                        "`".to_string()
                    } else {
                        "└".to_string()
                    },
                    st.dim,
                )));
                fence = None;
                hl = None;
            } else {
                out.push(Line::from({
                    let mut line: Vec<Span<'static>> = vec![Span::styled(
                        if ascii {
                            "| ".to_string()
                        } else {
                            "│ ".to_string()
                        },
                        st.dim,
                    )];
                    match hl.as_mut() {
                        Some(highlighter) => line.extend(highlighter.line(raw)),
                        None => line.push(Span::styled(raw.to_string(), st.code)),
                    }
                    line
                }));
            }
            continue;
        }

        if let Some(lang) = trimmed.strip_prefix("```") {
            let label = lang.trim();
            let title = if label.is_empty() {
                "code".to_string()
            } else {
                label.to_string()
            };
            out.push(Line::from(vec![
                Span::styled(if ascii { "-- " } else { "┌ " }, st.dim),
                Span::styled(title, st.dim.add_modifier(Modifier::BOLD)),
            ]));
            fence = Some(label.to_string());
            hl = Some(crate::highlight::Highlighter::new(label, st.code));
            blank_run = false;
            continue;
        }

        if trimmed.is_empty() {
            if !blank_run && !out.is_empty() {
                out.push(Line::default());
            }
            blank_run = true;
            continue;
        }
        blank_run = false;

        // Horizontal rule.
        let rule_run = trimmed
            .chars()
            .take_while(|c| *c == '-' || *c == '*' || *c == '_')
            .count();
        if rule_run >= 3
            && trimmed[..rule_run]
                .chars()
                .all(|c| c == trimmed.chars().next().unwrap())
            && trimmed[rule_run..].trim().is_empty()
        {
            let glyph = if ascii { "-" } else { "─" };
            out.push(Line::from(Span::styled(glyph.repeat(24), st.dim)));
            continue;
        }

        // Headings.
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let text = trimmed[hashes..].trim();
            let style = if hashes == 1 { st.heading1 } else { st.heading };
            out.push(Line::from(inline(text, style, &st)));
            continue;
        }

        // Block quote.
        if let Some(rest) = trimmed.strip_prefix('>') {
            let mut spans = vec![Span::styled(if ascii { "| " } else { "│ " }, st.dim)];
            spans.extend(inline(rest.trim_start(), st.quote, &st));
            out.push(Line::from(spans));
            continue;
        }

        // Bullets and numbered items keep a hanging indent so wrapped text lines
        // up under the text rather than the marker.
        let bullet = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "));
        if let Some(rest) = bullet {
            let marker = if ascii { "- " } else { "• " };
            let mut spans = vec![Span::styled(marker.to_string(), st.dim)];
            spans.extend(inline(rest, st.base, &st));
            out.push(Line::from(spans));
            continue;
        }
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") {
            let mut spans = vec![Span::styled(format!("{}. ", &trimmed[..digits]), st.dim)];
            spans.extend(inline(trimmed[digits + 2..].trim_start(), st.base, &st));
            out.push(Line::from(spans));
            continue;
        }

        out.push(Line::from(inline(trimmed, st.base, &st)));
    }

    // An unterminated fence just ends; nothing to close.
    out
}

/// Inline formatting: `code`, **bold**, *italic*, [text](url).
fn inline(text: &str, base: Style, st: &Styles) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            flush(&mut plain, &mut out, base);
            out.push(Span::styled(after[..end].to_string(), st.inline_code));
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            flush(&mut plain, &mut out, st.base);
            out.push(Span::styled(
                after[..end].to_string(),
                base.add_modifier(Modifier::BOLD),
            ));
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find("](")
            && let Some(end) = after[close + 2..].find(')')
        {
            let label = &after[..close];
            let url = &after[close + 2..close + 2 + end];
            flush(&mut plain, &mut out, st.base);
            out.push(Span::styled(
                label.to_string(),
                base.add_modifier(Modifier::UNDERLINED),
            ));
            if url != label {
                out.push(Span::styled(format!(" ({url})"), st.dim));
            }
            rest = &after[close + 2 + end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
            && end > 0
        {
            flush(&mut plain, &mut out, st.base);
            out.push(Span::styled(
                after[..end].to_string(),
                base.add_modifier(Modifier::ITALIC),
            ));
            rest = &after[end + 1..];
            continue;
        }
        let ch = rest.chars().next().expect("non-empty");
        plain.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut plain, &mut out, base);
    if out.is_empty() {
        out.push(Span::styled(String::new(), base));
    }
    out
}

fn flush(plain: &mut String, out: &mut Vec<Span<'static>>, style: Style) {
    if !plain.is_empty() {
        out.push(Span::styled(std::mem::take(plain), style));
    }
}

/// Plain text of a rendered line — test helper.
#[cfg(test)]
pub fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(md: &str) -> Vec<String> {
        render(md, Style::new(), false)
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn headings_lose_their_markers() {
        let out = text("# Title\n\n## Sub");
        assert_eq!(
            out,
            vec!["Title".to_string(), String::new(), "Sub".to_string()]
        );
    }

    #[test]
    fn fenced_code_is_literal_and_labelled() {
        let out = text("```rust\nlet x = *1*;\n```");
        assert_eq!(out, vec!["┌ rust", "│ let x = *1*;", "└"]);
    }

    #[test]
    fn bullets_quotes_and_rules() {
        let out = text("- one\n> quoted\n---");
        assert_eq!(
            out,
            vec!["• one".to_string(), "│ quoted".to_string(), "─".repeat(24)]
        );
    }

    #[test]
    fn ascii_mode_swaps_glyphs() {
        let out: Vec<String> = render("- one\n> q\n```sh\nls\n```", Style::new(), true)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(out, vec!["- one", "| q", "-- sh", "| ls", "`"]);
    }

    #[test]
    fn inline_styles_split_without_markers() {
        let md = "a **b** c `d` *e* [f](http://g)";
        let line = &render(md, Style::new(), false)[0];
        assert_eq!(line_text(line), "a b c d e f (http://g)");
        let bold = line
            .spans
            .iter()
            .find(|s| s.content == "b")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = line
            .spans
            .iter()
            .find(|s| s.content == "d")
            .expect("code span");
        assert_eq!(code.style.fg, Some(Color::Yellow));
        // A link whose label equals its url prints once.
        assert_eq!(line_text(&render("[x](x)", Style::new(), false)[0]), "x");
    }

    #[test]
    fn blank_runs_collapse() {
        assert_eq!(text("a\n\n\n\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn unterminated_fence_does_not_panic() {
        assert_eq!(text("```\nunclosed"), vec!["┌ code", "│ unclosed"]);
    }
}

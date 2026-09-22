//! `web_fetch`: read-only web research for the model, with the network-egress
//! hazards handled structurally rather than by approval. Only http/https, and
//! targets that name the machine itself or a private network are refused
//! outright — a prompt-injected model must not be able to probe
//! `169.254.169.254` or an internal service through this tool. (Known limit,
//! recorded rather than hidden: hostname rebinding to a private IP after the
//! literal check is not defended; `bash` remains the approval-gated path.)

use async_trait::async_trait;
use harness_core::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

const TIMEOUT_SECS: u64 = 20;
const MAX_BODY: usize = 512 * 1024;
/// What goes back inline; anything bigger pages through the artifact store.
const INLINE_LIMIT: usize = 8 * 1024;

pub const WEB_FETCH_TOOL_NAME: &str = "web_fetch";

pub struct WebFetchTool;

/// True when the host is safe to fetch: not an IP literal in a private or
/// link-local range, and not an obvious local name.
pub fn host_is_fetchable(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "metadata.google.internal") {
        return false;
    }
    if host.ends_with(".local") || host.ends_with(".localhost") || host.ends_with(".internal") {
        return false;
    }
    // A bare IPv6 literal arrives as `[::1]`.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                !(v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_unspecified())
            }
            std::net::IpAddr::V6(v6) => {
                !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00)
            }
        };
    }
    true
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: WEB_FETCH_TOOL_NAME.into(),
            description: "Fetch a public http(s) URL and return readable text (HTML is \
                          stripped). Private and local addresses are refused."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http(s) URL"}
                },
                "required": ["url"]
            }),
        }
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &mut ToolCtx<'_>) -> ToolOutput {
        let Some(url) = args["url"].as_str() else {
            return ToolOutput::error("missing `url`");
        };
        let parsed = match reqwest::Url::parse(url) {
            Ok(u) => u,
            Err(e) => return ToolOutput::error(format!("invalid URL: {e}")),
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return ToolOutput::error(format!("scheme {} not allowed", parsed.scheme()));
        }
        let host = parsed.host_str().unwrap_or_default().to_string();
        if !host_is_fetchable(&host) {
            return ToolOutput::error(format!(
                "refused: {host} is local or private — this tool only reads the public web"
            ));
        }
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("client: {e}")),
        };
        let resp = match client.get(parsed).send().await {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("fetch failed: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            return ToolOutput::error(format!("HTTP {status}"));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(format!("body: {e}")),
        };
        let bytes = &bytes[..bytes.len().min(MAX_BODY)];
        let raw = String::from_utf8_lossy(bytes);
        let text = if content_type.contains("html") {
            html_to_text(&raw)
        } else {
            raw.into_owned()
        };
        let note = if bytes.len() == MAX_BODY {
            format!("\n[truncated at {} KB]", MAX_BODY / 1024)
        } else {
            String::new()
        };
        if text.len() + note.len() <= INLINE_LIMIT {
            ToolOutput::text(format!("{text}{note}"))
        } else {
            let path = ctx_store(_ctx, &text);
            match path {
                Ok(p) => ToolOutput::text(format!(
                    "{}{note}\n[full text stored → read {p}]",
                    &text[..INLINE_LIMIT]
                )),
                Err(e) => ToolOutput::text(format!(
                    "{}{note}\n[storing the full text failed: {e}]",
                    &text[..INLINE_LIMIT]
                )),
            }
        }
    }
}

fn ctx_store(ctx: &ToolCtx<'_>, text: &str) -> std::io::Result<String> {
    ctx.artifacts.store("web", text.as_bytes())
}

/// Minimal HTML → readable text: drop script/style content, tags, and decode
/// the common entities. Not a renderer — good enough for research answers.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    // Strip script/style blocks including their content.
    while let Some(pos) = rest.find('<') {
        let (text, tail) = rest.split_at(pos);
        out.push_str(text);
        let lower = tail.to_ascii_lowercase();
        let skip_block = ["<script", "<style", "<!--"]
            .iter()
            .find_map(|tag| lower.find(tag).filter(|p| *p == 0).map(|_| *tag));
        if let Some(tag) = skip_block {
            let end_tag = match tag {
                "<script" => "</script>",
                "<style" => "</style>",
                _ => "-->",
            };
            match lower.find(end_tag) {
                Some(end) => {
                    let after = end + end_tag.len();
                    rest = &tail[after.min(tail.len())..];
                    continue;
                }
                None => break,
            }
        }
        // Any other tag: skip to its '>'.
        match tail.find('>') {
            Some(gt) => {
                let after = gt + 1;
                let is_block = {
                    let t = tail[..gt].to_ascii_lowercase();
                    t.starts_with("</p")
                        || t.starts_with("<p")
                        || t.starts_with("<br")
                        || t.starts_with("</div")
                        || t.starts_with("<li")
                        || t.starts_with("</h")
                        || t.starts_with("<h")
                        || t.starts_with("<tr")
                        || t.starts_with("</tr")
                };
                if is_block {
                    out.push('\n');
                }
                rest = &tail[after..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    // Collapse the whitespace runs HTML ignores.
    let mut collapsed = String::with_capacity(decoded.len());
    let mut in_blank = false;
    for line in decoded.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !in_blank {
                collapsed.push('\n');
                in_blank = true;
            }
        } else {
            collapsed.push_str(t);
            collapsed.push('\n');
            in_blank = false;
        }
    }
    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_private_targets_are_refused() {
        for host in [
            "localhost",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.5",
            "172.16.9.9",
            "169.254.169.254",
            "[::1]",
            "metadata.google.internal",
            "router.internal",
            "printer.local",
        ] {
            assert!(!host_is_fetchable(host), "{host} must be refused");
        }
        for host in ["example.com", "api.deepseek.com", "8.8.8.8", "1.1.1.1"] {
            assert!(host_is_fetchable(host), "{host} must be allowed");
        }
    }

    #[test]
    fn html_becomes_readable_text() {
        let html = "<html><head><style>body{color:red}</style><title>t</title></head>\
                    <body><h1>Hello</h1><script>alert(1)</script>\
                    <p>First &amp; second</p><br><div>line</div>\
                    <!-- a comment --></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("First & second"));
        assert!(text.contains("line"));
        assert!(!text.contains("alert"), "script content dropped: {text}");
        assert!(!text.contains("color:red"), "style content dropped: {text}");
        assert!(!text.contains('<'));
    }
}

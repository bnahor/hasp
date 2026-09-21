//! Preview helpers that keep a tool's output useful without letting a huge dump
//! swamp the transcript (the model still receives the full text).

use harness_core::engine::{TOOL_PREVIEW_CHARS, TOOL_PREVIEW_LINES, args_preview, content_preview};
use serde_json::json;

#[test]
fn args_preview_prefers_the_human_facing_field() {
    assert_eq!(
        args_preview(&json!({"command": "cargo test", "timeout_ms": 5})),
        "cargo test"
    );
    assert_eq!(
        args_preview(&json!({"path": "src/lib.rs", "content": "x"})),
        "src/lib.rs"
    );
    assert_eq!(args_preview(&json!({})), "");
    assert_eq!(args_preview(&json!({"weird": 1})), "{\"weird\":1}");
    // multi-line commands collapse to their first line
    assert_eq!(args_preview(&json!({"command": "a\nb"})), "a");
}

#[test]
fn content_preview_caps_lines_and_stays_on_char_boundaries() {
    let many = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = content_preview(&many);
    assert_eq!(out.lines().count(), TOOL_PREVIEW_LINES);
    assert!(!out.contains("line 99"));

    // Truncation must never split a multi-byte character.
    let wide = "→".repeat(4000);
    let out = content_preview(&wide);
    assert!(
        out.len() <= TOOL_PREVIEW_CHARS + 8,
        "capped at the constant: {}",
        out.len()
    );
    assert!(out.ends_with('…'), "capped output is marked");
    let body = out.strip_suffix('…').expect("ellipsis suffix");
    assert!(body.chars().all(|c| c == '→'), "never a broken character");
}

#[test]
fn short_output_is_untouched() {
    assert_eq!(content_preview("exit: 0\nok\n"), "exit: 0\nok");
}

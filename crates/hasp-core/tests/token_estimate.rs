//! Context accounting: the calibrated estimate, and the provider-measured
//! number that supersedes it when it is still representative.

use hasp_core::session::SessionManager;
use hasp_core::{AgentMessage, ContentBlock, StopReason, Usage, estimate_text_tokens};
use hasp_workspace::Workspace;

#[test]
fn prose_lands_near_four_chars_per_token() {
    let text =
        "The quick brown fox jumps over the lazy dog and keeps on running through the field.";
    let naive = (text.len() / 4) as u64;
    let got = estimate_text_tokens(text);
    // Within a quarter of chars/4, and never materially below it: over-reporting
    // is the safe direction for a compaction trigger.
    assert!(
        got <= naive * 5 / 4 + 1 && got + 1 >= naive,
        "prose should stay near chars/4: {got} vs {naive}"
    );
}

#[test]
fn punctuation_is_charged_more_densely_than_words() {
    let punct: String = std::iter::repeat_n("=+-*/%<>()[]{};:,.", 3).collect();
    let words: String = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        .chars()
        .take(punct.len())
        .collect();
    assert_eq!(punct.len(), words.len(), "same character count");
    assert!(
        estimate_text_tokens(&punct) > estimate_text_tokens(&words),
        "code costs more than prose of the same length"
    );
}

#[test]
fn non_ascii_is_charged_per_character() {
    let cjk = "日本語のテキスト";
    assert_eq!(estimate_text_tokens(cjk), cjk.chars().count() as u64);
}

#[test]
fn empty_text_costs_nothing() {
    assert_eq!(estimate_text_tokens(""), 0);
}

#[test]
fn measured_size_comes_from_the_last_sample_until_a_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::resolve(tmp.path()).unwrap();
    let mut session = SessionManager::create(&ws).unwrap();

    assert_eq!(session.last_input_tokens(), None, "nothing sampled yet");
    session.append_message(AgentMessage::User {
        content: "hi".into(),
        ts: 0,
        images: Vec::new(),
    });
    assert_eq!(
        session.last_input_tokens(),
        None,
        "a prompt is not a sample"
    );

    session.append_message(AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text: "hello".into(),
        }],
        provider: "mock".into(),
        model: "mock".into(),
        usage: Usage {
            input: 100,
            output: 20,
            cache_read: 300,
            cache_write: 5,
        },
        stop_reason: StopReason::Stop,
        ts: 1,
    });
    assert_eq!(
        session.last_input_tokens(),
        Some(425),
        "input (including cache) plus output"
    );

    // A compaction reshapes the context, so the old number no longer applies.
    session.append_compaction("brief".into(), 425, Vec::new());
    assert_eq!(session.last_input_tokens(), None);
}

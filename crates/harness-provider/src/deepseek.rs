//! DeepSeek via their Anthropic-compatible endpoint.
//!
//! Models are thinking-by-default and return `thinking` blocks alongside text;
//! `anthropic.rs` keeps those out of the replayed transcript, which is what
//! both providers expect. Context is 1M tokens with tool calls supported.

use crate::anthropic::Anthropic;

pub const BASE_URL: &str = "https://api.deepseek.com/anthropic";
pub const DEFAULT_MODEL: &str = "deepseek-v4-pro";

pub fn client(api_key: String, model: String) -> Anthropic {
    Anthropic::new(api_key, model)
        .with_base_url(BASE_URL)
        .with_name("deepseek")
}

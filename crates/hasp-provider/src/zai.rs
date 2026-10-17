//! z.ai (GLM) via their Anthropic-compatible endpoint — the coding-plan
//! surface Claude Code speaks to, so the same wire works here.

use crate::anthropic::Anthropic;

pub const BASE_URL: &str = "https://api.z.ai/api/anthropic";
pub const DEFAULT_MODEL: &str = "glm-4.6";

pub fn client(api_key: String, model: String) -> Anthropic {
    Anthropic::new(api_key, model)
        .with_base_url(BASE_URL)
        .with_name("zai")
}

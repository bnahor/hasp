//! Provider implementations. The wire lives here; the core only sees the
//! `Provider` trait.

pub mod anthropic;
pub mod catalog;
pub mod deepseek;
pub mod echo;
pub mod mock;
pub mod openai_compat;
pub mod plans;
pub mod retry;
pub mod zai;

/// Providers whose model list is discovered from the endpoint at runtime
/// rather than fixed in code — the automatic half of model extensibility.
pub fn is_dynamic(provider: &str) -> bool {
    matches!(provider, "ollama" | "compat" | "openai-compat")
}

/// The provider client for a provider id. One door, so callers never
/// re-implement dispatch; `None` means the id is unknown (`mock` is handled
/// by callers because it has no key or model).
pub fn build(provider: &str, api_key: String, model: String) -> Option<Box<dyn Provider>> {
    match provider {
        "ollama" => Some(Box::new(openai_compat::OpenAICompat::ollama(model))),
        "compat" | "openai-compat" => {
            openai_compat::OpenAICompat::from_env(model).map(|c| Box::new(c) as Box<dyn Provider>)
        }
        _ => {
            anthropic_compatible(provider, api_key, model).map(|c| Box::new(c) as Box<dyn Provider>)
        }
    }
}

use hasp_core::Provider;

/// What a model that cannot see images gets in their place — the session
/// stays usable instead of failing every later request on the same history.
pub(crate) fn omitted(n: usize) -> String {
    format!("[{n} image(s) omitted: this model does not accept images]")
}

/// Default model for a provider id (`--provider` / `HASP_PROVIDER`). One
/// definition, so the client and the daemon can never disagree about it.
pub fn default_model(provider: &str) -> String {
    match provider {
        "zai" => zai::DEFAULT_MODEL.into(),
        "deepseek" => deepseek::DEFAULT_MODEL.into(),
        "anthropic" => "claude-opus-5".into(),
        other => plans::find(other)
            .map(|p| p.default_model)
            .unwrap_or_else(|| "claude-opus-5".into()),
    }
}

/// A client for any provider that speaks the Anthropic wire format. `None`
/// means the provider id is unknown (or `mock`, which is handled separately).
pub fn anthropic_compatible(
    provider: &str,
    api_key: String,
    model: String,
) -> Option<anthropic::Anthropic> {
    let window = context_window_for(&model);
    let client = match provider {
        "anthropic" => {
            let effort = hasp_workspace::env_var("HASP_EFFORT").ok();
            Some(anthropic::Anthropic::new(api_key, model).with_reasoning(effort.as_deref()))
        }
        "zai" => Some(zai::client(api_key, model)),
        "deepseek" => Some(deepseek::client(api_key, model)),
        other => plans::find(other).map(|p| plans::client(&p, api_key, model)),
    };
    client.map(|c| c.with_window(window))
}

/// Output-token ceiling for a request to `model`: the catalog's limit, capped
/// at 64k (streamed, so no HTTP-timeout concern; large enough that writing a
/// big file does not get its tool input cut off). Unknown models get a
/// conservative 16k, which every endpoint accepts.
pub fn max_output_for(model: &str) -> u32 {
    catalog::load()
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.max_output)
        .map(|n| n.min(64_000) as u32)
        .unwrap_or(16_384)
}

/// Context window for a model id, preferring the fetched catalog. The 200k
/// fallback is Anthropic-sized; wrong guesses only make compaction cautious.
pub fn context_window_for(model: &str) -> usize {
    catalog::load()
        .into_iter()
        .find(|m| m.id == model)
        .map(|m| m.context_window as usize)
        .unwrap_or(200_000)
}

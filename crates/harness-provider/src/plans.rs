//! Coding plans and other Anthropic-compatible endpoints, as data.
//!
//! Most coding plans (GLM, Kimi, MiniMax, Qwen, …) ship an endpoint that
//! speaks the Anthropic Messages wire so Claude Code can use them; the same
//! wire is what this harness's engine speaks, so a plan is a base URL, a key,
//! an auth header and a default model — no new client. Built-ins live in
//! [`builtin`]; anything else goes in `~/.harness/providers.json`:
//!
//! ```json
//! { "providers": { "work-gateway": {
//!     "base_url": "https://llm.example.com/anthropic",
//!     "key_env": "WORK_LLM_KEY", "model": "glm-5.1", "auth": "bearer" } } }
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;

/// How the key is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// `x-api-key: <key>` — Anthropic's own header.
    #[default]
    XApiKey,
    /// `Authorization: Bearer <key>` — what `ANTHROPIC_AUTH_TOKEN` sends.
    Bearer,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Plan {
    #[serde(skip)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub base_url: String,
    /// Environment variable holding the key (checked before the credentials
    /// file, where `harness login <id>` stores it).
    pub key_env: String,
    #[serde(rename = "model")]
    pub default_model: String,
    #[serde(default)]
    pub auth: Auth,
    /// Whether the models take images; images are replaced by a note when not.
    #[serde(default = "yes")]
    pub vision: bool,
    /// Where to get a key, for `harness login` and `harness providers`.
    #[serde(default)]
    pub signup: String,
    /// Reasoning effort sent as `output_config.effort` (overridden by
    /// HARNESS_EFFORT); `None` sends nothing and gets the endpoint's default.
    #[serde(default)]
    pub effort: Option<String>,
}

fn yes() -> bool {
    true
}

#[allow(clippy::too_many_arguments)]
fn plan(
    id: &str,
    label: &str,
    base_url: &str,
    key_env: &str,
    model: &str,
    auth: Auth,
    vision: bool,
    signup: &str,
) -> Plan {
    Plan {
        id: id.into(),
        label: label.into(),
        base_url: base_url.into(),
        key_env: key_env.into(),
        default_model: model.into(),
        auth,
        vision,
        signup: signup.into(),
        effort: None,
    }
}

/// Kimi K3 reasons at `max` unless told otherwise; `high` is what Codex
/// sends it, and measured at about half the output tokens.
fn kimi(mut p: Plan) -> Plan {
    p.effort = Some("high".into());
    p
}

/// The built-in endpoints. `zai` and `deepseek` also have dedicated modules
/// (they predate this table); they are listed here so they show up with the
/// rest and resolve keys the same way.
pub fn builtin() -> Vec<Plan> {
    use Auth::*;
    vec![
        plan(
            "deepseek",
            "DeepSeek",
            crate::deepseek::BASE_URL,
            "DEEPSEEK_API_KEY",
            crate::deepseek::DEFAULT_MODEL,
            XApiKey,
            false,
            "https://platform.deepseek.com/api_keys",
        ),
        kimi(plan(
            "kimi",
            "Kimi Code (coding plan)",
            "https://api.kimi.ai/coding",
            "KIMI_API_KEY",
            "kimi-for-coding",
            XApiKey,
            true,
            "https://www.kimi.com/code",
        )),
        plan(
            "minimax",
            "MiniMax (coding / token plan)",
            "https://api.minimax.io/anthropic",
            "MINIMAX_API_KEY",
            "MiniMax-M3",
            Bearer,
            false,
            "https://platform.minimax.io",
        ),
        plan(
            "minimax-cn",
            "MiniMax China (coding / token plan)",
            "https://api.minimax.cn/anthropic",
            "MINIMAX_API_KEY",
            "MiniMax-M3",
            Bearer,
            false,
            "https://platform.minimaxi.com",
        ),
        kimi(plan(
            "moonshot",
            "Moonshot (Kimi, pay as you go)",
            "https://api.moonshot.ai/anthropic",
            "MOONSHOT_API_KEY",
            "kimi-k3",
            Bearer,
            true,
            "https://platform.kimi.ai",
        )),
        plan(
            "qwen",
            "Alibaba Model Studio coding plan (Qwen, GLM, Kimi, MiniMax)",
            "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic",
            "QWEN_CODING_API_KEY",
            "qwen3.7-plus",
            XApiKey,
            true,
            "https://www.alibabacloud.com/help/en/model-studio/coding-plan",
        ),
        plan(
            "zai",
            "Z.ai GLM coding plan",
            crate::zai::BASE_URL,
            "ZAI_API_KEY",
            crate::zai::DEFAULT_MODEL,
            XApiKey,
            false,
            "https://z.ai/subscribe",
        ),
    ]
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    providers: BTreeMap<String, Plan>,
}

/// `~/.harness/providers.json`. A malformed file is reported and ignored.
pub fn custom() -> Vec<Plan> {
    let Ok(home) = harness_workspace::harness_home() else {
        return Vec::new();
    };
    let path = home.join("providers.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_custom(&text).unwrap_or_else(|e| {
        eprintln!("▪ ignoring {}: {e}", path.display());
        Vec::new()
    })
}

fn parse_custom(text: &str) -> Result<Vec<Plan>, serde_json::Error> {
    let file: File = serde_json::from_str(text)?;
    Ok(file
        .providers
        .into_iter()
        .map(|(id, mut p)| {
            if p.label.is_empty() {
                p.label = id.clone();
            }
            p.id = id;
            p
        })
        .collect())
}

/// Every plan: built-ins, then custom ones (a custom entry with a built-in's
/// id replaces it).
pub fn all() -> Vec<Plan> {
    let custom = custom();
    let mut out: Vec<Plan> = builtin()
        .into_iter()
        .filter(|b| !custom.iter().any(|c| c.id == b.id))
        .collect();
    out.extend(custom);
    out
}

pub fn find(id: &str) -> Option<Plan> {
    all().into_iter().find(|p| p.id == id)
}

/// A client for a plan.
pub fn client(p: &Plan, api_key: String, model: String) -> crate::anthropic::Anthropic {
    // Labels are recorded in transcripts for the life of the process; the
    // handful of plan ids is leaked once each rather than threading lifetimes.
    let label: &'static str = Box::leak(p.id.clone().into_boxed_str());
    crate::anthropic::Anthropic::new(api_key, model)
        .with_base_url(p.base_url.trim_end_matches('/'))
        .with_name(label)
        .with_bearer(p.auth == Auth::Bearer)
        .with_vision(p.vision)
        .with_effort(
            std::env::var("HARNESS_EFFORT")
                .ok()
                .filter(|e| !e.is_empty())
                .or_else(|| p.effort.clone()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_ids_are_unique_and_urls_are_https() {
        let all = builtin();
        let mut ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all.len());
        assert!(all.iter().all(|p| p.base_url.starts_with("https://")));
    }

    #[test]
    fn kimi_endpoints_ask_for_high_effort_not_the_max_default() {
        for id in ["kimi", "moonshot"] {
            let p = builtin().into_iter().find(|p| p.id == id).unwrap();
            assert_eq!(p.effort.as_deref(), Some("high"), "{id}");
        }
        let glm = builtin().into_iter().find(|p| p.id == "zai").unwrap();
        assert_eq!(glm.effort, None);
    }

    #[test]
    fn custom_providers_parse_with_defaults() {
        let v = parse_custom(
            r#"{"providers": {"gw": {"base_url": "https://gw/anthropic/", "key_env": "GW",
                "model": "m", "auth": "bearer"}}}"#,
        )
        .unwrap();
        assert_eq!(v[0].id, "gw");
        assert_eq!(v[0].label, "gw");
        assert_eq!(v[0].auth, Auth::Bearer);
        assert!(v[0].vision);
        let c = client(&v[0], "k".into(), "m".into());
        assert_eq!(c.base_url(), "https://gw/anthropic");
    }
}

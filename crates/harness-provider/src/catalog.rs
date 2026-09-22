//! Model catalog: bundled fallback (ids + windows) plus a fetched catalog from
//! models.dev (`harness update --models`) — the same source codegraff uses.
//! Prices are optional everywhere; cost display only appears when present.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use harness_core::Usage;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: u64,
    pub max_output: Option<u64>,
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
}

impl ModelInfo {
    pub fn priced(&self) -> bool {
        self.input_per_mtok.is_some() && self.output_per_mtok.is_some()
    }
}

pub fn fallback_catalog() -> Vec<ModelInfo> {
    let mut models: Vec<ModelInfo> = [("glm-4.6", 200_000u64), ("glm-4.5", 128_000)]
        .into_iter()
        .map(|(id, w)| ModelInfo {
            id: id.into(),
            context_window: w,
            ..Default::default()
        })
        .collect();
    models.extend(claude_models());
    models.extend(deepseek_models());
    models
}

/// First-party Claude list prices (input, output per MTok). Cache reads bill
/// at 0.1× input and 5-minute cache writes at 1.25×.
fn claude_models() -> Vec<ModelInfo> {
    let model = |id: &str, window: u64, max_out: u64, input: f64, output: f64| ModelInfo {
        id: id.into(),
        context_window: window,
        max_output: Some(max_out),
        input_per_mtok: Some(input),
        output_per_mtok: Some(output),
        cache_read_per_mtok: Some(input * 0.1),
        cache_write_per_mtok: Some(input * 1.25),
    };
    vec![
        model("claude-fable-5-1", 1_000_000, 128_000, 10.0, 50.0),
        model("claude-fable-5", 1_000_000, 128_000, 10.0, 50.0),
        model("claude-opus-5-5", 1_000_000, 128_000, 4.0, 20.0),
        model("claude-opus-5", 1_000_000, 128_000, 5.0, 25.0),
        model("claude-opus-4-8", 1_000_000, 128_000, 5.0, 25.0),
        model("claude-opus-4-7", 1_000_000, 128_000, 5.0, 25.0),
        model("claude-opus-4-6", 1_000_000, 128_000, 5.0, 25.0),
        model("claude-sonnet-5", 1_000_000, 128_000, 2.0, 10.0),
        model("claude-sonnet-4-6", 1_000_000, 128_000, 3.0, 15.0),
        model("claude-sonnet-4-5", 200_000, 64_000, 3.0, 15.0),
        model("claude-haiku-4-5", 200_000, 64_000, 1.0, 5.0),
    ]
}

/// DeepSeek bills peak and off-peak rates; the peak (higher) numbers are used
/// so the displayed cost is never below what was actually charged. Cache
/// writes are not billed separately.
fn deepseek_models() -> Vec<ModelInfo> {
    let model = |id: &str, input: f64, output: f64, cache_read: f64| ModelInfo {
        id: id.into(),
        context_window: 1_000_000,
        max_output: Some(384_000),
        input_per_mtok: Some(input),
        output_per_mtok: Some(output),
        cache_read_per_mtok: Some(cache_read),
        cache_write_per_mtok: None,
    };
    vec![
        model("deepseek-v4-pro", 1.32, 3.96, 0.044),
        model("deepseek-flash", 0.30, 1.20, 0.006),
    ]
}

fn catalog_path() -> std::io::Result<PathBuf> {
    Ok(harness_workspace::harness_home()
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .join("models.json"))
}

pub fn path() -> std::io::Result<PathBuf> {
    catalog_path()
}

/// The fetched catalog, topped up with bundled entries it does not list — a
/// model newer than the last fetch still gets its real window and prices.
pub fn load() -> Vec<ModelInfo> {
    let mut models: Vec<ModelInfo> = catalog_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    for m in fallback_catalog() {
        if !models.iter().any(|have| have.id == m.id) {
            models.push(m);
        }
    }
    models
}

/// How old the stored catalog may get before startup refreshes it.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

fn is_stale_at(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    meta.modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_none_or(|age| age > STALE_AFTER)
}

/// No stored catalog, or one old enough that ids and prices have moved.
pub fn is_stale() -> bool {
    catalog_path().map(|p| is_stale_at(&p)).unwrap_or(true)
}

/// Best-effort refresh at startup. The catalog feeds window sizes, cost
/// reporting and `--model` validation, so it should not need remembering a
/// command — but it must never block or fail a run, and offline simply keeps
/// the stored copy. `HARNESS_OFFLINE=1` opts out.
pub fn refresh_if_stale() {
    if std::env::var("HARNESS_OFFLINE").is_ok() || !is_stale() {
        return;
    }
    tokio::spawn(async {
        if let Ok(models) = fetch_models_dev().await {
            let _ = store(&models);
        }
    });
}

pub fn store(models: &[ModelInfo]) -> std::io::Result<PathBuf> {
    let path = catalog_path()?;
    std::fs::write(
        &path,
        serde_json::to_string_pretty(models).expect("catalog serializes"),
    )?;
    Ok(path)
}

pub async fn fetch_models_dev() -> Result<Vec<ModelInfo>, String> {
    let body = reqwest::Client::new()
        .get("https://models.dev/api.json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let models = parse_models_dev(&v);
    if models.is_empty() {
        return Err("no anthropic models found in models.dev response".into());
    }
    Ok(models)
}

fn parse_models_dev(v: &serde_json::Value) -> Vec<ModelInfo> {
    let Some(models) = v
        .get("anthropic")
        .and_then(|a| a.get("models"))
        .and_then(|m| m.as_object())
    else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|(id, m)| {
            // Real schema (verified 2026-09): window in limit.context, output in limit.output.
            let context_window = m.pointer("/limit/context")?.as_u64()?;
            Some(ModelInfo {
                id: id.clone(),
                context_window,
                max_output: m.pointer("/limit/output").and_then(|x| x.as_u64()),
                input_per_mtok: m.pointer("/cost/input").and_then(|x| x.as_f64()),
                output_per_mtok: m.pointer("/cost/output").and_then(|x| x.as_f64()),
                cache_read_per_mtok: m.pointer("/cost/cache_read").and_then(|x| x.as_f64()),
                cache_write_per_mtok: m.pointer("/cost/cache_write").and_then(|x| x.as_f64()),
            })
        })
        .collect()
}

pub fn find<'a>(models: &'a [ModelInfo], id: &str) -> Option<&'a ModelInfo> {
    models.iter().find(|m| m.id == id)
}

/// USD cost of a usage block given per-MTok prices; None when unpriced.
pub fn cost_of(model: &ModelInfo, usage: &Usage) -> Option<f64> {
    let input = model.input_per_mtok?;
    let output = model.output_per_mtok?;
    let cache_read = model.cache_read_per_mtok.unwrap_or(0.0);
    let cache_write = model.cache_write_per_mtok.unwrap_or(0.0);
    Some(
        (usage.input as f64 * input
            + usage.output as f64 * output
            + usage.cache_read as f64 * cache_read
            + usage.cache_write as f64 * cache_write)
            / 1_000_000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staleness_is_decided_by_presence_and_age() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        assert!(is_stale_at(&missing), "no catalog is stale");
        let fresh = tmp.path().join("fresh.json");
        std::fs::write(&fresh, "[]").unwrap();
        assert!(!is_stale_at(&fresh), "a just-written catalog is current");
    }

    #[test]
    fn parses_models_dev_shape() {
        let v: serde_json::Value = serde_json::json!({
            "anthropic": {
                "models": {
                    "claude-sonnet-4-5": {
                        "limit": {"context": 200000, "output": 64000},
                        "cost": {"input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.75}
                    },
                    "claude-haiku-4-5": {"limit": {"context": 200000}}
                }
            }
        });
        let models = parse_models_dev(&v);
        assert_eq!(models.len(), 2);
        let sonnet = find(&models, "claude-sonnet-4-5").unwrap();
        assert_eq!(sonnet.max_output, Some(64000));
        assert!(sonnet.priced());

        let usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 10_000_000,
            cache_write: 1_000_000,
        };
        let cost = cost_of(sonnet, &usage).unwrap();
        // 1M*3 + 0.1M*15 + 10M*0.3 + 1M*3.75 = 3 + 1.5 + 3.0 + 3.75 = 11.25
        assert!((cost - 11.25).abs() < 1e-9, "cost: {cost}");

        let haiku = find(&models, "claude-haiku-4-5").unwrap();
        assert!(!haiku.priced());
        assert!(cost_of(haiku, &usage).is_none());
    }
}

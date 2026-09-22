//! Retry and stall policy shared by the provider clients.
//!
//! Retrying is only ever safe *before* anything reaches the sink: a turn that
//! already streamed text cannot be replayed, because the sink (a client, a
//! journal writer, a child agent) has no way to un-see it. So these helpers
//! wrap the request itself, and the streaming loops carry only the stall
//! timeout.

use harness_core::ProviderError;

/// Attempts per request, including the first.
const MAX_ATTEMPTS: u32 = 4;
/// Never park a turn longer than this between attempts.
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Rate limits and transient server errors. 529 is Anthropic's overloaded.
pub fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Default retry cadence; tests inject a short one through the client builder.
pub fn default_base_ms() -> u64 {
    std::env::var("HARNESS_RETRY_BASE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250)
}

/// How long to wait before attempt `attempt` (1-based). Honors the server's
/// `Retry-After` when it sent one, and stays deterministic when it did not —
/// parallel children hammering a rate-limited endpoint is the user's own
/// problem to bound with concurrency, not something jitter hides.
pub fn backoff(base_ms: u64, attempt: u32, retry_after: Option<u64>) -> std::time::Duration {
    let exp = base_ms.saturating_mul(1u64 << attempt.min(5));
    let hinted = retry_after
        .map(std::time::Duration::from_secs)
        .unwrap_or_default();
    std::time::Duration::from_millis(exp)
        .max(hinted)
        .min(MAX_BACKOFF)
}

/// No bytes for this long mid-stream means the turn is dead; the alternative is
/// a client that waits forever on a socket nobody will write to again.
pub fn stall_timeout() -> std::time::Duration {
    let ms = std::env::var("HARNESS_STREAM_STALL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120_000);
    std::time::Duration::from_millis(ms)
}

fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Send a request, retrying transient failures. `build` is called per attempt
/// (a `RequestBuilder` is consumed by `send`).
pub async fn send_retrying<F>(base_ms: u64, build: F) -> Result<reqwest::Response, ProviderError>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match build().send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    return Ok(resp);
                }
                let status = resp.status().as_u16();
                if is_retryable(status) && attempt < MAX_ATTEMPTS {
                    let wait = backoff(base_ms, attempt, retry_after_secs(&resp));
                    tokio::time::sleep(wait).await;
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Api { status, body });
            }
            Err(e) => {
                let transient = e.is_timeout() || e.is_connect() || e.is_request();
                if transient && attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(backoff(base_ms, attempt, None)).await;
                    continue;
                }
                return Err(ProviderError::Transport(e.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_rate_limits_and_server_errors_only() {
        for s in [429, 500, 502, 503, 504, 529] {
            assert!(is_retryable(s), "{s} should retry");
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(!is_retryable(s), "{s} must not retry");
        }
    }

    #[test]
    fn backoff_grows_and_respects_the_server_hint() {
        let first = backoff(250, 1, None);
        let third = backoff(250, 3, None);
        assert!(third > first, "{third:?} should exceed {first:?}");
        assert!(
            backoff(250, 9, None) <= MAX_BACKOFF,
            "capped: {:?}",
            backoff(250, 9, None)
        );
        // A server that says "wait 5 seconds" wins over a shorter guess.
        assert!(backoff(250, 1, Some(5)) >= std::time::Duration::from_secs(5));
    }
}

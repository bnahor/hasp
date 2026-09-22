//! The client↔agent wire: newline-delimited JSON-RPC 2.0 with ACP-shaped
//! methods (research/03-architecture.md §8.1). One JSON value per line;
//! swapping to strict ACP framing later is mechanical.

pub mod client;

pub use client::WireClient;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_SESSION_NEW: &str = "session/new";
pub const METHOD_SESSION_CONTINUE: &str = "session/continue";
pub const METHOD_SESSION_LOAD: &str = "session/load";
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";
pub const METHOD_SESSION_NAME: &str = "session/name";
pub const METHOD_SESSION_STATS: &str = "session/stats";
pub const METHOD_SESSION_HISTORY: &str = "session/history";
pub const METHOD_SESSION_RESIDENTS: &str = "session/residents";
pub const METHOD_SESSION_LISTINGS: &str = "session/listings";
pub const METHOD_SESSION_BRANCH: &str = "session/branch";
pub const METHOD_SESSION_FORK: &str = "session/fork";
pub const METHOD_SESSION_COMPACT: &str = "session/compact";
pub const METHOD_SESSION_WORKTREE: &str = "session/worktree";
pub const METHOD_SESSION_WORKTREE_EXIT: &str = "session/worktree-exit";
pub const METHOD_MODEL_SET: &str = "model/set";
pub const METHOD_WORKSPACE_FILES: &str = "workspace/files";
pub const METHOD_AGENT_SPAWN: &str = "agent/spawn";
pub const METHOD_AGENT_MESSAGE: &str = "agent/message";
pub const METHOD_AGENT_LIST: &str = "agent/list";
pub const METHOD_GOAL_SET: &str = "goal/set";
pub const METHOD_GOAL_STATUS: &str = "goal/status";
pub const METHOD_AUTONOMOUS_SET: &str = "autonomous/set";
pub const METHOD_HEARTBEAT_SET: &str = "heartbeat/set";
pub const METHOD_HEARTBEAT_LIST: &str = "heartbeat/list";
pub const METHOD_PERMISSION_REQUEST: &str = "permission/request";
pub const METHOD_SESSION_UPDATE: &str = "session/update";
pub const METHOD_SHUTDOWN: &str = "shutdown";

pub const ERR_WORKSPACE_BUSY: i32 = -32001;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }

    pub fn app(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A wire message: requests carry `id` + `method`; responses carry `id` and
/// exactly one of `result`/`error`; notifications carry `method` only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Wire {
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    Notification {
        method: String,
        #[serde(default)]
        params: Value,
    },
}

impl Wire {
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self::Request {
            id,
            method: method.to_string(),
            params,
        }
    }

    pub fn notification(method: &str, params: Value) -> Self {
        Self::Notification {
            method: method.to_string(),
            params,
        }
    }

    pub fn ok(id: u64, result: Value) -> Self {
        Self::Response {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: RpcError) -> Self {
        Self::Response {
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn id(&self) -> Option<u64> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } => Some(*id),
            Self::Notification { .. } => None,
        }
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } => None,
        }
    }
}

/// Encode to exactly one newline-terminated line.
pub fn encode(msg: &Wire) -> String {
    let mut s = serde_json::to_string(msg).expect("wire serializes");
    s.push('\n');
    s
}

pub fn decode(line: &str) -> Option<Wire> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str(t).ok()
}

pub fn session_update(session_id: &str, kind: &str, text: &str) -> Wire {
    Wire::notification(
        METHOD_SESSION_UPDATE,
        serde_json::json!({"sessionId": session_id, "kind": kind, "text": text}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_all_three_kinds() {
        let req = Wire::request(
            7,
            METHOD_SESSION_PROMPT,
            serde_json::json!({"prompt": "hi"}),
        );
        assert_eq!(decode(&encode(&req)).unwrap(), req);

        let res = Wire::ok(7, serde_json::json!({"samples": 1}));
        assert_eq!(decode(&encode(&res)).unwrap(), res);

        let err = Wire::err(7, RpcError::app(ERR_WORKSPACE_BUSY, "busy"));
        let decoded = decode(&encode(&err)).unwrap();
        assert_eq!(decoded, err);
        match decoded {
            Wire::Response { error: Some(e), .. } => assert_eq!(e.code, ERR_WORKSPACE_BUSY),
            other => panic!("expected error response, got {other:?}"),
        }

        let note = session_update("s1", "delta", "chunk");
        assert_eq!(decode(&encode(&note)).unwrap(), note);
        assert_eq!(note.method(), Some(METHOD_SESSION_UPDATE));
        assert_eq!(note.id(), None);
    }

    #[test]
    fn decode_tolerates_whitespace_and_junk() {
        assert!(decode("   \n").is_none());
        assert!(decode("not json").is_none());
        let msg = decode("  {\"id\":1,\"method\":\"initialize\",\"params\":{}}  \n").unwrap();
        assert_eq!(msg.method(), Some("initialize"));
    }
}

//! Test fixtures: a tiny HTTP server for fake platforms, and a scriptable
//! mock agent backend. No network beyond 127.0.0.1, no accounts.
#![allow(dead_code)]

use async_trait::async_trait;
use hasp_gateway::backend::{Asker, Backend};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub enum Resp {
    Json(u16, serde_json::Value),
    Raw(u16, &'static str, Vec<u8>),
    /// Read the request, then hang up without answering (a lost reply).
    Drop,
}

pub type Handler = Arc<dyn Fn(Req) -> Resp + Send + Sync>;

/// Serves `handler` on 127.0.0.1; returns the base URL.
pub async fn serve(handler: Handler) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let h = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let header_end = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.lines();
                let first: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
                let mut headers = HashMap::new();
                for l in lines {
                    if let Some((k, v)) = l.split_once(':') {
                        headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                    }
                }
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                while buf.len() < header_end + len {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let target = first.get(1).copied().unwrap_or("/");
                let (path, q) = target.split_once('?').unwrap_or((target, ""));
                let query = q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
                let req = Req { method: first[0].to_string(), path: path.to_string(), query, headers, body: buf[header_end..].to_vec() };
                let (status, ctype, body) = match h(req) {
                    Resp::Json(st, v) => (st, "application/json", v.to_string().into_bytes()),
                    Resp::Raw(st, ct, b) => (st, ct, b),
                    Resp::Drop => return,
                };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// Scriptable agent: echoes prompts, can block until released, can ask a
/// permission question, records every call.
#[derive(Default)]
pub struct MockBackend {
    pub prompts: Mutex<Vec<(String, String)>>,
    pub steers: Mutex<Vec<(String, String)>>,
    pub cancels: Mutex<Vec<String>>,
    pub hold: Mutex<Option<Arc<tokio::sync::Notify>>>,
    pub ask: Mutex<Option<String>>,
    pub answers: Mutex<Vec<char>>,
    pub fail_with: Mutex<Option<String>>,
}

#[async_trait]
impl Backend for MockBackend {
    async fn prompt(&self, session_key: &str, text: &str, ask: Arc<dyn Asker>) -> anyhow::Result<String> {
        self.prompts.lock().unwrap().push((session_key.into(), text.into()));
        if let Some(e) = self.fail_with.lock().unwrap().clone() {
            anyhow::bail!(e);
        }
        let q = self.ask.lock().unwrap().clone();
        if let Some(q) = q {
            let a = ask.ask(q).await;
            self.answers.lock().unwrap().push(a);
        }
        let hold = self.hold.lock().unwrap().clone();
        if let Some(n) = hold {
            n.notified().await;
            if self.cancels.lock().unwrap().contains(&session_key.to_string()) {
                anyhow::bail!("stopped");
            }
        }
        Ok(format!("echo: {text}"))
    }
    async fn cancel(&self, session_key: &str) -> anyhow::Result<()> {
        self.cancels.lock().unwrap().push(session_key.into());
        if let Some(n) = self.hold.lock().unwrap().clone() {
            n.notify_waiters();
        }
        Ok(())
    }
    async fn steer(&self, session_key: &str, text: &str) -> anyhow::Result<()> {
        self.steers.lock().unwrap().push((session_key.into(), text.into()));
        Ok(())
    }
}

/// A fake Telegram Bot API server. Holds updates; getUpdates returns those
/// with update_id >= offset (so an un-advanced offset replays, like the real
/// one). sendMessage can be told to fail in specific ways.
#[derive(Default)]
pub struct FakeTelegram {
    pub token: String,
    pub updates: Mutex<Vec<serde_json::Value>>,
    pub sent: Mutex<Vec<serde_json::Value>>,
    pub answered: Mutex<Vec<String>>,
    pub next_send: Mutex<Vec<&'static str>>, // "429", "drop", "500" consumed in order
    pub offsets: Mutex<Vec<i64>>,
    pub msg_id: Mutex<i64>,
}

impl FakeTelegram {
    pub fn push_text(&self, update_id: i64, chat: i64, from: i64, name: &str, text: &str, reply_to: Option<i64>) {
        let mut m = serde_json::json!({"message_id": 1000 + update_id, "date": 1, "chat": {"id": chat, "type": "private"},
                                        "from": {"id": from, "is_bot": false, "first_name": name}, "text": text});
        if let Some(r) = reply_to {
            m["reply_to_message"] = serde_json::json!({"message_id": r, "chat": {"id": chat, "type": "private"}, "date": 1});
        }
        self.updates.lock().unwrap().push(serde_json::json!({"update_id": update_id, "message": m}));
    }

    pub fn push_button(&self, update_id: i64, chat: i64, from: i64, data: &str) {
        self.updates.lock().unwrap().push(serde_json::json!({"update_id": update_id, "callback_query": {
            "id": format!("cb{update_id}"), "from": {"id": from, "is_bot": false, "first_name": "x"}, "chat_instance": "ci",
            "data": data, "message": {"message_id": 1, "date": 1, "chat": {"id": chat, "type": "private"}}}}));
    }

    pub fn texts(&self) -> Vec<String> {
        self.sent.lock().unwrap().iter().map(|m| m["text"].as_str().unwrap_or("").to_string()).collect()
    }

    pub fn handler(self: &Arc<Self>) -> Handler {
        let me = self.clone();
        Arc::new(move |r: Req| {
            let Some(rest) = r.path.strip_prefix(&format!("/bot{}/", me.token)) else {
                return Resp::Json(401, serde_json::json!({"ok": false, "error_code": 401, "description": "Unauthorized"}));
            };
            let b = r.json();
            match rest {
                "deleteWebhook" => Resp::Json(200, serde_json::json!({"ok": true, "result": true})),
                "getUpdates" => {
                    let off = b["offset"].as_i64().unwrap_or(0);
                    me.offsets.lock().unwrap().push(off);
                    let mut ups = me.updates.lock().unwrap();
                    ups.retain(|u| u["update_id"].as_i64().unwrap_or(0) >= off); // confirmed ones are gone
                    Resp::Json(200, serde_json::json!({"ok": true, "result": ups.clone()}))
                }
                "sendMessage" => {
                    let mode = { let mut n = me.next_send.lock().unwrap(); if n.is_empty() { None } else { Some(n.remove(0)) } };
                    match mode {
                        Some("429") => Resp::Json(429, serde_json::json!({"ok": false, "error_code": 429, "description": "Too Many Requests", "parameters": {"retry_after": 1}})),
                        Some("500") => Resp::Json(500, serde_json::json!({"ok": false, "error_code": 500, "description": "Internal"})),
                        Some("drop") => {
                            me.sent.lock().unwrap().push(b); // delivered, but the reply is lost
                            Resp::Drop
                        }
                        _ => {
                            let mut id = me.msg_id.lock().unwrap();
                            *id += 1;
                            me.sent.lock().unwrap().push(b.clone());
                            Resp::Json(200, serde_json::json!({"ok": true, "result": {"message_id": 5000 + *id, "chat": {"id": b["chat_id"]}, "date": 1}}))
                        }
                    }
                }
                "answerCallbackQuery" => {
                    me.answered.lock().unwrap().push(b["callback_query_id"].as_str().unwrap_or("").into());
                    Resp::Json(200, serde_json::json!({"ok": true, "result": true}))
                }
                "sendChatAction" => Resp::Json(200, serde_json::json!({"ok": true, "result": true})),
                _ => Resp::Json(404, serde_json::json!({"ok": false, "description": "not found"})),
            }
        })
    }
}

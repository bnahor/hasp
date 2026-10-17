//! Local web view and API (127.0.0.1 only).
//!
//! Two kinds of caller, two tokens (only hashes stored):
//! - the owner (web page, `Authorization: Bearer <owner token>` or the `gw`
//!   cookie set by /login): read everything, chat, decide approvals, edit
//!   memory, activate workflows. Decisions go through the same authority as
//!   chat buttons.
//! - the agent (the MCP server hasp starts): `/agent/<tool>` only. Its session
//!   is the hasp workspace the MCP process runs in (`x-session-dir`, mapped
//!   by the gateway), never something the model chooses. It can't decide.

use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::approvals::{Decider, Decision};
use crate::gateway::Gateway;
use crate::model::Inbound;
use crate::people::OWNER;
use crate::store::sha256;

const APP: &str = include_str!("../../hasp-cli/assets/app.html");

/// The hasp app's own design tokens (dark/light), so this page matches it.
fn tokens() -> &'static str {
    let start = APP.find(":root {").unwrap_or(0);
    let end = APP[start..].find("* { box-sizing").map(|i| start + i).unwrap_or(start);
    &APP[start..end]
}

const PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>hasp · assistant</title><style>TOKENS
body{margin:0;background:var(--bg);color:var(--text);font:var(--fs)/1.55 var(--sans)}
main{max-width:980px;margin:0 auto;padding:24px 16px;display:grid;gap:18px}
h1{font:600 20px var(--serif);margin:0}h2{font:600 13px var(--sans);text-transform:uppercase;letter-spacing:.06em;color:var(--muted);margin:0 0 8px}
section{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:14px 16px}
.row{display:flex;gap:10px;align-items:baseline;padding:6px 0;border-top:1px solid var(--line-soft)}.row:first-of-type{border-top:0}
.tag{font:11px var(--mono);color:var(--muted);min-width:90px}.dim{color:var(--faint)}.warn{color:var(--warn)}.err{color:var(--err)}.ok{color:var(--ok)}
pre{white-space:pre-wrap;margin:6px 0;font:12.5px/1.5 var(--mono);background:var(--code);padding:8px 10px;border-radius:8px}
button{font:inherit;padding:4px 10px;border-radius:8px;border:1px solid var(--line);background:var(--raised);color:var(--text);cursor:pointer}
button.go{background:var(--accent);color:var(--accent-ink);border-color:var(--accent)}
form{display:flex;gap:8px}input{flex:1;padding:7px 10px;border-radius:8px;border:1px solid var(--line);background:var(--raised);color:var(--text)}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:18px}
</style></head><body><main>
<h1>Assistant</h1>
<section><h2>Chat · personal conversation</h2><div id="chat"></div>
<form id="say"><input name="t" placeholder="Message (same as your phone chat)" autocomplete="off"><button class="go">Send</button></form></section>
<section><h2>Approvals</h2><div id="approvals"></div></section>
<div class="grid">
<section><h2>Tasks &amp; turns</h2><div id="turns"></div></section>
<section><h2>Waiting</h2><div id="waiting"></div></section>
<section><h2>Event inbox</h2><div id="events"></div></section>
<section><h2>Delivery</h2><div id="outbox"></div></section>
<section><h2>Memory</h2><div id="memory"></div></section>
<section><h2>Workflows</h2><div id="workflows"></div></section>
</div>
<section><h2>Audit</h2><div id="audit"></div></section>
</main><script>
const $=s=>document.querySelector(s),esc=s=>String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const row=(tag,body,extra='')=>`<div class="row"><span class="tag">${esc(tag)}</span><span>${body}</span>${extra}</div>`;
async function api(p,b){const r=await fetch(p,b?{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(b)}:{});return r.json()}
async function load(){const s=await api('/api/state');
$('#chat').innerHTML=s.chat.map(m=>row(m.dir==='in'?'you':'assistant',`<pre>${esc(m.text)}</pre>`)).join('')||'<span class="dim">no messages yet</span>';
$('#approvals').innerHTML=s.approvals.map(a=>row(a.state,`<pre>${esc(a.card)}</pre>`,a.state==='pending'?`<button class="go" onclick="decide('${a.id}','${a.nonce}','approve')">Approve</button><button onclick="decide('${a.id}','${a.nonce}','deny')">Deny</button>`:'')).join('')||'<span class="dim">nothing to approve</span>';
$('#turns').innerHTML=s.turns.map(t=>row(t.state,`${esc(t.session_key)} · ${esc(t.prompt)}`)).join('');
$('#waiting').innerHTML=s.waiting.map(w=>row(w.kind,`${esc(w.id)} ${esc(w.question)}`,`<button onclick="post('/api/waiting/${w.id}/cancel')">Cancel</button>`)).join('')||'<span class="dim">nothing waiting</span>';
$('#events').innerHTML=s.events.map(e=>row(e.state,`<b>${esc(e.source)}</b> ${esc(e.title)} <span class="dim">${esc(e.trust)}</span>`)).join('');
$('#outbox').innerHTML=s.outbox.map(o=>row(o.state,`${esc(o.adapter)} ${esc(o.dedupe_key)} <span class="${o.state==='uncertain'?'warn':o.state==='failed'?'err':'dim'}">${esc(o.last_error||'')}</span>`)).join('');
$('#memory').innerHTML=s.memory.map(m=>row(m.status,`${esc(m.text)} <span class="dim">${esc(m.kind)} · ${esc(m.provenance.source)}</span>`,(m.status==='proposed'?`<button onclick="post('/api/memory/${m.id}/confirm')">Confirm</button>`:'')+`<button onclick="post('/api/memory/${m.id}/forget')">Forget</button>`)).join('')||'<span class="dim">empty</span>';
$('#workflows').innerHTML=s.workflows.map(w=>row(`${w.name} v${w.version}`,`${esc(w.status)} · ${esc(w.description)}`,w.status==='draft'?`<button onclick="post('/api/workflows/activate',{name:'${w.name}',version:${w.version},sha256:'${w.sha256}'})">Activate</button>`:'')).join('')||'<span class="dim">none</span>';
$('#audit').innerHTML=s.audit.map(a=>row(new Date(a.ts).toLocaleTimeString(),`${esc(a.actor)} ${esc(a.action)} ${esc(a.subject||'')}`)).join('');}
async function post(p,b){await api(p,b||{});load()}
async function decide(id,nonce,d){const r=await api(`/api/approvals/${id}/decide`,{decision:d,nonce});alert(r.message);load()}
$('#say').onsubmit=async e=>{e.preventDefault();const t=e.target.t.value.trim();if(!t)return;e.target.t.value='';await api('/api/chat',{text:t});load()};
load();setInterval(load,3000);
</script></body></html>"#;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub ctype: &'static str,
    pub body: Vec<u8>,
    pub cookie: Option<String>,
}

fn json_resp(status: u16, v: Value) -> Response {
    Response { status, ctype: "application/json", body: v.to_string().into_bytes(), cookie: None }
}

pub fn set_token(gw: &Gateway, kind: &str) -> Result<String> {
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    gw.store.set_meta(&format!("{kind}_token_hash"), &sha256(&token))?;
    gw.store.audit("cli", &format!("{kind}_token_issued"), "", "");
    Ok(token)
}

fn bearer(r: &Request) -> Option<String> {
    if let Some(a) = r.headers.get("authorization").and_then(|a| a.strip_prefix("Bearer ")) {
        return Some(a.trim().to_string());
    }
    r.headers.get("cookie")?.split(';').find_map(|c| c.trim().strip_prefix("gw=").map(String::from))
}

fn is(gw: &Gateway, r: &Request, kind: &str) -> bool {
    match (bearer(r), gw.store.get_meta(&format!("{kind}_token_hash"))) {
        (Some(t), Some(h)) => sha256(&t) == h,
        _ => false,
    }
}

pub async fn handle(gw: &Arc<Gateway>, r: Request) -> Response {
    match route(gw, r).await {
        Ok(resp) => resp,
        Err(e) => json_resp(400, json!({"error": e.to_string()})),
    }
}

async fn route(gw: &Arc<Gateway>, r: Request) -> Result<Response> {
    let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    // agent side
    if let Some(tool) = r.path.strip_prefix("/agent/") {
        if !is(gw, &r, "agent") {
            return Ok(json_resp(401, json!({"error": "agent token required"})));
        }
        let root = gw.store.get_meta("workspace_root").unwrap_or_default();
        let dir = r.headers.get("x-session-dir").cloned().unwrap_or_default();
        let Some(session) = crate::backend::session_of_dir(std::path::Path::new(&root), std::path::Path::new(&dir)) else {
            return Ok(json_resp(403, json!({"error": "not a gateway session workspace"})));
        };
        return Ok(match gw.agent_call(&session, tool, &body) {
            Ok(v) => json_resp(200, v),
            Err(e) => json_resp(400, json!({"error": e.to_string()})),
        });
    }
    if r.path.starts_with("/login") {
        // /login?token=… sets an HttpOnly cookie so the token isn't kept in page JS.
        let token = r.path.split("token=").nth(1).unwrap_or("").to_string();
        let ok = gw.store.get_meta("owner_token_hash").is_some_and(|h| h == sha256(&token));
        return Ok(Response { status: if ok { 302 } else { 401 }, ctype: "text/plain", body: if ok { b"/".to_vec() } else { b"bad token".to_vec() },
                             cookie: ok.then(|| format!("gw={token}; HttpOnly; SameSite=Strict; Path=/")) });
    }
    if !is(gw, &r, "owner") {
        return Ok(Response { status: 401, ctype: "text/plain", body: b"owner token required: hasp-gateway owner-token, then open /login?token=...".to_vec(), cookie: None });
    }
    // Browser-originated writes must come from this page (no cross-site form posts).
    if r.method == "POST" && r.headers.get("content-type").is_none_or(|c| !c.starts_with("application/json")) {
        return Ok(json_resp(403, json!({"error": "json only"})));
    }
    let owner = Decider { via: "web".into(), person: Some(OWNER.into()), label: "web:owner".into() };
    let mem_path = hasp_recall::memory::default_path(&gw.cfg.hasp_home);
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/") => Ok(Response { status: 200, ctype: "text/html; charset=utf-8", body: PAGE.replace("TOKENS", tokens()).into_bytes(), cookie: None }),
        ("GET", "/api/state") => {
            let s = &gw.store;
            let mut approvals = s.json_rows("SELECT id, kind, action, state, expires, session_key FROM approvals ORDER BY created DESC LIMIT 20", &[])?;
            for a in &mut approvals {
                // the owner page shows pending cards with their one-time code
                let card = s.json_rows("SELECT message FROM outbox WHERE dedupe_key LIKE ?1 LIMIT 1", &[&format!("card:{}:%", a["id"].as_str().unwrap_or(""))])?;
                let text = card.first().and_then(|c| serde_json::from_str::<Value>(c["message"].as_str().unwrap_or("{}")).ok()).map(|m| m["text"].clone()).unwrap_or(Value::Null);
                let nonce = text.as_str().and_then(|t| t.split(&format!("/approve {} ", a["id"].as_str().unwrap_or(""))).nth(1)).and_then(|t| t.split_whitespace().next()).unwrap_or("");
                a["card"] = text.clone();
                a["nonce"] = json!(nonce);
            }
            let memory = hasp_recall::MemoryStore::open(&mem_path).map(|m| m.all().to_vec()).unwrap_or_default();
            Ok(json_resp(200, json!({
                "chat": s.conn().prepare("SELECT dir, text, ts FROM web_messages ORDER BY id DESC LIMIT 30").ok().map(|mut st| st.query_map([], |r| Ok(json!({"dir": r.get::<_,String>(0)?, "text": r.get::<_,String>(1)?}))).map(|it| it.filter_map(|x| x.ok()).collect::<Vec<_>>()).unwrap_or_default()).unwrap_or_default().into_iter().rev().collect::<Vec<_>>(),
                "approvals": approvals,
                "turns": s.json_rows("SELECT id, session_key, kind, state, substr(prompt,1,80) AS prompt FROM turns ORDER BY id DESC LIMIT 20", &[])?,
                "waiting": s.open_waiting(None)?,
                "events": s.json_rows("SELECT id, source, kind, title, trust, state FROM events ORDER BY id DESC LIMIT 20", &[])?,
                "outbox": s.json_rows("SELECT adapter, dedupe_key, state, last_error FROM outbox ORDER BY id DESC LIMIT 20", &[])?,
                "memory": memory,
                "workflows": hasp_recall::workflows::Workflows::new(&gw.cfg.hasp_home).list(),
                "audit": s.json_rows("SELECT ts, actor, action, subject FROM audit ORDER BY id DESC LIMIT 30", &[])?,
            })))
        }
        ("POST", "/api/chat") => {
            let text = body["text"].as_str().unwrap_or("").to_string();
            gw.store.conn().execute("INSERT INTO web_messages(chat_id, dir, text, ts) VALUES ('owner', 'in', ?1, ?2)", rusqlite::params![text, crate::model::now_ms()])?;
            let e = Inbound { event_key: uuid::Uuid::new_v4().simple().to_string(), chat_id: "owner".into(), thread_id: None,
                              sender_id: crate::adapters::web::WEB_SENDER.into(), sender_name: None, from_self: false, direct: true, text,
                              attachments: vec![], reply_to: None, message_id: None, button: None, ts_ms: crate::model::now_ms() };
            gw.store.ingest("web", &[e], None)?;
            Ok(json_resp(200, json!({"queued": true})))
        }
        ("POST", p) if p.starts_with("/api/approvals/") && p.ends_with("/decide") => {
            let id = p.trim_start_matches("/api/approvals/").trim_end_matches("/decide");
            let d = if body["decision"] == "approve" { Decision::Approve } else { Decision::Deny };
            let msg = gw.apply_decision(id, body["nonce"].as_str().unwrap_or(""), d, &owner).await;
            Ok(json_resp(200, json!({"message": msg})))
        }
        ("POST", p) if p.starts_with("/api/waiting/") && p.ends_with("/cancel") => {
            let id = p.trim_start_matches("/api/waiting/").trim_end_matches("/cancel");
            Ok(json_resp(200, json!({"cancelled": gw.store.resolve_waiting(id, "cancelled", "cancelled on the web")?})))
        }
        ("POST", p) if p.starts_with("/api/memory/") => {
            let mut m = hasp_recall::MemoryStore::open(&mem_path)?;
            let rest = p.trim_start_matches("/api/memory/");
            let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
            match action {
                "confirm" => m.confirm(id, "owner:web", body["supersedes"].as_str())?,
                "forget" => m.forget(id)?,
                "edit" => m.edit(id, body["text"].as_str().unwrap_or(""), "web")?,
                _ => anyhow::bail!("unknown memory action"),
            }
            gw.store.audit("web:owner", &format!("memory_{action}"), id, "");
            Ok(json_resp(200, json!({"ok": true})))
        }
        ("POST", "/api/workflows/activate") => {
            let wf = hasp_recall::workflows::Workflows::new(&gw.cfg.hasp_home);
            wf.activate(body["name"].as_str().unwrap_or(""), body["version"].as_u64().unwrap_or(0) as u32, body["sha256"].as_str().unwrap_or(""), "owner:web")?;
            gw.store.audit("web:owner", "workflow_activated", body["name"].as_str().unwrap_or(""), "");
            Ok(json_resp(200, json!({"ok": true})))
        }
        _ => Ok(json_resp(404, json!({"error": "not found"}))),
    }
}

/// Serve on 127.0.0.1:port until the process ends.
pub async fn serve(gw: Arc<Gateway>, port: u16) -> Result<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let gw = gw.clone();
            tokio::spawn(async move {
                let Some(req) = read_request(&mut s).await else { return };
                let resp = handle(&gw, req).await;
                let mut head = format!("HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-store\r\nx-frame-options: DENY\r\nconnection: close\r\n",
                                       resp.status, resp.ctype, resp.body.len());
                if let Some(c) = resp.cookie {
                    head.push_str(&format!("set-cookie: {c}\r\n"));
                }
                if resp.status == 302 {
                    head.push_str("location: /\r\n");
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&resp.body).await;
            });
        }
    });
    Ok(port)
}

async fn read_request(s: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let end = loop {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.lines();
    let first: Vec<&str> = lines.next()?.split(' ').collect();
    let headers: std::collections::HashMap<String, String> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())).collect();
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0).min(1 << 20);
    while buf.len() < end + len {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(Request { method: first.first()?.to_string(), path: first.get(1)?.to_string(), headers, body: buf[end..].to_vec() })
}

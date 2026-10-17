//! Gateway acceptance tests: real adapters against fake platform servers,
//! the real store and pipeline, a mock agent. Each test is one property.

mod common;

use common::{FakeTelegram, MockBackend, serve};
use hasp_gateway::adapters::telegram::Telegram;
use hasp_gateway::gateway::{Config, Gateway};
use hasp_gateway::leader::{Leader, Solo};
use hasp_gateway::people::OWNER;
use hasp_gateway::store::Store;
use std::sync::Arc;

const OWNER_TG: i64 = 111;
const CHAT: i64 = 111;

struct World {
    tg: Arc<FakeTelegram>,
    backend: Arc<MockBackend>,
    gw: Arc<Gateway>,
    dir: tempfile::TempDir,
    url: String,
}

fn config(dir: &std::path::Path) -> Config {
    Config { approval_ttl_ms: 60_000, hasp_home: dir.join("hasp"), send_to_others: vec!["telegram".into()], notify_per_hour: 20 }
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let tg = Arc::new(FakeTelegram { token: "T0K3N".into(), ..Default::default() });
    let url = serve(tg.handler()).await;
    let backend = Arc::new(MockBackend::default());
    let gw = build(&dir, &url, backend.clone(), Arc::new(Solo));
    // The owner links their Telegram account with a code from the gateway CLI.
    let code = gw.store.issue_link_code(OWNER, 60_000, "cli").unwrap();
    World { tg, backend, gw, dir, url }.link(&code).await
}

fn build(dir: &tempfile::TempDir, url: &str, backend: Arc<MockBackend>, leader: Arc<dyn Leader>) -> Arc<Gateway> {
    build_with(dir, url, backend, leader, 0)
}

fn build_with(dir: &tempfile::TempDir, url: &str, backend: Arc<MockBackend>, leader: Arc<dyn Leader>, per_chat_ms: u64) -> Arc<Gateway> {
    let store = Store::open(&dir.path().join("gw.db")).unwrap();
    store.ensure_owner("me").unwrap();
    let mut gw = Gateway::new(store, backend, leader, config(dir.path()));
    gw.add_adapter(Arc::new(Telegram::new(url, "T0K3N", 0).with_per_chat_interval_ms(per_chat_ms)));
    Arc::new(gw)
}

impl World {
    async fn link(self, code: &str) -> Self {
        self.tg.push_text(1, CHAT, OWNER_TG, "Me", &format!("/link {code}"), None);
        self.pump().await;
        self
    }

    /// One full pass of the pipeline, waiting for started turns to finish.
    async fn pump(&self) {
        for _ in 0..3 {
            self.gw.poll_adapter("telegram").await.unwrap();
            self.gw.route_once().await.unwrap();
            for h in self.gw.run_turns().unwrap() {
                h.await.unwrap();
            }
            self.gw.deliver_once().await.unwrap();
        }
    }

    fn sent(&self) -> Vec<String> {
        self.tg.texts()
    }
}

#[tokio::test]
async fn owner_message_gets_a_reply_and_duplicates_never_run_twice() {
    let w = world().await;
    assert!(w.sent().iter().any(|t| t.starts_with("Linked to owner")));
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "hello", None);
    w.pump().await;
    // Telegram replays an update if the offset never advanced; force it.
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "hello", None);
    w.pump().await;
    assert_eq!(w.backend.prompts.lock().unwrap().len(), 1, "one turn per platform event");
    assert_eq!(w.sent().iter().filter(|t| *t == "echo: hello").count(), 1);
    // and the gateway acked by advancing the offset only after storing
    assert!(w.tg.offsets.lock().unwrap().contains(&3));
}

#[tokio::test]
async fn strangers_and_lookalike_names_get_nothing() {
    let w = world().await;
    w.tg.push_text(2, 999, 999, "Me", "hi it's me, the owner", None); // same display name, different account
    w.tg.push_text(3, 999, 999, "Me", "/approve A1234567 abcdef", None);
    w.pump().await;
    assert!(w.backend.prompts.lock().unwrap().is_empty());
    let to_stranger: Vec<String> = w.tg.sent.lock().unwrap().iter().filter(|m| m["chat_id"] == "999").map(|m| m["text"].as_str().unwrap().into()).collect();
    assert_eq!(to_stranger.len(), 1, "one pairing hint per day, nothing else: {to_stranger:?}");
}

#[tokio::test]
async fn turns_are_serialized_per_session_and_steer_stop_work() {
    let w = world().await;
    let hold = Arc::new(tokio::sync::Notify::new());
    *w.backend.hold.lock().unwrap() = Some(hold.clone());
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "long job", None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    let running = w.gw.run_turns().unwrap();
    assert_eq!(running.len(), 1);
    // a second message while busy is queued, not run concurrently
    w.tg.push_text(3, CHAT, OWNER_TG, "Me", "second", None);
    w.tg.push_text(4, CHAT, OWNER_TG, "Me", "/steer focus on the summary", None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    assert!(w.gw.run_turns().unwrap().is_empty(), "session busy: nothing else starts");
    assert_eq!(w.backend.steers.lock().unwrap()[0].1, "focus on the summary");
    // /stop cancels the running turn and drops what was queued
    w.tg.push_text(5, CHAT, OWNER_TG, "Me", "/stop", None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    for h in running {
        h.await.unwrap();
    }
    w.pump().await;
    let prompts: Vec<String> = w.backend.prompts.lock().unwrap().iter().map(|p| p.1.clone()).collect();
    assert_eq!(prompts, vec!["long job"], "the queued 'second' was dropped by /stop");
    assert!(w.sent().iter().any(|t| t == "Stopped."));
}

#[tokio::test]
async fn lost_send_reply_is_marked_uncertain_and_resent_visibly_without_rerunning() {
    let w = world().await;
    w.tg.next_send.lock().unwrap().push("drop");
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "what's up", None);
    w.pump().await;
    let replies: Vec<String> = w.sent().into_iter().filter(|t| t.contains("echo: what's up")).collect();
    assert_eq!(replies.len(), 2, "original (reply lost) + one marked resend: {replies:?}");
    assert!(replies[1].starts_with("↻ resent"));
    assert_eq!(w.backend.prompts.lock().unwrap().len(), 1, "the agent ran once");
}

#[tokio::test]
async fn rate_limits_are_honoured() {
    let w = world().await;
    // per-chat spacing: two replies queued at once go out at least 1 s apart
    let paced = build_with(&w.dir, &w.url, w.backend.clone(), Arc::new(Solo), 1_000);
    paced.reply(&hasp_gateway::turns::Origin { adapter: "telegram".into(), chat_id: CHAT.to_string(), thread_id: None, reply_to: None, person: OWNER.into() }, "one", vec![], "pace:1").unwrap();
    paced.reply(&hasp_gateway::turns::Origin { adapter: "telegram".into(), chat_id: CHAT.to_string(), thread_id: None, reply_to: None, person: OWNER.into() }, "two", vec![], "pace:2").unwrap();
    assert_eq!(paced.deliver_once().await.unwrap(), 1, "second message waits for the per-chat interval");
    tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
    assert_eq!(paced.deliver_once().await.unwrap(), 1);
    w.tg.next_send.lock().unwrap().push("429");
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "x", None);
    w.pump().await;
    let row = w.gw.store.json_rows("SELECT state, attempts, next_attempt FROM outbox WHERE dedupe_key LIKE 'turn:%'", &[]).unwrap();
    assert_eq!(row[0]["state"], "pending");
    assert!(row[0]["next_attempt"].as_i64().unwrap() > hasp_gateway::model::now_ms(), "waits retry_after before resending");
}

#[tokio::test]
async fn gateway_restart_reports_interrupted_turns_and_delivers_saved_replies_without_rerunning() {
    let w = world().await;
    // A reply saved but not delivered when the gateway died...
    w.tg.next_send.lock().unwrap().push("500");
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "saved", None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    for h in w.gw.run_turns().unwrap() {
        h.await.unwrap();
    }
    w.gw.deliver_once().await.unwrap(); // 500 -> retry later
    // ...and a turn that was running.
    w.tg.push_text(3, CHAT, OWNER_TG, "Me", "in flight", None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    w.gw.store.claim_runnable().unwrap(); // marked running, then "crash"
    let before = w.backend.prompts.lock().unwrap().len();

    let gw2 = build(&w.dir, &w.url, w.backend.clone(), Arc::new(Solo));
    assert_eq!(gw2.startup().unwrap(), 1);
    gw2.store.conn().execute("UPDATE outbox SET next_attempt = 0", []).unwrap();
    for _ in 0..3 {
        gw2.deliver_once().await.unwrap();
    }
    assert_eq!(w.backend.prompts.lock().unwrap().len(), before, "nothing re-ran after the restart");
    assert!(w.sent().iter().any(|t| t == "echo: saved"), "saved reply delivered");
    assert!(w.sent().iter().any(|t| t.contains("/resume")), "interrupted turn reported");
    // /resume is the owner's explicit choice
    w.tg.push_text(4, CHAT, OWNER_TG, "Me", "/resume", None);
    for _ in 0..3 {
        gw2.poll_adapter("telegram").await.unwrap();
        gw2.route_once().await.unwrap();
        for h in gw2.run_turns().unwrap() {
            h.await.unwrap();
        }
        gw2.deliver_once().await.unwrap();
    }
    assert!(w.sent().iter().any(|t| t == "echo: in flight"));
}

#[tokio::test]
async fn permission_cards_need_the_owner_and_bind_to_code_and_time() {
    let w = world().await;
    *w.backend.ask.lock().unwrap() = Some("run `rm -rf build`? [y]es / [a]lways / [n]o".into());
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "clean up", None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    let running = w.gw.run_turns().unwrap();
    // the card reaches the owner with buttons
    let mut card = None;
    for _ in 0..50 {
        w.gw.deliver_once().await.unwrap();
        card = w.tg.sent.lock().unwrap().iter().find(|m| m["text"].as_str().unwrap_or("").contains("rm -rf build")).cloned();
        if card.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let card = card.expect("card sent");
    let approve = card["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().unwrap().to_string();
    let (_, id, _) = hasp_gateway::approvals::parse_decision(&approve).unwrap();
    // a linked non-owner (member) can't approve
    w.gw.store.add_member("friend", "Friend").unwrap();
    let code = w.gw.store.issue_link_code("friend", 60_000, "cli").unwrap();
    w.tg.push_text(3, 222, 222, "Friend", &format!("/link {code}"), None);
    w.tg.push_button(4, 222, 222, &approve);
    // wrong code from the owner fails too
    w.tg.push_text(5, CHAT, OWNER_TG, "Me", &format!("/approve {id} 000000"), None);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    assert_eq!(w.gw.store.approval_state(&id).as_deref(), Some("pending"));
    // the owner's button works, once
    w.tg.push_button(6, CHAT, OWNER_TG, &approve);
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    for h in running {
        h.await.unwrap();
    }
    assert_eq!(*w.backend.answers.lock().unwrap(), vec!['y']);
    w.tg.push_button(7, CHAT, OWNER_TG, &approve); // replay
    w.gw.poll_adapter("telegram").await.unwrap();
    w.gw.route_once().await.unwrap();
    assert!(w.tg.answered.lock().unwrap().len() >= 3, "every button press is answered");
    let audit = w.gw.store.json_rows("SELECT action FROM audit WHERE subject = ?1", &[&id]).unwrap();
    assert!(audit.iter().any(|a| a["action"] == "approval_refused"));
}

#[tokio::test]
async fn expired_cards_cannot_be_approved() {
    let w = world().await;
    let card = w.gw.store.propose("send_message", &serde_json::json!({"adapter": "telegram", "recipient": "42", "text": "hi"}), "", None, None, "send:telegram", "agent:x", -1).unwrap();
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", &format!("/approve {} {}", card.id, card.nonce), None);
    w.pump().await;
    assert!(w.sent().iter().any(|t| t.contains("Not accepted: expired")));
    assert!(!w.tg.sent.lock().unwrap().iter().any(|m| m["chat_id"] == "42"));
}

#[tokio::test]
async fn drafting_is_not_sending_and_sending_needs_approval_once() {
    let w = world().await;
    let s = "personal:owner";
    let d = w.gw.agent_call(s, "draft_message", &serde_json::json!({"adapter": "telegram", "recipient": "42", "text": "Running late, 10 min", "context": "owner asked"})).unwrap();
    w.pump().await;
    assert!(!w.tg.sent.lock().unwrap().iter().any(|m| m["chat_id"] == "42"), "a draft sends nothing");
    // another session can't use this draft
    assert!(w.gw.agent_call("personal:someone", "request_send", &serde_json::json!({"draft_id": d["draft_id"]})).is_err());
    let r = w.gw.agent_call(s, "request_send", &serde_json::json!({"draft_id": d["draft_id"]})).unwrap();
    w.pump().await;
    let card_text = w.sent().into_iter().find(|t| t.contains("Running late")).expect("card");
    assert!(card_text.contains("To: 42 on telegram"), "exact destination shown: {card_text}");
    let id = r["approval"].as_str().unwrap();
    let nonce = card_text.split(&format!("/approve {id} ")).nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", &format!("/approve {id} {nonce}"), None);
    w.tg.push_text(3, CHAT, OWNER_TG, "Me", &format!("/approve {id} {nonce}"), None); // replay
    w.pump().await;
    let to_42: Vec<String> = w.tg.sent.lock().unwrap().iter().filter(|m| m["chat_id"] == "42").map(|m| m["text"].as_str().unwrap().into()).collect();
    assert_eq!(to_42, vec!["Running late, 10 min"], "sent exactly once");
    assert!(w.sent().iter().any(|t| t.contains("Not accepted: already")));
}

#[tokio::test]
async fn replies_resume_the_right_task_in_any_order() {
    let w = world().await;
    w.gw.agent_call("task:aaa", "wait_for_user", &serde_json::json!({"question": "Which hotel?"})).unwrap();
    w.gw.agent_call("task:bbb", "wait_for_user", &serde_json::json!({"question": "Which flight?"})).unwrap();
    w.pump().await;
    let sent = w.tg.sent.lock().unwrap().clone();
    let id_of = |q: &str| sent.iter().find(|m| m["text"].as_str().unwrap().contains(q)).map(|m| m.clone()).unwrap();
    let flight = id_of("Which flight?");
    let hotel = id_of("Which hotel?");
    // the owner replies to the second question first, then the first by code
    let flight_msg_id: i64 = 5000 + flight_index(&sent, "Which flight?");
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "the 9am one", Some(flight_msg_id));
    let hotel_code = hotel["text"].as_str().unwrap().split("start with ").nth(1).unwrap().trim_end_matches(')').to_string();
    w.tg.push_text(3, CHAT, OWNER_TG, "Me", &format!("{hotel_code} the cheap one"), None);
    w.pump().await;
    let p = w.backend.prompts.lock().unwrap().clone();
    assert!(p.iter().any(|(s, t)| s == "task:bbb" && t.contains("the 9am one")), "{p:?}");
    assert!(p.iter().any(|(s, t)| s == "task:aaa" && t.contains("the cheap one")), "{p:?}");
    assert!(!p.iter().any(|(s, _)| s == "personal:owner"), "answers didn't leak into the personal chat");
    let _ = flight;
}

fn flight_index(sent: &[serde_json::Value], q: &str) -> i64 {
    // fake ids are 5000 + send order (1-based)
    sent.iter().position(|m| m["text"].as_str().unwrap().contains(q)).unwrap() as i64 + 1
}

#[tokio::test]
async fn follow_ups_survive_a_restart_and_resume_their_session() {
    let w = world().await;
    let r = w.gw.agent_call("task:ccc", "schedule_followup", &serde_json::json!({"minutes": 0, "note": "check the build"})).unwrap();
    assert!(r["waiting"].as_str().is_some());
    let gw2 = build(&w.dir, &w.url, w.backend.clone(), Arc::new(Solo));
    gw2.startup().unwrap();
    assert_eq!(gw2.timers_once(hasp_gateway::model::now_ms() + 1000).unwrap(), 1);
    assert_eq!(gw2.timers_once(hasp_gateway::model::now_ms() + 2000).unwrap(), 0, "a due item fires once");
    for h in gw2.run_turns().unwrap() {
        h.await.unwrap();
    }
    assert!(w.backend.prompts.lock().unwrap().iter().any(|(s, t)| s == "task:ccc" && t.contains("check the build")));
    // and a cancelled one never fires
    let r = w.gw.agent_call("task:ddd", "schedule_followup", &serde_json::json!({"minutes": 0, "note": "never"})).unwrap();
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", &format!("/cancel {}", r["waiting"].as_str().unwrap()), None);
    w.pump().await;
    assert_eq!(gw2.timers_once(hasp_gateway::model::now_ms() + 1000).unwrap(), 0);
}

#[tokio::test]
async fn memory_and_workflows_are_owner_controlled_from_chat() {
    let w = world().await;
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "/remember I prefer aisle seats", None);
    w.pump().await;
    let path = hasp_recall::memory::default_path(&w.dir.path().join("hasp"));
    let m = hasp_recall::MemoryStore::open(&path).unwrap();
    let mem = m.all()[0].clone();
    assert_eq!(mem.provenance.source, hasp_recall::Source::Owner);
    assert!(mem.provenance.channel.as_deref().unwrap().starts_with("telegram:"));
    // a member can't forget the owner's memory or activate workflows
    w.gw.store.add_member("friend", "Friend").unwrap();
    let code = w.gw.store.issue_link_code("friend", 60_000, "cli").unwrap();
    w.tg.push_text(3, 222, 222, "Friend", &format!("/link {code}"), None);
    w.tg.push_text(4, 222, 222, "Friend", &format!("/forget {}", mem.id), None);
    let wf = hasp_recall::workflows::Workflows::new(&w.dir.path().join("hasp"));
    let d = wf.propose("daily-brief", "Morning summary", "1. calendar\n2. inbox", &[], "agent").unwrap();
    w.tg.push_text(5, 222, 222, "Friend", &format!("/activate daily-brief 1 {}", &d.sha256[..8]), None);
    w.pump().await;
    assert!(hasp_recall::MemoryStore::open(&path).unwrap().get(&mem.id).is_some());
    assert!(!w.dir.path().join("hasp/skills/daily-brief/SKILL.md").exists());
    // the owner can
    w.tg.push_text(6, CHAT, OWNER_TG, "Me", &format!("/activate daily-brief 1 {}", &d.sha256[..8]), None);
    w.tg.push_text(7, CHAT, OWNER_TG, "Me", &format!("/forget {}", mem.id), None);
    w.pump().await;
    assert!(w.dir.path().join("hasp/skills/daily-brief/SKILL.md").exists());
    assert!(!std::fs::read_to_string(&path).unwrap().contains("aisle"));
}

#[tokio::test]
async fn sessions_are_selectable_and_tasks_get_their_own() {
    let w = world().await;
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "/session new trip", None);
    w.tg.push_text(3, CHAT, OWNER_TG, "Me", "book things", None);
    w.tg.push_text(4, CHAT, OWNER_TG, "Me", "/task summarise my week", None);
    w.pump().await;
    let p = w.backend.prompts.lock().unwrap().clone();
    assert!(p.iter().any(|(s, t)| s == "named:owner:trip" && t == "book things"));
    assert!(p.iter().any(|(s, t)| s.starts_with("task:") && t == "summarise my week"));
}

/// Two gateways, one lease: only the leader polls and sends; a takeover
/// stops the old one before its next send.
#[tokio::test]
async fn only_the_leader_polls_and_sends() {
    let w = world().await;
    let lease = Arc::new(SharedLease::default());
    let a = build(&w.dir, &w.url, w.backend.clone(), Arc::new(Contender { lease: lease.clone(), me: "a" }));
    let b = build(&w.dir, &w.url, w.backend.clone(), Arc::new(Contender { lease: lease.clone(), me: "b" }));
    a.leader.hold().await.unwrap();
    b.leader.hold().await.unwrap();
    w.tg.push_text(2, CHAT, OWNER_TG, "Me", "who answers?", None);
    let offsets_before = w.tg.offsets.lock().unwrap().len();
    assert_eq!(b.poll_adapter("telegram").await.unwrap(), 0, "standby never polls");
    assert_eq!(w.tg.offsets.lock().unwrap().len(), offsets_before);
    a.poll_adapter("telegram").await.unwrap();
    a.route_once().await.unwrap();
    for h in a.run_turns().unwrap() {
        h.await.unwrap();
    }
    // takeover before a delivers
    *lease.holder.lock().unwrap() = Some(("b".into(), 2));
    assert_eq!(a.deliver_once().await.unwrap(), 0, "old leader stops sending");
    assert_eq!(b.deliver_once().await.unwrap(), 1);
    let fencing = w.gw.store.json_rows("SELECT fencing FROM outbox WHERE state = 'sent' ORDER BY id DESC LIMIT 1", &[]).unwrap();
    assert_eq!(fencing[0]["fencing"], 2, "send recorded under the new token");
}

#[derive(Default)]
struct SharedLease {
    holder: std::sync::Mutex<Option<(String, u64)>>,
}

struct Contender {
    lease: Arc<SharedLease>,
    me: &'static str,
}

#[async_trait::async_trait]
impl Leader for Contender {
    async fn hold(&self) -> anyhow::Result<Option<u64>> {
        let mut h = self.lease.holder.lock().unwrap();
        if h.is_none() {
            *h = Some((self.me.into(), 1));
        }
        Ok(h.as_ref().filter(|(who, _)| who == self.me).map(|(_, f)| *f))
    }
    fn current(&self) -> Option<u64> {
        self.lease.holder.lock().unwrap().as_ref().filter(|(who, _)| who == self.me).map(|(_, f)| *f)
    }
    fn release_local(&self) {}
}

#[tokio::test]
async fn telegram_auth_failure_is_an_error_not_silence() {
    let w = world().await;
    let bad = Telegram::new(&w.url, "WRONG", 0);
    let err = hasp_gateway::adapters::Adapter::poll(&bad, None).await.err().expect("401");
    assert!(err.to_string().contains("401"), "{err}");
}


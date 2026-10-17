//! LocalBus: the agent-bus implementation shared by embedded and daemon
//! modes (research/03-architecture.md §8.2). Children are fresh engines in
//! fresh sessions; mutating children get managed worktrees (`sa-<id>`
//! branches). Harness-assigned ids, family-scoped, one level deep.

use hasp_core::engine::{Engine, TurnSink};
use hasp_core::session::SessionManager;
use hasp_core::{
    AgentMessage, Bus, ChildInfo, ChildReceipt, ChildStatus, ContentBlock, Gate, Provider, Sink,
    SpawnHandle, SpawnRequest,
};
use hasp_tools::builtins_base;
use hasp_workspace::Workspace;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

pub type ProviderFactory = Arc<dyn Fn() -> Result<Box<dyn Provider>, String> + Send + Sync>;
pub type GateFactory = Arc<dyn Fn() -> Box<dyn Gate> + Send + Sync>;
pub type NoticeFanout = Arc<dyn Fn(&str) + Send + Sync>;

struct ChildEntry {
    info: ChildInfo,
    engine: Arc<AsyncMutex<Engine>>,
    inbox: Arc<Mutex<Vec<String>>>,
}

pub struct LocalBus {
    workspace: Workspace,
    model: String,
    max_tokens: u32,
    provider_factory: ProviderFactory,
    gate_factory: GateFactory,
    parent_inbox: Arc<Mutex<Vec<String>>>,
    fanout: Option<NoticeFanout>,
    children: Mutex<HashMap<String, Arc<Mutex<ChildEntry>>>>,
    counter: AtomicU64,
    /// The parent's environment: children run their tools there unless they
    /// name another one, so a sandboxed session cannot spawn its way out.
    environment: Option<String>,
}

struct MuteSink;

impl Sink for MuteSink {
    fn on_delta(&mut self, _text: &str) {}
}

impl TurnSink for MuteSink {
    fn on_note(&mut self, _note: &str) {}
}

fn last_assistant_text(session: &SessionManager) -> String {
    session
        .context_messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            AgentMessage::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn snippet(text: &str, cap: usize) -> String {
    let mut s: String = text.chars().take(cap).collect();
    if text.chars().count() > cap {
        s.push('…');
    }
    s
}

impl LocalBus {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: Workspace,
        model: String,
        max_tokens: u32,
        provider_factory: ProviderFactory,
        gate_factory: GateFactory,
        parent_inbox: Arc<Mutex<Vec<String>>>,
        fanout: Option<NoticeFanout>,
    ) -> Self {
        Self {
            workspace,
            model,
            max_tokens,
            provider_factory,
            gate_factory,
            parent_inbox,
            fanout,
            children: Mutex::new(HashMap::new()),
            environment: None,
            counter: AtomicU64::new(0),
        }
    }

    /// Runs one child turn to completion, stamps the registry entry, and
    /// reports via the parent's notice channel. Shared by spawn and
    /// trigger-messages.
    fn run_child(&self, entry: Arc<Mutex<ChildEntry>>, task: String, reply_label: bool) {
        let parent_inbox = self.parent_inbox.clone();
        let fanout = self.fanout.clone();
        tokio::spawn(async move {
            let (child_id, name, engine) = {
                let e = entry.lock().expect("child entry");
                (
                    e.info.child_id.clone(),
                    e.info.name.clone(),
                    e.engine.clone(),
                )
            };
            let label = format!("{child_id} ({name})");
            let mut eng = engine.lock().await;
            let mut sink = MuteSink;
            let result = eng.run_turn(&task, &mut sink).await;
            let final_text = last_assistant_text(&eng.session);
            drop(eng);
            let (status, verb) = match &result {
                Ok(stats) if stats.aborted => {
                    (ChildStatus::Failed("cancelled".into()), "was cancelled")
                }
                Ok(_) => (
                    ChildStatus::Completed,
                    if reply_label { "replied" } else { "finished" },
                ),
                Err(e) => (ChildStatus::Failed(e.to_string()), "failed"),
            };
            let report = snippet(&final_text, 600);
            {
                let mut e = entry.lock().expect("child entry");
                e.info.status = status;
                e.info.report = Some(report);
            }
            let notice = format!("[child {label} {verb}]\n{}", snippet(&final_text, 2000));
            parent_inbox
                .lock()
                .expect("parent inbox")
                .push(notice.clone());
            if let Some(f) = fanout {
                f(&notice);
            }
        });
    }
}

impl LocalBus {
    /// Children inherit this environment unless they name another.
    pub fn with_environment(mut self, name: Option<String>) -> Self {
        self.environment = name;
        self
    }
}

#[async_trait::async_trait]
impl Bus for LocalBus {
    async fn spawn_child(&self, req: SpawnRequest) -> Result<SpawnHandle, String> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!(
            "sa-{:03}-{}",
            n,
            &uuid::Uuid::new_v4().simple().to_string()[..4]
        );
        let name = req.name.clone().unwrap_or_else(|| format!("child-{n}"));
        let ws = if req.mutation {
            self.workspace
                .create_worktree(&id)
                .map_err(|e| e.to_string())?
        } else {
            self.workspace.clone()
        };
        let session = SessionManager::create(&ws).map_err(|e| e.to_string())?;
        let session_id = session.header.id.clone();
        let root = ws.root().display().to_string();
        let provider = (self.provider_factory)()?;
        // A child in an environment gets its own instance of it, with its own
        // checkout (the worktree, for a mutation child) mounted inside.
        let (tools, environment) = match req.environment.as_ref().or(self.environment.as_ref()) {
            Some(env_name) => {
                let env = hasp_env::Environment::open(env_name, ws.root())
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                let label = format!("{env_name} ({})", env.id());
                (
                    hasp_env::with_environment(builtins_base(), &env),
                    Some(label),
                )
            }
            None => (builtins_base(), None),
        };
        let engine = Engine::new(
            ws,
            session,
            provider,
            tools,
            (self.gate_factory)(),
            self.model.clone(),
            self.max_tokens,
        )
        .map_err(|e| e.to_string())?;
        let engine = Arc::new(AsyncMutex::new(engine));
        let inbox = engine.lock().await.notice_inbox();

        let entry = Arc::new(Mutex::new(ChildEntry {
            info: ChildInfo {
                child_id: id.clone(),
                name: name.clone(),
                status: ChildStatus::Running,
                session_id: session_id.clone(),
                mutation: req.mutation,
                root: root.clone(),
                environment: environment.clone(),
                report: None,
            },
            engine: engine.clone(),
            inbox,
        }));
        self.children
            .lock()
            .expect("children")
            .insert(id.clone(), entry.clone());

        self.run_child(entry, req.task, false);
        Ok(SpawnHandle {
            child_id: id,
            session_id,
            root,
            environment,
        })
    }

    async fn message_child(
        &self,
        child_id: &str,
        text: &str,
        trigger: bool,
    ) -> Result<ChildReceipt, String> {
        let entry = self
            .children
            .lock()
            .expect("children")
            .get(child_id)
            .cloned()
            .ok_or_else(|| format!("unknown child {child_id} — agents lists ids"))?;
        let running = {
            let e = entry.lock().expect("child entry");
            e.info.status == ChildStatus::Running
        };
        if running || !trigger {
            // queued-and-never-awaited: delivered at the child's next boundary
            // (its turn-start drain picks this up)
            let inbox = entry.lock().expect("child entry").inbox.clone();
            inbox
                .lock()
                .expect("child inbox")
                .push(format!("[message from parent]\n{text}"));
            Ok(ChildReceipt::Queued)
        } else {
            self.run_child(entry, text.to_string(), true);
            Ok(ChildReceipt::Delivered)
        }
    }

    async fn list_children(&self) -> Vec<ChildInfo> {
        self.children
            .lock()
            .expect("children")
            .values()
            .map(|e| e.lock().expect("child entry").info.clone())
            .collect()
    }
}

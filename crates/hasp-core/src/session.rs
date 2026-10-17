//! Append-only JSONL session trees (research/03-architecture.md §4).
//! Entries form a tree via `id`/`parent_id`; the active leaf defines the
//! context path. Compactions carry `retained_tail` checkpoints so context
//! rebuilds never walk pre-compaction history.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use hasp_workspace::{Workspace, WorkspaceIdentity};
use serde::{Deserialize, Serialize};

use crate::{AgentMessage, ContentBlock, Usage, estimate_tokens};

/// `HASP_JOURNAL_FSYNC=0` skips the per-append fsync (tests, slow disks).
fn journal_fsync() -> bool {
    std::env::var("HASP_JOURNAL_FSYNC").as_deref() != Ok("0")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub format: u32,
    pub id: String,
    pub created_ms: i64,
    pub workspace: WorkspaceIdentity,
    pub parent_session: Option<PathBuf>,
    #[serde(default)]
    pub dirty_state: Option<hasp_workspace::DirtyState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        message: AgentMessage,
    },
    Compaction {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        summary: String,
        tokens_before: u64,
        retained_tail: Vec<AgentMessage>,
    },
    BranchSummary {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        summary: String,
        from_id: String,
    },
    SessionInfo {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        name: String,
    },
    ModelChange {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        provider: String,
        model: String,
    },
    Label {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        target_id: String,
        label: String,
    },
    Custom {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        custom_type: String,
        data: serde_json::Value,
    },
    Goal {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        action: String,
        text: String,
        budget_tokens: Option<u64>,
    },
    Autonomous {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        on: bool,
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
        gate: Option<String>,
    },
    Heartbeat {
        id: String,
        parent_id: Option<String>,
        ts: i64,
        action: String,
        hb_id: String,
        every_ms: Option<u64>,
        in_ms: Option<u64>,
        /// `#[serde(default)]`: journals written before cron existed load with
        /// `None`, which is simply "no cron schedule".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cron: Option<String>,
        instruction: String,
        mode: String,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Message { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::BranchSummary { id, .. }
            | Entry::SessionInfo { id, .. }
            | Entry::ModelChange { id, .. }
            | Entry::Label { id, .. }
            | Entry::Custom { id, .. }
            | Entry::Goal { id, .. }
            | Entry::Autonomous { id, .. }
            | Entry::Heartbeat { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Message { parent_id, .. }
            | Entry::Compaction { parent_id, .. }
            | Entry::BranchSummary { parent_id, .. }
            | Entry::SessionInfo { parent_id, .. }
            | Entry::ModelChange { parent_id, .. }
            | Entry::Label { parent_id, .. }
            | Entry::Custom { parent_id, .. }
            | Entry::Goal { parent_id, .. }
            | Entry::Autonomous { parent_id, .. }
            | Entry::Heartbeat { parent_id, .. } => parent_id.as_deref(),
        }
    }
}

/// Folding view of the `goal` entries: the latest action wins. Only the model
/// completes a goal (via the `goal_complete` tool); pause/resume/clear are
/// user surfaces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalState {
    pub text: String,
    pub budget_tokens: Option<u64>,
    pub paused: bool,
    pub completed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutoState {
    pub on: bool,
    pub max_turns: Option<u64>,
    pub max_tokens: Option<u64>,
    pub gate: Option<String>,
}

/// A heartbeat or one-shot schedule, folded from its entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatState {
    pub id: String,
    pub instruction: String,
    /// `steer` (deliver at the next turn boundary) or `follow_up` (own turn).
    pub mode: String,
    pub every_ms: Option<u64>,
    pub in_ms: Option<u64>,
    /// Five-field cron expression, for wall-clock schedules.
    pub cron: Option<String>,
    pub paused: bool,
    /// When it was (last) added: a one-shot is due at `set_ms + in_ms`.
    #[serde(default)]
    pub set_ms: i64,
}

pub struct SessionManager {
    file: Option<std::fs::File>,
    pub path: Option<PathBuf>,
    pub header: SessionHeader,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    leaf: Option<String>,
    /// Tool calls closed with an "interrupted" result when this session was
    /// opened — non-zero means the previous run was killed mid-tool.
    pub repaired_tool_calls: usize,
    /// Set when an append could not be made durable. The journal is canonical,
    /// so from then on nothing more is appended (in memory or on disk) and the
    /// engine stops before any further side effect: history must never claim
    /// less, or more, than what happened. Latched until the session is reopened.
    fault: Option<String>,
    /// Bytes of a torn final record found (and set aside) when opening.
    pub recovered_tail: Option<TornTail>,
    /// Test hook: appends left before writes start failing (`fail_appends_after`).
    fail_after: Option<usize>,
    /// Project instructions (AGENTS.md ...) as first read for this session. The
    /// system prompt must stay byte-stable within a session (prompt-cache
    /// contract), so an edit to AGENTS.md mid-session doesn't leak into the
    /// next request; `reload_instructions` adopts it deliberately.
    instructions: std::sync::OnceLock<Option<String>>,
}

/// A final journal line cut off mid-write (crash, full disk). Valid entries
/// before it are kept untouched; the partial bytes move to `sidecar`.
#[derive(Debug, Clone)]
pub struct TornTail {
    pub offset: u64,
    pub bytes: usize,
    pub sidecar: PathBuf,
}

impl SessionManager {
    pub fn in_memory(ws: &Workspace) -> Self {
        let header = SessionHeader {
            kind: "session".into(),
            format: 1,
            id: new_id(),
            created_ms: now_ms(),
            workspace: ws.identity().clone(),
            parent_session: None,
            dirty_state: ws.dirty_state(),
        };
        Self {
            file: None,
            path: None,
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
            repaired_tool_calls: 0,
            fault: None,
            recovered_tail: None,
            fail_after: None,
            instructions: Default::default(),
        }
    }

    pub fn create(ws: &Workspace) -> std::io::Result<Self> {
        let dir = hasp_workspace::sessions_root()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join(ws.session_key());
        std::fs::create_dir_all(&dir)?;
        let header = SessionHeader {
            kind: "session".into(),
            format: 1,
            id: new_id(),
            created_ms: now_ms(),
            workspace: ws.identity().clone(),
            parent_session: None,
            dirty_state: ws.dirty_state(),
        };
        let path = dir.join(format!("{}-{}.jsonl", header.created_ms, header.id));
        let mut file = std::fs::File::create(&path)?;
        writeln!(
            file,
            "{}",
            serde_json::to_string(&header).expect("header serializes")
        )?;
        file.flush()?;
        Ok(Self {
            file: Some(file),
            path: Some(path),
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf: None,
            repaired_tool_calls: 0,
            fault: None,
            recovered_tail: None,
            fail_after: None,
            instructions: Default::default(),
        })
    }

    /// Most recent persisted session for this workspace, if any.
    pub fn open_latest(ws: &Workspace) -> std::io::Result<Option<Self>> {
        let dir = hasp_workspace::sessions_root()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join(ws.session_key());
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut best: Option<(String, PathBuf)> = None;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".jsonl") && best.as_ref().is_none_or(|(b, _)| name > *b) {
                best = Some((name, e.path()));
            }
        }
        let Some((_, path)) = best else {
            return Ok(None);
        };
        let session = Self::open(&path)?;
        if session.header.workspace.root != ws.identity().root {
            return Ok(None);
        }
        if session.header.workspace.branch != ws.identity().branch
            || session.header.workspace.head != ws.identity().head
        {
            eprintln!(
                "warning: session started on {}@{}, now on {}@{}",
                session.header.workspace.branch.as_deref().unwrap_or("-"),
                session.header.workspace.head.as_deref().unwrap_or("-"),
                ws.identity().branch.as_deref().unwrap_or("-"),
                ws.identity().head.as_deref().unwrap_or("-")
            );
        }
        if let (Some(prev), Some(now)) = (&session.header.dirty_state, ws.dirty_state())
            && prev != &now
        {
            eprintln!(
                "warning: workspace dirtiness changed: was {} modified/{} untracked, now {}/{}",
                prev.modified, prev.untracked, now.modified, now.untracked
            );
        }
        Ok(Some(session))
    }

    /// Open a journal. Every complete line must parse: corruption in the
    /// middle is an error, never skipped. The one recoverable case is a torn
    /// *final* record (no trailing newline and not valid JSON: a crash or full
    /// disk mid-append). Its bytes are moved to `<file>.torn-<offset>`, the file
    /// is truncated back to the last complete entry, and `recovered_tail` says
    /// so. Complete entries are never rewritten.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut raw = String::from_utf8_lossy(&bytes).into_owned();
        let mut recovered_tail = None;
        if !raw.is_empty() && !raw.ends_with('\n') {
            let start = raw.rfind('\n').map_or(0, |i| i + 1);
            let tail = &raw[start..];
            if start > 0 && serde_json::from_str::<Entry>(tail).is_err() {
                let offset = start as u64;
                let sidecar = path.with_file_name(format!(
                    "{}.torn-{offset}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                std::fs::write(&sidecar, &bytes[start..])?;
                let f = std::fs::OpenOptions::new().write(true).open(path)?;
                f.set_len(offset)?;
                f.sync_all()?;
                recovered_tail = Some(TornTail {
                    offset,
                    bytes: bytes.len() - start,
                    sidecar,
                });
                raw.truncate(start);
            }
        }
        let mut lines = raw.lines();
        let header: SessionHeader = serde_json::from_str(
            lines
                .next()
                .ok_or_else(|| std::io::Error::other("empty session file"))?,
        )
        .map_err(|e| std::io::Error::other(format!("bad session header: {e}")))?;
        let mut entries = Vec::new();
        let mut index = HashMap::new();
        let mut leaf = None;
        for (n, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: Entry = serde_json::from_str(line).map_err(|e| {
                std::io::Error::other(format!(
                    "bad session entry on line {} of {}: {e} (mid-file corruption is not repaired automatically)",
                    n + 2,
                    path.display()
                ))
            })?;
            index.insert(entry.id().to_string(), entries.len());
            leaf = Some(entry.id().to_string());
            entries.push(entry);
        }
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        if !raw.is_empty() && !raw.ends_with('\n') {
            // A complete last entry that lost only its newline: terminate it so
            // the next append starts on its own line.
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        let mut session = Self {
            file: Some(file),
            path: Some(path.to_path_buf()),
            header,
            entries,
            index,
            leaf,
            repaired_tool_calls: 0,
            fault: None,
            recovered_tail,
            fail_after: None,
            instructions: Default::default(),
        };
        session.repaired_tool_calls = session.repair_dangling_tool_calls();
        Ok(session)
    }

    /// A session interrupted mid-tool ends with `tool_use` blocks whose results
    /// were never written; every later request would be rejected by the API
    /// ("tool_use ids were found without tool_result blocks"). Close those turns
    /// with an explicit interrupted result, journaled like any other outcome.
    pub fn repair_dangling_tool_calls(&mut self) -> usize {
        let Some(idx) = self.entries.iter().rposition(|e| {
            matches!(
                e,
                Entry::Message {
                    message: AgentMessage::Assistant { .. },
                    ..
                }
            )
        }) else {
            return 0;
        };
        let pending: Vec<(String, String)> = match &self.entries[idx] {
            Entry::Message {
                message: AgentMessage::Assistant { content, .. },
                ..
            } => content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if pending.is_empty() {
            return 0;
        }
        let answered: std::collections::HashSet<&str> = self.entries[idx + 1..]
            .iter()
            .filter_map(|e| match e {
                Entry::Message {
                    message: AgentMessage::ToolResult { tool_call_id, .. },
                    ..
                } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<(String, String)> = pending
            .into_iter()
            .filter(|(id, _)| !answered.contains(id.as_str()))
            .collect();
        let repaired = missing.len();
        for (id, name) in missing {
            self.append_message(AgentMessage::ToolResult {
                tool_call_id: id,
                tool_name: name,
                content: "[interrupted: this tool never reported a result —                           the session was closed while it was running]"
                    .into(),
                is_error: true,
                ts: now_ms(),
                images: Vec::new(),
            });
        }
        repaired
    }

    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.header.workspace
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn leaf(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    pub fn append_message(&mut self, message: AgentMessage) -> String {
        let id = new_id();
        self.write(Entry::Message {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            message,
        });
        id
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        tokens_before: u64,
        retained_tail: Vec<AgentMessage>,
    ) -> String {
        let id = new_id();
        self.write(Entry::Compaction {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            summary,
            tokens_before,
            retained_tail,
        });
        id
    }

    pub fn append_goal(&mut self, action: &str, text: &str, budget_tokens: Option<u64>) -> String {
        let id = new_id();
        self.write(Entry::Goal {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            action: action.to_string(),
            text: text.to_string(),
            budget_tokens,
        });
        id
    }

    pub fn append_autonomous(
        &mut self,
        on: bool,
        max_turns: Option<u64>,
        max_tokens: Option<u64>,
        gate: Option<&str>,
    ) -> String {
        let id = new_id();
        self.write(Entry::Autonomous {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            on,
            max_turns,
            max_tokens,
            gate: gate.map(str::to_string),
        });
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_heartbeat(
        &mut self,
        action: &str,
        hb_id: &str,
        every_ms: Option<u64>,
        in_ms: Option<u64>,
        cron: Option<&str>,
        instruction: &str,
        mode: &str,
    ) -> String {
        let id = new_id();
        self.write(Entry::Heartbeat {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            action: action.to_string(),
            hb_id: hb_id.to_string(),
            every_ms,
            in_ms,
            cron: cron.map(str::to_string),
            instruction: instruction.to_string(),
            mode: mode.to_string(),
        });
        id
    }

    /// Latest goal state, or `None` when the goal was cleared or never set.
    pub fn goal(&self) -> Option<GoalState> {
        let mut cur: Option<GoalState> = None;
        for e in &self.entries {
            let Entry::Goal {
                action,
                text,
                budget_tokens,
                ..
            } = e
            else {
                continue;
            };
            match action.as_str() {
                "set" => {
                    cur = Some(GoalState {
                        text: text.clone(),
                        budget_tokens: *budget_tokens,
                        paused: false,
                        completed: false,
                    })
                }
                "pause" => cur.iter_mut().for_each(|g| g.paused = true),
                "resume" => cur.iter_mut().for_each(|g| g.paused = false),
                "complete" => cur.iter_mut().for_each(|g| g.completed = true),
                "clear" => cur = None,
                _ => {}
            }
        }
        cur
    }

    /// Latest autonomous-mode state (the last entry wins outright).
    pub fn autonomous(&self) -> Option<AutoState> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Autonomous {
                on,
                max_turns,
                max_tokens,
                gate,
                ..
            } => Some(AutoState {
                on: *on,
                max_turns: *max_turns,
                max_tokens: *max_tokens,
                gate: gate.clone(),
            }),
            _ => None,
        })
    }

    /// Heartbeats and one-shots, folded by id in first-seen order.
    pub fn heartbeats(&self) -> Vec<HeartbeatState> {
        let mut out: Vec<HeartbeatState> = Vec::new();
        for e in &self.entries {
            let Entry::Heartbeat {
                action,
                hb_id,
                every_ms,
                in_ms,
                cron,
                instruction,
                mode,
                ts,
                ..
            } = e
            else {
                continue;
            };
            match action.as_str() {
                "add" => {
                    let hb = HeartbeatState {
                        id: hb_id.clone(),
                        instruction: instruction.clone(),
                        mode: mode.clone(),
                        every_ms: *every_ms,
                        in_ms: *in_ms,
                        cron: cron.clone(),
                        paused: false,
                        set_ms: *ts,
                    };
                    match out.iter_mut().find(|h| h.id == *hb_id) {
                        Some(slot) => *slot = hb,
                        None => out.push(hb),
                    }
                }
                "pause" => out
                    .iter_mut()
                    .filter(|h| h.id == *hb_id)
                    .for_each(|h| h.paused = true),
                "resume" => out
                    .iter_mut()
                    .filter(|h| h.id == *hb_id)
                    .for_each(|h| h.paused = false),
                // "fired": a one-shot was handed to the engine (journaled before
                // it runs, so a restart never fires it twice). "expired": its
                // time passed while the daemon was down, so it was dropped.
                "clear" | "fired" | "expired" => out.retain(|h| h.id != *hb_id),
                _ => {}
            }
        }
        out
    }

    pub fn append_session_info(&mut self, name: &str) -> String {
        let id = new_id();
        self.write(Entry::SessionInfo {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            name: name.to_string(),
        });
        id
    }

    pub fn append_model_change(&mut self, provider: &str, model: &str) -> String {
        let id = new_id();
        self.write(Entry::ModelChange {
            id: id.clone(),
            parent_id: self.leaf.clone(),
            ts: now_ms(),
            provider: provider.to_string(),
            model: model.to_string(),
        });
        id
    }

    /// Fork: clone this session to a new file (new id, parent pointer) — a
    /// snapshot before risky work. Context is identical.
    pub fn clone_session(&self, ws: &Workspace) -> std::io::Result<SessionManager> {
        let dir = hasp_workspace::sessions_root()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join(ws.session_key());
        std::fs::create_dir_all(&dir)?;
        let header = SessionHeader {
            kind: "session".into(),
            format: 1,
            id: new_id(),
            created_ms: now_ms(),
            workspace: ws.identity().clone(),
            parent_session: self.path.clone(),
            dirty_state: ws.dirty_state(),
        };
        let path = dir.join(format!("{}-{}.jsonl", header.created_ms, header.id));
        let mut file = std::fs::File::create(&path)?;
        writeln!(
            file,
            "{}",
            serde_json::to_string(&header).expect("header serializes")
        )?;
        for entry in &self.entries {
            writeln!(
                file,
                "{}",
                serde_json::to_string(entry).expect("entry serializes")
            )?;
        }
        file.flush()?;
        let mut index = HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            index.insert(e.id().to_string(), i);
        }
        Ok(SessionManager {
            file: Some(file),
            path: Some(path),
            header,
            entries: self.entries.clone(),
            index,
            leaf: self.leaf.clone(),
            repaired_tool_calls: 0,
            fault: None,
            recovered_tail: None,
            fail_after: None,
            instructions: Default::default(),
        })
    }

    /// Rewind targets: user messages and compaction checkpoints with entry ids.
    pub fn branch_points(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Message {
                    id,
                    message: AgentMessage::User { content, .. },
                    ..
                } => Some((id.clone(), first_chars(content))),
                Entry::Compaction { .. } => {
                    Some((e.id().to_string(), "[compaction checkpoint]".to_string()))
                }
                _ => None,
            })
            .collect()
    }

    /// Move the active leaf to an earlier entry; subsequent appends branch there.
    pub fn branch(&mut self, id: &str) -> bool {
        if self.index.contains_key(id) {
            self.leaf = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// The instruction snapshot for this session, loading it on first use.
    pub fn instructions_with(&self, load: impl FnOnce() -> Option<String>) -> Option<&str> {
        self.instructions.get_or_init(load).as_deref()
    }

    /// Drop the snapshot: the next request re-reads AGENTS.md (one deliberate
    /// prompt-cache break, e.g. after the user edited the instructions).
    pub fn reload_instructions(&mut self) {
        self.instructions = std::sync::OnceLock::new();
    }

    /// Why the journal stopped accepting entries, if it did.
    pub fn fault(&self) -> Option<&str> {
        self.fault.as_deref()
    }

    /// `Err` once an append has failed: callers about to cause a side effect
    /// (a model request, a tool run) must check this first.
    pub fn ensure_durable(&self) -> std::io::Result<()> {
        match &self.fault {
            Some(f) => Err(std::io::Error::other(format!("session journal is not durable: {f}"))),
            None => Ok(()),
        }
    }

    /// Append one entry. It joins the in-memory history only after the whole
    /// line is written and synced; on failure the session faults instead
    /// (see `fault`) and a partially written line is cut off again.
    fn write(&mut self, entry: Entry) {
        if self.fault.is_some() {
            return;
        }
        let id = entry.id().to_string();
        if let Some(f) = &mut self.file {
            let mut line = serde_json::to_string(&entry).expect("entry serializes");
            line.push('\n');
            let before = f.metadata().map(|m| m.len()).ok();
            let injected = match &mut self.fail_after {
                Some(0) => true,
                Some(n) => {
                    *n -= 1;
                    false
                }
                None => false,
            };
            let res = if injected {
                // a partial write, then the error: what a full disk looks like
                f.write_all(&line.as_bytes()[..line.len() / 2])
                    .and_then(|_| Err(std::io::Error::other("injected write failure")))
            } else {
                f.write_all(line.as_bytes())
            }
                .and_then(|_| f.flush())
                .and_then(|_| if journal_fsync() { f.sync_data() } else { Ok(()) });
            if let Err(e) = res {
                if let Some(len) = before {
                    let _ = f.set_len(len); // don't leave half a record behind
                }
                self.fault = Some(format!("append failed: {e}"));
                return;
            }
        }
        self.index.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf = Some(id);
    }

    /// Test hook: let `n` more appends succeed, then fail every write (after
    /// writing half a line, like a full disk).
    #[doc(hidden)]
    pub fn fail_appends_after(&mut self, n: usize) {
        self.fail_after = Some(n);
    }

    /// Path from root to the active leaf, root-first.
    fn path_entries(&self) -> Vec<&Entry> {
        let mut path = Vec::new();
        let mut cur = self.leaf.clone();
        while let Some(id) = cur {
            match self.index.get(&id) {
                Some(&i) => {
                    let e = &self.entries[i];
                    cur = e.parent_id().map(|s| s.to_string());
                    path.push(e);
                }
                None => break,
            }
        }
        path.reverse();
        path
    }

    /// Active context messages, honoring the latest compaction checkpoint on
    /// the path: everything before it is replaced by its summary + tail.
    pub fn context_messages(&self) -> Vec<AgentMessage> {
        let path = self.path_entries();
        let reset = path
            .iter()
            .rposition(|e| matches!(e, Entry::Compaction { .. }));
        let mut out = Vec::new();
        for (i, e) in path.iter().enumerate() {
            let active = reset.is_none_or(|r| i > r);
            match e {
                Entry::Compaction {
                    summary,
                    retained_tail,
                    ts,
                    ..
                } if Some(i) == reset => {
                    out.push(AgentMessage::User {
                        content: format!(
                            "[Continuation summary of earlier session history]\n\n{summary}"
                        ),
                        ts: *ts,
                        images: Vec::new(),
                    });
                    out.extend(retained_tail.iter().cloned());
                }
                Entry::Message { message, .. } if active => out.push(message.clone()),
                Entry::BranchSummary { summary, ts, .. } if active => {
                    out.push(AgentMessage::User {
                        content: format!("[Summary of the branch left behind]\n\n{summary}"),
                        ts: *ts,
                        images: Vec::new(),
                    });
                }
                _ => {}
            }
        }
        out
    }

    pub fn context_est_tokens(&self) -> u64 {
        estimate_tokens(&self.context_messages())
    }

    /// Input+output tokens the provider reported for the most recent sample, when
    /// nothing has been compacted since — that is the real size of what the model
    /// last saw, as opposed to an estimate. `None` after a compaction, because the
    /// context has changed shape since the number was produced.
    pub fn last_input_tokens(&self) -> Option<u64> {
        let assistant = self.entries.iter().rposition(|e| {
            matches!(
                e,
                Entry::Message {
                    message: AgentMessage::Assistant { .. },
                    ..
                }
            )
        })?;
        let compacted = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Compaction { .. }))
            .is_some_and(|c| c > assistant);
        if compacted {
            return None;
        }
        match &self.entries[assistant] {
            Entry::Message {
                message: AgentMessage::Assistant { usage, .. },
                ..
            } => Some(usage.total_in() + usage.output),
            _ => None,
        }
    }

    pub fn totals(&self) -> Usage {
        let mut t = Usage::default();
        for e in &self.entries {
            if let Entry::Message {
                message: AgentMessage::Assistant { usage, .. },
                ..
            } = e
            {
                t.add(usage);
            }
        }
        t
    }

    pub fn name(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::SessionInfo { name, .. } => Some(name.clone()),
            _ => None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SessionListing {
    pub created_ms: i64,
    pub id: String,
    pub name: Option<String>,
    pub message_entries: usize,
    pub path: PathBuf,
}

impl SessionManager {
    /// Sessions for this workspace, newest first.
    pub fn list_for(ws: &Workspace) -> std::io::Result<Vec<SessionListing>> {
        let dir = hasp_workspace::sessions_root()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .join(ws.session_key());
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = raw.lines();
            let Some(header_line) = lines.next() else {
                continue;
            };
            let Ok(header) = serde_json::from_str::<SessionHeader>(header_line) else {
                continue;
            };
            let mut message_entries = 0;
            let mut name = None;
            for line in lines {
                let Ok(entry) = serde_json::from_str::<Entry>(line) else {
                    continue;
                };
                match entry {
                    Entry::Message { .. } => message_entries += 1,
                    Entry::SessionInfo { name: n, .. } => name = Some(n),
                    _ => {}
                }
            }
            out.push(SessionListing {
                created_ms: header.created_ms,
                id: header.id,
                name,
                message_entries,
                path,
            });
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        Ok(out)
    }
}

fn first_chars(s: &str) -> String {
    let mut t: String = s.chars().take(48).collect();
    if s.chars().count() > 48 {
        t.push('…');
    }
    t.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, StopReason};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User {
            content: text.into(),
            ts: 1,
            images: Vec::new(),
        }
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text { text: text.into() }],
            provider: "mock".into(),
            model: "m".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            ts: 2,
        }
    }

    fn ws_in(path: &Path) -> Workspace {
        Workspace::resolve(path).unwrap()
    }

    #[test]
    fn linear_context_is_root_first() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = SessionManager::in_memory(&ws_in(tmp.path()));
        s.append_message(user("a"));
        s.append_message(assistant("b"));
        s.append_message(user("c"));
        let msgs = s.context_messages();
        assert_eq!(msgs.len(), 3);
        assert!(matches!(&msgs[0], AgentMessage::User { content, .. } if content == "a"));
    }

    #[test]
    fn compaction_resets_context_with_retained_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = SessionManager::in_memory(&ws_in(tmp.path()));
        s.append_message(user("old1"));
        s.append_message(assistant("old2"));
        let tail = vec![user("recent1"), assistant("recent2")];
        s.append_compaction("the brief".into(), 10_000, tail);
        s.append_message(user("new"));
        let msgs = s.context_messages();
        assert_eq!(msgs.len(), 4); // summary + 2 tail + new
        match &msgs[0] {
            AgentMessage::User { content, .. } => assert!(content.contains("the brief")),
            other => panic!("expected summary user message, got {other:?}"),
        }
        assert!(matches!(&msgs[3], AgentMessage::User { content, .. } if content == "new"));
    }

    #[test]
    fn branch_switches_active_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = SessionManager::in_memory(&ws_in(tmp.path()));
        let first = s.append_message(user("first"));
        s.append_message(assistant("response"));
        s.branch(&first);
        s.append_message(user("alternative"));
        let msgs = s.context_messages();
        assert_eq!(msgs.len(), 2); // first + alternative; "response" left behind
        assert!(matches!(&msgs[1], AgentMessage::User { content, .. } if content == "alternative"));
    }

    #[test]
    fn persisted_session_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ws_in(tmp.path());
        let mut s = SessionManager::create(&ws).unwrap();
        s.append_message(user("hello"));
        s.append_message(assistant("hi"));
        let path = s.path.clone().unwrap();

        let reopened = SessionManager::open(&path).unwrap();
        assert_eq!(reopened.context_messages().len(), 2);
        assert_eq!(reopened.leaf(), s.leaf());

        let mut latest = SessionManager::open_latest(&ws).unwrap().unwrap();
        assert_eq!(latest.context_messages().len(), 2);
        latest.append_message(user("more"));
        assert_eq!(latest.context_messages().len(), 3);
    }

    #[test]
    fn fork_snapshots_and_branch_points_list() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ws_in(tmp.path());
        let mut s = SessionManager::create(&ws).unwrap();
        s.append_message(user("one"));
        s.append_message(assistant("two"));
        s.append_message(user("three"));

        let points = s.branch_points();
        assert_eq!(points.len(), 2);
        assert!(points[0].1.contains("one"));
        assert!(points[1].1.contains("three"));

        let fork = s.clone_session(&ws).unwrap();
        assert_ne!(fork.header.id, s.header.id);
        assert_eq!(fork.context_messages().len(), 3);
        assert_eq!(fork.header.parent_session.as_deref(), s.path.as_deref());
        // the fork is a real persisted session: openable again
        let path = fork.path.clone().unwrap();
        let reopened = SessionManager::open(&path).unwrap();
        assert_eq!(reopened.context_messages().len(), 3);
    }
}

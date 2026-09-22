//! Full-screen TUI (ratatui): streaming scrollback, input editor + history,
//! message queue while a turn runs, status footer, in-UI approval prompts,
//! slash commands. Operates through a `Backend` (embedded engine or the
//! daemon wire) with one JSON command path.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use harness_core::Usage;
use harness_core::compaction::CompactStats;
use harness_core::engine::ToolEvent;
use harness_core::engine::TurnStats;
use harness_provider::catalog::{self, ModelInfo};

use crate::backend::Backend;
use crate::gate::PromptRequest;
use serde_json::{Value, json};

/// Seed a temp file with `initial`, run `editor` on it, and return what came
/// back. Terminal handoff is the caller's business, which keeps this testable.
fn edit_text_with(editor: &str, initial: &str) -> Option<String> {
    let path = std::env::temp_dir().join(format!(
        "harness-prompt-{}.md",
        harness_core::session::now_ms()
    ));
    std::fs::write(&path, initial).ok()?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status();
    let out = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    match status {
        Ok(s) if s.success() => out,
        _ => None,
    }
}

/// `$VISUAL`, then `$EDITOR`, then `vi` — the usual precedence.
fn edit_text(initial: &str) -> Option<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    edit_text_with(&editor, initial)
}

/// Ring the terminal bell when a turn ran at least this long.
const LONG_TURN_MS: u128 = 30_000;

/// Body lines shown for a collapsed tool block before ctrl+o expands it.
const TOOL_BODY_PREVIEW: usize = 6;

/// Single source for the slash commands: drives `/help` and the completion menu.
const COMMANDS: &[(&str, &str)] = &[
    (
        "autonomous",
        "keep working until a gate passes: on|off [--max-turns N] [--max-tokens N] [--gate \"cmd\"]",
    ),
    (
        "branch",
        "list rewind points, or move there: /branch <n|id>",
    ),
    ("clear", "clear the scrollback"),
    ("compact", "summarize older history into a 9-section brief"),
    ("exit", "quit"),
    ("fork", "snapshot this session to a new file"),
    (
        "goal",
        "persistent objective: <text>|status|pause|resume|clear [--budget N|--unlimited]",
    ),
    (
        "heartbeat",
        "scheduled task: every 10m <text> · in 30m <text> · cron 0 9 * * 1-5 <text> · list|pause|resume|clear",
    ),
    ("help", "this list"),
    ("model", "list models, or switch: /model <id>"),
    ("name", "name this session: /name <text>"),
    ("new", "start a fresh session"),
    (
        "resume",
        "list sessions for this workspace, or open one: /resume <n>",
    ),
    ("stats", "tokens, cache R/W, cost, context, entries"),
    (
        "theme",
        "color palette: light or dark (default dark; HARNESS_THEME too)",
    ),
    (
        "worktree",
        "isolate this session in a git worktree: /worktree [name]",
    ),
    (
        "worktree-exit",
        "back to the main checkout: /worktree-exit [rm]",
    ),
];

/// One row of the completion menu.
enum MenuItem {
    Command(&'static str, &'static str),
    File(String),
    /// `/model <id>` — id plus a short descriptor.
    Model(String, String),
    /// `/resume <n>` — 1-based index plus a label.
    Session(usize, String),
}

pub enum UiEvent {
    Delta(String),
    Note(String),
    /// Prior turns replayed when a session opens: (is_user, text).
    History(Vec<(bool, String)>),
    /// Reasoning streamed by a thinking model.
    Thinking(String),
    /// A tool finished: name, subject, output, status.
    Tool(ToolEvent),
    Done(Result<DoneKind, String>),
}

pub enum DoneKind {
    Turn(TurnStats),
    Compact(CompactStats),
}

#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    User,
    Assistant,
    Note,
    Error,
    Meta,
    /// Tool call header/output: dim, with +/- lines coloured like a diff.
    Tool,
    /// The model's reasoning: dim and italic, kept out of the way.
    Thinking,
}

/// A transcript entry. Styling is baked in at push time (markdown and diffs
/// produce multi-span lines), so a frame never re-parses the transcript; tool
/// output and reasoning are bodies that expand on demand with ctrl+o.
enum Entry {
    Line(Line<'static>),
    Body {
        header: Line<'static>,
        body: Vec<Line<'static>>,
        /// Body lines shown while collapsed.
        preview: usize,
        expanded: bool,
    },
}

impl Entry {
    fn shown(&self) -> usize {
        match self {
            Entry::Line(_) => 1,
            Entry::Body {
                body,
                preview,
                expanded,
                ..
            } => {
                1 + if *expanded {
                    body.len()
                } else {
                    (*preview).min(body.len())
                }
            }
        }
    }

    fn hidden(&self) -> usize {
        match self {
            Entry::Body {
                body,
                preview,
                expanded,
                ..
            } if !*expanded => body.len().saturating_sub(*preview),
            _ => 0,
        }
    }

    fn height(&self) -> usize {
        self.shown() + usize::from(self.hidden() > 0)
    }

    fn append(&self, out: &mut Vec<Line<'static>>) {
        match self {
            Entry::Line(line) => out.push(line.clone()),
            Entry::Body {
                header,
                body,
                preview,
                expanded,
            } => {
                out.push(header.clone());
                let n = if *expanded {
                    body.len()
                } else {
                    (*preview).min(body.len())
                };
                out.extend(body.iter().take(n).cloned());
                if self.hidden() > 0 {
                    out.push(Line::from(Span::styled(
                        format!("  … {} more line(s) · ctrl+o", self.hidden()),
                        Style::new().dim(),
                    )));
                }
            }
        }
    }
}

struct Footer {
    model: String,
    /// Whether the context figure came from the provider rather than the estimate.
    ctx_measured: bool,
    branch: String,
    root: String,
    /// `/help` overlay is drawn instead of dumping lines into the transcript.
    help_open: bool,
    window: u64,
    est: u64,
    totals: Usage,
    cost: Option<f64>,
    entries: usize,
}

struct App {
    backend: Box<dyn Backend>,
    catalog: Vec<ModelInfo>,
    entries: Vec<Entry>,
    current: String,
    /// Reasoning for the turn in flight; moved into the transcript when it ends.
    thinking: String,
    /// `--ascii`: swap box/bullet glyphs for terminals that misrender them.
    ascii: bool,
    input: String,
    /// Byte offset of the caret inside `input`.
    cursor: usize,
    /// Highlighted row of the completion menu.
    completion_idx: usize,
    /// Set by ctrl+g; `run` owns the terminal, so it performs the handoff.
    request_editor: bool,
    /// Workspace files for `@` completion, fetched once on first use.
    files: Vec<String>,
    files_loaded: bool,
    history: Vec<String>,
    history_idx: Option<usize>,
    queue: VecDeque<String>,
    running: bool,
    scroll_up: u16,
    pending_prompt: Option<PromptRequest>,
    last_sessions: Vec<Value>,
    last_branch_points: Vec<String>,
    footer: Footer,
    should_quit: bool,
}

impl App {
    /// Constructed in one place so `run` and the tests cannot drift.
    fn new(backend: Box<dyn Backend>, catalog: Vec<ModelInfo>, ascii: bool) -> Self {
        Self {
            backend,
            catalog,
            entries: Vec::new(),
            current: String::new(),
            thinking: String::new(),
            cursor: 0,
            request_editor: false,
            files: Vec::new(),
            files_loaded: false,
            completion_idx: 0,
            ascii,
            input: String::new(),
            history: Vec::new(),
            history_idx: None,
            queue: VecDeque::new(),
            running: false,
            scroll_up: 0,
            pending_prompt: None,
            last_sessions: Vec::new(),
            last_branch_points: Vec::new(),
            footer: Footer {
                model: String::new(),
                ctx_measured: false,
                branch: String::new(),
                root: String::new(),
                help_open: false,
                window: 0,
                est: 0,
                totals: Usage::default(),
                cost: None,
                entries: 0,
            },
            should_quit: false,
        }
    }

    fn push(&mut self, text: impl Into<String>, kind: LineKind) {
        self.push_line(Line::styled(text.into(), style_for(kind)), kind);
    }

    fn push_line(&mut self, line: Line<'static>, _kind: LineKind) {
        self.entries.push(Entry::Line(line));
    }

    /// Assistant output arrives as markdown; tool output is raw text that may
    /// contain a diff.
    fn push_rich(&mut self, text: &str, kind: LineKind) {
        let base = style_for(kind);
        let lines = if kind == LineKind::Assistant {
            crate::markdown::render(text, base, self.ascii)
        } else {
            render_plain(text, base)
        };
        for line in lines {
            self.entries.push(Entry::Line(line));
        }
    }

    /// One tool call: what ran, what it printed, whether it failed.
    fn push_tool(&mut self, ev: &ToolEvent) {
        let mark = match (ev.is_error, self.ascii) {
            (true, true) => "x",
            (true, false) => "✗",
            (false, true) => "ok",
            (false, false) => "✓",
        };
        let subject = if ev.args_preview.is_empty() {
            String::new()
        } else {
            format!(": {}", ev.args_preview)
        };
        let header = Line::styled(
            format!("{mark} {}{subject}  ({} ms)", ev.name, ev.ms),
            style_for(if ev.is_error {
                LineKind::Error
            } else {
                LineKind::Tool
            }),
        );
        let body = render_plain(&ev.content_preview, style_for(LineKind::Tool));
        self.entries.push(Entry::Body {
            header,
            body,
            preview: TOOL_BODY_PREVIEW,
            expanded: false,
        });
    }

    fn user_line(&mut self, text: &str) {
        let marker = if self.ascii { "> " } else { "▸ " };
        self.push(format!("{marker}{text}"), LineKind::User);
    }

    fn flush_current(&mut self) {
        self.flush_thinking();
        if !self.current.trim().is_empty() {
            let done = std::mem::take(&mut self.current);
            self.push_rich(done.trim_end(), LineKind::Assistant);
        }
        self.current.clear();
    }

    /// The streaming message, rendered as markdown each frame so fences and
    /// emphasis snap into place as they complete; reasoning streams above it.
    fn current_lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if !self.thinking.trim().is_empty() {
            for line in self.thinking.trim_end().lines() {
                out.push(Line::from(Span::styled(
                    format!("  {line}"),
                    style_for(LineKind::Thinking),
                )));
            }
        }
        if !self.current.trim().is_empty() {
            out.extend(crate::markdown::render(
                self.current.trim_end(),
                style_for(LineKind::Assistant),
                self.ascii,
            ));
        }
        out
    }

    /// Move streamed reasoning into the transcript when the turn ends.
    fn flush_thinking(&mut self) {
        let text = std::mem::take(&mut self.thinking);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let lines = text.lines().count();
        let marker = if self.ascii { "-" } else { "▸" };
        self.entries.push(Entry::Body {
            header: Line::styled(
                format!("{marker} thinking · {lines} line(s)"),
                style_for(LineKind::Thinking),
            ),
            body: text
                .lines()
                .map(|l| Line::styled(format!("  {l}"), style_for(LineKind::Thinking)))
                .collect(),
            preview: 0,
            expanded: false,
        });
    }

    /// Expand or collapse the newest block (tool output or reasoning).
    fn toggle_last_body(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            if let Entry::Body { expanded, .. } = entry {
                *expanded = !*expanded;
                return;
            }
        }
    }

    fn refresh_footer(&mut self) {
        let Ok(s) = self.backend.call("session/stats", json!({})) else {
            return;
        };
        let model = s["model"].as_str().unwrap_or("?").to_string();
        let branch = s["branch"].as_str().unwrap_or("-").to_string();
        let root = s["root"].as_str().unwrap_or("").to_string();
        let window = s["window"].as_u64().unwrap_or(0);
        let est = s["estTokens"].as_u64().unwrap_or(0);
        // The provider's own count when available; `est` is the fallback.
        let est = s["ctx"]["tokens"].as_u64().unwrap_or(est);
        let ctx_measured = s["ctx"]["measured"].as_bool().unwrap_or(false);
        let totals = Usage {
            input: s["input"].as_u64().unwrap_or(0),
            cache_read: s["cacheRead"].as_u64().unwrap_or(0),
            cache_write: s["cacheWrite"].as_u64().unwrap_or(0),
            output: s["output"].as_u64().unwrap_or(0),
        };
        let entries = s["entries"].as_u64().unwrap_or(0) as usize;
        let cost = catalog::find(&self.catalog, &model).and_then(|m| catalog::cost_of(m, &totals));
        self.footer = Footer {
            model,
            ctx_measured,
            branch,
            root,
            window,
            est,
            totals,
            cost,
            entries,
            help_open: self.footer.help_open,
        };
    }

    fn spawn_turn(&mut self, prompt: String) {
        self.running = true;
        self.backend.submit_prompt(prompt);
    }

    /// Replay of prior turns: user lines echo like typed input, assistant text
    /// goes through the markdown renderer like a live reply.
    fn push_history(&mut self, lines: &[(bool, String)]) {
        for (is_user, text) in lines {
            if *is_user {
                self.push(format!("▸ {text}"), LineKind::User);
            } else if !text.is_empty() {
                self.push_rich(text, LineKind::Assistant);
            }
        }
    }

    fn on_done(&mut self, res: Result<DoneKind, String>) {
        self.running = false;
        self.flush_current();
        match &res {
            Ok(DoneKind::Turn(stats)) => {
                // A long turn usually means you looked away: ring the bell.
                if stats.ms >= LONG_TURN_MS && !stats.aborted {
                    eprint!("\x07");
                }
                self.push(
                    format!(
                        "─ {} sample(s), {} tool call(s), {} ms{}",
                        stats.samples,
                        stats.tool_calls,
                        stats.ms,
                        if stats.aborted { " · cancelled" } else { "" },
                    ),
                    LineKind::Note,
                )
            }
            Ok(DoneKind::Compact(cs)) => self.push(
                format!(
                    "▪ compacted ~{}k tokens → brief + {} retained message(s)",
                    cs.tokens_before / 1000,
                    cs.retained_messages
                ),
                LineKind::Note,
            ),
            Err(e) => self.push(format!("error: {e}"), LineKind::Error),
        }
        if let Some(next) = self.queue.pop_front() {
            if next == "/exit" {
                self.should_quit = true;
                return;
            }
            self.user_line(&next);
            self.spawn_turn(next);
        } else {
            self.refresh_footer();
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if self.pending_prompt.is_some() {
            let answer = match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => 'y',
                KeyCode::Char('a') | KeyCode::Char('A') => 'a',
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => 'n',
                _ => return,
            };
            if let Some(req) = self.pending_prompt.take() {
                let _ = req.respond.send(answer);
                self.push(
                    format!(
                        "{} {}",
                        if answer == 'n' {
                            "✗ declined"
                        } else {
                            "✓ approved"
                        },
                        req.question
                    ),
                    LineKind::Note,
                );
            }
            return;
        }
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.running {
            match code {
                KeyCode::Enter => {
                    let text = self.input.trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    if text == "/exit" {
                        self.should_quit = true;
                        return;
                    }
                    self.history.push(text.clone());
                    self.history_idx = None;
                    self.input.clear();
                    self.push(format!("(queued) {text}"), LineKind::Note);
                }
                KeyCode::Esc => {
                    self.backend.cancel();
                    self.push("▪ cancelling…", LineKind::Note);
                }
                KeyCode::PageUp => self.scroll_up = self.scroll_up.saturating_add(10),
                KeyCode::PageDown => self.scroll_up = self.scroll_up.saturating_sub(10),
                _ => self.edit_input(code, mods),
            }
            return;
        }
        match code {
            KeyCode::Enter => self.submit(),
            KeyCode::Up => {
                if !self.menu().is_empty() {
                    self.completion_idx = self.completion_idx.saturating_sub(1);
                } else if !self.history.is_empty() {
                    let idx = self
                        .history_idx
                        .map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                    self.history_idx = Some(idx);
                    self.input = self.history[idx].clone();
                    self.cursor = self.input.len();
                }
            }
            KeyCode::Down => {
                let menu = self.menu().len();
                if menu > 0 {
                    self.completion_idx = (self.completion_idx + 1).min(menu - 1);
                } else if let Some(idx) = self.history_idx {
                    if idx + 1 < self.history.len() {
                        self.history_idx = Some(idx + 1);
                        self.input = self.history[idx + 1].clone();
                        self.cursor = self.input.len();
                    } else {
                        self.history_idx = None;
                        self.input.clear();
                        self.cursor = 0;
                    }
                }
            }
            KeyCode::Tab => {
                self.complete();
            }
            KeyCode::PageUp => self.scroll_up = self.scroll_up.saturating_add(10),
            KeyCode::PageDown => self.scroll_up = self.scroll_up.saturating_sub(10),
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
                self.completion_idx = 0;
            }
            _ => self.edit_input(code, mods),
        }
    }

    /// The menu under the caret: commands for a bare `/word`, files for `@token`.
    fn menu(&self) -> Vec<MenuItem> {
        let head = self.input.trim();
        // A picker-specific submenu wins over the command list: these commands
        // take a value, so show the values.
        match head {
            "/model" => {
                return self
                    .catalog
                    .iter()
                    .map(|m| {
                        let price = match (m.input_per_mtok, m.output_per_mtok) {
                            (Some(i), Some(o)) => format!("  ${i}/${o} per MTok"),
                            _ => String::new(),
                        };
                        MenuItem::Model(
                            m.id.clone(),
                            format!("{}k window{price}", m.context_window / 1000),
                        )
                    })
                    .collect();
            }
            "/resume" => {
                return self
                    .last_sessions
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        MenuItem::Session(
                            i + 1,
                            format!(
                                "{}  {}  {} entries",
                                s["id"].as_str().unwrap_or("?"),
                                s["name"].as_str().unwrap_or("-"),
                                s["entries"].as_u64().unwrap_or(0),
                            ),
                        )
                    })
                    .collect();
            }
            _ => {}
        }
        let word = head.split_whitespace().next().unwrap_or_default();
        if word.starts_with('/') && head.len() == word.len() {
            return completions_for(&self.input)
                .into_iter()
                .map(|(name, description)| MenuItem::Command(name, description))
                .collect();
        }
        if let Some(token) = self.file_token() {
            let needle = token.to_ascii_lowercase();
            let mut hits: Vec<&String> = self
                .files
                .iter()
                .filter(|f| f.to_ascii_lowercase().contains(&needle))
                .collect();
            // Shortest and prefix-matching paths first — the usual intent.
            hits.sort_by_key(|f| {
                let lower = f.to_ascii_lowercase();
                (!lower.starts_with(&needle), lower.len())
            });
            return hits
                .into_iter()
                .take(8)
                .map(|f| MenuItem::File(f.clone()))
                .collect();
        }
        Vec::new()
    }

    /// The `@token` before the caret, if the caret is in one.
    fn file_token(&self) -> Option<String> {
        let upto = &self.input[..self.cursor];
        let start = upto.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        upto[start..].strip_prefix('@').map(str::to_string)
    }

    fn wants_files(&self) -> bool {
        self.file_token().is_some()
    }

    /// The `/resume` picker needs the session list; fetch it once.
    fn wants_sessions(&self) -> bool {
        self.input.trim() == "/resume" && self.last_sessions.is_empty()
    }

    fn load_sessions(&mut self) {
        if let Ok(v) = self.backend.call("session/listings", json!({}))
            && let Some(list) = v.as_array()
        {
            self.last_sessions = list.clone();
        }
    }

    /// One call, cached: the file list comes from the backend, which owns the
    /// workspace (the TUI deliberately has no filesystem access of its own).
    fn load_files(&mut self) {
        self.files_loaded = true;
        if let Ok(v) = self.backend.call("workspace/files", json!({"limit": 4000}))
            && let Some(list) = v["files"].as_array()
        {
            self.files = list
                .iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect();
        }
    }

    /// Tab accepts the highlighted row.
    fn complete(&mut self) -> bool {
        let items = self.menu();
        if items.is_empty() {
            return false;
        }
        match &items[self.completion_idx.min(items.len() - 1)] {
            MenuItem::Model(id, _) => {
                self.input = format!("/model {id} ");
                self.cursor = self.input.len();
            }
            MenuItem::Session(n, _) => {
                self.input = format!("/resume {n} ");
                self.cursor = self.input.len();
            }
            MenuItem::Command(name, _) => {
                self.input = format!("/{name}");
                self.cursor = self.input.len();
            }
            MenuItem::File(path) => {
                let start = self.input[..self.cursor]
                    .rfind(char::is_whitespace)
                    .map_or(0, |i| i + 1);
                let end = self.input[self.cursor..]
                    .find(char::is_whitespace)
                    .map_or(self.input.len(), |i| self.cursor + i);
                let trailing_space = end == self.input.len();
                let mut next = String::new();
                next.push_str(&self.input[..start]);
                next.push('@');
                next.push_str(path);
                next.push_str(&self.input[end..]);
                if trailing_space {
                    next.push(' ');
                }
                self.cursor = start + 1 + path.len() + usize::from(trailing_space);
                self.input = next;
            }
        }
        true
    }

    /// Insert pasted text at the caret, normalising CRLF so the buffer stays clean.
    fn paste(&mut self, text: &str) {
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    fn insert(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Byte offset `n` characters before `from`.
    fn boundary_before(&self, from: usize, n: usize) -> usize {
        let mut i = from;
        for _ in 0..n {
            if i == 0 {
                break;
            }
            i -= 1;
            while i > 0 && !self.input.is_char_boundary(i) {
                i -= 1;
            }
        }
        i
    }

    fn boundary_after(&self, from: usize, n: usize) -> usize {
        let mut i = from;
        for _ in 0..n {
            if i >= self.input.len() {
                break;
            }
            i += 1;
            while i < self.input.len() && !self.input.is_char_boundary(i) {
                i += 1;
            }
        }
        i
    }

    /// Start of the whitespace-delimited word before the caret.
    fn word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 {
            let prev = self.boundary_before(i, 1);
            if self.input[prev..i].trim().is_empty() {
                break;
            }
            i = prev;
        }
        i
    }

    fn line_start(&self) -> usize {
        self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |i| self.cursor + i)
    }

    fn edit_input(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        match code {
            KeyCode::Char('j') if ctrl => self.insert('\n'),
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start();
                self.input.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char('k') if ctrl => {
                let end = self.line_end();
                self.input.replace_range(self.cursor..end, "");
            }
            KeyCode::Char('o') if ctrl => self.toggle_last_body(),
            KeyCode::Char('g') if ctrl => self.request_editor = true,
            KeyCode::Char('w') if ctrl => {
                let start = self.word_start();
                self.input.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_start(),
            KeyCode::Left => self.cursor = self.boundary_before(self.cursor, 1),
            KeyCode::Right => self.cursor = self.boundary_after(self.cursor, 1),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Backspace => {
                let start = self.boundary_before(self.cursor, 1);
                if start < self.cursor {
                    self.input.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
            }
            KeyCode::Delete => {
                let end = self.boundary_after(self.cursor, 1);
                if end > self.cursor {
                    self.input.replace_range(self.cursor..end, "");
                }
            }
            _ => {}
        }
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.history.push(text.clone());
        self.history_idx = None;
        self.input.clear();
        self.cursor = 0;
        self.completion_idx = 0;
        self.scroll_up = 0;
        if let Some(cmd) = text.strip_prefix('/') {
            self.command(cmd.trim());
        } else {
            self.user_line(&text);
            self.spawn_turn(text);
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        // command() result helper: surfaces backend errors as UI lines
        match self.backend.call(method, params) {
            Ok(v) => v,
            Err(e) => {
                self.push(format!("error: {e}"), LineKind::Error);
                Value::Null
            }
        }
    }

    fn command(&mut self, cmd: &str) {
        let (head, rest) = match cmd.split_once(' ') {
            Some((h, r)) => (h, r.trim()),
            None => (cmd, ""),
        };
        match head {
            "exit" | "quit" => self.should_quit = true,
            "clear" => {
                self.entries.clear();
                self.current.clear();
            }
            "help" => self.footer.help_open = true,
            "theme" => {
                // A palette swap, not a redraw protocol: existing entries keep
                // their spans, so re-render the transcript through the new one.
                let t = match rest.trim() {
                    "light" => crate::theme::Theme::light(),
                    "dark" => crate::theme::Theme::dark(),
                    "" => {
                        self.push(
                            format!(
                                "▪ theme {} · /theme light|dark",
                                crate::theme::current().name
                            ),
                            LineKind::Note,
                        );
                        return;
                    }
                    other => {
                        self.push(format!("✗ unknown theme {other}"), LineKind::Error);
                        return;
                    }
                };
                crate::theme::set(t);
                self.push(format!("▪ theme → {}", t.name), LineKind::Note);
            }
            "stats" => {
                self.refresh_footer();
                let f = &self.footer;
                let cost = f
                    .cost
                    .map(|c| format!("${c:.3}"))
                    .unwrap_or_else(|| "n/a".into());
                self.push(
                    format!(
                        "session: in {} (cache R {}/W {}) · out {} · cost {cost} · {} entries · ctx {}{}/{}",
                        f.totals.total_in(),
                        f.totals.cache_read,
                        f.totals.cache_write,
                        f.totals.output,
                        f.entries,
                        if f.ctx_measured { "" } else { "~" },
                        tok(f.est),
                        tok(f.window),
                    ),
                    LineKind::Meta,
                );
            }
            "new" => {
                let r = self.call("session/new", json!({}));
                if let Some(id) = r["sessionId"].as_str() {
                    self.push(format!("▪ new session {id}"), LineKind::Meta);
                }
            }
            "name" if !rest.is_empty() => {
                let r = self.call("session/name", json!({"name": rest}));
                if !r.is_null() {
                    self.push(format!("▪ session named “{rest}”"), LineKind::Meta);
                }
            }
            "model" if rest.is_empty() => {
                let current = self.footer.model.clone();
                let mut out = Vec::new();
                for m in &self.catalog {
                    let marker = if current == m.id { "▶" } else { " " };
                    let price = match (m.input_per_mtok, m.output_per_mtok) {
                        (Some(i), Some(o)) => format!("  ${i}/${o} per MTok"),
                        _ => String::new(),
                    };
                    out.push(format!(
                        "{marker} {} ({}k window{price})",
                        m.id,
                        m.context_window / 1000
                    ));
                }
                for l in out {
                    self.push(l, LineKind::Meta);
                }
                self.push(
                    "(harness update --models refreshes the catalog)",
                    LineKind::Note,
                );
            }
            "model" => {
                let r = self.call("model/set", json!({"model": rest}));
                if !r.is_null() {
                    self.push(format!("▪ model → {rest}"), LineKind::Meta);
                }
            }
            "resume" if rest.is_empty() => {
                let r = self.call("session/listings", json!({}));
                if let Some(list) = r.as_array() {
                    self.last_sessions = list.to_vec();
                    if self.last_sessions.is_empty() {
                        self.push("no saved sessions for this workspace", LineKind::Meta);
                    }
                    let mut out = Vec::new();
                    for (i, s) in self.last_sessions.iter().enumerate() {
                        out.push(format!(
                            "{:>2}. {}  {}  {} entries  [{}]",
                            i + 1,
                            ago(s["createdMs"].as_i64().unwrap_or(0)),
                            s["name"].as_str().unwrap_or("-"),
                            s["entries"].as_u64().unwrap_or(0),
                            s["id"].as_str().unwrap_or("?"),
                        ));
                    }
                    for l in out {
                        self.push(l, LineKind::Meta);
                    }
                    self.push("/resume <n> opens a session", LineKind::Note);
                }
            }
            "resume" => match rest.parse::<usize>() {
                Ok(n) if n >= 1 && n <= self.last_sessions.len() => {
                    let path = self.last_sessions[n - 1]["path"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let r = self.call("session/load", json!({"path": path}));
                    if let Some(id) = r["sessionId"].as_str() {
                        self.push(format!("▪ resumed {id}"), LineKind::Meta);
                    }
                }
                _ => self.push("/resume without arguments lists sessions", LineKind::Error),
            },
            "compact" => {
                self.push("▪ compacting…", LineKind::Note);
                self.running = true;
                self.backend.submit_compact();
            }
            "branch" if rest.is_empty() => {
                let r = self.call("session/branch", json!({}));
                if let Some(list) = r.as_array() {
                    self.last_branch_points = list
                        .iter()
                        .filter_map(|b| b["id"].as_str().map(String::from))
                        .collect();
                    let mut out = Vec::new();
                    if list.is_empty() {
                        out.push("(no branch points yet)".into());
                    }
                    for (i, b) in list.iter().enumerate() {
                        out.push(format!(
                            "{:>3}. [{}] {}",
                            i + 1,
                            b["id"].as_str().unwrap_or("?"),
                            b["label"].as_str().unwrap_or("")
                        ));
                    }
                    for l in out {
                        self.push(l, LineKind::Meta);
                    }
                    self.push(
                        "/branch <n|id> rewinds there; later work branches from that point",
                        LineKind::Note,
                    );
                }
            }
            "branch" => {
                let target = match rest.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= self.last_branch_points.len() => {
                        self.last_branch_points[n - 1].clone()
                    }
                    _ => rest.to_string(),
                };
                let r = self.call("session/branch", json!({"target": target}));
                if r["moved"].as_bool().unwrap_or(false) {
                    self.push(
                        "▪ leaf moved — new work continues from there".to_string(),
                        LineKind::Meta,
                    );
                } else if !r.is_null() {
                    self.push(
                        "no such entry — /branch lists ids".to_string(),
                        LineKind::Error,
                    );
                }
            }
            "fork" => {
                let r = self.call("session/fork", json!({}));
                if let Some(id) = r["sessionId"].as_str() {
                    self.push(
                        format!(
                            "▪ forked → session {} ({})",
                            id,
                            r["path"].as_str().unwrap_or("")
                        ),
                        LineKind::Meta,
                    );
                }
            }
            "worktree" => {
                let r = self.call("session/worktree", json!({"name": rest}));
                if !r.is_null() {
                    self.push(
                        format!(
                            "▪ worktree {} on harness/{} — fresh session bound to it (/worktree-exit [rm])",
                            r["root"].as_str().unwrap_or("?"),
                            r["name"].as_str().unwrap_or("?"),
                        ),
                        LineKind::Meta,
                    );
                }
            }
            "worktree-exit" => {
                let remove = rest == "remove" || rest == "rm";
                let r = self.call("session/worktree-exit", json!({"remove": remove}));
                if !r.is_null() {
                    self.push(
                        format!("▪ back at {}", r["root"].as_str().unwrap_or("?")),
                        LineKind::Meta,
                    );
                }
            }
            "goal" => {
                let (action, text, budget): (&str, String, Option<u64>) = match rest {
                    "" | "status" => ("status", String::new(), None),
                    "pause" | "resume" | "clear" => (rest, String::new(), None),
                    other => {
                        let (text, budget) = crate::parse_goal(other);
                        ("set", text, budget)
                    }
                };
                let r = self.call(
                    "goal/set",
                    json!({"action": action, "text": text, "budget": budget}),
                );
                if r.is_null() {
                    // error already surfaced
                } else if let Some(g) = r["goal"].as_object() {
                    let state = if g["completed"].as_bool().unwrap_or(false) {
                        " (completed)"
                    } else if g["paused"].as_bool().unwrap_or(false) {
                        " (paused)"
                    } else {
                        ""
                    };
                    let budget = g["budget"]
                        .as_u64()
                        .map(|b| format!(" [budget {b} tokens]"))
                        .unwrap_or_else(|| " [unlimited]".into());
                    self.push(
                        format!(
                            "▪ goal{state}: {}{budget}",
                            g["text"].as_str().unwrap_or("")
                        ),
                        LineKind::Meta,
                    );
                } else {
                    let auto = r["autonomous"]
                        .as_object()
                        .map(|a| {
                            format!(
                                " · autonomous on{} {}",
                                a["maxTurns"]
                                    .as_u64()
                                    .map(|m| format!(" max-turns {m}"))
                                    .unwrap_or_default(),
                                a["gate"]
                                    .as_str()
                                    .map(|g| format!("gate: {g}"))
                                    .unwrap_or_else(|| "(no gate)".into())
                            )
                        })
                        .unwrap_or_default();
                    self.push(format!("▪ no goal set{auto}"), LineKind::Meta);
                }
            }
            "autonomous" => {
                let (on, max_turns, max_tokens, gate) = crate::parse_auto_flags(rest);
                let r = self.call(
                    "autonomous/set",
                    json!({"on": on, "maxTurns": max_turns, "maxTokens": max_tokens, "gate": gate}),
                );
                if !r.is_null() {
                    self.push(
                        format!("▪ autonomous {}", if on { "on" } else { "off" }),
                        LineKind::Meta,
                    );
                }
            }
            "heartbeat" => {
                let (verb, tail) = match rest.split_once(' ') {
                    Some((v, t)) => (v, t.trim()),
                    None => (rest, ""),
                };
                match verb {
                    "list" => {
                        let r = self.call("heartbeat/list", json!({}));
                        match r.as_array() {
                            Some(list) if list.is_empty() => {
                                self.push("no heartbeats", LineKind::Meta)
                            }
                            Some(list) => {
                                for h in list {
                                    self.push(
                                        format!(
                                            "{}{}  {}  {}",
                                            h["id"].as_str().unwrap_or("?"),
                                            if h["paused"].as_bool().unwrap_or(false) {
                                                " (paused)"
                                            } else {
                                                ""
                                            },
                                            h["cron"]
                                                .as_str()
                                                .map(|c| format!("cron {c}"))
                                                .or_else(|| {
                                                    h["everyMs"]
                                                        .as_u64()
                                                        .map(fmt_every)
                                                        .or_else(|| {
                                                            h["inMs"].as_u64().map(fmt_every)
                                                        })
                                                })
                                                .unwrap_or_else(|| "-".into()),
                                            h["instruction"].as_str().unwrap_or("")
                                        ),
                                        LineKind::Meta,
                                    );
                                }
                            }
                            None => {}
                        }
                    }
                    "pause" | "resume" | "clear" => {
                        let r = self.call("heartbeat/set", json!({"action": verb, "id": tail}));
                        if !r.is_null() {
                            self.push(
                                format!("▪ heartbeat {} {}", r["id"].as_str().unwrap_or("?"), verb),
                                LineKind::Meta,
                            );
                        }
                    }
                    "cron" => {
                        // Five space-separated fields then the instruction: no
                        // quoting needed, the expression's shape is fixed.
                        let toks: Vec<&str> = tail.splitn(6, ' ').collect();
                        match toks.as_slice() {
                            [mi, hh, dom, mon, dow, instruction] => {
                                let expr = [*mi, *hh, *dom, *mon, *dow].join(" ");
                                let r = self.call(
                                    "heartbeat/set",
                                    json!({
                                        "action": "add",
                                        "cron": expr,
                                        "instruction": instruction,
                                    }),
                                );
                                if !r.is_null() {
                                    self.push(
                                        format!(
                                            "▪ heartbeat {} cron {expr}",
                                            r["id"].as_str().unwrap_or("?"),
                                        ),
                                        LineKind::Meta,
                                    );
                                }
                            }
                            _ => self.push(
                                "/heartbeat cron <min hour day-of-month month day-of-week> <text>",
                                LineKind::Error,
                            ),
                        }
                    }
                    "every" | "in" => match tail.split_once(' ') {
                        Some((dur, instruction)) => match crate::parse_duration_ms(dur) {
                            Some(ms) => {
                                let params = if verb == "every" {
                                    json!({"action": "add", "everyMs": ms, "instruction": instruction})
                                } else {
                                    json!({"action": "add", "inMs": ms, "instruction": instruction})
                                };
                                let r = self.call("heartbeat/set", params);
                                if !r.is_null() {
                                    self.push(
                                        format!(
                                            "▪ heartbeat {} {} {dur}",
                                            r["id"].as_str().unwrap_or("?"),
                                            if verb == "every" { "every" } else { "in" },
                                        ),
                                        LineKind::Meta,
                                    );
                                }
                            }
                            None => {
                                self.push("duration must look like 30s, 10m or 1h", LineKind::Error)
                            }
                        },
                        None => {
                            self.push("/heartbeat every|in <dur> <instruction>", LineKind::Error)
                        }
                    },
                    _ => self.push(
                        "/heartbeat every|in <dur> <text> · cron <expr> <text> · list · pause|resume|clear [id]",
                        LineKind::Error,
                    ),
                }
            }
            other => self.push(format!("unknown command /{other} — /help"), LineKind::Error),
        }
        self.refresh_footer();
    }

    fn render(&mut self, f: &mut Frame) {
        let [scroll_area, input_area, footer_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        // Only the visible window is materialised; `scroll_up == 0` follows the tail.
        let tail = self.current_lines();
        let entries_height: usize = self.entries.iter().map(Entry::height).sum();
        let total = entries_height + tail.len();
        let height = scroll_area.height.max(1) as usize;
        let up = (self.scroll_up as usize).min(total.saturating_sub(1));
        let end = total - up;
        let start = end.saturating_sub(height);
        let mut visible: Vec<Line<'static>> = Vec::with_capacity(end - start);
        let mut pos = 0usize;
        for entry in &self.entries {
            let entry_height = entry.height();
            if pos + entry_height > start && pos < end {
                let mut lines = Vec::with_capacity(entry_height);
                entry.append(&mut lines);
                for (i, line) in lines.into_iter().enumerate() {
                    let at = pos + i;
                    if at >= start && at < end {
                        visible.push(line);
                    }
                }
            }
            pos += entry_height;
            if pos >= end {
                break;
            }
        }
        for (i, line) in tail.into_iter().enumerate() {
            let at = pos + i;
            if at >= start && at < end {
                visible.push(line);
            }
        }
        f.render_widget(
            Paragraph::new(Text::from(visible)).wrap(Wrap { trim: false }),
            scroll_area,
        );

        let title = if let Some(req) = &self.pending_prompt {
            format!(" {} [y/a/n]", req.question)
        } else if self.running {
            " turn running — enter queues · esc cancels · /exit exits ".into()
        } else {
            " input ".into()
        };
        let input = Paragraph::new(self.input.as_str())
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(title));
        f.render_widget(input, input_area);
        if self.pending_prompt.is_none() {
            let inner = input_area.width.saturating_sub(2).max(1) as usize;
            let (row, col) = cursor_pos(&self.input, self.cursor, inner);
            let y = input_area.y + 1 + row.min(input_area.height.saturating_sub(2));
            let x = input_area.x + 1 + col.min(input_area.width.saturating_sub(3));
            f.set_cursor_position(Position { x, y });
        }

        // Completion menu, drawn over the bottom of the transcript.
        let menu = self.menu();
        if !menu.is_empty() {
            let height = (menu.len() as u16 + 2).min(9);
            let area = Rect {
                x: input_area.x,
                y: input_area.y.saturating_sub(height),
                width: input_area.width,
                height,
            };
            let rows: Vec<Line<'static>> = menu
                .iter()
                .enumerate()
                .take(height.saturating_sub(2) as usize)
                .map(|(i, item)| {
                    let selected = i == self.completion_idx;
                    let style = if selected {
                        Style::new().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::new()
                    };
                    match item {
                        MenuItem::Command(name, description) => Line::from(vec![
                            Span::styled(
                                format!(" /{name:<14}"),
                                style.add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!(" {description}"), style),
                        ]),
                        MenuItem::Model(id, detail) => Line::from(vec![
                            Span::styled(format!(" {id:<24}"), style.add_modifier(Modifier::BOLD)),
                            Span::styled(detail.to_string(), style.add_modifier(Modifier::DIM)),
                        ]),
                        MenuItem::Session(n, label) => Line::from(vec![
                            Span::styled(format!(" #{n:<3}"), style.add_modifier(Modifier::BOLD)),
                            Span::styled(label.to_string(), style.add_modifier(Modifier::DIM)),
                        ]),
                        MenuItem::File(path) => {
                            let (dir, base) = match path.rfind('/') {
                                Some(i) => (&path[..i + 1], &path[i + 1..]),
                                None => ("", path.as_str()),
                            };
                            Line::from(vec![
                                Span::styled(" @", style.add_modifier(Modifier::DIM)),
                                Span::styled(dir.to_string(), style.add_modifier(Modifier::DIM)),
                                Span::styled(base.to_string(), style.add_modifier(Modifier::BOLD)),
                            ])
                        }
                    }
                })
                .collect();
            let title = match menu.first() {
                Some(MenuItem::File(_)) => " files · tab to insert ",
                Some(MenuItem::Model(..)) => " models · tab to choose ",
                Some(MenuItem::Session(..)) => " sessions · tab to choose ",
                _ => " commands · tab to complete ",
            };
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(rows).block(Block::bordered().title(title)),
                area,
            );
        }

        let f1 = &self.footer;
        let cost = f1.cost.map(|c| format!(" ${c:.3}")).unwrap_or_default();
        let queued = if self.queue.is_empty() {
            String::new()
        } else {
            format!(" · queued {}", self.queue.len())
        };
        let scrolled = if self.scroll_up > 0 {
            format!(" · scrolled {} lines (end to follow)", self.scroll_up)
        } else {
            String::new()
        };
        let spin = if self.running {
            spinner(self.ascii)
        } else {
            String::new()
        };
        // Context pressure at a glance: green → yellow → red as compaction nears.
        let frac = if f1.window == 0 {
            0.0
        } else {
            (f1.est as f64 / f1.window as f64).clamp(0.0, 1.0)
        };
        let filled = (frac * 10.0).round() as usize;
        let (on, off) = if self.ascii {
            ("#", ".")
        } else {
            ("▇", "░")
        };
        let bar_style = if frac > 0.85 {
            Style::new().fg(Color::Red)
        } else if frac > 0.6 {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::Green)
        };
        let repo = if f1.root.is_empty() {
            String::new()
        } else {
            let name = std::path::Path::new(&f1.root)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| f1.root.clone());
            format!("{name} ({}) · ", f1.branch)
        };
        // Bound here so the format string above can capture it implicitly: an
        // explicit named argument may not precede positional ones.
        let tilde = if f1.ctx_measured { "" } else { "~" };
        let line = Line::from(vec![
            Span::raw(format!(" {spin}{repo}{} · ", f1.model,)),
            Span::styled(on.repeat(filled), bar_style),
            Span::styled(off.repeat(10 - filled), Style::new().fg(Color::DarkGray)),
            Span::raw(format!(
                " ctx {tilde}{}/{} · R {} W {} · out {}{cost} · {} entries{queued}{scrolled} ",
                tok(f1.est),
                tok(f1.window),
                f1.totals.cache_read,
                f1.totals.cache_write,
                f1.totals.output,
                f1.entries,
            )),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::new().add_modifier(Modifier::REVERSED)),
            footer_area,
        );

        // `/help` overlay: covers the transcript until any key is pressed.
        if self.footer.help_open {
            let area = f.area();
            let mut lines: Vec<Line<'static>> = COMMANDS
                .iter()
                .map(|(name, description)| {
                    Line::from(vec![
                        Span::styled(
                            format!(" /{name:<14}"),
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(" {description}")),
                    ])
                })
                .collect();
            for hint in [
                "",
                " enter send · ctrl+j newline · tab complete · esc cancel",
                " ctrl+a/e line start/end · ctrl+u/k delete to start/end · ctrl+w delete word",
                " ctrl+o expand or collapse the newest tool/reasoning block",
                " ctrl+g edit this prompt in $EDITOR",
                " ←/→ move · alt+← word · ↑/↓ menu then history · pgup/pgdn scroll · wheel scrolls",
                " any key closes this",
            ] {
                lines.push(Line::from(Span::styled(
                    hint.to_string(),
                    Style::new().dim(),
                )));
            }
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(" help "))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

/// Commands matching a half-typed slash command. Offers nothing once arguments
/// have been typed, and tolerates leading/trailing whitespace.
fn completions_for(input: &str) -> Vec<&'static (&'static str, &'static str)> {
    let head = input.trim();
    let word = head.split_whitespace().next().unwrap_or_default();
    if !word.starts_with('/') || head.len() > word.len() {
        return Vec::new();
    }
    let prefix = &word[1..];
    COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .collect()
}

/// Where the caret sits in a wrapped input buffer: (row, column). Columns are
/// display cells, so a CJK/emoji character (two cells) does not shift the caret.
fn cursor_pos(text: &str, cursor: usize, width: usize) -> (u16, u16) {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1) as u16;
    let (mut row, mut col) = (0u16, 0u16);
    for (idx, ch) in text.char_indices() {
        if idx >= cursor {
            break;
        }
        if ch == '\n' {
            row += 1;
            col = 0;
            continue;
        }
        let cells = UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
        if cells > 0 && col + cells > width {
            row += 1;
            col = 0;
        }
        col += cells;
    }
    (row, col)
}

/// Braille spinner while a turn runs; ASCII fallback for mismatched fonts.
fn spinner(ascii: bool) -> String {
    const UNICODE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
    let ms = harness_core::session::now_ms() / 100;
    if ascii {
        format!("{} ", ASCII[(ms as usize) % ASCII.len()])
    } else {
        format!("{} ", UNICODE[(ms as usize) % UNICODE.len()])
    }
}

/// `ctx 0/200k` reads like a bug; show counts under 1k exactly.
fn tok(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{}k", n / 1000)
    }
}

fn style_for(kind: LineKind) -> Style {
    let t = crate::theme::current();
    match kind {
        LineKind::User => Style::new().fg(t.user).bold(),
        LineKind::Assistant => Style::new(),
        LineKind::Note => Style::new().dim(),
        LineKind::Error => Style::new().red(),
        LineKind::Meta => Style::new().fg(t.meta),
        LineKind::Tool => Style::new().fg(t.tool),
        LineKind::Thinking => Style::new().fg(t.thinking).add_modifier(Modifier::ITALIC),
    }
}

/// Tool output: plain text, except `+`/`-`/`@@` lines from a diff, which get
/// the usual green/red treatment so a write or edit reads at a glance.
fn render_plain(text: &str, base: Style) -> Vec<Line<'static>> {
    // Tool output is indented under its call; there is no glyph to swap here,
    // so this renderer ignores the ascii preference entirely.
    let prefix = "  ";
    text.lines()
        .map(|l| {
            let t = crate::theme::current();
            let styled = if l.starts_with("@@") {
                Style::new().fg(t.diff_hunk)
            } else if l.starts_with('+') {
                Style::new().fg(t.diff_add)
            } else if l.starts_with('-') {
                Style::new().fg(t.diff_del)
            } else {
                base
            };
            Line::from(Span::styled(format!("{prefix}{l}"), styled))
        })
        .collect()
}

fn ago(ms: i64) -> String {
    let now = harness_core::session::now_ms();
    let mins = ((now - ms).max(0) / 60_000) as u64;
    match mins {
        0..=1 => "just now".into(),
        2..=59 => format!("{mins}m ago"),
        60..=1439 => format!("{}h ago", mins / 60),
        _ => format!("{}d ago", mins / 1440),
    }
}

pub fn run(
    terminal: &mut DefaultTerminal,
    backend: Box<dyn Backend>,
    catalog: Vec<ModelInfo>,
    prompt_rx: Receiver<PromptRequest>,
    event_rx: Receiver<UiEvent>,
    ascii: bool,
) -> Result<()> {
    let mut app = App::new(backend, catalog, ascii);
    app.refresh_footer();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    app.push(
        format!(
            "▪ session · {} · /help for commands · ctrl+c exits",
            app.footer.model
        ),
        LineKind::Meta,
    );

    loop {
        terminal.draw(|f| app.render(f))?;
        if app.request_editor {
            app.request_editor = false;
            let initial = std::mem::take(&mut app.input);
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let edited = edit_text(&initial);
            let _ = execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture);
            let _ = enable_raw_mode();
            terminal.clear()?;
            match edited {
                Some(text) => {
                    app.input = text.trim_end().to_string();
                    app.cursor = app.input.len();
                }
                None => {
                    // Put the prompt back rather than losing it.
                    app.input = initial;
                    app.cursor = app.input.len();
                    app.push("editor exited without saving", LineKind::Note);
                }
            }
        }
        if app.wants_files() && !app.files_loaded {
            app.load_files();
        }
        if app.wants_sessions() {
            app.load_sessions();
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.footer.help_open {
                        app.footer.help_open = false;
                    } else {
                        app.on_key(key.code, key.modifiers);
                    }
                }
                // A paste is never a submit, however many newlines it carries.
                Event::Paste(text) => app.paste(&text),
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => {
                        app.scroll_up = app.scroll_up.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll_up = app.scroll_up.saturating_sub(3);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        loop {
            match event_rx.try_recv() {
                Ok(UiEvent::Delta(t)) => {
                    app.scroll_up = 0;
                    app.current.push_str(&t);
                }
                Ok(UiEvent::Note(n)) => app.push(n, LineKind::Note),
                Ok(UiEvent::History(lines)) => app.push_history(&lines),
                Ok(UiEvent::Tool(ev)) => app.push_tool(&ev),
                Ok(UiEvent::Thinking(t)) => app.thinking.push_str(&t),
                Ok(UiEvent::Done(res)) => app.on_done(res),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        while let Ok(req) = prompt_rx.try_recv() {
            app.pending_prompt = Some(req);
        }
        if app.should_quit {
            break;
        }
    }
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    Ok(())
}

/// `10m` → 600000 ms. Plain ASCII digits with an s/m/h suffix.
pub fn fmt_every(ms: u64) -> String {
    if ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(input: &str) -> Vec<&'static str> {
        completions_for(input)
            .into_iter()
            .map(|(n, _)| *n)
            .collect()
    }

    #[test]
    fn completes_a_half_typed_command_ignoring_stray_whitespace() {
        assert_eq!(names("/go"), vec!["goal"]);
        assert_eq!(names("  /go"), vec!["goal"]);
        assert_eq!(names("/worktree"), vec!["worktree", "worktree-exit"]);
        assert_eq!(
            names("/"),
            COMMANDS.iter().map(|(n, _)| *n).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stops_offering_once_arguments_are_present() {
        assert!(names("/goal ship it").is_empty());
        assert!(names("explain this").is_empty());
        assert!(names("").is_empty());
        assert!(names("/zzz").is_empty());
    }

    #[test]
    fn caret_tracks_wrapping_and_newlines() {
        assert_eq!(cursor_pos("", 0, 10), (0, 0));
        assert_eq!(cursor_pos("abc", 3, 10), (0, 3));
        // wrap: 12 chars at width 10 puts the caret on the next row
        assert_eq!(cursor_pos(&"x".repeat(12), 12, 10), (1, 2));
        assert_eq!(cursor_pos("a\nb", 3, 10), (1, 1));
        // Wide characters occupy two cells each.
        assert_eq!(cursor_pos("日本語", 9, 10), (0, 6));
        assert_eq!(cursor_pos("日本", 6, 4), (0, 4));
        assert_eq!(cursor_pos("日本", 6, 3), (1, 2));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    struct StubBackend;

    impl Backend for StubBackend {
        fn submit_prompt(&mut self, _prompt: String) {}
        fn submit_compact(&mut self) {}
        fn cancel(&mut self) {}
        fn call(&mut self, _method: &str, _params: Value) -> anyhow::Result<Value> {
            Ok(Value::Null)
        }
    }

    fn app() -> App {
        App::new(Box::new(StubBackend), Vec::new(), false)
    }

    /// Render into a virtual terminal and return the visible screen.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn completion_menu_draws_above_the_input() {
        let mut a = app();
        a.input = "/go".into();
        a.cursor = 3;
        let s = screen(&mut a, 80, 24);
        assert!(s.contains("commands · tab to complete"), "{s}");
        assert!(s.contains("/goal"), "{s}");
        assert!(s.contains("persistent objective"), "{s}");
    }

    #[test]
    fn assistant_markdown_and_tool_blocks_render() {
        let mut a = app();
        a.push_rich(
            "# Title\n\nSome **bold** text and `code`.",
            LineKind::Assistant,
        );
        a.push_tool(&ToolEvent {
            name: "bash".into(),
            args_preview: "ls -la".into(),
            content_preview: "-gone\n+added".into(),
            is_error: false,
            ms: 12,
            content_len: 4000,
        });
        let s = screen(&mut a, 80, 24);
        assert!(s.contains("Title"), "{s}");
        assert!(!s.contains('#'), "heading marker stripped: {s}");
        assert!(!s.contains("**"), "bold markers stripped: {s}");
        assert!(s.contains("✓ bash: ls -la  (12 ms)"), "{s}");
        assert!(s.contains("+added"), "{s}");
        // short bodies are shown whole, with no collapse note
        assert!(!s.contains("more line(s)"), "{s}");
    }

    #[test]
    fn a_long_tool_body_collapses_and_ctrl_o_expands_it() {
        let mut a = app();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        a.push_tool(&ToolEvent {
            name: "bash".into(),
            args_preview: "seq 1 10".into(),
            content_preview: body,
            is_error: false,
            ms: 3,
            content_len: 70,
        });
        let collapsed = screen(&mut a, 80, 30);
        assert!(collapsed.contains("line 6"), "{collapsed}");
        assert!(
            !collapsed.contains("line 7"),
            "hidden while collapsed: {collapsed}"
        );
        assert!(collapsed.contains("more line(s)"), "{collapsed}");

        a.toggle_last_body();
        let expanded = screen(&mut a, 80, 40);
        assert!(expanded.contains("line 10"), "{expanded}");
        assert!(!expanded.contains("more line(s)"), "{expanded}");
    }

    #[test]
    fn at_file_menu_filters_workspace_files_and_tab_inserts() {
        let mut a = app();
        a.files = vec![
            "src/lib.rs".into(),
            "src/main.rs".into(),
            "README.md".into(),
        ];
        a.files_loaded = true;
        a.input = "explain @src/ma".into();
        a.cursor = a.input.len();

        let s = screen(&mut a, 80, 24);
        assert!(s.contains("files · tab to insert"), "{s}");
        assert!(s.contains("src/main.rs"), "{s}");
        assert!(!s.contains("README.md"), "filtered out: {s}");

        assert!(a.complete());
        assert_eq!(a.input, "explain @src/main.rs ");
        assert_eq!(a.cursor, a.input.len());
    }

    #[test]
    fn model_picker_lists_the_catalog_and_tab_inserts_the_id() {
        let mut a = app();
        a.catalog = vec![
            ModelInfo {
                id: "deepseek-v4-pro".into(),
                context_window: 1_000_000,
                ..Default::default()
            },
            ModelInfo {
                id: "deepseek-flash".into(),
                context_window: 1_000_000,
                ..Default::default()
            },
        ];
        a.input = "/model".into();
        a.cursor = 6;

        let s = screen(&mut a, 90, 24);
        assert!(s.contains("models · tab to choose"), "{s}");
        assert!(s.contains("deepseek-v4-pro"), "{s}");
        assert!(s.contains("1000k window"), "{s}");

        assert!(a.complete());
        assert_eq!(a.input, "/model deepseek-v4-pro ");
        assert_eq!(a.cursor, a.input.len());
    }

    #[test]
    fn editor_handoff_round_trips_the_prompt() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fake-editor.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'edited by the editor' > \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = edit_text_with(script.to_str().unwrap(), "seeded prompt");
        assert_eq!(out.as_deref(), Some("edited by the editor"));
        // A failing editor reports failure instead of clobbering the prompt.
        assert!(edit_text_with("false", "keep me").is_none());
    }

    #[test]
    fn non_ascii_tool_output_does_not_panic_the_screen() {
        let mut a = app();
        a.push_tool(&ToolEvent {
            name: "bash".into(),
            args_preview: "ls".into(),
            content_preview: "→ alpha\n→ beta".into(),
            is_error: true,
            ms: 1,
            content_len: 20,
        });
        let s = screen(&mut a, 60, 20);
        assert!(s.contains("✗ bash: ls"), "{s}");
        assert!(s.contains("→ alpha"), "{s}");
    }

    #[test]
    fn footer_shows_context_pressure_and_spinner_when_running() {
        let mut a = app();
        a.footer.window = 1000;
        a.footer.est = 900; // 90% → the red band
        a.footer.ctx_measured = true;
        a.push("hello", LineKind::Note);
        let s = screen(&mut a, 100, 20);
        assert!(s.contains("ctx 900/1k"), "{s}");
        a.running = true;
        let s = screen(&mut a, 100, 20);
        assert!(
            s.contains('⠋')
                || s.contains('⠙')
                || s.contains('⠹')
                || s.contains('⠸')
                || s.contains('⠼')
                || s.contains('⠴')
                || s.contains('⠦')
                || s.contains('⠧')
                || s.contains('⠇')
                || s.contains('⠏'),
            "spinner visible: {s}"
        );
    }

    #[test]
    fn ascii_mode_swaps_the_glyphs() {
        let mut a = App::new(Box::new(StubBackend), Vec::new(), true);
        a.push_rich("- one", LineKind::Assistant);
        a.push_tool(&ToolEvent {
            name: "read".into(),
            args_preview: "f.txt".into(),
            content_preview: "ok".into(),
            is_error: true,
            ms: 1,
            content_len: 2,
        });
        let s = screen(&mut a, 60, 20);
        assert!(s.contains("- one"), "{s}");
        assert!(s.contains("x read: f.txt"), "{s}");
    }
}

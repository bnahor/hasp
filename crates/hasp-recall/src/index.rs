//! Full-text index over session journals (SQLite FTS5), node-local and
//! rebuildable. The journals stay canonical: this file can be deleted at any
//! time and `rebuild` recreates it. It is never synced between machines (each
//! node indexes its own journals); copy journals, not this database.
//!
//! Indexing is incremental: journals are append-only, so each file is read
//! from the byte offset it was last indexed to. A file that shrank or whose
//! first line changed (replaced, re-imported) is re-indexed from scratch. A
//! torn final line (no newline yet) is left for the next sync.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY, offset INTEGER NOT NULL, head TEXT NOT NULL,
    session_id TEXT, workspace TEXT, created_ms INTEGER);
CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(
    text, role UNINDEXED, path UNINDEXED, entry_id UNINDEXED, ts UNINDEXED,
    tokenize = 'porter unicode61');
";

/// Longest stretch of one entry that is indexed (tool output can be huge).
const MAX_DOC_CHARS: usize = 8_000;

pub struct Index {
    db: Connection,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct SyncStats {
    pub files: usize,
    pub reindexed: usize,
    pub entries: usize,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub session_id: Option<String>,
    pub session_path: PathBuf,
    pub workspace: Option<String>,
    pub entry_id: String,
    pub ts: i64,
    pub role: String,
    pub snippet: String,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.execute_batch("PRAGMA journal_mode=WAL;")?;
        db.execute_batch(SCHEMA)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self { db })
    }

    /// Drop everything and index all journals under `sessions_root` again.
    pub fn rebuild(&mut self, sessions_root: &Path) -> Result<SyncStats> {
        self.db.execute_batch("DELETE FROM docs; DELETE FROM files;")?;
        self.sync(sessions_root)
    }

    /// Bring the index up to date with the journals under `sessions_root`.
    pub fn sync(&mut self, sessions_root: &Path) -> Result<SyncStats> {
        let mut stats = SyncStats::default();
        for path in journals(sessions_root) {
            stats.files += 1;
            let (n, re) = self.sync_file(&path)?;
            stats.entries += n;
            stats.reindexed += re as usize;
        }
        Ok(stats)
    }

    fn sync_file(&mut self, path: &Path) -> Result<(usize, bool)> {
        let key = path.to_string_lossy().to_string();
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        let mut head = String::new();
        std::io::BufReader::new(&mut f).read_line(&mut head)?;
        if !head.ends_with('\n') {
            return Ok((0, false)); // header not complete yet
        }
        let head_hash = crate::sha256_hex(head.as_bytes());
        let known: Option<(i64, String)> = self
            .db
            .query_row("SELECT offset, head FROM files WHERE path = ?1", [&key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let tx = self.db.transaction()?;
        let (start, reindexed) = match known {
            Some((off, h)) if h == head_hash && (off as u64) <= len => (off as u64, false),
            Some(_) => {
                tx.execute("DELETE FROM docs WHERE path = ?1", [&key])?;
                (head.len() as u64, true)
            }
            None => (head.len() as u64, false),
        };
        let header: serde_json::Value = serde_json::from_str(&head).unwrap_or_default();
        f.seek(SeekFrom::Start(start))?;
        let mut rest = Vec::new();
        f.read_to_end(&mut rest)?;
        // Only complete lines; a torn tail waits for the next sync.
        let complete = rest.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let mut n = 0;
        for line in String::from_utf8_lossy(&rest[..complete]).lines() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue; // the journal reader reports corruption; the index just skips
            };
            for (role, text) in entry_texts(&entry) {
                let text: String = text.chars().take(MAX_DOC_CHARS).collect();
                if text.trim().is_empty() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO docs(text, role, path, entry_id, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![text, role, key, entry["id"].as_str().unwrap_or(""), entry["ts"].as_i64().unwrap_or(0)],
                )?;
                n += 1;
            }
        }
        tx.execute(
            "INSERT INTO files(path, offset, head, session_id, workspace, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET offset = excluded.offset, head = excluded.head,
               session_id = excluded.session_id, workspace = excluded.workspace, created_ms = excluded.created_ms",
            params![
                key,
                (start + complete as u64) as i64,
                head_hash,
                header["id"].as_str(),
                header["workspace"]["root"].as_str(),
                header["created_ms"].as_i64()
            ],
        )?;
        tx.commit()?;
        Ok((n, reindexed))
    }

    /// Best matches first. `workspace` limits hits to sessions of that root.
    pub fn search(&self, query: &str, limit: usize, workspace: Option<&str>) -> Result<Vec<Hit>> {
        let Some(q) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.db.prepare(
            "SELECT snippet(docs, 0, '[', ']', '…', 16), d.role, d.path, d.entry_id, d.ts, f.session_id, f.workspace
             FROM docs d JOIN files f ON f.path = d.path
             WHERE docs MATCH ?1 AND (?2 IS NULL OR f.workspace = ?2)
             ORDER BY bm25(docs) LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![q, workspace, limit as i64], |r| {
            Ok(Hit {
                snippet: r.get(0)?,
                role: r.get(1)?,
                session_path: PathBuf::from(r.get::<_, String>(2)?),
                entry_id: r.get(3)?,
                ts: r.get(4)?,
                session_id: r.get(5)?,
                workspace: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

impl Index {
    /// (session id, text) of every indexed user message, for repetition detection.
    pub fn user_messages(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.db.prepare(
            "SELECT COALESCE(f.session_id, d.path), d.text FROM docs d JOIN files f ON f.path = d.path WHERE d.role = 'user'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// Every `*.jsonl` under the sessions root (one directory per workspace).
fn journals(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return out;
    };
    for d in dirs.flatten() {
        if let Ok(files) = std::fs::read_dir(d.path()) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_some_and(|e| e == "jsonl") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// The searchable text of one journal entry, by role.
fn entry_texts(e: &serde_json::Value) -> Vec<(String, String)> {
    match e["type"].as_str() {
        Some("message") => {
            let m = &e["message"];
            let role = m["role"].as_str().unwrap_or("message").to_string();
            match &m["content"] {
                serde_json::Value::String(s) => vec![(role, s.clone())],
                serde_json::Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str().map(|t| (role.clone(), t.to_string())))
                    .collect(),
                _ => Vec::new(),
            }
        }
        Some("compaction") => vec![("summary".into(), e["summary"].as_str().unwrap_or("").to_string())],
        _ => Vec::new(),
    }
}

/// User text as an FTS5 query: every word quoted (so `-`, `:`, `"`, `*` are
/// literal, never query syntax), all words required.
pub fn fts_query(q: &str) -> Option<String> {
    let words: Vec<String> = q
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

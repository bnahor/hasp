//! Per-run JSONL trace (research/03-architecture.md §11): every API call and
//! tool execution, flushed per line so the file is valid mid-session. The
//! system prompt points the agent at it for self-debugging.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::Usage;
use crate::session::now_ms;

pub struct Trace {
    pub run_id: String,
    file: Mutex<std::fs::File>,
}

impl Trace {
    pub fn create(dir: &Path, run_id: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = std::fs::File::create(dir.join(format!("{run_id}.jsonl")))?;
        Ok(Self {
            run_id: run_id.to_string(),
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, mut v: Value) {
        if let Some(o) = v.as_object_mut() {
            o.insert("run".into(), Value::String(self.run_id.clone()));
            o.insert("ts".into(), Value::Number(now_ms().into()));
        }
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{v}");
            let _ = f.flush();
        }
    }

    pub fn record_api(&self, provider: &str, model: &str, ms: u64, usage: &Usage) {
        self.record(json!({
            "event": "api",
            "provider": provider,
            "model": model,
            "ms": ms,
            "input": usage.input,
            "output": usage.output,
            "cache_read": usage.cache_read,
            "cache_write": usage.cache_write,
        }));
    }

    pub fn record_tool(&self, name: &str, ms: u64, ok: bool) {
        self.record(json!({ "event": "tool", "name": name, "ms": ms, "ok": ok }));
    }
}

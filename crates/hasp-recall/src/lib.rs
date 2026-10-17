//! Node-local recall for hasp: a rebuildable full-text index over session
//! journals (`index`) and a small curated memory with provenance (`memory`).
//! See docs/recall.md.

pub mod index;
pub mod memory;
pub mod workflows;

pub use index::{Hit, Index, SyncStats};
pub use memory::{Kind, Memory, MemoryStore, NewMemory, Provenance, Source, Status};

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `~/.hasp/recall/index.db`: node-local, never synced (journals are what you sync).
pub fn default_index_path(hasp_home: &std::path::Path) -> std::path::PathBuf {
    hasp_home.join("recall").join("index.db")
}

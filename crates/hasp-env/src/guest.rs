//! The guest agent: the harness's own static Linux build, run inside an
//! environment as `harness --mcp-serve`. Tools then execute *there*, with the
//! same confinement rules, while the host keeps the session and the approvals.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Linux architectures we build guests for, as `uname -m` / Docker name them.
pub fn normalize_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "amd64" => Some("amd64"),
        "aarch64" | "arm64" => Some("arm64"),
        _ => None,
    }
}

/// The ELF machine of a Linux executable, if it is one.
pub fn elf_arch(path: &Path) -> Option<&'static str> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 20 || &bytes[..4] != b"\x7fELF" || bytes[5] != 1 {
        return None; // not ELF, or not little-endian
    }
    match u16::from_le_bytes([bytes[18], bytes[19]]) {
        0x3E => Some("amd64"),
        0xB7 => Some("arm64"),
        _ => None,
    }
}

/// Where `harness env build-guest` puts its builds.
pub fn guest_dir() -> Result<PathBuf> {
    Ok(hasp_workspace::hasp_home()?.join("guest"))
}

pub fn guest_path(arch: &str) -> Result<PathBuf> {
    Ok(guest_dir()?.join(format!("hasp-linux-{arch}")))
}

/// A guest binary for `arch`: `HASP_GUEST_BINARY`, then the built guest in
/// `~/.harness/guest/`, then — on a Linux host of the same architecture — the
/// running binary itself.
pub fn binary_for(arch: &str) -> Result<PathBuf> {
    let Some(arch) = normalize_arch(arch) else {
        bail!("no guest agent for architecture {arch}");
    };
    let mut tried = Vec::new();
    if let Ok(p) = hasp_workspace::env_var("HASP_GUEST_BINARY") {
        let p = PathBuf::from(p);
        match elf_arch(&p) {
            Some(a) if a == arch => return Ok(p),
            found => tried.push(format!(
                "HASP_GUEST_BINARY={} ({})",
                p.display(),
                found
                    .map(|f| format!("linux/{f}"))
                    .unwrap_or("not a Linux executable".into())
            )),
        }
    }
    let built = guest_path(arch)?;
    if elf_arch(&built) == Some(arch) {
        return Ok(built);
    }
    tried.push(built.display().to_string());
    if let Ok(exe) = std::env::current_exe()
        && elf_arch(&exe) == Some(arch)
    {
        return Ok(exe);
    }
    bail!(
        "no guest agent for linux/{arch} (tried {}). Build one with `hasp env build-guest`.",
        tried.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_machine_from_an_elf_header() {
        let tmp = tempfile::tempdir().unwrap();
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[5] = 1;
        elf[18] = 0x3E;
        std::fs::write(tmp.path().join("x86"), &elf).unwrap();
        elf[18] = 0xB7;
        std::fs::write(tmp.path().join("arm"), &elf).unwrap();
        std::fs::write(tmp.path().join("script"), b"#!/bin/sh\n").unwrap();
        assert_eq!(elf_arch(&tmp.path().join("x86")), Some("amd64"));
        assert_eq!(elf_arch(&tmp.path().join("arm")), Some("arm64"));
        assert_eq!(elf_arch(&tmp.path().join("script")), None);
        assert_eq!(normalize_arch("aarch64"), Some("arm64"));
        assert_eq!(normalize_arch("s390x"), None);
    }
}

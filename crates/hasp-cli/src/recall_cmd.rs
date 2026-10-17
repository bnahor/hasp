//! Owner surfaces for recall: `hasp recall`, `hasp memory`, `hasp workflow`.
//! Everything here acts as the owner (the person at this terminal); the agent's
//! tools can only search, list and propose (hasp-tools/src/recall.rs).

use anyhow::{Context, Result, bail};
use hasp_recall::memory::{Kind, NewMemory, Provenance, Source};

fn home() -> Result<std::path::PathBuf> {
    hasp_workspace::hasp_home().map_err(|e| anyhow::anyhow!("{e}"))
}

pub fn recall(args: &[String]) -> Result<()> {
    let home = home()?;
    let mut idx = hasp_recall::Index::open(&hasp_recall::default_index_path(&home))?;
    match args.first().map(String::as_str) {
        Some("rebuild") => println!("{:?}", idx.rebuild(&home.join("sessions"))?),
        Some("sync") => println!("{:?}", idx.sync(&home.join("sessions"))?),
        Some(_) => {
            idx.sync(&home.join("sessions"))?;
            for h in idx.search(&args.join(" "), 20, None)? {
                println!("{}  {}  {}  {}", h.session_id.as_deref().unwrap_or("?"), h.entry_id, h.role, h.snippet);
            }
        }
        None => bail!("usage: hasp recall <words…> | rebuild | sync"),
    }
    Ok(())
}

fn arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

pub fn memory(args: &[String]) -> Result<()> {
    let path = hasp_recall::memory::default_path(&home()?);
    let mut m = hasp_recall::MemoryStore::open(&path)?;
    let id = || args.get(1).map(String::as_str).context("which memory id?");
    match args.first().map(String::as_str) {
        Some("list") | None => {
            for x in m.all() {
                println!("{}  {:?}/{:?}  {}  {}{}", x.id, x.kind, x.status, x.scope, x.text,
                         if x.unverified { "  (reported)" } else { "" });
            }
        }
        Some("add") => {
            let kind = match arg(args, "--kind").unwrap_or("note") {
                "preference" => Kind::Preference,
                "fact" => Kind::Fact,
                "rule" => Kind::Rule,
                _ => Kind::Note,
            };
            let text = arg(args, "--text").context("--text required")?;
            let new = m.add(NewMemory {
                kind,
                key: arg(args, "--key").map(String::from),
                text: text.into(),
                scope: arg(args, "--scope").unwrap_or("global").into(),
                provenance: Provenance { source: Source::Owner, session_id: None, entry_id: None, channel: Some("cli".into()) },
                supersedes: arg(args, "--supersedes").map(String::from),
            })?;
            println!("added {new}");
        }
        Some("confirm") => m.confirm(id()?, "owner:cli", arg(args, "--supersedes"))?,
        Some("reject") => m.reject(id()?)?,
        Some("edit") => m.edit(id()?, arg(args, "--text").context("--text required")?, "cli")?,
        Some("forget") => m.forget(id()?)?,
        Some(other) => bail!("unknown memory subcommand {other}: list | add --text T [--kind K --key K --scope S --supersedes ID] | confirm ID | reject ID | edit ID --text T | forget ID"),
    }
    Ok(())
}

pub fn workflow(args: &[String]) -> Result<()> {
    let home = home()?;
    let wf = hasp_recall::workflows::Workflows::new(&home);
    match args.first().map(String::as_str) {
        Some("list") | None => {
            for d in wf.list() {
                println!("{} v{}  {}  by {}  sha256 {}  {}", d.name, d.version, d.status, d.proposed_by, &d.sha256[..12], d.description);
            }
        }
        Some("candidates") => {
            let mut idx = hasp_recall::Index::open(&hasp_recall::default_index_path(&home))?;
            idx.sync(&home.join("sessions"))?;
            for c in hasp_recall::workflows::candidates(&idx, 3)? {
                println!("{} sessions: {}", c.sessions.len(), c.request);
            }
        }
        Some("show") => {
            let name = args.get(1).context("name")?;
            let v: u32 = args.get(2).context("version")?.trim_start_matches('v').parse()?;
            print!("{}", std::fs::read_to_string(home.join("workflows").join(name).join(format!("v{v}")).join("SKILL.md"))?);
        }
        Some("activate") => {
            let name = args.get(1).context("name")?;
            let v: u32 = args.get(2).context("version")?.trim_start_matches('v').parse()?;
            let sha = arg(args, "--sha256").context("--sha256 of the version you reviewed (from `hasp workflow list`/`show`)")?;
            let full = wf.versions(name).into_iter().find(|d| d.version == v && d.sha256.starts_with(sha)).context("no such version/hash")?;
            wf.activate(name, v, &full.sha256, "owner:cli")?;
            println!("{name} v{v} is now a skill");
        }
        Some("retire") => wf.retire(args.get(1).context("name")?)?,
        Some(other) => bail!("unknown workflow subcommand {other}: list | candidates | show NAME V | activate NAME V --sha256 H | retire NAME"),
    }
    Ok(())
}

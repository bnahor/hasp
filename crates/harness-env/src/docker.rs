//! Docker (or any Docker-CLI-compatible engine) as an environment backend.
//!
//! One container per environment session:
//!   * the workspace is bind-mounted at its own host path, and for a git
//!     worktree so is the main repository's `.git`, which the worktree's
//!     `.git` file points at by absolute path — git works inside unchanged;
//!   * the guest binary is copied to `/harness-guest` (the filesystem root
//!     exists in every image) and is the container's main process
//!     (`--guest-idle`), under `--init` so orphaned children are reaped;
//!   * limits map directly: `--cpus`, `--memory`, `--pids-limit`,
//!     `--network none`;
//!   * the guest agent runs as `docker exec -i … /harness-guest --mcp-serve`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use harness_mcp::McpServerConfig;

use crate::backend::{Backend, Instance};
use crate::spec::{EnvSpec, Network};

const GUEST_PATH: &str = "/harness-guest";

#[derive(Default)]
pub struct Docker;

async fn docker(args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running docker (is it installed?)")
}

async fn docker_ok(args: &[&str], what: &str) -> Result<String> {
    let out = docker(args).await?;
    if !out.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// For a git worktree, the main repository's `.git` directory it points to.
fn worktree_common_dir(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(".git")).ok()?; // a file only in worktrees
    let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    // <repo>/.git/worktrees/<name> → <repo>/.git
    let parent = gitdir.parent()?;
    (parent.file_name()? == "worktrees").then(|| parent.parent().map(Path::to_path_buf))?
}

#[async_trait]
impl Backend for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn start(&self, spec: &EnvSpec, workspace: &Path) -> Result<Box<dyn Instance>> {
        if !docker(&["info", "--format", "{{.ServerVersion}}"])
            .await?
            .status
            .success()
        {
            bail!("Docker is not running");
        }
        let root = workspace.to_string_lossy().into_owned();

        // The image's architecture decides which guest build goes in.
        let mut pull = vec!["pull", "-q"];
        if let Some(p) = &spec.platform {
            pull.extend(["--platform", p]);
        }
        pull.push(&spec.image);
        if !docker(&["image", "inspect", &spec.image])
            .await?
            .status
            .success()
        {
            docker_ok(&pull, &format!("pulling {}", spec.image)).await?;
        }
        let arch = docker_ok(
            &[
                "image",
                "inspect",
                "--format",
                "{{.Architecture}}",
                &spec.image,
            ],
            "inspecting the image",
        )
        .await?;
        let guest = crate::guest::binary_for(&arch)?;

        let name = format!(
            "harness-env-{}-{}",
            spec.name,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let mut args: Vec<String> = vec![
            "create".into(),
            "--init".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("harness.env={}", spec.name),
            "--label".into(),
            format!("harness.workspace={root}"),
            // The harness process that owns it: `harness env prune` removes
            // containers whose owner is gone (a killed session skips Drop).
            "--label".into(),
            format!("harness.pid={}", std::process::id()),
            "-v".into(),
            format!("{root}:{root}"),
            "-w".into(),
            root.clone(),
            "--entrypoint".into(),
            GUEST_PATH.into(),
        ];
        if let Some(common) = worktree_common_dir(workspace) {
            let c = common.to_string_lossy();
            args.extend(["-v".into(), format!("{c}:{c}")]);
        }
        if let Some(p) = &spec.platform {
            args.extend(["--platform".into(), p.clone()]);
        }
        if let Some(rt) = &spec.runtime {
            args.extend(["--runtime".into(), rt.clone()]);
        }
        if let Some(c) = spec.cpus {
            args.extend(["--cpus".into(), c.to_string()]);
        }
        if let Some(m) = &spec.memory {
            args.extend(["--memory".into(), m.to_ascii_lowercase()]);
        }
        if let Some(p) = spec.pids {
            args.extend(["--pids-limit".into(), p.to_string()]);
        }
        if spec.network == Network::None {
            args.extend(["--network".into(), "none".into()]);
        }
        for (k, v) in &spec.env {
            args.extend(["-e".into(), format!("{k}={v}")]);
        }
        args.push(spec.image.clone());
        args.push("--guest-idle".into());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let id = docker_ok(&argv, "creating the container").await?;

        let instance = DockerInstance {
            container: id.clone(),
            name,
            root: root.clone(),
            user: host_user().await,
        };
        // From here on, a failure must not leave a container behind.
        let prepared: Result<()> = async {
            docker_ok(
                &[
                    "cp",
                    &guest.to_string_lossy(),
                    &format!("{id}:{GUEST_PATH}"),
                ],
                "installing the guest agent",
            )
            .await?;
            docker_ok(&["start", &id], "starting the container").await?;
            for cmd in &spec.setup {
                let out = docker(&["exec", "-u", "0", "-w", &root, &id, "sh", "-c", cmd]).await?;
                if !out.status.success() {
                    bail!(
                        "setup `{cmd}` failed (exit {}):\n{}{}",
                        out.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = prepared {
            instance.stop();
            return Err(e);
        }
        Ok(Box::new(instance))
    }
}

/// On Linux, files the agent creates in the bind mount should belong to the
/// host user, not root. (Docker Desktop already maps ownership.)
async fn host_user() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let id = |flag: &'static str| async move {
        tokio::process::Command::new("id")
            .arg(flag)
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Some(format!("{}:{}", id("-u").await?, id("-g").await?))
}

pub struct DockerInstance {
    /// Docker's container id — what every docker command is given.
    container: String,
    /// The readable container name, reported as the instance id.
    name: String,
    root: String,
    user: Option<String>,
}

#[async_trait]
impl Instance for DockerInstance {
    fn id(&self) -> &str {
        &self.name
    }

    fn guest_command(&self) -> McpServerConfig {
        let mut args = vec!["exec".to_string(), "-i".into()];
        if let Some(u) = &self.user {
            args.extend(["-u".into(), u.clone()]);
        }
        args.extend([
            "-w".into(),
            self.root.clone(),
            // The host gates every call before it is sent; inside, the guest
            // only enforces the structural rules.
            "-e".into(),
            "HARNESS_MCP_YOLO=1".into(),
            "-e".into(),
            "HOME=/tmp".into(),
            self.container.clone(),
            GUEST_PATH.into(),
            "--mcp-serve".into(),
        ]);
        McpServerConfig {
            command: "docker".into(),
            args,
            env: Default::default(),
            cwd: None,
        }
    }

    async fn exec(&self, command: &str) -> Result<(i32, String)> {
        let out = docker(&[
            "exec",
            "-w",
            &self.root,
            &self.container,
            "sh",
            "-c",
            command,
        ])
        .await?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.code().unwrap_or(-1), text))
    }

    fn stop(&self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_main_git_dir_of_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_git = tmp.path().join("repo/.git");
        std::fs::create_dir_all(repo_git.join("worktrees/wt1")).unwrap();
        let wt = tmp.path().join("wt1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", repo_git.join("worktrees/wt1").display()),
        )
        .unwrap();
        assert_eq!(worktree_common_dir(&wt), Some(repo_git));
        assert_eq!(worktree_common_dir(tmp.path()), None, "not a worktree");
    }
}

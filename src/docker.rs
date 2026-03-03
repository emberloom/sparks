use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, RemoveContainerOptions,
    StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{HostConfig, Mount, MountTypeEnum};
use bollard::Docker;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Mutex, OnceLock};

use crate::config::{Config, DockerConfig, GhostConfig};
use crate::error::{AthenaError, Result};
use crate::reason_codes;

pub const CONTAINER_MODE: &str = "docker";
pub const HOST_TRUSTED_MODE: &str = "host_trusted";
pub const CAP_DROP_ALL: &str = "ALL";
pub const ROOTFS_READONLY: bool = true;
pub const NETWORK_MODE_NONE: &str = "none";
pub const DEFAULT_PIDS_LIMIT: i64 = 256;
const CONTAINER_PATH: &str =
    "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CONTAINER_TMPDIR: &str = "/tmp";
const CONTAINER_CARGO_HOME: &str = "/tmp/cargo-home";
const CONTAINER_RUSTUP_HOME: &str = "/tmp/rustup-home";
const CRATES_INDEX_HASH_PRIMARY: &str = "index.crates.io-1949cf8c6b5b557f";
const CRATES_INDEX_HASH_ALT: &str = "index.crates.io-6f17d22bba15001f";
const LINUX_TARGET_X86_64: &str = "x86_64-unknown-linux-gnu";
const LINUX_TARGET_AARCH64: &str = "aarch64-unknown-linux-gnu";
const HOST_WORKSPACE_ALIAS: &str = "/tmp/athena-workspace";
static WARMED_WORKSPACES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn exec_env() -> Vec<String> {
    vec![
        format!("PATH={}", CONTAINER_PATH),
        format!("TMPDIR={}", CONTAINER_TMPDIR),
        format!("CARGO_HOME={}", CONTAINER_CARGO_HOME),
        format!("RUSTUP_HOME={}", CONTAINER_RUSTUP_HOME),
        "CARGO_NET_OFFLINE=true".to_string(),
        "RUSTUP_SKIP_UPDATE_CHECK=1".to_string(),
        "RUSTUP_AUTO_INSTALL=0".to_string(),
        "HOME=/tmp".to_string(),
    ]
}

fn exec_shell_prelude() -> String {
    let tag = reason_codes::reason_tag(reason_codes::REASON_GHOST_TOOL_UNAVAILABLE);
    format!(
        "export TMPDIR=\"{tmp}\" CARGO_HOME=\"{cargo}\" RUSTUP_HOME=\"{rustup}\" HOME=/tmp CARGO_NET_OFFLINE=true RUSTUP_SKIP_UPDATE_CHECK=1 RUSTUP_AUTO_INSTALL=0; \
mkdir -p \"$TMPDIR\" \"$CARGO_HOME\" \"$RUSTUP_HOME\" \"$RUSTUP_HOME/tmp\"; \
mkdir -p \"$CARGO_HOME/registry\" \"$CARGO_HOME/registry/index\" \"$CARGO_HOME/registry/cache\" \"$CARGO_HOME/registry/src\"; \
INDEX_SRC=\"\"; \
if [ -d /usr/local/cargo/registry/index/{idx_primary} ]; then INDEX_SRC=/usr/local/cargo/registry/index/{idx_primary}; \
elif [ -d /usr/local/cargo/registry/index/{idx_alt} ]; then INDEX_SRC=/usr/local/cargo/registry/index/{idx_alt}; fi; \
if [ -n \"$INDEX_SRC\" ]; then \
if [ ! -e \"$CARGO_HOME/registry/index/{idx_primary}\" ]; then ln -s \"$INDEX_SRC\" \"$CARGO_HOME/registry/index/{idx_primary}\"; fi; \
if [ ! -e \"$CARGO_HOME/registry/index/{idx_alt}\" ]; then ln -s \"$INDEX_SRC\" \"$CARGO_HOME/registry/index/{idx_alt}\"; fi; \
fi; \
CACHE_SRC=\"\"; \
if [ -d /usr/local/cargo/registry/cache/{idx_primary} ]; then CACHE_SRC=/usr/local/cargo/registry/cache/{idx_primary}; \
elif [ -d /usr/local/cargo/registry/cache/{idx_alt} ]; then CACHE_SRC=/usr/local/cargo/registry/cache/{idx_alt}; fi; \
if [ -n \"$CACHE_SRC\" ]; then \
if [ ! -e \"$CARGO_HOME/registry/cache/{idx_primary}\" ]; then ln -s \"$CACHE_SRC\" \"$CARGO_HOME/registry/cache/{idx_primary}\"; fi; \
if [ ! -e \"$CARGO_HOME/registry/cache/{idx_alt}\" ]; then ln -s \"$CACHE_SRC\" \"$CARGO_HOME/registry/cache/{idx_alt}\"; fi; \
fi; \
SRC_SRC=\"\"; \
if [ -d /usr/local/cargo/registry/src/{idx_primary} ]; then SRC_SRC=/usr/local/cargo/registry/src/{idx_primary}; \
elif [ -d /usr/local/cargo/registry/src/{idx_alt} ]; then SRC_SRC=/usr/local/cargo/registry/src/{idx_alt}; fi; \
if [ -n \"$SRC_SRC\" ]; then \
if [ ! -e \"$CARGO_HOME/registry/src/{idx_primary}\" ]; then ln -s \"$SRC_SRC\" \"$CARGO_HOME/registry/src/{idx_primary}\"; fi; \
if [ ! -e \"$CARGO_HOME/registry/src/{idx_alt}\" ]; then ln -s \"$SRC_SRC\" \"$CARGO_HOME/registry/src/{idx_alt}\"; fi; \
fi; \
if [ -d /usr/local/cargo/git ] && [ ! -e \"$CARGO_HOME/git\" ]; then ln -s /usr/local/cargo/git \"$CARGO_HOME/git\"; fi; \
TOOLBIN=$(find /usr/local/rustup/toolchains -maxdepth 2 -type d -name bin 2>/dev/null | head -n 1); \
if [ -n \"$TOOLBIN\" ]; then export PATH=\"$TOOLBIN:$PATH\"; fi; \
if ! command -v rg >/dev/null 2>&1; then \
rg() {{ \
if [ \"$1\" = \"--files\" ]; then \
shift; \
if [ \"$#\" -eq 0 ]; then find . -type f; else find \"$@\" -type f; fi; \
return 0; \
fi; \
echo '{tag} ripgrep (rg) is not installed in this ghost image. Fallback supports only `rg --files ...` via `find ... -type f`.' >&2; \
return 127; \
}}; \
fi",
        tmp = CONTAINER_TMPDIR,
        cargo = CONTAINER_CARGO_HOME,
        rustup = CONTAINER_RUSTUP_HOME,
        idx_primary = CRATES_INDEX_HASH_PRIMARY,
        idx_alt = CRATES_INDEX_HASH_ALT,
        tag = tag
    )
}

fn wrap_exec_command(cmd: &str) -> String {
    format!("{}; {}", exec_shell_prelude(), cmd)
}

fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn host_exec_shell_prelude(workdir: &Path) -> String {
    let tag = reason_codes::reason_tag(reason_codes::REASON_GHOST_TOOL_UNAVAILABLE);
    let workdir_q = shell_single_quote(workdir.to_string_lossy().as_ref());
    format!(
        "ATHENA_HOST_WORKSPACE={workdir}; ATHENA_WORKSPACE_ALIAS={alias}; \
mkdir -p /tmp; ln -sfn \"$ATHENA_HOST_WORKSPACE\" \"$ATHENA_WORKSPACE_ALIAS\"; \
if ! command -v rg >/dev/null 2>&1; then \
rg() {{ \
if [ \"$1\" = \"--files\" ]; then \
shift; \
if [ \"$#\" -eq 0 ]; then find . -type f; else find \"$@\" -type f; fi; \
return 0; \
fi; \
echo '{tag} ripgrep (rg) is not installed on host. Fallback supports only `rg --files ...` via `find ... -type f`.' >&2; \
return 127; \
}}; \
fi",
        workdir = workdir_q,
        alias = HOST_WORKSPACE_ALIAS,
        tag = tag
    )
}

fn rewrite_workspace_paths_for_host(cmd: &str) -> String {
    cmd.replace("/workspace", HOST_WORKSPACE_ALIAS)
}

fn wrap_host_exec_command(workdir: &Path, cmd: &str) -> String {
    format!(
        "{}; {}",
        host_exec_shell_prelude(workdir),
        rewrite_workspace_paths_for_host(cmd)
    )
}

fn first_workspace_mount(ghost: &GhostConfig) -> Option<PathBuf> {
    ghost
        .mounts
        .iter()
        .find(|m| m.container_path == "/workspace")
        .or_else(|| ghost.mounts.first())
        .map(|m| PathBuf::from(Config::resolve_mount_path(&m.host_path)))
}

fn normalized_repo_token(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn workspace_repo_name(workspace: &Path) -> Option<String> {
    workspace
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalized_repo_token)
        .filter(|name| !name.is_empty())
}

fn resolve_trusted_host_workspace(
    ghost: &GhostConfig,
    trusted_repos: &[String],
) -> Option<PathBuf> {
    if trusted_repos.is_empty() {
        return None;
    }

    let trusted = trusted_repos
        .iter()
        .map(|repo| normalized_repo_token(repo))
        .filter(|repo| !repo.is_empty())
        .collect::<HashSet<_>>();
    if trusted.is_empty() {
        return None;
    }

    let mount = first_workspace_mount(ghost)?;
    let repo_name = workspace_repo_name(&mount)?;
    if !trusted.contains(&repo_name) {
        return None;
    }

    mount.exists().then_some(mount)
}

fn mark_workspace_warm_started(workspace: &Path) -> bool {
    let key = workspace.to_string_lossy().to_string();
    let warmed = WARMED_WORKSPACES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = warmed.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(key)
}

fn unmark_workspace_warm(workspace: &Path) {
    let key = workspace.to_string_lossy().to_string();
    if let Some(warmed) = WARMED_WORKSPACES.get() {
        let mut guard = warmed.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(&key);
    }
}

fn cargo_fetch_locked(workspace: &Path, target: &str) -> Result<()> {
    let status = StdCommand::new("cargo")
        .arg("fetch")
        .arg("--locked")
        .arg("--target")
        .arg(target)
        .current_dir(workspace)
        .status()
        .map_err(|e| {
            AthenaError::Tool(format!(
                "Failed to execute cargo fetch for target {} in {}: {}",
                target,
                workspace.display(),
                e
            ))
        })?;

    if status.success() {
        Ok(())
    } else {
        Err(AthenaError::Tool(format!(
            "cargo fetch failed for target {} in {} with status {}",
            target,
            workspace.display(),
            status
        )))
    }
}

async fn warm_host_cargo_cache(ghost: &GhostConfig) {
    let Some(workspace) = first_workspace_mount(ghost) else {
        return;
    };
    if !workspace.join("Cargo.toml").exists() {
        return;
    }
    if !mark_workspace_warm_started(&workspace) {
        return;
    }

    let workspace_for_fetch = workspace.clone();
    let fetch_result = tokio::task::spawn_blocking(move || -> Result<()> {
        tracing::info!(
            workspace = %workspace_for_fetch.display(),
            "Warming host cargo registry for offline ghost execution"
        );
        let mut success = false;
        for target in [LINUX_TARGET_X86_64, LINUX_TARGET_AARCH64] {
            match cargo_fetch_locked(&workspace_for_fetch, target) {
                Ok(()) => {
                    success = true;
                }
                Err(err) => {
                    tracing::warn!(
                        workspace = %workspace_for_fetch.display(),
                        target = target,
                        error = %err,
                        "cargo fetch target warm-up failed"
                    );
                }
            }
        }
        if success {
            Ok(())
        } else {
            Err(AthenaError::Tool(format!(
                "Unable to warm cargo registry for linux targets in {}",
                workspace_for_fetch.display()
            )))
        }
    })
    .await;

    match fetch_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                workspace = %workspace.display(),
                error = %err,
                "Cargo warm-up did not complete; offline container builds may fail"
            );
            unmark_workspace_warm(&workspace);
        }
        Err(join_err) => {
            tracing::warn!(
                workspace = %workspace.display(),
                error = %join_err,
                "Cargo warm-up task join error; offline container builds may fail"
            );
            unmark_workspace_warm(&workspace);
        }
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveContainerSecurity {
    pub container_mode: &'static str,
    pub caps_dropped: Vec<String>,
    pub rootfs_readonly: bool,
    pub network_mode: &'static str,
    pub pid_limit: i64,
    pub memory_limit: i64,
    pub cpu_quota: i64,
}

pub fn effective_container_security(docker_config: &DockerConfig) -> EffectiveContainerSecurity {
    EffectiveContainerSecurity {
        container_mode: CONTAINER_MODE,
        caps_dropped: vec![CAP_DROP_ALL.to_string()],
        rootfs_readonly: ROOTFS_READONLY,
        network_mode: NETWORK_MODE_NONE,
        pid_limit: DEFAULT_PIDS_LIMIT,
        memory_limit: docker_config.memory_limit,
        cpu_quota: docker_config.cpu_quota,
    }
}

enum SessionBackend {
    Container {
        docker: Docker,
        container_id: String,
    },
    Host {
        workspace: PathBuf,
    },
}

pub struct DockerSession {
    backend: SessionBackend,
    timeout_secs: u64,
    session_id: String,
}

impl DockerSession {
    /// Create and start a hardened execution session.
    /// Uses Docker by default; when `trusted_host_repos` is provided and the
    /// mounted workspace repo is allowlisted, uses trusted host execution mode.
    pub async fn new(
        ghost: &GhostConfig,
        docker_config: &DockerConfig,
        trusted_host_repos: Option<&[String]>,
    ) -> Result<Self> {
        let session_id = uuid::Uuid::new_v4().to_string();
        warm_host_cargo_cache(ghost).await;

        if let Some(trusted_repos) = trusted_host_repos {
            if let Some(workspace) = resolve_trusted_host_workspace(ghost, trusted_repos) {
                tracing::warn!(
                    ghost = %ghost.name,
                    workspace = %workspace.display(),
                    "Using trusted host execution mode"
                );
                return Ok(Self {
                    backend: SessionBackend::Host { workspace },
                    timeout_secs: docker_config.timeout_secs,
                    session_id,
                });
            }
        }

        let docker = Docker::connect_with_socket(
            &docker_config.socket_path,
            120,
            bollard::API_DEFAULT_VERSION,
        )?;

        // Build mounts
        let mut mounts: Vec<Mount> = ghost
            .mounts
            .iter()
            .map(|m| {
                let host = Config::resolve_mount_path(&m.host_path);
                Mount {
                    target: Some(m.container_path.clone()),
                    source: Some(host),
                    typ: Some(MountTypeEnum::BIND),
                    read_only: Some(m.read_only),
                    ..Default::default()
                }
            })
            .collect();

        // Mount host cargo registry (read-only) so `cargo check/test` works offline
        let cargo_home = std::env::var("CARGO_HOME")
            .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_default()));
        let cargo_registry = format!("{}/registry", cargo_home);
        if std::path::Path::new(&cargo_registry).exists() {
            mounts.push(Mount {
                target: Some("/usr/local/cargo/registry".into()),
                source: Some(cargo_registry),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            });
        }
        let cargo_git = format!("{}/git", cargo_home);
        if std::path::Path::new(&cargo_git).exists() {
            mounts.push(Mount {
                target: Some("/usr/local/cargo/git".into()),
                source: Some(cargo_git),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            });
        }

        let host_config = HostConfig {
            mounts: Some(mounts),
            readonly_rootfs: Some(ROOTFS_READONLY),
            cap_drop: Some(vec![CAP_DROP_ALL.into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            network_mode: Some(NETWORK_MODE_NONE.into()),
            memory: Some(docker_config.memory_limit),
            cpu_quota: Some(docker_config.cpu_quota),
            pids_limit: Some(DEFAULT_PIDS_LIMIT),
            // Writable /tmp for tools that need scratch space
            tmpfs: Some(HashMap::from([(
                "/tmp".into(),
                "rw,noexec,nosuid,size=64m".into(),
            )])),
            ..Default::default()
        };

        let container_name = format!("athena-{}-{}", ghost.name, uuid::Uuid::new_v4().simple());

        let image = ghost.image.as_deref().unwrap_or(&docker_config.image);

        let config = ContainerConfig {
            image: Some(image.to_string()),
            user: Some("65534:65534".into()),
            cmd: Some(vec!["sleep".into(), "infinity".into()]),
            env: Some(exec_env()),
            working_dir: Some(
                ghost
                    .mounts
                    .first()
                    .map(|m| m.container_path.clone())
                    .unwrap_or_else(|| "/".into()),
            ),
            host_config: Some(host_config),
            ..Default::default()
        };

        let resp = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: &container_name,
                    platform: None,
                }),
                config,
            )
            .await?;

        docker
            .start_container(&resp.id, None::<StartContainerOptions<String>>)
            .await?;

        tracing::info!(container_id = %resp.id, ghost = %ghost.name, "Container started");

        Ok(Self {
            backend: SessionBackend::Container {
                docker,
                container_id: resp.id,
            },
            timeout_secs: docker_config.timeout_secs,
            session_id,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn execution_mode(&self) -> &'static str {
        match self.backend {
            SessionBackend::Container { .. } => CONTAINER_MODE,
            SessionBackend::Host { .. } => HOST_TRUSTED_MODE,
        }
    }

    /// Execute a command, returning combined stdout+stderr
    pub async fn exec(&self, cmd: &str) -> Result<String> {
        match &self.backend {
            SessionBackend::Container {
                docker,
                container_id,
            } => {
                let wrapped_cmd = wrap_exec_command(cmd);
                let exec = docker
                    .create_exec(
                        container_id,
                        CreateExecOptions::<String> {
                            cmd: Some(vec!["sh".into(), "-c".into(), wrapped_cmd]),
                            env: Some(exec_env()),
                            attach_stdout: Some(true),
                            attach_stderr: Some(true),
                            ..Default::default()
                        },
                    )
                    .await?;

                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(self.timeout_secs),
                    self.collect_exec_output(docker, &exec.id),
                )
                .await
                .map_err(|_| AthenaError::Timeout(self.timeout_secs))??;

                Ok(output)
            }
            SessionBackend::Host { workspace } => {
                let wrapped_cmd = wrap_host_exec_command(workspace, cmd);
                self.exec_host(workspace, &wrapped_cmd).await
            }
        }
    }

    /// Execute a command with stdin input (for file writes)
    pub async fn exec_with_stdin(&self, cmd: &str, stdin_data: &str) -> Result<String> {
        // Encode stdin data as base64 to avoid shell injection via crafted content
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(stdin_data.as_bytes());
        let full_cmd = format!("echo '{}' | base64 -d | {}", encoded, cmd);
        self.exec(&full_cmd).await
    }

    async fn collect_exec_output(&self, docker: &Docker, exec_id: &str) -> Result<String> {
        let start_result = docker.start_exec(exec_id, None).await?;

        let mut output = String::new();
        if let StartExecResults::Attached {
            output: mut stream, ..
        } = start_result
        {
            while let Some(Ok(msg)) = stream.next().await {
                use std::fmt::Write;
                let _ = write!(output, "{}", msg);
            }
        }

        Ok(output)
    }

    async fn exec_host(&self, workspace: &Path, cmd: &str) -> Result<String> {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(cmd).current_dir(workspace);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            command.output(),
        )
        .await
        .map_err(|_| AthenaError::Timeout(self.timeout_secs))?
        .map_err(|e| AthenaError::Tool(format!("host execution failed: {}", e)))?;

        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(combined)
    }

    /// Kill and remove the container (no-op for trusted host mode)
    pub async fn close(self) -> Result<()> {
        match self.backend {
            SessionBackend::Container {
                docker,
                container_id,
            } => {
                tracing::info!(container_id = %container_id, "Closing container");

                // Kill if running (ignore errors — may already be stopped)
                let _ = docker.kill_container::<&str>(&container_id, None).await;

                docker
                    .remove_container(
                        &container_id,
                        Some(RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await?;

                Ok(())
            }
            SessionBackend::Host { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_trusted_host_workspace, rewrite_workspace_paths_for_host, wrap_exec_command,
        CONTAINER_CARGO_HOME, CONTAINER_RUSTUP_HOME, CONTAINER_TMPDIR, CRATES_INDEX_HASH_ALT,
        CRATES_INDEX_HASH_PRIMARY, HOST_WORKSPACE_ALIAS,
    };
    use crate::config::{GhostConfig, MountConfig};
    use crate::reason_codes;
    use std::path::Path;

    fn make_ghost(host_path: &str) -> GhostConfig {
        GhostConfig {
            name: "coder".to_string(),
            description: "test".to_string(),
            tools: vec!["shell".to_string()],
            mounts: vec![MountConfig {
                host_path: host_path.to_string(),
                container_path: "/workspace".to_string(),
                read_only: false,
            }],
            strategy: "code".to_string(),
            soul_file: None,
            soul: None,
            image: None,
        }
    }

    #[test]
    fn wrapped_exec_command_sets_writable_rust_env() {
        let wrapped = wrap_exec_command("cargo check");
        assert!(wrapped.contains(&format!("TMPDIR=\"{}\"", CONTAINER_TMPDIR)));
        assert!(wrapped.contains(&format!("CARGO_HOME=\"{}\"", CONTAINER_CARGO_HOME)));
        assert!(wrapped.contains(&format!("RUSTUP_HOME=\"{}\"", CONTAINER_RUSTUP_HOME)));
        assert!(wrapped.contains("CARGO_NET_OFFLINE=true"));
        assert!(wrapped.contains("RUSTUP_SKIP_UPDATE_CHECK=1"));
        assert!(wrapped.contains("RUSTUP_AUTO_INSTALL=0"));
        assert!(wrapped.contains("TOOLBIN=$(find /usr/local/rustup/toolchains"));
        assert!(wrapped.contains("export PATH=\"$TOOLBIN:$PATH\""));
        assert!(
            wrapped.contains("mkdir -p \"$CARGO_HOME/registry\" \"$CARGO_HOME/registry/index\"")
        );
        assert!(wrapped.contains(&format!(
            "/usr/local/cargo/registry/index/{}",
            CRATES_INDEX_HASH_PRIMARY
        )));
        assert!(wrapped.contains(&format!(
            "/usr/local/cargo/registry/index/{}",
            CRATES_INDEX_HASH_ALT
        )));
        assert!(wrapped.contains(
            "mkdir -p \"$TMPDIR\" \"$CARGO_HOME\" \"$RUSTUP_HOME\" \"$RUSTUP_HOME/tmp\""
        ));
    }

    #[test]
    fn wrapped_exec_command_contains_rg_files_fallback() {
        let wrapped = wrap_exec_command("rg --files src | wc -l");
        assert!(wrapped.contains("if [ \"$1\" = \"--files\" ]"));
        assert!(wrapped.contains("find \"$@\" -type f"));
    }

    #[test]
    fn wrapped_exec_command_tags_rg_missing_errors_with_reason_code() {
        let wrapped = wrap_exec_command("rg foo src");
        let expected_tag = reason_codes::reason_tag(reason_codes::REASON_GHOST_TOOL_UNAVAILABLE);
        assert!(wrapped.contains(&expected_tag));
    }

    #[test]
    fn rewrite_workspace_paths_for_host_maps_container_workspace_alias() {
        let cmd = "cat /workspace/src/main.rs && ls /workspace";
        let rewritten = rewrite_workspace_paths_for_host(cmd);
        assert!(rewritten.contains(HOST_WORKSPACE_ALIAS));
        assert!(!rewritten.contains("/workspace"));
    }

    #[test]
    fn trusted_host_workspace_resolution_matches_repo_name() {
        let workspace = std::env::current_dir()
            .ok()
            .and_then(|dir| {
                dir.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "athena".to_string());
        let ghost = make_ghost(".");
        let trusted = vec![workspace.to_ascii_lowercase()];
        let resolved = resolve_trusted_host_workspace(&ghost, &trusted);
        assert!(resolved.as_deref().map(Path::exists).unwrap_or(false));
    }

    #[test]
    fn trusted_host_workspace_resolution_rejects_untrusted_repo() {
        let ghost = make_ghost(".");
        let trusted = vec!["different-repo".to_string()];
        assert!(resolve_trusted_host_workspace(&ghost, &trusted).is_none());
    }
}

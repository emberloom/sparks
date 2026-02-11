use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, RemoveContainerOptions,
    StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{HostConfig, Mount, MountTypeEnum};
use bollard::Docker;
use futures::StreamExt;
use std::collections::HashMap;

use crate::config::{GhostConfig, Config, DockerConfig};
use crate::error::{AthenaError, Result};

pub struct DockerSession {
    docker: Docker,
    container_id: String,
    timeout_secs: u64,
}

impl DockerSession {
    /// Create and start a hardened container for a ghost task
    pub async fn new(
        ghost: &GhostConfig,
        docker_config: &DockerConfig,
    ) -> Result<Self> {
        let docker = Docker::connect_with_socket(
            &docker_config.socket_path,
            120,
            bollard::API_DEFAULT_VERSION,
        )?;

        // Build mounts
        let mounts: Vec<Mount> = ghost.mounts.iter().map(|m| {
            let host = Config::resolve_mount_path(&m.host_path);
            Mount {
                target: Some(m.container_path.clone()),
                source: Some(host),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(m.read_only),
                ..Default::default()
            }
        }).collect();

        let host_config = HostConfig {
            mounts: Some(mounts),
            readonly_rootfs: Some(true),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            network_mode: Some("none".into()),
            memory: Some(docker_config.memory_limit),
            cpu_quota: Some(docker_config.cpu_quota),
            pids_limit: Some(256),
            // Writable /tmp for tools that need scratch space
            tmpfs: Some(HashMap::from([
                ("/tmp".into(), "rw,noexec,nosuid,size=64m".into()),
            ])),
            ..Default::default()
        };

        let container_name = format!("athena-{}-{}", ghost.name, uuid::Uuid::new_v4().simple());

        let config = ContainerConfig {
            image: Some(docker_config.image.clone()),
            user: Some("65534:65534".into()),
            cmd: Some(vec!["sleep".into(), "infinity".into()]),
            working_dir: Some(
                ghost.mounts.first()
                    .map(|m| m.container_path.clone())
                    .unwrap_or_else(|| "/".into())
            ),
            host_config: Some(host_config),
            ..Default::default()
        };

        let resp = docker
            .create_container(
                Some(CreateContainerOptions { name: &container_name, platform: None }),
                config,
            )
            .await?;

        docker
            .start_container(&resp.id, None::<StartContainerOptions<String>>)
            .await?;

        tracing::info!(container_id = %resp.id, ghost = %ghost.name, "Container started");

        Ok(Self {
            docker,
            container_id: resp.id,
            timeout_secs: docker_config.timeout_secs,
        })
    }

    /// Execute a command in the container, returning combined stdout+stderr
    pub async fn exec(&self, cmd: &str) -> Result<String> {
        let exec = self.docker.create_exec(
            &self.container_id,
            CreateExecOptions::<String> {
                cmd: Some(vec!["sh".into(), "-c".into(), cmd.into()]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        ).await?;

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.collect_exec_output(&exec.id),
        ).await
            .map_err(|_| AthenaError::Timeout(self.timeout_secs))??;

        Ok(output)
    }

    /// Execute a command with stdin input (for file writes)
    pub async fn exec_with_stdin(&self, cmd: &str, stdin_data: &str) -> Result<String> {
        // Encode stdin data as base64 to avoid shell injection via crafted content
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(stdin_data.as_bytes());
        let full_cmd = format!("echo '{}' | base64 -d | {}", encoded, cmd);
        self.exec(&full_cmd).await
    }

    async fn collect_exec_output(&self, exec_id: &str) -> Result<String> {
        let start_result = self.docker.start_exec(exec_id, None).await?;

        let mut output = String::new();
        if let StartExecResults::Attached { output: mut stream, .. } = start_result {
            while let Some(Ok(msg)) = stream.next().await {
                use std::fmt::Write;
                let _ = write!(output, "{}", msg);
            }
        }

        Ok(output)
    }

    /// Kill and remove the container
    pub async fn close(self) -> Result<()> {
        tracing::info!(container_id = %self.container_id, "Closing container");

        // Kill if running (ignore errors — may already be stopped)
        let _ = self.docker.kill_container::<&str>(&self.container_id, None).await;

        self.docker.remove_container(
            &self.container_id,
            Some(RemoveContainerOptions { force: true, ..Default::default() }),
        ).await?;

        Ok(())
    }
}

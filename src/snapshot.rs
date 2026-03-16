//! Workspace snapshot and time-travel debugging.
//!
//! Creates compressed tar.gz snapshots of the workspace before agent task execution.
//! Supports listing, diffing, and restoring snapshots.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::SnapshotConfig;
use crate::error::{SparksError, Result};

/// Metadata for a single snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotMeta {
    pub id: String,
    pub created_at: String,
    pub session_key: String,
    pub label: Option<String>,
    pub size_bytes: u64,
    pub path: PathBuf,
}

impl SnapshotMeta {
    pub fn size_human(&self) -> String {
        let kb = self.size_bytes / 1024;
        if kb < 1024 {
            format!("{} KB", kb)
        } else {
            format!("{:.1} MB", kb as f64 / 1024.0)
        }
    }
}

/// Store for workspace snapshots.
pub struct SnapshotStore {
    config: SnapshotConfig,
    workspace_root: PathBuf,
}

impl SnapshotStore {
    pub fn new(config: SnapshotConfig, workspace_root: PathBuf) -> Self {
        Self { config, workspace_root }
    }

    fn snapshot_dir(&self) -> PathBuf {
        if let Some(dir) = &self.config.snapshot_dir {
            PathBuf::from(dir)
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".sparks")
                .join("snapshots")
        }
    }

    /// Create a new snapshot of the workspace.
    /// Returns the snapshot metadata on success.
    pub fn create(&self, session_key: &str, label: Option<&str>) -> Result<SnapshotMeta> {
        if !self.config.enabled {
            return Err(SparksError::Config("Snapshots are not enabled (set snapshot.enabled = true)".into()));
        }

        // Check workspace size
        if self.config.max_workspace_mb > 0 {
            let size_mb = workspace_size_mb(&self.workspace_root);
            if size_mb > self.config.max_workspace_mb {
                return Err(SparksError::Config(format!(
                    "Workspace is {}MB, exceeding snapshot limit of {}MB",
                    size_mb, self.config.max_workspace_mb
                )));
            }
        }

        let snap_dir = self.snapshot_dir();
        std::fs::create_dir_all(&snap_dir).map_err(|e| SparksError::Tool(e.to_string()))?;

        let id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let filename = format!("{}.tar.gz", id);
        let snap_path = snap_dir.join(&filename);

        // Build tar command with exclusions
        let mut cmd = Command::new("tar");
        cmd.arg("czf")
           .arg(&snap_path);
        for excl in &self.config.exclude {
            cmd.arg(format!("--exclude={}", excl));
        }
        cmd.arg("-C").arg(&self.workspace_root);
        for incl in &self.config.include {
            cmd.arg(incl);
        }

        let output = cmd.output().map_err(|e| SparksError::Tool(format!("tar failed: {}", e)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SparksError::Tool(format!("tar error: {}", stderr)));
        }

        let size_bytes = std::fs::metadata(&snap_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let meta = SnapshotMeta {
            id: id.clone(),
            created_at,
            session_key: session_key.to_string(),
            label: label.map(str::to_string),
            size_bytes,
            path: snap_path.clone(),
        };

        // Save metadata sidecar
        let meta_path = snap_path.with_extension("").with_extension("json");
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| SparksError::Internal(e.to_string()))?;
        std::fs::write(&meta_path, meta_json).map_err(|e| SparksError::Tool(e.to_string()))?;

        // Prune old snapshots if over limit
        if self.config.max_snapshots > 0 {
            self.prune_old_snapshots()?;
        }

        Ok(meta)
    }

    /// List all snapshots, newest first.
    pub fn list(&self) -> Result<Vec<SnapshotMeta>> {
        let snap_dir = self.snapshot_dir();
        if !snap_dir.exists() {
            return Ok(vec![]);
        }

        let mut metas = Vec::new();
        for entry in std::fs::read_dir(&snap_dir).map_err(|e| SparksError::Tool(e.to_string()))? {
            let entry = entry.map_err(|e| SparksError::Tool(e.to_string()))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(meta) = serde_json::from_str::<SnapshotMeta>(&content) {
                        metas.push(meta);
                    }
                }
            }
        }
        metas.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(metas)
    }

    /// Get a snapshot by ID prefix.
    pub fn get(&self, id_prefix: &str) -> Result<SnapshotMeta> {
        let all = self.list()?;
        let matches: Vec<_> = all.into_iter().filter(|m| m.id.starts_with(id_prefix)).collect();
        match matches.len() {
            0 => Err(SparksError::Tool(format!("No snapshot found with id starting '{}'", id_prefix))),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(SparksError::Tool(format!("{} snapshots match '{}', be more specific", n, id_prefix))),
        }
    }

    /// Show file-level diff between two snapshots.
    pub fn diff(&self, id_a: &str, id_b: &str) -> Result<String> {
        let meta_a = self.get(id_a)?;
        let meta_b = self.get(id_b)?;

        // Extract both to temp dirs
        let tmp_a = std::env::temp_dir().join(format!("sparks_snap_{}", &meta_a.id[..8]));
        let tmp_b = std::env::temp_dir().join(format!("sparks_snap_{}", &meta_b.id[..8]));
        std::fs::create_dir_all(&tmp_a).ok();
        std::fs::create_dir_all(&tmp_b).ok();

        extract_snapshot(&meta_a.path, &tmp_a)?;
        extract_snapshot(&meta_b.path, &tmp_b)?;

        // Use diff -rq for file-level diff
        let output = Command::new("diff")
            .arg("-rq")
            .arg("--brief")
            .arg(&tmp_a)
            .arg(&tmp_b)
            .output()
            .map_err(|e| SparksError::Tool(format!("diff failed: {}", e)))?;

        let diff_text = String::from_utf8_lossy(&output.stdout).to_string();
        let header = format!(
            "Diff: {} ({}) -> {} ({})\n\n",
            &meta_a.id[..8], meta_a.created_at,
            &meta_b.id[..8], meta_b.created_at,
        );

        // Cleanup temp dirs
        std::fs::remove_dir_all(&tmp_a).ok();
        std::fs::remove_dir_all(&tmp_b).ok();

        if diff_text.is_empty() {
            Ok(format!("{}No differences found.", header))
        } else {
            let cleaned = diff_text
                .replace(tmp_a.to_string_lossy().as_ref(), "snapshot_a")
                .replace(tmp_b.to_string_lossy().as_ref(), "snapshot_b");
            Ok(format!("{}{}", header, cleaned))
        }
    }

    /// Restore a snapshot to the workspace (dry-run by default).
    pub fn restore(&self, id_prefix: &str, dry_run: bool) -> Result<String> {
        let meta = self.get(id_prefix)?;
        if dry_run {
            return Ok(format!(
                "Would restore snapshot {} ({}) to {}\nUse --apply to actually restore.",
                &meta.id[..8], meta.created_at, self.workspace_root.display()
            ));
        }
        extract_snapshot(&meta.path, &self.workspace_root)?;
        Ok(format!(
            "Restored snapshot {} ({}) to {}",
            &meta.id[..8], meta.created_at, self.workspace_root.display()
        ))
    }

    fn prune_old_snapshots(&self) -> Result<()> {
        let mut all = self.list()?;
        while all.len() > self.config.max_snapshots {
            if let Some(oldest) = all.pop() {
                std::fs::remove_file(&oldest.path).ok();
                let meta_path = oldest.path.with_extension("").with_extension("json");
                std::fs::remove_file(&meta_path).ok();
            }
        }
        Ok(())
    }
}

fn extract_snapshot(archive: &Path, dest: &Path) -> Result<()> {
    let output = Command::new("tar")
        .arg("xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .output()
        .map_err(|e| SparksError::Tool(format!("tar extract failed: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SparksError::Tool(format!("tar extract error: {}", stderr)));
    }
    Ok(())
}

fn workspace_size_mb(root: &Path) -> u64 {
    let output = Command::new("du")
        .arg("-sm")
        .arg(root)
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
        }
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_meta_size_human_kb() {
        let meta = SnapshotMeta {
            id: "test".into(),
            created_at: "2026-01-01 00:00:00".into(),
            session_key: "s".into(),
            label: None,
            size_bytes: 512 * 1024,
            path: PathBuf::from("/tmp/test.tar.gz"),
        };
        assert_eq!(meta.size_human(), "512 KB");
    }

    #[test]
    fn snapshot_meta_size_human_mb() {
        let meta = SnapshotMeta {
            id: "test".into(),
            created_at: "2026-01-01 00:00:00".into(),
            session_key: "s".into(),
            label: None,
            size_bytes: 2 * 1024 * 1024,
            path: PathBuf::from("/tmp/test.tar.gz"),
        };
        assert!(meta.size_human().contains("MB"));
    }

    #[test]
    fn snapshot_config_defaults() {
        let c = SnapshotConfig::default();
        assert!(!c.enabled);  // opt-in
        assert_eq!(c.max_snapshots, 20);
        assert!(!c.exclude.is_empty());
        assert!(c.exclude.iter().any(|e| e.contains("target")));
    }

    #[test]
    fn snapshot_store_list_empty_dir() {
        let tmp = std::env::temp_dir().join(format!("sparks_snap_test_{}", uuid::Uuid::new_v4()));
        let mut config = SnapshotConfig::default();
        config.snapshot_dir = Some(tmp.to_string_lossy().to_string());
        let store = SnapshotStore::new(config, PathBuf::from("."));
        let list = store.list().unwrap();
        assert!(list.is_empty());
    }
}

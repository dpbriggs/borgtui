use crate::{
    borgtui::CommandResponse,
    profiles::{Passphrase, PruneOptions, Repository, RepositoryOptions},
    types::{
        Archive, BackupCreateProgress, BackupCreationProgress, BorgResult, CheckComplete,
        CommandResponseSender, RepositoryArchives,
    },
};
use anyhow::anyhow;
use async_trait::async_trait;
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::Semaphore,
};

use super::backup_provider::BackupProvider;

const LOGGING_THROTTLE_TIME: std::time::Duration = std::time::Duration::from_millis(40);

#[derive(Deserialize)]
struct ResticSnapshot {
    time: String,
    // ignore the rest of the fields
}

#[derive(Deserialize)]
struct ResticProgress<'a> {
    message_type: &'a str,
    total_files: Option<u64>,
    total_bytes: Option<u64>,
    bytes_done: Option<u64>,
    current_files: Option<Vec<&'a str>>,
}

pub(crate) struct ResticProvider;

#[async_trait]
impl BackupProvider for ResticProvider {
    async fn create_backup(
        &self,
        _archive_name: String, // restic doesn't have named archives in the same way as borg
        backup_paths: &[PathBuf],
        exclude_patterns: &[String],
        exclude_caches: bool,
        repo: Repository,
        progress_channel: CommandResponseSender,
        completion_semaphore: Arc<Semaphore>,
    ) -> BorgResult<()> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to create a backup."))?;

        let mut command = tokio::process::Command::new("restic");
        command
            .arg("backup")
            .arg("--repo")
            .arg(repo.path())
            .arg("--json")
            .args(backup_paths)
            .args(["--tag", "borgtui"])
            .env("RESTIC_PASSWORD", passphrase.inner());

        for pattern in exclude_patterns {
            command.arg("--exclude").arg(pattern);
        }

        if exclude_caches {
            command.arg("--exclude-caches");
        }

        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        let repo_path = repo.path();
        let progress_channel_clone = progress_channel.clone();

        tokio::spawn(async move {
            let mut last_update = std::time::Instant::now();
            while let Ok(Some(line)) = lines.next_line().await {
                if last_update.elapsed() < LOGGING_THROTTLE_TIME {
                    continue;
                }
                last_update = std::time::Instant::now();
                if let Ok(progress) = serde_json::from_str::<ResticProgress>(&line) {
                    if progress.message_type == "status" {
                        let create_progress = BackupCreateProgress {
                            repository: repo_path.clone(),
                            create_progress: BackupCreationProgress::InProgress {
                                original_size: progress.total_bytes.unwrap_or(0),
                                compressed_size: progress.bytes_done.unwrap_or(0),
                                deduplicated_size: progress.bytes_done.unwrap_or(0),
                                num_files: progress.total_files.unwrap_or(0),
                                current_path: progress.current_files.unwrap_or_default().join(", "),
                            },
                        };
                        if let Err(e) = progress_channel_clone
                            .send(CommandResponse::CreateProgress(create_progress))
                            .await
                        {
                            tracing::error!("Failed to send progress: {}", e);
                        }
                    }
                }
            }
            completion_semaphore.add_permits(1);
        });

        Ok(())
    }

    async fn list_archives(&self, repo: &Repository) -> BorgResult<RepositoryArchives> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to list archives."))?;

        let mut command = tokio::process::Command::new("restic");
        command
            .arg("snapshots")
            .arg("--repo")
            .arg(repo.path())
            .arg("--json")
            .env("RESTIC_PASSWORD", passphrase.inner())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command.spawn()?.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Failed to list restic snapshots: {}", stderr));
        }

        let snapshots: Vec<ResticSnapshot> = serde_json::from_slice(&output.stdout)?;

        let archives = snapshots
            .into_iter()
            .map(|snapshot| {
                let creation_date = chrono::DateTime::parse_from_rfc3339(&snapshot.time)
                    .unwrap()
                    .naive_utc();
                Archive {
                    name: snapshot.time,
                    creation_date,
                }
            })
            .collect();

        Ok(RepositoryArchives {
            path: repo.path(),
            archives,
        })
    }

    async fn init_repo(
        &self,
        repo_loc: String,
        passphrase: Option<Passphrase>,
        _config: RepositoryOptions,
    ) -> BorgResult<()> {
        let passphrase =
            passphrase.ok_or_else(|| anyhow!("Restic requires a password for initialization."))?;

        let mut command = tokio::process::Command::new("restic");
        command
            .arg("init")
            .arg("--repo")
            .arg(repo_loc)
            .env("RESTIC_PASSWORD", passphrase.inner())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command.spawn()?.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to initialize restic repository: {}",
                stderr
            ));
        }

        Ok(())
    }

    async fn mount(
        &self,
        repo: &Repository,
        _given_repository_path: String,
        mountpoint: PathBuf,
    ) -> BorgResult<()> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to mount."))?;
        let repo_path = repo.path();

        tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new("restic");
            command
                .arg("mount")
                .arg(&mountpoint)
                .arg("--repo")
                .arg(repo_path)
                .env("RESTIC_PASSWORD", passphrase.inner());

            let status = command.status()?;
            if !status.success() {
                return Err(anyhow!("Failed to mount restic repository"));
            }
            Ok(())
        });
        Ok(())
    }

    async fn unmount(&self, mountpoint: PathBuf) -> BorgResult<()> {
        let mut command = tokio::process::Command::new("umount");
        command.arg(mountpoint);
        let output = command.spawn()?.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Failed to unmount: {}", stderr));
        }
        Ok(())
    }

    async fn prune(
        &self,
        repo: &Repository,
        _prune_options: PruneOptions,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to prune."))?;

        let mut command = tokio::process::Command::new("restic");
        command
            .arg("prune")
            .arg("--repo")
            .arg(repo.path())
            .env("RESTIC_PASSWORD", passphrase.inner());

        let output = command.spawn()?.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Failed to prune restic repository: {}", stderr));
        }

        Ok(())
    }

    async fn compact(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        self.prune(repo, PruneOptions::default(), progress_channel)
            .await
    }

    async fn check(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to check."))?;

        let mut command = tokio::process::Command::new("restic");
        command
            .arg("check")
            .arg("--repo")
            .arg(repo.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RESTIC_PASSWORD", passphrase.inner());

        let output = command.spawn()?.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!("Restic check failed: {}", stderr);
            if let Err(e) = progress_channel
                .send(CommandResponse::CheckComplete(CheckComplete::new(
                    repo.path(),
                    Some(stderr.to_string()),
                )))
                .await
            {
                tracing::error!("Failed to send CheckComplete: {e}");
            }
            return Ok(false);
        } else {
            if let Err(e) = progress_channel
                .send(CommandResponse::CheckComplete(CheckComplete::new(
                    repo.path(),
                    None,
                )))
                .await
            {
                tracing::error!("Failed to send CheckComplete: {e}");
            }
            tracing::info!(
                "Restic check output: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }

        Ok(true)
    }

    async fn repair(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to check."))?;

        let commands = &[["repair", "index"], ["repair", "snapshots"]];
        let mut error_occured = false;

        for command in commands {
            let mut cmd = tokio::process::Command::new("restic");
            cmd.args(command)
                .arg("--repo")
                .arg(repo.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("RESTIC_PASSWORD", passphrase.inner());

            let output = cmd.spawn()?.wait_with_output().await?;
            let mut error = None;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                error = Some(stderr.to_string());
                error_occured = true;
                tracing::error!("Restic repair failed: {}", stderr);
            } else {
                tracing::info!("Restic repair command {:?} completed successfully", command);
                tracing::info!(
                    "Restic repair output: {}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }

            if let Err(e) = progress_channel
                .send(CommandResponse::CheckComplete(CheckComplete::new(
                    repo.path(),
                    error.clone(),
                )))
                .await
            {
                tracing::error!("Failed to send CheckComplete: {e:?}");
            }
        }

        Ok(!error_occured)
    }
}

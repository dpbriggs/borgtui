use crate::{
    borgtui::CommandResponse,
    profiles::{Passphrase, PruneOptions, Repository, RepositoryOptions},
    types::{
        Archive, BackupCreateProgress, BackupCreationProgress, BorgResult, CommandResponseSender,
        RepositoryArchives,
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
            while let Ok(Some(line)) = lines.next_line().await {
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
                            eprintln!("Failed to send progress: {}", e);
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
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        self.check_or_repair(repo, false).await
    }

    async fn repair(
        &self,
        repo: &Repository,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        self.check_or_repair(repo, true).await
    }
}

impl ResticProvider {
    async fn check_or_repair(&self, repo: &Repository, repair: bool) -> BorgResult<bool> {
        let passphrase = repo
            .get_passphrase()?
            .ok_or_else(|| anyhow!("Restic requires a password to check."))?;

        let mut command = tokio::process::Command::new("restic");
        command
            .arg("check")
            .arg("--repo")
            .arg(repo.path())
            .env("RESTIC_PASSWORD", passphrase.inner());

        if repair {
            command.arg("--with-cache"); // TODO: Is this the right way to repair?
        }

        let output = command.spawn()?.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Restic check failed: {}", stderr);
            return Ok(false);
        }

        Ok(true)
    }
}

use crate::types::RepositoryArchives;
use crate::{
    profiles::{Passphrase, PruneOptions, Repository},
    BorgResult,
};
use async_trait::async_trait;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

use super::borg_provider::CommandResponseSender;

#[async_trait]
pub(crate) trait BackupProvider: Send {
    #[allow(clippy::too_many_arguments)]
    async fn create_backup(
        &self,
        archive_name: String,
        backup_paths: &[PathBuf],
        exclude_patterns: &[String],
        exclude_caches: bool,
        repo: Repository,
        progress_channel: CommandResponseSender,
        completion_semaphore: Arc<Semaphore>,
    ) -> BorgResult<()>;
    async fn list_archives(&self, repo: &Repository) -> BorgResult<RepositoryArchives>;
    async fn init_repo(
        &self,
        repo_loc: String,
        passphrase: Option<Passphrase>,
        rsh: Option<String>,
    ) -> BorgResult<()>;
    async fn mount(
        &self,
        repo: &Repository,
        given_repository_path: String,
        mountpoint: PathBuf,
    ) -> BorgResult<()>;
    // TODO: Figure out unmounting
    #[allow(unused)]
    async fn unmount(&self, mountpoint: PathBuf) -> BorgResult<()>;
    async fn prune(
        &self,
        repo: &Repository,
        prune_options: PruneOptions,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()>;
    async fn compact(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()>;
    async fn check(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<bool>;
}

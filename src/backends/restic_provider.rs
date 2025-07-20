use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::{
    profiles::{Passphrase, PruneOptions, Repository, RepositoryOptions},
    types::{BorgResult, CommandResponseSender, RepositoryArchives},
};

use super::backup_provider::BackupProvider;

pub(crate) struct ResticProvider;

#[async_trait]
impl BackupProvider for ResticProvider {
    async fn create_backup(
        &self,
        _archive_name: String,
        _backup_paths: &[PathBuf],
        _exclude_patterns: &[String],
        _exclude_caches: bool,
        _repo: Repository,
        _progress_channel: CommandResponseSender,
        _completion_semaphore: Arc<Semaphore>,
    ) -> BorgResult<()> {
        todo!()
    }

    async fn list_archives(&self, _repo: &Repository) -> BorgResult<RepositoryArchives> {
        todo!()
    }

    async fn init_repo(
        &self,
        _repo_loc: String,
        _passphrase: Option<Passphrase>,
        _config: RepositoryOptions,
    ) -> BorgResult<()> {
        todo!()
    }

    async fn mount(
        &self,
        _repo: &Repository,
        _given_repository_path: String,
        _mountpoint: PathBuf,
    ) -> BorgResult<()> {
        todo!()
    }

    async fn unmount(&self, _mountpoint: PathBuf) -> BorgResult<()> {
        todo!()
    }

    async fn prune(
        &self,
        _repo: &Repository,
        _prune_options: PruneOptions,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        todo!()
    }

    async fn compact(
        &self,
        _repo: &Repository,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        todo!()
    }

    async fn check(
        &self,
        _repo: &Repository,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        todo!()
    }

    async fn repair(
        &self,
        _repo: &Repository,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        todo!()
    }
}

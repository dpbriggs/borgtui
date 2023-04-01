use crate::{profiles::Profile, types::BorgResult};
use anyhow::bail;
use borgbackup::common::CommonOptions;
use tracing::info;

fn archive_name(name: &str) -> String {
    format!(
        "{}-{}",
        name,
        chrono::Local::now().format("%Y-%m-%d:%H:%M:%S")
    )
}

pub(crate) fn create_backup(profile: &Profile) -> BorgResult<()> {
    let archive_name = archive_name(profile.name());
    for create_option in profile.borg_create_options(archive_name)? {
        info!(
            "Creating archive {} in repository {}",
            create_option.archive, create_option.repository
        );
        match borgbackup::sync::create(&create_option, &CommonOptions::default()) {
            Ok(c) => info!("Archive created successfully: {:?}", c.archive.stats),
            Err(e) => bail!(
                "Failed to create archive {} in repo {}: {:?}",
                create_option.archive,
                create_option.repository,
                e
            ),
        }
    }
    Ok(())
}

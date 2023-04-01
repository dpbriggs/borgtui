use crate::cli::Action;
use crate::profiles::Profile;
use crate::types::BorgResult;
use anyhow::Context;
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;

mod borg;
mod cli;
mod profiles;
mod types;

fn handle_non_interactive_action(action: &Action) -> BorgResult<()> {
    match action {
        Action::Init => todo!(),
        Action::Create { profile } => {
            let profile = Profile::try_open_profile_or_create_default(profile)?;
            info!("Creating backup for profile {}", profile);
            borg::create_backup(&profile)?;
            Ok(())
        }
        Action::Add { directory, profile } => {
            let mut profile = Profile::try_open_profile_or_create_default(profile)?;
            profile.add_backup_path(directory.clone())?;
            profile.save_profile()?;
            info!("Added {} to profile {}", directory, profile);
            Ok(())
        }
        Action::AddRepo {
            repository,
            profile,
        } => {
            let mut profile = Profile::try_open_profile_or_create_default(profile)?;
            profile.add_repository(repository.clone())?;
            profile.save_profile()?;
            info!("Added repository {} to profile {}", repository, profile);
            Ok(())
        }
    }
}

fn main() -> BorgResult<()> {
    let args = cli::get_args();
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .with_context(|| "setting default subscriber failed")?;

    println!("{:?}", args);
    let res = match args.action {
        Some(action) => handle_non_interactive_action(&action),
        None => todo!("handle interactive mode"),
    };

    if let Err(e) = res {
        error!("Error: {}", e);
        std::process::exit(1);
    }
    Ok(())
}

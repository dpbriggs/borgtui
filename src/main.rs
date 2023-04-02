use crate::cli::Action;
use crate::profiles::Profile;
use crate::types::BorgResult;
use anyhow::{bail, Context};
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;

mod borg;
mod cli;
mod profiles;
mod types;

fn try_get_initial_repo_password() -> BorgResult<Option<String>> {
    if atty::is(atty::Stream::Stdin) {
        rpassword::read_password()
            .with_context(|| "Failed to read password from tty")
            .map(|pass| if pass.is_empty() { None } else { Some(pass) })
    } else {
        bail!("Password must be provided via an interactive terminal!")
    }
}

fn handle_non_tui_action(action: &Action) -> BorgResult<()> {
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
            encryption_passphrase,
            store_passphase_in_cleartext,
        } => {
            let mut profile = Profile::try_open_profile_or_create_default(profile)?;
            let passphrase = match encryption_passphrase {
                Some(passphrase) => Some(passphrase.clone()),
                None => try_get_initial_repo_password()?,
            };
            profile.add_repository(
                repository.clone(),
                passphrase,
                *store_passphase_in_cleartext,
            )?;
            profile.save_profile()?;
            info!("Added repository {} to profile {}", repository, profile);
            Ok(())
        }
    }
}

fn main() -> BorgResult<()> {
    let args = cli::get_args();
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .with_context(|| "setting default subscriber failed")?;

    println!("{:?}", args);
    let res = match args.action {
        Some(action) => handle_non_tui_action(&action),
        None => todo!("handle interactive mode"),
    };

    if let Err(e) = res {
        error!("Error: {}", e);
        std::process::exit(1);
    }
    Ok(())
}

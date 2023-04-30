use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) action: Option<Action>,

    /// The profile to use. If not specified, the default profile
    /// will be used.
    #[arg(env, short = 'p', long = "profile")]
    pub(crate) borgtui_profile: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Action {
    /// Initialize a new borg repository and add it to a profile
    Init {
        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(env, short = 'e', long)]
        borg_passphrase: String,

        /// The repo location
        #[arg(short, long)]
        location: String,

        /// Do not store the passphrase in the keyring
        #[arg(short, long)]
        do_not_store_in_keyring: bool,
    },
    /// Create a new backup
    Create,
    /// Add a directory or file to backup
    Add {
        /// The directory or file path to add to backup
        directory: PathBuf,
    },
    /// Add a directory or file to backup
    Remove {
        /// The directory or file path to add to backup
        directory: PathBuf,
    },
    /// Add a directory or file to backup
    AddRepo {
        /// The directory or file path to add to backup
        repository: String,

        // TODO: Simplify these options!
        /// The encryption passphrase to use. If not specified and borgtui
        /// called in an interactive terminal, the user will be prompted.
        #[arg(short, long, default_value = "true")]
        no_encryption: bool,

        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(env, short, long)]
        borg_passphrase: Option<String>,

        /// If true, store the encryption passphrase in cleartext in the
        /// configuration file. This is not recommended.
        #[arg(short, long, default_value = "false")]
        store_passphase_in_cleartext: bool,
    },
    Mount {
        /// The directory or file path to add to backup
        repository_path: String,
        /// The mount point
        mountpoint: PathBuf,
    },
    Umount {
        /// The mount point
        mountpoint: PathBuf,
    },
    /// List the archives in a directory
    List,
    Compact,
    Prune,
}

pub(crate) fn get_args() -> Args {
    Args::parse()
}

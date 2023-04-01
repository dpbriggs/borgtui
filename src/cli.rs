use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) action: Option<Action>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Action {
    /// Initialize a new borg repository
    Init,
    /// Create a new backup
    Create {
        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(short, long)]
        profile: Option<String>,
    },
    /// Add a directory or file to backup
    Add {
        /// The directory or file path to add to backup
        directory: String, // TODO: Make this a PathBuf

        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(short, long)]
        profile: Option<String>,
    },
    /// Add a directory or file to backup
    AddRepo {
        /// The directory or file path to add to backup
        repository: String,

        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(short, long)]
        profile: Option<String>,
    },
}

pub(crate) fn get_args() -> Args {
    Args::parse()
}

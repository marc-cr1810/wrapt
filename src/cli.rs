use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "wrapt", version, about = "A faster, prettier front-end for apt")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Number of parallel downloads
    #[arg(short = 'j', long, global = true, default_value_t = 5)]
    pub parallel: usize,

    /// Show apt's raw output instead of the clean progress display
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Emit machine-readable JSON (supported by search, why, history, doctor)
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Refresh the package lists
    Update,
    /// Upgrade all installed packages
    Upgrade {
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
        /// Allow installing/removing packages to satisfy the upgrade (dist-upgrade)
        #[arg(long)]
        full: bool,
        /// Only apply upgrades from a security pocket
        #[arg(long)]
        security_only: bool,
    },
    /// Install packages
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove packages
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
        /// Also delete configuration files
        #[arg(long)]
        purge: bool,
    },
    /// Remove packages that are no longer needed
    Autoremove {
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Show the transaction history
    History {
        /// Show the details of one transaction
        id: Option<u64>,
    },
    /// Undo a past transaction (the most recent one by default)
    Undo {
        /// The transaction to undo (see `wrapt history`)
        id: Option<u64>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Re-apply a past transaction
    Redo {
        /// The transaction to redo (see `wrapt history`)
        id: u64,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Undo every transaction after the given one, restoring that state
    Rollback {
        /// Roll back to the state just after this transaction id
        id: u64,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Hold packages at their current version (exclude from upgrades)
    Hold { packages: Vec<String> },
    /// Release held packages
    Unhold { packages: Vec<String> },
    /// List held packages
    Held,
    /// Review configuration files left behind by upgrades (*.dpkg-dist)
    ConfigDiff,
    /// Generate a shell completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    /// Search for packages
    Search { query: String },
    /// Show detailed information about a package
    Show { package: String },
    /// Explain why a package is installed
    Why {
        package: String,
        /// List every manually-installed package that pulls it in
        #[arg(short, long)]
        all: bool,
    },
    /// Find which package provides a file or command
    Provides { pattern: String },
    /// Check the system for common package problems
    Doctor,
}

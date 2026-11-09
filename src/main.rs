//! winner: a small, daemonless CLI for Windows containers built directly on HCS.

mod backup;
mod hcs;
mod host;
mod image;
mod layer;
mod paths;
mod pull;
mod run;
mod shim;
mod spec;
mod state;
mod stdio;
mod table;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(version, about = "Run Windows containers without a daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create and run a container in the foreground
    Run {
        /// Remove the container after it exits
        #[arg(long)]
        remove: bool,
        /// Name the container, usable wherever an ID is accepted
        #[arg(long)]
        name: Option<String>,
        /// Override the image entrypoint (drops the image's default arguments)
        #[arg(long)]
        entrypoint: Option<String>,
        /// Isolation mode (default: chosen from host and image compatibility)
        #[arg(long, value_enum)]
        isolation: Option<Isolation>,
        /// Force a tty (default: auto-detect from stdin/stdout)
        #[arg(long, overrides_with = "no_tty")]
        tty: bool,
        /// Disable the tty even when attached to a terminal
        #[arg(long, overrides_with = "tty")]
        no_tty: bool,
        /// Image reference
        image: String,
        /// Command and arguments (default: the image's entrypoint and command)
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// List containers
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },
    /// Delete a container and its scratch layer
    #[command(visible_alias = "rm")]
    Delete {
        /// Container ID, unique ID prefix, or name
        container: String,
    },
    /// Manage images
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Internal: per-container supervisor for detached containers
    #[command(hide = true)]
    Shim { id: String },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Pull an image from a registry and import its layers
    Pull {
        /// Image reference, e.g. mcr.microsoft.com/windows/nanoserver:ltsc2025
        image: String,
    },
    /// List local images
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },
    /// Delete a local image record
    #[command(visible_alias = "rm")]
    Delete { image: String },
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    Process,
    // Hidden until utility VM support lands.
    #[value(hide = true)]
    Hyperv,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

/// Exit code for failures of winner itself under `run`, so they can be told
/// apart from the container process's own exit code.
const RUN_FAILURE: i32 = 125;

fn main() {
    let cli = Cli::parse();
    let failure = if matches!(cli.command, Command::Run { .. }) { RUN_FAILURE } else { 1 };
    match dispatch(cli.command) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(failure);
        }
    }
}

fn dispatch(command: Command) -> Result<i32> {
    match command {
        Command::Run {
            remove,
            name,
            entrypoint,
            isolation,
            tty,
            no_tty,
            image,
            command,
        } => {
            let tty = match (tty, no_tty) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let opts = run::Options { name, entrypoint, isolation, tty, remove };
            run::run(&image, &command, opts).map(|code| code as i32)
        }
        Command::List { format } => state::list(format).map(|()| 0),
        Command::Delete { container } => run::remove(&container).map(|()| 0),
        Command::Image { command } => match command {
            ImageCommand::Pull { image } => pull::pull(&image),
            ImageCommand::List { format } => image::list(format),
            ImageCommand::Delete { image } => image::delete(&image),
        }
        .map(|()| 0),
        Command::Shim { id } => shim::main(&id).map(|()| 0),
    }
}

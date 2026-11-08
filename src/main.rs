//! winner: a small, daemonless CLI for Windows containers built directly on HCS.

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
        rm: bool,
        /// Isolation mode
        #[arg(long, value_enum, default_value_t = Isolation::Process)]
        isolation: Isolation,
        /// Force a tty (default: auto-detect from stdin/stdout)
        #[arg(long, overrides_with = "no_tty")]
        tty: bool,
        /// Disable the tty even when attached to a terminal
        #[arg(long, overrides_with = "tty")]
        no_tty: bool,
        /// Image reference
        image: String,
        /// Command line to run inside the container
        #[arg(trailing_var_arg = true, default_values_t = ["cmd.exe".to_string()])]
        command: Vec<String>,
    },
    /// List containers
    #[command(visible_alias = "ls")]
    List,
    /// Delete a container and its scratch layer
    #[command(visible_alias = "rm")]
    Delete { id: String },
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
    List,
    /// Delete a local image record
    #[command(visible_alias = "rm")]
    Delete { image: String },
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum Isolation {
    Process,
    Hyperv,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            rm,
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
            let code = run::run(&image, &command, isolation, tty, rm)?;
            std::process::exit(code as i32);
        }
        Command::List => state::list(),
        Command::Delete { id } => run::remove(&id),
        Command::Image { command } => match command {
            ImageCommand::Pull { image } => pull::pull(&image),
            ImageCommand::List => image::list(),
            ImageCommand::Delete { image } => image::delete(&image),
        },
        Command::Shim { id } => shim::main(&id),
    }
}

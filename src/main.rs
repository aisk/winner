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
    /// Pull an image from a registry and import its layers
    Pull {
        /// Image reference, e.g. mcr.microsoft.com/windows/nanoserver:ltsc2025
        image: String,
    },
    /// List local images
    Images,
    /// Create and run a container in the foreground
    Run {
        /// Remove the container after it exits
        #[arg(long)]
        rm: bool,
        /// Isolation mode
        #[arg(long, value_enum, default_value_t = Isolation::Process)]
        isolation: Isolation,
        /// Image reference
        image: String,
        /// Command line to run inside the container
        #[arg(trailing_var_arg = true, default_values_t = ["cmd.exe".to_string()])]
        command: Vec<String>,
    },
    /// List containers
    Ps,
    /// Remove a container and its scratch layer
    Rm {
        id: String,
    },
    /// Internal: per-container supervisor for detached containers
    #[command(hide = true)]
    Shim {
        id: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum Isolation {
    Process,
    Hyperv,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Pull { image } => pull::pull(&image),
        Command::Images => image::list(),
        Command::Run {
            rm,
            isolation,
            image,
            command,
        } => {
            let code = run::run(&image, &command, isolation, rm)?;
            std::process::exit(code as i32);
        }
        Command::Ps => state::list(),
        Command::Rm { id } => run::remove(&id),
        Command::Shim { id } => shim::main(&id),
    }
}

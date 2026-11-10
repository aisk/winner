//! winner: a small, daemonless CLI for Windows containers built directly on HCS.

mod backup;
mod control;
mod hcn;
mod hcs;
mod host;
mod image;
mod layer;
mod lock;
mod network;
mod paths;
mod pull;
mod run;
mod shim;
mod spec;
mod state;
mod stdio;
mod system;
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
    /// Create and run a container (in the foreground unless --detach)
    Run {
        /// Run in the background and print the container ID
        #[arg(short, long, conflicts_with = "tty")]
        detach: bool,
        /// Force a tty (default: auto-detect from stdin/stdout)
        #[arg(long, overrides_with = "no_tty")]
        tty: bool,
        /// Disable the tty even when attached to a terminal
        #[arg(long, overrides_with = "tty")]
        no_tty: bool,
        #[command(flatten)]
        container: ContainerArgs,
    },
    /// Create a container without starting it
    Create {
        #[command(flatten)]
        container: ContainerArgs,
    },
    /// Start created or exited containers in the background
    Start {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Stop running containers, forcing them after a timeout
    Stop {
        /// Seconds to wait for a clean shutdown before terminating
        #[arg(short, long, default_value_t = 10)]
        timeout: u64,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Terminate running containers immediately
    Kill {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Run a command in a running container
    Exec {
        /// Force a tty (default: auto-detect from stdin/stdout)
        #[arg(long, overrides_with = "no_tty")]
        tty: bool,
        /// Disable the tty even when attached to a terminal
        #[arg(long, overrides_with = "tty")]
        no_tty: bool,
        /// Set an environment variable (KEY=VALUE, or KEY to copy it from the host)
        #[arg(short, long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Working directory inside the container
        #[arg(short, long)]
        workdir: Option<String>,
        /// Account to run as, e.g. ContainerAdministrator
        #[arg(short, long)]
        user: Option<String>,
        /// Container ID, unique ID prefix, or name
        container: String,
        /// Command and arguments
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Print the output of a container started in the background
    Logs {
        /// Keep printing new output until the container stops
        #[arg(short, long)]
        follow: bool,
        container: String,
    },
    /// Show container details as JSON
    Inspect {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// List containers
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },
    /// Delete containers and their scratch layers
    #[command(visible_alias = "rm")]
    Delete {
        /// Terminate running containers instead of refusing to delete them
        #[arg(short, long)]
        force: bool,
        /// Container IDs, unique ID prefixes, or names
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Manage images
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Manage networks
    Network {
        #[command(subcommand)]
        command: NetworkCommand,
    },
    /// Manage winner's data as a whole
    System {
        #[command(subcommand)]
        command: SystemCommand,
    },
    /// Internal: per-container supervisor for detached containers
    #[command(hide = true)]
    Shim { id: String },
}

fn tty_override(tty: bool, no_tty: bool) -> Option<bool> {
    match (tty, no_tty) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

/// Options shared by `run` and `create`.
#[derive(clap::Args)]
struct ContainerArgs {
    /// Name the container, usable wherever an ID is accepted
    #[arg(long)]
    name: Option<String>,
    /// Set an environment variable (KEY=VALUE, or KEY to copy it from the host)
    #[arg(short, long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Working directory inside the container (default: from the image)
    #[arg(short, long)]
    workdir: Option<String>,
    /// Account to run as (default: from the image), e.g. ContainerAdministrator
    #[arg(short, long)]
    user: Option<String>,
    /// Map a host directory into the container
    #[arg(short, long = "volume", value_name = "HOST:CONTAINER[:ro]")]
    volumes: Vec<String>,
    /// Network to connect to, "host" to share the host's network, or "none" (default: the "winner" NAT network, created on demand)
    #[arg(long)]
    network: Option<String>,
    /// Publish a container port on the host
    #[arg(short, long = "publish", value_name = "HOST:CONTAINER[/udp]")]
    publish: Vec<String>,
    /// Override the image entrypoint (drops the image's default arguments)
    #[arg(long)]
    entrypoint: Option<String>,
    /// When to pull the image
    #[arg(long, value_enum, default_value_t = Pull::Missing)]
    pull: Pull,
    /// Isolation mode (default: chosen from host and image compatibility)
    #[arg(long, value_enum)]
    isolation: Option<Isolation>,
    /// Delete the container after it exits
    #[arg(long)]
    remove: bool,
    /// Image reference
    image: String,
    /// Command and arguments (default: the image's entrypoint and command)
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

impl ContainerArgs {
    fn split(self) -> (String, Vec<String>, run::Options) {
        let opts = run::Options {
            name: self.name,
            entrypoint: self.entrypoint,
            pull: self.pull,
            isolation: self.isolation,
            env: self.env,
            workdir: self.workdir,
            user: self.user,
            volumes: self.volumes,
            network: self.network,
            publish: self.publish,
            remove: self.remove,
        };
        (self.image, self.command, opts)
    }
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
    /// Show image details (manifest digest, layers, config) as JSON
    Inspect {
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Delete local images and the layers no longer used by anything
    #[command(visible_alias = "rm")]
    Delete {
        /// Delete even if containers were created from the image (their layers are kept)
        #[arg(short, long)]
        force: bool,
        #[arg(required = true)]
        images: Vec<String>,
    },
}

#[derive(Subcommand)]
enum NetworkCommand {
    /// Create a NAT network
    Create {
        /// IPv4 subnet, e.g. 172.30.0.0/16 (default: picked by the system)
        #[arg(long)]
        subnet: Option<String>,
        /// Gateway address (default: the first address of the subnet)
        #[arg(long, requires = "subnet")]
        gateway: Option<String>,
        name: String,
    },
    /// List networks, including those created by other tools
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },
    /// Show network details as JSON
    Inspect {
        #[arg(required = true)]
        networks: Vec<String>,
    },
    /// Delete networks created by winner
    #[command(visible_alias = "rm")]
    Delete {
        #[arg(required = true)]
        networks: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SystemCommand {
    /// Delete stopped containers, unused networks and layers, and leftovers
    Prune {
        /// Also delete images no container uses
        #[arg(short, long)]
        all: bool,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },
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
pub enum Pull {
    /// Pull only if the image is not available locally
    Missing,
    /// Always pull, picking up a newer image under the same tag
    Always,
    /// Never pull; fail if the image is not available locally
    Never,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

/// Exit code for failures of winner itself under `run` and `exec`, so they
/// can be told apart from the container process's own exit code.
const RUN_FAILURE: i32 = 125;

fn main() {
    let cli = Cli::parse();
    let failure = if matches!(cli.command, Command::Run { .. } | Command::Exec { .. }) { RUN_FAILURE } else { 1 };
    match dispatch(cli.command) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(failure);
        }
    }
}

fn dispatch(command: Command) -> Result<i32> {
    let done = |r: Result<()>| r.map(|()| 0);
    match command {
        Command::Run { detach, tty, no_tty, container } => {
            let (image, command, opts) = container.split();
            run::run(&image, &command, opts, detach, tty_override(tty, no_tty)).map(|code| code as i32)
        }
        Command::Create { container } => {
            let (image, command, opts) = container.split();
            done(run::create(&image, &command, opts).map(|(s, _lock)| println!("{}", s.id)))
        }
        Command::Start { containers } => done(each(&containers, run::start)),
        Command::Stop { timeout, containers } => {
            done(each(&containers, |c| control::stop(c, std::time::Duration::from_secs(timeout))))
        }
        Command::Kill { containers } => done(each(&containers, control::kill)),
        Command::Exec { tty, no_tty, env, workdir, user, container, command } => {
            let opts = control::ExecOptions { tty: tty_override(tty, no_tty), env, workdir, user };
            control::exec(&container, &command, opts).map(|code| code as i32)
        }
        Command::Logs { follow, container } => done(control::logs(&container, follow)),
        Command::Inspect { containers } => done(control::inspect(&containers)),
        Command::List { format } => done(state::list(format)),
        Command::Delete { force, containers } => done(each(&containers, |c| control::remove(c, force))),
        Command::Image { command } => done(match command {
            ImageCommand::Pull { image } => pull::pull(&image).map(|r| println!("{}: {}", r.reference, r.manifest_digest)),
            ImageCommand::List { format } => image::list(format),
            ImageCommand::Inspect { images } => image::inspect(&images),
            ImageCommand::Delete { force, images } => {
                let deleted = each(&images, |i| image::delete(i, force));
                image::collect_garbage();
                deleted
            }
        }),
        Command::Network { command } => done(match command {
            NetworkCommand::Create { subnet, gateway, name } => {
                network::create(&name, subnet.as_deref(), gateway.as_deref()).map(|_| println!("{name}"))
            }
            NetworkCommand::List { format } => network::list(format),
            NetworkCommand::Inspect { networks } => network::inspect(&networks),
            NetworkCommand::Delete { networks } => {
                network::sweep();
                each(&networks, |n| network::delete(n).map(|()| println!("{n}")))
            }
        }),
        Command::System { command } => done(match command {
            SystemCommand::Prune { all, force } => system::prune(all, force),
        }),
        Command::Shim { id } => done(shim::main(&id)),
    }
}

/// Apply `f` to every argument, reporting failures one by one so a bad
/// argument does not stop the rest.
fn each(items: &[String], f: impl Fn(&str) -> Result<()>) -> Result<()> {
    if let [item] = items {
        return f(item);
    }
    let failed = items.iter().filter(|i| f(i).inspect_err(|e| eprintln!("error: {e:#}")).is_err()).count();
    if failed > 0 {
        anyhow::bail!("{failed} of {} failed", items.len());
    }
    Ok(())
}

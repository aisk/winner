//! Container lifecycle commands: `create`, `run` and `start`, and deleting a
//! container's resources. Supervising a run is [`crate::supervisor`]'s job.

use crate::hcs::System;
use crate::image::ImageRecord;
use crate::process::{merge_env, quote_arg};
use crate::spec::MappedDirectory;
use crate::state::{self, ContainerState, Isolation, ProcessConfig, Status};
use crate::supervisor::{self, Io};
use crate::{host, image, layer, network, paths, pull, stdio};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// When `create` / `run` pull the image.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Pull {
    /// Pull only if the image is not available locally
    Missing,
    /// Always pull, picking up a newer image under the same tag
    Always,
    /// Never pull; fail if the image is not available locally
    Never,
}

/// Options shared by `create` and `run`.
pub struct Options {
    pub name: Option<String>,
    pub entrypoint: Option<String>,
    pub pull: Pull,
    /// `None` picks process isolation when the image build is compatible with the host.
    pub isolation: Option<Isolation>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub volumes: Vec<String>,
    /// Network name, `none`, or `None` for the default network.
    pub network: Option<String>,
    pub publish: Vec<String>,
    pub remove: bool,
}

/// Record a new container and create its scratch layer. Nothing runs yet.
/// Returns the container's lock, so nothing deletes it before the caller
/// starts it (or lets go).
pub fn create(image_ref: &str, args: &[String], opts: Options) -> Result<(ContainerState, std::fs::File)> {
    let image = match (opts.pull, image::find(image_ref)?) {
        (Pull::Always, Some(local)) => pull::pull(&local.reference)?,
        (Pull::Always | Pull::Missing, None) => pull::pull(image_ref)?,
        (Pull::Never, None) => bail!("image {image_ref} not found locally"),
        (_, Some(local)) => local,
    };
    let layers: Vec<PathBuf> = image.layers.iter().map(|d| paths::layer(d)).collect();
    let isolation = resolve_isolation(&image, &layers, opts.isolation)?;
    let (command, command_line) = resolve_command(&image, opts.entrypoint.as_deref(), args)?;
    if let Some(name) = &opts.name {
        state::validate_name(name)?;
    }
    let mounts = opts.volumes.iter().map(|v| parse_volume(v)).collect::<Result<_>>()?;
    let ports: Vec<_> = opts.publish.iter().map(|p| network::parse_publish(p)).collect::<Result<_>>()?;
    let network = network::resolve(opts.network.as_deref(), &ports, isolation)?;

    let state = ContainerState {
        id: state::new_id()?,
        name: opts.name,
        image: image.reference.clone(),
        command,
        isolation,
        status: Status::Created,
        exit_code: None,
        error: None,
        created: state::now(),
        layers,
        process: ProcessConfig {
            command_line,
            environment: merge_env(image.env().collect(), &opts.env)?,
            working_directory: opts.workdir.or_else(|| image.working_dir()),
            user: opts.user.or_else(|| image.user()),
        },
        mounts,
        network,
        ports,
        remove: opts.remove,
        started: false,
        pid: None,
    };
    std::fs::create_dir_all(state.dir())?;
    let lock = state.lock_supervisor(Duration::ZERO)?;
    if let Err(e) = state.save().and_then(|()| layer::create_scratch(&state.scratch(), &state.layers)) {
        drop(lock);
        let _ = std::fs::remove_dir_all(state.dir());
        return Err(e);
    }
    Ok((state, lock))
}

/// `run`: create, then either supervise in the foreground (returning the
/// container's exit code) or start in the background and print the ID.
pub fn run(image_ref: &str, args: &[String], opts: Options, detach: bool, tty: Option<bool>) -> Result<u32> {
    let (mut state, lock) = create(image_ref, args, opts)?;
    if detach {
        drop(lock); // the shim takes it
        start_detached(&mut state)?;
        println!("{}", state.id);
        return Ok(0);
    }
    let tty = tty.unwrap_or_else(stdio::detect_tty);
    let result = supervisor::execute(&mut state, Io::Console { tty });
    drop(lock);
    if state.remove
        && let Err(e) = cleanup(&state)
    {
        eprintln!("warning: cleanup failed: {e:#}");
    }
    result
}

/// `start`: run an existing created or exited container in the background.
pub fn start(query: &str) -> Result<()> {
    let state = state::resolve(query)?;
    if state.is_running() {
        bail!("container {} is already running", state.id);
    }
    // Let a supervisor still cleaning up after the last exit finish. The shim
    // takes the lock itself, so only wait for it here.
    drop(state::lock(&state.id, crate::control::EXIT_CLEANUP)?);
    // Reload, reconciled: a killed supervisor leaves `running` on disk.
    let mut state = state::resolve(&state.id)?;
    if state.is_running() {
        bail!("container {} is already running", state.id);
    }
    start_detached(&mut state)?;
    println!("{}", state.id);
    Ok(())
}

/// Hand the container to a detached `winner shim` and wait until it is running.
fn start_detached(state: &mut ContainerState) -> Result<()> {
    state.status = Status::Created;
    state.exit_code = None;
    state.error = None;
    state.save()?;

    let mut shim = spawn_shim(&state.id)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let current = match ContainerState::load(&state.id) {
            Ok(c) => c,
            // Ran to completion and was deleted (`--remove`) before we looked.
            Err(_) if !state.dir().exists() => {
                state.status = Status::Exited;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if let Some(error) = current.error {
            bail!("start container {}: {error}", state.id);
        }
        if !matches!(current.status, Status::Created) {
            *state = current;
            return Ok(());
        }
        if let Some(code) = shim.try_wait()? {
            bail!("start container {}: supervisor exited unexpectedly ({code})", state.id);
        }
        if Instant::now() > deadline {
            bail!("start container {}: timed out waiting for it to run", state.id);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_shim(id: &str) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    // CreateProcess runs with handle inheritance on, so the shim would also
    // inherit our own stdio if it is inheritable, e.g. a pipe from a caller
    // capturing `run -d` output, which would then wait for the container to exit.
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if let Ok(h) = unsafe { GetStdHandle(which) } {
            let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
        }
    }

    let exe = std::env::current_exe()?;
    let spawn = |flags: u32| {
        Command::new(&exe)
            .args(["shim", id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    // Leave any job the CLI runs in (terminals and sudo may kill the job when
    // the CLI exits); fall back if the job does not allow breaking away.
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    spawn(flags | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(flags))
        .context("spawn container supervisor")
}

/// `layers` are the image's layer directories, base first.
fn resolve_isolation(image: &ImageRecord, layers: &[PathBuf], requested: Option<Isolation>) -> Result<Isolation> {
    let host = host::build();
    let image_build = image.os_version.as_deref().and_then(host::build_of);
    if let Some(b) = image_build
        && b > host
    {
        bail!("image build {b} is newer than host build {host}: Windows cannot run it with any isolation");
    }
    let isolation = requested.unwrap_or(match image_build {
        Some(b) if !host::process_compatible(host, b) => Isolation::Hyperv,
        _ => Isolation::Process,
    });
    match (isolation, image_build) {
        (Isolation::Process, Some(b)) if !host::process_compatible(host, b) => {
            bail!("image build {b} cannot run with process isolation on host build {host}; use --isolation hyperv")
        }
        (Isolation::Hyperv, _) => {
            layer::utility_vm(layers)?;
        }
        _ => {}
    }
    Ok(isolation)
}

/// Resolve the process to run following the OCI rules: `entrypoint` replaces
/// the image Entrypoint and drops its Cmd; non-empty `args` replace Cmd.
/// Returns the argument list for display and the command line for HCS.
fn resolve_command(image: &ImageRecord, entrypoint: Option<&str>, args: &[String]) -> Result<(Vec<String>, String)> {
    // (argument, comes from the image) pairs
    let mut parts: Vec<(String, bool)> = match entrypoint {
        Some(e) => vec![(e.to_string(), false)],
        None => image.entrypoint().into_iter().map(|a| (a, true)).collect(),
    };
    if !args.is_empty() {
        parts.extend(args.iter().map(|a| (a.clone(), false)));
    } else if entrypoint.is_none() {
        parts.extend(image.cmd().into_iter().map(|a| (a, true)));
    }
    if parts.is_empty() {
        bail!("no command given and image {} has no default command", image.reference);
    }
    let verbatim = image.args_escaped();
    let command_line = parts
        .iter()
        .map(|(a, from_image)| if *from_image && verbatim { a.clone() } else { quote_arg(a) })
        .collect::<Vec<_>>()
        .join(" ");
    Ok((parts.into_iter().map(|(a, _)| a).collect(), command_line))
}

/// Parse `HOST:CONTAINER[:ro]`, where both sides are Windows paths that may
/// carry a drive letter (`C:\data:C:\app\data:ro`).
fn parse_volume(spec: &str) -> Result<MappedDirectory> {
    // Split on ':' and glue a drive letter back onto the path that follows it.
    let mut parts: Vec<String> = Vec::new();
    let mut pending_drive: Option<&str> = None;
    for piece in spec.split(':') {
        if let Some(drive) = pending_drive.take() {
            parts.push(format!("{drive}:{piece}"));
        } else if piece.len() == 1 && piece.as_bytes()[0].is_ascii_alphabetic() {
            pending_drive = Some(piece);
        } else {
            parts.push(piece.to_string());
        }
    }
    if let Some(drive) = pending_drive {
        parts.push(format!("{drive}:"));
    }
    let (host, container, read_only) = match parts.as_slice() {
        [h, c] => (h, c, false),
        [h, c, mode] if mode == "ro" => (h, c, true),
        [h, c, mode] if mode == "rw" => (h, c, false),
        _ => bail!("invalid --volume {spec:?}: expected HOST:CONTAINER[:ro]"),
    };
    let container_ok = container.len() >= 3 && container.as_bytes()[0].is_ascii_alphabetic() && container[1..].starts_with(":\\");
    if !container_ok {
        bail!("invalid --volume {spec:?}: container path must be absolute, like C:\\data");
    }
    let host = std::path::absolute(host).with_context(|| format!("invalid --volume {spec:?}"))?;
    if !host.is_dir() {
        bail!("invalid --volume {spec:?}: {} is not a directory", host.display());
    }
    Ok(MappedDirectory {
        host_path: host.to_string_lossy().into_owned(),
        container_path: container.clone(),
        read_only,
    })
}

/// Terminate whatever is left, then delete the scratch layer and the state.
pub fn cleanup(state: &ContainerState) -> Result<()> {
    if let Ok(system) = System::open(&state.id) {
        let _ = system.terminate();
    }
    if state.network.as_deref().is_some_and(|n| n != network::HOST) {
        // A failure leaves objects that `network::sweep` finds later.
        if let Err(e) = network::release(&state.id) {
            eprintln!("warning: release network: {e:#}");
        }
    }
    let scratch = state.scratch();
    if scratch.exists() {
        let _ = layer::unmount(&scratch); // may already be unmounted
        layer::destroy(&scratch)?;
    }
    std::fs::remove_dir_all(state.dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_volume;

    #[test]
    fn volumes() {
        let tmp = std::env::temp_dir();
        let host = tmp.to_string_lossy().trim_end_matches('\\').to_string();
        let v = parse_volume(&format!(r"{host}:C:\data:ro")).unwrap();
        assert_eq!(v.container_path, r"C:\data");
        assert!(v.read_only);
        let v = parse_volume(&format!(r"{host}:D:\x")).unwrap();
        assert_eq!(v.container_path, r"D:\x");
        assert!(!v.read_only);
        assert!(parse_volume(&format!(r"{host}:data")).is_err());
        assert!(parse_volume(&format!(r"{host}:C:\data:bogus")).is_err());
        assert!(parse_volume(r"C:\definitely\missing\dir:C:\data").is_err());
    }
}

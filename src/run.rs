//! Creating and running containers.
//!
//! `execute` is the supervisor for one run of a container: mount the scratch
//! layer, create and start the compute system, run the process, wait, record
//! the exit. It runs in the foreground `run` process with the console
//! attached, or in a detached `winner shim` process writing to the log.

use crate::hcs::System;
use crate::image::ImageRecord;
use crate::spec::{self, ComputeSystem, Container, MappedDirectory, Networking, ProcessParameters, Storage};
use crate::state::{self, ContainerState, ProcessConfig, Status};
use crate::{Isolation, Pull, host, image, layer, network, paths, pull, stdio};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

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

/// Where a container's stdio goes.
pub enum Io {
    /// Attached to this process's console; `tty` selects ConPTY.
    Console { tty: bool },
    /// Detached: stdout and stderr appended to the container log, no stdin.
    Log,
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
    let isolation = resolve_isolation(&image, opts.isolation)?;
    let (command, command_line) = resolve_command(&image, opts.entrypoint.as_deref(), args)?;
    if let Some(name) = &opts.name {
        state::validate_name(name)?;
    }
    let mounts = opts.volumes.iter().map(|v| parse_volume(v)).collect::<Result<_>>()?;
    let ports: Vec<_> = opts.publish.iter().map(|p| network::parse_publish(p)).collect::<Result<_>>()?;
    let network = network::resolve(opts.network.as_deref(), &ports)?;

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
        layers: image.layers.iter().map(|d| paths::layer(d)).collect(),
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
    let result = execute(&mut state, Io::Console { tty });
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
    let mut state = state::resolve(query)?;
    if state.is_running() || state::supervised(&state.id) {
        bail!("container {} is already running, or still stopping", state.id);
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

/// Unmounts the scratch layer on every exit path of `execute`.
struct Mounted<'a>(&'a std::path::Path);

impl Drop for Mounted<'_> {
    fn drop(&mut self) {
        if let Err(e) = layer::unmount(self.0) {
            eprintln!("warning: unmount scratch layer: {e:#}");
        }
    }
}

/// Terminates the compute system on every exit path of `execute`, before
/// the network and scratch layer it uses are released.
struct Terminating(System);

impl Drop for Terminating {
    fn drop(&mut self) {
        let _ = self.0.terminate(); // fails harmlessly if already stopped
    }
}

/// Supervise one run of the container until its process exits. The caller
/// holds the supervisor lock ([`ContainerState::lock_supervisor`]).
pub fn execute(state: &mut ContainerState, io: Io) -> Result<u32> {
    if matches!(state.isolation, Isolation::Hyperv) {
        bail!("hyper-v isolation is not implemented yet");
    }
    let scratch = state.scratch();
    // Left mounted by a supervisor that was killed; we hold the lock, so no
    // live run uses it.
    let _ = layer::unmount(&scratch);
    let volume = layer::mount(&scratch, &state.layers)?;
    let _mounted = Mounted(&scratch);
    let network = network::attach(state)?;

    let config = ComputeSystem {
        owner: "winner".into(),
        schema_version: spec::SCHEMA_V2_1,
        // The supervisor holds the only handle: if it dies, so does the container.
        should_terminate_on_last_handle_closed: true,
        container: Some(Container {
            storage: Storage { layers: layer::chain(&state.layers)?, path: volume },
            mapped_directories: state.mounts.clone(),
            networking: network.as_ref().map(|n| Networking { namespace: n.namespace.clone() }),
        }),
    };
    let system = Terminating(System::create(&state.id, &config)?);
    let system = &system.0;
    let _interruptible = matches!(io, Io::Console { .. }).then(|| Interruptible::new(&state.id));
    system.start()?;

    let (tty, stdin) = match io {
        Io::Console { tty } => (tty, true),
        Io::Log => (false, false),
    };
    let (process, pipes) = system.create_process(&process_parameters(&state.process, tty, stdin))?;
    // Only now is it running: `start` treats this as success, so a process
    // that fails to launch must surface as an error instead.
    state.status = Status::Running;
    state.save()?;

    let code = match io {
        Io::Console { tty } => {
            let code = forward_console(process, pipes, tty);
            // Terminated by Ctrl+C: whatever HCS reports for the killed
            // process (0xC0000001), report it the way Windows does.
            if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) { STATUS_CONTROL_C_EXIT } else { code? }
        }
        Io::Log => {
            let log = std::fs::OpenOptions::new().create(true).append(true).open(state.log())?;
            let outputs = stdio::forward_to_log(pipes, log);
            let code = process.wait()?;
            for t in outputs {
                let _ = t.join();
            }
            code
        }
    };

    state.status = Status::Exited;
    state.exit_code = Some(code);
    state.save()?;
    Ok(code)
}

/// Exit code of a process ended by Ctrl+C.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// The container this process runs in the foreground, for the console
/// control handler, and whether that handler terminated it.
static FOREGROUND: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// While alive, Ctrl+C / Ctrl+Break / closing the console terminate the
/// foreground container instead of killing winner, so `execute` still
/// records the exit and releases the network and scratch layer (and `--remove`
/// still deletes the container). With a tty, Ctrl+C goes to the container as
/// input instead and only Ctrl+Break gets here.
struct Interruptible;

impl Interruptible {
    fn new(id: &str) -> Self {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        *FOREGROUND.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
        let _ = unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true) };
        Self
    }
}

impl Drop for Interruptible {
    fn drop(&mut self) {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        let _ = unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), false) };
        *FOREGROUND.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

unsafe extern "system" fn on_console_ctrl(event: u32) -> windows::core::BOOL {
    const CTRL_CLOSE_EVENT: u32 = 2;
    let id = FOREGROUND.lock().ok().and_then(|g| g.clone());
    let Some(id) = id else { return false.into() };
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Ok(system) = System::open(&id) {
        let _ = system.terminate();
    }
    if event == CTRL_CLOSE_EVENT {
        // Windows ends the process when the handler returns; give the main
        // thread a moment to clean up.
        std::thread::sleep(Duration::from_secs(4));
    }
    true.into()
}

pub fn process_parameters(process: &ProcessConfig, tty: bool, stdin: bool) -> ProcessParameters {
    ProcessParameters {
        command_line: process.command_line.clone(),
        working_directory: process.working_directory.clone(),
        environment: process.environment.clone(),
        user: process.user.clone(),
        emulate_console: tty,
        console_size: if tty { stdio::RawConsole::size() } else { None },
        create_std_in_pipe: stdin,
        create_std_out_pipe: true,
        create_std_err_pipe: !tty, // ConPTY merges stderr into stdout
    }
}

/// Run `process` in `system` with this process's console attached; returns its exit code.
pub fn attach(system: &System, process: &ProcessConfig, tty: bool) -> Result<u32> {
    let (process, pipes) = system.create_process(&process_parameters(process, tty, true))?;
    forward_console(process, pipes, tty)
}

fn forward_console(process: crate::hcs::Process, pipes: crate::hcs::Stdio, tty: bool) -> Result<u32> {
    let process = std::sync::Arc::new(process);

    let _raw = tty.then(stdio::RawConsole::enable);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resizer = tty.then(|| watch_console_size(process.clone(), done.clone()));
    let outputs = stdio::forward(pipes, {
        let process = process.clone();
        move || {
            // Fails harmlessly if the process already exited.
            let _ = process.close_stdin();
        }
    });
    let code = process.wait();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    for t in outputs.into_iter().chain(resizer) {
        let _ = t.join();
    }
    code
}

/// Forward host console size changes to the container's ConPTY. Polled: the
/// stdin thread consumes the console input, including resize events.
fn watch_console_size(
    process: std::sync::Arc<crate::hcs::Process>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last = stdio::RawConsole::size();
        while !done.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
            let size = stdio::RawConsole::size();
            if let Some(s) = size.filter(|_| size != last) {
                // Fails harmlessly if the process just exited.
                let _ = process.resize_console(s);
                last = size;
            }
        }
    })
}

fn resolve_isolation(image: &ImageRecord, requested: Option<Isolation>) -> Result<Isolation> {
    let host = host::build();
    let image_build = image.os_version.as_deref().and_then(host::build_of);
    let isolation = requested.unwrap_or(match image_build {
        Some(b) if !host::process_compatible(host, b) => Isolation::Hyperv,
        _ => Isolation::Process,
    });
    if matches!(isolation, Isolation::Hyperv) {
        // TODO: utility VM (HostingSystemId + VirtualMachine schema).
        match (requested, image_build) {
            (None, Some(b)) => bail!(
                "image build {b} cannot run with process isolation on host build {host}, and hyper-v isolation is not implemented yet"
            ),
            _ => bail!("hyper-v isolation is not implemented yet"),
        }
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

/// Apply `--env` options on top of the image environment. `KEY=VALUE` sets a
/// variable; a bare `KEY` copies it from the host, or is ignored if unset there.
pub fn merge_env(mut env: BTreeMap<String, String>, overrides: &[String]) -> Result<BTreeMap<String, String>> {
    for item in overrides {
        match item.split_once('=') {
            Some(("", _)) => bail!("invalid --env {item:?}: empty name"),
            Some((k, v)) => {
                // Windows variable names are case-insensitive.
                env.retain(|existing, _| !existing.eq_ignore_ascii_case(k));
                env.insert(k.to_string(), v.to_string());
            }
            None => {
                if let Ok(v) = std::env::var(item) {
                    env.retain(|existing, _| !existing.eq_ignore_ascii_case(item));
                    env.insert(item.clone(), v);
                }
            }
        }
    }
    Ok(env)
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

/// Quote an argument per the MSVCRT rules so the container's
/// CommandLineToArgvW round-trips it.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
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
    use super::{merge_env, parse_volume, quote_arg};

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("dir"), "dir");
        assert_eq!(quote_arg("a b"), r#""a b""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }

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

    #[test]
    fn env() {
        let base = [("Path".to_string(), "x".to_string())].into_iter().collect();
        let env = merge_env(base, &["PATH=y".into(), "A=1=2".into()]).unwrap();
        assert_eq!(env.get("PATH").map(String::as_str), Some("y"));
        assert!(!env.contains_key("Path"));
        assert_eq!(env.get("A").map(String::as_str), Some("1=2"));
        assert!(merge_env(Default::default(), &["=x".into()]).is_err());
    }
}

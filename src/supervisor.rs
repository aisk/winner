//! The supervisor of one run of a container ([`execute`]): mount the scratch
//! layer (process isolation only), connect the network, create and start the
//! compute system, run the main process, wait, record the exit, and release
//! all of it again. It runs in the foreground `run` process with the console
//! attached, or in a detached `winner shim` process writing to the log.

use crate::hcs::System;
use crate::process::{forward_console, process_parameters};
use crate::spec::{self, ComputeSystem, Container, ContainerV1, HvRuntime, Networking, Storage};
use crate::state::{ContainerState, Status};
use crate::{Isolation, layer, network, stdio};
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

/// Where a container's stdio goes.
pub enum Io {
    /// Attached to this process's console; `tty` selects ConPTY.
    Console { tty: bool },
    /// Detached: stdout and stderr appended to the container log, no stdin.
    Log,
}

/// Unmounts the scratch layer on every exit path of `execute`.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Err(e) = layer::unmount(&self.0) {
            eprintln!("warning: unmount scratch layer: {e:#}");
        }
    }
}

/// Stops the compute system on every exit path of `execute`, before the
/// network and scratch layer it uses are released. Shuts it down first, as
/// Docker does: terminating a utility VM is pulling its power, and a scratch
/// layer cut off mid-write may not boot again.
struct Terminating(System);

impl Drop for Terminating {
    fn drop(&mut self) {
        // Both fail harmlessly if it already stopped.
        if self.0.shutdown(Duration::from_secs(30)).is_err() {
            let _ = self.0.terminate();
        }
    }
}

/// Supervise one run of the container until its process exits. The caller
/// holds the supervisor lock ([`ContainerState::lock_supervisor`]).
pub fn execute(state: &mut ContainerState, io: Io) -> Result<u32> {
    // Bindings drop last first: compute system, then network, then mount.
    let (_mounted, _network, system) = match state.isolation {
        Isolation::Process => {
            let scratch = state.scratch();
            // Left mounted by a supervisor that was killed; we hold the lock,
            // so no live run uses it.
            let _ = layer::unmount(&scratch);
            let volume = layer::mount(&scratch, &state.layers)?;
            let mounted = Mounted(scratch);
            let network = network::attach(state)?;
            let config = ComputeSystem {
                owner: "winner".into(),
                schema_version: spec::SCHEMA_V2_1,
                // The supervisor holds the only handle: if it dies, so does the container.
                should_terminate_on_last_handle_closed: true,
                container: Some(Container {
                    storage: Storage { layers: layer::chain(&state.layers)?, path: volume },
                    mapped_directories: state.mounts.clone(),
                    networking: network
                        .as_ref()
                        .and_then(|n| n.namespace.clone())
                        .map(|namespace| Networking { namespace }),
                }),
            };
            (Some(mounted), network, Terminating(System::create(&state.id, &config)?))
        }
        Isolation::Hyperv => {
            // vmcompute attaches the scratch layer to the utility VM itself.
            let network = network::attach(state)?;
            let config = ContainerV1 {
                system_type: "Container",
                name: state.id.clone(),
                owner: "winner".into(),
                // Safe only while the scratch layer is still empty, as
                // Docker does. Cuts about 0.2s off the first boot.
                ignore_flushes_during_boot: !state.started,
                layer_folder_path: state.scratch().to_string_lossy().into_owned(),
                layers: layer::chain(&state.layers)?.into_iter().map(Into::into).collect(),
                hv_partition: true,
                hv_runtime: HvRuntime { image_path: layer::utility_vm(&state.layers)?.to_string_lossy().into_owned() },
                endpoint_list: network.as_ref().and_then(|n| n.endpoint.clone()).into_iter().collect(),
                mapped_directories: state.mounts.clone(),
                terminate_on_last_handle_closed: true,
            };
            (None, network, Terminating(System::create(&state.id, &config)?))
        }
    };
    let system = &system.0;
    let _interruptible = matches!(io, Io::Console { .. }).then(|| Interruptible::new(&state.id));
    system.start()?;
    if !state.started {
        state.started = true;
        state.save()?;
    }

    let (tty, stdin) = match io {
        Io::Console { tty } => (tty, true),
        Io::Log => (false, false),
    };
    let (process, pipes) = system.create_process(&process_parameters(&state.process, tty, stdin))?;
    FOREGROUND_PID.store(pipes.pid, std::sync::atomic::Ordering::Relaxed);
    // Only now is it running: `start` treats this as success, so a process
    // that fails to launch must surface as an error instead.
    state.status = Status::Running;
    state.pid = Some(pipes.pid);
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
    state.pid = None;
    state.save()?;
    Ok(code)
}

/// Exit code of a process ended by Ctrl+C.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// The container this process runs in the foreground, for the console
/// control handler, and whether that handler terminated it.
static FOREGROUND: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Its main process, once started (0 before).
static FOREGROUND_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
        FOREGROUND_PID.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

unsafe extern "system" fn on_console_ctrl(event: u32) -> windows::core::BOOL {
    const CTRL_CLOSE_EVENT: u32 = 2;
    let id = FOREGROUND.lock().ok().and_then(|g| g.clone());
    let Some(id) = id else { return false.into() };
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let pid = FOREGROUND_PID.load(std::sync::atomic::Ordering::Relaxed);
    let _ = crate::control::end(&id, (pid != 0).then_some(pid));
    if event == CTRL_CLOSE_EVENT {
        // Windows ends the process when the handler returns; give the main
        // thread a moment to clean up.
        std::thread::sleep(Duration::from_secs(4));
    }
    true.into()
}

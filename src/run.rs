//! Foreground container lifecycle: create -> start -> exec -> wait -> cleanup.

use crate::hcs::System;
use crate::image::ImageRecord;
use crate::spec::{self, ComputeSystem, Container, ProcessParameters, Storage};
use crate::state::{self, ContainerState, Status};
use crate::{Isolation, host, layer, paths, stdio};
use anyhow::{Result, bail};
use std::io::IsTerminal;

pub struct Options {
    pub name: Option<String>,
    pub entrypoint: Option<String>,
    /// `None` picks process isolation when the image build is compatible with the host.
    pub isolation: Option<Isolation>,
    /// `None` auto-detects from whether stdin and stdout are terminals.
    pub tty: Option<bool>,
    pub remove: bool,
}

pub fn run(image_ref: &str, args: &[String], opts: Options) -> Result<u32> {
    let image = ImageRecord::load(image_ref)?;
    let isolation = resolve_isolation(&image, opts.isolation)?;
    let (command, command_line) = resolve_command(&image, opts.entrypoint.as_deref(), args)?;
    if let Some(name) = &opts.name {
        state::validate_name(name)?;
    }

    let mut state = ContainerState {
        id: state::new_id()?,
        name: opts.name,
        image: image.reference.clone(),
        command,
        isolation,
        status: Status::Created,
        exit_code: None,
        layers: image.layers.iter().map(|d| paths::layer(d)).collect(),
    };
    state.save()?;

    let tty = opts.tty.unwrap_or_else(|| std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let result = run_inner(&mut state, &image, &command_line, tty);
    if opts.remove {
        if let Err(e) = cleanup(&state) {
            eprintln!("warning: cleanup failed: {e:#}");
        }
    }
    result
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
                "image build {b} cannot run with process isolation on host build {host},                  and hyper-v isolation is not implemented yet"
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

/// Unmounts the scratch layer on every exit path of `run_inner`.
struct Mounted<'a>(&'a std::path::Path);

impl Drop for Mounted<'_> {
    fn drop(&mut self) {
        if let Err(e) = layer::unmount(self.0) {
            eprintln!("warning: unmount scratch layer: {e:#}");
        }
    }
}

fn run_inner(state: &mut ContainerState, image: &ImageRecord, command_line: &str, tty: bool) -> Result<u32> {
    let scratch = state.scratch();
    layer::create_scratch(&scratch, &state.layers)?;
    let volume = layer::mount(&scratch, &state.layers)?;
    let _mounted = Mounted(&scratch);

    let config = ComputeSystem {
        owner: "winner".into(),
        schema_version: spec::SCHEMA_V2_1,
        should_terminate_on_last_handle_closed: true,
        container: Some(Container {
            storage: Storage { layers: layer::chain(&state.layers)?, path: volume },
            mapped_directories: Vec::new(),
        }),
    };
    let system = System::create(&state.id, &config)?;
    system.start()?;
    state.status = Status::Running;
    state.save()?;

    let params = ProcessParameters {
        command_line: command_line.to_string(),
        working_directory: image.working_dir(),
        environment: image.env().collect(),
        emulate_console: tty,
        console_size: if tty { stdio::RawConsole::size() } else { None },
        create_std_in_pipe: true,
        create_std_out_pipe: true,
        create_std_err_pipe: !tty, // ConPTY merges stderr into stdout
    };
    let (process, pipes) = system.create_process(&params)?;
    let process = std::sync::Arc::new(process);

    let _raw = tty.then(stdio::RawConsole::enable);
    // TODO: forward console resize events (HcsModifyProcess with ConsoleSize).
    let outputs = stdio::forward(pipes, {
        let process = process.clone();
        move || {
            // Fails harmlessly if the process already exited.
            let _ = process.close_stdin();
        }
    });
    let code = process.wait()?;
    for t in outputs {
        let _ = t.join();
    }

    state.status = Status::Exited;
    state.exit_code = Some(code);
    state.save()?;
    let _ = system.terminate();
    drop(system);
    Ok(code)
}

/// Quote an argument per the MSVCRT rules so the container's
/// CommandLineToArgvW round-trips it.
fn quote_arg(a: &str) -> String {
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

fn cleanup(state: &ContainerState) -> Result<()> {
    if let Ok(system) = System::open(&state.id) {
        let _ = system.terminate();
    }
    let scratch = state.scratch();
    if scratch.exists() {
        let _ = layer::unmount(&scratch); // may already be unmounted
        layer::destroy(&scratch)?;
    }
    std::fs::remove_dir_all(state.dir())?;
    Ok(())
}

pub fn remove(query: &str) -> Result<()> {
    let state = state::resolve(query)?;
    if matches!(state.status, Status::Running) {
        // TODO: --force; for now terminate whatever is still there.
        eprintln!("container {} is marked running, terminating", state.id);
    }
    cleanup(&state)?;
    println!("{}", state.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("dir"), "dir");
        assert_eq!(quote_arg("a b"), r#""a b""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }
}

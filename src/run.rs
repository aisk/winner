//! Foreground container lifecycle: create -> start -> exec -> wait -> cleanup.

use crate::hcs::System;
use crate::image::ImageRecord;
use crate::spec::{self, ComputeSystem, Container, ProcessParameters, Storage};
use crate::state::{ContainerState, Status};
use crate::{Isolation, layer, paths, stdio};
use anyhow::{Result, bail};
use std::io::IsTerminal;

/// `tty`: `None` auto-detects from whether stdin and stdout are terminals.
pub fn run(image_ref: &str, command: &[String], isolation: Isolation, tty: Option<bool>, rm: bool) -> Result<u32> {
    if matches!(isolation, Isolation::Hyperv) {
        // TODO: utility VM (HostingSystemId + VirtualMachine schema).
        bail!("hyper-v isolation is not implemented yet");
    }

    let image = ImageRecord::load(image_ref)?;
    let mut state = ContainerState {
        id: format!("{:?}", windows::core::GUID::new()?).to_lowercase(),
        image: image.reference.clone(),
        command: command.to_vec(),
        isolation,
        status: Status::Created,
        exit_code: None,
        layers: image.layers.iter().map(|d| paths::layer(d)).collect(),
    };
    state.save()?;

    let tty = tty.unwrap_or_else(|| std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let result = run_inner(&mut state, &image, tty);
    if rm {
        if let Err(e) = cleanup(&state) {
            eprintln!("warning: cleanup failed: {e:#}");
        }
    }
    result
}

fn run_inner(state: &mut ContainerState, image: &ImageRecord, tty: bool) -> Result<u32> {
    let parents = layer::chain(&state.layers);
    let scratch = state.scratch();
    layer::create_scratch(&scratch, parents.clone())?;
    layer::attach(&scratch, parents.clone())?;

    let config = ComputeSystem {
        owner: "winner".into(),
        schema_version: spec::SCHEMA_V2_1,
        should_terminate_on_last_handle_closed: true,
        container: Some(Container {
            storage: Storage {
                layers: parents,
                // TODO: confirm whether HCS wants the scratch dir or its volume path here.
                path: scratch.to_string_lossy().into_owned(),
            },
            mapped_directories: Vec::new(),
        }),
    };
    let system = System::create(&state.id, &config)?;
    system.start()?;
    state.status = Status::Running;
    state.save()?;

    let params = ProcessParameters {
        command_line: join_command_line(&state.command),
        working_directory: image.working_dir(),
        environment: image.env().collect(),
        emulate_console: tty,
        console_size: if tty { stdio::RawConsole::size() } else { None },
        create_std_in_pipe: true,
        create_std_out_pipe: true,
        create_std_err_pipe: !tty, // ConPTY merges stderr into stdout
    };
    let (process, pipes) = system.create_process(&params)?;

    let _raw = tty.then(stdio::RawConsole::enable);
    // TODO: forward console resize events (HcsModifyProcess with ConsoleSize).
    let outputs = stdio::forward(pipes);
    let code = process.wait()?;
    for t in outputs {
        let _ = t.join();
    }

    state.status = Status::Exited;
    state.exit_code = Some(code);
    state.save()?;
    let _ = system.terminate();
    Ok(code)
}

/// Quote arguments per the MSVCRT rules so the container's CommandLineToArgvW
/// round-trips them.
fn join_command_line(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains([' ', '\t', '"']) {
                return a.clone();
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
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn cleanup(state: &ContainerState) -> Result<()> {
    if let Ok(system) = System::open(&state.id) {
        let _ = system.terminate();
    }
    let scratch = state.scratch();
    if scratch.exists() {
        let _ = layer::detach(&scratch);
        layer::destroy(&scratch)?;
    }
    std::fs::remove_dir_all(state.dir())?;
    Ok(())
}

pub fn remove(id: &str) -> Result<()> {
    let state = ContainerState::load(id)?;
    if matches!(state.status, Status::Running) {
        // TODO: --force; for now terminate whatever is still there.
        eprintln!("container {id} is marked running, terminating");
    }
    cleanup(&state)
}

#[cfg(test)]
mod tests {
    use super::join_command_line;

    #[test]
    fn quoting() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(join_command_line(&args(&["cmd.exe", "/c", "dir"])), "cmd.exe /c dir");
        assert_eq!(join_command_line(&args(&["a b", ""])), r#""a b" """#);
        assert_eq!(join_command_line(&args(&[r#"say "hi""#])), r#""say \"hi\"""#);
        assert_eq!(join_command_line(&args(&[r"C:\dir with space\"])), r#""C:\dir with space\\""#);
    }
}

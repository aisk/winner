//! Processes inside a container: their HCS parameters, environment and
//! command line, and forwarding this process's console to one.

use crate::hcs::{Process, Stdio, System};
use crate::spec::ProcessParameters;
use crate::state::ProcessConfig;
use crate::stdio;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

pub fn forward_console(process: Process, pipes: Stdio, tty: bool) -> Result<u32> {
    let process = Arc::new(process);

    let _raw = tty.then(stdio::RawConsole::enable);
    let done = Arc::new(AtomicBool::new(false));
    let resizer = tty.then(|| watch_console_size(process.clone(), done.clone()));
    let outputs = stdio::forward(pipes, {
        let process = process.clone();
        move || {
            // Fails harmlessly if the process already exited.
            let _ = process.close_stdin();
        }
    });
    let code = process.wait();
    done.store(true, Ordering::Relaxed);
    for t in outputs.into_iter().chain(resizer) {
        let _ = t.join();
    }
    code
}

/// Forward host console size changes to the container's ConPTY. Polled: the
/// stdin thread consumes the console input, including resize events.
fn watch_console_size(
    process: Arc<Process>,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last = stdio::RawConsole::size();
        while !done.load(Ordering::Relaxed) {
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

#[cfg(test)]
mod tests {
    use super::{merge_env, quote_arg};

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("dir"), "dir");
        assert_eq!(quote_arg("a b"), r#""a b""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
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

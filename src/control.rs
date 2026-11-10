//! Operations on existing containers. These work the same whether the
//! container runs in a foreground `run` or under a detached shim, because
//! they go through vmcompute by container ID.

use crate::hcs::System;
use crate::run::{self, quote_arg};
use crate::state::{self, ContainerState, ProcessConfig};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Shut the container down, terminating it if it has not exited after `timeout`.
pub fn stop(query: &str, timeout: Duration) -> Result<()> {
    let state = state::resolve(query)?;
    if state.is_running() {
        let system = System::open(&state.id)?;
        if let Err(e) = system.shutdown(timeout) {
            // Not every container honours a shutdown request; fall back to force.
            eprintln!("warning: container {} did not shut down cleanly ({e:#}), terminating", state.id);
            if let Err(e) = system.terminate() {
                // It may have exited on its own in the meantime.
                if crate::hcs::running_systems().is_ok_and(|live| live.contains(&state.id)) {
                    return Err(e);
                }
            }
        }
        wait_exited(&state)?;
    }
    println!("{}", state.id);
    Ok(())
}

pub fn kill(query: &str) -> Result<()> {
    let state = state::resolve(query)?;
    if !state.is_running() {
        bail!("container {} is not running", state.id);
    }
    System::open(&state.id)?.terminate()?;
    wait_exited(&state)?;
    println!("{}", state.id);
    Ok(())
}

/// The supervisor records the exit, then releases the network and scratch
/// layer and its lock; wait for all of it so a following `delete` or `start`
/// does not collide with that cleanup.
fn wait_exited(state: &ContainerState) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while state::supervised(&state.id) || state::resolve(&state.id).is_ok_and(|s| s.is_running()) {
        if Instant::now() > deadline {
            bail!("container {} stopped but its state was not updated", state.id);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub struct ExecOptions {
    pub tty: Option<bool>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

/// Run another process in a running container with the console attached.
pub fn exec(query: &str, args: &[String], opts: ExecOptions) -> Result<u32> {
    let state = state::resolve(query)?;
    if !state.is_running() {
        bail!("container {} is not running", state.id);
    }
    let system = System::open(&state.id)?;
    let process = ProcessConfig {
        command_line: args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" "),
        environment: run::merge_env(state.process.environment.clone(), &opts.env)?,
        working_directory: opts.workdir.or_else(|| state.process.working_directory.clone()),
        user: opts.user.or_else(|| state.process.user.clone()),
    };
    let tty = opts.tty.unwrap_or_else(crate::stdio::detect_tty);
    run::attach(&system, &process, tty)
}

pub fn remove(query: &str, force: bool) -> Result<()> {
    let state = state::resolve(query)?;
    if (state.is_running() || state::supervised(&state.id)) && !force {
        bail!("container {} is running or still stopping; stop it first or use --force", state.id);
    }
    run::cleanup(&state)?;
    println!("{}", state.id);
    // Layers kept alive only by this container (its image was deleted with
    // --force) are garbage now.
    if crate::image::find(&state.image).ok().flatten().is_none() {
        crate::image::collect_garbage();
    }
    Ok(())
}

pub fn inspect(queries: &[String]) -> Result<()> {
    let states = queries.iter().map(|q| state::resolve(q)).collect::<Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for s in states {
        let mut v = serde_json::to_value(&s)?;
        if s.network.is_some() && s.is_running() {
            match crate::network::endpoint_summary(&s.id) {
                Ok(Some(e)) => v["endpoint"] = e,
                Ok(None) => {}
                Err(e) => eprintln!("warning: query endpoint of {}: {e:#}", s.id),
            }
        }
        out.push(v);
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Print the output of a detached container; with `follow`, keep printing
/// until it stops running.
pub fn logs(query: &str, follow: bool) -> Result<()> {
    let state = state::resolve(query)?;
    let path = state.log();
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !follow {
                return Ok(()); // never ran detached, or no output yet
            }
            wait_for_file(&state)?
        }
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let mut out = std::io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n > 0 {
            out.write_all(&buf[..n])?;
            out.flush()?;
            continue;
        }
        if !follow {
            return Ok(());
        }
        // At the end: stop once the container is no longer running, after
        // one more read for anything written just before it exited.
        let running = state::resolve(&state.id).is_ok_and(|s| s.is_running());
        if !running {
            std::io::copy(&mut file, &mut out)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_file(state: &ContainerState) -> Result<std::fs::File> {
    loop {
        if let Ok(f) = std::fs::File::open(state.log()) {
            return Ok(f);
        }
        let current = state::resolve(&state.id)?;
        if !current.is_running() && !matches!(current.status, state::Status::Created) {
            bail!("container {} has no logs", state.id);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

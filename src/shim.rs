//! Per-container supervisor for containers running in the background.
//!
//! `start` and `run --detach` spawn `winner shim <id>` as a detached process
//! and wait for the state to leave `created`. The shim runs the same
//! supervisor as a foreground `run` with output going to the container log,
//! records the exit code, and deletes the container if it was created with
//! `--remove` (unless it failed to start: that one is kept so `start` and
//! `inspect` can report why). It holds the only handle to the compute system, so killing the
//! shim takes the container down too, and `list` notices.

use crate::run::{self, Io};
use crate::state::{ContainerState, Status};
use anyhow::Result;
use std::io::Write;

pub fn main(id: &str) -> Result<()> {
    let mut state = ContainerState::load(id)?;
    let result = run::execute(&mut state, Io::Log);
    if let Err(e) = &result {
        let message = format!("{e:#}");
        if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.log()) {
            let _ = writeln!(log, "winner: {message}");
        }
        // `start` is polling for this.
        state.status = Status::Exited;
        state.error = Some(message);
        state.save()?;
    }
    if state.remove && result.is_ok() {
        run::cleanup(&state)?;
    }
    Ok(())
}

//! Per-container supervisor for `run -d`.
//!
//! Plan: `run -d` spawns `winner shim <id>` detached (DETACHED_PROCESS |
//! CREATE_NEW_PROCESS_GROUP) and exits. The shim then:
//!   1. creates and starts the compute system and the process, without a tty
//!   2. writes stdout/stderr to `containers/<id>/log` (via `stdio`)
//!   3. waits for exit, records the exit code in state.json
//!   4. performs cleanup if the container was created with --rm

use anyhow::{Result, bail};

pub fn main(id: &str) -> Result<()> {
    let _ = id;
    bail!("detached containers are not implemented yet")
}

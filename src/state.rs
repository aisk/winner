//! Container state persisted on disk. There is no daemon, so this file is
//! the only record of a container besides what vmcompute tracks.

use crate::network::PortMapping;
use crate::spec::MappedDirectory;
use crate::{Format, Isolation, paths, table};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Exited,
}

#[derive(Serialize, Deserialize)]
pub struct ContainerState {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub image: String,
    pub command: Vec<String>,
    pub isolation: Isolation,
    pub status: Status,
    pub exit_code: Option<u32>,
    /// Why the last start failed, for containers started in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub created: u64,
    /// Imported layer directories, base first.
    pub layers: Vec<PathBuf>,
    /// The process to start, resolved from the image and options at create time.
    #[serde(default)]
    pub process: ProcessConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MappedDirectory>,
    /// HNS network to connect to; `None` runs without networking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMapping>,
    /// Delete the container once it exits.
    #[serde(default)]
    pub remove: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ProcessConfig {
    pub command_line: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl ContainerState {
    pub fn dir(&self) -> PathBuf {
        paths::container(&self.id)
    }

    pub fn scratch(&self) -> PathBuf {
        self.dir().join("scratch")
    }

    /// Output of a container started in the background.
    pub fn log(&self) -> PathBuf {
        self.dir().join("log")
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, Status::Running)
    }

    /// Write the state. Only `run::create` makes the directory: a container
    /// deleted meanwhile (`delete --force` racing its supervisor or `start`)
    /// must not come back without a scratch layer.
    pub fn save(&self) -> Result<()> {
        if !self.dir().exists() {
            return Ok(());
        }
        // Per process: two writers must not interleave into one temp file.
        let tmp = self.dir().join(format!("state.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, self.dir().join("state.json"))?;
        Ok(())
    }

    /// See [`lock`].
    pub fn lock_supervisor(&self, wait: Duration) -> Result<std::fs::File> {
        lock(&self.id, wait)
    }

    pub fn load(id: &str) -> Result<Self> {
        let data = std::fs::read(paths::container(id).join("state.json"))
            .with_context(|| format!("container {id} not found"))?;
        Ok(serde_json::from_slice(&data)?)
    }
}

const LOCK: &str = "supervisor.lock";

/// The container's lock, held by its supervisor for as long as it runs, and
/// by anyone about to release the container's resources (`create`, `system
/// prune`, network sweeping), so those never act on a live run. Deleting the
/// container directory is still allowed while it is held.
pub fn lock(id: &str, wait: Duration) -> Result<std::fs::File> {
    crate::lock::wait(&paths::container(id).join(LOCK), crate::lock::FILE_SHARE_DELETE, wait).map_err(|e| {
        if crate::lock::is_held(&e) {
            anyhow::anyhow!("container {id} is in use (running, starting or stopping)")
        } else {
            anyhow::Error::new(e).context(format!("lock container {id}"))
        }
    })
}

/// Whether someone holds container `id`'s lock, normally its supervisor.
/// Unlike the recorded status this cannot go stale.
pub fn supervised(id: &str) -> bool {
    crate::lock::is_locked(&paths::container(id).join(LOCK))
}

/// Length of the ID prefix shown in tables.
pub const SHORT_ID: usize = 12;

/// A new container ID: 32 lowercase hex digits.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn new_id() -> Result<String> {
    Ok(format!("{:032x}", windows::core::GUID::new()?.to_u128()))
}

pub fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok {
        bail!("invalid container name {name:?}: use letters, digits, '_', '.' and '-', starting with a letter or digit");
    }
    if all().iter().any(|s| s.name.as_deref() == Some(name)) {
        bail!("container name {name:?} is already in use");
    }
    Ok(())
}

/// Every container's state, plus a description of each state file that
/// exists but cannot be read. A directory without state.json is a container
/// still being created and is skipped.
fn load_all() -> (Vec<ContainerState>, Vec<String>) {
    let (mut states, mut unreadable) = (Vec::new(), Vec::new());
    for entry in std::fs::read_dir(paths::containers()).into_iter().flatten().flatten() {
        let path = entry.path().join("state.json");
        match std::fs::read(&path) {
            Ok(data) => match serde_json::from_slice(&data) {
                Ok(s) => states.push(s),
                Err(e) => unreadable.push(format!("{}: {e}", path.display())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => unreadable.push(format!("{}: {e}", path.display())),
        }
    }
    (states, unreadable)
}

/// All containers whose state can be read, reconciled with vmcompute.
/// Unreadable ones are reported on stderr.
pub fn all() -> Vec<ContainerState> {
    let (mut states, unreadable) = load_all();
    for u in unreadable {
        eprintln!("warning: unreadable container state {u}");
    }
    reconcile(&mut states);
    states.sort_by_key(|s| s.created);
    states
}

/// All containers, or an error if any state file cannot be read. For
/// garbage collection, which must not take an unreadable container's layers
/// for unused ones.
pub fn all_checked() -> Result<Vec<ContainerState>> {
    let (states, unreadable) = load_all();
    if !unreadable.is_empty() {
        bail!("unreadable container state: {}", unreadable.join("; "));
    }
    Ok(states)
}

/// A record left "running" by a supervisor that was killed has no compute
/// system behind it anymore (it is terminated when the last handle closes).
/// Report it as exited, with an unknown exit code.
///
/// Only the in-memory copy changes: the supervisor is the sole writer of
/// state.json, and writing back here could overwrite what it records in the
/// meantime (such as the error of a failed start).
fn reconcile(states: &mut [ContainerState]) {
    if !states.iter().any(|s| matches!(s.status, Status::Running)) {
        return;
    }
    // Enumeration needs elevation; without it keep the recorded status.
    let Ok(live) = crate::hcs::running_systems() else { return };
    for s in states.iter_mut() {
        if matches!(s.status, Status::Running) && !live.contains(&s.id) {
            s.status = Status::Exited;
        }
    }
}

/// Find a container by full ID, name, or unique ID prefix.
pub fn resolve(query: &str) -> Result<ContainerState> {
    if query.is_empty() {
        bail!("empty container ID");
    }
    let mut all = all();
    if let Some(i) = all.iter().position(|s| s.id == query || s.name.as_deref() == Some(query)) {
        return Ok(all.swap_remove(i));
    }
    let mut matches: Vec<_> = all.into_iter().filter(|s| s.id.starts_with(query)).collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("container {query} not found"),
        n => bail!("{query} is ambiguous: matches {n} containers"),
    }
}

pub fn list(format: Format) -> Result<()> {
    let containers = all();
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&containers)?),
        Format::Table => {
            let rows: Vec<_> = containers
                .iter()
                .map(|s| {
                    let mut status = format!("{:?}", s.status).to_lowercase();
                    if let Some(code) = s.exit_code {
                        status.push_str(&format!(" ({code})"));
                    }
                    [
                        s.id.chars().take(SHORT_ID).collect(),
                        s.name.clone().unwrap_or_default(),
                        s.image.clone(),
                        status,
                        s.ports.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
                        s.command.join(" "),
                    ]
                })
                .collect();
            table::print(["ID", "NAME", "IMAGE", "STATUS", "PORTS", "COMMAND"], &rows);
        }
    }
    Ok(())
}

//! Container state persisted on disk. There is no daemon, so this file is
//! the only record of a container besides what vmcompute tracks.

use crate::{Format, Isolation, paths, table};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// Imported layer directories, base first.
    pub layers: Vec<PathBuf>,
}

impl ContainerState {
    pub fn dir(&self) -> PathBuf {
        paths::container(&self.id)
    }

    pub fn scratch(&self) -> PathBuf {
        self.dir().join("scratch")
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(self.dir())?;
        let tmp = self.dir().join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, self.dir().join("state.json"))?;
        Ok(())
    }

    pub fn load(id: &str) -> Result<Self> {
        let data = std::fs::read(paths::container(id).join("state.json"))
            .with_context(|| format!("container {id} not found"))?;
        Ok(serde_json::from_slice(&data)?)
    }
}

/// Length of the ID prefix shown in tables.
pub const SHORT_ID: usize = 12;

/// A new container ID: 32 lowercase hex digits.
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

/// All containers with a readable state file.
pub fn all() -> Vec<ContainerState> {
    let Ok(dir) = std::fs::read_dir(paths::containers()) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| ContainerState::load(&e.file_name().to_string_lossy()).ok())
        .collect()
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
            // TODO: reconcile with HcsEnumerateComputeSystems, a "running" record
            // may be stale if the CLI was killed.
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
                        s.command.join(" "),
                    ]
                })
                .collect();
            table::print(["ID", "NAME", "IMAGE", "STATUS", "COMMAND"], &rows);
        }
    }
    Ok(())
}

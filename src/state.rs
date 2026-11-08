//! Container state persisted on disk. There is no daemon, so this file is
//! the only record of a container besides what vmcompute tracks.

use crate::{Isolation, paths};
use anyhow::{Context, Result};
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

pub fn list() -> Result<()> {
    let Ok(dir) = std::fs::read_dir(paths::containers()) else {
        return Ok(());
    };
    println!("{:<36} {:<40} {:<10} COMMAND", "ID", "IMAGE", "STATUS");
    for entry in dir.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        let Ok(s) = ContainerState::load(&id) else { continue };
        // TODO: reconcile with HcsEnumerateComputeSystems, a "running" record
        // may be stale if the CLI was killed.
        println!("{:<36} {:<40} {:<10} {}", s.id, s.image, format!("{:?}", s.status).to_lowercase(), s.command.join(" "));
    }
    Ok(())
}

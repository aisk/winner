//! Local image records.

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct ImageRecord {
    pub reference: String,
    pub manifest_digest: String,
    pub os_version: Option<String>,
    /// Layer digests, base layer first.
    pub layers: Vec<String>,
    /// Raw image config (Entrypoint/Cmd/Env/WorkingDir live under "config").
    pub config: serde_json::Value,
}

impl ImageRecord {
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(paths::images())?;
        std::fs::write(paths::image(&self.reference), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn load(reference: &str) -> Result<Self> {
        let reference = normalize(reference);
        let data = std::fs::read(paths::image(&reference))
            .with_context(|| format!("image {reference} not found locally, run `winner pull` first"))?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Image environment as `KEY=VALUE` pairs.
    pub fn env(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.config["config"]["Env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str()?.split_once('=').map(|(k, v)| (k.into(), v.into())))
    }

    pub fn working_dir(&self) -> Option<String> {
        self.config["config"]["WorkingDir"].as_str().filter(|s| !s.is_empty()).map(Into::into)
    }
}

/// Normalize a reference the same way the registry client does, so that
/// `nanoserver:ltsc2025` and the fully qualified form share one record.
pub fn normalize(reference: &str) -> String {
    reference
        .parse::<oci_client::Reference>()
        .map(|r| r.whole())
        .unwrap_or_else(|_| reference.to_string())
}

pub fn list() -> Result<()> {
    let Ok(dir) = std::fs::read_dir(paths::images()) else {
        return Ok(());
    };
    println!("{:<60} {:<20} DIGEST", "REFERENCE", "OS VERSION");
    for entry in dir.flatten() {
        let Ok(data) = std::fs::read(entry.path()) else { continue };
        let Ok(rec) = serde_json::from_slice::<ImageRecord>(&data) else { continue };
        let short = rec.manifest_digest.trim_start_matches("sha256:").get(..12).unwrap_or("");
        println!("{:<60} {:<20} {short}", rec.reference, rec.os_version.as_deref().unwrap_or("-"));
    }
    Ok(())
}

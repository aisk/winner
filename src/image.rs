//! Local image records.

use crate::{Format, paths, table};
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
            .with_context(|| format!("image {reference} not found locally, run `winner image pull` first"))?;
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

    pub fn entrypoint(&self) -> Vec<String> {
        self.string_array("Entrypoint")
    }

    pub fn cmd(&self) -> Vec<String> {
        self.string_array("Cmd")
    }

    /// Set by Windows image builders when Entrypoint/Cmd hold an already
    /// escaped command line that must be passed through verbatim.
    pub fn args_escaped(&self) -> bool {
        self.config["config"]["ArgsEscaped"].as_bool().unwrap_or(false)
    }

    fn string_array(&self, key: &str) -> Vec<String> {
        self.config["config"][key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(Into::into))
            .collect()
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

pub fn delete(reference: &str) -> Result<()> {
    let reference = normalize(reference);
    std::fs::remove_file(paths::image(&reference))
        .with_context(|| format!("image {reference} not found locally"))?;
    // TODO: garbage-collect layers and blobs no longer referenced by any image or container.
    println!("{reference}");
    Ok(())
}

/// Summary of an image for `image list --format json`.
#[derive(Serialize)]
struct Summary<'a> {
    reference: &'a str,
    digest: &'a str,
    os_version: Option<&'a str>,
    layers: &'a [String],
}

pub fn list(format: Format) -> Result<()> {
    let Ok(dir) = std::fs::read_dir(paths::images()) else {
        return Ok(());
    };
    let records: Vec<ImageRecord> = dir
        .flatten()
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    match format {
        Format::Json => {
            let summaries: Vec<_> = records
                .iter()
                .map(|r| Summary {
                    reference: &r.reference,
                    digest: &r.manifest_digest,
                    os_version: r.os_version.as_deref(),
                    layers: &r.layers,
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&summaries)?);
        }
        Format::Table => {
            let rows: Vec<_> = records
                .iter()
                .map(|r| {
                    let short = r.manifest_digest.trim_start_matches("sha256:").get(..12).unwrap_or("");
                    [r.reference.clone(), r.os_version.clone().unwrap_or_else(|| "-".into()), short.into()]
                })
                .collect();
            table::print(["REFERENCE", "OS VERSION", "DIGEST"], &rows);
        }
    }
    Ok(())
}

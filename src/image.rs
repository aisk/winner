//! Local image records.

use crate::{Format, paths, table};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::PathBuf;
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

    /// Default account inside the container, e.g. `ContainerUser`.
    pub fn user(&self) -> Option<String> {
        self.config["config"]["User"].as_str().filter(|s| !s.is_empty()).map(Into::into)
    }

    pub fn working_dir(&self) -> Option<String> {
        self.config["config"]["WorkingDir"].as_str().filter(|s| !s.is_empty()).map(Into::into)
    }
}

/// Whether the first path component names a registry host, using the same
/// rule as the OCI distribution reference grammar.
fn has_registry(reference: &str) -> bool {
    reference
        .split_once('/')
        .is_some_and(|(host, _)| host.contains(['.', ':']) || host == "localhost")
}

/// Append `:latest` when the reference has neither a tag nor a digest.
fn with_default_tag(reference: &str) -> String {
    let last = reference.rsplit('/').next().unwrap_or(reference);
    if last.contains([':', '@']) { reference.to_string() } else { format!("{reference}:latest") }
}

/// Canonical form of a fully qualified reference. There is deliberately no
/// default registry: Windows images do not live on Docker Hub, so silently
/// expanding `nanoserver` to `docker.io/library/nanoserver` only misleads.
pub fn normalize(reference: &str) -> Result<String> {
    if !has_registry(reference) {
        bail!("{reference}: image references must include the registry, e.g. mcr.microsoft.com/windows/nanoserver:ltsc2025");
    }
    let parsed: oci_client::Reference = reference.parse().with_context(|| format!("invalid image reference {reference}"))?;
    Ok(parsed.whole())
}

/// All local image records.
pub fn all() -> Vec<ImageRecord> {
    let Ok(dir) = std::fs::read_dir(paths::images()) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect()
}

/// Find a local image. A fully qualified reference must match exactly; a
/// short one such as `nanoserver:ltsc2025` matches a unique local image whose
/// reference ends with it. A short reference that matches nothing is an error,
/// since it could not be pulled either.
pub fn find(reference: &str) -> Result<Option<ImageRecord>> {
    if has_registry(reference) {
        let reference = normalize(reference)?;
        return Ok(all().into_iter().find(|r| r.reference == reference));
    }
    let suffix = format!("/{}", with_default_tag(reference));
    let mut matches: Vec<_> = all().into_iter().filter(|r| r.reference.ends_with(&suffix)).collect();
    match matches.len() {
        1 => Ok(matches.pop()),
        0 => bail!(
            "image {reference} not found locally; to pull it, include the registry, e.g. mcr.microsoft.com/windows/nanoserver:ltsc2025"
        ),
        _ => {
            let names: Vec<_> = matches.iter().map(|r| r.reference.as_str()).collect();
            bail!("{reference} is ambiguous: {}", names.join(", "))
        }
    }
}

pub fn delete(reference: &str, force: bool) -> Result<()> {
    let image = find(reference)?.with_context(|| format!("image {reference} not found locally"))?;
    let users: Vec<_> = crate::state::all()
        .into_iter()
        .filter(|c| c.image == image.reference)
        .map(|c| c.id.chars().take(crate::state::SHORT_ID).collect::<String>())
        .collect();
    if !users.is_empty() && !force {
        bail!("image {} is used by containers {}; delete them first or use --force", image.reference, users.join(", "));
    }
    std::fs::remove_file(paths::image(&image.reference))?;
    println!("{}", image.reference);
    Ok(())
}

/// Destroy imported layers, and their downloaded blobs, that no image record
/// or container references anymore. Failures are reported and skipped.
pub fn collect_garbage() {
    let mut keep: HashSet<PathBuf> = all().iter().flat_map(|r| r.layers.iter().map(|d| paths::layer(d))).collect();
    keep.extend(crate::state::all().into_iter().flat_map(|c| c.layers));

    let layers = paths::root().join("layers");
    for entry in std::fs::read_dir(&layers).into_iter().flatten().flatten() {
        let dir = entry.path();
        if keep.contains(&dir) {
            continue;
        }
        eprintln!("deleting layer {}", entry.file_name().to_string_lossy());
        if let Err(e) = crate::layer::destroy(&dir) {
            eprintln!("warning: delete layer {}: {e:#}", dir.display());
        }
    }

    // A blob is only needed to (re)import its layer: blobs/<algo>/<hex> pairs
    // with layers/<algo>-<hex>. Leftover `.partial` downloads go too.
    for algo in std::fs::read_dir(paths::root().join("blobs")).into_iter().flatten().flatten() {
        for blob in std::fs::read_dir(algo.path()).into_iter().flatten().flatten() {
            let name = blob.file_name().to_string_lossy().into_owned();
            let layer = layers.join(format!("{}-{name}", algo.file_name().to_string_lossy()));
            if name.ends_with(".partial") || !keep.contains(&layer) {
                if let Err(e) = std::fs::remove_file(blob.path()) {
                    eprintln!("warning: delete blob {name}: {e}");
                }
            }
        }
    }
}

pub fn inspect(references: &[String]) -> Result<()> {
    let records = references
        .iter()
        .map(|r| find(r)?.with_context(|| format!("image {r} not found locally")))
        .collect::<Result<Vec<_>>>()?;
    println!("{}", serde_json::to_string_pretty(&records)?);
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
    let records = all();
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

#[cfg(test)]
mod tests {
    use super::{has_registry, normalize, with_default_tag};

    #[test]
    fn registry() {
        assert!(has_registry("mcr.microsoft.com/windows/nanoserver:ltsc2025"));
        assert!(has_registry("localhost/x"));
        assert!(has_registry("myhost:5000/x"));
        assert!(!has_registry("nanoserver:ltsc2025"));
        assert!(!has_registry("windows/nanoserver"));
        assert!(normalize("nanoserver").is_err());
        assert_eq!(normalize("mcr.microsoft.com/windows/nanoserver").unwrap(), "mcr.microsoft.com/windows/nanoserver:latest");
    }

    #[test]
    fn default_tag() {
        assert_eq!(with_default_tag("windows/nanoserver"), "windows/nanoserver:latest");
        assert_eq!(with_default_tag("nanoserver:ltsc2025"), "nanoserver:ltsc2025");
        assert_eq!(with_default_tag("x@sha256:ab"), "x@sha256:ab");
        assert_eq!(with_default_tag("host:5000/x"), "host:5000/x:latest");
    }
}

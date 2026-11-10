//! Local image records.

use crate::{Format, lock, paths, table};
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
        let path = paths::image(&self.reference);
        // Write and rename: a truncated record would read as missing, and
        // garbage collection would then take its layers for unused ones.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
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

/// Every image record, plus a description of each record file that cannot
/// be read.
fn load_all() -> (Vec<ImageRecord>, Vec<String>) {
    let (mut records, mut unreadable) = (Vec::new(), Vec::new());
    for entry in std::fs::read_dir(paths::images()).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue; // e.g. a record being written
        }
        match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|d| Ok(serde_json::from_slice(&d)?)) {
            Ok(r) => records.push(r),
            Err(e) => unreadable.push(format!("{}: {e}", path.display())),
        }
    }
    (records, unreadable)
}

/// All local image records that can be read; others are reported on stderr.
pub fn all() -> Vec<ImageRecord> {
    let (records, unreadable) = load_all();
    for u in unreadable {
        eprintln!("warning: unreadable image record {u}");
    }
    records
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

/// Coordinates pulls with garbage collection: a layer is imported before the
/// image record that references it is written, so collecting while a pull
/// runs would delete its fresh layers. Pulls hold the lock shared (several
/// may run at once), collection holds it exclusively.
pub struct StoreLock(#[allow(dead_code)] std::fs::File);

impl StoreLock {
    fn path() -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(paths::root())?;
        Ok(paths::root().join("store.lock"))
    }

    /// Shared, for a pull. Waits while garbage collection runs.
    pub fn shared() -> Result<Self> {
        let share = lock::FILE_SHARE_READ | lock::FILE_SHARE_WRITE;
        Self::path()
            .and_then(|p| lock::wait(&p, share, std::time::Duration::from_secs(300)))
            .map(Self)
            .context("lock the image store")
    }

    /// Exclusive, for garbage collection.
    fn exclusive() -> std::io::Result<Self> {
        lock::open(&Self::path()?, lock::FILE_SHARE_NONE).map(Self)
    }
}

/// Destroy imported layers, and their downloaded blobs, that no image record
/// or container references anymore, and leftovers of interrupted imports.
/// Failures are reported and skipped. Skipped entirely while a pull runs, or
/// if any record cannot be read (its layers would look unused).
pub fn collect_garbage() {
    let _lock = match StoreLock::exclusive() {
        Ok(l) => l,
        Err(e) if lock::is_held(&e) => {
            eprintln!("note: an image pull is running; unused layers are kept for now");
            return;
        }
        Err(e) => {
            eprintln!("warning: lock the image store: {e}; unused layers are kept");
            return;
        }
    };
    let (records, unreadable) = load_all();
    let containers = match crate::state::all_checked() {
        Ok(c) if unreadable.is_empty() => c,
        Ok(_) => {
            eprintln!("warning: unreadable image record {}; unused layers are kept", unreadable.join("; "));
            return;
        }
        Err(e) => {
            eprintln!("warning: {e:#}; unused layers are kept");
            return;
        }
    };

    // Leftovers of interrupted imports (none runs now): staging directories,
    // and layers whose import marker is still there.
    for entry in std::fs::read_dir(paths::tmp()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let result = match name.strip_prefix("importing-") {
            Some(layer) => {
                let dir = paths::root().join("layers").join(layer);
                let destroyed = if dir.exists() { crate::layer::destroy(&dir) } else { Ok(()) };
                destroyed.and_then(|()| Ok(std::fs::remove_file(entry.path())?))
            }
            None => std::fs::remove_dir_all(entry.path()).map_err(Into::into),
        };
        if let Err(e) = result {
            eprintln!("warning: delete {}: {e:#}", entry.path().display());
        }
    }

    let mut keep: HashSet<PathBuf> = records.iter().flat_map(|r| r.layers.iter().map(|d| paths::layer(d))).collect();
    keep.extend(containers.into_iter().flat_map(|c| c.layers));

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
            if (name.contains(".partial") || !keep.contains(&layer))
                && let Err(e) = std::fs::remove_file(blob.path())
            {
                eprintln!("warning: delete blob {name}: {e}");
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

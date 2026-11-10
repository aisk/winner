//! On-disk layout under %ProgramData%\winner.
//!
//! ```text
//! root/
//!   blobs/<algo>/<hex>          downloaded, compressed layer blobs
//!   layers/<algo>-<hex>/        imported (expanded) read-only layers
//!   images/<escaped-ref>.json   image records (manifest + config)
//!   containers/<id>/            container state + scratch layer
//! ```

use std::path::PathBuf;

pub fn root() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("winner")
}

pub fn blob(digest: &str) -> PathBuf {
    let (algo, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
    root().join("blobs").join(algo).join(hex)
}

pub fn layer(digest: &str) -> PathBuf {
    root().join("layers").join(digest.replace(':', "-"))
}

/// Staging and bookkeeping of imports; see `image::collect_garbage`.
pub fn tmp() -> PathBuf {
    root().join("tmp")
}

/// Exists while layer `digest` is being imported. Left behind by an
/// interrupted import, it marks the layer directory as incomplete.
pub fn import_marker(digest: &str) -> PathBuf {
    tmp().join(format!("importing-{}", digest.replace(':', "-")))
}

pub fn images() -> PathBuf {
    root().join("images")
}

pub fn image(reference: &str) -> PathBuf {
    let escaped: String = reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    images().join(format!("{escaped}.json"))
}

pub fn containers() -> PathBuf {
    root().join("containers")
}

pub fn container(id: &str) -> PathBuf {
    containers().join(id)
}

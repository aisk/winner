//! On-disk layout under %ProgramData%\winner.
//!
//! ```text
//! root/
//!   blobs/<algo>/<hex>          downloaded, compressed layer blobs
//!   layers/<algo>-<hex>/        imported (expanded) read-only layers
//!   tmp/                        import staging and markers
//!   store.lock, import.lock     see `store`
//!   images/<escaped-ref>.json   image records (manifest + config), see `image`
//!   containers/<id>/            container state + scratch layer
//! ```

use std::path::PathBuf;

pub fn root() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("winner")
}

pub fn blobs() -> PathBuf {
    root().join("blobs")
}

pub fn blob(digest: &str) -> PathBuf {
    let (algo, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
    blobs().join(algo).join(hex)
}

/// Where this process downloads `digest` before renaming it into place; per
/// process, so concurrent pulls of the same blob do not share a file.
pub fn partial_blob(digest: &str) -> PathBuf {
    blob(digest).with_extension(format!("partial-{}", std::process::id()))
}

pub fn layers() -> PathBuf {
    root().join("layers")
}

pub fn layer(digest: &str) -> PathBuf {
    layers().join(digest.replace(':', "-"))
}

pub fn store_lock() -> PathBuf {
    root().join("store.lock")
}

pub fn import_lock() -> PathBuf {
    root().join("import.lock")
}

/// Staging and bookkeeping of imports; see `store::collect_garbage`.
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

/// The record of image `reference`. Every byte other than a lowercase letter,
/// digit, `.` or `-` is written as `_xx` (hex), so distinct references get
/// distinct names even on a case-insensitive file system.
pub fn image(reference: &str) -> PathBuf {
    let mut name = String::new();
    for b in reference.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' => name.push(b as char),
            _ => name.push_str(&format!("_{b:02x}")),
        }
    }
    images().join(format!("{name}.json"))
}

pub fn containers() -> PathBuf {
    root().join("containers")
}

pub fn container(id: &str) -> PathBuf {
    containers().join(id)
}

#[cfg(test)]
mod tests {
    use super::image;

    #[test]
    fn image_names() {
        let name = |r: &str| image(r).file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name("mcr.microsoft.com/windows/nanoserver:ltsc2025"), "mcr.microsoft.com_2fwindows_2fnanoserver_3altsc2025.json");
        // Once the same name: '/' and ':' were both '_', and NTFS ignores case.
        assert_ne!(name("a/b:c"), name("a_b:c"));
        assert_ne!(name("a/b:c"), name("a/b_c"));
        assert_ne!(name("r/x:Tag").to_lowercase(), name("r/x:tag").to_lowercase());
    }
}

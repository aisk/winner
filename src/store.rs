//! The layer store: downloaded blobs and the layers imported from them,
//! shared by every image and container. Pulls add to it under [`Lock`]
//! (shared) and one [`import`] at a time; [`collect_garbage`] removes what no
//! image or container uses, holding the lock exclusively.

use crate::{image, layer, lock, paths, state};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;

/// Coordinates pulls with garbage collection: a layer is imported before the
/// image record that references it is written, so collecting while a pull
/// runs would delete its fresh layers. Pulls hold the lock shared (several
/// may run at once), collection holds it exclusively.
pub struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    fn path() -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(paths::root())?;
        Ok(paths::store_lock())
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

/// Import the layers (digests, base first) that are not on disk yet. A layer
/// directory counts only once its import finished: the import marker is
/// written before and removed after, so a killed import is redone instead of
/// being taken as complete.
pub fn import(layers: &[String]) -> Result<()> {
    // One import at a time across processes; two pulls of images sharing a
    // layer would otherwise write the same directory.
    let _lock = lock::wait(&paths::import_lock(), lock::FILE_SHARE_NONE, std::time::Duration::from_secs(3600))
        .context("wait for another layer import")?;
    let mut imported = Vec::new();
    for digest in layers {
        let dir = paths::layer(digest);
        let marker = paths::import_marker(digest);
        if marker.exists() {
            if dir.exists() {
                layer::destroy(&dir).with_context(|| format!("delete incomplete layer {digest}"))?;
            }
            std::fs::remove_file(&marker)?;
        }
        if !dir.exists() {
            eprintln!("importing {digest}");
            std::fs::create_dir_all(paths::tmp())?;
            std::fs::write(&marker, b"")?;
            layer::import(&paths::blob(digest), &dir, &imported)?;
            std::fs::remove_file(&marker)?;
        }
        imported.push(dir);
    }
    Ok(())
}

/// Destroy imported layers, and their downloaded blobs, that no image record
/// or container references anymore, and leftovers of interrupted imports.
/// Failures are reported and skipped. Skipped entirely while a pull runs, or
/// if any record cannot be read (its layers would look unused).
pub fn collect_garbage() {
    let _lock = match Lock::exclusive() {
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
    let (records, unreadable) = image::load_all();
    let containers = match state::all_checked() {
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
                let dir = paths::layers().join(layer);
                let destroyed = if dir.exists() { layer::destroy(&dir) } else { Ok(()) };
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

    let layers = paths::layers();
    for entry in std::fs::read_dir(&layers).into_iter().flatten().flatten() {
        let dir = entry.path();
        if keep.contains(&dir) {
            continue;
        }
        eprintln!("deleting layer {}", entry.file_name().to_string_lossy());
        if let Err(e) = layer::destroy(&dir) {
            eprintln!("warning: delete layer {}: {e:#}", dir.display());
        }
    }

    // A blob is only needed to (re)import its layer: blobs/<algo>/<hex> pairs
    // with layers/<algo>-<hex>. Leftover `.partial` downloads go too.
    for algo in std::fs::read_dir(paths::blobs()).into_iter().flatten().flatten() {
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

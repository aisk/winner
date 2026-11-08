//! Layer management on top of computestorage.dll.

use crate::spec::{self, LayerData};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::Path;
use windows::Win32::System::HostComputeSystem::*;
use windows::core::HSTRING;

/// Deterministic GUID for a layer path. HCS only needs the ID to be stable
/// and unique per layer; it does not have to match Docker's NameToGuid.
pub fn layer_id(path: &Path) -> String {
    let h = Sha256::digest(path.to_string_lossy().to_lowercase().as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        h[0], h[1], h[2], h[3], h[4], h[5],
        (h[6] & 0x0f) | 0x50, h[7], // version 5
        (h[8] & 0x3f) | 0x80, h[9], // RFC 4122 variant
        h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

/// Build the HCS layer list (topmost first) from directories given base first.
pub fn chain(base_first: &[impl AsRef<Path>]) -> Vec<spec::Layer> {
    base_first
        .iter()
        .rev()
        .map(|p| {
            let p = p.as_ref();
            spec::Layer { id: layer_id(p), path: p.to_string_lossy().into_owned() }
        })
        .collect()
}

fn layer_data(parents: Vec<spec::Layer>) -> Result<HSTRING> {
    let doc = LayerData { schema_version: spec::SCHEMA_V2_1, layers: parents };
    Ok(HSTRING::from(serde_json::to_string(&doc)?))
}

/// Import a compressed layer tarball into `layer_dir`.
///
/// `parents` are the already-imported layers below this one, topmost first.
pub fn import(blob: &Path, layer_dir: &Path, parents: Vec<spec::Layer>) -> Result<()> {
    let _ = (blob, layer_dir, parents);
    // TODO: this is the hard part. Steps:
    //   1. Enable SeBackupPrivilege + SeRestorePrivilege on the process token.
    //   2. Stream the gzip tar into a temp staging dir with backup semantics:
    //      - entries live under `Files/` and `Hives/`
    //      - PAX headers carry MSWINDOWS.rawsd (security descriptor),
    //        MSWINDOWS.fileattr, and possibly LIBARCHIVE.creationtime
    //      - write files via CreateFileW(FILE_FLAG_BACKUP_SEMANTICS) + BackupWrite,
    //        or set attributes/SD after a plain write
    //      - whiteout entries (`.wh.<name>`) mark deletions
    //   3. HcsImportLayer(layer_dir, staging_dir, layer_data(parents))
    //   4. For the base layer (no parents): HcsSetupBaseOSLayer to process the
    //      registry hives (needs a VHD handle for Hyper-V; process isolation may
    //      not need it, to be verified).
    // Reference: hcsshim pkg/ociwclayer + internal/wclayer.
    bail!("layer import not implemented yet")
}

/// Create the writable scratch layer for a container.
pub fn create_scratch(scratch_dir: &Path, parents: Vec<spec::Layer>) -> Result<()> {
    std::fs::create_dir_all(scratch_dir)?;
    // TODO: verify whether process isolation needs a sandbox.vhdx here first
    // (CreateVirtualDisk + HcsFormatWritableLayerVhd), as Hyper-V isolation does.
    unsafe {
        HcsInitializeWritableLayer(&HSTRING::from(scratch_dir.as_os_str()), &layer_data(parents)?, None)
    }
    .context("HcsInitializeWritableLayer")
}

/// Attach the wcifs storage filter so the scratch layer presents the merged view.
pub fn attach(scratch_dir: &Path, parents: Vec<spec::Layer>) -> Result<()> {
    unsafe { HcsAttachLayerStorageFilter(&HSTRING::from(scratch_dir.as_os_str()), &layer_data(parents)?) }
        .context("HcsAttachLayerStorageFilter")
}

pub fn detach(scratch_dir: &Path) -> Result<()> {
    unsafe { HcsDetachLayerStorageFilter(&HSTRING::from(scratch_dir.as_os_str())) }
        .context("HcsDetachLayerStorageFilter")
}

/// Remove a layer directory, including files only removable with backup privileges.
pub fn destroy(dir: &Path) -> Result<()> {
    unsafe { HcsDestroyLayer(&HSTRING::from(dir.as_os_str())) }.context("HcsDestroyLayer")
}

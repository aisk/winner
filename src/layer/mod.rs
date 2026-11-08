//! Layer management on top of computestorage.dll.

mod import;

use crate::spec::{self, LayerData};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::Path;
use windows::Win32::System::HostComputeSystem::*;
use windows::core::{HSTRING, PCWSTR};

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

// Legacy (HCS v1) vmcompute exports that have no computestorage equivalent
// for wcifs-based layers. hcsshim uses the same calls.
mod vmcompute {
    windows_link::link!("vmcompute.dll" "system" fn ProcessBaseImage(path: windows::core::PCWSTR) -> windows::core::HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn ProcessUtilityImage(path: windows::core::PCWSTR) -> windows::core::HRESULT);
}

/// Import a compressed layer tarball into `layer_dir`.
///
/// `parents` are the already-imported layers below this one, topmost first.
/// On failure the partially written layer is removed.
pub fn import(blob: &Path, layer_dir: &Path, parents: Vec<spec::Layer>) -> Result<()> {
    crate::backup::enable_privileges(&["SeBackupPrivilege", "SeRestorePrivilege"])?;
    if !parents.is_empty() {
        // TODO: port hcsshim's legacyLayerWriter (staging dir + tombstones.txt)
        // and finish with HcsImportLayer.
        bail!("importing non-base layers is not implemented yet");
    }

    std::fs::create_dir_all(layer_dir)?;
    let result = import_base(blob, layer_dir);
    // WINNER_KEEP_FAILED leaves the partial layer on disk for debugging.
    if result.is_err() && std::env::var_os("WINNER_KEEP_FAILED").is_none() {
        let _ = destroy(layer_dir);
    }
    result
}

fn import_base(blob: &Path, layer_dir: &Path) -> Result<()> {
    let extracted = import::extract_base_layer(blob, layer_dir)?;
    // Processes the registry hives under Hives/ and lays down the
    // structure HCS expects for a base image.
    let path = HSTRING::from(layer_dir.as_os_str());
    unsafe { vmcompute::ProcessBaseImage(PCWSTR(path.as_ptr())) }
        .ok()
        .context("ProcessBaseImage")?;
    if extracted.has_utility_vm {
        let uvm = HSTRING::from(layer_dir.join("UtilityVM").as_os_str());
        unsafe { vmcompute::ProcessUtilityImage(PCWSTR(uvm.as_ptr())) }
            .ok()
            .context("ProcessUtilityImage")?;
    }
    Ok(())
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

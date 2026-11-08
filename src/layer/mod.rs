//! Layer management for process-isolated (wcifs) containers.
//!
//! Like hcsshim, this uses the legacy vmcompute layer APIs: computestorage's
//! writable-layer functions target VHD/CimFS layers and do not apply here.

mod import;

use crate::spec;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{ERROR_DEVICE_NOT_CONNECTED, ERROR_NOT_READY};
use windows::Win32::System::HostComputeSystem::HcsDestroyLayer;
use windows::core::{GUID, HRESULT, HSTRING, PCWSTR};

mod vmcompute {
    use super::{DriverInfo, LayerDescriptor};
    use windows::core::{GUID, HRESULT, PCWSTR};
    windows_link::link!("vmcompute.dll" "system" fn ProcessBaseImage(path: PCWSTR) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn ProcessUtilityImage(path: PCWSTR) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn NameToGuid(name: PCWSTR, guid: *mut GUID) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn CreateSandboxLayer(info: *const DriverInfo, id: PCWSTR, parent: PCWSTR, layers: *const LayerDescriptor, count: u32) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn ActivateLayer(info: *const DriverInfo, id: PCWSTR) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn DeactivateLayer(info: *const DriverInfo, id: PCWSTR) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn PrepareLayer(info: *const DriverInfo, id: PCWSTR, layers: *const LayerDescriptor, count: u32) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn UnprepareLayer(info: *const DriverInfo, id: PCWSTR) -> HRESULT);
    windows_link::link!("vmcompute.dll" "system" fn GetLayerMountPath(info: *const DriverInfo, id: PCWSTR, length: *mut usize, buffer: *mut u16) -> HRESULT);
}

/// `DriverInfo`: Flavour 1 = filter driver (wcifs), empty home dir. The layer
/// "id" passed alongside is the full layer path.
#[repr(C)]
pub struct DriverInfo {
    flavour: i32,
    home_dir: PCWSTR,
}

/// `WC_LAYER_DESCRIPTOR`
#[repr(C)]
pub struct LayerDescriptor {
    id: GUID,
    flags: u32,
    path: PCWSTR,
}

fn driver_info() -> DriverInfo {
    static EMPTY: [u16; 1] = [0];
    DriverInfo { flavour: 1, home_dir: PCWSTR(EMPTY.as_ptr()) }
}

fn wide(path: &Path) -> HSTRING {
    HSTRING::from(path.as_os_str())
}

/// Layer GUID as HCS derives it: NameToGuid of the layer directory name.
fn layer_guid(path: &Path) -> Result<GUID> {
    let name = path.file_name().context("layer path has no file name")?;
    let mut id = GUID::zeroed();
    unsafe { vmcompute::NameToGuid(PCWSTR(HSTRING::from(name).as_ptr()), &mut id) }
        .ok()
        .context("NameToGuid")?;
    Ok(id)
}

/// Build the HCS layer list (topmost first) from directories given base first.
pub fn chain(base_first: &[PathBuf]) -> Result<Vec<spec::Layer>> {
    base_first
        .iter()
        .rev()
        .map(|p| {
            Ok(spec::Layer {
                id: format!("{:?}", layer_guid(p)?).to_lowercase(),
                path: p.to_string_lossy().into_owned(),
            })
        })
        .collect()
}

/// Owned descriptor array; keeps the path strings alive while vmcompute reads them.
struct Descriptors {
    _paths: Vec<HSTRING>,
    list: Vec<LayerDescriptor>,
}

fn descriptors(base_first: &[PathBuf]) -> Result<Descriptors> {
    let paths: Vec<HSTRING> = base_first.iter().rev().map(|p| wide(p)).collect();
    let list = base_first
        .iter()
        .rev()
        .zip(&paths)
        .map(|(p, w)| Ok(LayerDescriptor { id: layer_guid(p)?, flags: 0, path: PCWSTR(w.as_ptr()) }))
        .collect::<Result<_>>()?;
    Ok(Descriptors { _paths: paths, list })
}

/// Import a compressed layer tarball into `layer_dir`.
///
/// `parents` are the already-imported layers below this one, base first.
/// On failure the partially written layer is removed.
pub fn import(blob: &Path, layer_dir: &Path, parents: &[PathBuf]) -> Result<()> {
    crate::backup::enable_privileges(&["SeBackupPrivilege", "SeRestorePrivilege"])?;
    if !parents.is_empty() {
        // TODO: port hcsshim's legacyLayerWriter (staging dir + tombstones.txt)
        // and finish with vmcompute ImportLayer.
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
    // Processes the registry hives under Hives/ and lays down blank.vhdx etc.
    unsafe { vmcompute::ProcessBaseImage(PCWSTR(wide(layer_dir).as_ptr())) }
        .ok()
        .context("ProcessBaseImage")?;
    if extracted.has_utility_vm {
        let uvm = wide(&layer_dir.join("UtilityVM"));
        unsafe { vmcompute::ProcessUtilityImage(PCWSTR(uvm.as_ptr())) }
            .ok()
            .context("ProcessUtilityImage")?;
    }
    Ok(())
}

/// Create the container's scratch layer (sandbox.vhdx, cloned from the base layer's blank.vhdx).
pub fn create_scratch(scratch_dir: &Path, parents: &[PathBuf]) -> Result<()> {
    std::fs::create_dir_all(scratch_dir)?;
    let d = descriptors(parents)?;
    unsafe {
        vmcompute::CreateSandboxLayer(
            &driver_info(),
            PCWSTR(wide(scratch_dir).as_ptr()),
            PCWSTR::null(),
            d.list.as_ptr(),
            d.list.len() as u32,
        )
    }
    .ok()
    .context("CreateSandboxLayer")
}

/// Mount the scratch VHD and enable the wcifs filter over the parent layers.
/// Returns the volume path to use as the container's `Storage.Path`.
pub fn mount(scratch_dir: &Path, parents: &[PathBuf]) -> Result<String> {
    let info = driver_info();
    let id = wide(scratch_dir);
    let d = descriptors(parents)?;

    // hcsshim retries these two transient errors; seen under load.
    let transient = [HRESULT::from_win32(ERROR_NOT_READY.0), HRESULT::from_win32(ERROR_DEVICE_NOT_CONNECTED.0)];
    let mut attempt = 0;
    loop {
        unsafe { vmcompute::ActivateLayer(&info, PCWSTR(id.as_ptr())) }.ok().context("ActivateLayer")?;
        let hr = unsafe { vmcompute::PrepareLayer(&info, PCWSTR(id.as_ptr()), d.list.as_ptr(), d.list.len() as u32) };
        if hr.is_ok() {
            break;
        }
        let _ = unsafe { vmcompute::DeactivateLayer(&info, PCWSTR(id.as_ptr())) };
        attempt += 1;
        if attempt >= 5 || !transient.contains(&hr) {
            return hr.ok().context("PrepareLayer").map(|_| String::new());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let path = mount_path(&info, &id);
    if path.is_err() {
        let _ = unmount(scratch_dir);
    }
    path
}

fn mount_path(info: &DriverInfo, id: &HSTRING) -> Result<String> {
    let mut len = 0usize;
    unsafe { vmcompute::GetLayerMountPath(info, PCWSTR(id.as_ptr()), &mut len, std::ptr::null_mut()) }
        .ok()
        .context("GetLayerMountPath")?;
    let mut buf = vec![0u16; len];
    unsafe { vmcompute::GetLayerMountPath(info, PCWSTR(id.as_ptr()), &mut len, buf.as_mut_ptr()) }
        .ok()
        .context("GetLayerMountPath")?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16(&buf[..end])?)
}

/// Disable the filter and dismount the scratch VHD.
pub fn unmount(scratch_dir: &Path) -> Result<()> {
    let info = driver_info();
    let id = wide(scratch_dir);
    let unprepare = unsafe { vmcompute::UnprepareLayer(&info, PCWSTR(id.as_ptr())) };
    let deactivate = unsafe { vmcompute::DeactivateLayer(&info, PCWSTR(id.as_ptr())) };
    unprepare.ok().context("UnprepareLayer")?;
    deactivate.ok().context("DeactivateLayer")
}

/// Remove a layer directory, including files only removable with backup privileges.
pub fn destroy(dir: &Path) -> Result<()> {
    unsafe { HcsDestroyLayer(&wide(dir)) }.context("HcsDestroyLayer")
}

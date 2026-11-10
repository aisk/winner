//! HCS JSON schema types (subset): v2, plus the v1 container document used
//! for Hyper-V isolation.
//!
//! Reference: https://learn.microsoft.com/virtualization/api/hcs/schemareference

use serde::{Deserialize, Serialize};

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "PascalCase")]
pub struct SchemaVersion {
    pub major: u32,
    pub minor: u32,
}

pub const SCHEMA_V2_1: SchemaVersion = SchemaVersion { major: 2, minor: 1 };

#[derive(Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Layer {
    /// Layer GUID, see [`crate::layer::chain`].
    pub id: String,
    pub path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ComputeSystem {
    pub owner: String,
    pub schema_version: SchemaVersion,
    pub should_terminate_on_last_handle_closed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Container {
    pub storage: Storage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mapped_directories: Vec<MappedDirectory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub networking: Option<Networking>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Networking {
    /// HCN namespace (GUID) holding the container's endpoints.
    pub namespace: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Storage {
    /// Parent layers, topmost first.
    pub layers: Vec<Layer>,
    /// Volume path of the mounted scratch layer (from GetLayerMountPath).
    pub path: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct MappedDirectory {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct ProcessParameters {
    pub command_line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub environment: std::collections::BTreeMap<String, String>,
    /// Account inside the container, e.g. `ContainerUser`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Use ConPTY inside the container; stdout and stderr are merged.
    pub emulate_console: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_size: Option<[u16; 2]>,
    pub create_std_in_pipe: bool,
    pub create_std_out_pipe: bool,
    pub create_std_err_pipe: bool,
}

/// HCS v1 document for a Hyper-V isolated container: vmcompute boots and
/// manages the utility VM itself (as Docker does), instead of the v2 route
/// where the caller runs the VM and talks to its guest compute service.
///
/// Reference: hcsshim `ContainerConfig` (internal/hcs/schema1).
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerV1 {
    pub system_type: &'static str,
    pub name: String,
    pub owner: String,
    pub ignore_flushes_during_boot: bool,
    /// Scratch layer directory (from CreateSandboxLayer); not mounted.
    pub layer_folder_path: String,
    /// Parent layers, topmost first.
    pub layers: Vec<LayerV1>,
    pub hv_partition: bool,
    pub hv_runtime: HvRuntime,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoint_list: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mapped_directories: Vec<MappedDirectory>,
    pub terminate_on_last_handle_closed: bool,
}

#[derive(Serialize)]
pub struct LayerV1 {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Path")]
    pub path: String,
}

impl From<Layer> for LayerV1 {
    fn from(l: Layer) -> Self {
        Self { id: l.id, path: l.path }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HvRuntime {
    /// `UtilityVM` directory of the topmost layer that has one.
    pub image_path: String,
}

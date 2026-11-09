//! HCS v2 JSON schema types (subset).
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
    // TODO: HostingSystemId / VirtualMachine for Hyper-V isolation.
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Container {
    pub storage: Storage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mapped_directories: Vec<MappedDirectory>,
    // TODO: Networking { Namespace } once HCN is wired up.
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

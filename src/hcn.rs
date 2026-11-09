//! Thin synchronous wrappers over computenetwork.dll (HCN), the v2 API of the
//! Host Network Service. Objects are JSON documents addressed by GUID.
//!
//! Unlike HCS, HCN returns its strings (results and error records) allocated
//! with CoTaskMemAlloc.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::ffi::c_void;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::HostComputeNetwork::*;
use windows::core::{GUID, HRESULT, HSTRING, PWSTR};

#[derive(Clone, Copy)]
pub enum Kind {
    Network,
    Endpoint,
    Namespace,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Network => "network",
            Kind::Endpoint => "endpoint",
            Kind::Namespace => "namespace",
        }
    }
}

/// GUIDs as HCN documents take them: no braces. HCN returns them in either
/// case, so compare parsed values, not strings.
pub fn guid_string(id: &GUID) -> String {
    format!("{id:?}")
}

/// The `ID` of an HCN object document.
pub fn id_of(object: &Value) -> Option<GUID> {
    object["ID"].as_str().and_then(|s| parse_guid(s).ok())
}

pub fn parse_guid(s: &str) -> Result<GUID> {
    GUID::try_from(s.trim_matches(['{', '}'])).map_err(|_| anyhow!("invalid GUID {s:?}"))
}

fn take(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s.filter(|s| !s.is_empty())
}

/// Turn an HCN result into an error carrying the error record's message.
/// The `windows::core::Error` stays reachable through `downcast_ref`.
fn check(r: windows::core::Result<()>, record: PWSTR, what: impl FnOnce() -> String) -> Result<()> {
    let record = take(record);
    r.map_err(|e| {
        let detail = record
            .as_deref()
            .and_then(|r| serde_json::from_str::<Value>(r).ok())
            .and_then(|v| v["ErrorMessage"].as_str().or(v["Error"].as_str()).map(String::from))
            .or(record);
        let what = what();
        let what = match detail {
            Some(d) if !d.trim().is_empty() && !e.message().contains(d.trim()) => format!("{what}: {}", d.trim()),
            _ => what,
        };
        anyhow::Error::new(e).context(what)
    })
}

pub fn hresult(e: &anyhow::Error) -> Option<HRESULT> {
    e.downcast_ref::<windows::core::Error>().map(|e| e.code())
}

fn query(filter: Option<&Value>) -> HSTRING {
    let mut q = json!({ "SchemaVersion": { "Major": 2, "Minor": 0 } });
    if let Some(f) = filter {
        q["Filter"] = Value::String(f.to_string());
    }
    HSTRING::from(q.to_string())
}

/// An open HCN object handle.
struct Handle(Kind, *mut c_void);

impl Handle {
    fn open(kind: Kind, id: &GUID) -> Result<Self> {
        let mut h = std::ptr::null_mut();
        let mut rec = PWSTR::null();
        let r = unsafe {
            match kind {
                Kind::Network => HcnOpenNetwork(id, &mut h, Some(&mut rec)),
                Kind::Endpoint => HcnOpenEndpoint(id, &mut h, Some(&mut rec)),
                Kind::Namespace => HcnOpenNamespace(id, &mut h, Some(&mut rec)),
            }
        };
        check(r, rec, || format!("open {} {}", kind.name(), guid_string(id)))?;
        Ok(Self(kind, h))
    }

    fn properties(&self) -> Result<Value> {
        let q = query(None);
        let mut out = PWSTR::null();
        let mut rec = PWSTR::null();
        let r = unsafe {
            match self.0 {
                Kind::Network => HcnQueryNetworkProperties(self.1, &q, &mut out, Some(&mut rec)),
                Kind::Endpoint => HcnQueryEndpointProperties(self.1, &q, &mut out, Some(&mut rec)),
                Kind::Namespace => HcnQueryNamespaceProperties(self.1, &q, &mut out, Some(&mut rec)),
            }
        };
        let out = take(out);
        check(r, rec, || format!("query {} properties", self.0.name()))?;
        Ok(serde_json::from_str(&out.unwrap_or_default())?)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe {
            match self.0 {
                Kind::Network => HcnCloseNetwork(self.1),
                Kind::Endpoint => HcnCloseEndpoint(self.1),
                Kind::Namespace => HcnCloseNamespace(self.1),
            }
        };
    }
}

/// Properties of every object of `kind` matching `filter` (an object of
/// property values, e.g. `{"Name": "nat"}`). Objects deleted while listing
/// are skipped.
pub fn list(kind: Kind, filter: Option<&Value>) -> Result<Vec<Value>> {
    let q = query(filter);
    let mut out = PWSTR::null();
    let mut rec = PWSTR::null();
    let r = unsafe {
        match kind {
            Kind::Network => HcnEnumerateNetworks(&q, &mut out, Some(&mut rec)),
            Kind::Endpoint => HcnEnumerateEndpoints(&q, &mut out, Some(&mut rec)),
            Kind::Namespace => HcnEnumerateNamespaces(&q, &mut out, Some(&mut rec)),
        }
    };
    let out = take(out);
    check(r, rec, || format!("enumerate {}s", kind.name()))?;
    let ids: Vec<String> = match out {
        Some(s) => serde_json::from_str::<Option<Vec<String>>>(&s)?.unwrap_or_default(),
        None => Vec::new(),
    };
    let mut objects = Vec::new();
    for id in ids {
        if let Some(v) = get(kind, &parse_guid(&id)?)? {
            objects.push(v);
        }
    }
    Ok(objects)
}

/// Properties of one object, or `None` if it does not exist.
pub fn get(kind: Kind, id: &GUID) -> Result<Option<Value>> {
    const E_ACCESSDENIED: HRESULT = HRESULT(0x8007_0005_u32 as i32);
    match Handle::open(kind, id) {
        Ok(h) => h.properties().map(Some),
        Err(e) if hresult(&e) == Some(E_ACCESSDENIED) => Err(e),
        Err(_) => Ok(None),
    }
}

pub fn create_network(id: &GUID, settings: &Value) -> Result<()> {
    let mut h = std::ptr::null_mut();
    let mut rec = PWSTR::null();
    let r = unsafe { HcnCreateNetwork(id, &HSTRING::from(settings.to_string()), &mut h, Some(&mut rec)) };
    check(r, rec, || "create network".into())?;
    drop(Handle(Kind::Network, h));
    Ok(())
}

pub fn create_endpoint(network: &GUID, id: &GUID, settings: &Value) -> Result<()> {
    let net = Handle::open(Kind::Network, network)?;
    let mut h = std::ptr::null_mut();
    let mut rec = PWSTR::null();
    let r = unsafe { HcnCreateEndpoint(net.1, id, &HSTRING::from(settings.to_string()), &mut h, Some(&mut rec)) };
    check(r, rec, || "create endpoint".into())?;
    drop(Handle(Kind::Endpoint, h));
    Ok(())
}

pub fn create_namespace(id: &GUID, settings: &Value) -> Result<()> {
    let mut h = std::ptr::null_mut();
    let mut rec = PWSTR::null();
    let r = unsafe { HcnCreateNamespace(id, &HSTRING::from(settings.to_string()), &mut h, Some(&mut rec)) };
    check(r, rec, || "create namespace".into())?;
    drop(Handle(Kind::Namespace, h));
    Ok(())
}

/// Put an endpoint into a namespace (hcsshim `AddNamespaceEndpoint`).
pub fn add_namespace_endpoint(namespace: &GUID, endpoint: &GUID) -> Result<()> {
    let ns = Handle::open(Kind::Namespace, namespace)?;
    let request = json!({
        "ResourceType": "Endpoint",
        "RequestType": "Add",
        "Settings": { "EndpointId": guid_string(endpoint) },
    });
    let mut rec = PWSTR::null();
    let r = unsafe { HcnModifyNamespace(ns.1, &HSTRING::from(request.to_string()), Some(&mut rec)) };
    check(r, rec, || "add endpoint to namespace".into())
}

/// Delete an object. Deleting one that does not exist succeeds.
pub fn delete(kind: Kind, id: &GUID) -> Result<()> {
    let mut rec = PWSTR::null();
    let r = unsafe {
        match kind {
            Kind::Network => HcnDeleteNetwork(id, Some(&mut rec)),
            Kind::Endpoint => HcnDeleteEndpoint(id, Some(&mut rec)),
            Kind::Namespace => HcnDeleteNamespace(id, Some(&mut rec)),
        }
    };
    match check(r, rec, || format!("delete {} {}", kind.name(), guid_string(id))) {
        Ok(()) => Ok(()),
        // Not every kind has its own "not found" code; check whether it is gone.
        Err(e) => match get(kind, id) {
            Ok(None) => Ok(()),
            _ => Err(e),
        },
    }
}

//! Container networking on top of HCN.
//!
//! HNS objects outlive the processes that create them, so nothing here is
//! recorded in container state. Instead their IDs are derived from names:
//!
//! - a network winner creates: `NameToGuid("winner/network/<name>")`
//! - a container's endpoint: `NameToGuid("winner/endpoint/<container id>")`,
//!   named `winner-<container id>`
//! - its namespace: `NameToGuid("winner/namespace/<container id>")`
//!
//! Creating and releasing are idempotent, and [`sweep`] removes endpoints
//! (with their namespaces) whose container has no live supervisor. HNS is the
//! source of truth; state.json only records which network and ports a
//! container asked for.

use crate::hcn::{self, Kind};
use crate::{Format, state, table};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::Ipv4Addr;
use windows::core::{GUID, HSTRING};

/// The NAT network containers join unless `--network` says otherwise.
/// Created on first use (and again if it disappears, e.g. after a reboot).
pub const DEFAULT: &str = "winner";
/// `--network none`: no network adapter at all.
pub const NONE: &str = "none";
/// `--network host`: share the host's network stack (its default compartment).
pub const HOST: &str = "host";

const ENDPOINT_PREFIX: &str = "winner-";

fn schema() -> Value {
    json!({ "Major": 2, "Minor": 0 })
}

fn derived(kind: &str, name: &str) -> Result<GUID> {
    crate::layer::name_to_guid(&HSTRING::from(format!("winner/{kind}/{name}")))
}

fn network_id(name: &str) -> Result<GUID> {
    derived("network", name)
}

fn endpoint_id(container: &str) -> Result<GUID> {
    derived("endpoint", container)
}

fn namespace_id(container: &str) -> Result<GUID> {
    derived("namespace", container)
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    /// IANA protocol number, as HNS expects it.
    fn number(self) -> u32 {
        match self {
            Protocol::Tcp => 6,
            Protocol::Udp => 17,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
    pub protocol: Protocol,
}

impl std::fmt::Display for PortMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let proto = if self.protocol == Protocol::Tcp { "tcp" } else { "udp" };
        write!(f, "{}->{}/{proto}", self.host, self.container)
    }
}

/// Parse `--publish HOST:CONTAINER[/tcp|udp]`.
pub fn parse_publish(spec: &str) -> Result<PortMapping> {
    let invalid = || format!("invalid --publish {spec:?}: expected HOST:CONTAINER[/tcp|udp]");
    let (ports, protocol) = match spec.rsplit_once('/') {
        Some((p, "tcp")) => (p, Protocol::Tcp),
        Some((p, "udp")) => (p, Protocol::Udp),
        Some(_) => bail!(invalid()),
        None => (spec, Protocol::Tcp),
    };
    let (host, container) = ports.split_once(':').with_context(invalid)?;
    let port = |s: &str| s.parse::<u16>().ok().filter(|&p| p != 0).with_context(invalid);
    Ok(PortMapping { host: port(host)?, container: port(container)?, protocol })
}

/// Validate `--network` and `--publish` at create time. Returns the network
/// to record: `None` for `--network none`.
pub fn resolve(network: Option<&str>, ports: &[PortMapping]) -> Result<Option<String>> {
    let network = match network {
        Some(NONE) => {
            if !ports.is_empty() {
                bail!("--publish needs a network, not --network none");
            }
            return Ok(None);
        }
        Some(HOST) => {
            if !ports.is_empty() {
                bail!("--publish has no effect with --network host: the container uses the host's ports directly");
            }
            return Ok(Some(HOST.to_string()));
        }
        None | Some(DEFAULT) => DEFAULT.to_string(),
        Some(name) => {
            find(name)?.with_context(|| format!("network {name} not found"))?;
            name.to_string()
        }
    };
    for (i, p) in ports.iter().enumerate() {
        if ports[..i].iter().any(|q| q.host == p.host && q.protocol == p.protocol) {
            bail!("host port {} is published twice", p.host);
        }
    }
    Ok(Some(network))
}

/// The network named `name`, if HNS has one.
pub fn find(name: &str) -> Result<Option<Value>> {
    Ok(hcn::list(Kind::Network, Some(&json!({ "Name": name })))?.into_iter().next())
}

fn id_of(object: &Value) -> Result<GUID> {
    hcn::id_of(object).context("HNS object without an ID")
}

/// Create a NAT network. Without `subnet`, HNS picks a free one.
pub fn create(name: &str, subnet: Option<&str>, gateway: Option<&str>) -> Result<Value> {
    if name == NONE || name == HOST {
        bail!("{name} is reserved");
    }
    if find(name)?.is_some() {
        bail!("network {name} already exists");
    }
    let mut settings = json!({ "Name": name, "Type": "NAT", "SchemaVersion": schema() });
    if let Some(subnet) = subnet {
        let gateway = match gateway {
            Some(g) => g.parse::<Ipv4Addr>().with_context(|| format!("invalid --gateway {g:?}"))?,
            None => first_host(subnet)?,
        };
        settings["Ipams"] = json!([{
            "Type": "Static",
            "Subnets": [{
                "IpAddressPrefix": subnet,
                "Routes": [{ "NextHop": gateway.to_string(), "DestinationPrefix": "0.0.0.0/0" }],
            }],
        }]);
    }
    let id = network_id(name)?;
    hcn::create_network(&id, &settings).with_context(|| format!("create network {name}"))?;
    hcn::get(Kind::Network, &id)?.with_context(|| format!("network {name} vanished after creation"))
}

/// The first host address of an IPv4 subnet such as `172.30.0.0/16`.
fn first_host(subnet: &str) -> Result<Ipv4Addr> {
    let invalid = || format!("invalid --subnet {subnet:?}: expected an IPv4 prefix like 172.30.0.0/16");
    let (addr, len) = subnet.split_once('/').with_context(invalid)?;
    let addr: Ipv4Addr = addr.parse().ok().with_context(invalid)?;
    let len: u32 = len.parse().ok().filter(|l| (8..=30).contains(l)).with_context(invalid)?;
    let mask = u32::MAX << (32 - len);
    let base = u32::from(addr) & mask;
    if base != u32::from(addr) {
        bail!("invalid --subnet {subnet:?}: host bits are set (did you mean {}/{len}?)", Ipv4Addr::from(base));
    }
    Ok(Ipv4Addr::from(base + 1))
}

/// The ID of the network a container should join, creating the default one
/// if it does not exist.
fn network_for(name: &str) -> Result<GUID> {
    if let Some(n) = find(name)? {
        return id_of(&n);
    }
    if name != DEFAULT {
        bail!("network {name} not found");
    }
    match create(DEFAULT, None, None) {
        Ok(n) => id_of(&n),
        // Another container created it at the same time (same derived ID).
        Err(e) => find(DEFAULT)?.map_or(Err(e), |n| id_of(&n)),
    }
}

pub fn delete(name: &str) -> Result<()> {
    let network = find(name)?.with_context(|| format!("network {name} not found"))?;
    let id = id_of(&network)?;
    if id != network_id(name)? {
        bail!("network {name} was not created by winner; refusing to delete it");
    }
    let users: Vec<_> = state::all()
        .into_iter()
        .filter(|s| s.network.as_deref() == Some(name) && state::supervised(&s.id))
        .map(|s| s.name.unwrap_or(s.id))
        .collect();
    if !users.is_empty() {
        bail!("network {name} is in use by running containers: {}", users.join(", "));
    }
    sweep();
    hcn::delete(Kind::Network, &id).with_context(|| format!("delete network {name}"))
}

/// Network resources of one container run, released on drop.
pub struct Attachment {
    /// The container whose objects to release; `None` for the host namespace.
    container: Option<String>,
    pub namespace: String,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let Some(container) = &self.container else { return };
        if let Err(e) = release(container) {
            eprintln!("warning: release network: {e:#}");
        }
    }
}

/// Connect a container about to start. The caller must hold the supervisor
/// lock, which keeps [`sweep`] away from these objects.
pub fn attach(state: &state::ContainerState) -> Result<Option<Attachment>> {
    let Some(name) = &state.network else { return Ok(None) };
    if name == HOST {
        return Ok(Some(Attachment { container: None, namespace: host_namespace()? }));
    }
    sweep();
    // Left over from a run whose supervisor was killed.
    release(&state.id)?;
    let network = network_for(name)?;
    let attachment =
        Attachment { container: Some(state.id.clone()), namespace: hcn::guid_string(&namespace_id(&state.id)?) };

    let policies: Vec<_> = state
        .ports
        .iter()
        .map(|p| {
            json!({
                "Type": "PortMapping",
                "Settings": { "InternalPort": p.container, "ExternalPort": p.host, "Protocol": p.protocol.number() },
            })
        })
        .collect();
    let endpoint = endpoint_id(&state.id)?;
    let settings = json!({
        "Name": format!("{ENDPOINT_PREFIX}{}", state.id),
        "HostComputeNetwork": hcn::guid_string(&network),
        "Policies": policies,
        "SchemaVersion": schema(),
    });
    // From here on, dropping `attachment` cleans up whatever was created.
    hcn::create_endpoint(&network, &endpoint, &settings).with_context(|| format!("connect to network {name}"))?;
    let namespace = namespace_id(&state.id)?;
    hcn::create_namespace(&namespace, &json!({ "SchemaVersion": schema() }))?;
    hcn::add_namespace_endpoint(&namespace, &endpoint)?;
    Ok(Some(attachment))
}

/// The HNS namespace of the host's default network compartment. A container
/// placed in it sees the host's adapters, like `--network host` on Linux.
fn host_namespace() -> Result<String> {
    let namespaces = hcn::list(Kind::Namespace, None)?;
    // The v2 query reports it as `"Type": "HostDefault"` (`NamespaceId` 1);
    // `hnsdiag list namespaces -d` shows the v1 view (`IsDefault: true`).
    let host = namespaces.iter().find(|n| n["Type"] == "HostDefault").context("the host's default network namespace was not found")?;
    Ok(hcn::guid_string(&id_of(host)?))
}

/// Remove a container's endpoint and namespace, if any. The namespace goes
/// first: a leftover endpoint is found again by name, a namespace is not.
pub fn release(container: &str) -> Result<()> {
    hcn::delete(Kind::Namespace, &namespace_id(container)?)?;
    hcn::delete(Kind::Endpoint, &endpoint_id(container)?)
}

/// Release the network objects of containers without a live supervisor
/// (killed supervisors, deleted containers). Best effort.
pub fn sweep() {
    let endpoints = match hcn::list(Kind::Endpoint, None) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: list network endpoints: {e:#}");
            return;
        }
    };
    for e in endpoints {
        let Some(id) = e["Name"].as_str().and_then(|n| n.strip_prefix(ENDPOINT_PREFIX)) else { continue };
        let ours = id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit());
        if !ours || state::supervised(id) {
            continue;
        }
        // Only touch objects whose ID we derive: the name alone could collide.
        if endpoint_id(id).ok() != hcn::id_of(&e) {
            continue;
        }
        if let Err(err) = release(id) {
            eprintln!("warning: release network of container {id}: {err:#}");
        }
    }
}

/// Address details of a running container, for `inspect`.
pub fn endpoint_summary(container: &str) -> Result<Option<Value>> {
    let Some(e) = hcn::get(Kind::Endpoint, &endpoint_id(container)?)? else { return Ok(None) };
    let ip = &e["IpConfigurations"][0];
    let gateway = e["Routes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["DestinationPrefix"] == "0.0.0.0/0")
        .map(|r| r["NextHop"].clone());
    Ok(Some(json!({
        "ip_address": ip["IpAddress"],
        "prefix_length": ip["PrefixLength"],
        "gateway": gateway,
        "mac_address": e["MacAddress"],
        "dns_servers": e["Dns"]["ServerList"],
    })))
}

/// HNS leaves `Type` out for NAT networks (presumably its zero value).
fn network_type(network: &Value) -> &str {
    network["Type"].as_str().unwrap_or("NAT")
}

fn subnets(network: &Value) -> Vec<String> {
    network["Ipams"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|i| i["Subnets"].as_array().into_iter().flatten())
        .filter_map(|s| s["IpAddressPrefix"].as_str().map(String::from))
        .collect()
}

pub fn list(format: Format) -> Result<()> {
    let mut networks = hcn::list(Kind::Network, None)?;
    networks.sort_by(|a, b| a["Name"].as_str().cmp(&b["Name"].as_str()));
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&networks)?),
        Format::Table => {
            let rows: Vec<_> = networks
                .iter()
                .map(|n| {
                    let name = n["Name"].as_str().unwrap_or_default();
                    let managed = network_id(name).ok().is_some_and(|g| Some(g) == hcn::id_of(n));
                    [
                        name.to_string(),
                        network_type(n).to_string(),
                        subnets(n).join(", "),
                        if managed { "winner" } else { "" }.to_string(),
                    ]
                })
                .collect();
            table::print(["NAME", "TYPE", "SUBNET", "OWNER"], &rows);
        }
    }
    Ok(())
}

pub fn inspect(names: &[String]) -> Result<()> {
    let networks = names
        .iter()
        .map(|n| find(n)?.with_context(|| format!("network {n} not found")))
        .collect::<Result<Vec<_>>>()?;
    println!("{}", serde_json::to_string_pretty(&networks)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish() {
        assert_eq!(parse_publish("8080:80").unwrap(), PortMapping { host: 8080, container: 80, protocol: Protocol::Tcp });
        assert_eq!(parse_publish("53:53/udp").unwrap().protocol, Protocol::Udp);
        for bad in ["80", "a:80", "0:80", "80:80/sctp", "70000:80", ":80"] {
            assert!(parse_publish(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn subnet() {
        assert_eq!(first_host("172.30.0.0/16").unwrap(), Ipv4Addr::new(172, 30, 0, 1));
        assert!(first_host("172.30.0.5/16").is_err());
        assert!(first_host("172.30.0.0").is_err());
        assert!(first_host("::/64").is_err());
    }
}

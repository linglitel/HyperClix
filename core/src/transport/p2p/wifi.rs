use crate::transport::dbus::{Properties, proxy, string};
use crate::{
    Result,
    transport::discovery::{self, DeviceEndpoint},
};
use anyhow::{Context, bail, ensure};
use std::{collections::HashMap, net::Ipv4Addr, time::Duration};
use zbus::{
    Connection,
    zvariant::{ObjectPath, OwnedObjectPath, Value},
};

const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const P2P: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
const PEER: &str = "org.freedesktop.NetworkManager.WifiP2PPeer";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";

#[derive(Clone, Debug)]
pub struct P2pPeer {
    pub path: String,
    pub address: String,
    pub name: String,
    pub strength: u8,
    pub manufacturer: String,
    pub model: String,
}

#[derive(Clone)]
pub struct WifiDirect {
    bus: Connection,
    device: OwnedObjectPath,
    pub interface: String,
}
impl WifiDirect {
    /// Uses NetworkManager's existing P2P device, keeping the STA connection.
    pub async fn new() -> Result<Self> {
        Self::select(None).await
    }

    pub async fn for_interface(interface: &str) -> Result<Self> {
        Self::select(Some(interface)).await
    }

    async fn select(requested: Option<&str>) -> Result<Self> {
        let bus = Connection::system()
            .await
            .context("connect NetworkManager system bus")?;
        let manager = proxy(&bus, NM, ROOT, NM).await?;
        let devices: Vec<OwnedObjectPath> = manager.call("GetDevices", &()).await?;
        let mut selected = None;
        for path in devices {
            let device = proxy(&bus, NM, path.as_str(), DEVICE).await?;
            if device.get_property::<u32>("DeviceType").await? == 30 {
                let managed: bool = device.get_property("Managed").await?;
                if managed {
                    let interface: String = device.get_property("Interface").await?;
                    if requested.is_some_and(|requested| requested != interface) {
                        continue;
                    }
                    selected = Some((path.clone(), interface));
                    break;
                }
            }
        }
        let (device, interface) =
            selected.context("NetworkManager 没有可用的 Wi-Fi Direct 设备")?;
        drop(manager);
        Ok(Self {
            bus,
            device,
            interface,
        })
    }

    pub async fn find(&self, duration: Duration) -> Result<Vec<P2pPeer>> {
        let p2p = proxy(&self.bus, NM, self.device.as_str(), P2P).await?;
        let options = HashMap::from([(
            "timeout",
            Value::from(duration.as_secs().clamp(1, 600) as i32),
        )]);
        let _: () = p2p
            .call("StartFind", &(options,))
            .await
            .context("start Wi-Fi Direct discovery")?;
        tracing::info!(
            "[nearby] Wi-Fi Direct discovery started: interface={}",
            self.interface
        );
        let result: Result<Vec<P2pPeer>> = async {
            tokio::time::sleep(duration).await;
            let paths: Vec<OwnedObjectPath> = p2p.get_property("Peers").await?;
            let mut peers = Vec::new();
            for path in paths {
                let peer = proxy(&self.bus, NM, path.as_str(), PEER).await?;
                let properties = proxy(
                    &self.bus,
                    NM,
                    path.as_str(),
                    "org.freedesktop.DBus.Properties",
                )
                .await?;
                let values: Properties = properties.call("GetAll", &(PEER,)).await?;
                peers.push(P2pPeer {
                    path: path.to_string(),
                    address: string(&values, "HwAddress").context("P2P peer has no address")?,
                    name: string(&values, "Name").unwrap_or_else(|| "Wi-Fi Direct 设备".into()),
                    strength: values
                        .get("Strength")
                        .and_then(|v| u8::try_from(v).ok())
                        .unwrap_or(0),
                    manufacturer: string(&values, "Manufacturer").unwrap_or_default(),
                    model: string(&values, "Model").unwrap_or_default(),
                });
                drop(peer);
            }
            Ok(peers)
        }
        .await;
        let stopped: zbus::Result<()> = p2p.call("StopFind", &()).await;
        let peers = result?;
        stopped.context("stop Wi-Fi Direct discovery")?;
        tracing::info!(
            "[nearby] Wi-Fi Direct discovery finished: peers={}",
            peers.len()
        );
        Ok(peers)
    }

    /// Connect a real discovered P2P peer using standard WPS negotiation.
    /// The phone may require its own WPS approval; Xiaomi BLE role negotiation
    /// is a separate protocol step and is not inferred from a BLE MAC address.
    pub async fn join(self, peer: &P2pPeer, timeout: Duration) -> Result<P2pLink> {
        self.join_with_ipv4(peer, None, timeout).await
    }

    async fn join_with_ipv4(
        self,
        peer: &P2pPeer,
        static_address: Option<(Ipv4Addr, Ipv4Addr)>,
        timeout: Duration,
    ) -> Result<P2pLink> {
        let device = proxy(&self.bus, NM, self.device.as_str(), DEVICE).await?;
        let current: OwnedObjectPath = device.get_property("ActiveConnection").await?;
        ensure!(
            current.as_str() == "/",
            "Wi-Fi Direct 已有连接，保留当前连接"
        );
        let manager = proxy(&self.bus, NM, ROOT, NM).await?;
        let id = format!("HyperClix-P2P-{}", uuid::Uuid::new_v4().simple());
        let uuid = uuid::Uuid::new_v4().to_string();
        let permission = format!(
            "user:{}:",
            std::env::var("USER").context("读取当前登录用户")?
        );
        let address = static_address.map(|(address, _)| address.to_string());
        let endpoint = static_address.map(|(_, endpoint)| endpoint.to_string());
        let mut ipv4 = HashMap::from([
            (
                "method",
                Value::from(if address.is_some() { "manual" } else { "auto" }),
            ),
            ("never-default", Value::from(true)),
            ("ignore-auto-dns", Value::from(true)),
        ]);
        if let (Some(address), Some(endpoint)) = (&address, &endpoint) {
            ipv4.insert(
                "address-data",
                Value::from(vec![HashMap::from([
                    ("address", Value::from(address.as_str())),
                    ("prefix", Value::from(32u32)),
                ])]),
            );
            ipv4.insert(
                "route-data",
                Value::from(vec![HashMap::from([
                    ("dest", Value::from(endpoint.as_str())),
                    ("prefix", Value::from(32u32)),
                ])]),
            );
        }
        let settings = HashMap::from([
            (
                "connection",
                HashMap::from([
                    ("id", Value::from(id.as_str())),
                    ("uuid", Value::from(uuid.as_str())),
                    ("type", Value::from("wifi-p2p")),
                    ("autoconnect", Value::from(false)),
                    ("permissions", Value::from(vec![permission.as_str()])),
                ]),
            ),
            (
                "wifi-p2p",
                HashMap::from([
                    ("peer", Value::from(peer.address.as_str())),
                    ("wps-method", Value::from(4u32)),
                ]),
            ),
            ("ipv4", ipv4),
            ("ipv6", HashMap::from([("method", Value::from("disabled"))])),
        ]);
        let options = HashMap::from([
            ("persist", Value::from("volatile")),
            ("bind-activation", Value::from("dbus-client")),
        ]);
        let specific = ObjectPath::try_from(peer.path.as_str())?;
        let (_profile, active, _result): (OwnedObjectPath, OwnedObjectPath, Properties) = manager
            .call(
                "AddAndActivateConnection2",
                &(settings, &self.device, specific, options),
            )
            .await
            .context("activate independent Wi-Fi Direct link")?;
        tracing::info!(
            "[nearby] Wi-Fi Direct group activation requested; original network retained"
        );
        drop(device);
        drop(manager);
        let mut link = P2pLink {
            owner: LinkOwner::NetworkManager {
                bus: self.bus,
                active,
            },
            interface: String::new(),
            local_ip: Ipv4Addr::UNSPECIFIED,
            gateway: None,
        };
        let result = tokio::time::timeout(timeout, link.wait_ready())
            .await
            .context("Wi-Fi Direct group activation timed out");
        match result {
            Ok(Ok(())) => Ok(link),
            Ok(Err(error)) | Err(error) => {
                let _ = link.close().await;
                Err(error)
            }
        }
    }
}

enum LinkOwner {
    NetworkManager {
        bus: Connection,
        active: OwnedObjectPath,
    },
    Oob(super::oob::OobLease),
    Closed,
}
pub struct P2pLink {
    owner: LinkOwner,
    pub interface: String,
    pub local_ip: Ipv4Addr,
    pub gateway: Option<Ipv4Addr>,
}
impl P2pLink {
    pub(crate) fn oob(helper: super::oob::OobLease, interface: String, local_ip: Ipv4Addr) -> Self {
        Self {
            owner: LinkOwner::Oob(helper),
            interface,
            local_ip,
            gateway: None,
        }
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let LinkOwner::NetworkManager { bus, active } = &self.owner else {
            unreachable!()
        };
        let connection = proxy(bus, NM, active.as_str(), ACTIVE).await?;
        loop {
            match connection.get_property::<u32>("State").await? {
                2 => break,
                3 | 4 => bail!("Wi-Fi Direct group activation was rejected or disconnected"),
                _ => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        let devices: Vec<OwnedObjectPath> = connection.get_property("Devices").await?;
        for path in devices {
            let device = proxy(bus, NM, path.as_str(), DEVICE).await?;
            let ip: OwnedObjectPath = device.get_property("Ip4Config").await?;
            if ip.as_str() == "/" {
                continue;
            }
            let config = proxy(
                bus,
                NM,
                ip.as_str(),
                "org.freedesktop.NetworkManager.IP4Config",
            )
            .await?;
            let addresses: Vec<Properties> = config.get_property("AddressData").await?;
            if let Some(address) = addresses
                .iter()
                .find_map(|p| string(p, "address").and_then(|ip| ip.parse::<Ipv4Addr>().ok()))
            {
                self.local_ip = address;
                self.interface = device.get_property("IpInterface").await?;
                let gateway: String = config.get_property("Gateway").await?;
                self.gateway = gateway.parse().ok();
                tracing::info!(
                    "[nearby] Wi-Fi Direct group ready: interface={}",
                    self.interface
                );
                return Ok(());
            }
        }
        bail!("Wi-Fi Direct group has no IPv4 address")
    }

    /// Re-discover actual current KCP endpoints on this group, never a fixed IP.
    pub async fn discover(&self, duration: Duration) -> Result<Vec<DeviceEndpoint>> {
        discovery::discover(self.local_ip, duration).await
    }

    pub async fn close(mut self) -> Result<()> {
        match std::mem::replace(&mut self.owner, LinkOwner::Closed) {
            LinkOwner::NetworkManager { bus, active } => {
                let manager = proxy(&bus, NM, ROOT, NM).await?;
                let _: () = manager
                    .call("DeactivateConnection", &(active,))
                    .await
                    .context("close own Wi-Fi Direct group")?;
            }
            LinkOwner::Oob(helper) => helper.close().await?,
            LinkOwner::Closed => {}
        }
        tracing::info!("[nearby] own Wi-Fi Direct group disconnected");
        Ok(())
    }
}
impl Drop for P2pLink {
    fn drop(&mut self) {
        let owner = std::mem::replace(&mut self.owner, LinkOwner::Closed);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                match owner {
                    LinkOwner::NetworkManager { bus, active } => {
                        if let Ok(manager) = proxy(&bus, NM, ROOT, NM).await {
                            let _: zbus::Result<()> =
                                manager.call("DeactivateConnection", &(active,)).await;
                        }
                    }
                    LinkOwner::Oob(helper) => {
                        let _ = helper.close().await;
                    }
                    LinkOwner::Closed => {}
                }
            });
        }
        // bind-activation=dbus-client also releases the group when this bus ends.
    }
}

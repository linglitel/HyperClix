//! Authenticated Lyra medium negotiation. Credentials stay in memory.
use super::p2p::{oob::OobClient, wifi::P2pLink};
use crate::Result;
use crate::protocol::messages::upgrade::{self, UpgradeOffer};
use anyhow::{Context, ensure};
use std::{
    fmt,
    net::{Ipv4Addr, SocketAddrV4},
};

/// Wi-Fi abilities selected from the prepared Linux backend.
/// Callers obtain this snapshot from `UpgradeClient`.
#[derive(Clone, Copy, Debug)]
pub struct WifiCapabilities {
    flags: u32,
}
impl WifiCapabilities {
    pub(crate) fn physical_proto(&self) -> Vec<u8> {
        // MiConnectProto.WifiCapabilites.field1 is fixed32, not a varint.
        let mut out = vec![0x0d];
        out.extend(self.flags.to_le_bytes());
        out
    }
}

/// A phone GO configuration received over the authenticated logical channel.
/// Debug deliberately omits network identity and credentials.
pub(crate) struct WifiGroup {
    pub(crate) ssid: String,
    pub(crate) password: String,
    pub(crate) client_ip: Option<Ipv4Addr>,
    pub endpoint: SocketAddrV4,
    pub channel: u32,
    pub(crate) server_id: Vec<u8>,
    pub(crate) server_mac: String,
}
impl fmt::Debug for WifiGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WifiGroup")
            .field("channel", &self.channel)
            .field("has_static_ip", &self.client_ip.is_some())
            .field(
                "has_credentials",
                &(!self.ssid.is_empty() && !self.password.is_empty()),
            )
            .finish_non_exhaustive()
    }
}

/// Owns the temporary P2P connection and the logical introduction
/// that must be sent on the new physical link before it replaces BLE.
pub(crate) struct P2pUpgradeLink {
    pub link: P2pLink,
    pub endpoint: SocketAddrV4,
    pub logical_id: u32,
    pub introduction: Vec<u8>,
}

/// Linux participates as P2P GC / phone GO (official role pair 1).
/// An isolated supplicant preserves the active STA network.
pub struct UpgradeClient {
    oob: OobClient,
    address: String,
    capabilities: WifiCapabilities,
}
impl UpgradeClient {
    pub async fn new(interface: &str) -> Result<Self> {
        let station = interface.strip_prefix("p2p-dev-").unwrap_or(interface);
        let phy = tokio::fs::canonicalize(format!("/sys/class/net/{station}/phy80211"))
            .await
            .with_context(|| format!("读取 P2P 设备 {interface} 的无线 PHY"))?;
        let info = tokio::process::Command::new("iw")
            .arg("phy")
            .arg(phy.file_name().context("无线 PHY 名称缺失")?)
            .arg("info")
            .output()
            .await
            .context("读取无线设备的 P2P GC 能力")?;
        ensure!(
            info.status.success()
                && String::from_utf8_lossy(&info.stdout)
                    .lines()
                    .any(|line| line.trim() == "* P2P-client"),
            "所选无线设备未声明 P2P-client 能力"
        );
        let oob = OobClient::prepare(station).await?;
        let address = oob.address.clone();
        tracing::info!(
            "[nearby] Linux P2P GC backend prepared: parent={station}, method=OOB WPA-PSK"
        );
        Ok(Self {
            oob,
            address,
            // QoS requires the general P2P bit as well as the supported GC role.
            capabilities: WifiCapabilities {
                flags: (1 << 11) | (1 << 22),
            },
        })
    }

    pub fn capabilities(&self) -> WifiCapabilities {
        self.capabilities
    }

    pub(crate) fn query(&self) -> Vec<u8> {
        upgrade::query(&self.address)
    }

    pub(crate) fn server_available(frame: &[u8]) -> Result<Option<WifiGroup>> {
        let Some(offer) = UpgradeOffer::decode(frame)? else {
            return Ok(None);
        };
        tracing::info!(
            "[nearby] authenticated P2P server available: role=gc:go, channel={}, static_ip={}",
            offer.wifi.channel,
            offer.wifi.client_ip.is_some()
        );
        Ok(Some(WifiGroup {
            ssid: offer.wifi.ssid,
            password: offer.wifi.password,
            client_ip: offer.wifi.client_ip,
            endpoint: SocketAddrV4::new(offer.wifi.kcp_ip, offer.wifi.kcp_port),
            channel: offer.wifi.channel,
            server_id: offer.server_id,
            server_mac: offer.wifi.server_mac,
        }))
    }

    pub(crate) fn introduction(&self, device_id: &[u8], group: &WifiGroup) -> Vec<u8> {
        upgrade::introduction(device_id, &group.server_id, &self.address)
    }
    pub(crate) fn oob(&self) -> &OobClient {
        &self.oob
    }
}

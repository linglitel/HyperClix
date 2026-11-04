//! P2P Wi-Fi Direct medium negotiation messages.
use crate::Result;
use crate::protocol::pb::{Fields, Writer};
use anyhow::{Context, ensure};
use std::net::Ipv4Addr;

#[derive(Clone, Debug)]
pub(crate) struct WifiInfo {
    pub ssid: String,
    pub password: String,
    pub channel: u32,
    pub kcp_ip: Ipv4Addr,
    pub kcp_port: u16,
    pub client_ip: Option<Ipv4Addr>,
    pub server_mac: String,
}

#[derive(Clone, Debug)]
pub(crate) struct UpgradeOffer {
    pub server_id: Vec<u8>,
    pub wifi: WifiInfo,
}

impl UpgradeOffer {
    pub(crate) fn decode(frame: &[u8]) -> Result<Option<Self>> {
        let frame = Fields::decode(frame)?;
        if frame.uint(1) != 1 {
            return Ok(None);
        }
        let available = frame.message::<Fields>(2)?;
        let config = available.message::<Fields>(1)?;
        ensure!(
            matches!(config.uint(1), 32 | 64 | 32768),
            "手机返回的升级服务不是 Wi-Fi P2P"
        );
        let wifi = config.message::<Fields>(5)?;
        let kcp = wifi.message::<Fields>(8)?;
        let kcp_ip: Ipv4Addr = kcp.text(1).parse().context("手机未返回有效 P2P KCP IPv4")?;
        let kcp_port: u16 = kcp.uint(2).try_into().context("P2P KCP 端口无效")?;
        ensure!(
            !kcp_ip.is_unspecified() && kcp_port != 0,
            "手机未返回 P2P KCP 端点"
        );
        let ssid = wifi.text(1);
        let password = wifi.text(2);
        ensure!(
            !ssid.is_empty() && !password.is_empty(),
            "手机未返回 P2P 入组凭据"
        );
        if available.has(5) {
            let roles = available.message::<Fields>(5)?;
            let selected = roles.message::<Fields>(3)?;
            ensure!(
                selected.uint(1) == 1 && selected.uint(4) != 0,
                "手机选择的 P2P 角色不是本机支持的 GC:GO"
            );
        }
        let client_ip = match wifi.text(9) {
            value if value.is_empty() => None,
            value => Some(value.parse().context("手机返回的静态客户端 IPv4 无效")?),
        };
        let server_mac = wifi.text(10);
        ensure!(!server_mac.is_empty(), "手机认证配置缺少 P2P server MAC");
        Ok(Some(Self {
            server_id: available.bytes(3).to_vec(),
            wifi: WifiInfo {
                ssid,
                password,
                channel: wifi.uint(5) as u32,
                kcp_ip,
                kcp_port,
                client_ip,
                server_mac,
            },
        }))
    }
}

pub(crate) fn client_config(address: &str) -> Vec<u8> {
    // ConnectMediumClientConfig: medium, wlan/p2p channels, actual MAC.
    let mut writer = Writer::new();
    writer.varint(1, 32);
    writer.bytes(4, address);
    writer.finish()
}

pub(crate) fn query(address: &str) -> Vec<u8> {
    let mut role = Writer::new();
    for (number, value) in [(1, 1), (2, 5), (3, 1), (5, 32)] {
        role.varint(number, value);
    }
    let mut roles = Writer::new();
    roles.varint(1, 32);
    roles.varint(2, 32);
    roles.bytes(3, role.finish());
    let mut query = Writer::new();
    query.bytes(1, client_config(address));
    query.varint(3, 1);
    query.varint(4, 1);
    query.bytes(5, roles.finish());
    let mut writer = Writer::new();
    writer.varint(1, 2);
    writer.bytes(3, query.finish());
    writer.finish()
}

pub(crate) fn introduction(device_id: &[u8], server_id: &[u8], address: &str) -> Vec<u8> {
    let mut intro = Writer::new();
    intro.bytes(1, device_id);
    intro.bytes(2, server_id);
    intro.varint(3, 1);
    intro.bytes(4, address);
    intro.bytes(5, client_config(address));
    let mut writer = Writer::new();
    writer.varint(1, 3);
    writer.bytes(4, intro.finish());
    writer.finish()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpgradeFrame {
    pub kind: u64,
}

impl UpgradeFrame {
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(Self {
            kind: Fields::decode(data)?.uint(1),
        })
    }
}

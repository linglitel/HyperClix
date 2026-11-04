//! Device, service and Networking payloads.
use crate::Result;
use crate::protocol::pb::{Fields, Message, Writer};
use anyhow::Context;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceService {
    pub name: String,
    pub app: String,
    pub data: Vec<u8>,
}

impl Message for DeviceService {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(1, &self.name);
        writer.bytes(2, &self.app);
        writer.opt_bytes(3, &self.data);
        writer.varint(4, 1);
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            name: fields.text(1),
            app: fields.text(2),
            data: fields.bytes(3).to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalDevice {
    pub short_id: Vec<u8>,
    pub account_id: Vec<u8>,
    pub device_type: u64,
    pub name: String,
    pub supported_medium: u64,
    pub wifi_proto: Option<Vec<u8>>,
}

impl Message for PhysicalDevice {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(2, &self.short_id);
        writer.varint(3, 1);
        writer.bytes(4, &self.account_id);
        writer.varint(5, self.device_type);
        writer.bytes(6, &self.name);
        writer.varint(9, self.supported_medium);
        if let Some(wifi) = &self.wifi_proto {
            writer.bytes(10, wifi);
        }
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            short_id: fields.bytes(2).to_vec(),
            account_id: fields.bytes(4).to_vec(),
            device_type: fields.uint(5),
            name: fields.text(6),
            supported_medium: fields.uint(9),
            wifi_proto: fields.has(10).then(|| fields.bytes(10).to_vec()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceCredential {
    pub nonce: Vec<u8>,
    pub certificate: Vec<u8>,
    pub signature: Vec<u8>,
}

impl Message for DeviceCredential {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(1, &self.nonce);
        writer.bytes(2, &self.certificate);
        writer.bytes(3, &self.signature);
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            nonce: fields.bytes(1).to_vec(),
            certificate: fields.bytes(2).to_vec(),
            signature: fields.bytes(3).to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceGroup {
    pub credential: DeviceCredential,
}

impl Message for DeviceGroup {
    fn encode(&self) -> Vec<u8> {
        let mut group = Writer::new();
        group.varint(1, 3);
        group.message(4, &self.credential);
        let mut writer = Writer::new();
        writer.varint(1, 1);
        writer.bytes(3, group.finish());
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        let group: Fields = fields
            .opt_message(3)?
            .context("device group has no credential envelope")?;
        Ok(Self {
            credential: group.message(4)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkDevice {
    pub name: String,
    pub device_type: u64,
    pub full_id: Vec<u8>,
    pub account_id: Vec<u8>,
    pub service_version: Option<u64>,
    pub services: Vec<DeviceService>,
    pub group: Option<DeviceGroup>,
    pub region: Option<String>,
    pub market_name: Option<String>,
    pub supported_medium: Option<u64>,
    pub short_id: Option<String>,
}

impl NetworkDevice {
    pub(crate) fn services_map(&self) -> Vec<(String, Vec<u8>)> {
        self.services
            .iter()
            .map(|service| {
                (
                    format!("{}:{}", service.app, service.name),
                    service.data.clone(),
                )
            })
            .collect()
    }
}

impl Message for NetworkDevice {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(1, &self.name);
        writer.varint(2, self.device_type);
        writer.bytes(3, &self.full_id);
        writer.bytes(4, &self.account_id);
        if let Some(version) = self.service_version {
            writer.varint(8, version);
        }
        for service in &self.services {
            writer.message(14, service);
        }
        if let Some(group) = &self.group {
            writer.message(15, group);
        }
        if let Some(region) = &self.region {
            writer.bytes(19, region);
        }
        if let Some(market) = &self.market_name {
            writer.bytes(20, market);
        }
        if let Some(medium) = self.supported_medium {
            writer.varint(21, medium);
        }
        if let Some(short_id) = &self.short_id {
            writer.bytes(23, short_id);
        }
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            name: fields.text(1),
            device_type: fields.uint(2),
            full_id: fields.bytes(3).to_vec(),
            account_id: fields.bytes(4).to_vec(),
            service_version: fields.has(8).then(|| fields.uint(8)),
            services: fields.repeated_message(14)?,
            group: fields.opt_message(15)?,
            region: fields.has(19).then(|| fields.text(19)),
            market_name: fields.has(20).then(|| fields.text(20)),
            supported_medium: fields.has(21).then(|| fields.uint(21)),
            short_id: fields.has(23).then(|| fields.text(23)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkingSync {
    pub version: u64,
    pub device: NetworkDevice,
}

impl Message for NetworkingSync {
    fn encode(&self) -> Vec<u8> {
        let mut sync = Writer::new();
        sync.varint(1, self.version);
        sync.message(2, &self.device);
        let mut writer = Writer::new();
        writer.varint(1, 1);
        writer.bytes(5, sync.finish());
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        let sync: Fields = fields.message(5)?;
        Ok(Self {
            version: sync.uint(1),
            device: sync.message(2)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkingReply {
    pub status: u64,
    pub version: u64,
    pub device: NetworkDevice,
}

impl Message for NetworkingReply {
    fn encode(&self) -> Vec<u8> {
        let mut reply = Writer::new();
        reply.opt_varint(1, self.status);
        reply.varint(2, self.version);
        reply.message(3, &self.device);
        let mut writer = Writer::new();
        writer.varint(1, 2);
        writer.bytes(6, reply.finish());
        writer.finish()
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        let reply: Fields = fields.message(6)?;
        Ok(Self {
            status: reply.uint(1),
            version: reply.uint(2),
            device: reply.message(3)?,
        })
    }
}

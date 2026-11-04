//! Typed Lyra physical envelopes.
use super::super::device::PhysicalDevice;
use crate::Result;
use crate::protocol::pb::{self, Fields, Message, Writer};
use anyhow::ensure;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PhysicalBody {
    SyncRequest(PhysicalSync),
    SyncReply(PhysicalSyncReply),
    Heartbeat(PhysicalHeartbeat),
    HeartbeatAck(PhysicalHeartbeatAck),
    Close(PhysicalClose),
    CloseAck(PhysicalCloseAck),
    Unknown {
        kind: u64,
        field: u32,
        body: Vec<u8>,
    },
}

impl PhysicalBody {
    pub(crate) fn kind(&self) -> u64 {
        match self {
            Self::SyncRequest(_) => 1,
            Self::SyncReply(_) => 2,
            Self::Heartbeat(_) => 4,
            Self::HeartbeatAck(_) => 5,
            Self::Close(_) => 6,
            Self::CloseAck(_) => 7,
            Self::Unknown { kind, .. } => *kind,
        }
    }

    fn field(&self) -> u32 {
        match self {
            Self::SyncRequest(_) => 3,
            Self::SyncReply(_) => 4,
            Self::Heartbeat(_) => 6,
            Self::HeartbeatAck(_) => 7,
            Self::Close(_) => 8,
            Self::CloseAck(_) => 9,
            Self::Unknown { field, .. } => *field,
        }
    }

    fn encode(&self) -> Vec<u8> {
        match self {
            Self::SyncRequest(message) => message.encode(),
            Self::SyncReply(message) => message.encode(),
            Self::Heartbeat(message) => message.encode(),
            Self::HeartbeatAck(message) => message.encode(),
            Self::Close(message) => message.encode(),
            Self::CloseAck(message) => message.encode(),
            Self::Unknown { body, .. } => body.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalFrame {
    pub id: u64,
    pub body: PhysicalBody,
}

impl PhysicalFrame {
    pub(crate) fn sync_request(timestamp: u64, device: PhysicalDevice, medium: u64) -> Self {
        Self {
            id: 1,
            body: PhysicalBody::SyncRequest(PhysicalSync {
                timestamp,
                device,
                medium,
                quick_connect: Vec::new(),
            }),
        }
    }

    pub(crate) fn sync_reply(timestamp: u64, device: PhysicalDevice, medium: u64) -> Self {
        Self {
            id: 0,
            body: PhysicalBody::SyncReply(PhysicalSyncReply {
                timestamp,
                device,
                medium,
            }),
        }
    }

    pub(crate) fn heartbeat(uptime_ms: u64, status: u64) -> Self {
        Self {
            id: 0,
            body: PhysicalBody::Heartbeat(PhysicalHeartbeat { uptime_ms, status }),
        }
    }

    pub(crate) fn heartbeat_ack(message: PhysicalHeartbeatAck) -> Self {
        Self {
            id: 0,
            body: PhysicalBody::HeartbeatAck(message),
        }
    }

    pub(crate) fn close(timestamp: u64) -> Self {
        Self {
            id: 0,
            body: PhysicalBody::Close(PhysicalClose { timestamp }),
        }
    }

    pub(crate) fn close_ack(timestamp: u64) -> Self {
        Self {
            id: 0,
            body: PhysicalBody::CloseAck(PhysicalCloseAck { timestamp }),
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let body = self.body.encode();
        let mut frame = Writer::new();
        if self.id != 0 {
            frame.varint(1, self.id);
        }
        frame.varint(2, self.body.kind());
        frame.bytes(self.body.field(), body);
        let mut envelope = Writer::new();
        envelope.bytes(2, frame.finish());
        let mut outer = Writer::new();
        outer.bytes(2, envelope.finish());
        pb::mesh(1, &outer.finish())
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let (mesh_kind, payload) = pb::unmesh(data)?;
        ensure!(mesh_kind == 1, "expected Physical mesh kind 1");
        let outer = Fields::decode(payload)?;
        let envelope = Fields::decode(outer.bytes(2))?;
        let frame = Fields::decode(envelope.bytes(2))?;
        let id = frame.uint(1);
        let kind = frame.uint(2);
        let (field, body) = match kind {
            1 => (3, frame.bytes(3).to_vec()),
            2 => (4, frame.bytes(4).to_vec()),
            4 => (6, frame.bytes(6).to_vec()),
            5 => (7, frame.bytes(7).to_vec()),
            6 => (8, frame.bytes(8).to_vec()),
            7 => (9, frame.bytes(9).to_vec()),
            _ => (0, Vec::new()),
        };
        let body = match kind {
            1 => PhysicalBody::SyncRequest(PhysicalSync::decode(&body)?),
            2 => PhysicalBody::SyncReply(PhysicalSyncReply::decode(&body)?),
            4 => PhysicalBody::Heartbeat(PhysicalHeartbeat::decode(&body)?),
            5 => PhysicalBody::HeartbeatAck(PhysicalHeartbeatAck::decode(&body)?),
            6 => PhysicalBody::Close(PhysicalClose::decode(&body)?),
            7 => PhysicalBody::CloseAck(PhysicalCloseAck::decode(&body)?),
            kind => PhysicalBody::Unknown { kind, field, body },
        };
        Ok(Self { id, body })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalSync {
    pub timestamp: u64,
    pub device: PhysicalDevice,
    pub medium: u64,
    pub quick_connect: Vec<u8>,
}

impl Message for PhysicalSync {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.timestamp);
        writer.message(2, &self.device);
        writer.varint(3, self.medium);
        writer.opt_bytes(4, &self.quick_connect);
        writer.varint(5, self.medium);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            timestamp: fields.uint(1),
            device: fields.message(2)?,
            medium: fields.uint(3),
            quick_connect: fields.bytes(4).to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalSyncReply {
    pub timestamp: u64,
    pub device: PhysicalDevice,
    pub medium: u64,
}

impl Message for PhysicalSyncReply {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.timestamp);
        writer.message(2, &self.device);
        writer.varint(3, self.medium);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            timestamp: fields.uint(1),
            device: fields.message(2)?,
            medium: fields.uint(3),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalHeartbeat {
    pub uptime_ms: u64,
    pub status: u64,
}

impl Message for PhysicalHeartbeat {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.uptime_ms);
        writer.varint(2, self.status);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            uptime_ms: fields.uint(1),
            status: fields.uint(2),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalHeartbeatAck {
    pub uptime_ms: u64,
    pub request_status: u64,
    pub request_uptime_ms: u64,
    pub request_field4: u64,
}

impl Message for PhysicalHeartbeatAck {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.uptime_ms);
        writer.varint(2, self.request_status);
        writer.varint(3, self.request_uptime_ms);
        writer.varint(4, self.request_field4);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            uptime_ms: fields.uint(1),
            request_status: fields.uint(2),
            request_uptime_ms: fields.uint(3),
            request_field4: fields.uint(4),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalClose {
    pub timestamp: u64,
}

impl Message for PhysicalClose {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.timestamp);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        Ok(Self {
            timestamp: Fields::decode(data)?.uint(1),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalCloseAck {
    pub timestamp: u64,
}

impl Message for PhysicalCloseAck {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.timestamp);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        Ok(Self {
            timestamp: Fields::decode(data)?.uint(1),
        })
    }
}

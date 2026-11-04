use crate::Result;
use crate::protocol::pb::{self, Fields, Message, Writer};
use anyhow::ensure;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LogicalBody {
    ServiceRequest {
        request: ServiceRequest,
        upgrade_query: Option<Vec<u8>>,
    },
    ServiceResponse(ServiceResponse),
    Ack {
        introduction: Option<Vec<u8>>,
    },
    Close(LogicalClose),
    Sync(LogicalSync),
    Auth(AuthExchange),
    Introduction(Vec<u8>),
    Unknown {
        kind: u64,
        body: Vec<u8>,
    },
}

fn fields_opt_query(inner: &Fields) -> Option<Vec<u8>> {
    inner.has(8).then(|| inner.bytes(8).to_vec())
}

impl LogicalBody {
    pub(crate) fn kind(&self) -> u64 {
        match self {
            Self::ServiceRequest { .. } => 1,
            Self::ServiceResponse(_) => 2,
            Self::Ack { .. } => 3,
            Self::Close(_) => 4,
            Self::Sync(_) => 5,
            Self::Auth(_) => 6,
            Self::Introduction(_) => 7,
            Self::Unknown { kind, .. } => *kind,
        }
    }

    pub(crate) fn introduction(&self) -> Option<&[u8]> {
        match self {
            Self::Ack {
                introduction: Some(introduction),
            }
            | Self::Introduction(introduction) => Some(introduction),
            _ => None,
        }
    }

    pub(crate) fn encode_inner(&self) -> Vec<u8> {
        let mut inner = Writer::new();
        inner.varint(1, self.kind());
        self.write(&mut inner);
        inner.finish()
    }

    pub(crate) fn decode_inner(data: &[u8]) -> Result<Self> {
        let inner = Fields::decode(data)?;
        let kind = inner.uint(1);
        Ok(match kind {
            1 => Self::ServiceRequest {
                request: inner.message(2)?,
                upgrade_query: fields_opt_query(&inner),
            },
            2 => Self::ServiceResponse(inner.message(3)?),
            3 => {
                let introduction = inner.bytes(8).to_vec();
                Self::Ack {
                    introduction: (!introduction.is_empty()).then_some(introduction),
                }
            }
            4 => Self::Close(inner.message(5)?),
            5 => Self::Sync(inner.message(6)?),
            6 => Self::Auth(inner.message(7)?),
            7 => Self::Introduction(inner.bytes(8).to_vec()),
            kind => Self::Unknown {
                kind,
                body: inner.bytes(2).to_vec(),
            },
        })
    }

    fn write(&self, inner: &mut Writer) {
        match self {
            Self::ServiceRequest {
                request,
                upgrade_query,
            } => {
                inner.bytes(2, request.encode());
                if let Some(query) = upgrade_query {
                    inner.bytes(8, query);
                }
            }
            Self::ServiceResponse(message) => inner.bytes(3, message.encode()),
            Self::Ack { introduction } => {
                inner.bytes(4, []);
                if let Some(introduction) = introduction {
                    inner.bytes(8, introduction);
                }
            }
            Self::Close(message) => inner.bytes(5, message.encode()),
            Self::Sync(message) => inner.bytes(6, message.encode()),
            Self::Auth(message) => inner.bytes(7, message.encode()),
            Self::Introduction(introduction) => inner.bytes(8, introduction),
            Self::Unknown { body, .. } => inner.bytes(2, body),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalFrame {
    pub local: u8,
    pub remote: u8,
    pub full: u32,
    pub encrypted: bool,
    pub body: LogicalBody,
}

impl LogicalFrame {
    pub(crate) fn encode(&self) -> Vec<u8> {
        LogicalEnvelope {
            local: self.local,
            remote: self.remote,
            full: self.full,
            encrypted: self.encrypted,
            body: self.body.encode_inner(),
        }
        .encode()
    }

    #[cfg(test)]
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let envelope = LogicalEnvelope::decode(data)?;
        Ok(Self {
            local: envelope.local,
            remote: envelope.remote,
            full: envelope.full,
            encrypted: envelope.encrypted,
            body: LogicalBody::decode_inner(&envelope.body)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalEnvelope {
    pub local: u8,
    pub remote: u8,
    pub full: u32,
    pub encrypted: bool,
    pub body: Vec<u8>,
}

impl LogicalEnvelope {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut frame = Writer::new();
        frame.varint(1, self.local as u64);
        if self.remote != 0 {
            frame.varint(2, self.remote as u64);
        }
        frame.varint(3, self.full as u64);
        if self.encrypted {
            frame.varint(4, 1);
        }
        frame.bytes(5, &self.body);
        let mut envelope = Writer::new();
        envelope.bytes(1, frame.finish());
        let mut outer = Writer::new();
        outer.bytes(2, envelope.finish());
        pb::mesh(2, &outer.finish())
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let (mesh_kind, payload) = pb::unmesh(data)?;
        ensure!(mesh_kind == 2, "expected Logical mesh kind 2");
        let outer = Fields::decode(payload)?;
        let envelope = Fields::decode(outer.bytes(2))?;
        let frame = Fields::decode(envelope.bytes(1))?;
        Ok(Self {
            local: frame.uint(1) as u8,
            remote: frame.uint(2) as u8,
            full: frame.uint(3) as u32,
            encrypted: frame.uint(4) != 0,
            body: frame.bytes(5).to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalSync {
    pub timeout_ms: u64,
    pub trust: u32,
    pub service: String,
    pub uid_feature: Vec<u8>,
    pub key_index: u64,
    pub cache: Vec<u8>,
}

impl Message for LogicalSync {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.timeout_ms);
        writer.varint(2, self.trust as u64);
        writer.opt_varint(3, self.key_index);
        writer.bytes(4, &self.service);
        writer.bytes(5, &self.uid_feature);
        writer.opt_bytes(8, &self.cache);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            timeout_ms: fields.uint(1),
            trust: fields.uint(2) as u32,
            service: fields.text(4),
            uid_feature: fields.bytes(5).to_vec(),
            key_index: fields.uint(3),
            cache: fields.bytes(8).to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserInfo {
    pub platform: u32,
    pub user_data: String,
}

impl Message for UserInfo {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.platform as u64);
        writer.bytes(2, b"org.xconnect.lab");
        writer.opt_bytes(4, self.user_data.as_bytes());
        writer.varint(6, 1);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            platform: fields.uint(1) as u32,
            user_data: fields.text(4),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceRequest {
    pub service: String,
    pub user_data: Vec<u8>,
    pub trust: u32,
    pub timeout_ms: u64,
    pub medium: Option<u64>,
}

impl Message for ServiceRequest {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, 1);
        writer.bytes(2, &self.service);
        writer.opt_bytes(3, &self.user_data);
        writer.varint(4, self.trust as u64);
        writer.varint(5, self.timeout_ms);
        if let Some(medium) = self.medium {
            writer.varint(6, medium);
        }
        writer.varint(7, 1);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            service: fields.text(2),
            user_data: fields.bytes(3).to_vec(),
            trust: fields.uint(4) as u32,
            timeout_ms: fields.uint(5),
            medium: fields.has(6).then(|| fields.uint(6)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceResponse {
    pub status: u64,
    pub user_data: Vec<u8>,
    pub version: Option<u64>,
}

impl Message for ServiceResponse {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.opt_varint(1, self.status);
        writer.opt_bytes(2, &self.user_data);
        if let Some(version) = self.version {
            writer.varint(3, version);
        }
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            status: fields.uint(1),
            user_data: fields.bytes(2).to_vec(),
            version: fields.has(3).then(|| fields.uint(3)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogicalClose {
    pub reason: u64,
}

impl Message for LogicalClose {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.reason);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        Ok(Self {
            reason: Fields::decode(data)?.uint(1),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthExchange {
    pub kind: u64,
    pub payload: Vec<u8>,
}

impl Message for AuthExchange {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.varint(1, self.kind);
        writer.bytes(2, &self.payload);
        writer.finish()
    }
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            kind: fields.uint(1),
            payload: fields.bytes(2).to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_frames_round_trip() -> Result<()> {
        let frame = LogicalFrame {
            local: 7,
            remote: 0,
            full: 1234,
            encrypted: false,
            body: LogicalBody::Ack {
                introduction: Some(vec![9, 8]),
            },
        };
        let encoded = frame.encode();
        assert_eq!(LogicalFrame::decode(&encoded)?, frame);
        Ok(())
    }
}

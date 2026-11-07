//! SDK control packets exchanged inside a logical channel.
use crate::Result;
use crate::protocol::pb::{Fields, Writer};
use anyhow::ensure;

pub(crate) const HEADER_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChannelBody {
    PeerPortRequest {
        id: u64,
        version: u64,
        full: u64,
        key: Vec<u8>,
    },
    PeerPortResponse {
        request_id: u64,
        sdk_id: u32,
        port: u16,
        version: u64,
    },
    Unknown(Vec<u8>),
}

/// Mesh wrapper for one authenticated SDK control payload.
pub(crate) fn wrap(remote: u8, encrypted: &[u8]) -> Vec<u8> {
    crate::protocol::pb::mesh(5, &[vec![remote, 1], encrypted.to_vec()].concat())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelControl {
    pub kind: u8,
    pub body: ChannelBody,
}

impl ChannelControl {
    pub(crate) fn peer_port_request(id: u64, full: u32, key: Vec<u8>) -> Self {
        Self {
            kind: 2,
            body: ChannelBody::PeerPortRequest {
                id,
                version: 1,
                full: full as u64,
                key,
            },
        }
    }

    pub(crate) fn peer_port_response(request_id: u64, sdk_id: u32, port: u16) -> Self {
        Self {
            kind: 3,
            body: ChannelBody::PeerPortResponse {
                request_id,
                sdk_id,
                port,
                version: 1,
            },
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let body = match &self.body {
            ChannelBody::PeerPortRequest {
                id,
                version,
                full,
                key,
            } => {
                let mut writer = Writer::new();
                writer.varint(1, *id);
                writer.varint(2, *version);
                writer.varint(3, *full);
                writer.bytes(4, key);
                writer.finish()
            }
            ChannelBody::PeerPortResponse {
                request_id,
                sdk_id,
                port,
                version,
            } => {
                let mut writer = Writer::new();
                writer.varint(1, *request_id);
                writer.varint(2, *sdk_id as u64);
                writer.varint(3, *port as u64);
                writer.varint(5, *version);
                writer.finish()
            }
            ChannelBody::Unknown(body) => body.clone(),
        };
        let mut out = vec![0u8; HEADER_LEN];
        out[0] = HEADER_LEN as u8;
        out[2] = self.kind;
        out[3] = HEADER_LEN as u8;
        out[4..6].copy_from_slice(&((body.len() + HEADER_LEN) as u16).to_be_bytes());
        out.extend(body);
        out
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() >= HEADER_LEN, "truncated SDK control header");
        let kind = data[2];
        let body = &data[HEADER_LEN..];
        let fields = Fields::decode(body)?;
        let body = match kind {
            2 => ChannelBody::PeerPortRequest {
                id: fields.uint(1),
                version: fields.uint(2),
                full: fields.uint(3),
                key: fields.bytes(4).to_vec(),
            },
            3 => ChannelBody::PeerPortResponse {
                request_id: fields.uint(1),
                sdk_id: fields.uint(2) as u32,
                port: fields.uint(3) as u16,
                version: fields.uint(5),
            },
            _ => ChannelBody::Unknown(body.to_vec()),
        };
        Ok(Self { kind, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_port_controls_round_trip() -> Result<()> {
        let request = ChannelControl::peer_port_request(42, 1234, vec![7; 32]);
        assert_eq!(ChannelControl::decode(&request.encode())?, request);
        let response = ChannelControl::peer_port_response(7, 42, 5000);
        assert_eq!(ChannelControl::decode(&response.encode())?, response);
        Ok(())
    }
}

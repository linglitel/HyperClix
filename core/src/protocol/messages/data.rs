//! Mesh-level data frames used by Networking and SDK channels.
use crate::Result;
use crate::protocol::pb::{self, Message};
use anyhow::ensure;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataKind {
    Networking,
    Sdk,
}

impl DataKind {
    fn mesh_kind(self) -> u8 {
        match self {
            Self::Networking => 4,
            Self::Sdk => 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataFrame {
    pub kind: DataKind,
    /// The receiving logical slot expected by the peer.
    pub slot: u8,
    pub encrypted: bool,
    pub payload: Vec<u8>,
}

impl DataFrame {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut body = vec![self.slot];
        if self.kind == DataKind::Sdk {
            body.push(u8::from(self.encrypted));
        }
        body.extend_from_slice(&self.payload);
        pb::mesh(self.kind.mesh_kind(), &body)
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let (mesh_kind, body) = pb::unmesh(data)?;
        let kind = match mesh_kind {
            4 => DataKind::Networking,
            5 => DataKind::Sdk,
            other => anyhow::bail!("not a data mesh frame: {other}"),
        };
        ensure!(!body.is_empty(), "truncated data frame");
        let slot = body[0];
        let (encrypted, payload) = match kind {
            DataKind::Networking => (true, body[1..].to_vec()),
            DataKind::Sdk => {
                ensure!(body.len() >= 2, "truncated SDK data frame");
                (body[1] & 1 != 0, body[2..].to_vec())
            }
        };
        Ok(Self {
            kind,
            slot,
            encrypted,
            payload,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkingPayload {
    pub module: u8,
    pub body: Vec<u8>,
}

impl Message for NetworkingPayload {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.module];
        out.extend_from_slice(&self.body);
        out
    }

    fn decode(data: &[u8]) -> Result<Self> {
        ensure!(!data.is_empty(), "empty Networking payload");
        Ok(Self {
            module: data[0],
            body: data[1..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::pb::{Fields, Writer};

    #[test]
    fn data_frames_round_trip() -> Result<()> {
        let frame = DataFrame {
            kind: DataKind::Sdk,
            slot: 7,
            encrypted: true,
            payload: vec![1, 2, 3],
        };
        assert_eq!(DataFrame::decode(&frame.encode())?, frame);
        Ok(())
    }

    #[test]
    fn networking_payload_round_trip() -> Result<()> {
        let payload = NetworkingPayload {
            module: 1,
            body: Fields::decode(&WriteHelper::bytes()).unwrap().encode(),
        };
        assert_eq!(NetworkingPayload::decode(&payload.encode())?, payload);
        Ok(())
    }

    struct WriteHelper;
    impl WriteHelper {
        fn bytes() -> Vec<u8> {
            let mut writer = Writer::new();
            writer.varint(1, 7);
            writer.finish()
        }
    }
}

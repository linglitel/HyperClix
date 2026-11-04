//! AccountPair / cached-certificate AuthHandshake envelopes.
use crate::Result;
use crate::protocol::pb::{Fields, Writer};
use anyhow::ensure;

#[derive(Clone, Debug)]
pub(crate) struct EncryptionSuite {
    pub id: u64,
    pub nonce: Vec<u8>,
    pub capability: u64,
    pub cipher: u64,
    pub point_kind: u64,
    pub point: Vec<u8>,
}

impl EncryptionSuite {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut point = Writer::new();
        point.varint(1, self.point_kind);
        point.bytes(2, &self.point);
        let mut writer = Writer::new();
        writer.varint(1, self.id);
        writer.bytes(2, &self.nonce);
        writer.varint(3, self.capability);
        writer.varint(4, self.cipher);
        writer.bytes(5, point.finish());
        writer.finish()
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        let point = fields.message::<Fields>(5)?;
        Ok(Self {
            id: fields.uint(1),
            nonce: fields.bytes(2).to_vec(),
            capability: fields.uint(3),
            cipher: fields.uint(4),
            point_kind: point.uint(1),
            point: point.bytes(2).to_vec(),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub certificate: Vec<u8>,
    pub signature: Vec<u8>,
}

impl Credential {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(1, &self.certificate);
        writer.bytes(2, &self.signature);
        writer.finish()
    }
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            certificate: fields.bytes(1).to_vec(),
            signature: fields.bytes(2).to_vec(),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum AuthFrame {
    PairClientNotify {
        suite: EncryptionSuite,
    },
    PairServerNotify {
        suite: EncryptionSuite,
        encrypted_credential: Vec<u8>,
    },
    PairClientFinished {
        encrypted_credential: Vec<u8>,
        encrypted_private_data: Option<Vec<u8>>,
    },
    PairServerFinished {
        encrypted_proof: Vec<u8>,
    },
    CertificateNotify {
        suite: EncryptionSuite,
        marker: u64,
    },
    CertificateReply {
        suite: EncryptionSuite,
        encrypted_signature: Vec<u8>,
    },
    CertificateFinished {
        encrypted_signature: Vec<u8>,
        private_data: Option<Vec<u8>>,
    },
    CertificateProof {
        encrypted_proof: Vec<u8>,
    },
}

impl AuthFrame {
    pub(crate) fn pair_client_notify(suite: EncryptionSuite) -> Self {
        Self::PairClientNotify { suite }
    }
    pub(crate) fn pair_server_notify(
        suite: EncryptionSuite,
        encrypted_credential: Vec<u8>,
    ) -> Self {
        Self::PairServerNotify {
            suite,
            encrypted_credential,
        }
    }
    pub(crate) fn pair_client_finished(
        encrypted_credential: Vec<u8>,
        encrypted_private_data: Option<Vec<u8>>,
    ) -> Self {
        Self::PairClientFinished {
            encrypted_credential,
            encrypted_private_data,
        }
    }
    pub(crate) fn pair_server_finished(encrypted_proof: Vec<u8>) -> Self {
        Self::PairServerFinished { encrypted_proof }
    }
    pub(crate) fn certificate_reply(suite: EncryptionSuite, encrypted_signature: Vec<u8>) -> Self {
        Self::CertificateReply {
            suite,
            encrypted_signature,
        }
    }
    pub(crate) fn certificate_proof(encrypted_proof: Vec<u8>) -> Self {
        Self::CertificateProof { encrypted_proof }
    }

    fn pair(&self) -> bool {
        matches!(
            self,
            Self::PairClientNotify { .. }
                | Self::PairServerNotify { .. }
                | Self::PairClientFinished { .. }
                | Self::PairServerFinished { .. }
        )
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let (kind, body) = match self {
            Self::PairClientNotify { suite } => {
                let mut body = Writer::new();
                body.bytes(1, suite.encode());
                body.bytes(2, &[8, 1]);
                body.bytes(3, &[8, 1]);
                (1, body.finish())
            }
            Self::PairServerNotify {
                suite,
                encrypted_credential,
            } => {
                let mut body = Writer::new();
                body.bytes(1, suite.encode());
                body.bytes(2, encrypted_credential);
                body.bytes(3, &[8, 1]);
                body.bytes(4, &[8, 1]);
                (2, body.finish())
            }
            Self::PairClientFinished {
                encrypted_credential,
                encrypted_private_data,
            } => {
                let mut body = Writer::new();
                body.bytes(1, encrypted_credential);
                if let Some(private_data) = encrypted_private_data {
                    body.bytes(2, private_data);
                }
                (3, body.finish())
            }
            Self::PairServerFinished { encrypted_proof } => {
                let mut body = Writer::new();
                body.bytes(1, encrypted_proof);
                (4, body.finish())
            }
            Self::CertificateNotify { suite, marker } => {
                let mut body = Writer::new();
                body.bytes(1, suite.encode());
                body.varint(2, *marker);
                (1, body.finish())
            }
            Self::CertificateReply {
                suite,
                encrypted_signature,
            } => {
                let mut body = Writer::new();
                body.bytes(1, suite.encode());
                body.bytes(2, encrypted_signature);
                body.bytes(4, &[8, 1]);
                body.bytes(5, &[8, 1]);
                (2, body.finish())
            }
            Self::CertificateFinished {
                encrypted_signature,
                private_data,
            } => {
                let mut body = Writer::new();
                body.bytes(1, encrypted_signature);
                if let Some(private_data) = private_data {
                    body.bytes(2, private_data);
                }
                (3, body.finish())
            }
            Self::CertificateProof { encrypted_proof } => {
                let mut body = Writer::new();
                body.bytes(1, encrypted_proof);
                (4, body.finish())
            }
        };
        let field = match kind {
            1 => 2,
            2 => 3,
            3 => 4,
            _ => 5,
        };
        let mut message = Writer::new();
        message.varint(1, kind);
        message.bytes(field, body);
        let mut envelope = Writer::new();
        if self.pair() {
            envelope.varint(1, 2);
            envelope.varint(2, 4);
            envelope.bytes(6, message.finish());
        } else {
            envelope.varint(1, 4);
            envelope.varint(2, 5);
            envelope.bytes(7, message.finish());
        }
        envelope.finish()
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        let outer = Fields::decode(data)?;
        ensure!(outer.uint(2) != 1, "对端拒绝账号认证");
        let pair = (outer.uint(1), outer.uint(2)) == (2, 4);
        let certificate = (outer.uint(1), outer.uint(2)) == (4, 5);
        ensure!(pair || certificate, "账号认证消息类型错误");
        let message = if pair {
            outer.message::<Fields>(6)?
        } else {
            outer.message::<Fields>(7)?
        };
        let kind = message.uint(1);
        let field = match kind {
            1 => 2,
            2 => 3,
            3 => 4,
            4 => 5,
            _ => anyhow::bail!("未知账号认证消息类型 {kind}"),
        };
        let body = message.message::<Fields>(field)?;
        Ok(match (pair, kind) {
            (true, 1) => Self::PairClientNotify {
                suite: EncryptionSuite::decode(body.bytes(1))?,
            },
            (true, 2) => Self::PairServerNotify {
                suite: EncryptionSuite::decode(body.bytes(1))?,
                encrypted_credential: body.bytes(2).to_vec(),
            },
            (true, 3) => Self::PairClientFinished {
                encrypted_credential: body.bytes(1).to_vec(),
                encrypted_private_data: body
                    .has(2)
                    .then(|| body.bytes(2).to_vec())
                    .filter(|v| !v.is_empty()),
            },
            (true, 4) => Self::PairServerFinished {
                encrypted_proof: body.bytes(1).to_vec(),
            },
            (false, 1) => Self::CertificateNotify {
                suite: EncryptionSuite::decode(body.bytes(1))?,
                marker: body.uint(2),
            },
            (false, 2) => Self::CertificateReply {
                suite: EncryptionSuite::decode(body.bytes(1))?,
                encrypted_signature: body.bytes(2).to_vec(),
            },
            (false, 3) => Self::CertificateFinished {
                encrypted_signature: body.bytes(1).to_vec(),
                private_data: body
                    .has(2)
                    .then(|| body.bytes(2).to_vec())
                    .filter(|v| !v.is_empty()),
            },
            (false, 4) => Self::CertificateProof {
                encrypted_proof: body.bytes(1).to_vec(),
            },
            _ => anyhow::bail!("未知账号认证消息"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::pb::{b, v};

    fn suite() -> EncryptionSuite {
        EncryptionSuite {
            id: 1,
            nonce: vec![0x11; 32],
            capability: 16,
            cipher: 8,
            point_kind: 1,
            point: vec![0x22; 65],
        }
    }

    #[test]
    fn pair_envelopes_match_wire_bytes() {
        let suite = suite();
        let suite_bytes = [
            v(1, 1),
            b(2, &suite.nonce),
            v(3, 16),
            v(4, 8),
            b(5, [v(1, 1), b(2, &suite.point)].concat()),
        ]
        .concat();
        let expected_notify = [
            v(1, 2),
            v(2, 4),
            b(
                6,
                [
                    v(1, 1),
                    b(
                        2,
                        [b(1, &suite_bytes), b(2, v(1, 1)), b(3, v(1, 1))].concat(),
                    ),
                ]
                .concat(),
            ),
        ]
        .concat();
        assert_eq!(
            AuthFrame::pair_client_notify(suite.clone()).encode(),
            expected_notify
        );

        let expected_reply = [
            v(1, 2),
            v(2, 4),
            b(
                6,
                [
                    v(1, 2),
                    b(
                        3,
                        [
                            b(1, &suite_bytes),
                            b(2, b"credential"),
                            b(3, v(1, 1)),
                            b(4, v(1, 1)),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        ]
        .concat();
        assert_eq!(
            AuthFrame::pair_server_notify(suite.clone(), b"credential".to_vec()).encode(),
            expected_reply
        );

        let expected_finished = [
            v(1, 2),
            v(2, 4),
            b(6, [v(1, 3), b(4, [b(1, b"credential")].concat())].concat()),
        ]
        .concat();
        assert_eq!(
            AuthFrame::pair_client_finished(b"credential".to_vec(), None).encode(),
            expected_finished
        );

        let credential = Credential {
            certificate: b"cert".to_vec(),
            signature: b"sig".to_vec(),
        };
        assert_eq!(credential.encode(), [b(1, b"cert"), b(2, b"sig")].concat());
    }

    #[test]
    fn certificate_reply_matches_wire_bytes() {
        let suite = suite();
        let suite_bytes = suite.encode();
        let expected = [
            v(1, 4),
            v(2, 5),
            b(
                7,
                [
                    v(1, 2),
                    b(
                        3,
                        [
                            b(1, &suite_bytes),
                            b(2, b"signature"),
                            b(4, v(1, 1)),
                            b(5, v(1, 1)),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        ]
        .concat();
        assert_eq!(
            AuthFrame::certificate_reply(suite, b"signature".to_vec()).encode(),
            expected
        );
    }
}

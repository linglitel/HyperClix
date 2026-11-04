//! AccountPair and cached certificate AuthHandshake, using the recovered suites.
use crate::protocol::messages::auth::{AuthFrame, Credential, EncryptionSuite};
use crate::{
    Result,
    common::crypto::{aes_decrypt, aes_encrypt},
    identity::Account,
};
use anyhow::{Context, ensure};
use openssl::{
    bn::BigNumContext,
    derive::Deriver,
    ec::{EcGroup, EcKey, EcPoint, PointConversionForm},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private, Public},
    sign::{Signer, Verifier},
    x509::X509,
};
use rand::RngCore;
use sha2::Sha256;

const PAIR_SALT: &str = "329bfc53393655d75ab08398ca9191effaa337f2e0beb573b1f9a3d015576480";
const AUTH_SALT: &str = "5ed5a3f836f6b54f7b1efad02714d5177b8a1f0f19e369cc0be8d98ba6297317";
const PAIR_CAPABILITY: u64 = 16;
const PAIR_CIPHER: u64 = 8;
const CERTIFICATE_CAPABILITY: u64 = 64;
const CERTIFICATE_CIPHER: u64 = 2;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    PairClient,
    PairServer,
    CertificateServer,
}
pub struct Auth {
    account: Account,
    mode: Mode,
    state: u8,
    ephemeral: PKey<Private>,
    point: Vec<u8>,
    nonce: Vec<u8>,
    peer_point: Vec<u8>,
    peer_nonce: Vec<u8>,
    shared: Vec<u8>,
    pending: Vec<u8>,
    private_data: Option<Vec<u8>>,
    pub key: Option<Vec<u8>>,
    pub peer_key: Option<PKey<Public>>,
    pub peer_short_id: String,
    expected_short: Option<String>,
    peer_type: u32,
}
impl Auth {
    fn new(
        account: &Account,
        mode: Mode,
        peer_type: u32,
        expected_short: Option<String>,
    ) -> Result<Self> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let ec = EcKey::generate(&group)?;
        let mut context = BigNumContext::new()?;
        let point =
            ec.public_key()
                .to_bytes(&group, PointConversionForm::UNCOMPRESSED, &mut context)?;
        let ephemeral = PKey::from_ec_key(ec)?;
        let mut nonce = vec![0; 32];
        rand::thread_rng().fill_bytes(&mut nonce);
        Ok(Self {
            account: account.clone(),
            mode,
            state: 0,
            ephemeral,
            point,
            nonce,
            peer_point: Vec::new(),
            peer_nonce: Vec::new(),
            shared: Vec::new(),
            pending: Vec::new(),
            private_data: None,
            key: None,
            peer_key: None,
            peer_short_id: String::new(),
            expected_short,
            peer_type,
        })
    }
    pub fn client(account: &Account, peer_type: u32) -> Result<Self> {
        Self::new(account, Mode::PairClient, peer_type, None)
    }
    pub fn server(account: &Account, peer_type: u32, expected_short: String) -> Result<Self> {
        Self::new(account, Mode::PairServer, peer_type, Some(expected_short))
    }
    pub fn certificate_server(
        account: &Account,
        peer_type: u32,
        short: String,
        peer_key: PKey<Public>,
    ) -> Result<Self> {
        let mut a = Self::new(
            account,
            Mode::CertificateServer,
            peer_type,
            Some(short.clone()),
        )?;
        a.peer_key = Some(peer_key);
        a.peer_short_id = short;
        Ok(a)
    }
    pub fn complete(&self) -> bool {
        self.key.is_some()
    }
    pub fn take_private_data(&mut self) -> Option<Vec<u8>> {
        self.private_data.take()
    }
    pub fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        aes_encrypt(self.key.as_ref().context("账号认证尚未完成")?, data)
    }
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        aes_decrypt(self.key.as_ref().context("账号认证尚未完成")?, data)
    }
    fn suite(&self, capability: u64, cipher: u64) -> EncryptionSuite {
        EncryptionSuite {
            id: 1,
            nonce: self.nonce.clone(),
            capability,
            cipher,
            point_kind: 1,
            point: self.point.clone(),
        }
    }
    pub fn client_notify(&self) -> Vec<u8> {
        AuthFrame::pair_client_notify(self.suite(PAIR_CAPABILITY, PAIR_CIPHER)).encode()
    }
    fn exchange(&mut self, suite: EncryptionSuite, capability: u64, cipher: u64) -> Result<()> {
        ensure!(
            suite.id == 1
                && suite.capability & capability == capability
                && suite.cipher & cipher == cipher
                && suite.point_kind == 1,
            "手机返回了未恢复的账号加密套件"
        );
        self.peer_nonce = suite.nonce;
        ensure!(self.peer_nonce.len() == 32, "认证nonce长度错误");
        self.peer_point = suite.point;
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let mut context = BigNumContext::new()?;
        let point = EcPoint::from_bytes(&group, &self.peer_point, &mut context)?;
        let peer = PKey::from_ec_key(EcKey::from_public_key(&group, &point)?)?;
        let mut deriver = Deriver::new(&self.ephemeral)?;
        deriver.set_peer(&peer)?;
        self.shared = deriver.derive_to_vec()?;
        Ok(())
    }
    fn signature(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut signer = Signer::new(MessageDigest::sha256(), &self.account.pkey)?;
        signer.update(data)?;
        Ok(signer.sign_to_vec()?)
    }
    fn own_credential(&self) -> Result<Vec<u8>> {
        Ok(Credential {
            certificate: self.account.cert3.to_der()?,
            signature: self.signature(&joined(&[&self.point, &self.peer_point]))?,
        }
        .encode())
    }
    fn verify_credential(&mut self, data: &[u8]) -> Result<()> {
        let cred = Credential::decode(data)?;
        let leaf = X509::from_der(&cred.certificate)?;
        let short = self.account.validate_peer_certificate(
            &leaf,
            self.peer_type,
            self.expected_short.as_deref(),
        )?;
        let public = leaf.public_key()?;
        verify(
            &public,
            &cred.signature,
            &joined(&[&self.peer_point, &self.point]),
        )?;
        self.account
            .cache_peer_certificate(&leaf, &short, self.peer_type)?;
        tracing::info!(
            "[auth] verified peer certificate cached: peer_type={}",
            self.peer_type
        );
        self.peer_short_id = short;
        self.peer_key = Some(public);
        Ok(())
    }
    pub fn handle(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>> {
        let frame = AuthFrame::decode(data)?;
        if self.mode == Mode::CertificateServer {
            self.handle_certificate(frame)
        } else {
            self.handle_pair(frame)
        }
    }
    fn handle_pair(&mut self, frame: AuthFrame) -> Result<Option<Vec<u8>>> {
        match (self.mode, self.state, frame) {
            (
                Mode::PairClient,
                0,
                AuthFrame::PairServerNotify {
                    suite,
                    encrypted_credential,
                },
            ) => {
                self.exchange(suite, PAIR_CAPABILITY, PAIR_CIPHER)?;
                let credential = aes_decrypt(&self.shared, &encrypted_credential)?;
                self.verify_credential(&credential)?;
                self.pending = derive(
                    &self.shared,
                    &joined(&[&self.nonce, &self.peer_nonce]),
                    PAIR_SALT,
                )?;
                self.state = 1;
                Ok(Some(
                    AuthFrame::pair_client_finished(
                        aes_encrypt(&self.shared, &self.own_credential()?)?,
                        None,
                    )
                    .encode(),
                ))
            }
            (Mode::PairClient, 1, AuthFrame::PairServerFinished { encrypted_proof }) => {
                let proof = aes_decrypt(&self.pending, &encrypted_proof)?;
                ensure!(
                    openssl::memcmp::eq(&proof, &joined(&[&self.shared, &self.peer_point])),
                    "账号认证完成证明不匹配"
                );
                self.key = Some(self.pending.clone());
                self.state = 2;
                Ok(None)
            }
            (Mode::PairServer, 0, AuthFrame::PairClientNotify { suite }) => {
                self.exchange(suite, PAIR_CAPABILITY, PAIR_CIPHER)?;
                self.pending = derive(
                    &self.shared,
                    &joined(&[&self.peer_nonce, &self.nonce]),
                    PAIR_SALT,
                )?;
                self.state = 1;
                Ok(Some(
                    AuthFrame::pair_server_notify(
                        self.suite(PAIR_CAPABILITY, PAIR_CIPHER),
                        aes_encrypt(&self.shared, &self.own_credential()?)?,
                    )
                    .encode(),
                ))
            }
            (
                Mode::PairServer,
                1,
                AuthFrame::PairClientFinished {
                    encrypted_credential,
                    encrypted_private_data,
                },
            ) => {
                let credential = aes_decrypt(&self.shared, &encrypted_credential)?;
                self.verify_credential(&credential)?;
                if let Some(private_data) = encrypted_private_data {
                    self.private_data = Some(aes_decrypt(&self.pending, &private_data)?);
                }
                self.key = Some(self.pending.clone());
                self.state = 2;
                Ok(Some(
                    AuthFrame::pair_server_finished(aes_encrypt(
                        &self.pending,
                        &joined(&[&self.shared, &self.point]),
                    )?)
                    .encode(),
                ))
            }
            (_, _, _) => anyhow::bail!("账号认证状态{}收到不匹配消息", self.state),
        }
    }
    fn handle_certificate(&mut self, frame: AuthFrame) -> Result<Option<Vec<u8>>> {
        match (self.state, frame) {
            (0, AuthFrame::CertificateNotify { suite, marker }) => {
                ensure!(marker == 4, "缓存证书认证类型错误");
                self.exchange(suite, CERTIFICATE_CAPABILITY, CERTIFICATE_CIPHER)?;
                let signature = self.signature(&joined(&[&self.point, &self.peer_point]))?;
                self.state = 1;
                Ok(Some(
                    AuthFrame::certificate_reply(
                        self.suite(CERTIFICATE_CAPABILITY, CERTIFICATE_CIPHER),
                        aes_encrypt(&self.shared, &signature)?,
                    )
                    .encode(),
                ))
            }
            (
                1,
                AuthFrame::CertificateFinished {
                    encrypted_signature,
                    private_data,
                },
            ) => {
                let signature = aes_decrypt(&self.shared, &encrypted_signature)?;
                verify(
                    self.peer_key.as_ref().context("缺少已配对的对端公钥")?,
                    &signature,
                    &joined(&[&self.peer_point, &self.point]),
                )?;
                self.pending = derive(
                    &self.shared,
                    &joined(&[&self.peer_nonce, &self.nonce]),
                    AUTH_SALT,
                )?;
                if let Some(private_data) = private_data {
                    self.private_data = Some(aes_decrypt(&self.pending, &private_data)?);
                }
                self.key = Some(self.pending.clone());
                self.state = 2;
                Ok(Some(
                    AuthFrame::certificate_proof(aes_encrypt(
                        &self.pending,
                        &joined(&[&self.shared, &self.point]),
                    )?)
                    .encode(),
                ))
            }
            (_, _) => anyhow::bail!("缓存证书认证状态{}收到消息", self.state),
        }
    }
}
fn derive(shared: &[u8], info: &[u8], salt: &str) -> Result<Vec<u8>> {
    let salt = hex::decode(salt)?;
    let mut key = vec![0; 32];
    hkdf::Hkdf::<Sha256>::new(Some(&salt), shared)
        .expand(info, &mut key)
        .map_err(|_| anyhow::anyhow!("HKDF派生失败"))?;
    Ok(key)
}
fn verify(public: &PKey<Public>, signature: &[u8], data: &[u8]) -> Result<()> {
    let mut verifier = Verifier::new(MessageDigest::sha256(), public)?;
    verifier.update(data)?;
    ensure!(verifier.verify(signature)?, "账号认证签名无效");
    Ok(())
}

fn joined(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|part| part.len()).sum());
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

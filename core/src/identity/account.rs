//! This computer's Xiaomi Lyra identity and device certificate.
use super::{
    certificate::{device_subject, validate_dates},
    http::{account_post, required},
};
use crate::{
    Result,
    common::{storage::save_private, time::epoch_secs},
};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{
    asn1::Asn1Type,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private, Public},
    x509::{X509, X509NameBuilder, X509Req},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const DEFAULT_ACCOUNT_DIR: &str = "artifacts/hyperclix/account-session";

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub sid: String,
    pub user_id: String,
    pub c_user_id: String,
    pub security: String,
    pub service_token: String,
}
impl Credentials {
    pub async fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let bytes = tokio::fs::read(directory.as_ref().join("credentials.json"))
            .await
            .context("读取账号凭据")?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub fn cookie(&self) -> String {
        format!(
            "serviceToken={}; cUserId={}",
            self.service_token, self.c_user_id
        )
    }
}

#[derive(Clone)]
pub struct Account {
    pub directory: PathBuf,
    pub uid: String,
    pub device_id: String,
    pub device_type: u32,
    pub pkey: PKey<Private>,
    pub cert2: X509,
    pub cert3: X509,
}

impl Account {
    /// Load and verify the existing own-computer identity. No network activity.
    pub async fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        let credentials = Credentials::load(&directory).await?;
        ensure!(
            credentials.sid == "miconnect",
            "设备证书需要 miconnect 账号目录"
        );
        let device: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(directory.join("device.json")).await?)?;
        let device_id = required(&device, "device_id")?.to_string();
        let device_type = device["device_type"].as_u64().context("device_type 缺失")? as u32;
        let pkey =
            PKey::private_key_from_pem(&tokio::fs::read(directory.join("device-key.pem")).await?)?;
        let cert2 = X509::from_pem(&tokio::fs::read(directory.join("cert2.pem")).await?)?;
        let cert3 = X509::from_pem(&tokio::fs::read(directory.join("cert3.pem")).await?)?;
        let account = Self {
            directory,
            uid: credentials.user_id,
            device_id,
            device_type,
            pkey,
            cert2,
            cert3,
        };
        account.validate()?;
        Ok(account)
    }

    pub fn short_id(&self) -> String {
        hex::encode_upper(Sha256::digest(self.device_id.as_bytes()))[..8].to_string()
    }
    pub fn full_id(&self) -> String {
        hex::encode_upper(Sha256::digest(self.device_id.as_bytes()))
    }
    pub fn account_id(&self) -> String {
        hex::encode_upper(Sha256::digest(self.uid.as_bytes()))
    }

    pub(crate) fn validate_peer_certificate(
        &self,
        cert: &X509,
        peer_type: u32,
        expected_short: Option<&str>,
    ) -> Result<String> {
        validate_dates(cert)?;
        let account_public = self.cert2.public_key()?;
        ensure!(cert.verify(&account_public)?, "对端证书账号签名无效");
        let cn = cert
            .subject_name()
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .context("对端证书缺CN")?
            .data()
            .to_string()?;
        let parts: Vec<_> = cn.split('.').collect();
        ensure!(
            (parts.len() == 4 || parts.len() == 5)
                && parts[0] == "lyra"
                && parts[3].parse::<u32>()? == peer_type,
            "对端证书设备类型错误"
        );
        ensure!(
            parts[1]
                == URL_SAFE_NO_PAD.encode(Sha256::digest(
                    format!("{}{}", parts[2], self.uid).as_bytes()
                )),
            "对端不是同一小米账号"
        );
        ensure!(
            parts.len() != 5 || parts[4].parse::<u32>()? & 1 == 0,
            "对端要求未恢复的IoT证书分支"
        );
        let short = hex::encode_upper(URL_SAFE_NO_PAD.decode(parts[2])?);
        ensure!(short.len() >= 8, "证书设备hash太短");
        let short = short[..8].to_owned();
        if let Some(expected) = expected_short {
            ensure!(expected == short, "对端证书与Physical身份不一致");
        }
        Ok(short)
    }

    fn peer_certificate_path(&self, peer_short: &str, peer_type: u32) -> Result<PathBuf> {
        ensure!(
            peer_short.len() == 8 && peer_short.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "手机证书缓存身份无效"
        );
        Ok(self
            .directory
            .join("peer-certificates")
            .join(format!("{peer_short}-{peer_type}.pem")))
    }

    // Called only after the peer's account certificate and handshake proof pass.
    pub(crate) fn cache_peer_certificate(
        &self,
        cert: &X509,
        peer_short: &str,
        peer_type: u32,
    ) -> Result<()> {
        let path = self.peer_certificate_path(peer_short, peer_type)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path.parent().unwrap())?;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&cert.to_pem()?)?;
            std::fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("保存已验证手机证书缓存")
    }

    pub(crate) fn load_peer_public_key(
        &self,
        peer_short: &str,
        peer_type: u32,
    ) -> Result<Option<PKey<Public>>> {
        let path = self.peer_certificate_path(peer_short, peer_type)?;
        let pem = match std::fs::read(path) {
            Ok(pem) => pem,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("读取手机证书缓存"),
        };
        let cert = X509::from_pem(&pem).context("解析手机证书缓存")?;
        self.validate_peer_certificate(&cert, peer_type, Some(peer_short))
            .context("手机证书缓存身份验证失败")?;
        Ok(Some(cert.public_key()?))
    }

    fn validate(&self) -> Result<()> {
        for cert in [&self.cert2, &self.cert3] {
            validate_dates(cert)?;
        }
        let name = self
            .cert3
            .subject_name()
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .context("设备证书缺 CN")?
            .data()
            .to_string()?;
        ensure!(
            name == device_subject(&self.uid, &self.device_id, self.device_type),
            "设备证书与本机身份不匹配"
        );
        ensure!(
            self.cert3.public_key()?.public_eq(&self.pkey),
            "设备证书与本机私钥不匹配"
        );
        let account_public = self.cert2.public_key()?;
        ensure!(self.cert3.verify(&account_public)?, "设备证书账号签名无效");
        Ok(())
    }

    /// Issue/refresh this computer's certificate after explicit QR authorization.
    pub async fn register(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        let credentials = Credentials::load(directory).await?;
        ensure!(
            credentials.sid == "miconnect",
            "设备登记需要 miconnect 凭据"
        );
        let client = Client::new();
        let cert2 = account_post(
            &client,
            &credentials,
            "/appgateway/miot/miconnect_server/MiconnectService/common/lv2Cert",
            b"{}",
        )
        .await?;
        save_private(
            &directory.join("cert2.pem"),
            required(&cert2, "cert")?.as_bytes(),
        )
        .await?;
        let device_path = directory.join("device.json");
        if !device_path.exists() {
            let value = serde_json::json!({"device_id":format!("linux-{}",uuid::Uuid::new_v4().simple()),"device_type":21,"created_epoch":epoch_secs()});
            save_private(&device_path, &serde_json::to_vec_pretty(&value)?).await?;
        }
        let device: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&device_path).await?)?;
        let device_id = required(&device, "device_id")?;
        let device_type = device["device_type"].as_u64().context("device_type 缺失")? as u32;
        let key_path = directory.join("device-key.pem");
        let pkey = if key_path.exists() {
            PKey::private_key_from_pem(&tokio::fs::read(&key_path).await?)?
        } else {
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
            let pkey = PKey::from_ec_key(EcKey::generate(&group)?)?;
            save_private(&key_path, &pkey.private_key_to_pem_pkcs8()?).await?;
            pkey
        };
        let mut subject = X509NameBuilder::new()?;
        // The native CN exceeds X.520's 64-character display-string bound.
        subject.append_entry_by_nid_with_type(
            Nid::COMMONNAME,
            &device_subject(&credentials.user_id, device_id, device_type),
            Asn1Type::UTF8STRING,
        )?;
        let mut csr = X509Req::builder()?;
        csr.set_version(0)?;
        csr.set_subject_name(&subject.build())?;
        csr.set_pubkey(&pkey)?;
        csr.sign(&pkey, MessageDigest::sha256())?;
        let csr = String::from_utf8(csr.build().to_pem()?)?;
        save_private(&directory.join("device.csr.pem"), csr.as_bytes()).await?;
        let payload = serde_json::json!({"did":URL_SAFE_NO_PAD.encode(Sha256::digest(device_id.as_bytes())),"deviceType":device_type,"csr":csr});
        let mut json = serde_json::to_vec(&payload)?;
        json.push(b'\n');
        let cert3 = account_post(
            &client,
            &credentials,
            "/appgateway/miot/miconnect_server/MiconnectService/common/lyra/getCert",
            &json,
        )
        .await?;
        save_private(
            &directory.join("cert3.pem"),
            required(&cert3, "cert")?.as_bytes(),
        )
        .await?;
        Self::load(directory).await
    }
}

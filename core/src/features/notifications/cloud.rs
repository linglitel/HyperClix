//! Micloud notification subscription and encrypted cloud requests.
use super::{PushRuntime, short_id};
use crate::{
    Result,
    common::storage::{json_path, private_json},
    identity::Account,
    session::Session,
};
use anyhow::{Context, anyhow, bail};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE},
};
use openssl::{
    sha::sha1,
    symm::{Cipher, decrypt, encrypt},
};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::{collections::BTreeMap, path::Path, time::Duration};

#[derive(Clone, Deserialize)]
pub struct CloudCredentials {
    pub sid: String,
    pub user_id: String,
    pub security: String,
    pub c_user_id: String,
    pub service_token: String,
}
impl CloudCredentials {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let value: Self = serde_json::from_slice(
            &tokio::fs::read(json_path(path.as_ref(), "credentials.json")).await?,
        )?;
        anyhow::ensure!(
            value.sid == "micloud",
            "Notification cloud needs the separate micloud authorization"
        );
        anyhow::ensure!(
            !value.user_id.is_empty(),
            "通知云凭据缺少账号身份，请授权当前互联账号的 micloud 服务"
        );
        Ok(value)
    }
    /// Different service tokens are required, but they must belong to the
    /// same raw Xiaomi account UID as the connected computer identity.
    pub fn require_account(&self, account: &Account) -> Result<()> {
        anyhow::ensure!(
            self.user_id == account.uid,
            "通知云授权不是当前互联账号，请为当前账号授权 micloud 服务"
        );
        Ok(())
    }
}
const CLOUD_BASE: &str = "https://cn.interconnectapi.micloud.xiaomi.net/mic/interconnect/v1/";
const CLOUD_IV: &[u8; 16] = b"0102030405060708";

#[derive(Clone, Debug)]
pub struct CloudClientInfo {
    pub client_type: String,
    pub push_type: String,
    pub os_version: String,
    pub app_version: String,
    pub region: String,
}
impl Default for CloudClientInfo {
    fn default() -> Self {
        Self {
            client_type: "0".into(),
            push_type: "0".into(),
            os_version: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
                .into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            region: "China".into(),
        }
    }
}
fn cipher_for(key: &[u8]) -> Result<Cipher> {
    match key.len() {
        16 => Ok(Cipher::aes_128_cbc()),
        24 => Ok(Cipher::aes_192_cbc()),
        32 => Ok(Cipher::aes_256_cbc()),
        _ => bail!("Unsupported notification cloud AES key length"),
    }
}
pub fn encrypt_cloud_value(value: &str, security: &str) -> Result<String> {
    let key = STANDARD.decode(security)?;
    Ok(URL_SAFE.encode(encrypt(
        cipher_for(&key)?,
        &key,
        Some(CLOUD_IV),
        value.as_bytes(),
    )?))
}
pub fn decrypt_cloud_response(value: &str, security: &str) -> Result<String> {
    let key = STANDARD.decode(security)?;
    Ok(String::from_utf8(decrypt(
        cipher_for(&key)?,
        &key,
        Some(CLOUD_IV),
        &STANDARD.decode(value)?,
    )?)?)
}
async fn cloud_post(
    path: &str,
    values: BTreeMap<String, String>,
    credentials: &CloudCredentials,
) -> Result<Json> {
    anyhow::ensure!(
        credentials.sid == "micloud",
        "Notification cloud needs micloud authorization"
    );
    let url = format!("{CLOUD_BASE}{path}");
    let mut encrypted = BTreeMap::new();
    for (k, v) in values {
        encrypted.insert(k, encrypt_cloud_value(&v, &credentials.security)?);
    }
    let mut parts = vec!["POST".into(), format!("/mic/interconnect/v1/{path}")];
    parts.extend(encrypted.iter().map(|(k, v)| format!("{k}={v}")));
    parts.push(credentials.security.clone());
    encrypted.insert(
        "signature".into(),
        STANDARD.encode(sha1(parts.join("&").as_bytes())),
    );
    let response = reqwest::Client::new()
        .post(url)
        .header(
            "Cookie",
            format!(
                "cUserId={}; serviceToken={}",
                credentials.c_user_id, credentials.service_token
            ),
        )
        .header("Accept-Encoding", "application/json")
        .header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .timeout(Duration::from_secs(20))
        .form(&encrypted)
        .send()
        .await
        .map_err(cloud_http_error)?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(cloud_http_error)?;
    decode_cloud_result(path, status, &body, credentials)
}
fn cloud_http_error(error: reqwest::Error) -> anyhow::Error {
    anyhow!("通知云请求失败：{}", error.without_url())
}
/// Cloud HTTP/business failure, without cookies or a response body.
#[derive(Debug)]
pub struct CloudRequestError {
    pub http_status: u16,
    pub business_code: Option<i64>,
    pub operation: String,
}
impl std::fmt::Display for CloudRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self
            .business_code
            .map(|v| v.to_string())
            .unwrap_or_else(|| "未提供".into());
        if self.http_status == 401 {
            write!(
                f,
                "通知云 {}：HTTP 401，业务 code={code}；当前 micloud 云授权被拒绝",
                self.operation
            )
        } else {
            write!(
                f,
                "通知云 {}：HTTP {}，业务 code={code}",
                self.operation, self.http_status
            )
        }
    }
}
impl std::error::Error for CloudRequestError {}
/// Remains accurate when a caller adds anyhow context around the cloud error.
pub fn is_authorization_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CloudRequestError>()
        .is_some_and(|error| error.http_status == 401)
}
pub(super) fn decode_cloud_result(
    path: &str,
    status: u16,
    response: &str,
    credentials: &CloudCredentials,
) -> Result<Json> {
    let decoded: Result<Json> = (|| {
        let plain = if response.trim_start().starts_with('{') {
            response.to_owned()
        } else {
            decrypt_cloud_response(response.trim(), &credentials.security).with_context(|| {
                format!("通知云 {path}：HTTP {status}，响应无法按当前 micloud security 解码")
            })?
        };
        Ok(serde_json::from_str(&plain)
            .with_context(|| format!("通知云 {path}：HTTP {status}，响应不是有效JSON"))?)
    })();
    if !(200..300).contains(&status) {
        let business_code = decoded
            .as_ref()
            .ok()
            .and_then(|result| result["code"].as_i64());
        return Err(CloudRequestError {
            http_status: status,
            business_code,
            operation: path.into(),
        }
        .into());
    }
    let result = decoded?;
    let code = result["code"].as_i64();
    if code != Some(0) {
        return Err(CloudRequestError {
            http_status: status,
            business_code: code,
            operation: path.into(),
        }
        .into());
    }
    Ok(result)
}

pub async fn register_cloud(
    session: &Session,
    credentials: &CloudCredentials,
    runtime: &PushRuntime,
    state: impl AsRef<Path>,
    info: CloudClientInfo,
) -> Result<Json> {
    credentials.require_account(session.account())?;
    anyhow::ensure!(
        !runtime.reg_id.is_empty(),
        "Register the MiPush application before registering notification cloud"
    );
    let account_device_id = own_cloud_identity(state.as_ref()).await?;
    let client = json!({"type":info.client_type,"spType":info.push_type,"os":"Linux","osVersion":info.os_version,"appVersion":info.app_version,"region":info.region});
    cloud_post(
        "push/register",
        BTreeMap::from([
            ("pushId".into(), runtime.reg_id.clone()),
            ("deviceId".into(), account_device_id),
            ("hcDid".into(), short_id(&session.account().device_id)),
            ("registerDeviceType".into(), "1".into()),
            ("clientInfoStr".into(), serde_json::to_string(&client)?),
        ]),
        credentials,
    )
    .await
}
pub async fn subscribe_cloud(
    session: &Session,
    credentials: &CloudCredentials,
    packages: &[String],
) -> Result<Json> {
    credentials.require_account(session.account())?;
    anyhow::ensure!(
        !session.peer_short_id().is_empty(),
        "手机 Networking 缺少通知订阅目标 ID"
    );
    session.report_phase("通知：建立云端应用订阅");
    let result = cloud_post(
        "notification/trans/subscribe",
        BTreeMap::from([
            ("hcDid".into(), short_id(&session.account().device_id)),
            ("targetHcDid".into(), session.peer_short_id()),
            ("appIdList".into(), packages.join(",")),
        ]),
        credentials,
    )
    .await?;
    session.report_phase("通知：云端应用订阅已确认");
    Ok(result)
}
async fn own_cloud_identity(directory: &Path) -> Result<String> {
    let path = directory.join("account-device.private.json");
    if path.exists() {
        let value: Json = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        let id = value["account_device_id"]
            .as_str()
            .context("Notification cloud state has no Passport identity")?;
        anyhow::ensure!(
            id.len() == 16,
            "Legacy Passport identity needs explicit migration using the existing reference tool"
        );
        return Ok(id.into());
    }
    let raw = format!("android_{}", uuid::Uuid::new_v4());
    let id = URL_SAFE.encode(sha1(raw.as_bytes()))[..16].to_owned();
    private_json(&path,&json!({"raw_account_device_id":raw,"account_device_id":id,"account_device_id_source":5,"account_device_id_scheme":"sha1-base64url-prefix16"})).await?;
    Ok(id)
}

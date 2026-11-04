//! Xiaomi account HTTP helpers.
use super::account::Credentials;
use crate::{
    Result,
    common::{
        crypto::{aes_decrypt, aes_encrypt},
        time::epoch_secs,
    },
};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(crate) fn required<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("账号响应缺少字段 {key}"))
}
fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0; n];
    rand::thread_rng().fill_bytes(&mut b);
    b
}
fn escape(s: &str) -> String {
    let mut out = String::new();
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char)
        } else {
            out.push_str(&format!("%{byte:02X}"))
        }
    }
    out
}
pub(super) async fn account_post(
    client: &Client,
    c: &Credentials,
    path: &str,
    data: &[u8],
) -> Result<serde_json::Value> {
    let nonce = [
        random(8),
        ((epoch_secs() / 60) as u32).to_be_bytes().to_vec(),
    ]
    .concat();
    let key = Sha256::digest([STANDARD.decode(&c.security)?, nonce.clone()].concat());
    let encrypted = STANDARD.encode(aes_encrypt(&key, data)?);
    let hash: String = random(10)
        .into_iter()
        .map(|v| {
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"[(v % 62) as usize]
                as char
        })
        .collect();
    let signature = STANDARD.encode(Sha256::digest(
        format!(
            "POST&{path}&data={encrypted}&rc4_hash__={hash}&{}",
            STANDARD.encode(key)
        )
        .as_bytes(),
    ));
    let body = format!(
        "_nonce={}&data={}&rc4_hash__={}&signature={}",
        escape(&STANDARD.encode(nonce)),
        escape(&encrypted),
        escape(&hash),
        escape(&signature)
    );
    let response = client
        .post(format!("https://idm.api.io.mi.com/app{path}"))
        .header("User-Agent", "IDM")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("X-XIAOMI-PROTOCAL-FLAG", "PROTOCAL-HTTPS")
        .header("MIOT-ENCRYPT-ALGORITHM", "ENCRYPT-AES")
        .header("SHA-ALGORITHM", "HASH-SHA256")
        .header("Cookie", c.cookie())
        .timeout(Duration::from_secs(25))
        .body(body)
        .send()
        .await
        .map_err(http_error)?
        .error_for_status()
        .map_err(http_error)?;
    let raw = response.bytes().await.map_err(http_error)?;
    let clear = if raw.starts_with(b"{") {
        raw.to_vec()
    } else {
        aes_decrypt(&key, &STANDARD.decode(raw)?)?
    };
    let value: serde_json::Value = serde_json::from_slice(&clear)?;
    ensure!(
        value["code"].as_i64() == Some(0),
        "账号请求失败，code={}",
        value["code"]
    );
    Ok(value["result"].clone())
}

/// An explicit user-initiated QR login. Creating this value contacts Xiaomi.

pub(super) async fn json_response(response: reqwest::Response) -> Result<serde_json::Value> {
    let bytes = response
        .error_for_status()
        .map_err(http_error)?
        .bytes()
        .await
        .map_err(http_error)?;
    let bytes = bytes.strip_prefix(b"&&&START&&&").unwrap_or(&bytes);
    Ok(serde_json::from_slice(bytes)?)
}

// OAuth callback and polling URLs contain short-lived secrets. Remove the URL
// before converting to the user-visible error chain.
pub(super) fn http_error(error: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!("小米账号 HTTP 请求失败：{}", error.without_url())
}

// A generic SSO cookie must not replace the service-specific STS token.

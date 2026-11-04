//! MiPush notification reception and content decryption.
use super::{
    NotificationKey, PushRuntime,
    push::{PUSH_HOST, PUSH_PORT},
    slim::{SlimClient, Thrift},
};
use crate::Result;
use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::symm::{Cipher, decrypt_aead};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhoneNotification {
    pub id: String,
    pub package: String,
    pub title: String,
    pub content: String,
    pub raw: Json,
}
pub fn decrypt_notification(content: &Json, key: &NotificationKey) -> Result<PhoneNotification> {
    anyhow::ensure!(
        content["plainMessage"]["encryptionNumber"] == key.encryption_number,
        "Notification encryption number differs from the exchanged key"
    );
    let encrypted = STANDARD.decode(
        content["encryptedMessage"]
            .as_str()
            .context("Notification has no encrypted message")?,
    )?;
    anyhow::ensure!(encrypted.len() >= 28, "Truncated encrypted notification");
    let n = encrypted.len();
    let key = key.decode()?;
    let plain = decrypt_aead(
        Cipher::aes_256_gcm(),
        &key,
        Some(&encrypted[..12]),
        &[],
        &encrypted[12..n - 16],
        &encrypted[n - 16..],
    )?;
    let raw: Json = serde_json::from_slice(&plain)?;
    Ok(PhoneNotification {
        id: raw
            .get("notificationId")
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default(),
        package: raw["packageName"].as_str().unwrap_or("手机").into(),
        title: raw["title"].as_str().unwrap_or_default().into(),
        content: raw["content"].as_str().unwrap_or_default().into(),
        raw,
    })
}
pub struct NotificationReceiver {
    client: SlimClient,
    key: NotificationKey,
}
impl NotificationReceiver {
    pub async fn connect(runtime: PushRuntime, key: NotificationKey) -> Result<Self> {
        Self::connect_to(runtime, key, PUSH_HOST, PUSH_PORT).await
    }
    pub async fn connect_to(
        runtime: PushRuntime,
        key: NotificationKey,
        host: &str,
        port: u16,
    ) -> Result<Self> {
        key.decode()?;
        let mut client = SlimClient::connect(runtime, host, port).await?;
        client.bind().await?;
        Ok(Self { client, key })
    }
    /// Wait for the next ordinary notification; MiPush acknowledgements and
    /// heartbeat frames are handled inside this receiver.
    pub async fn next(&mut self) -> Result<PhoneNotification> {
        loop {
            let blob =
                match tokio::time::timeout(Duration::from_secs(25), self.client.receive()).await {
                    Ok(result) => match result? {
                        Some(blob) => blob,
                        None => continue,
                    },
                    Err(_) => {
                        self.client.heartbeat().await?;
                        continue;
                    }
                };
            if blob.command() != "SECMSG" {
                continue;
            }
            let container = self.client.decode_container(&blob)?;
            if container.get(&1).map(Thrift::int).transpose()?.unwrap_or(0) != 5 {
                continue;
            }
            let matching = container
                .get(&8)
                .and_then(|v| v.structure().ok())
                .and_then(|m| m.get(&10))
                .and_then(|v| v.map().ok())
                .map(|items| {
                    items.iter().any(|(k, v)| {
                        k.bytes().ok() == Some(b"channel_id".as_slice())
                            && v.bytes().ok() == Some(b"143442".as_slice())
                    })
                })
                .unwrap_or(false);
            if !matching {
                continue;
            }
            let message = self.client.decode_action(&container)?;
            self.client.send_ack(&container, &message).await?;
            let nested = message
                .get(&8)
                .context("MiPush notification has no push message")?
                .structure()?;
            let raw = nested
                .get(&4)
                .context("MiPush notification has no content")?
                .bytes()?;
            let content: Json = serde_json::from_slice(raw)?;
            if content["plainMessage"]["encryptionNumber"] != self.key.encryption_number {
                // Already ACKed above. This belongs to the previous exchanged
                // key and must not stop reception after a fresh configuration.
                continue;
            }
            let notification = decrypt_notification(&content, &self.key)?;
            if !notification.title.is_empty() || !notification.content.is_empty() {
                return Ok(notification);
            }
        }
    }
    pub async fn close(&mut self) -> Result<()> {
        self.client.close().await
    }
}

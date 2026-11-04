//! Phone notification configuration, cloud subscription and MiPush reception.
use crate::{Result, common::storage::private_json};
use anyhow::anyhow;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::path::Path;

mod cloud;
mod control;
mod push;
mod receiver;
mod slim;

pub use crate::protocol::message_center::{decode_message, encode_message};
pub use cloud::{
    CloudClientInfo, CloudCredentials, CloudRequestError, decrypt_cloud_response,
    encrypt_cloud_value, is_authorization_error, register_cloud, subscribe_cloud,
};
pub use control::configure;
pub use push::{PUSH_HOST, PUSH_PORT, PushAppCredentials, PushRuntime, register_push};
pub use receiver::{NotificationReceiver, PhoneNotification, decrypt_notification};

#[derive(Clone, Serialize, Deserialize)]
pub struct NotificationKey {
    #[serde(rename = "encryptionKey")]
    pub encryption_key: String,
    #[serde(rename = "encryptionNumber")]
    pub encryption_number: Json,
}
impl NotificationKey {
    pub fn decode(&self) -> Result<[u8; 32]> {
        STANDARD
            .decode(&self.encryption_key)?
            .try_into()
            .map_err(|_| anyhow!("Notification content key must be AES-256"))
    }
}
#[derive(Clone)]
pub struct NotificationConfig {
    pub key_info: NotificationKey,
    pub query: Json,
    pub subscription: Json,
}
impl NotificationConfig {
    pub async fn save_key(&self, path: impl AsRef<Path>) -> Result<()> {
        private_json(path.as_ref(), &json!({"encryption_info":self.key_info})).await
    }
    pub async fn load_key(path: impl AsRef<Path>) -> Result<NotificationKey> {
        let value: Json = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        let key: NotificationKey =
            serde_json::from_value(value.get("encryption_info").cloned().unwrap_or(value))?;
        key.decode()?;
        Ok(key)
    }
}

pub(super) fn short_id(raw: &str) -> String {
    hex::encode_upper(Sha256::digest(raw.as_bytes()))[..8].to_owned()
}

#[cfg(test)]
mod tests {
    use super::cloud::decode_cloud_result;
    use super::*;
    #[test]
    fn cloud_401_preserves_numeric_business_code_without_response_secrets() -> Result<()> {
        let credentials = CloudCredentials {
            sid: "micloud".into(),
            user_id: "synthetic-account".into(),
            security: "AAECAwQFBgcICQoLDA0ODw==".into(),
            c_user_id: "synthetic".into(),
            service_token: "not-logged".into(),
        };
        let error = decode_cloud_result(
            "notification/trans/subscribe",
            401,
            r#"{"code":3,"description":"private-response-marker"}"#,
            &credentials,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("HTTP 401") && error.contains("code=3"));
        let typed = decode_cloud_result("push/register", 401, "not a JSON body", &credentials)
            .unwrap_err()
            .context("UI operation");
        assert!(is_authorization_error(&typed));
        assert_eq!(
            typed
                .downcast_ref::<CloudRequestError>()
                .unwrap()
                .http_status,
            401
        );
        assert!(!error.contains("private-response-marker") && !error.contains("not-logged"));
        Ok(())
    }
    #[tokio::test]
    async fn push_registration_reuses_complete_existing_identity_without_network() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("xiaomi-push-reuse-{}.json", uuid::Uuid::new_v4()));
        let runtime = PushRuntime {
            uuid: "7@xiaomi.com/synthetic".into(),
            token: "synthetic-token".into(),
            security: "AAECAwQFBgcICQoLDA0ODw==".into(),
            device_id: "synthetic-device".into(),
            app_id: "synthetic-app".into(),
            app_token: "synthetic-app-token".into(),
            package: "synthetic.package".into(),
            reg_id: "synthetic-reg".into(),
            reg_secret: "synthetic-secret".into(),
        };
        runtime.save(&path).await?;
        let before = tokio::fs::read(&path).await?;
        let app = PushAppCredentials {
            app_id: runtime.app_id.clone(),
            app_token: runtime.app_token.clone(),
            package: runtime.package.clone(),
        };
        let returned = register_push(&app, &path).await?;
        assert_eq!(returned.uuid, runtime.uuid);
        assert_eq!(returned.reg_id, runtime.reg_id);
        assert_eq!(tokio::fs::read(&path).await?, before);
        tokio::fs::remove_file(path).await?;
        Ok(())
    }
    #[test]
    fn native_messagecenter_and_cloud_cipher_vectors() -> Result<()> {
        let entity = json!({"className":"noti_trans","methodName":"exc_key"});
        let expected=b"\x00\xff\x01\x14\x00\x02\x0cnotifi_trans\x01A\x01\x01B\xfe\x01\x31{\"className\":\"noti_trans\",\"methodName\":\"exc_key\"}";
        assert_eq!(encode_message(&entity, "A", "B", 1)?, expected);
        assert_eq!(decode_message(expected)?, vec![entity]);
        assert_eq!(
            encrypt_cloud_value("XConnect-通知 +/测试", "AAECAwQFBgcICQoLDA0ODw==")?,
            "x_SAVG7wLXRukTnfoYkvGdsat8SXrmqColP7EErHfA0="
        );
        assert_eq!(
            decrypt_cloud_response(
                "x/SAVG7wLXRukTnfoYkvGdsat8SXrmqColP7EErHfA0=",
                "AAECAwQFBgcICQoLDA0ODw=="
            )?,
            "XConnect-通知 +/测试"
        );
        // Fixed AES-256-GCM content generated by the existing Python codec.
        let key = NotificationKey {
            encryption_key: STANDARD.encode((0..32).collect::<Vec<u8>>()),
            encryption_number: json!(7),
        };
        let content = json!({"plainMessage":{"encryptionNumber":7},"encryptedMessage":"AAECAwQFBgcICQoLPCCicrGJpzm3Y+Ty350QCPe/5BbcWTwTVhOA62lLOpBvf46M3ahk+QDBX4np80kak7jBig0NJqovXHSsxSXum04="});
        let notice = decrypt_notification(&content, &key)?;
        assert_eq!(notice.title, "synthetic");
        assert_eq!(notice.content, "no private data");
        Ok(())
    }
}

//! MiPush runtime registration and Slim identity persistence.
use super::slim::{SlimClient, Thrift, decode_thrift};
use crate::{
    Result,
    common::storage::{json_path, private_json},
};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::{path::Path, time::Duration};

pub const PUSH_HOST: &str = "app.chat.xiaomi.net";
pub const PUSH_PORT: u16 = 5222;

#[derive(Clone, Deserialize, Serialize)]
pub struct PushRuntime {
    pub uuid: String,
    pub token: String,
    pub security: String,
    pub device_id: String,
    pub app_id: String,
    pub app_token: String,
    pub package: String,
    #[serde(default)]
    pub reg_id: String,
    #[serde(default)]
    pub reg_secret: String,
}
impl PushRuntime {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = json_path(path.as_ref(), "runtime.private.json");
        Ok(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    }
    pub async fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        private_json(path.as_ref(), &serde_json::to_value(self)?).await
    }
}
#[derive(Clone, Deserialize)]
pub struct PushAppCredentials {
    pub app_id: String,
    pub app_token: String,
    #[serde(default = "default_package")]
    pub package: String,
}
fn default_package() -> String {
    "com.milink.service".into()
}
impl PushAppCredentials {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        Ok(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    }
}

/// Register the caller's own application/runtime; call only as an explicit setup action.
pub async fn register_push(
    app: &PushAppCredentials,
    output: impl AsRef<Path>,
) -> Result<PushRuntime> {
    let output = json_path(output.as_ref(), "runtime.private.json");
    let mut runtime = if output.exists() {
        let runtime = PushRuntime::load(&output).await?;
        anyhow::ensure!(
            runtime.app_id == app.app_id && runtime.package == app.package,
            "已有推送身份属于不同应用；请保留该状态并为其他应用选择单独路径"
        );
        if !runtime.reg_id.is_empty() && !runtime.reg_secret.is_empty() {
            return Ok(runtime);
        }
        runtime
    } else {
        let runtime = register_runtime(app).await?;
        runtime.save(&output).await?;
        runtime
    };
    let mut client = SlimClient::connect(runtime.clone(), PUSH_HOST, PUSH_PORT).await?;
    client.bind().await?;
    client.send_registration().await?;
    let registered = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let Some(blob) = client.receive().await? else {
                continue;
            };
            if blob.command() != "SECMSG" {
                continue;
            }
            let container = client.decode_container(&blob)?;
            if container.get(&1).map(Thrift::int).transpose()?.unwrap_or(0) != 1 {
                continue;
            }
            anyhow::ensure!(
                container.get(&2).map(Thrift::int).transpose()?.unwrap_or(0) == 0,
                "MiPush app registration reply unexpectedly encrypted"
            );
            let raw = container
                .get(&4)
                .context("MiPush registration has no action")?
                .bytes()?;
            let result = decode_thrift(raw)?;
            let code = result
                .get(&6)
                .map(Thrift::int)
                .transpose()?
                .context("MiPush app registration has no result code")?;
            anyhow::ensure!(
                code == 0,
                "MiPush app registration was rejected: code {code}"
            );
            let reg_id = String::from_utf8(
                result
                    .get(&8)
                    .context("MiPush registration has no regId")?
                    .bytes()?
                    .to_vec(),
            )?;
            let reg_secret = String::from_utf8(
                result
                    .get(&9)
                    .context("MiPush registration has no regSecret")?
                    .bytes()?
                    .to_vec(),
            )?;
            return Ok::<_, anyhow::Error>((reg_id, reg_secret));
        }
    })
    .await
    .context("MiPush app registration timed out")?;
    let _ = client.close().await;
    let (reg_id, reg_secret) = registered?;
    runtime.reg_id = reg_id;
    runtime.reg_secret = reg_secret;
    runtime.save(&output).await?;
    Ok(runtime)
}

async fn register_runtime(app: &PushAppCredentials) -> Result<PushRuntime> {
    let device_id = format!("a-{}", uuid::Uuid::new_v4().simple());
    let os = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let fields = [
        ("devid", device_id.as_str()),
        ("appid", app.app_id.as_str()),
        ("apptoken", app.app_token.as_str()),
        ("appversion", "1"),
        ("sdkversion", "60088"),
        ("packagename", app.package.as_str()),
        ("model", crate::DEFAULT_DEVICE_NAME),
        ("board", "linux"),
        ("os", os.trim()),
        ("brand", "Linux"),
    ];
    let result: Json = reqwest::Client::new()
        .post("https://cn.register.xmpush.xiaomi.com/pass/v2/register")
        .form(&fields)
        .send()
        .await
        .context("MiPush runtime registration request failed")?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        result["code"] == 0,
        "MiPush runtime registration failed: code {}",
        result["code"]
    );
    let data = &result["data"];
    let user = data["userId"]
        .as_str()
        .context("MiPush registration has no user ID")?;
    let resource = user
        .split_once('/')
        .map(|(_, r)| r.to_owned())
        .unwrap_or_else(|| format!("an{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
    let mut username = if user.contains('@') {
        user.to_owned()
    } else {
        format!("{user}@xiaomi.com")
    };
    if !username.contains('/') {
        username.push('/');
        username.push_str(&resource);
    }
    let runtime = PushRuntime {
        uuid: username,
        token: data["token"]
            .as_str()
            .context("MiPush registration has no token")?
            .into(),
        security: data["ssecurity"]
            .as_str()
            .context("MiPush registration has no security")?
            .into(),
        device_id,
        app_id: app.app_id.clone(),
        app_token: app.app_token.clone(),
        package: app.package.clone(),
        reg_id: String::new(),
        reg_secret: String::new(),
    };
    Ok(runtime)
}

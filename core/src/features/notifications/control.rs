//! LAN notification configuration over the Networking control channel.
use super::{NotificationConfig, NotificationKey, short_id};
use crate::{
    Result,
    protocol::{
        message_center::{decode_message, encode_message, networking_frame},
        pb::fields,
    },
    session::{Session, SessionEvent},
};
use anyhow::{Context, bail};
use serde_json::{Value as Json, json};
use std::time::Duration;
use tokio::sync::broadcast;

pub async fn configure(session: &Session, packages: &[String]) -> Result<NotificationConfig> {
    let own = short_id(&session.account().device_id);
    let target = session.peer_short_id();
    anyhow::ensure!(
        !target.is_empty(),
        "手机 Networking 设备资料尚未包含公共 ID，无法配置通知目标"
    );
    session.report_phase("通知：交换新的内容密钥");
    let mut rx = session.events();
    send_control(session, &own, &target, "exc_key", None, 1).await?;
    let key_reply = receive_control(&mut rx, "key_callback").await?;
    anyhow::ensure!(
        key_reply["methodName"] == "success",
        "Phone rejected notification key exchange: code {}",
        key_reply["data"]["code"]
    );
    let key_info: NotificationKey = serde_json::from_value(key_reply["data"]["result"].clone())?;
    key_info.decode()?;
    send_control(
        session,
        &own,
        &target,
        "ack_key",
        Some(json!({"enc_number":key_info.encryption_number})),
        2,
    )
    .await?;
    session.report_phase("通知：密钥已确认，查询手机应用与权限");
    send_control(session, &own, &target, "query", None, 3).await?;
    let query = receive_control(&mut rx, "query_callback").await?;
    let code = query["data"]["code"]
        .as_i64()
        .context("Notification query has no code")?;
    anyhow::ensure!(
        code == 1 || code == -2,
        "Notification application query rejected: code {code}"
    );
    session.report_phase(format!("通知：应用查询 code={code}，提交局域网订阅"));
    send_control(session,&own,&target,"sub",Some(json!({"app_list":packages.iter().map(|p|json!({"packageName":p})).collect::<Vec<_>>()})),4).await?;
    let subscription = receive_control(&mut rx, "sub_callback").await?;
    anyhow::ensure!(
        subscription["data"]["code"] == 1,
        "Notification subscription rejected: code {} (phone permission may be required)",
        subscription["data"]["code"]
    );
    let results = subscription["data"]["result"]
        .as_array()
        .context("Notification subscription has no application results")?;
    for package in packages {
        anyhow::ensure!(
            results
                .iter()
                .any(|r| r["appInfo"]["packageName"] == *package && r["resultCode"] == 1),
            "Phone did not subscribe to application {package}"
        );
    }
    session.report_phase("通知：局域网密钥、权限和应用订阅已确认");
    Ok(NotificationConfig {
        key_info,
        query,
        subscription,
    })
}

async fn send_control(
    session: &Session,
    own: &str,
    target: &str,
    method: &str,
    data: Option<Json>,
    sequence: u8,
) -> Result<()> {
    let mut entity = json!({"className":"noti_trans","methodName":method});
    if let Some(data) = data {
        entity["data"] = data;
    }
    let frame = networking_frame(&encode_message(&entity, own, target, sequence)?);
    session.send_networking(1, &frame).await
}
async fn receive_control(rx: &mut broadcast::Receiver<SessionEvent>, name: &str) -> Result<Json> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let event = rx.recv().await?;
            if let SessionEvent::Disconnected(reason) = &event {
                bail!("通知 {name} 等待期间连接断开：{reason}");
            }
            if let SessionEvent::Networking { module: 1, data } = event {
                let packet = fields(&data)?;
                if packet.uint(1) != 4 {
                    continue;
                }
                let body = fields(packet.bytes(8))?;
                for entity in decode_message(body.bytes(1))? {
                    if entity["className"] == name {
                        return Ok(entity);
                    }
                }
            }
        }
    })
    .await
    .with_context(|| format!("Notification {name} timed out"))?
}

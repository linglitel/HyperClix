//! Camera JSON-RPC and hardware-service framing helpers.
use crate::{
    Result,
    protocol::messages::camera::{
        CameraResponse, HardwareInfo, METADATA_CATEGORY, hardware_from_response,
    },
    session::{Channel, ChannelEvent},
};
use anyhow::{Context, bail};
use serde_json::{Value as Json, json};
use std::time::Duration;
use tokio::sync::broadcast;

pub(super) const ERROR_NO_CAMERA: i64 = -20001;
pub(super) const ERROR_CONFIGURE_FAIL: i64 = -20002;

async fn raw_receive(rx: &mut broadcast::Receiver<ChannelEvent>) -> Result<Vec<u8>> {
    match rx.recv().await? {
        ChannelEvent::Data(data) => Ok(data),
        ChannelEvent::Closed(reason) => bail!("Camera channel closed: {reason}"),
    }
}

pub(super) async fn metadata_exchange(
    channel: &Channel,
    rx: &mut broadcast::Receiver<ChannelEvent>,
    packet: &[u8],
    action: u64,
    timeout: Duration,
) -> Result<Vec<u8>> {
    channel.send(packet).await?;
    tokio::time::timeout(timeout, async {
        loop {
            let data = raw_receive(rx).await?;
            let response = CameraResponse::decode(&data)?;
            if response.category != METADATA_CATEGORY {
                continue;
            }
            if response.action == action {
                if action == 4 {
                    anyhow::ensure!(
                        response.request_flag == 1,
                        "Camera hardware query rejected: {}",
                        response.request_flag
                    );
                } else {
                    // CommandResult: 4 SUCCESSFUL, 5 HASASSO (SETDEFAULT when
                    // the phone still holds an association), 6 HASNOASSO
                    // (RESETDEFAULT when it was already released). Treat the
                    // idempotent answers as success.
                    let accepted = match action {
                        6 => matches!(response.status, 4 | 5),
                        8 => matches!(response.status, 4 | 6),
                        _ => response.status == 4,
                    };
                    anyhow::ensure!(
                        accepted,
                        "Camera hardware action {action} rejected: {}",
                        response.status
                    );
                }
                return Ok(data);
            }
        }
    })
    .await
    .with_context(|| format!("Camera hardware action {action} timed out"))?
}

pub(super) fn select_hardware(data: &[u8], index: usize) -> Result<HardwareInfo> {
    let response = CameraResponse::decode(data)?;
    for (position, hardware) in response.hardware.iter().enumerate() {
        tracing::info!(
            "[camera] exposed hardware[{position}]: id={:?} owner={:?} kind={} type={} device_id={:?} property={:?} attributes={:?}",
            hardware.id,
            hardware.owner,
            hardware.kind,
            hardware.device_type,
            hardware.device_id,
            truncate(&hardware.property),
            truncate(&hardware.attributes)
        );
    }
    tracing::info!(
        "[camera] selecting hardware index={index} of {} exposed",
        response.hardware.len()
    );
    hardware_from_response(&response, index)
}

fn truncate(value: &str) -> &str {
    const LIMIT: usize = 160;
    if value.len() <= LIMIT {
        return value;
    }
    value.get(..LIMIT).unwrap_or(value)
}

pub(super) fn cleanup_timeout(options: &super::CameraOptions) -> Duration {
    options.timeout.min(Duration::from_secs(5))
}

pub(super) fn reassociate_timeout(options: &super::CameraOptions) -> Duration {
    options.timeout.min(Duration::from_secs(10))
}

pub(super) fn version_timeout(options: &super::CameraOptions) -> Duration {
    options.timeout.min(Duration::from_secs(5))
}

pub(super) fn is_retryable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RpcRejection>())
        .is_some_and(|rejection| matches!(rejection.code, ERROR_NO_CAMERA | ERROR_CONFIGURE_FAIL))
}

pub(super) async fn rpc_exchange(
    channel: &Channel,
    rx: &mut broadcast::Receiver<ChannelEvent>,
    method: &str,
    params: Option<Json>,
    id: u64,
    duration: Duration,
) -> Result<Json> {
    let mut request = json!({"jsonrpc":"2.0", "id":id, "method":format!("rpcCamera.{method}")});
    if let Some(params) = params {
        request["params"] = params;
    }
    channel.send(&serde_json::to_vec(&request)?).await?;
    tokio::time::timeout(duration, async {
        loop {
            let data = raw_receive(rx).await?;
            let reply: Json = serde_json::from_slice(&data)?;
            if reply["id"].as_u64() != Some(id) {
                continue;
            }
            return rpc_result(method, &reply);
        }
    })
    .await
    .with_context(|| format!("Camera {method} timed out"))?
}

#[derive(Debug)]
struct RpcRejection {
    method: String,
    code: i64,
    message: String,
}
impl std::fmt::Display for RpcRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Camera {} rejected: code {} ({})",
            self.method, self.code, self.message
        )
    }
}
impl std::error::Error for RpcRejection {}

pub(super) fn rpc_result(method: &str, reply: &Json) -> Result<Json> {
    let cleanup = matches!(method, "stopPreview" | "close");
    if let Some(error) = reply.get("error") {
        let code = error["code"].as_i64().unwrap_or(0);
        let message = error["message"].as_str().unwrap_or_default();
        tracing::info!("[camera] {method} JSON-RPC error code={code} message={message}");
        if cleanup && code == ERROR_NO_CAMERA {
            tracing::info!("[camera] {method} returned -20001 (device already closed)");
            return Ok(Json::Null);
        }
        return Err(RpcRejection {
            method: method.into(),
            code,
            message: message.into(),
        }
        .into());
    }
    let result = reply.get("result").context("Camera RPC has no result")?;
    let code = result["code"].as_i64().unwrap_or(0);
    // IRpcCamera.ERROR_NO_CAMERA: the phone can finish before our cleanup RPC.
    // Its own DistCamera close also accepts this as an already closed device.
    if code != 0 && !(cleanup && code == ERROR_NO_CAMERA) {
        return Err(RpcRejection {
            method: method.into(),
            code,
            message: result["message"].as_str().unwrap_or_default().into(),
        }
        .into());
    }
    Ok(result.clone())
}

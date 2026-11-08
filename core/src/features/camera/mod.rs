//! Distributed camera control and the negotiated AES-CBC preview format.
//!
//! `CameraPreview::open` reuses the session-scoped hardware association, opens
//! the command channel and runs the camera RPC sequence. `close` ends only the
//! preview; the association stays until the device session ends.
mod association;
mod rpc;
mod types;

use crate::{
    Result,
    session::{Channel, ChannelEvent, Session},
};
use association::{CameraRuntime, reassociate};
use rpc::{
    cleanup_timeout, is_retryable, metadata_exchange, reassociate_timeout, rpc_exchange,
    select_hardware, version_timeout,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, broadcast};

pub use types::{CameraMediaConfig, CameraOptions, CameraStream};

pub const SERVICE: &str = crate::protocol::service::CAMERA_COMMAND;
pub const METADATA_SERVICE: &str = crate::protocol::service::CAMERA_METADATA;
pub const P2P_SERVICE: &str = crate::protocol::service::CAMERA_P2P;
pub struct CameraPreview {
    command: Channel,
    runtime: Arc<CameraRuntime>,
    incoming: Mutex<Option<tokio::task::JoinHandle<Result<Channel>>>>,
    command_rx: Mutex<broadcast::Receiver<ChannelEvent>>,
    options: CameraOptions,
    config: CameraMediaConfig,
    closed: AtomicBool,
}
struct ActiveCamera(Arc<CameraRuntime>);
impl Drop for ActiveCamera {
    fn drop(&mut self) {
        self.0.association.release();
    }
}

impl Session {
    pub async fn open_camera(&self, options: CameraOptions) -> Result<CameraPreview> {
        CameraPreview::open(self, options).await
    }
}
impl CameraPreview {
    pub async fn open(session: &Session, options: CameraOptions) -> Result<Self> {
        tracing::info!(
            "[camera] open request camera_id={} size={}x{} hardware_index={}",
            options.camera_id,
            options.width,
            options.height,
            options.hardware_index
        );
        let runtime = session.extension::<CameraRuntime>();
        let hardware = runtime.association.acquire(session, &options).await?;
        let claim = ActiveCamera(runtime.clone());
        let mut command = runtime.ensure_command(session, &options).await?;
        let mut command_rx = command.subscribe();
        let mut incoming = if let Some(channel) = runtime.retained_p2p().await {
            tracing::info!("[camera] reusing cameraP2P channel id={}", channel.id());
            None
        } else {
            let accept_session = session.clone();
            let accept_runtime = runtime.clone();
            Some(tokio::spawn(async move {
                let channel = accept_session.accept_channel(P2P_SERVICE).await?;
                accept_runtime.store_p2p(channel.clone()).await;
                Ok(channel)
            }))
        };
        let streams = vec![CameraStream::preview(options.width, options.height)];
        let (mut opened, mut result) = camera_open_sequence(
            &runtime,
            &command,
            &mut command_rx,
            &options,
            &hardware.info.id,
            &streams,
        )
        .await;
        if result.is_err() && command.is_closed() {
            tracing::info!("[camera] command channel closed; rebuilding and retrying once");
            let was_opened = opened;
            runtime.invalidate_command().await;
            match runtime.ensure_command(session, &options).await {
                Ok(channel) => {
                    command = channel;
                    command_rx = command.subscribe();
                }
                Err(error) => {
                    if let Some(task) = incoming.take() {
                        task.abort();
                    }
                    return Err(error.context("重建手机相机命令通道失败"));
                }
            }
            if was_opened {
                let id = runtime.next_id();
                let _ = rpc_exchange(
                    &command,
                    &mut command_rx,
                    "close",
                    Some(json!([1, options.camera_id, {"isComposing":false}])),
                    id,
                    cleanup_timeout(&options),
                )
                .await;
            }
            let (retried_open, retried) = camera_open_sequence(
                &runtime,
                &command,
                &mut command_rx,
                &options,
                &hardware.info.id,
                &streams,
            )
            .await;
            opened = retried_open;
            result = retried;
        }
        if result.as_ref().is_err_and(is_retryable) {
            tracing::info!("[camera] phone rejected open; reassociating and retrying once");
            if opened {
                let id = runtime.next_id();
                let _ = rpc_exchange(
                    &command,
                    &mut command_rx,
                    "close",
                    Some(json!([1, options.camera_id, {"isComposing":false}])),
                    id,
                    cleanup_timeout(&options),
                )
                .await;
                opened = false;
            }
            result = match reassociate(&hardware, reassociate_timeout(&options)).await {
                Ok(()) => {
                    let (retried_open, retried) = camera_open_sequence(
                        &runtime,
                        &command,
                        &mut command_rx,
                        &options,
                        &hardware.info.id,
                        &streams,
                    )
                    .await;
                    opened = retried_open;
                    retried
                }
                Err(error) => Err(error.context("手机相机重新关联失败")),
            };
        }
        match result {
            Ok(config) => {
                std::mem::forget(claim);
                Ok(Self {
                    command,
                    runtime,
                    incoming: Mutex::new(incoming),
                    command_rx: Mutex::new(command_rx),
                    options,
                    config,
                    closed: AtomicBool::new(false),
                })
            }
            Err(error) => {
                if let Some(task) = incoming.take() {
                    task.abort();
                }
                if opened {
                    let id = runtime.next_id();
                    let _ = rpc_exchange(
                        &command,
                        &mut command_rx,
                        "close",
                        Some(json!([1, options.camera_id, {"isComposing":false}])),
                        id,
                        cleanup_timeout(&options),
                    )
                    .await;
                }
                if command.is_closed() {
                    runtime.invalidate_command().await;
                }
                Err(error)
            }
        }
    }
    pub fn config(&self) -> &CameraMediaConfig {
        &self.config
    }

    /// Media-layer playback configuration for this preview.
    pub fn feed(&self) -> crate::media::CameraFeed {
        let codec = match self.config.video_format.as_str().unwrap_or("video/avc") {
            "video/hevc" | "video/h265" => crate::media::CameraCodec::H265,
            _ => crate::media::CameraCodec::H264,
        };
        crate::media::CameraFeed {
            ip: self.config.ip,
            port: self.config.port,
            key: self.config.key,
            codec,
        }
    }
    pub async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let timeout = cleanup_timeout(&self.options);
        let mut rx = self.command_rx.lock().await;
        let id = self.runtime.next_id();
        let closed = rpc_exchange(
            &self.command,
            &mut rx,
            "close",
            Some(json!([1, self.options.camera_id, {"isComposing":false}])),
            id,
            timeout,
        )
        .await;
        if self.command.is_closed() {
            self.runtime.invalidate_command().await;
        }
        if let Some(task) = self.incoming.lock().await.take() {
            if task.is_finished() {
                let _ = task.await;
            } else {
                task.abort();
            }
        }
        self.runtime.association.release();
        closed.map(|_| ())
    }
}
impl Drop for CameraPreview {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Acquire) {
            self.closed.store(true, Ordering::Release);
            if let Ok(mut slot) = self.incoming.try_lock() {
                if let Some(task) = slot.take() {
                    task.abort();
                }
            }
            self.runtime.association.release();
        }
    }
}

async fn camera_open_sequence(
    runtime: &CameraRuntime,
    command: &Channel,
    rx: &mut broadcast::Receiver<ChannelEvent>,
    options: &CameraOptions,
    hardware_id: &str,
    streams: &[CameraStream],
) -> (bool, Result<CameraMediaConfig>) {
    let mut opened = false;
    let result = async {
        let id = runtime.next_id();
        rpc_exchange(
            command,
            rx,
            "querySupportedVersions",
            None,
            id,
            version_timeout(options),
        )
        .await?;
        let id = runtime.next_id();
        let opened_result = rpc_exchange(
            command,
            rx,
            "open",
            Some(json!([1, options.camera_id, hardware_id, 1, null, {"isComposing":false,"isSwitchingCamera":false}])),
            id,
            options.timeout,
        )
        .await?;
        if let Some(reason) = opened_result["mOpenFailReason"].as_i64()
            && reason != 0
        {
            tracing::info!("[camera] phone reported open fail reason={reason}");
        }
        opened = true;
        let id = runtime.next_id();
        let configured = rpc_exchange(
            command,
            rx,
            "configure",
            Some(json!([1, options.camera_id, 0, 0, streams, {"rotation":0,"mirrorMode":0,"disableCropRotateScale":false}])),
            id,
            options.timeout,
        )
        .await?;
        let config = CameraMediaConfig::from_result(&configured)?;
        let id = runtime.next_id();
        rpc_exchange(
            command,
            rx,
            "startPreview",
            Some(json!([1, options.camera_id, streams])),
            id,
            options.timeout,
        )
        .await?;
        Ok(config)
    }
    .await;
    (opened, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::camera::rpc::rpc_result;
    use crate::media::camera_ts::decrypt_pes;
    #[test]
    fn phone_finished_camera_accepts_cleanup_without_hiding_open_errors() -> Result<()> {
        let ended = json!({"jsonrpc":"2.0", "id":5,
            "result":{"code":-20001,"message":"no valid camera device"}});
        for method in ["stopPreview", "close"] {
            assert_eq!(rpc_result(method, &ended)?["code"], -20001);
        }
        for method in ["open", "configure", "startPreview"] {
            assert!(rpc_result(method, &ended).is_err());
        }
        let error = json!({"jsonrpc":"2.0", "id":6,
            "error":{"code":-20001,"message":"no valid camera device"}});
        assert!(rpc_result("close", &error).is_ok());
        assert!(rpc_result("open", &error).is_err());
        assert!(rpc_result("close", &json!({"result":{"code":-20004}})).is_err());
        Ok(())
    }
    #[test]
    fn native_camera_cbc_pes_vector_preserves_clear_tail() -> Result<()> {
        // Native libCastService AES fixture, independently matching NIST SP800-38A.
        let key: [u8; 16] = hex::decode("2b7e151628aed2a6abf7158809cf4f3c")?
            .try_into()
            .unwrap();
        let iv = hex::decode("000102030405060708090a0b0c0d0e0f")?;
        let mut optional = vec![0x21, 0, 1, 0, 1, 0x8e];
        optional.extend(iv);
        let mut encrypted =
            hex::decode("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2")?;
        encrypted.extend(b"tail123");
        let length = (3 + optional.len() + encrypted.len()) as u16;
        let packet = [
            vec![0, 0, 1, 0xe0],
            length.to_be_bytes().to_vec(),
            vec![0x80, 0x81, optional.len() as u8],
            optional,
            encrypted.clone(),
        ]
        .concat();
        let mut plain =
            hex::decode("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51")?;
        plain.extend(b"tail123");
        assert_eq!(decrypt_pes(&packet, &key, 0)?, plain);
        assert_eq!(
            decrypt_pes(&packet, &key, 16)?,
            [&plain[..16], &encrypted[16..]].concat()
        );
        Ok(())
    }
}

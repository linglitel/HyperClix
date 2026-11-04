//! Session-scoped distributed-camera hardware association.
//!
//! The official PC client associates a phone camera once, keeps the hardware
//! service channel for the lifetime of the device session, and only tears the
//! camera preview down on close. Rebuilding the association for every preview
//! leaves the phone's distributed-hardware service with stale channel state, so
//! the next open can wait forever for a metadata channel that is never answered.
use super::{CameraOptions, METADATA_SERVICE, metadata_exchange, select_hardware};
use crate::{
    Result,
    protocol::messages::camera::{HardwareInfo, hardware_message, metadata_request},
    session::{Channel, ChannelEvent, SdkPlatform, Session, TRUST_PAIR},
};
use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::sync::{Mutex, broadcast};

/// A reused association must answer quickly; a stale one is replaced instead of
/// blocking the next preview for the full RPC timeout.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct CameraHardware {
    pub(super) channel: Channel,
    pub(super) info: HardwareInfo,
    pub(super) own_id: String,
    pub(super) device_type: u64,
}

#[derive(Default)]
pub(super) struct CameraAssociation {
    hardware: Mutex<Option<CameraHardware>>,
    active: std::sync::atomic::AtomicBool,
}

impl CameraAssociation {
    /// Claim the single local preview slot and reuse or rebuild the association.
    pub(super) async fn acquire(
        &self,
        session: &Session,
        options: &CameraOptions,
    ) -> Result<CameraHardware> {
        ensure!(
            !self.active.swap(true, std::sync::atomic::Ordering::AcqRel),
            "手机相机预览已经打开"
        );
        let guard = ActiveGuard(self);
        let hardware = self.acquire_inner(session, options).await?;
        std::mem::forget(guard);
        Ok(hardware)
    }

    async fn acquire_inner(
        &self,
        session: &Session,
        options: &CameraOptions,
    ) -> Result<CameraHardware> {
        let mut slot = self.hardware.lock().await;
        if let Some(cached) = slot.as_ref().cloned() {
            match refresh(&cached, options).await {
                Ok(Some(hardware)) => {
                    tracing::info!(
                        "[camera] reusing hardware association id={} channel={}",
                        hardware.info.id,
                        hardware.channel.id()
                    );
                    *slot = Some(hardware.clone());
                    return Ok(hardware);
                }
                Ok(None) => {
                    let _ = cached.channel.close().await;
                }
                Err(error) => {
                    *slot = Some(cached);
                    return Err(error);
                }
            }
        }
        let hardware = open_fresh(session, options).await?;
        *slot = Some(hardware.clone());
        Ok(hardware)
    }

    pub(super) fn release(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

struct ActiveGuard<'a>(&'a CameraAssociation);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn refresh(
    cached: &CameraHardware,
    options: &CameraOptions,
) -> Result<Option<CameraHardware>> {
    if cached.channel.is_closed() {
        tracing::info!("[camera] association channel already closed; reopening");
        return Ok(None);
    }
    let mut rx = cached.channel.subscribe();
    let timeout = options.timeout.min(HEALTH_TIMEOUT);
    let data = match metadata_exchange(
        &cached.channel,
        &mut rx,
        &metadata_request(&cached.own_id, cached.device_type),
        4,
        timeout,
    )
    .await
    {
        Ok(data) => data,
        Err(error) => {
            tracing::info!("[camera] association channel stale, reopening: {error:#}");
            return Ok(None);
        }
    };
    let info = select_hardware(&data, options.hardware_index)?;
    if info.id == cached.info.id {
        return Ok(Some(CameraHardware {
            info,
            ..cached.clone()
        }));
    }
    tracing::info!(
        "[camera] switching hardware {} -> {}",
        cached.info.id,
        info.id
    );
    let _ = metadata_exchange(
        &cached.channel,
        &mut rx,
        &hardware_message(&cached.own_id, &cached.info, cached.device_type, 7),
        8,
        timeout,
    )
    .await;
    bind(
        &cached.channel,
        &mut rx,
        &cached.own_id,
        cached.device_type,
        &info,
        timeout,
    )
    .await?;
    Ok(Some(CameraHardware {
        channel: cached.channel.clone(),
        info,
        own_id: cached.own_id.clone(),
        device_type: cached.device_type,
    }))
}

async fn open_fresh(session: &Session, options: &CameraOptions) -> Result<CameraHardware> {
    let own_id =
        hex::encode_upper(Sha256::digest(session.account().device_id.as_bytes()))[..8].to_owned();
    let device_type = session.account().device_type as u64;
    tracing::info!("[camera] opening metadata channel");
    let channel = session
        .open_channel(METADATA_SERVICE, TRUST_PAIR, SdkPlatform::Linux)
        .await?;
    tracing::info!("[camera] metadata channel ready id={}", channel.id());
    let mut rx = channel.subscribe();
    let result: Result<CameraHardware> = async {
        tracing::info!("[camera] query metadata");
        let data = metadata_exchange(
            &channel,
            &mut rx,
            &metadata_request(&own_id, device_type),
            4,
            options.timeout,
        )
        .await?;
        tracing::info!("[camera] metadata response bytes={}", data.len());
        let info = select_hardware(&data, options.hardware_index)?;
        bind(
            &channel,
            &mut rx,
            &own_id,
            device_type,
            &info,
            options.timeout,
        )
        .await?;
        Ok(CameraHardware {
            channel: channel.clone(),
            info,
            own_id,
            device_type,
        })
    }
    .await;
    if result.is_err() {
        let _ = channel.close().await;
    }
    result
}

async fn bind(
    channel: &Channel,
    rx: &mut broadcast::Receiver<ChannelEvent>,
    own_id: &str,
    device_type: u64,
    info: &HardwareInfo,
    timeout: Duration,
) -> Result<()> {
    metadata_exchange(
        channel,
        rx,
        &hardware_message(own_id, info, device_type, 5),
        6,
        timeout,
    )
    .await
    .with_context(|| format!("绑定手机相机硬件 {}", info.id))?;
    tracing::info!("[camera] hardware association ready id={}", info.id);
    Ok(())
}

/// Re-issue SETDEFAULT after the phone rejected an open with -20001/-20002.
pub(super) async fn reassociate(hardware: &CameraHardware, timeout: Duration) -> Result<()> {
    let mut rx = hardware.channel.subscribe();
    metadata_exchange(
        &hardware.channel,
        &mut rx,
        &hardware_message(&hardware.own_id, &hardware.info, hardware.device_type, 5),
        6,
        timeout,
    )
    .await
    .context("重新绑定手机相机硬件")
    .map(|_| ())
}

/// Session-scoped camera transport.
///
/// The official client registers the command and P2P listeners once per device
/// session and keeps them across previews. Only the preview RPCs are repeated.
#[derive(Default)]
pub(super) struct CameraRuntime {
    pub(super) association: CameraAssociation,
    pub(super) command: Mutex<Option<Channel>>,
    pub(super) p2p: Mutex<Option<Channel>>,
    sequence: std::sync::atomic::AtomicU64,
}

impl CameraRuntime {
    pub(super) fn next_id(&self) -> u64 {
        self.sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Reuse the session command channel when it answers a version query;
    /// otherwise rebuild it before the next preview.
    pub(super) async fn ensure_command(
        &self,
        session: &Session,
        options: &CameraOptions,
    ) -> Result<Channel> {
        let mut slot = self.command.lock().await;
        if let Some(channel) = slot.as_ref().cloned() {
            if !channel.is_closed() {
                let mut rx = channel.subscribe();
                let id = self.next_id();
                match super::rpc_exchange(
                    &channel,
                    &mut rx,
                    "querySupportedVersions",
                    None,
                    id,
                    options.timeout.min(HEALTH_TIMEOUT),
                )
                .await
                {
                    Ok(_) => {
                        tracing::info!("[camera] reusing command channel id={}", channel.id());
                        return Ok(channel);
                    }
                    Err(error) => {
                        tracing::info!("[camera] command channel stale, reopening: {error:#}");
                        let _ = channel.close().await;
                    }
                }
            }
            *slot = None;
        }
        tracing::info!("[camera] opening command channel");
        let channel = session
            .open_channel_with_data(
                super::SERVICE,
                TRUST_PAIR,
                SdkPlatform::Linux,
                "{\"serviceVersion\":\"\"}",
            )
            .await?;
        tracing::info!("[camera] command channel ready id={}", channel.id());
        *slot = Some(channel.clone());
        Ok(channel)
    }

    pub(super) async fn invalidate_command(&self) {
        let mut slot = self.command.lock().await;
        if let Some(channel) = slot.take() {
            let _ = channel.close().await;
        }
    }

    /// Return the cameraP2P channel accepted in an earlier preview if the phone
    /// still keeps it open. A closed channel is dropped and re-accepted.
    pub(super) async fn retained_p2p(&self) -> Option<Channel> {
        let mut slot = self.p2p.lock().await;
        match slot.as_ref() {
            Some(channel) if !channel.is_closed() => Some(channel.clone()),
            Some(_) => {
                *slot = None;
                None
            }
            None => None,
        }
    }

    pub(super) async fn store_p2p(&self, channel: Channel) {
        let mut slot = self.p2p.lock().await;
        if let Some(old) = slot.replace(channel.clone())
            && old.id() != channel.id()
        {
            let _ = old.close().await;
        }
    }
}

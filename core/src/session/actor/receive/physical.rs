//! Inbound Physical synchronization and close handling.
use super::super::super::device::physical_device;
use super::super::PhysicalActor;
use crate::{
    Result,
    common::time::epoch_ms,
    protocol::{
        messages::lyra::{
            LogicalBody, LogicalEnvelope, PhysicalBody, PhysicalFrame, PhysicalHeartbeatAck,
            PhysicalSync, PhysicalSyncReply,
        },
        pb::Fields,
    },
    session::{NETWORKING, SdkPlatform},
};
use anyhow::{Context, ensure};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

impl PhysicalActor {
    pub(super) async fn receive_physical(
        &mut self,
        frame: PhysicalFrame,
        physical_id: u8,
    ) -> Result<()> {
        match frame.body {
            PhysicalBody::SyncRequest(sync) if self.server => {
                self.handle_sync_request(frame.id, sync, physical_id)
                    .await?;
            }
            PhysicalBody::SyncReply(sync) if physical_id == 1 => {
                self.handle_upgrade_sync(sync, physical_id).await?;
            }
            PhysicalBody::SyncReply(sync) => {
                self.handle_peer_sync(sync)?;
            }
            PhysicalBody::Heartbeat(heartbeat) => {
                let reply = PhysicalFrame::heartbeat_ack(PhysicalHeartbeatAck {
                    uptime_ms: self.started.elapsed().as_millis() as u64,
                    request_status: heartbeat.status,
                    request_uptime_ms: heartbeat.uptime_ms,
                    request_field4: 0,
                });
                self.queue_on(physical_id, &reply.encode())?;
            }
            PhysicalBody::Close(_) => {
                self.handle_physical_close(physical_id).await?;
            }
            PhysicalBody::CloseAck(_) => {
                if let Some((deadline, _, sent)) = &mut self.closing {
                    if *sent {
                        *deadline = Instant::now();
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_sync_request(
        &mut self,
        id: u64,
        sync: PhysicalSync,
        physical_id: u8,
    ) -> Result<()> {
        self.phase(&format!(
            "入站 Physical SYNC：name={} type={} medium={} quick_bytes={}",
            sync.device.name,
            sync.device.device_type,
            sync.medium,
            sync.quick_connect.len()
        ));
        self.peer_type = sync.device.device_type as u32;
        self.peer_short = String::from_utf8_lossy(&sync.device.short_id).into_owned();
        let mut reply = PhysicalFrame::sync_reply(
            epoch_ms(),
            physical_device(
                self.session.account(),
                &self.session.inner.options.device_name,
                self.session.account().device_type,
                crate::session::MEDIUM_LAN | self.transport.medium(),
                self.session.inner.options.wifi_capabilities.as_ref(),
            ),
            self.transport.medium(),
        );
        reply.id = id;
        self.queue_on(physical_id, &reply.encode())?;
        self.physical_ready = true;
        self.phase("入站 Physical 同步完成，等待同账号认证");
        if !sync.quick_connect.is_empty() {
            tracing::debug!(
                "[session] 入站 QuickConnect raw bytes={} hex={}",
                sync.quick_connect.len(),
                hex::encode(&sync.quick_connect)
            );
            let root = Fields::decode(&sync.quick_connect)?;
            let private = root.message::<Fields>(2)?;
            self.phase(&format!(
                "入站 QuickConnect：private_bytes={} logical_rows={}",
                sync.quick_connect.len(),
                private.repeated_bytes(1).len()
            ));
            for data in private.repeated_bytes(1) {
                tracing::debug!(
                    "[session] 入站 QuickConnect row bytes={} hex={}",
                    data.len(),
                    hex::encode(data)
                );
                let envelope = if data.first().is_some_and(|byte| byte >> 3 == 2) {
                    LogicalEnvelope::decode(data)?
                } else {
                    LogicalEnvelope::decode_quick(data)?
                };
                self.receive_logical(envelope, physical_id).await?;
            }
        }
        Ok(())
    }

    async fn handle_upgrade_sync(
        &mut self,
        _sync: PhysicalSyncReply,
        physical_id: u8,
    ) -> Result<()> {
        let upgrade = self.upgrade.as_ref().context("没有待附着的 P2P Logical")?;
        ensure!(!upgrade.introduced, "P2P Physical重复同步响应");
        let full = upgrade.full;
        let introduction = upgrade.introduction.clone();
        let (local, body) = {
            let connection = self
                .connections
                .get(&full)
                .context("待升级 Logical已关闭")?;
            let body = LogicalBody::Ack {
                introduction: Some(introduction),
            }
            .encode_inner();
            (connection.local, connection.auth.encrypt(&body)?)
        };
        let envelope = LogicalEnvelope {
            local,
            remote: 0,
            full,
            encrypted: true,
            body,
        };
        self.queue_on(physical_id, &envelope.encode())?;
        self.upgrade.as_mut().unwrap().introduced = true;
        self.phase(&format!(
            "P2P Physical已同步，发送认证 Introduction full={full} local={local} remote=0"
        ));
        Ok(())
    }

    fn handle_peer_sync(&mut self, sync: PhysicalSyncReply) -> Result<()> {
        if !self.physical_ready {
            self.physical_ready = true;
            self.peer_type = sync.device.device_type as u32;
            self.peer_short = String::from_utf8_lossy(&sync.device.short_id).into_owned();
            self.phase("AccountPair 同账号认证");
            self.network_full = self.start_logical(
                NETWORKING.into(),
                crate::session::TRUST_PAIR,
                SdkPlatform::Linux,
                String::new(),
                Duration::from_secs(10),
                None,
            )?;
        }
        Ok(())
    }

    async fn handle_physical_close(&mut self, physical_id: u8) -> Result<()> {
        self.phase(&format!(
            "手机请求关闭 Physical：transport={}",
            if physical_id == 0 {
                "BLE/primary"
            } else {
                "P2P"
            }
        ));
        self.queue_on(physical_id, &PhysicalFrame::close_ack(epoch_ms()).encode())?;
        if physical_id == 0
            && self.closing.is_none()
            && self.physical_active(1)
            && (self.secondary_ready || self.upgrade.is_some())
        {
            self.retire_primary().await?;
            return Ok(());
        }
        if physical_id == 1
            && let Some(upgrade) = &self.upgrade
        {
            let full = upgrade.full;
            self.fail_upgrade(full, "手机关闭待升级的 P2P Physical".into());
            return Ok(());
        }
        if let Some((deadline, _, sent)) = &mut self.closing {
            *deadline = Instant::now() + Duration::from_millis(100);
            *sent = true;
        } else {
            let (reply, _) = oneshot::channel();
            self.closing = Some((Instant::now() + Duration::from_millis(100), reply, true));
        }
        Ok(())
    }
}

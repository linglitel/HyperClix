//! Physical link lifecycle, link upgrade and close negotiation.
use super::super::{MediaCommand, SessionEvent};
use super::PhysicalActor;
use crate::transport::physical::PhysicalTransport;
use crate::{
    Result,
    protocol::messages::lyra::{LogicalBody, LogicalClose, LogicalFrame},
};
use anyhow::Context;
use std::{io, sync::atomic::Ordering};

impl PhysicalActor {
    pub(super) async fn flush(&mut self) -> Result<()> {
        self.transport.flush().await?;
        if let Some(secondary) = &mut self.secondary {
            if let Err(error) = secondary.flush().await {
                if let Some(upgrade) = &self.upgrade {
                    let full = upgrade.full;
                    self.fail_upgrade(full, format!("P2P Physical发送失败：{error}"));
                } else {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    pub(super) fn queue_on(&mut self, physical: u8, message: &[u8]) -> Result<()> {
        if physical == 0 {
            self.transport.queue(message)
        } else {
            self.secondary
                .as_mut()
                .context("P2P Physical尚未连接")?
                .queue(message)
        }
    }
    pub(super) fn physical_active(&self, physical: u8) -> bool {
        if physical == 0 {
            self.transport.is_active()
        } else {
            self.secondary
                .as_ref()
                .is_some_and(PhysicalTransport::is_active)
        }
    }
    pub(super) async fn retire_primary(&mut self) -> Result<()> {
        self.transport.flush().await?;
        let old = std::mem::replace(&mut self.transport, PhysicalTransport::Closed);
        old.close().await?;
        self.physical_ready = false;
        let moving = self.upgrade.as_ref().map(|upgrade| upgrade.full);
        let retired = self
            .connections
            .iter()
            .filter_map(|(full, c)| (c.physical == 0 && Some(*full) != moving).then_some(*full))
            .collect::<Vec<_>>();
        for full in &retired {
            let mut connection = self.connections.remove(full).unwrap();
            if self.network_full == *full {
                self.network_full = 0;
            }
            if let Some(reply) = connection.reply.take() {
                let _ = reply.send(Err(anyhow::anyhow!("原 BLE Physical已正常关闭")));
            }
            if let Some(channel) = &connection.channel {
                channel.notify_closed("原 BLE Physical已正常关闭".into());
                let _ = channel.inner.commands.send(MediaCommand::Stop).await;
            }
        }
        for channels in self.accepted.values_mut() {
            channels.retain(|channel| !channel.inner.closed.load(Ordering::Acquire));
        }
        self.phase(&format!(
            "旧 BLE Physical已正常退休：logicals={} pending_upgrade={}；继续使用 P2P Physical",
            retired.len(),
            moving.is_some()
        ));
        Ok(())
    }
    pub(super) fn upgrade_event(&self, full: u32, local: u8, remote: u8, frame: &[u8]) {
        let event = SessionEvent::LinkUpgrade {
            full,
            local,
            remote,
            frame: frame.to_vec(),
        };
        let _ = self.session.inner.events.send(event);
    }
    pub(super) fn fail_upgrade(&mut self, full: u32, reason: String) {
        let _ = self
            .session
            .inner
            .events
            .send(SessionEvent::LinkUpgradeFailed {
                full,
                reason: reason.clone(),
            });
        if self
            .upgrade
            .as_ref()
            .is_some_and(|upgrade| upgrade.full == full)
        {
            let upgrade = self.upgrade.take().unwrap();
            self.session
                .set_link_route(upgrade.previous_route.0, upgrade.previous_route.1);
            let _ = upgrade.reply.send(Err(anyhow::anyhow!(reason.clone())));
            self.secondary_ready = false;
            if let Some(connection) = self.connections.remove(&full) {
                if let Some(channel) = connection.channel {
                    channel.notify_closed(reason);
                    tokio::spawn(async move {
                        let _ = channel.inner.commands.send(MediaCommand::Stop).await;
                    });
                }
                let close = LogicalFrame {
                    local: connection.local,
                    remote: connection.remote,
                    full,
                    encrypted: false,
                    body: LogicalBody::Close(LogicalClose { reason: 15001 }),
                }
                .encode();
                let _ = self.queue_on(connection.physical, &close);
            }
            if let Some(mut secondary) = self.secondary.take() {
                tokio::spawn(async move {
                    let _ = secondary.flush().await;
                    let _ = secondary.close().await;
                });
            }
        }
    }
    pub(super) fn physical_close_eof(&self, error: &anyhow::Error) -> bool {
        self.closing.as_ref().is_some_and(|(_, _, sent)| *sent)
            && error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof)
            })
    }
}

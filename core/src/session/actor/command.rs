//! Commands sent to the physical actor.
use super::super::device::physical_sync;
use super::super::{Command, MediaCommand, SdkPlatform};
use super::logical::network_transport;
use super::{PendingPhysical, PhysicalActor};
use crate::Result;
use crate::protocol::{
    messages::{
        device::{DeviceService, NetworkingSync},
        lyra::{LogicalBody, LogicalClose, LogicalFrame},
    },
    pb::Message,
};
use crate::transport::physical::PhysicalTransport;
use anyhow::{Context, ensure};
use std::{
    collections::VecDeque,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

impl PhysicalActor {
    pub(super) async fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::BeginLinkUpgrade {
                query,
                timeout,
                reply,
            } => {
                let _ = reply.send(self.begin_link_upgrade(query, timeout));
            }
            Command::CancelLinkUpgrade { full, reply } => {
                let result = self.cancel_link_upgrade(full).await;
                let _ = reply.send(result);
            }
            Command::UpgradePhysical {
                io,
                full,
                introduction,
                local,
                peer,
                timeout,
                reply,
            } => {
                if let Err(error) =
                    self.start_upgrade_physical(io, full, introduction, local, peer, timeout, reply)
                {
                    // The reply sender was moved into the failed construction.
                    self.phase(&format!("P2P Physical准备失败：{error:#}"));
                }
            }
            Command::Open {
                service,
                trust,
                platform,
                user_data,
                reply,
            } => self.open_logical(service, trust, platform, user_data, reply),
            Command::Accept { service, reply } => self.accept_channel(service, reply),
            Command::Networking {
                module,
                data,
                reply,
            } => {
                let _ = reply.send(self.send_networking(module, &data));
            }
            Command::SetService {
                app,
                name,
                data,
                reply,
            } => {
                let _ = reply.send(self.update_service(&app, &name, data));
            }
            Command::CloseChannel(id, force, reply) => {
                let result = self.close_channel(id, force).await;
                let _ = reply.send(result);
            }
            Command::Close(reply) => self.begin_close(reply).await?,
        }
        Ok(())
    }

    fn begin_link_upgrade(&mut self, query: Vec<u8>, timeout: Duration) -> Result<u32> {
        ensure!(
            self.transport.medium() == crate::session::MEDIUM_BLE && !self.secondary_ready,
            "链路升级需要已认证的 BLE 会话"
        );
        ensure!(
            !self
                .connections
                .values()
                .any(|connection| connection.upgrade_query.is_some()),
            "链路升级正在进行"
        );
        let full = self.start_logical(
            crate::session::CAST_SERVICE.into(),
            crate::session::TRUST_PAIR,
            SdkPlatform::Linux,
            String::new(),
            timeout,
            None,
        )?;
        self.connections.get_mut(&full).unwrap().upgrade_query = Some(query);
        self.phase(&format!(
            "开始 CAST Logical 的 P2P medium32 升级协商 full={full} timeout_ms={}",
            timeout.as_millis()
        ));
        Ok(full)
    }

    async fn cancel_link_upgrade(&mut self, full: u32) -> Result<()> {
        let result = if let Some(connection) = self.connections.remove(&full) {
            if let Some(channel) = &connection.channel {
                channel.notify_closed("链路升级已取消".into());
                let _ = channel.inner.commands.send(MediaCommand::Stop).await;
            }
            if self.physical_active(connection.physical) {
                self.queue_on(
                    connection.physical,
                    &close_frame(connection.local, connection.remote, full),
                )
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        self.fail_upgrade(full, "链路升级已取消".into());
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn start_upgrade_physical(
        &mut self,
        io: crate::transport::physical::PhysicalIo,
        full: u32,
        introduction: Vec<u8>,
        local: std::net::Ipv4Addr,
        peer: std::net::Ipv4Addr,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        let connection = self
            .connections
            .get(&full)
            .context("升级 CAST Logical不存在")?;
        ensure!(
            connection.upgrade_query.is_some() && connection.phase == 3,
            "升级 Logical尚未收到服务响应"
        );
        ensure!(self.secondary.is_none(), "P2P Physical已存在");
        let mut secondary = PhysicalTransport::udp_medium(io, crate::session::MEDIUM_P2P)?;
        secondary.queue(
            &physical_sync(
                self.session.account(),
                &self.session.inner.options.device_name,
                crate::session::MEDIUM_P2P,
                self.session.inner.options.wifi_capabilities.as_ref(),
            )
            .encode(),
        )?;
        self.secondary = Some(secondary);
        self.upgrade = Some(PendingPhysical {
            full,
            introduction,
            local,
            peer,
            previous_route: (self.session.local_ip(), self.session.peer_ip()),
            deadline: Instant::now() + timeout,
            introduced: false,
            reply,
        });
        Ok(())
    }

    fn open_logical(
        &mut self,
        service: String,
        trust: u32,
        platform: SdkPlatform,
        user_data: String,
        reply: tokio::sync::oneshot::Sender<Result<crate::session::Channel>>,
    ) {
        if self.session.peer_ip().is_unspecified() || self.session.local_ip().is_unspecified() {
            let _ = reply.send(Err(anyhow::anyhow!(
                "BLE同账号连接已建立，请先协商并接通 Wi-Fi Direct 再打开业务"
            )));
            return;
        }
        if service == crate::session::CAST_SERVICE
            && let Some(channel) = self
                .accepted
                .get_mut(&service)
                .and_then(VecDeque::pop_front)
        {
            let _ = reply.send(Ok(channel));
            return;
        }
        if let Err(error) = self.start_logical(
            service,
            trust,
            platform,
            user_data,
            Duration::from_secs(10),
            Some(reply),
        ) {
            self.phase(&format!("业务通道创建失败：{error}"));
        }
    }

    fn accept_channel(
        &mut self,
        service: String,
        reply: tokio::sync::oneshot::Sender<Result<crate::session::Channel>>,
    ) {
        if let Some(channel) = self
            .accepted
            .get_mut(&service)
            .and_then(VecDeque::pop_front)
        {
            if let Err(Ok(channel)) = reply.send(Ok(channel)) {
                self.accepted
                    .entry(service)
                    .or_default()
                    .push_front(channel);
            }
        } else {
            self.waiters.entry(service).or_default().push_back(reply);
        }
    }

    fn send_networking(&mut self, module: u8, data: &[u8]) -> Result<()> {
        let connection = self
            .connections
            .get(&self.network_full)
            .context("Networking已关闭")?;
        let physical = connection.physical;
        let packet = network_transport(connection, module, data)?;
        self.queue_on(physical, &packet)
    }

    fn update_service(&mut self, app: &str, name: &str, data: Option<Vec<u8>>) -> Result<()> {
        let mut device = self.own_device.clone();
        device
            .services
            .retain(|service| !(service.name == name && service.app == app));
        if let Some(data) = &data {
            device.services.push(DeviceService {
                name: name.to_owned(),
                app: app.to_owned(),
                data: data.clone(),
            });
        }
        self.replace_own_device(device);
        let connection = self
            .connections
            .get(&self.network_full)
            .context("Networking已关闭")?;
        let sync = NetworkingSync {
            version: 1,
            device: self.own_device.clone(),
        }
        .encode();
        let physical = connection.physical;
        let packet = network_transport(connection, 0, &sync)?;
        self.queue_on(physical, &packet)?;
        if app == "pcmanager" && name == "universalClipboard" {
            self.clipboard_active = data.is_some();
            if data.is_none() {
                for channel in self
                    .accepted
                    .remove(crate::protocol::service::CLIPBOARD_LOCAL)
                    .unwrap_or_default()
                {
                    tokio::spawn(async move {
                        let _ = channel.close().await;
                    });
                }
            }
        }
        self.phase(&format!(
            "Networking：{}服务 {app}:{name} state_version={}",
            if data.is_some() { "发布" } else { "撤销" },
            self.service_version
        ));
        Ok(())
    }

    async fn close_channel(&mut self, id: u32, force: bool) -> Result<()> {
        let Some(full) = self
            .connections
            .iter()
            .find_map(|(full, c)| (c.sdk_id == id).then_some(*full))
        else {
            return Ok(());
        };
        let (physical, packet) = {
            let Some(connection) = self.connections.get_mut(&full) else {
                return Ok(());
            };
            if !force && !connection.closing {
                connection.closing = true;
                if let Some(channel) = &connection.channel {
                    channel.inner.closed.store(true, Ordering::Release);
                    let _ = channel.inner.commands.send(MediaCommand::Stop).await;
                }
                (
                    connection.physical,
                    close_frame(connection.local, connection.remote, connection.full),
                )
            } else {
                let connection = self.connections.remove(&full).unwrap();
                if let Some(channel) = &connection.channel {
                    channel.notify_closed("通道关闭完成".into());
                    let _ = channel.inner.commands.send(MediaCommand::Stop).await;
                }
                (
                    connection.physical,
                    close_frame(connection.local, connection.remote, connection.full),
                )
            }
        };
        self.queue_on(physical, &packet)?;
        Ok(())
    }

    async fn begin_close(&mut self, reply: tokio::sync::oneshot::Sender<Result<()>>) -> Result<()> {
        let mut closing = Vec::new();
        for connection in self.connections.values() {
            if connection.remote != 0 && self.physical_active(connection.physical) {
                closing.push((
                    connection.physical,
                    close_frame(connection.local, connection.remote, connection.full),
                ));
            }
            if let Some(channel) = &connection.channel {
                channel.inner.closed.store(true, Ordering::Release);
                let _ = channel.inner.commands.send(MediaCommand::Stop).await;
            }
        }
        for (physical, packet) in closing {
            self.queue_on(physical, &packet)?;
        }
        self.closing = Some((Instant::now() + Duration::from_millis(200), reply, false));
        Ok(())
    }
}

fn close_frame(local: u8, remote: u8, full: u32) -> Vec<u8> {
    LogicalFrame {
        local,
        remote,
        full,
        encrypted: false,
        body: LogicalBody::Close(LogicalClose { reason: 15001 }),
    }
    .encode()
}

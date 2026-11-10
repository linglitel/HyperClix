//! Inbound Logical authentication and service dispatch.
use super::super::MediaCommand;
use super::super::logical::network_transport;
use super::super::{LogicalConnection, PhysicalActor};
use crate::{
    Result,
    protocol::{
        messages::{
            device::NetworkingSync,
            lyra::{
                AuthExchange, LogicalBody, LogicalEnvelope, LogicalFrame, LogicalSync,
                ServiceRequest, ServiceResponse, uid_feature,
            },
        },
        pb::{Fields, Message},
    },
    session::{INCOMING, NETWORKING, SdkPlatform},
    transport::auth::Auth,
};
use anyhow::{Context, ensure};
enum InboundConnection {
    Existing,
    Quick(Vec<u8>),
    Ignore,
}

impl PhysicalActor {
    pub(super) async fn receive_logical(
        &mut self,
        envelope: LogicalEnvelope,
        physical_id: u8,
    ) -> Result<()> {
        let full = envelope.full;
        let inline_auth = match self.prepare_inbound(&envelope, physical_id).await? {
            InboundConnection::Existing => None,
            InboundConnection::Quick(cache) => Some(cache),
            InboundConnection::Ignore => return Ok(()),
        };
        let mut connection = self.connections.remove(&full).unwrap();
        connection.remote = if connection.outgoing {
            envelope.remote
        } else {
            envelope.local
        };
        connection.peer_local = envelope.local;
        let clear = if let Some(auth) = inline_auth {
            auth
        } else if envelope.encrypted {
            connection.auth.decrypt(&envelope.body)?
        } else {
            envelope.body
        };
        let body = LogicalBody::decode_inner(&clear)?;
        let candidate = physical_id == 1
            && self
                .upgrade
                .as_ref()
                .is_some_and(|upgrade| upgrade.full == full && upgrade.introduced);
        ensure!(
            connection.physical == physical_id || candidate,
            "Logical到达错误的 Physical"
        );
        if let Some(introduction) = body.introduction() {
            ensure!(
                envelope.encrypted && connection.auth.complete(),
                "拒绝未认证的链路升级请求"
            );
            self.upgrade_event(full, connection.local, connection.remote, introduction);
            if candidate {
                ensure!(
                    matches!(body, LogicalBody::Introduction(_))
                        && Fields::decode(introduction)?.uint(1) == 4,
                    "P2P候选 Physical未返回 Introduction ACK"
                );
                connection.physical = 1;
                self.secondary_ready = true;
                let upgrade = self.upgrade.as_ref().unwrap();
                self.session.set_link_route(upgrade.local, upgrade.peer);
                self.phase(&format!(
                    "P2P Introduction已认证，开始 SDK full={full} local={} remote={}",
                    connection.local, connection.remote
                ));
                self.start_sdk(&connection)?;
            }
        }
        ensure!(
            connection.physical == physical_id,
            "候选 Physical缺少已认证 Introduction ACK"
        );
        self.accept_introduction(&mut connection, &body, candidate, full)?;
        ensure!(
            connection.physical == physical_id,
            "候选 Physical缺少已认证 Introduction ACK"
        );
        let keep = self
            .handle_logical_body(&mut connection, body, full, envelope.encrypted)
            .await?;
        if keep {
            self.connections.insert(full, connection);
        }
        Ok(())
    }

    async fn prepare_inbound(
        &mut self,
        envelope: &LogicalEnvelope,
        physical_id: u8,
    ) -> Result<InboundConnection> {
        let full = envelope.full;
        if self.connections.contains_key(&full) {
            return Ok(InboundConnection::Existing);
        }
        if envelope.encrypted {
            self.phase(&format!(
                "未知入站 Logical：full={full} physical={physical_id} encrypted=true"
            ));
            return Ok(InboundConnection::Ignore);
        }
        let body = LogicalBody::decode_inner(&envelope.body)?;
        self.phase(&format!(
            "未知入站 Logical：full={full} physical={physical_id} kind={}",
            body.kind()
        ));
        let LogicalBody::Sync(sync) = body else {
            return Ok(InboundConnection::Ignore);
        };
        let service = sync.service.clone();
        let cached = if sync.key_index != 0 {
            self.phase(&format!(
                "入站 Logical请求旧密钥复用：full={full} key_index={}；本机无该缓存，回退完整认证",
                sync.key_index
            ));
            None
        } else if !sync.cache.is_empty() {
            let cached = Fields::decode(&sync.cache)?;
            self.phase(&format!(
                "入站 QuickConnect cache：full={full} kind={}",
                cached.uint(1)
            ));
            Some(cached)
        } else {
            None
        };
        if service != NETWORKING && !INCOMING.contains(&service.as_str()) {
            return Ok(InboundConnection::Ignore);
        }
        if service == crate::protocol::service::CLIPBOARD_LOCAL && !self.clipboard_active {
            return Ok(InboundConnection::Ignore);
        }
        let local = (128u8..=255)
            .find(|id| !self.connections.values().any(|c| c.local == *id))
            .context("入站逻辑通道数量已满")?;
        let auth = Auth::server(
            self.session.account(),
            self.peer_type,
            self.peer_short.clone(),
        )?;
        let platform = if service.contains("miShare") || service.contains("miLyraShare") {
            SdkPlatform::Windows
        } else {
            SdkPlatform::Linux
        };
        let user_data = if service == NETWORKING {
            Vec::new()
        } else {
            crate::protocol::messages::lyra::UserInfo {
                platform: platform as u32,
                user_data: String::new(),
            }
            .encode()
        };
        let quick_request = cached
            .as_ref()
            .filter(|cached| cached.uint(1) == 1)
            .map(|_| sync.cache.clone());
        let connection = LogicalConnection::new(
            physical_id,
            local,
            envelope.local,
            full,
            service,
            sync.trust,
            user_data,
            auth,
            false,
            sync.timeout_ms,
            None,
            quick_request,
        );
        let response = LogicalFrame {
            local: connection.local,
            remote: connection.remote,
            full,
            encrypted: false,
            body: LogicalBody::Sync(LogicalSync {
                timeout_ms: 10000,
                trust: crate::session::TRUST_PAIR,
                service: connection.service.clone(),
                uid_feature: uid_feature(&self.session.account().uid),
                key_index: 0,
                cache: Vec::new(),
            }),
        };
        self.queue_on(physical_id, &response.encode())?;
        self.connections.insert(full, connection);
        if self.connections[&full].service == NETWORKING {
            self.network_full = full;
        }
        if !cached.as_ref().is_some_and(|cached| cached.uint(1) == 6) {
            return Ok(InboundConnection::Ignore);
        }
        self.phase(&format!(
            "入站 QuickConnect Logical：full={full} auth_bytes={}",
            sync.cache.len()
        ));
        Ok(InboundConnection::Quick(sync.cache))
    }

    fn accept_introduction(
        &mut self,
        connection: &mut LogicalConnection,
        body: &LogicalBody,
        candidate: bool,
        full: u32,
    ) -> Result<()> {
        let Some(introduction) = body.introduction() else {
            return Ok(());
        };
        ensure!(connection.auth.complete(), "拒绝未认证的链路升级请求");
        self.upgrade_event(full, connection.local, connection.remote, introduction);
        if candidate {
            ensure!(
                matches!(body, LogicalBody::Introduction(_))
                    && Fields::decode(introduction)?.uint(1) == 4,
                "P2P候选 Physical未返回 Introduction ACK"
            );
            connection.physical = 1;
            self.secondary_ready = true;
            let upgrade = self.upgrade.as_ref().unwrap();
            self.session.set_link_route(upgrade.local, upgrade.peer);
            self.phase(&format!(
                "P2P Introduction已认证，开始 SDK full={full} local={} remote={}",
                connection.local, connection.remote
            ));
            self.start_sdk(connection)?;
        }
        Ok(())
    }

    async fn handle_logical_body(
        &mut self,
        connection: &mut LogicalConnection,
        body: LogicalBody,
        full: u32,
        encrypted: bool,
    ) -> Result<bool> {
        tracing::info!(
            target: "session",
            "[session] inbound logical full={full} kind={} service={} outgoing={} phase={}",
            body.kind(),
            connection.service,
            connection.outgoing,
            connection.phase
        );
        match body {
            LogicalBody::Sync(_) if connection.outgoing && connection.phase == 0 => {
                let notify = connection.auth.client_notify();
                self.send_inner(
                    connection,
                    LogicalBody::Auth(AuthExchange {
                        kind: 1,
                        payload: notify,
                    }),
                    false,
                )?;
                connection.phase = 1;
            }
            LogicalBody::Auth(exchange) => {
                self.handle_auth(connection, exchange, full)?;
            }
            LogicalBody::ServiceRequest { request, .. } if !connection.outgoing => {
                ensure!(encrypted, "拒绝未认证的业务请求");
                self.respond_request(connection, request)?;
            }
            LogicalBody::ServiceResponse(response) if connection.outgoing => {
                return self.handle_service_response(connection, response, full);
            }
            LogicalBody::Ack { .. } => {
                self.handle_ack(connection).await?;
            }
            LogicalBody::Close(_) => {
                return self.handle_close(connection, full).await;
            }
            _ => {}
        }
        Ok(true)
    }

    fn handle_auth(
        &mut self,
        connection: &mut LogicalConnection,
        exchange: AuthExchange,
        full: u32,
    ) -> Result<()> {
        if !connection.outgoing {
            let header = Fields::decode(&exchange.payload)?;
            if (header.uint(1), header.uint(2)) == (4, 5) && connection.phase == 0 {
                let peer_key = match &self.cached_peer {
                    Some(key) => key.clone(),
                    None => {
                        let cached = self
                            .session
                            .account()
                            .load_peer_public_key(&self.peer_short, self.peer_type)?;
                        let key = cached.context(
                            "入站证书认证缺少已验证的手机证书缓存；请先主动连接完成同账号认证",
                        )?;
                        self.phase(&format!(
                            "入站证书认证：已加载并验证手机公钥缓存 full={full} service={}",
                            connection.service
                        ));
                        self.cached_peer = Some(key.clone());
                        key
                    }
                };
                connection.auth = Auth::certificate_server(
                    self.session.account(),
                    self.peer_type,
                    self.peer_short.clone(),
                    peer_key,
                )?;
            }
        }
        if let Some(reply) = connection.auth.handle(&exchange.payload)? {
            self.send_inner(
                connection,
                LogicalBody::Auth(AuthExchange {
                    kind: exchange.kind,
                    payload: reply,
                }),
                false,
            )?;
        }
        connection.phase = 1;
        if !connection.auth.complete() {
            return Ok(());
        }
        if let Some(key) = &connection.auth.peer_key {
            self.cached_peer = Some(key.clone());
        }
        if connection.outgoing {
            let request = ServiceRequest {
                service: connection.service.clone(),
                user_data: connection.user_data.clone(),
                trust: connection.trust,
                timeout_ms: connection.timeout_ms,
                medium: connection.upgrade_query.as_ref().map(|_| 32),
            };
            self.send_inner(
                connection,
                LogicalBody::ServiceRequest {
                    request,
                    upgrade_query: connection.upgrade_query.clone(),
                },
                true,
            )?;
        } else if let Some(request) = connection
            .auth
            .take_private_data()
            .or_else(|| connection.quick_request.take())
        {
            let body = LogicalBody::decode_inner(&request).context("解析已认证的内嵌 Logical")?;
            self.phase(&format!(
                "入站认证携带 Logical：full={full} kind={} bytes={}",
                body.kind(),
                request.len()
            ));
            if let LogicalBody::ServiceRequest { request, .. } = body {
                self.respond_request(connection, request)?;
            }
            self.phase(&format!(
                "入站 QuickConnect 已认证：full={full} service={}",
                connection.service
            ));
        }
        connection.phase = 2;
        Ok(())
    }

    fn handle_service_response(
        &mut self,
        connection: &mut LogicalConnection,
        response: ServiceResponse,
        full: u32,
    ) -> Result<bool> {
        tracing::info!(
            target: "session",
            "[session] ServiceResponse full={full} service={} status={} phase={}",
            connection.service,
            response.status,
            connection.phase
        );
        if response.status != 0 {
            if let Some(reply) = connection.reply.take() {
                let _ = reply.send(Err(anyhow::anyhow!(
                    "手机拒绝业务 {}：{}",
                    connection.service,
                    response.status
                )));
            }
            if connection.service == NETWORKING {
                anyhow::bail!("手机拒绝Networking：{}", response.status);
            }
            if connection.upgrade_query.is_some() {
                self.fail_upgrade(full, format!("手机拒绝升级业务：{}", response.status));
            }
            return Ok(false);
        }
        if connection.upgrade_query.is_some() {
            self.phase(&format!("升级 Logical服务已接受，等待 P2P附着 full={full}"));
            connection.phase = 3;
            return Ok(true);
        }
        self.send_inner(connection, LogicalBody::Ack { introduction: None }, true)?;
        if connection.service == NETWORKING {
            self.phase("Networking 设备和服务同步");
            let sync = NetworkingSync {
                version: 1,
                device: self.own_device.clone(),
            }
            .encode();
            self.queue_on(
                connection.physical,
                &network_transport(connection, 0, &sync)?,
            )?;
        } else {
            self.start_sdk(connection)?;
            self.schedule_sdk_retry(connection.full);
        }
        connection.phase = 3;
        Ok(true)
    }

    /// OS4 can confirm a logical channel several seconds after the service
    /// response. The first PeerPortRequest is then sent before the phone's
    /// channel exists and is dropped, so resend it while the caller waits.
    fn schedule_sdk_retry(&self, full: u32) {
        let session = self.session.clone();
        tokio::spawn(async move {
            for delay in [4u64, 8] {
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                session.retry_sdk(full).await;
            }
        });
    }

    async fn handle_ack(&mut self, connection: &mut LogicalConnection) -> Result<()> {
        let accepted_outgoing = connection.outgoing
            && connection.phase == 2
            && connection.channel.is_none()
            && connection.upgrade_query.is_none();
        connection.phase = 3;
        if connection.service == NETWORKING && !connection.outgoing {
            self.phase("入站 Networking 设备和服务同步");
            let sync = NetworkingSync {
                version: 1,
                device: self.own_device.clone(),
            }
            .encode();
            self.queue_on(
                connection.physical,
                &network_transport(connection, 0, &sync)?,
            )?;
        } else if !connection.outgoing
            && let Some(request) = connection.quick_peer_port.take()
        {
            let request = Fields::decode(&request)?;
            self.respond_peer_port(connection, request.uint(1), request.bytes(4))
                .await?;
        } else if accepted_outgoing && connection.service != NETWORKING {
            // OS4 confirms a service channel with a logical Ack instead of a
            // separate ServiceResponse. Continue with the SDK port exchange
            // here as well, otherwise the phone waits for PeerPortRequest and
            // the caller times out at the channel-open stage.
            self.phase(&format!("手机已确认业务 {}，开始 SDK", connection.service));
            self.start_sdk(connection)?;
        }
        Ok(())
    }

    async fn handle_close(&mut self, connection: &LogicalConnection, full: u32) -> Result<bool> {
        if connection.upgrade_query.is_some() {
            self.fail_upgrade(full, "手机关闭升级 Logical".into());
        }
        if let Some(channel) = &connection.channel {
            channel.notify_closed("手机关闭业务通道".into());
            let _ = channel.inner.commands.send(MediaCommand::Stop).await;
        }
        Ok(false)
    }
}

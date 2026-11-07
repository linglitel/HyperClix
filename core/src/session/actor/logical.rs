//! Logical service handshake and Networking device helpers.
use super::super::{Channel, NETWORKING, SdkPlatform, random_id};
use super::LogicalConnection;
use super::PhysicalActor;
use crate::{
    Result,
    protocol::{
        messages::{
            device::NetworkDevice,
            lyra::{
                LogicalBody, LogicalEnvelope, LogicalFrame, LogicalSync, ServiceRequest,
                ServiceResponse, UserInfo, uid_feature,
            },
            sdk::{self, ChannelControl},
        },
        pb::{Fields, Message},
    },
    transport::auth::Auth,
};
use anyhow::{Context, ensure};
use std::time::Duration;
use tokio::{net::UdpSocket, sync::oneshot};

impl PhysicalActor {
    pub(super) fn start_logical(
        &mut self,
        service: String,
        trust: u32,
        platform: SdkPlatform,
        user_data: String,
        timeout: Duration,
        reply: Option<oneshot::Sender<Result<Channel>>>,
    ) -> Result<u32> {
        let local = self.allocate_local().context("并行逻辑通道数量已满")?;
        let full = random_id();
        let auth = Auth::client(self.session.account(), self.peer_type)?;
        let timeout_ms = timeout.as_millis() as u64;
        let physical = if self.secondary_ready { 1 } else { 0 };
        let frame = LogicalFrame {
            local,
            remote: 0,
            full,
            encrypted: false,
            body: LogicalBody::Sync(LogicalSync {
                timeout_ms,
                trust,
                service: service.clone(),
                uid_feature: uid_feature(&self.session.account().uid),
                key_index: 0,
                cache: Vec::new(),
            }),
        };
        self.queue_on(physical, &frame.encode())?;
        self.connections.insert(
            full,
            LogicalConnection::new(
                physical,
                local,
                0,
                full,
                service.clone(),
                trust,
                if service == NETWORKING {
                    Vec::new()
                } else {
                    UserInfo {
                        platform: platform as u32,
                        user_data,
                    }
                    .encode()
                },
                auth,
                true,
                timeout_ms,
                reply,
                None,
            ),
        );
        Ok(full)
    }
    fn allocate_local(&mut self) -> Option<u8> {
        for _ in 0..127 {
            let id = self.next_local;
            self.next_local = if id >= 127 { 1 } else { id + 1 };
            if !self.connections.values().any(|c| c.local == id) {
                return Some(id);
            }
        }
        None
    }
    pub(super) fn send_inner(
        &mut self,
        c: &LogicalConnection,
        body: LogicalBody,
        encrypted: bool,
    ) -> Result<()> {
        let body = body.encode_inner();
        let body = if encrypted {
            c.auth.encrypt(&body)?
        } else {
            body
        };
        let envelope = LogicalEnvelope {
            local: c.local,
            remote: c.remote,
            full: c.full,
            encrypted,
            body,
        };
        self.queue_on(c.physical, &envelope.encode())
    }
    pub(super) fn respond_request(
        &mut self,
        c: &mut LogicalConnection,
        request: ServiceRequest,
    ) -> Result<()> {
        ensure!(c.auth.complete(), "拒绝认证未完成的业务请求");
        ensure!(request.service == c.service, "逻辑服务名发生变化");
        if c.service_responded {
            // The phone retransmits the same ServiceRequest when its ACK was
            // delayed. Re-send the cached acceptance; creating a second SDK
            // port here leaves an extra media channel and confuses cameraP2P.
            return self.send_inner(
                c,
                LogicalBody::ServiceResponse(ServiceResponse {
                    status: 0,
                    user_data: c.user_data.clone(),
                    version: Some(1),
                }),
                true,
            );
        }
        if c.service != NETWORKING && !request.user_data.is_empty() {
            let user = Fields::decode(&request.user_data)?;
            if user.has(10) {
                c.quick_peer_port = Some(user.bytes(10).to_vec());
                self.phase(&format!(
                    "入站快速建链携带 SDK 端口请求：full={} service={}",
                    c.full, c.service
                ));
            }
        }
        c.service_responded = true;
        self.phase(&format!(
            "入站 Logical 已接受：full={} service={}，等待 ACK",
            c.full, c.service
        ));
        self.send_inner(
            c,
            LogicalBody::ServiceResponse(ServiceResponse {
                status: 0,
                user_data: c.user_data.clone(),
                version: Some(1),
            }),
            true,
        )
    }
    pub(super) async fn respond_peer_port(
        &mut self,
        c: &mut LogicalConnection,
        request_id: u64,
        key: &[u8],
    ) -> Result<()> {
        if let Some(port) = c.sdk_port {
            let reply = ChannelControl::peer_port_response(request_id, c.sdk_id, port);
            self.queue_on(
                c.physical,
                &sdk::wrap(c.remote, &c.auth.encrypt(&reply.encode())?),
            )?;
            self.phase(&format!(
                "入站 SDK 端口重复请求，沿用 full={} service={} port={port}",
                c.full, c.service
            ));
            return Ok(());
        }
        c.trans_key = key.to_vec();
        ensure!(c.trans_key.len() == 32, "PeerPort密钥长度错误");
        let socket = UdpSocket::bind((self.session.local_ip(), 0)).await?;
        let port = socket.local_addr()?.port();
        let channel = self.spawn_media(c, socket, None, c.sdk_id, true)?;
        c.channel = Some(channel);
        c.sdk_port = Some(port);
        let reply = ChannelControl::peer_port_response(request_id, c.sdk_id, port);
        self.queue_on(
            c.physical,
            &sdk::wrap(c.remote, &c.auth.encrypt(&reply.encode())?),
        )?;
        self.phase(&format!(
            "入站 SDK 端口已响应：full={} service={} port={port}，等待关联",
            c.full, c.service
        ));
        Ok(())
    }
    pub(super) fn start_sdk(&mut self, c: &LogicalConnection) -> Result<()> {
        tracing::info!(
            target: "session",
            "[session] start_sdk full={} service={} physical={} sdk_id={} key_len={} remote={}",
            c.full,
            c.service,
            c.physical,
            c.sdk_id,
            c.trans_key.len(),
            c.remote
        );
        let request =
            ChannelControl::peer_port_request(c.sdk_id as u64, c.full, c.trans_key.clone());
        let clear = request.encode();
        tracing::info!(
            target: "session",
            "[session] start_sdk clear bytes={} hex={}",
            clear.len(),
            hex::encode(&clear)
        );
        let encrypted = c.auth.encrypt(&clear)?;
        let wrapped = sdk::wrap(c.remote, &encrypted);
        tracing::info!(
            target: "session",
            "[session] start_sdk wrapped bytes={} slot={} head={}",
            wrapped.len(),
            c.remote,
            hex::encode(&wrapped[..wrapped.len().min(32)])
        );
        self.queue_on(c.physical, &wrapped)
    }

    pub(super) fn remember_networking_peer(&mut self, device: &NetworkDevice) -> Result<bool> {
        let id = String::from_utf8(device.full_id.clone())?;
        if !id.is_empty() {
            ensure!(
                id.len() >= 8 && id.is_ascii(),
                "手机 Networking DeviceInfo.f3 不是有效的公共设备 ID"
            );
            *self.session.inner.peer_short.lock().unwrap() = id[..8].to_owned();
        }
        let region = device.region.clone().unwrap_or_default();
        *self.session.inner.peer_region.lock().unwrap() = region.clone();
        *self.session.inner.peer_services.lock().unwrap() =
            device.services_map().into_iter().collect();
        let learn_region = self.session.inner.options.build_region.is_none()
            && !region.is_empty()
            && *self.session.inner.region.lock().unwrap() != region;
        if learn_region {
            let mut own = self.own_device.clone();
            own.region = Some(region.clone());
            self.replace_own_device(own);
            *self.session.inner.region.lock().unwrap() = region.clone();
            self.phase(&format!("Networking：协议区域沿用已认证手机声明 {region}"));
        }
        Ok(learn_region)
    }

    pub(super) fn replace_own_device(&mut self, mut device: NetworkDevice) {
        self.service_version = self.service_version.wrapping_add(1);
        device.service_version = Some(self.service_version);
        self.own_device = device;
    }
}

pub(super) fn network_transport(c: &LogicalConnection, module: u8, data: &[u8]) -> Result<Vec<u8>> {
    let payload = crate::protocol::messages::data::NetworkingPayload {
        module,
        body: data.to_vec(),
    }
    .encode();
    let payload = c.auth.encrypt(&payload)?;
    Ok(crate::protocol::messages::data::DataFrame {
        kind: crate::protocol::messages::data::DataKind::Networking,
        slot: c.remote,
        encrypted: true,
        payload,
    }
    .encode())
}

//! Networking and SDK data-plane dispatch.
use super::super::{NETWORKING, SessionEvent};
use super::PhysicalActor;
use super::logical::network_transport;
use crate::{
    Result,
    protocol::{
        messages::{
            data::{DataFrame, DataKind, NetworkingPayload},
            device::{NetworkingReply, NetworkingSync},
            sdk::{ChannelBody, ChannelControl},
        },
        pb::{Fields, Message},
    },
};
use anyhow::ensure;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

impl PhysicalActor {
    pub(super) async fn receive_data(&mut self, frame: DataFrame, physical_id: u8) -> Result<()> {
        let Some(full) = self
            .connections
            .iter()
            .find_map(|(full, c)| (c.local == frame.slot).then_some(*full))
        else {
            return Ok(());
        };
        let mut c = self.connections.remove(&full).unwrap();
        ensure!(c.physical == physical_id, "业务数据到达错误的 Physical");
        let clear = if frame.kind == DataKind::Networking || frame.encrypted {
            c.auth.decrypt(&frame.payload)?
        } else {
            frame.payload
        };
        if c.service == NETWORKING {
            let payload = NetworkingPayload::decode(&clear)?;
            let _ = self.session.inner.events.send(SessionEvent::Networking {
                module: payload.module,
                data: payload.body.clone(),
            });
            if payload.module == 0 {
                let frame = Fields::decode(&payload.body)?;
                match frame.uint(1) {
                    1 => {
                        let sync = NetworkingSync::decode(&payload.body)?;
                        self.remember_networking_peer(&sync.device)?;
                        let reply = NetworkingReply {
                            status: 0,
                            version: 1,
                            device: self.own_device.clone(),
                        }
                        .encode();
                        self.queue_on(c.physical, &network_transport(&c, 0, &reply)?)?;
                        if !c.outgoing {
                            c.phase = 3;
                            self.phase(&format!(
                                "入站 Networking同步请求已认证，回复已排队 full={full}"
                            ));
                            if let Some(ready) = self.ready.take() {
                                self.phase("已连接");
                                let _ = ready.send(Ok(()));
                            }
                        }
                    }
                    2 => {
                        let reply = NetworkingReply::decode(&payload.body)?;
                        ensure!(reply.status == 0, "Networking同步被拒绝：{}", reply.status);
                        let region_changed = self.remember_networking_peer(&reply.device)?;
                        if region_changed {
                            let sync = NetworkingSync {
                                version: 1,
                                device: self.own_device.clone(),
                            }
                            .encode();
                            self.queue_on(c.physical, &network_transport(&c, 0, &sync)?)?;
                        }
                        if let Some(ready) = self.ready.take() {
                            self.phase("已连接");
                            let _ = ready.send(Ok(()));
                        }
                    }
                    _ => {}
                }
            }
        } else {
            let control = ChannelControl::decode(&clear)?;
            match control.body {
                ChannelBody::PeerPortResponse {
                    request_id,
                    sdk_id,
                    port,
                    ..
                } if c.outgoing => {
                    ensure!(request_id == c.sdk_id as u64, "PeerPort未回显本地通道ID");
                    let socket = UdpSocket::bind((self.session.local_ip(), 0)).await?;
                    let channel = self.spawn_media(
                        &c,
                        socket,
                        Some(SocketAddr::from((self.session.peer_ip(), port))),
                        sdk_id,
                        false,
                    )?;
                    c.channel = Some(channel);
                }
                ChannelBody::PeerPortRequest { id, key, .. } if !c.outgoing => {
                    self.respond_peer_port(&mut c, id, &key).await?;
                }
                _ => {}
            }
        }
        self.connections.insert(full, c);
        Ok(())
    }
}

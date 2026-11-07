//! KCP SDK media channels and CAST readiness reporting.
use super::super::{Channel, ChannelEvent, ChannelInner, MediaCommand, MediaEvent};
use super::{LogicalConnection, PhysicalActor};
use crate::{
    Result,
    common::crypto::aes_decrypt,
    protocol::messages::media::{self, MAGIC, MediaPacket},
    transport::kcp::Kcp,
};
use anyhow::{Context, ensure};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast, mpsc},
};

impl PhysicalActor {
    pub(super) fn media_event(&mut self, event: MediaEvent) -> Result<()> {
        match event {
            MediaEvent::Ready { full, channel } => {
                if self
                    .upgrade
                    .as_ref()
                    .is_some_and(|upgrade| upgrade.full == full)
                {
                    let upgrade = self.upgrade.take().unwrap();
                    if let Some(c) = self.connections.get_mut(&full) {
                        c.upgrade_query = None;
                    }
                    self.phase(&format!("P2P CAST SDK已就绪 full={full}"));
                    let _ = upgrade.reply.send(Ok(()));
                }
                if channel.service() == "pcmanager:universalClipboard" && !self.clipboard_active {
                    tokio::spawn(async move {
                        let _ = channel.close().await;
                    });
                    return Ok(());
                }
                if let Some(c) = self.connections.get_mut(&full) {
                    c.channel = Some(channel.clone());
                    if let Some(reply) = c.reply.take() {
                        if let Err(Ok(channel)) = reply.send(Ok(channel)) {
                            // The caller timed out or cancelled while the SDK
                            // was opening. Release its late result normally.
                            tokio::spawn(async move {
                                let _ = channel.close().await;
                            });
                        }
                    } else {
                        let service = c.service.clone();
                        let mut unclaimed = Some(channel);
                        if let Some(waiters) = self.waiters.get_mut(&service) {
                            while let Some(waiter) = waiters.pop_front() {
                                let channel = unclaimed.take().unwrap();
                                match waiter.send(Ok(channel)) {
                                    Ok(()) => break,
                                    Err(Ok(channel)) => unclaimed = Some(channel),
                                    Err(Err(_)) => unreachable!(),
                                }
                            }
                        }
                        if let Some(channel) = unclaimed {
                            self.accepted.entry(service).or_default().push_back(channel);
                        }
                    }
                }
            }
            MediaEvent::Failed { full, reason } => {
                if self
                    .connections
                    .get(&full)
                    .is_some_and(|c| c.upgrade_query.is_some())
                {
                    self.fail_upgrade(full, reason.clone());
                }
                if let Some(c) = self.connections.get_mut(&full) {
                    if let Some(reply) = c.reply.take() {
                        let _ = reply.send(Err(anyhow::anyhow!(reason.clone())));
                    }
                    if let Some(channel) = &c.channel {
                        channel.notify_closed(reason.clone());
                    }
                }
                self.phase(&format!("SDK通道结束：{reason}"));
            }
        }
        Ok(())
    }
    pub(super) fn spawn_media(
        &self,
        c: &LogicalConnection,
        socket: UdpSocket,
        peer: Option<SocketAddr>,
        sdk_id: u32,
        server: bool,
    ) -> Result<Channel> {
        let (commands, receiver) = mpsc::channel(64);
        let (events, initial) = broadcast::channel(256);
        let channel = Channel {
            inner: Arc::new(ChannelInner {
                id: c.sdk_id,
                service: c.service.clone(),
                commands,
                control: self.session.inner.commands.clone(),
                events,
                initial: Mutex::new(Some(initial)),
                closed: AtomicBool::new(false),
            }),
            lease: None,
        };
        let actor = MediaActor {
            socket,
            kcp: Kcp::new()?,
            key: c.trans_key.clone(),
            peer,
            peer_ip: self.session.peer_ip(),
            sdk_id,
            server,
            channel: channel.clone(),
            commands: receiver,
            report: self.media_sender.clone(),
            full: c.full,
            associated: false,
            fragments: BTreeMap::new(),
            started: Instant::now(),
        };
        tokio::spawn(async move {
            let full = actor.full;
            let report = actor.report.clone();
            let channel = actor.channel.clone();
            if let Err(error) = actor.run().await {
                let reason = error.to_string();
                channel.notify_closed(reason.clone());
                let _ = report.send(MediaEvent::Failed { full, reason }).await;
            }
        });
        Ok(channel)
    }
}

struct MediaActor {
    socket: UdpSocket,
    kcp: Kcp,
    key: Vec<u8>,
    peer: Option<SocketAddr>,
    peer_ip: Ipv4Addr,
    sdk_id: u32,
    server: bool,
    channel: Channel,
    commands: mpsc::Receiver<MediaCommand>,
    report: mpsc::Sender<MediaEvent>,
    full: u32,
    associated: bool,
    fragments: BTreeMap<u16, Vec<u8>>,
    started: Instant,
}
impl MediaActor {
    fn encrypt(&self, plain: &[u8]) -> Result<Vec<u8>> {
        media::encrypt(&self.key, plain)
    }
    async fn flush(&mut self) -> Result<()> {
        self.kcp.update(self.started.elapsed().as_millis() as u32);
        for packet in self.kcp.output() {
            if let Some(peer) = self.peer {
                self.socket.send_to(&packet, peer).await?;
            }
        }
        Ok(())
    }
    async fn run(mut self) -> Result<()> {
        if !self.server {
            self.kcp.send(&media::association(self.sdk_id)?)?;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(3));
        heartbeat.tick().await;
        let mut buffer = vec![0; 65535];
        loop {
            tokio::select! {
                _=tick.tick()=>{},
                _=heartbeat.tick()=>{if self.associated&&self.channel.service()=="com.xiaomi.mirror:cast"{self.kcp.send(&self.encrypt(&media::application_data(&[0,0,0,0,0]))?)?;}},
                packet=self.socket.recv_from(&mut buffer)=>{let(n,source)=packet?;if source.ip()!=std::net::IpAddr::V4(self.peer_ip)||self.peer.is_some_and(|p|p!=source){continue;}self.peer=Some(source);self.kcp.input(&buffer[..n])?;},
                command=self.commands.recv()=>{match command{
                    Some(MediaCommand::Send(data,reply))=>{let result=self.encrypt(&data).and_then(|packet|self.kcp.send(&packet));let _=reply.send(result);},
                    Some(MediaCommand::Stop)|None=>{self.kcp.flush();self.flush().await?;self.channel.notify_closed("业务已关闭".into());return Ok(());}
                }}
            }
            while let Some(packet) = self.kcp.receive()? {
                let clear = if packet.starts_with(&MAGIC) {
                    packet
                } else {
                    let fragment = media::fragment(&packet)?.context("非法SDK片段")?;
                    let piece = aes_decrypt(&self.key, fragment.ciphertext)?;
                    if fragment.index == 0 {
                        self.fragments.clear();
                    }
                    self.fragments.insert(fragment.index, piece);
                    if self.fragments.len() != fragment.count as usize {
                        continue;
                    }
                    let mut out = Vec::new();
                    for index in 0..fragment.count {
                        out.extend(self.fragments.remove(&index).context("SDK片段序号缺失")?);
                    }
                    out
                };
                let packet = MediaPacket::decode(&clear)?;
                if packet.kind == 0 || packet.kind == 4 {
                    ensure!(packet.id() == self.sdk_id, "SDK关联通道ID不匹配");
                    if self.server {
                        self.kcp.send(&media::server_info(self.sdk_id))?;
                    }
                    if !self.associated {
                        self.associated = true;
                        self.report
                            .send(MediaEvent::Ready {
                                full: self.full,
                                channel: self.channel.clone(),
                            })
                            .await?;
                    }
                } else if packet.kind == 1 {
                    if let Some(data) = packet.value(0) {
                        let _ = self
                            .channel
                            .inner
                            .events
                            .send(ChannelEvent::Data(data.to_vec()));
                    }
                } else if packet.kind == 2 || packet.kind == 3 {
                    let _ = self.channel.inner.events.send(ChannelEvent::Data(clear));
                }
            }
            self.flush().await?;
        }
    }
}

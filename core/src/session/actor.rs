//! Physical actor state shared across physical, logical and SDK handling.
mod command;
mod data;
mod logical;
mod physical;
mod receive;
mod sdk;

use super::{
    Channel, Command, MediaCommand, MediaEvent, Session, SessionEvent,
    device::{network_device, physical_sync},
    random_id, random_key,
};
use crate::{
    Result,
    common::time::epoch_ms,
    protocol::messages::lyra::PhysicalFrame,
    transport::{auth::Auth, physical::PhysicalTransport},
};
use anyhow::ensure;
use openssl::pkey::{PKey, Public};
use std::{
    collections::{BTreeMap, VecDeque},
    net::Ipv4Addr,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

struct LogicalConnection {
    physical: u8,
    upgrade_query: Option<Vec<u8>>,
    quick_request: Option<Vec<u8>>,
    quick_peer_port: Option<Vec<u8>>,
    service_responded: bool,
    sdk_port: Option<u16>,
    timeout_ms: u64,
    local: u8,
    remote: u8,
    /// Peer's local logical id, used as the first byte of SDK wrappers.
    peer_local: u8,
    full: u32,
    service: String,
    trust: u32,
    user_data: Vec<u8>,
    auth: Auth,
    outgoing: bool,
    phase: u8,
    sdk_id: u32,
    trans_key: Vec<u8>,
    reply: Option<oneshot::Sender<Result<Channel>>>,
    channel: Option<Channel>,
    closing: bool,
}

impl LogicalConnection {
    #[allow(clippy::too_many_arguments)]
    fn new(
        physical: u8,
        local: u8,
        remote: u8,
        full: u32,
        service: String,
        trust: u32,
        user_data: Vec<u8>,
        auth: Auth,
        outgoing: bool,
        timeout_ms: u64,
        reply: Option<oneshot::Sender<Result<Channel>>>,
        quick_request: Option<Vec<u8>>,
    ) -> Self {
        Self {
            physical,
            upgrade_query: None,
            quick_request,
            quick_peer_port: None,
            service_responded: false,
            sdk_port: None,
            timeout_ms,
            local,
            remote,
            peer_local: 0,
            full,
            service,
            trust,
            user_data,
            auth,
            outgoing,
            phase: 0,
            sdk_id: random_id() & 0x7fff_ffff,
            trans_key: if outgoing { random_key() } else { Vec::new() },
            reply,
            channel: None,
            closing: false,
        }
    }
}

struct PendingPhysical {
    full: u32,
    introduction: Vec<u8>,
    local: Ipv4Addr,
    peer: Ipv4Addr,
    previous_route: (Ipv4Addr, Ipv4Addr),
    deadline: Instant,
    introduced: bool,
    reply: oneshot::Sender<Result<()>>,
}

pub(super) struct PhysicalActor {
    session: Session,
    transport: PhysicalTransport,
    secondary: Option<PhysicalTransport>,
    secondary_ready: bool,
    upgrade: Option<PendingPhysical>,
    server: bool,
    commands: mpsc::Receiver<Command>,
    connections: BTreeMap<u32, LogicalConnection>,
    physical_ready: bool,
    ready: Option<oneshot::Sender<Result<()>>>,
    own_device: crate::protocol::messages::device::NetworkDevice,
    service_version: u64,
    clipboard_active: bool,
    network_full: u32,
    media_events: mpsc::Receiver<MediaEvent>,
    media_sender: mpsc::Sender<MediaEvent>,
    accepted: BTreeMap<String, VecDeque<Channel>>,
    waiters: BTreeMap<String, VecDeque<oneshot::Sender<Result<Channel>>>>,
    cached_peer: Option<PKey<Public>>,
    next_local: u8,
    peer_type: u32,
    peer_short: String,
    started: Instant,
    closing: Option<(Instant, oneshot::Sender<Result<()>>, bool)>,
}

impl PhysicalActor {
    pub(super) fn new(
        session: Session,
        transport: PhysicalTransport,
        commands: mpsc::Receiver<Command>,
        ready: oneshot::Sender<Result<()>>,
        server: bool,
    ) -> Result<Self> {
        let own_device = network_device(session.account(), &session.inner.options)?;
        let (network_events, media_events) = mpsc::channel(32);
        let peer_type = session.inner.endpoint.device_type;
        let clipboard_active = session.inner.options.publish_clipboard;
        Ok(Self {
            session,
            transport,
            secondary: None,
            secondary_ready: false,
            upgrade: None,
            server,
            commands,
            connections: BTreeMap::new(),
            physical_ready: false,
            ready: Some(ready),
            own_device,
            service_version: 0,
            clipboard_active,
            network_full: 0,
            media_events,
            media_sender: network_events,
            accepted: BTreeMap::new(),
            waiters: BTreeMap::new(),
            cached_peer: None,
            next_local: 1,
            peer_type,
            peer_short: String::new(),
            started: Instant::now(),
            closing: None,
        })
    }
    fn phase(&self, value: &str) {
        // The UI log pane consumes tracing directly; also keep the structured
        // event for the headless service and feature code.
        tracing::info!(target: "session", "[session] {value}");
        let _ = self
            .session
            .inner
            .events
            .send(SessionEvent::Phase(value.into()));
    }
}

impl PhysicalActor {
    pub(super) async fn run(mut self) -> Result<()> {
        self.phase("Physical 设备握手");
        if !self.server {
            self.transport.queue(
                &physical_sync(
                    self.session.account(),
                    &self.session.inner.options.device_name,
                    self.session.account().device_type,
                    self.transport.medium(),
                    self.session.inner.options.wifi_capabilities.as_ref(),
                )
                .encode(),
            )?;
        }
        self.flush().await?;
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        let mut last_heartbeat = Instant::now();
        let outcome:Result<()>=async {
            loop{tokio::select!{
                _=tick.tick()=>{
                    if let Some(last)=self.transport.last_received(){ensure!(last.elapsed() < self.session.inner.options.idle_timeout, "Physical链路超时：对端已离线");}
                    if let Some(upgrade) = &self.upgrade {
                        if Instant::now() >= upgrade.deadline || upgrade.reply.is_closed() {
                            let full=upgrade.full;
                            self.fail_upgrade(full,"P2P Physical附着超时或已取消".into());
                        }
                    }
                    if self.secondary_ready && let Some(secondary)=&self.secondary {
                        if let Some(last)=secondary.last_received(){ensure!(last.elapsed() < self.session.inner.options.idle_timeout,"P2P Physical链路超时");}
                    }
                    if self.closing.is_none() && last_heartbeat.elapsed() >= Duration::from_secs(10) {
                        if self.physical_ready && self.transport.is_active(){self.transport.queue(&PhysicalFrame::heartbeat(self.started.elapsed().as_millis() as u64, 1).encode())?;}
                        if self.secondary_ready && let Some(secondary)=&mut self.secondary {
                            secondary.queue(&PhysicalFrame::heartbeat(self.started.elapsed().as_millis() as u64, 1).encode())?;
                        }
                        last_heartbeat=Instant::now();
                    }
                    if let Some((deadline,_,sent))=&self.closing{if Instant::now()>=*deadline{if *sent{break;}if self.transport.is_active(){self.transport.queue(&PhysicalFrame::close(epoch_ms()).encode())?;}if let Some(secondary)=&mut self.secondary{secondary.queue(&PhysicalFrame::close(epoch_ms()).encode())?;}if let Some((deadline,_,sent))=&mut self.closing{*deadline=Instant::now()+Duration::from_millis(800);*sent=true;}}}
                },
                message=self.transport.recv_message()=>{match message {
                    Ok(message)=>self.receive(&message,0).await?,
                    Err(error) if self.physical_close_eof(&error)=>{
                        self.phase("Physical关闭已协商，传输结束");
                        break;
                    },
                    Err(error)=>return Err(error),
                }},
                message=recv_secondary(&mut self.secondary)=>{
                    let result=match message { Ok(message)=>self.receive(&message,1).await, Err(error)=>Err(error) };
                    if let Err(error)=result {
                        if self.physical_close_eof(&error){self.phase("Physical关闭已协商，传输结束");break;}
                        if let Some(upgrade)=&self.upgrade {let full=upgrade.full;self.fail_upgrade(full,error.to_string());}
                        else{return Err(error);}
                    }
                },
                event=self.media_events.recv()=>{if let Some(event)=event{self.media_event(event)?;}},
                command=self.commands.recv()=>{match command{Some(command)=>self.command(command).await?,None=>break,}}
            }
            ensure!(self.transport.is_active() || self.secondary.as_ref().is_some_and(PhysicalTransport::is_active),"没有可用的Physical链路");
            self.flush().await?;
            }
            Ok(())
        }.await;
        for connection in self.connections.values_mut() {
            if let Some(channel) = &connection.channel {
                channel.notify_closed("连接已关闭".into());
                let _ = channel.inner.commands.send(MediaCommand::Stop).await;
            }
            if let Some(reply) = connection.reply.take() {
                let _ = reply.send(Err(anyhow::anyhow!(
                    outcome
                        .as_ref()
                        .err()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "连接已关闭".into())
                )));
            }
        }
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Err(anyhow::anyhow!(
                outcome
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "连接已关闭".into())
            )));
        }
        if let Some(upgrade) = self.upgrade.take() {
            let _ = upgrade
                .reply
                .send(Err(anyhow::anyhow!("会话已关闭，链路升级结束")));
        }
        let closed = self.transport.close().await;
        let secondary_closed = if let Some(secondary) = self.secondary.take() {
            secondary.close().await
        } else {
            Ok(())
        };
        let outcome = outcome.and(closed).and(secondary_closed);
        if let Some((_, reply, _)) = self.closing.take() {
            let _ = reply.send(
                outcome
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| anyhow::anyhow!(error.to_string())),
            );
        }
        let _ = self.session.inner.events.send(SessionEvent::Disconnected(
            outcome
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "已断开".into()),
        ));
        outcome
    }
}

async fn recv_secondary(secondary: &mut Option<PhysicalTransport>) -> Result<Vec<u8>> {
    match secondary {
        Some(transport) => transport.recv_message().await,
        None => std::future::pending().await,
    }
}

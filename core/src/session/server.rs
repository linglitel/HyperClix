//! Local presence and phone-initiated, same-account connections over LAN or BLE.
use crate::{
    Result,
    discovery::{Announcement, AnnouncementOptions, DeviceEndpoint},
    identity::Account,
    proximity::{BleListener, BleListenerOptions, IncomingBleChannel},
    session::{ConnectOptions, Session, SessionEvent},
    transport::physical::PhysicalIo,
};
use anyhow::{Context, ensure};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};

#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Unspecified disables LAN listening; BLE can operate on its own.
    pub local_ip: Ipv4Addr,
    /// Zero chooses a free LAN port and announces the actual port.
    pub port: u16,
    pub ble: Option<BleListenerOptions>,
    pub connection: ConnectOptions,
}
impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            local_ip: Ipv4Addr::UNSPECIFIED,
            port: 0,
            ble: None,
            connection: ConnectOptions::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ServerPeer {
    Lan(SocketAddr),
    Ble(String),
}
#[derive(Clone, Debug)]
pub enum ServerEvent {
    Authenticating(ServerPeer),
    Connected(ServerPeer),
    Disconnected(ServerPeer),
    Error { peer: ServerPeer, reason: String },
}
pub struct Server {
    address: Option<SocketAddr>,
    incoming: mpsc::Receiver<Session>,
    events: broadcast::Sender<ServerEvent>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}
impl Server {
    pub async fn start(account: &Account, options: ServerOptions) -> Result<Self> {
        ensure!(
            !options.local_ip.is_unspecified() || options.ble.is_some(),
            "请选择局域网地址或启用 BLE 接收"
        );
        let socket = if options.local_ip.is_unspecified() {
            None
        } else {
            Some(Arc::new(
                UdpSocket::bind((options.local_ip, options.port))
                    .await
                    .context("绑定入站 Physical 服务")?,
            ))
        };
        let address = socket
            .as_ref()
            .map(|socket| socket.local_addr())
            .transpose()?;
        let announcement = if let Some(address) = address {
            Some(
                Announcement::start(
                    account,
                    AnnouncementOptions {
                        local_ip: options.local_ip,
                        name: options.connection.device_name.clone(),
                        // Lyra authenticates the certificate against this discovery identity.
                        device_type: account
                            .device_type
                            .try_into()
                            .context("设备类型超出公告字段范围")?,
                        kcp_port: Some(address.port()),
                        ..Default::default()
                    },
                )
                .await?,
            )
        } else {
            None
        };
        let mut ble = if let Some(ble) = &options.ble {
            Some(BleListener::start(account, ble.clone()).await?)
        } else {
            None
        };
        let (stop, stopped) = watch::channel(false);
        let (incoming_tx, incoming) = mpsc::channel(16);
        let (events, _) = broadcast::channel(64);
        let account = account.clone();
        let reporter = events.clone();
        let task = tokio::spawn(async move {
            tracing::info!("[server] started: lan={address:?}, ble={}", ble.is_some());
            let result = serve(
                account,
                options,
                socket,
                &mut ble,
                incoming_tx,
                reporter,
                stopped,
            )
            .await;
            let ble_closed = if let Some(ble) = ble {
                ble.close().await
            } else {
                Ok(())
            };
            let withdrawn = if let Some(announcement) = announcement {
                announcement.close().await
            } else {
                Ok(())
            };
            tracing::info!("[server] stopped; local presence withdrawn");
            result.and(ble_closed).and(withdrawn)
        });
        Ok(Self {
            address,
            incoming,
            events,
            stop,
            task: Some(task),
        })
    }
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.address
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }
    /// Returns only sessions that completed account authentication and Networking.
    pub async fn accept(&mut self) -> Result<Session> {
        self.incoming.recv().await.context("后台入站服务已停止")
    }
    pub async fn close(mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

async fn receive_udp(
    socket: &Option<Arc<UdpSocket>>,
    buffer: &mut [u8],
) -> std::io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buffer).await,
        None => std::future::pending().await,
    }
}
async fn receive_ble(ble: &mut Option<BleListener>) -> Result<IncomingBleChannel> {
    match ble {
        Some(ble) => ble.accept().await,
        None => std::future::pending().await,
    }
}

async fn serve(
    account: Account,
    options: ServerOptions,
    socket: Option<Arc<UdpSocket>>,
    ble: &mut Option<BleListener>,
    incoming: mpsc::Sender<Session>,
    events: broadcast::Sender<ServerEvent>,
    mut stopped: watch::Receiver<bool>,
) -> Result<()> {
    let mut routes: BTreeMap<SocketAddr, mpsc::Sender<Vec<u8>>> = BTreeMap::new();
    let mut tasks = JoinSet::new();
    let (shutdown, cancelled) = watch::channel(false);
    let mut buffer = vec![0; 65535];
    let result = async {
        loop {
            let (session, ready, peer) = tokio::select! {
                _ = stopped.changed() => break,
                finished = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Ok(ServerPeer::Lan(peer))) = finished {
                        if routes.get(&peer).is_some_and(mpsc::Sender::is_closed) { routes.remove(&peer); }
                    }
                    continue;
                },
                channel = receive_ble(ble) => {
                    let channel = channel?;
                    let peer = ServerPeer::Ble(channel.peer_path().to_owned());
                    let (session, ready) = Session::accept_ble(&account, channel, options.connection.clone())?;
                    (session, ready, peer)
                },
                packet = receive_udp(&socket, &mut buffer) => {
                    let (length, peer) = packet?;
                    let bytes = &buffer[..length];
                    if let Some(route) = routes.get(&peer) {
                        // A full queue drops a UDP datagram; KCP retransmits it.
                        if !route.is_closed() { let _ = route.try_send(bytes.to_vec()); continue; }
                        routes.remove(&peer);
                    }
                    if bytes.len() < 24 || bytes[..4] != [0x78,0x56,0x34,0x12] || bytes[4] != 81 { continue; }
                    let (tx, rx) = mpsc::channel(128);
                    tx.try_send(bytes.to_vec())?;
                    let std::net::IpAddr::V4(peer_ip) = peer.ip() else { continue };
                    let endpoint = DeviceEndpoint::new(options.local_ip, peer_ip, peer.port());
                    let io = PhysicalIo { socket: socket.as_ref().unwrap().clone(), peer, incoming: Some(rx) };
                    let (session, ready) = Session::start(&account, &endpoint, options.connection.clone(), io, true)?;
                    routes.insert(peer, tx);
                    (session, ready, ServerPeer::Lan(peer))
                }
            };
            let _ = events.send(ServerEvent::Authenticating(peer.clone()));
            tasks.spawn(hold_session(session, ready, peer, options.connection.timeout,
                incoming.clone(), events.clone(), cancelled.clone()));
        }
        Ok(())
    }.await;
    shutdown.send_replace(true);
    while tasks.join_next().await.is_some() {}
    result
}

async fn hold_session(
    session: Session,
    ready: oneshot::Receiver<Result<()>>,
    peer: ServerPeer,
    timeout: Duration,
    incoming: mpsc::Sender<Session>,
    events: broadcast::Sender<ServerEvent>,
    mut cancelled: watch::Receiver<bool>,
) -> ServerPeer {
    let mut changes = session.events();
    let result: Result<()> = async {
        tokio::select! {
            ready = tokio::time::timeout(timeout, ready) => { ready.context("入站认证超时")?.context("入站认证任务结束")??; },
            _ = cancelled.changed() => return Ok(()),
        }
        tracing::info!("[server] authenticated phone connection: {peer:?}");
        let _ = events.send(ServerEvent::Connected(peer.clone()));
        tokio::select! {
            sent = incoming.send(session.clone()) => { sent.context("入站会话接收端已关闭")?; },
            _ = cancelled.changed() => return Ok(()),
        }
        loop {
            tokio::select! {
                _ = cancelled.changed() => break,
                event = changes.recv() => match event {
                    Ok(SessionEvent::Disconnected(_)) | Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    _ => {},
                }
            }
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        tracing::info!("[server] connection {peer:?}: {error:#}");
        let _ = events.send(ServerEvent::Error {
            peer: peer.clone(),
            reason: format!("{error:#}"),
        });
    }
    let _ = session.close().await;
    let _ = events.send(ServerEvent::Disconnected(peer.clone()));
    peer
}

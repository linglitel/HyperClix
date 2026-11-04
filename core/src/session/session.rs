//! Session lifecycle, logical channel requests and CAST ownership.
use super::actor::PhysicalActor;
use super::{
    CastLease, CastPool, CastShared, Channel, Command, ConnectOptions, INCOMING, SdkPlatform,
    Session, SessionEvent, SessionInner,
};
use crate::transport::physical::{PhysicalIo, PhysicalTransport};
use crate::{
    Result,
    discovery::DeviceEndpoint,
    identity::Account,
    proximity::{BleChannel, BleDevice, IncomingBleChannel, P2pLink},
};
use anyhow::{Context, ensure};
use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, HashMap},
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast, mpsc, oneshot},
};

impl Session {
    /// Succeeds only after authenticated Networking Reply.status=0.
    pub async fn connect(
        account: &Account,
        endpoint: &DeviceEndpoint,
        options: ConnectOptions,
    ) -> Result<Self> {
        let (_cancel, cancelled) = tokio::sync::watch::channel(false);
        Self::connect_cancellable(account, endpoint, options, cancelled).await
    }
    /// Cancellation runs the normal Logical/Physical close before returning.
    pub async fn connect_cancellable(
        account: &Account,
        endpoint: &DeviceEndpoint,
        options: ConnectOptions,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> Result<Self> {
        ensure!(!*cancelled.borrow(), "连接已取消");
        let socket = UdpSocket::bind((endpoint.local_ip, 0))
            .await
            .context("绑定电脑局域网接口")?;
        socket.connect((endpoint.ip, endpoint.port)).await?;
        let io = PhysicalIo {
            socket: Arc::new(socket),
            peer: SocketAddr::from((endpoint.ip, endpoint.port)),
            incoming: None,
        };
        let (session, waiting) = Self::start(account, endpoint, options.clone(), io, false)?;
        tokio::select! {
            result=tokio::time::timeout(options.timeout, waiting)=>match result {
                Ok(Ok(result))=>{
                    result?;
                    if *cancelled.borrow() { let _=session.close().await; anyhow::bail!("连接已取消"); }
                    Ok(session)
                }
                Ok(Err(_))=>anyhow::bail!("连接在 Networking 上线前结束"),
                Err(_)=>{ let _=session.close().await; anyhow::bail!("连接超时：未收到 Networking 成功回包"); }
            },
            _=cancelled.changed()=>{ let _=session.close().await; anyhow::bail!("连接已取消"); }
        }
    }

    pub(crate) fn start(
        account: &Account,
        endpoint: &DeviceEndpoint,
        options: ConnectOptions,
        io: PhysicalIo,
        server: bool,
    ) -> Result<(Self, oneshot::Receiver<Result<()>>)> {
        Self::start_transport(
            account,
            endpoint,
            options,
            PhysicalTransport::udp(io)?,
            server,
        )
    }

    fn start_transport(
        account: &Account,
        endpoint: &DeviceEndpoint,
        options: ConnectOptions,
        transport: PhysicalTransport,
        server: bool,
    ) -> Result<(Self, oneshot::Receiver<Result<()>>)> {
        let (commands, receiver) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        let session = Self {
            inner: Arc::new(SessionInner {
                account: account.clone(),
                endpoint: endpoint.clone(),
                route: Mutex::new((endpoint.local_ip, endpoint.ip)),
                p2p_link: Mutex::new(None),
                close: tokio::sync::Mutex::new(()),
                options: options.clone(),
                commands,
                events,
                peer_short: Mutex::new(String::new()),
                peer_services: Mutex::new(BTreeMap::new()),
                peer_region: Mutex::new(String::new()),
                region: Mutex::new(options.build_region.clone().unwrap_or_default()),
                cast: Arc::new(CastPool::default()),
                extensions: Mutex::new(HashMap::new()),
            }),
        };
        let (ready, waiting) = oneshot::channel();
        let actor = PhysicalActor::new(session.clone(), transport, receiver, ready, server)?;
        let task_session = session.clone();
        tokio::spawn(async move {
            if let Err(error) = actor.run().await {
                let _ = task_session
                    .inner
                    .events
                    .send(SessionEvent::Disconnected(error.to_string()));
            }
        });
        Ok((session, waiting))
    }

    pub(crate) fn accept_ble(
        account: &Account,
        channel: IncomingBleChannel,
        options: ConnectOptions,
    ) -> Result<(Self, oneshot::Receiver<Result<()>>)> {
        let endpoint = DeviceEndpoint {
            local_ip: Ipv4Addr::UNSPECIFIED,
            ip: Ipv4Addr::UNSPECIFIED,
            port: 0,
            name: String::new(),
            device_type: 0,
            instance: String::new(),
        };
        Self::start_transport(
            account,
            &endpoint,
            options,
            PhysicalTransport::incoming_ble(channel),
            true,
        )
    }

    /// Authenticates Networking over the phone's actual Lyra BLE GATT transport.
    /// Wi-Fi routes remain unset until an authenticated link upgrade supplies them.
    pub async fn connect_ble(
        account: &Account,
        device: &BleDevice,
        options: ConnectOptions,
    ) -> Result<Self> {
        let channel = BleChannel::open(device, options.timeout).await?;
        let endpoint = DeviceEndpoint {
            local_ip: Ipv4Addr::UNSPECIFIED,
            ip: Ipv4Addr::UNSPECIFIED,
            port: 0,
            name: device.name.clone(),
            device_type: device.advertisement.device_type as u32,
            instance: String::new(),
        };
        let (session, waiting) = Self::start_transport(
            account,
            &endpoint,
            options.clone(),
            PhysicalTransport::ble(channel),
            false,
        )?;
        match tokio::time::timeout(options.timeout, waiting).await {
            Ok(Ok(Ok(()))) => Ok(session),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => anyhow::bail!("BLE连接在 Networking 上线前结束"),
            Err(_) => {
                let _ = session.close().await;
                anyhow::bail!("BLE同账号认证超时：未收到 Networking 成功回包")
            }
        }
    }

    pub fn account(&self) -> &Account {
        &self.inner.account
    }
    pub fn local_ip(&self) -> Ipv4Addr {
        self.inner.route.lock().unwrap().0
    }
    pub fn peer_ip(&self) -> Ipv4Addr {
        self.inner.route.lock().unwrap().1
    }
    pub(crate) fn set_link_route(&self, local: Ipv4Addr, remote: Ipv4Addr) {
        *self.inner.route.lock().unwrap() = (local, remote);
    }
    pub(crate) fn retain_p2p_link(&self, link: P2pLink) {
        *self.inner.p2p_link.lock().unwrap() = Some(link);
    }
    pub fn peer_short_id(&self) -> String {
        self.inner.peer_short.lock().unwrap().clone()
    }
    pub fn peer_service_data(&self, unique_name: &str) -> Option<Vec<u8>> {
        self.inner
            .peer_services
            .lock()
            .unwrap()
            .get(unique_name)
            .cloned()
    }
    pub fn peer_build_region(&self) -> String {
        self.inner.peer_region.lock().unwrap().clone()
    }
    pub fn build_region(&self) -> String {
        self.inner.region.lock().unwrap().clone()
    }
    pub fn events(&self) -> broadcast::Receiver<SessionEvent> {
        self.inner.events.subscribe()
    }
    /// Lazily created per-session state owned by a feature module.
    pub(crate) fn extension<T>(&self) -> Arc<T>
    where
        T: Any + Send + Sync + Default + 'static,
    {
        let mut extensions = self.inner.extensions.lock().unwrap();
        extensions
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Arc::new(T::default()))
            .clone()
            .downcast::<T>()
            .unwrap_or_else(|_| unreachable!("session extension type id mismatch"))
    }
    pub(crate) fn report_phase(&self, phase: impl Into<String>) {
        let _ = self.inner.events.send(SessionEvent::Phase(phase.into()));
    }
    pub(crate) async fn open_channel(
        &self,
        service: &str,
        trust: u32,
        platform: SdkPlatform,
    ) -> Result<Channel> {
        self.open_channel_with_data(service, trust, platform, "")
            .await
    }
    /// One CAST SDK connection per session, with an independent lease for each
    /// screen owner. Near-field upgrades retain it until the session closes.
    /// Cloning a Channel preserves its existing lease.
    pub(crate) async fn open_cast(&self) -> Result<Channel> {
        let pool = self.inner.cast.clone();
        let mut current = pool.current.lock().await;
        let shared = match current.as_ref().filter(|shared| {
            !shared.channel.inner.closed.load(Ordering::Acquire)
                && !shared.channel.inner.commands.is_closed()
        }) {
            Some(shared) => shared.clone(),
            None => {
                let channel = self
                    .open_channel(super::CAST_SERVICE, super::TRUST_PAIR, SdkPlatform::Linux)
                    .await?;
                let shared = Arc::new(CastShared {
                    channel,
                    users: AtomicUsize::new(0),
                });
                *current = Some(shared.clone());
                shared
            }
        };
        shared.users.fetch_add(1, Ordering::AcqRel);
        let initial = shared.channel.subscribe();
        let inner = shared.channel.inner.clone();
        drop(current);
        Ok(Channel {
            inner,
            lease: Some(Arc::new(CastLease {
                pool,
                shared,
                released: AtomicBool::new(false),
                initial: Mutex::new(Some(initial)),
                runtime: tokio::runtime::Handle::current(),
            })),
        })
    }
    pub(crate) async fn open_channel_with_data(
        &self,
        service: &str,
        trust: u32,
        platform: SdkPlatform,
        user_data: &str,
    ) -> Result<Channel> {
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Open {
                service: service.into(),
                trust,
                platform,
                user_data: user_data.into(),
                reply,
            })
            .await
            .context("连接已经关闭")?;
        tokio::time::timeout(self.inner.options.timeout, wait)
            .await
            .context("业务通道建立超时")?
            .context("业务通道建立中断")?
    }
    /// Wait for one reverse SDK channel. Ready channels are retained until claimed.
    pub(crate) async fn accept_channel(&self, service: &str) -> Result<Channel> {
        ensure!(INCOMING.contains(&service), "未注册的入站服务 {service}");
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Accept {
                service: service.into(),
                reply,
            })
            .await
            .context("连接已经关闭")?;
        wait.await.context("入站通道等待中断")?
    }
    pub(crate) async fn send_networking(&self, module: u8, data: &[u8]) -> Result<()> {
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Networking {
                module,
                data: data.to_vec(),
                reply,
            })
            .await
            .context("连接已经关闭")?;
        wait.await?
    }
    pub(crate) async fn begin_link_upgrade(&self, query: &[u8], timeout: Duration) -> Result<u32> {
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::BeginLinkUpgrade {
                query: query.to_vec(),
                timeout,
                reply,
            })
            .await?;
        wait.await?
    }
    pub(crate) async fn cancel_link_upgrade(&self, full: u32) -> Result<()> {
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::CancelLinkUpgrade { full, reply })
            .await?;
        wait.await?
    }
    pub(crate) async fn upgrade_physical(
        &self,
        local: Ipv4Addr,
        peer: Ipv4Addr,
        port: u16,
        full: u32,
        introduction: &[u8],
        timeout: Duration,
    ) -> Result<()> {
        let socket = UdpSocket::bind((local, 0)).await?;
        socket.connect((peer, port)).await?;
        let io = PhysicalIo {
            socket: Arc::new(socket),
            peer: SocketAddr::from((peer, port)),
            incoming: None,
        };
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::UpgradePhysical {
                io,
                full,
                introduction: introduction.to_vec(),
                local,
                peer,
                timeout,
                reply,
            })
            .await?;
        wait.await?
    }
    /// Add/update an active business service, or withdraw it with None.
    pub(crate) async fn set_service(
        &self,
        app_name: &str,
        service_name: &str,
        data: Option<&[u8]>,
    ) -> Result<()> {
        let (reply, wait) = oneshot::channel();
        self.inner
            .commands
            .send(Command::SetService {
                app: app_name.to_owned(),
                name: service_name.to_owned(),
                data: data.map(<[u8]>::to_vec),
                reply,
            })
            .await
            .context("Networking已关闭，无法更新业务服务")?;
        wait.await?
    }
    pub async fn close(&self) -> Result<()> {
        let _closing = self.inner.close.lock().await;
        let (reply, wait) = oneshot::channel();
        let result = if self
            .inner
            .commands
            .send(Command::Close(reply))
            .await
            .is_err()
        {
            Ok(())
        } else {
            wait.await
                .context("会话关闭确认中断")
                .and_then(|result| result)
        };
        let link = self.inner.p2p_link.lock().unwrap().take();
        let released = if let Some(link) = link {
            link.close().await
        } else {
            Ok(())
        };
        result.and(released)
    }
}

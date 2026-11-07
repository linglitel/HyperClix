//! One authenticated Physical connection, routed logical services and SDK handles.
use crate::{
    Result,
    discovery::DeviceEndpoint,
    identity::Account,
    protocol::capabilities::CastCapabilities,
    proximity::{P2pLink, WifiCapabilities},
    transport::physical::PhysicalIo,
};
use rand::RngCore;
use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, HashMap},
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, oneshot};

mod actor;
mod channel;
mod device;
pub mod server;
mod session;
mod upgrade;

pub(crate) use crate::protocol::service::{
    CAST as CAST_SERVICE, NETWORKING, TRUST_MISHARE, TRUST_PAIR,
};
pub(crate) const MEDIUM_BLE: u64 = 2;
pub(crate) const MEDIUM_P2P: u64 = 32;
pub(crate) const MEDIUM_LAN: u64 = 128;
pub(crate) const MEDIUM_LAN1: u64 = 0x100;
const INCOMING: &[&str] = &[
    crate::protocol::service::CAST,
    crate::protocol::service::CAMERA_METADATA,
    crate::protocol::service::CAMERA_P2P,
    crate::protocol::service::MISHARE_BASIC,
    crate::protocol::service::MISHARE_TRANSFER,
    crate::protocol::service::PC_MISHARE_BASIC,
    crate::protocol::service::PC_MISHARE_TRANSFER,
    crate::protocol::service::CLIPBOARD_LOCAL,
];

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub(crate) enum SdkPlatform {
    Windows = 2,
    Linux = 3,
}

#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub device_name: String,
    pub market_name: Option<String>,
    pub capabilities: CastCapabilities,
    pub wifi_capabilities: Option<WifiCapabilities>,
    pub publish_mishare: bool,
    /// Publish the notification service used by the phone's device-info filter.
    pub publish_notifications: bool,
    pub publish_clipboard: bool,
    /// Protocol region. None follows the authenticated peer's advertised region.
    pub build_region: Option<String>,
    pub timeout: Duration,
    /// Release a dead Physical link after this long without receiving traffic.
    pub idle_timeout: Duration,
}
impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            device_name: crate::DEFAULT_DEVICE_NAME.into(),
            market_name: None,
            capabilities: CastCapabilities {
                support_text_paste: Some(false),
                ..Default::default()
            },
            wifi_capabilities: None,
            publish_mishare: true,
            publish_notifications: true,
            publish_clipboard: false,
            build_region: None,
            timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Phase(String),
    Networking {
        module: u8,
        data: Vec<u8>,
    },
    LinkUpgrade {
        local: u8,
        remote: u8,
        full: u32,
        frame: Vec<u8>,
    },
    LinkUpgradeFailed {
        full: u32,
        reason: String,
    },
    Disconnected(String),
}
#[derive(Clone, Debug)]
pub enum ChannelEvent {
    Data(Vec<u8>),
    Closed(String),
}

#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}
struct SessionInner {
    account: Account,
    endpoint: DeviceEndpoint,
    route: Mutex<(Ipv4Addr, Ipv4Addr)>,
    p2p_link: Mutex<Option<P2pLink>>,
    close: tokio::sync::Mutex<()>,
    options: ConnectOptions,
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<SessionEvent>,
    peer_short: Mutex<String>,
    peer_services: Mutex<BTreeMap<String, Vec<u8>>>,
    peer_region: Mutex<String>,
    region: Mutex<String>,
    cast: Arc<CastPool>,
    extensions: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

#[derive(Clone)]
pub struct Channel {
    inner: Arc<ChannelInner>,
    lease: Option<Arc<CastLease>>,
}
#[derive(Default)]
struct CastPool {
    current: tokio::sync::Mutex<Option<Arc<CastShared>>>,
}
struct CastShared {
    channel: Channel,
    users: AtomicUsize,
}
struct CastLease {
    shared: Arc<CastShared>,
    released: AtomicBool,
    initial: Mutex<Option<broadcast::Receiver<ChannelEvent>>>,
    runtime: tokio::runtime::Handle,
}

struct ChannelInner {
    id: u32,
    service: String,
    commands: mpsc::Sender<MediaCommand>,
    control: mpsc::Sender<Command>,
    events: broadcast::Sender<ChannelEvent>,
    initial: Mutex<Option<broadcast::Receiver<ChannelEvent>>>,
    closed: AtomicBool,
}

enum Command {
    BeginLinkUpgrade {
        query: Vec<u8>,
        timeout: Duration,
        reply: oneshot::Sender<Result<u32>>,
    },
    CancelLinkUpgrade {
        full: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    UpgradePhysical {
        io: PhysicalIo,
        full: u32,
        introduction: Vec<u8>,
        local: Ipv4Addr,
        peer: Ipv4Addr,
        timeout: Duration,
        reply: oneshot::Sender<Result<()>>,
    },
    Open {
        service: String,
        trust: u32,
        platform: SdkPlatform,
        user_data: String,
        reply: oneshot::Sender<Result<Channel>>,
    },
    Accept {
        service: String,
        reply: oneshot::Sender<Result<Channel>>,
    },
    Networking {
        module: u8,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    SetService {
        app: String,
        name: String,
        data: Option<Vec<u8>>,
        reply: oneshot::Sender<Result<()>>,
    },
    CloseChannel(u32, bool, oneshot::Sender<Result<()>>),
    RetrySdk {
        full: u32,
    },
    Close(oneshot::Sender<Result<()>>),
}
enum MediaCommand {
    Send(Vec<u8>, oneshot::Sender<Result<()>>),
    Stop,
}
enum MediaEvent {
    Ready { full: u32, channel: Channel },
    Failed { full: u32, reason: String },
}

fn random_id() -> u32 {
    let mut id = rand::random::<u32>();
    if id == 0 {
        id = 1;
    }
    id
}
fn random_key() -> Vec<u8> {
    let mut key = vec![0; 32];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

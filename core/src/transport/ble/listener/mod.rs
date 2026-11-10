//! Connectable Lyra BLE presence and incoming GATT byte transport.

use super::frame as ble_frame;
use crate::transport::dbus::{Properties, proxy};
use crate::{Result, identity::Account};
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, watch},
    task::JoinHandle,
};
mod gatt;

use gatt::{Advertisement, GattService, NotifyCharacteristic, WriteCharacteristic, remove_objects};

use zbus::{
    Connection, MatchRule, MessageStream, fdo,
    message::Type,
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, Value},
};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const GATT_MANAGER: &str = "org.bluez.GattManager1";
const ADV_MANAGER: &str = "org.bluez.LEAdvertisingManager1";
const CHARACTERISTIC: &str = "org.bluez.GattCharacteristic1";

#[derive(Clone, Debug)]
pub struct BleListenerOptions {
    pub adapter: Option<String>,
    pub name: String,
    /// Advertised Lyra device type; None follows the certificate type.
    pub device_type: Option<u32>,
}

impl Default for BleListenerOptions {
    fn default() -> Self {
        Self {
            adapter: None,
            name: crate::DEFAULT_DEVICE_NAME.into(),
            device_type: None,
        }
    }
}

pub struct BleListener {
    incoming: mpsc::Receiver<IncomingBleChannel>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}

struct Peer {
    id: u64,
    path: String,
    incoming: mpsc::Sender<Result<Vec<u8>>>,
    assembler: ble_frame::Assembler,
    packet_size: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

struct Shared {
    bus: Connection,
    notify_path: String,
    accepting: Mutex<Option<mpsc::Sender<IncomingBleChannel>>>,
    peer: Mutex<Option<Peer>>,
    notifying: AtomicBool,
    stopped: AtomicBool,
    value: Mutex<Vec<u8>>,
}

impl Shared {
    fn retire(&self, id: Option<u64>, reason: &str) -> Option<String> {
        let peer = {
            let mut current = self.peer.lock().unwrap();
            if current
                .as_ref()
                .is_some_and(|peer| id.is_none_or(|id| peer.id == id))
            {
                current.take()
            } else {
                None
            }
        }?;
        peer.closed.store(true, Ordering::Release);
        let _ = peer.incoming.try_send(Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            reason.to_owned(),
        )
        .into()));
        tracing::info!(
            "[nearby] incoming BLE peer closed: id={} reason={reason}",
            peer.id
        );
        Some(peer.path)
    }

    async fn disconnect(&self, path: &str) -> Result<()> {
        let device = proxy(&self.bus, BLUEZ, path, DEVICE).await?;
        let _: () = device.call("Disconnect", &()).await?;
        Ok(())
    }

    async fn receive(self: &Arc<Self>, value: &[u8], options: &Properties) -> Result<()> {
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "BLE listener has stopped"
        );
        ensure!(
            options
                .get("offset")
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(0)
                == 0,
            "Lyra BLE writes require offset zero"
        );
        let path = options
            .get("device")
            .context("BlueZ omitted the incoming BLE device")?;
        let path = OwnedObjectPath::try_from(path.try_clone()?)?.to_string();
        let mtu = options
            .get("mtu")
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(23);
        let (incoming, message) = {
            let mut current = self.peer.lock().unwrap();
            if current.is_none() {
                let id = rand::random();
                let (tx, rx) = mpsc::channel(128);
                let packet_size = Arc::new(AtomicUsize::new(usize::from(mtu.clamp(23, 483)) - 3));
                let closed = Arc::new(AtomicBool::new(false));
                let channel = IncomingBleChannel {
                    path: path.clone(),
                    incoming: rx,
                    sender: IncomingBleSender {
                        shared: self.clone(),
                        id,
                        packet_size: packet_size.clone(),
                        closed: closed.clone(),
                        index: Arc::new(AsyncMutex::new(u16::MAX)),
                    },
                };
                self.accepting
                    .lock()
                    .unwrap()
                    .as_ref()
                    .context("BLE listener has stopped")?
                    .try_send(channel)
                    .context("incoming BLE session receiver is unavailable")?;
                *current = Some(Peer {
                    id,
                    path: path.clone(),
                    incoming: tx,
                    assembler: ble_frame::Assembler::default(),
                    packet_size,
                    closed,
                });
                tracing::info!("[nearby] incoming BLE peer accepted: id={id} mtu={mtu}");
            }
            let peer = current.as_mut().unwrap();
            ensure!(
                peer.path == path,
                "another phone already owns the incoming BLE service"
            );
            peer.packet_size
                .store(usize::from(mtu.clamp(23, 483)) - 3, Ordering::Relaxed);
            (peer.incoming.clone(), peer.assembler.feed(value)?)
        };
        if let Some(message) = message {
            tracing::info!(
                "[nearby] incoming BLE message received: bytes={}",
                message.len()
            );
            incoming
                .send(Ok(message))
                .await
                .context("incoming BLE session is closed")?;
        }
        Ok(())
    }
}

pub struct IncomingBleChannel {
    path: String,
    incoming: mpsc::Receiver<Result<Vec<u8>>>,
    sender: IncomingBleSender,
}

impl IncomingBleChannel {
    pub fn peer_path(&self) -> &str {
        &self.path
    }

    pub fn sender(&self) -> IncomingBleSender {
        self.sender.clone()
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        self.incoming.recv().await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incoming BLE transport closed",
            )
        })?
    }

    pub async fn close(self) -> Result<()> {
        self.sender.close().await
    }
}

impl Drop for IncomingBleChannel {
    fn drop(&mut self) {
        if !self.sender.closed.load(Ordering::Acquire) {
            let sender = self.sender.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = sender.close().await;
                });
            }
        }
    }
}

#[derive(Clone)]
pub struct IncomingBleSender {
    shared: Arc<Shared>,
    id: u64,
    packet_size: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    index: Arc<AsyncMutex<u16>>,
}

impl IncomingBleSender {
    pub async fn send(&self, payload: &[u8]) -> Result<()> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "incoming BLE transport is closed"
        );
        ensure!(
            self.shared.notifying.load(Ordering::Acquire),
            "phone has not enabled BLE notifications"
        );
        let mut index = self.index.lock().await;
        *index = index.wrapping_add(1);
        let packets = ble_frame::pack(payload, *index, self.packet_size.load(Ordering::Relaxed))?;
        tracing::info!(
            "[nearby] incoming BLE message send: bytes={} index={} fragments={}",
            payload.len(),
            *index,
            packets.len()
        );
        let emitter = SignalEmitter::new(&self.shared.bus, self.shared.notify_path.as_str())?;
        for packet in packets {
            ensure!(
                !self.closed.load(Ordering::Acquire),
                "incoming BLE transport is closed"
            );
            *self.shared.value.lock().unwrap() = packet.clone();
            fdo::Properties::properties_changed(
                &emitter,
                CHARACTERISTIC.try_into()?,
                HashMap::from([("Value", Value::from(packet))]),
                Cow::Borrowed(&[]),
            )
            .await?;
        }
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(path) = self.shared.retire(Some(self.id), "local session closed") {
            self.shared.disconnect(&path).await?;
        }
        Ok(())
    }
}

impl BleListener {
    pub async fn start(account: &Account, options: BleListenerOptions) -> Result<Self> {
        let bus = Connection::system()
            .await
            .context("connect BlueZ system bus")?;
        let manager = proxy(&bus, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").await?;
        let objects: HashMap<OwnedObjectPath, HashMap<String, Properties>> =
            manager.call("GetManagedObjects", &()).await?;
        let adapter_path = objects
            .iter()
            .find_map(|(path, interfaces)| {
                let props = interfaces.get(ADAPTER)?;
                let selected = options
                    .adapter
                    .as_ref()
                    .is_none_or(|name| path.as_str().rsplit('/').next() == Some(name.as_str()));
                let powered = props
                    .get("Powered")
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(false);
                (selected
                    && powered
                    && interfaces.contains_key(GATT_MANAGER)
                    && interfaces.contains_key(ADV_MANAGER))
                .then(|| path.to_string())
            })
            .context("BlueZ没有已开启且支持GATT和BLE广播的适配器")?;
        drop(manager);
        let root = format!("/org/hyperclix/ble_{}", uuid::Uuid::new_v4().simple());
        let app = format!("{root}/app");
        let service = format!("{app}/service0");
        let write = format!("{service}/write");
        let notify = format!("{service}/notify");
        let advertisement = format!("{root}/advertisement");
        let (incoming_tx, incoming) = mpsc::channel(1);
        let (stop, mut stopped) = watch::channel(false);
        let shared = Arc::new(Shared {
            bus: bus.clone(),
            notify_path: notify.clone(),
            accepting: Mutex::new(Some(incoming_tx)),
            peer: Mutex::new(None),
            notifying: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            value: Mutex::new(Vec::new()),
        });
        let object_server = bus.object_server();
        object_server.at(app.as_str(), fdo::ObjectManager).await?;
        object_server.at(service.as_str(), GattService).await?;
        let service_path = OwnedObjectPath::try_from(service.as_str())?;
        object_server
            .at(
                write.as_str(),
                WriteCharacteristic {
                    service: service_path.clone(),
                    shared: shared.clone(),
                },
            )
            .await?;
        object_server
            .at(
                notify.as_str(),
                NotifyCharacteristic {
                    service: service_path,
                    shared: shared.clone(),
                },
            )
            .await?;
        object_server
            .at(
                advertisement.as_str(),
                Advertisement {
                    data: advertising_data(
                        account,
                        options.device_type.unwrap_or(account.device_type),
                    )?,
                    name: options.name,
                    stop: stop.clone(),
                },
            )
            .await?;
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(BLUEZ)?
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace(adapter_path.as_str())?
            .build();
        let mut signals = MessageStream::for_match_rule(rule, &bus, Some(64)).await?;
        let gatt = proxy(&bus, BLUEZ, &adapter_path, GATT_MANAGER).await?;
        let empty: HashMap<&str, Value<'_>> = HashMap::new();
        let application = OwnedObjectPath::try_from(app.as_str())?;
        let advert_path = OwnedObjectPath::try_from(advertisement.as_str())?;
        if let Err(error) = gatt
            .call::<_, _, ()>("RegisterApplication", &(&application, &empty))
            .await
        {
            let _ = remove_objects(&bus, &app, &service, &write, &notify, &advertisement).await;
            return Err(error).context("注册Lyra BLE GATT服务");
        }
        let adv = proxy(&bus, BLUEZ, &adapter_path, ADV_MANAGER).await?;
        if let Err(error) = adv
            .call::<_, _, ()>("RegisterAdvertisement", &(&advert_path, &empty))
            .await
        {
            let _: zbus::Result<()> = gatt.call("UnregisterApplication", &(&application,)).await;
            let _ = remove_objects(&bus, &app, &service, &write, &notify, &advertisement).await;
            return Err(error).context("注册Lyra BLE近场广播");
        }
        drop(adv);
        drop(gatt);
        tracing::info!(
            "[nearby] incoming BLE listener started: adapter={} device_type={}",
            adapter_path.rsplit('/').next().unwrap_or(""),
            account.device_type
        );
        let task = tokio::spawn(async move {
            let result: Result<()> = async {
                loop {
                    tokio::select! {
                        _ = stopped.changed() => break,
                        signal = signals.next() => {
                            let signal = signal.context("BlueZ信号流已关闭")??;
                            let path = signal.header().path().map(|path| path.as_str().to_owned()).unwrap_or_default();
                            let (interface, changed, _): (String, Properties, Vec<String>) = signal.body().deserialize()?;
                            if interface == DEVICE && changed.get("Connected").and_then(|value| bool::try_from(value).ok()) == Some(false) {
                                let id = shared.peer.lock().unwrap().as_ref().filter(|peer| peer.path == path).map(|peer| peer.id);
                                if let Some(id) = id { shared.retire(Some(id), "phone disconnected"); }
                            }
                        }
                    }
                }
                Ok(())
            }.await;
            shared.stopped.store(true, Ordering::Release);
            shared.accepting.lock().unwrap().take();
            if let Some(path) = shared.retire(None, "BLE listener stopped") {
                let _ = shared.disconnect(&path).await;
            }
            let unregistered: Result<()> = async {
                let adv = proxy(&bus, BLUEZ, &adapter_path, ADV_MANAGER).await?;
                let _: zbus::Result<()> =
                    adv.call("UnregisterAdvertisement", &(&advert_path,)).await;
                let gatt = proxy(&bus, BLUEZ, &adapter_path, GATT_MANAGER).await?;
                let _: () = gatt.call("UnregisterApplication", &(&application,)).await?;
                Ok(())
            }
            .await;
            let removed =
                remove_objects(&bus, &app, &service, &write, &notify, &advertisement).await;
            tracing::info!(
                "[nearby] incoming BLE listener stopped; GATT and advertisement withdrawn"
            );
            result
                .and(unregistered.context("撤销Lyra BLE GATT服务"))
                .and(removed)
        });
        Ok(Self {
            incoming,
            stop,
            task: Some(task),
        })
    }

    pub async fn accept(&mut self) -> Result<IncomingBleChannel> {
        self.incoming.recv().await.context("BLE入站监听已停止")
    }

    pub async fn close(mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for BleListener {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

fn advertising_data(account: &Account, device_type: u32) -> Result<Vec<u8>> {
    let identity = hex::decode(account.short_id())?;
    ensure!(identity.len() == 4, "BLE公告需要4字节设备短ID");
    let kind = u8::try_from(device_type).context("BLE设备类型超出字节范围")?;
    let mut data = vec![0, 1, kind];
    data.extend(identity);
    data.extend_from_slice(&Sha256::digest(account.uid.as_bytes())[..2]);
    // BLE HeadExt: connection medium present (BLE=1), switch/index fields,
    // Bluetooth and Lyra enabled. No LAN address is advertised.
    data.extend_from_slice(&[0x05, 0x19, 1, 0x14, 0, 3]);
    // System service 041F0323 -> A3. Legacy compact length: 2 data bytes,
    // info nibble 0 (normal discovery); Networking state version is u16 LE.
    data.extend_from_slice(&[0xa3, 0x20, 1, 0]);
    Ok(data)
}

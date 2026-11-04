use super::{BleDevice, frame as ble_frame};
use crate::Result;
use crate::transport::dbus::{Properties, proxy, string};
use anyhow::{Context, bail, ensure};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use zbus::{
    Connection, MatchRule, MessageStream,
    message::Type,
    zvariant::{OwnedObjectPath, Value},
};

const BLUEZ: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";
const CHARACTERISTIC: &str = "org.bluez.GattCharacteristic1";
// BleConnect::GattSendData / ConnectService in the official phone SDK.
const WRITE: &str = "00000001-09da-4bed-9652-f507366fcfc5";
const NOTIFY: &str = "00000002-09da-4bed-9652-f507366fcfc5";

struct Lease {
    bus: Connection,
    device: String,
    notify: Mutex<Option<String>>,
    connected_here: bool,
    closed: AtomicBool,
}
impl Lease {
    async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let notify = self.notify.lock().unwrap().take();
        if let Some(path) = notify {
            if let Ok(characteristic) = proxy(&self.bus, BLUEZ, &path, CHARACTERISTIC).await {
                let _: zbus::Result<()> = characteristic.call("StopNotify", &()).await;
            }
        }
        if self.connected_here {
            let device = proxy(&self.bus, BLUEZ, &self.device, DEVICE).await?;
            let _: () = device
                .call("Disconnect", &())
                .await
                .context("disconnect own BLE link")?;
        }
        tracing::info!("[nearby] BLE transport closed");
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let bus = self.bus.clone();
        let device = self.device.clone();
        let connected_here = self.connected_here;
        let notify = self.notify.lock().unwrap().take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(path) = notify {
                    if let Ok(characteristic) = proxy(&bus, BLUEZ, &path, CHARACTERISTIC).await {
                        let _: zbus::Result<()> = characteristic.call("StopNotify", &()).await;
                    }
                }
                if connected_here {
                    if let Ok(device) = proxy(&bus, BLUEZ, &device, DEVICE).await {
                        let _: zbus::Result<()> = device.call("Disconnect", &()).await;
                    }
                }
            });
        }
    }
}

/// Byte transport only. Physical/Logical account authentication belongs above
/// this layer, just as it does above UDP/KCP.
pub struct BleChannel {
    sender: BleSender,
    notify: String,
    signals: MessageStream,
    assembler: ble_frame::Assembler,
}
#[derive(Clone)]
pub struct BleSender {
    lease: Arc<Lease>,
    characteristic: String,
    packet_size: usize,
    write_type: &'static str,
    index: Arc<AsyncMutex<u16>>,
}
impl BleChannel {
    pub async fn open(device: &BleDevice, timeout: Duration) -> Result<Self> {
        let bus = Connection::system().await?;
        let remote = proxy(&bus, BLUEZ, &device.path, DEVICE).await?;
        let connected: bool = remote.get_property("Connected").await?;
        let lease = Arc::new(Lease {
            bus: bus.clone(),
            device: device.path.clone(),
            notify: Mutex::new(None),
            connected_here: !connected,
            closed: AtomicBool::new(false),
        });
        let operation = async {
            if !connected {
                let _: () = remote
                    .call("Connect", &())
                    .await
                    .context("connect phone BLE GATT")?;
            }
            while !remote.get_property::<bool>("ServicesResolved").await? {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let manager = proxy(&bus, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").await?;
            let objects: HashMap<OwnedObjectPath, HashMap<String, Properties>> =
                manager.call("GetManagedObjects", &()).await?;
            let mut write = None;
            let mut notify = None;
            for (path, interfaces) in &objects {
                if !path.as_str().starts_with(&format!("{}/", device.path)) {
                    continue;
                }
                let Some(properties) = interfaces.get(CHARACTERISTIC) else {
                    continue;
                };
                match string(properties, "UUID").as_deref() {
                    Some(WRITE) => write = Some(path.to_string()),
                    Some(NOTIFY) => notify = Some(path.to_string()),
                    _ => {}
                }
            }
            let write = write.context("手机未提供 Lyra BLE 写特征")?;
            let notify = notify.context("手机未提供 Lyra BLE 通知特征")?;
            let characteristic = proxy(&bus, BLUEZ, &write, CHARACTERISTIC).await?;
            let flags: Vec<String> = characteristic.get_property("Flags").await?;
            let write_type = if flags.iter().any(|flag| flag == "write") {
                "request"
            } else if flags.iter().any(|flag| flag == "write-without-response") {
                "command"
            } else {
                bail!("Lyra BLE characteristic is not writable");
            };
            let mtu = characteristic
                .get_property::<u16>("MTU")
                .await
                .unwrap_or(23);
            let rule = MatchRule::builder()
                .msg_type(Type::Signal)
                .sender(BLUEZ)?
                .interface("org.freedesktop.DBus.Properties")?
                .member("PropertiesChanged")?
                .path_namespace(device.path.as_str())?
                .build();
            let signals = MessageStream::for_match_rule(rule, &bus, Some(128)).await?;
            let characteristic = proxy(&bus, BLUEZ, &notify, CHARACTERISTIC).await?;
            let _: () = characteristic
                .call("StartNotify", &())
                .await
                .context("subscribe phone Lyra BLE transport")?;
            *lease.notify.lock().unwrap() = Some(notify.clone());
            tracing::info!("[nearby] Lyra BLE transport ready: mtu={mtu} write_type={write_type}");
            Ok::<_, anyhow::Error>(Self {
                sender: BleSender {
                    lease: lease.clone(),
                    characteristic: write,
                    // Native BleConnect clamps the negotiated ATT MTU to 483.
                    packet_size: usize::from(mtu.min(483)) - 3,
                    write_type,
                    index: Arc::new(AsyncMutex::new(u16::MAX)),
                },
                notify,
                signals,
                assembler: ble_frame::Assembler::default(),
            })
        };
        match tokio::time::timeout(timeout, operation)
            .await
            .context("phone BLE GATT setup timed out")
        {
            Ok(Ok(channel)) => Ok(channel),
            Ok(Err(error)) | Err(error) => {
                let _ = lease.close().await;
                Err(error)
            }
        }
    }

    pub fn sender(&self) -> BleSender {
        self.sender.clone()
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        while let Some(message) = self.signals.next().await {
            let message = message?;
            let header = message.header();
            let path = header.path().map(|p| p.as_str()).unwrap_or("");
            let (interface, mut changed, _): (String, Properties, Vec<String>) =
                message.body().deserialize()?;
            if path == self.sender.lease.device && interface == DEVICE {
                if changed
                    .get("Connected")
                    .is_some_and(|v| bool::try_from(v).ok() == Some(false))
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "phone disconnected BLE transport",
                    )
                    .into());
                }
            } else if path == self.notify && interface == CHARACTERISTIC {
                if let Some(value) = changed.remove("Value") {
                    let packet = Vec::<u8>::try_from(value)?;
                    if let Some(payload) = self.assembler.feed(&packet)? {
                        tracing::info!("[nearby] BLE message received: bytes={}", payload.len());
                        return Ok(payload);
                    }
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "BlueZ BLE notification stream closed",
        )
        .into())
    }

    pub async fn close(self) -> Result<()> {
        self.sender.lease.close().await
    }
}
impl BleSender {
    pub async fn send(&self, payload: &[u8]) -> Result<()> {
        ensure!(
            !self.lease.closed.load(Ordering::Acquire),
            "BLE transport is closed"
        );
        let mut index = self.index.lock().await;
        *index = index.wrapping_add(1);
        let packets = ble_frame::pack(payload, *index, self.packet_size)?;
        tracing::info!(
            "[nearby] BLE message send: bytes={} index={} fragments={}",
            payload.len(),
            *index,
            packets.len()
        );
        let characteristic =
            proxy(&self.lease.bus, BLUEZ, &self.characteristic, CHARACTERISTIC).await?;
        let options = HashMap::from([("type", Value::from(self.write_type))]);
        for packet in packets {
            let _: () = characteristic
                .call("WriteValue", &(&packet, &options))
                .await
                .context("write Lyra BLE transport fragment")?;
        }
        Ok(())
    }
}

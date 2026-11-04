use crate::transport::dbus::{Properties, proxy, string};
use crate::{Result, identity::Account};
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};
use zbus::{
    Connection, MatchRule, MessageStream,
    message::Type,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const FCC0: &str = "0000fcc0-0000-1000-8000-00805f9b34fb";
const FC66: &str = "0000fc66-0000-1000-8000-00805f9b34fb";
type Interfaces = std::collections::HashMap<String, Properties>;
type Objects = std::collections::HashMap<OwnedObjectPath, Interfaces>;

#[derive(Clone, Debug)]
pub struct NearbyOptions {
    /// BlueZ adapter name, for example hci0. None selects a powered adapter.
    pub adapter: Option<String>,
    pub duration: Duration,
}
impl Default for NearbyOptions {
    fn default() -> Self {
        Self {
            adapter: None,
            duration: Duration::from_secs(12),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Advertisement {
    pub flags: u16,
    pub device_type: u8,
    pub advertising_id: u32,
    pub same_account: Option<bool>,
    pub name: Option<String>,
    pub records: Vec<(u8, Vec<u8>)>,
}
impl Advertisement {
    // libmicontinuity DiscoveryProtocolBase::ParseCommon/ParseAccount and
    // DiscoveryProtocolBle::GetTypeLength; FCC0 verified on the actual phone.
    fn parse(data: &[u8], account_filter: [u8; 2]) -> Result<Self> {
        ensure!(
            data.len() >= 7,
            "Lyra BLE advertisement header is incomplete"
        );
        let flags = u16::from_be_bytes([data[0], data[1]]);
        let mut position = 7;
        let same_account = if flags & 1 != 0 {
            ensure!(
                data.len() >= position + 2,
                "Lyra BLE account filter is incomplete"
            );
            let same = data[position..position + 2] == account_filter;
            position += 2;
            Some(same)
        } else {
            None
        };
        for (flag, length) in [(2, 2), (4, 3)] {
            if flags & flag != 0 {
                position += length;
            }
        }
        ensure!(
            position <= data.len(),
            "Lyra BLE account filters are incomplete"
        );
        let mut records = Vec::new();
        let mut name = None;
        while position < data.len() {
            let head = data[position];
            position += 1;
            let (kind, length) = if head < 0xa0 {
                (head >> 4, usize::from(head & 15))
            } else {
                let length = *data
                    .get(position)
                    .context("Lyra BLE record length is missing")?;
                position += 1;
                (
                    head,
                    usize::from(if flags & 0x100 != 0 {
                        length
                    } else {
                        length >> 4
                    }),
                )
            };
            let value = data
                .get(position..position + length)
                .context("Lyra BLE record is incomplete")?;
            if kind == 2 {
                name = std::str::from_utf8(value).ok().map(str::to_owned);
            }
            records.push((kind, value.to_vec()));
            position += length;
        }
        Ok(Self {
            flags,
            device_type: data[2],
            advertising_id: u32::from_be_bytes(data[3..7].try_into().unwrap()),
            same_account,
            name,
            records,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BleDevice {
    /// Private local BlueZ object path; this is not a Wi-Fi Direct address.
    pub path: String,
    pub address: String,
    pub address_type: String,
    pub name: String,
    pub rssi: Option<i16>,
    pub advertisement: Advertisement,
}

pub struct NearbyScanner {
    devices: watch::Receiver<Vec<BleDevice>>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}
impl NearbyScanner {
    pub async fn start(account: &Account, options: NearbyOptions) -> Result<Self> {
        let bus = Connection::system()
            .await
            .context("connect BlueZ system bus")?;
        let manager = proxy(&bus, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager").await?;
        let mut objects: Objects = manager.call("GetManagedObjects", &()).await?;
        let adapter_path = objects
            .iter()
            .find_map(|(path, interfaces)| {
                let properties = interfaces.get(ADAPTER)?;
                let selected = options
                    .adapter
                    .as_ref()
                    .is_none_or(|name| path.as_str().rsplit('/').next() == Some(name.as_str()));
                let powered = properties
                    .get("Powered")
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(false);
                (selected && powered).then(|| path.to_string())
            })
            .context("没有已开启的 BlueZ 蓝牙适配器")?;
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(BLUEZ)?
            .path_namespace("/")?
            .build();
        let mut signals = MessageStream::for_match_rule(rule, &bus, Some(64)).await?;
        let adapter = proxy(&bus, BLUEZ, &adapter_path, ADAPTER).await?;
        let filter = std::collections::HashMap::from([
            ("Transport", Value::from("le")),
            ("DuplicateData", Value::from(true)),
        ]);
        let _: () = adapter.call("SetDiscoveryFilter", &(filter,)).await?;
        let _: () = adapter
            .call("StartDiscovery", &())
            .await
            .context("start BlueZ LE discovery")?;
        let filter: [u8; 2] = Sha256::digest(account.uid.as_bytes())[..2]
            .try_into()
            .unwrap();
        let (updates, devices) = watch::channel(Vec::new());
        let (stop, mut stopped) = watch::channel(false);
        tracing::info!("[nearby] BlueZ Lyra LE discovery started");
        drop(adapter);
        drop(manager);
        let task = tokio::spawn(async move {
            let mut seen = BTreeSet::new();
            let mut device_events = 0u64;
            let mut service_data_events = 0u64;
            let deadline = tokio::time::sleep(options.duration);
            tokio::pin!(deadline);
            let result: Result<()> = async {
                loop {
                    let message = tokio::select! {
                        _ = &mut deadline => break,
                        _ = stopped.changed() => break,
                        message = signals.next() => message.context("BlueZ signal stream closed")??,
                    };
                    let header = message.header();
                    match header.member().map(|m| m.as_str()) {
                        Some("InterfacesAdded") => {
                            let (path, interfaces): (OwnedObjectPath, Interfaces) =
                                message.body().deserialize()?;
                            if interfaces.contains_key(DEVICE) {
                                device_events += 1;
                                if interfaces
                                    .get(DEVICE)
                                    .is_some_and(|p| p.contains_key("ServiceData"))
                                {
                                    service_data_events += 1;
                                }
                                seen.insert(path.to_string());
                            }
                            objects.entry(path).or_default().extend(interfaces);
                        }
                        Some("InterfacesRemoved") => {
                            let (path, interfaces): (OwnedObjectPath, Vec<String>) =
                                message.body().deserialize()?;
                            if let Some(current) = objects.get_mut(&path) {
                                for name in interfaces {
                                    current.remove(&name);
                                    if name == DEVICE {
                                        seen.remove(path.as_str());
                                    }
                                }
                            }
                        }
                        Some("PropertiesChanged") => {
                            let (interface, changed, invalidated): (
                                String,
                                Properties,
                                Vec<String>,
                            ) = message.body().deserialize()?;
                            if interface != DEVICE {
                                continue;
                            }
                            device_events += 1;
                            if changed.contains_key("ServiceData") {
                                service_data_events += 1;
                            }
                            if let Some(path) = header.path() {
                                if changed.contains_key("ServiceData")
                                    || changed.contains_key("RSSI")
                                {
                                    seen.insert(path.to_string());
                                }
                                let properties = objects
                                    .entry(path.to_owned().into())
                                    .or_default()
                                    .entry(interface)
                                    .or_default();
                                properties.extend(changed);
                                for key in invalidated {
                                    properties.remove(&key);
                                }
                            }
                        }
                        _ => continue,
                    }
                    updates.send_replace(snapshot(&objects, &seen, filter).0);
                }
                Ok(())
            }
            .await;
            let adapter = proxy(&bus, BLUEZ, &adapter_path, ADAPTER).await?;
            let stopped: zbus::Result<()> = adapter.call("StopDiscovery", &()).await;
            let cached_paths = objects.keys().map(|path| path.to_string()).collect();
            let (cached, cached_fcc0_decode_failures) = snapshot(&objects, &cached_paths, filter);
            let fresh_same_account = updates
                .borrow()
                .iter()
                .filter(|device| device.advertisement.same_account == Some(true))
                .count();
            let cached_same_account = cached
                .iter()
                .filter(|device| device.advertisement.same_account == Some(true))
                .count();
            tracing::info!(
                "[nearby] BlueZ discovery finished: devices={}, device_events={device_events}, service_data_events={service_data_events}, seen={}, cached_lyra={}, fresh_same_account={fresh_same_account}, cached_same_account={cached_same_account}, cached_fcc0_decode_failures={cached_fcc0_decode_failures}",
                updates.borrow().len(),
                seen.len(),
                cached.len()
            );
            result?;
            stopped.context("stop own BlueZ discovery session")?;
            Ok(())
        });
        Ok(Self {
            devices,
            stop,
            task: Some(task),
        })
    }
    pub fn subscribe(&self) -> watch::Receiver<Vec<BleDevice>> {
        self.devices.clone()
    }
    pub fn devices(&self) -> Vec<BleDevice> {
        self.devices.borrow().clone()
    }
    pub async fn wait(&mut self) -> Result<Vec<BleDevice>> {
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(self.devices())
    }
    pub async fn close(mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}
impl Drop for NearbyScanner {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

fn snapshot(
    objects: &Objects,
    seen: &BTreeSet<String>,
    account_filter: [u8; 2],
) -> (Vec<BleDevice>, usize) {
    let mut devices = BTreeMap::new();
    let mut fcc0_decode_failures = 0;
    for (path, interfaces) in objects {
        if !seen.contains(path.as_str()) {
            continue;
        }
        let Some(properties) = interfaces.get(DEVICE) else {
            continue;
        };
        let Some(value) = properties.get("ServiceData") else {
            continue;
        };
        let Ok(value) = value.try_clone() else {
            continue;
        };
        let Ok(services) = std::collections::HashMap::<String, OwnedValue>::try_from(value) else {
            continue;
        };
        for (uuid, value) in services {
            if uuid != FCC0 && uuid != FC66 {
                continue;
            }
            let Ok(bytes) = Vec::<u8>::try_from(value) else {
                fcc0_decode_failures += usize::from(uuid == FCC0);
                continue;
            };
            let Ok(advertisement) = Advertisement::parse(&bytes, account_filter) else {
                fcc0_decode_failures += usize::from(uuid == FCC0);
                continue;
            };
            let Some(address) = string(properties, "Address") else {
                continue;
            };
            let name = advertisement
                .name
                .clone()
                .or_else(|| string(properties, "Name"))
                .unwrap_or_else(|| "小米设备".into());
            devices.insert(
                path.to_string(),
                BleDevice {
                    path: path.to_string(),
                    address,
                    address_type: string(properties, "AddressType").unwrap_or_default(),
                    name,
                    rssi: properties.get("RSSI").and_then(|v| i16::try_from(v).ok()),
                    advertisement,
                },
            );
        }
    }
    (devices.into_values().collect(), fcc0_decode_failures)
}

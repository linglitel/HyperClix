//! BlueZ GATT service/characteristic and advertisement objects.
use super::Shared;
use crate::Result;
use crate::transport::dbus::{Properties, string};
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
};
use tokio::sync::watch;
use zbus::{
    Connection, fdo,
    zvariant::{OwnedObjectPath, Value},
};

const FCC0: &str = "0000fcc0-0000-1000-8000-00805f9b34fb";
const WRITE: &str = "00000001-09da-4bed-9652-f507366fcfc5";
const NOTIFY: &str = "00000002-09da-4bed-9652-f507366fcfc5";

pub(super) struct GattService;

#[zbus::interface(name = "org.bluez.GattService1")]
impl GattService {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        FCC0
    }
    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn includes(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }
}

pub(super) struct WriteCharacteristic {
    pub(super) service: OwnedObjectPath,
    pub(super) shared: Arc<Shared>,
}

// Fragment writes must keep the order in which BlueZ delivered them.
#[zbus::interface(name = "org.bluez.GattCharacteristic1", spawn = false)]
impl WriteCharacteristic {
    async fn write_value(&self, value: Vec<u8>, options: Properties) -> fdo::Result<()> {
        let write_type = string(&options, "type").unwrap_or_default();
        let offset = options
            .get("offset")
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(0);
        let mtu = options
            .get("mtu")
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(23);
        if value.len() >= 8 {
            let length = (usize::from(value[2]) << 4) | usize::from(value[3] >> 4);
            tracing::info!(
                "[nearby] incoming BLE fragment: bytes={} wire_length={length} header_length={} index={} sequence={} flags=0x{:02x} write_type={write_type} offset={offset} mtu={mtu}",
                value.len(),
                value[3] & 15,
                u16::from_be_bytes([value[4], value[5]]),
                value[6],
                value[7],
            );
        } else {
            tracing::info!(
                "[nearby] incoming BLE fragment: bytes={} short_header=true write_type={write_type} offset={offset} mtu={mtu}",
                value.len(),
            );
        }
        match self.shared.receive(&value, &options).await {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::info!("[nearby] incoming BLE fragment rejected: {error:#}");
                Err(fdo::Error::Failed(error.to_string()))
            }
        }
    }
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        WRITE
    }
    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        self.service.clone()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<&str> {
        vec!["write", "write-without-response"]
    }
}

pub(super) struct NotifyCharacteristic {
    pub(super) service: OwnedObjectPath,
    pub(super) shared: Arc<Shared>,
}

#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl NotifyCharacteristic {
    async fn start_notify(&self) {
        self.shared.notifying.store(true, Ordering::Release);
        tracing::info!("[nearby] incoming BLE notifications enabled");
    }
    async fn stop_notify(&self) {
        self.shared.notifying.store(false, Ordering::Release);
        self.shared.retire(None, "phone disabled BLE notifications");
    }
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        NOTIFY
    }
    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        self.service.clone()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<&str> {
        vec!["notify"]
    }
    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        self.shared.value.lock().unwrap().clone()
    }
    #[zbus(property)]
    fn notifying(&self) -> bool {
        self.shared.notifying.load(Ordering::Acquire)
    }
}

pub(super) struct Advertisement {
    pub(super) data: Vec<u8>,
    pub(super) name: String,
    pub(super) stop: watch::Sender<bool>,
}

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    fn release(&self) {
        self.stop.send_replace(true);
    }
    #[zbus(property, name = "Type")]
    fn advertisement_type(&self) -> &str {
        "peripheral"
    }
    #[zbus(property)]
    fn service_data(&self) -> HashMap<&str, Value<'_>> {
        HashMap::from([(FCC0, Value::from(self.data.as_slice()))])
    }
    #[zbus(property)]
    fn local_name(&self) -> &str {
        &self.name
    }
}

pub(super) async fn remove_objects(
    bus: &Connection,
    app: &str,
    service: &str,
    write: &str,
    notify: &str,
    advertisement: &str,
) -> Result<()> {
    let objects = bus.object_server();
    let results = [
        objects.remove::<WriteCharacteristic, _>(write).await,
        objects.remove::<NotifyCharacteristic, _>(notify).await,
        objects.remove::<GattService, _>(service).await,
        objects.remove::<fdo::ObjectManager, _>(app).await,
        objects.remove::<Advertisement, _>(advertisement).await,
    ];
    for result in results {
        result?;
    }
    Ok(())
}

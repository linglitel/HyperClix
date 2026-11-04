//! BlueZ BLE scan, GATT byte channel and connectable listener.
pub(crate) mod frame;
pub(crate) mod gatt;
pub(crate) mod listener;
pub(crate) mod scan;

pub use gatt::{BleChannel, BleSender};
pub use listener::{BleListener, BleListenerOptions, IncomingBleChannel, IncomingBleSender};
pub use scan::{Advertisement, BleDevice, NearbyOptions, NearbyScanner};

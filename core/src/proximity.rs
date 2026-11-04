//! Public near-field facade. The implementation lives in `transport`.
pub use crate::transport::ble::{
    Advertisement, BleChannel, BleDevice, BleListener, BleListenerOptions, BleSender,
    IncomingBleChannel, IncomingBleSender, NearbyOptions, NearbyScanner,
};
pub use crate::transport::p2p::{P2pLink, P2pPeer, WifiDirect};
pub use crate::transport::upgrade::{UpgradeClient, WifiCapabilities};

#[doc(hidden)]
pub use crate::transport::p2p::helper as p2p_helper;

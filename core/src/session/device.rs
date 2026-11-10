//! Local device identity and published Networking services.
use super::{ConnectOptions, MEDIUM_LAN, MEDIUM_LAN1, MEDIUM_P2P, random_key};
use crate::common::time::epoch_ms;
use crate::protocol::messages::device::{
    DeviceCredential, DeviceGroup, DeviceService, NetworkDevice, PhysicalDevice,
};
use crate::protocol::messages::lyra::PhysicalFrame;
use crate::{Result, identity::Account, proximity::WifiCapabilities};
use openssl::{hash::MessageDigest, sign::Signer};

pub(super) fn physical_device(
    account: &Account,
    name: &str,
    device_type: u32,
    supported_medium: u64,
    wifi: Option<&WifiCapabilities>,
) -> PhysicalDevice {
    let supported_medium = supported_medium | if wifi.is_some() { MEDIUM_P2P } else { 0 };
    PhysicalDevice {
        short_id: account.short_id().into_bytes(),
        account_id: account.account_id()[..4].as_bytes().to_vec(),
        device_type: device_type as u64,
        name: name.into(),
        supported_medium,
        wifi_proto: wifi.map(WifiCapabilities::physical_proto),
    }
}

pub(super) fn physical_sync(
    account: &Account,
    name: &str,
    device_type: u32,
    medium: u64,
    wifi: Option<&WifiCapabilities>,
) -> PhysicalFrame {
    PhysicalFrame::sync_request(
        epoch_ms(),
        physical_device(account, name, device_type, MEDIUM_LAN | medium, wifi),
        medium,
    )
}

pub(super) fn network_device(account: &Account, o: &ConnectOptions) -> Result<NetworkDevice> {
    let nonce = random_key();
    let mut signer = Signer::new(MessageDigest::sha256(), &account.pkey)?;
    signer.update(&nonce)?;
    let group = DeviceGroup {
        credential: DeviceCredential {
            nonce,
            certificate: account.cert3.to_der()?,
            signature: signer.sign_to_vec()?,
        },
    };
    let mut services = vec![DeviceService {
        name: "cast".into(),
        app: "com.xiaomi.mirror".into(),
        data: o.capabilities.encode().to_vec(),
    }];
    if o.publish_mishare {
        services.push(DeviceService {
            name: "miLyraShare".into(),
            app: "com.miui.mishare.connectivity".into(),
            data: Vec::new(),
        });
    }
    if o.publish_notifications {
        // PreferencesManager#getTrustDeviceInfo filters by this exact service
        // before reading PropMarketName. A name alone does not pass the filter.
        services.push(DeviceService {
            name: "NotificationTrans".into(),
            app: "com.milink.service".into(),
            data: Vec::new(),
        });
    }
    if o.publish_clipboard {
        services.push(DeviceService {
            name: "universalClipboard".into(),
            app: "pcmanager".into(),
            data: vec![1],
        });
    }
    // HardwareManager.onDeviceOnline reads this service to obtain the PC's
    // metadata version; without it the phone logs serviceInfo=null and treats
    // the version as zero.
    services.push(DeviceService {
        name: "publicMetadataVersion".into(),
        app: "com.milink.service".into(),
        // DeviceManager.p(byte[]) folds the service data as a big-endian
        // integer. The phone itself publishes two bytes, not ASCII "0000".
        data: vec![0, 0],
    });
    services.push(DeviceService {
        name: "cameraP2P".into(),
        app: "com.milink.service".into(),
        data: Vec::new(),
    });
    Ok(NetworkDevice {
        name: o.device_name.clone(),
        device_type: account.device_type as u64,
        full_id: account.full_id().into_bytes(),
        account_id: account.account_id().into_bytes(),
        service_version: None,
        services,
        group: Some(group),
        region: o.build_region.clone(),
        market_name: Some(
            o.market_name
                .as_deref()
                .unwrap_or(&o.device_name)
                .to_owned(),
        ),
        supported_medium: Some(MEDIUM_LAN | MEDIUM_LAN1),
        short_id: Some(account.short_id()),
    })
}

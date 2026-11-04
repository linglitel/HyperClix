//! Cast service declarations and the recovered type 1/64/66 messages.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The four-byte service block, with named overrides. Unspecified bits are kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CastCapabilities {
    pub raw: [u8; 4],
    pub status_version: Option<u8>,
    pub handoff_status: Option<bool>,
    pub desktop_status: Option<bool>,
    pub car_connected: Option<bool>,
    pub keyguard_open_status: Option<bool>,
    pub connectivity_service: Option<bool>,
    pub multi_user_status: Option<bool>,
    pub screen_fold_status: Option<bool>,
    pub sku_open: Option<bool>,
    pub support_wlan_channel: Option<bool>,
    pub support_mac_sidecar: Option<bool>,
    pub support_new_ios_home_page: Option<bool>,
    pub support_common_pc: Option<bool>,
    pub support_mac_subscreen_create_reason: Option<bool>,
    pub support_same_device_connect: Option<bool>,
    pub support_screen_permission_prompt: Option<bool>,
    pub support_text_paste: Option<bool>,
    pub support_iphone_mirror: Option<bool>,
}

impl Default for CastCapabilities {
    fn default() -> Self {
        Self {
            raw: [1, 0, 0, 8],
            status_version: None,
            handoff_status: None,
            desktop_status: None,
            car_connected: None,
            keyguard_open_status: None,
            connectivity_service: None,
            multi_user_status: None,
            screen_fold_status: None,
            sku_open: None,
            support_wlan_channel: Some(true),
            support_mac_sidecar: None,
            support_new_ios_home_page: None,
            support_common_pc: None,
            support_mac_subscreen_create_reason: None,
            support_same_device_connect: None,
            support_screen_permission_prompt: Some(true),
            support_text_paste: Some(true),
            support_iphone_mirror: None,
        }
    }
}

impl CastCapabilities {
    pub fn encode(&self) -> [u8; 4] {
        let mut data = self.raw;
        if let Some(version) = self.status_version {
            data[0] = (data[0] & 0xf0) | (version & 15);
        }
        for (byte, bit, value) in [
            (1, 7, self.handoff_status),
            (1, 6, self.desktop_status),
            (1, 5, self.car_connected),
            (1, 4, self.keyguard_open_status),
            (1, 3, self.connectivity_service),
            (1, 2, self.multi_user_status),
            (1, 1, self.screen_fold_status),
            (1, 0, self.sku_open),
            (2, 6, self.support_wlan_channel),
            (2, 5, self.support_mac_sidecar),
            (2, 0, self.support_new_ios_home_page),
            (3, 7, self.support_common_pc),
            (3, 6, self.support_mac_subscreen_create_reason),
            (3, 5, self.support_same_device_connect),
            (3, 4, self.support_screen_permission_prompt),
            (3, 3, self.support_text_paste),
            (3, 2, self.support_iphone_mirror),
        ] {
            if let Some(on) = value {
                data[byte] = (data[byte] & !(1 << bit)) | ((on as u8) << bit);
            }
        }
        data
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceData {
    pub status_version: u8,
    pub raw: BTreeMap<String, bool>,
    pub effective: BTreeMap<String, bool>,
}

/// Inspect the declared bits and the phone's version-dependent interpretation.
pub fn read_service_data(data: [u8; 4]) -> ServiceData {
    let status_version = data[0] & 15;
    let raw: BTreeMap<_, _> = [
        ("handoff_status", 1, 7),
        ("desktop_status", 1, 6),
        ("car_connected", 1, 5),
        ("keyguard_open_status", 1, 4),
        ("connectivity_service", 1, 3),
        ("multi_user_status", 1, 2),
        ("screen_fold_status", 1, 1),
        ("sku_open", 1, 0),
        ("support_wlan_channel", 2, 6),
        ("support_mac_sidecar", 2, 5),
        ("support_new_ios_home_page", 2, 0),
        ("support_common_pc", 3, 7),
        ("support_mac_subscreen_create_reason", 3, 6),
        ("support_same_device_connect", 3, 5),
        ("support_screen_permission_prompt", 3, 4),
        ("support_text_paste", 3, 3),
        ("support_iphone_mirror", 3, 2),
    ]
    .into_iter()
    .map(|(name, byte, bit)| (name.to_string(), data[byte] & (1 << bit) != 0))
    .collect();
    let mut effective = raw.clone();
    for (name, minimum, earlier) in [
        ("car_connected", 11, false),
        ("connectivity_service", 5, true),
        ("multi_user_status", 6, true),
        ("screen_fold_status", 9, false),
    ] {
        if status_version < minimum {
            effective.insert(name.to_string(), earlier);
        }
    }
    ServiceData {
        status_version,
        raw,
        effective,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_service_vector() {
        assert_eq!(CastCapabilities::default().encode(), [1, 0, 0x40, 0x18]);
    }
}

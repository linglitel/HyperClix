//! Public screen configuration and event types.
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use super::audio;

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Requested App canvas. Defaults to 720×1280 at Android density 320.
/// Width and height are source pixels; density controls Android logical size.
pub struct AppOptions {
    pub width: u32,
    pub height: u32,
    pub density: u32,
    pub prefer_phone_ime: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            width: 720,
            height: 1280,
            density: 320,
            prefer_phone_ime: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenConfiguration {
    pub phone_ip: Ipv4Addr,
    pub screen_id: u32,
    pub session_id: u64,
    pub port: u16,
    pub width: u32,
    pub height: u32,
    pub real_width: u32,
    pub real_height: u32,
    pub landscape: bool,
    pub app_name: String,
    pub configuration: u32,
    pub support_max: u32,
    pub supports_volume: bool,
    pub volume_value: i32,
}

#[derive(Clone, Debug)]
pub enum ScreenEvent {
    ConfigurationChanged(ScreenConfiguration),
    AudioFocusChanged(audio::AudioFocus),
    PhoneVolumeChanged(audio::PhoneVolume),
    Message { message_type: u8, payload: Vec<u8> },
    Closed(String),
    Error(String),
}

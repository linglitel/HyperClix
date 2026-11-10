//! Typed CAST/Mirror messages.
use crate::Result;
use crate::protocol::pb::{Fields, Writer};
use anyhow::ensure;

pub(crate) const TYPE_MOUSE: u8 = 3;
pub(crate) const TYPE_INPUT: u8 = 4;
pub(crate) const TYPE_SCREEN: u8 = 5;
pub(crate) const TYPE_DRAG: u8 = 8;
pub(crate) const TYPE_SCREEN_COMMAND: u8 = 12;
pub(crate) const TYPE_SIMPLE_EVENT: u8 = 18;
pub(crate) const TYPE_RELAY_APP: u8 = 22;
pub(crate) const TYPE_RELAY_OFFICE: u8 = 23;
pub(crate) const TYPE_RELAY_APP_INTENT: u8 = 30;
pub(crate) const TYPE_RELAY_BROWSER: u8 = 32;
pub(crate) const TYPE_AUDIO_FOCUS: u8 = 46;
pub(crate) const TYPE_APP_COMMAND: u8 = 62;
pub(crate) const TYPE_RECENT_APPS: u8 = 72;
pub(crate) const TYPE_VOLUME_SET: u8 = 53;
pub(crate) const TYPE_VOLUME_BASIS: u8 = 54;
pub(crate) const TYPE_VOLUME_INDEX: u8 = 59;

#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub message_type: u8,
    pub payload: Vec<u8>,
}

pub(crate) fn frame(message_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![message_type];
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Default)]
pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub(crate) fn feed(&mut self, data: &[u8]) -> Result<Vec<Frame>> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        let mut cursor = 0;
        while self.buffer.len() - cursor >= 5 {
            let size = u32::from_le_bytes(self.buffer[cursor + 1..cursor + 5].try_into()?) as usize;
            ensure!(
                size <= 16 * 1024 * 1024,
                "Mirror payload exceeds protocol limit"
            );
            let end = cursor + 5 + size;
            if end > self.buffer.len() {
                break;
            }
            out.push(Frame {
                message_type: self.buffer[cursor],
                payload: self.buffer[cursor + 5..end].to_vec(),
            });
            cursor = end;
        }
        self.buffer.drain(..cursor);
        Ok(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenUpdate {
    pub session_id: u64,
    pub screen_id: u32,
    pub configuration: u32,
    pub port: u16,
    pub width: u32,
    pub height: u32,
    pub real_width: u32,
    pub real_height: u32,
    pub landscape: bool,
    pub package: String,
    pub app_name: String,
    pub support_max: u32,
    pub supports_volume: bool,
    pub has_supports_volume: bool,
    pub volume: i32,
    pub has_volume: bool,
    pub update_flag: u64,
}

impl ScreenUpdate {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            session_id: fields.uint(1),
            screen_id: fields.uint(2) as u32,
            configuration: fields.uint(3) as u32,
            port: fields.uint(5) as u16,
            width: fields.uint(6) as u32,
            height: fields.uint(7) as u32,
            real_width: fields.uint(23) as u32,
            real_height: fields.uint(24) as u32,
            landscape: fields.uint(14) != 0,
            package: fields.text(11),
            app_name: fields.text(15),
            support_max: fields.uint(12) as u32,
            supports_volume: fields.uint(21) != 0,
            has_supports_volume: fields.has(21),
            volume: fields.uint(20) as i32,
            has_volume: fields.has(20),
            update_flag: fields.uint(4),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScreenCommand {
    Desktop,
    App {
        package: String,
        width: u32,
        height: u32,
        density: u32,
        screen_id: u32,
        resize: bool,
        prefer_phone_ime: bool,
    },
    Resize {
        session_id: u64,
        screen_id: u32,
        width: u32,
        height: u32,
        density: u32,
    },
    Close {
        session_id: u64,
        screen_id: u32,
    },
}

impl ScreenCommand {
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Self::Desktop => frame(TYPE_SCREEN_COMMAND, &field(3, 4)),
            Self::App {
                package,
                width,
                height,
                density,
                screen_id,
                resize,
                prefer_phone_ime,
            } => {
                let payload = [
                    field(1, 2),
                    optional(2, u64::from(*height >= *width)),
                    optional(3, u64::from(*resize)),
                    bytes(4, package),
                    field(5, *width as u64),
                    field(6, *height as u64),
                    optional(7, *screen_id as u64),
                    field(8, *density as u64),
                    optional(12, u64::from(*prefer_phone_ime)),
                ]
                .concat();
                frame(TYPE_APP_COMMAND, &payload)
            }
            Self::Resize {
                session_id,
                screen_id,
                width,
                height,
                density,
            } => {
                let payload = [
                    optional(1, *session_id),
                    optional(2, *screen_id as u64),
                    field(3, 2),
                    field(4, *width as u64),
                    field(5, *height as u64),
                    field(8, *density as u64),
                ]
                .concat();
                frame(TYPE_SCREEN_COMMAND, &payload)
            }
            Self::Close {
                session_id,
                screen_id,
            } => {
                let payload = [optional(1, *session_id), optional(2, *screen_id as u64)].concat();
                frame(TYPE_SCREEN_COMMAND, &payload)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenAck {
    pub session_id: u64,
    pub screen_id: u32,
}

impl ScreenAck {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(
            TYPE_SCREEN,
            &[
                optional(1, self.session_id),
                optional(2, self.screen_id as u64),
                field(4, 1),
            ]
            .concat(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MouseInput {
    pub session_id: u64,
    pub screen_id: u32,
    pub action: u32,
    pub state: u32,
    pub x: i32,
    pub y: i32,
    pub scroll: i32,
}

impl MouseInput {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(
            TYPE_MOUSE,
            &[
                optional(1, self.session_id),
                optional(2, self.screen_id as u64),
                optional(3, self.action as u64),
                optional(4, self.state as u64),
                optional(5, self.x as i64 as u64),
                optional(6, self.y as i64 as u64),
                optional(7, self.scroll as i64 as u64),
            ]
            .concat(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyInput {
    pub session_id: u64,
    pub screen_id: u32,
    pub code: u32,
    pub meta: u32,
    pub down: bool,
}

impl KeyInput {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let key = [
            field(1, self.code as u64),
            optional(2, self.meta as u64),
            optional(3, u64::from(self.down)),
        ]
        .concat();
        frame(
            TYPE_INPUT,
            &[
                optional(1, self.session_id),
                optional(2, self.screen_id as u64),
                bytes(3, &key),
                field(5, 1),
            ]
            .concat(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextInput {
    pub session_id: u64,
    pub screen_id: u32,
    pub text: String,
}

impl TextInput {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(
            TYPE_INPUT,
            &[
                optional(1, self.session_id),
                optional(2, self.screen_id as u64),
                bytes(4, &self.text),
            ]
            .concat(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioFocus {
    pub screen_id: Option<u32>,
    pub muted: bool,
    pub channel_type: u32,
    pub focus_gain: u32,
    pub usage: u32,
    pub package_name: String,
    pub client_id: String,
    pub update_flag: u64,
}

impl AudioFocus {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            screen_id: fields.has(2).then(|| fields.uint(2) as u32),
            muted: fields.uint(3) != 0,
            channel_type: fields.uint(10) as u32,
            focus_gain: fields.uint(6) as u32,
            usage: fields.uint(7) as u32,
            package_name: fields.text(8),
            client_id: fields.text(9),
            update_flag: fields.uint(11),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VolumeBasis {
    pub kind: u64,
    pub basis_points: u32,
}

impl VolumeBasis {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            kind: fields.uint(1),
            basis_points: fields.uint(2) as u32,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VolumeIndex {
    pub index: i32,
}

impl VolumeIndex {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        Ok(Self {
            index: Fields::decode(payload)?.uint(1) as i32,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AudioFocusSync {
    pub session_id: u64,
    pub screen_id: u32,
}

impl AudioFocusSync {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(
            TYPE_AUDIO_FOCUS,
            &[
                optional(1, self.session_id),
                optional(2, self.screen_id as u64),
                field(11, 1),
            ]
            .concat(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VolumeSet {
    pub basis_points: u32,
}

impl VolumeSet {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(
            TYPE_VOLUME_SET,
            &[field(1, 1), field(2, self.basis_points as u64)].concat(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DragMessage {
    pub session_id: u64,
    pub screen_id: u32,
    pub action: u64,
    pub x: i32,
    pub y: i32,
    pub data: Option<Vec<u8>>,
}

impl DragMessage {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            session_id: fields.uint(1),
            screen_id: fields.uint(3) as u32,
            action: fields.uint(2),
            x: fields.uint(4) as i32,
            y: fields.uint(5) as i32,
            data: fields.has(6).then(|| fields.bytes(6).to_vec()),
        })
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut payload = [
            field(1, self.session_id),
            field(2, self.action),
            field(3, self.screen_id as u64),
            field(4, self.x as i64 as u64),
            field(5, self.y as i64 as u64),
        ]
        .concat();
        if let Some(data) = &self.data {
            payload.extend(bytes(6, data));
        }
        frame(TYPE_DRAG, &payload)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecentAppsRequest;

impl RecentAppsRequest {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(TYPE_RECENT_APPS, &field(2, 1))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecentApp {
    pub package: String,
    pub name: String,
    pub icon: Vec<u8>,
}

impl RecentAppsRequest {
    pub(crate) fn decode_response(payload: &[u8]) -> Result<Vec<RecentApp>> {
        let response = Fields::decode(payload)?;
        ensure!(response.uint(2) == 2, "unexpected recent-apps response");
        let clip = response.message::<Fields>(1)?;
        let names: Vec<String> = clip
            .repeated_message::<Fields>(3)?
            .into_iter()
            .map(|entry| entry.text(2))
            .collect();
        let mut apps = Vec::new();
        for (index, app) in clip.repeated_message::<Fields>(2)?.into_iter().enumerate() {
            let package = app.text(2);
            let name = names
                .get(index)
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| package.clone());
            apps.push(RecentApp {
                package,
                name,
                icon: app.bytes(4).to_vec(),
            });
        }
        Ok(apps)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RelayTrackTime {
    pub start_time: i64,
    pub send_time: i64,
}

impl RelayTrackTime {
    fn decode(data: &[u8]) -> Result<Self> {
        let fields = Fields::decode(data)?;
        Ok(Self {
            start_time: fields.uint(1) as i64,
            send_time: fields.uint(2) as i64,
        })
    }
}

/// `duo.screen.RelayApplication` (Mirror message type 22).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RelayApplication {
    pub app_name: String,
    pub app_icon: Vec<u8>,
    pub description: String,
    pub process_name: String,
    pub kind: u32,
    pub is_relay: bool,
    pub is_hide_icon: bool,
    pub no_icon: bool,
    pub support_sub_screen: bool,
    pub support_handoff: bool,
    pub unregister_relay: bool,
    pub support_pin: bool,
    pub track_time: Option<RelayTrackTime>,
}

impl RelayApplication {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        let track_time = if fields.has(8) {
            Some(RelayTrackTime::decode(fields.bytes(8))?)
        } else {
            None
        };
        Ok(Self {
            app_name: fields.text(1),
            app_icon: fields.bytes(2).to_vec(),
            description: fields.text(3),
            process_name: fields.text(4),
            kind: fields.uint(5) as u32,
            is_relay: fields.uint(6) != 0,
            is_hide_icon: fields.uint(7) != 0,
            no_icon: fields.uint(9) != 0,
            support_sub_screen: fields.uint(10) != 0,
            support_handoff: fields.uint(11) != 0,
            unregister_relay: fields.uint(12) != 0,
            support_pin: fields.uint(13) != 0,
            track_time,
        })
    }
}

/// `duo.screen.RelayBrowser` (Mirror message type 32).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RelayBrowser {
    pub url_type: u32,
    pub url: String,
    pub cookie: String,
    pub target_packages: Vec<String>,
    pub session_id: u64,
    pub package: String,
}

impl RelayBrowser {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            url_type: fields.uint(1) as u32,
            url: fields.text(2),
            cookie: fields.text(3),
            target_packages: fields
                .repeated_bytes(4)
                .into_iter()
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .collect(),
            session_id: fields.uint(5),
            package: fields.text(6),
        })
    }
}

/// `duo.screen.RelayOffice` (Mirror message type 23).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RelayOffice {
    pub office_type: u32,
    pub download_url: String,
    pub download_path: String,
    pub progress_page: u32,
    pub progress_range: u32,
    pub progress_string: String,
    pub download_size: u64,
    pub session_id: u64,
    pub package: String,
    pub use_adv: u32,
}

impl RelayOffice {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            office_type: fields.uint(1) as u32,
            download_url: fields.text(2),
            download_path: fields.text(3),
            progress_page: fields.uint(4) as u32,
            progress_range: fields.uint(5) as u32,
            progress_string: fields.text(6),
            download_size: fields.uint(7),
            session_id: fields.uint(8),
            package: fields.text(9),
            use_adv: fields.uint(10) as u32,
        })
    }
}

/// `duo.screen.RelayAppIntent` (Mirror message type 30).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RelayAppIntent {
    pub target_packages: Vec<String>,
    pub signature: String,
    pub uri: String,
    pub session_id: u64,
    pub package: String,
}

impl RelayAppIntent {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let fields = Fields::decode(payload)?;
        Ok(Self {
            target_packages: fields
                .repeated_bytes(1)
                .into_iter()
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .collect(),
            signature: fields.text(2),
            uri: fields.text(3),
            session_id: fields.uint(4),
            package: fields.text(5),
        })
    }
}

/// `duo.screen.ProtoSimpleEvent` acknowledgement for relayed app data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelayDataAck {
    pub session_id: u64,
    pub result: i32,
}

impl RelayDataAck {
    pub(crate) fn encode(&self) -> Vec<u8> {
        frame(
            TYPE_SIMPLE_EVENT,
            &[
                optional(1, self.session_id),
                field(2, 15),
                field(4, self.result as u32 as u64),
            ]
            .concat(),
        )
    }
}

fn field(number: u32, value: u64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(number, value);
    writer.finish()
}

fn optional(number: u32, value: u64) -> Vec<u8> {
    if value == 0 {
        Vec::new()
    } else {
        field(number, value)
    }
}

fn bytes(number: u32, value: impl AsRef<[u8]>) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.bytes(number, value);
    writer.finish()
}

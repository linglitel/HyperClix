use std::sync::{Arc, Mutex, atomic::Ordering};

use anyhow::bail;
use serde::{Deserialize, Serialize};

use super::screen::{Screen, ScreenEvent, ScreenRegistry};
use crate::media::MediaPlayer;
use crate::{
    Result,
    protocol::messages::mirror::{
        AudioFocus as WireAudioFocus, AudioFocusSync, TYPE_AUDIO_FOCUS, TYPE_VOLUME_BASIS,
        TYPE_VOLUME_INDEX, VolumeBasis, VolumeIndex, VolumeSet,
    },
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PhoneVolume {
    pub basis_points: Option<u32>,
    pub device_index: Option<i32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RawVolumeRange {
    pub min: i32,
    pub max: i32,
}

impl Default for RawVolumeRange {
    fn default() -> Self {
        // This phone reports STREAM_MUSIC 0..150; this is not a wire constant.
        Self { min: 0, max: 150 }
    }
}

impl RawVolumeRange {
    fn normalize(self, index: i32) -> u32 {
        ((i64::from(index.clamp(self.min, self.max)) - i64::from(self.min)) * 10000
            / (i64::from(self.max) - i64::from(self.min))) as u32
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioFocus {
    pub screen_id: u32,
    pub muted: bool,
    pub channel_type: u32,
    pub focus_gain: u32,
    pub usage: u32,
    pub package_name: String,
    pub client_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenAudioState {
    pub supports_volume: bool,
    pub active: bool,
    pub volume: PhoneVolume,
    pub focus: Option<AudioFocus>,
}

#[derive(Clone, Copy, Debug)]
pub enum VolumeDirection {
    Up,
    Down,
    ToggleMute,
}

#[derive(Default)]
pub(super) struct PhoneAudioState {
    volume: PhoneVolume,
    last_nonzero: Option<u32>,
    active_screen: Option<u32>,
    raw_volume_range: Option<RawVolumeRange>,
}

pub(super) fn receive(
    message_type: u8,
    payload: &[u8],
    screen_id: Option<u32>,
    package: Option<&str>,
    registry: &ScreenRegistry,
    focus_state: &Mutex<Option<AudioFocus>>,
) -> Result<Option<ScreenEvent>> {
    let state = &registry.audio;
    match message_type {
        TYPE_AUDIO_FOCUS => {
            let message = WireAudioFocus::decode(payload)?;
            if message.update_flag != 0 {
                return Ok(None);
            }
            let Some(screen_id) = screen_id else {
                return Ok(None);
            };
            let package_name = message.package_name;
            let matches = if let Some(id) = message.screen_id {
                id == screen_id
            } else {
                match package {
                    Some(name) => name == package_name,
                    None => !registry
                        .active
                        .lock()
                        .unwrap()
                        .contains(&Some(package_name.clone())),
                }
            };
            if !matches {
                return Ok(None);
            }
            let focus = AudioFocus {
                screen_id,
                muted: message.muted,
                channel_type: message.channel_type,
                focus_gain: message.focus_gain,
                usage: message.usage,
                package_name,
                client_id: message.client_id,
            };
            tracing::info!(
                "[screen={screen_id}] phone audio focus: channel={} muted={} gain={}",
                focus.channel_type,
                focus.muted,
                focus.focus_gain
            );
            if focus.channel_type == 0 {
                *focus_state.lock().unwrap() = Some(focus.clone());
            }
            Ok(Some(ScreenEvent::AudioFocusChanged(focus)))
        }
        TYPE_VOLUME_BASIS => {
            let message = VolumeBasis::decode(payload)?;
            if message.kind != 1 {
                return Ok(None);
            }
            let basis_points = message.basis_points;
            let mut state = state.lock().unwrap();
            state.volume.basis_points = Some(basis_points);
            state.volume.device_index = None;
            if basis_points != 0 {
                state.last_nonzero = Some(basis_points);
            }
            tracing::info!("[screen={screen_id:?}] phone media volume: {basis_points}/10000");
            Ok(Some(ScreenEvent::PhoneVolumeChanged(state.volume.clone())))
        }
        TYPE_VOLUME_INDEX => {
            let message = VolumeIndex::decode(payload)?;
            let mut state = state.lock().unwrap();
            let index = message.index;
            state.volume.device_index = Some(index);
            state.volume.basis_points = state.raw_volume_range.map(|range| range.normalize(index));
            if let Some(value) = state.volume.basis_points.filter(|value| *value != 0) {
                state.last_nonzero = Some(value);
            }
            tracing::info!(
                "[screen={screen_id:?}] phone media volume index: {:?}",
                state.volume.device_index
            );
            Ok(Some(ScreenEvent::PhoneVolumeChanged(state.volume.clone())))
        }
        _ => Ok(None),
    }
}

impl Screen {
    pub fn set_raw_volume_range(&self, range: Option<RawVolumeRange>) -> Result<()> {
        if range.is_some_and(|range| range.min >= range.max) {
            bail!("手机原始音量范围必须满足 min < max");
        }
        let mut state = self.inner.registration.registry.audio.lock().unwrap();
        state.raw_volume_range = range;
        if let Some(index) = state.volume.device_index {
            state.volume.basis_points = range.map(|range| range.normalize(index));
        }
        Ok(())
    }

    pub fn audio_state(&self) -> ScreenAudioState {
        let configuration = self.configuration();
        let state = self.inner.registration.registry.audio.lock().unwrap();
        ScreenAudioState {
            supports_volume: configuration.supports_volume,
            active: state.active_screen == Some(configuration.screen_id),
            volume: state.volume.clone(),
            focus: self.inner.audio_focus.lock().unwrap().clone(),
        }
    }

    /// Ask the phone to resend its current media focus for this screen.
    pub async fn sync_audio_focus(&self) -> Result<()> {
        let configuration = self.configuration();
        tracing::info!(
            "[screen={}] resync phone audio focus",
            configuration.screen_id
        );
        self.inner
            .channel
            .send(
                &AudioFocusSync {
                    session_id: configuration.session_id,
                    screen_id: configuration.screen_id,
                }
                .encode(),
            )
            .await
    }

    /// Active desktop window selects key routing; Android owns audio focus.
    pub async fn set_audio_active(&self, active: bool) -> Result<()> {
        let id = self.configuration().screen_id;
        {
            let mut state = self.inner.registration.registry.audio.lock().unwrap();
            if active {
                state.active_screen = Some(id);
            } else if state.active_screen == Some(id) {
                state.active_screen = None;
            }
        }
        tracing::info!("[screen={id}] audio window active={active}");
        if active {
            self.sync_audio_focus().await?;
        }
        Ok(())
    }

    /// Phone media volume is global; percent is 0..=100.
    pub async fn set_volume_percent(&self, percent: u8) -> Result<()> {
        if percent > 100 {
            bail!("音量百分比必须在 0–100 之间");
        }
        self.set_volume_basis_points(u32::from(percent) * 100).await
    }

    pub async fn adjust_volume(&self, direction: VolumeDirection) -> Result<()> {
        let target = {
            let state = self.inner.registration.registry.audio.lock().unwrap();
            let Some(current) = state.volume.basis_points else {
                bail!("尚未收到手机音量同步，请先用音量滑块设置音量");
            };
            match direction {
                VolumeDirection::Up => (current + 500).min(10000),
                VolumeDirection::Down => current.saturating_sub(500),
                VolumeDirection::ToggleMute if current != 0 => 0,
                VolumeDirection::ToggleMute => state
                    .last_nonzero
                    .ok_or_else(|| anyhow::anyhow!("尚无可恢复的手机音量，请用音量滑块设置音量"))?,
            }
        };
        self.set_volume_basis_points(target).await
    }

    async fn set_volume_basis_points(&self, value: u32) -> Result<()> {
        let configuration = self.configuration();
        if !configuration.supports_volume {
            bail!("手机未声明支持投屏音量调整");
        }
        self.inner
            .channel
            .send(
                &VolumeSet {
                    basis_points: value,
                }
                .encode(),
            )
            .await?;
        let volume = {
            let mut state = self.inner.registration.registry.audio.lock().unwrap();
            state.volume.basis_points = Some(value);
            state.volume.device_index = None;
            if value != 0 {
                state.last_nonzero = Some(value);
            }
            state.volume.clone()
        };
        tracing::info!(
            "[screen={}] set phone media volume: {value}/10000",
            configuration.screen_id
        );
        let _ = self
            .inner
            .events
            .send(ScreenEvent::PhoneVolumeChanged(volume));
        Ok(())
    }
}

impl MediaPlayer {
    /// Follow the phone's volume/focus events for this projection's PCM output.
    /// A raw index range is optional: normalized CtrlState values need none.
    pub fn follow_screen_audio(
        &mut self,
        screen: &Screen,
        raw_range: Option<RawVolumeRange>,
    ) -> Result<()> {
        if let Some(task) = self.audio_sync.take() {
            task.abort();
        }
        screen.set_raw_volume_range(raw_range)?;
        let mut events = screen.subscribe();
        let state = screen.audio_state();
        if let Some(gain) = state.volume.basis_points {
            self.set_gain_basis_points(gain);
        }
        if let Some(focus) = state.focus {
            self.control
                .focus_muted
                .store(focus.muted, Ordering::Release);
        }
        let request = screen.clone();
        let control = self.control.clone();
        self.audio_sync = Some(tokio::spawn(async move {
            if let Err(error) = request.sync_audio_focus().await {
                tracing::info!("[audio] focus resync: {error:#}");
            }
            drop(request);
            loop {
                match events.recv().await {
                    Ok(ScreenEvent::PhoneVolumeChanged(volume)) => {
                        if let Some(gain) = volume.basis_points {
                            control
                                .gain_basis_points
                                .store(gain.min(10000), Ordering::Release);
                            tracing::info!(
                                "[media={:p}] projection gain={gain}/10000; desktop mixer unchanged",
                                Arc::as_ptr(&control)
                            );
                        }
                    }
                    Ok(ScreenEvent::AudioFocusChanged(focus)) if focus.channel_type == 0 => {
                        control.focus_muted.store(focus.muted, Ordering::Release);
                    }
                    Ok(ScreenEvent::Closed(_))
                    | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    _ => {}
                }
            }
        }));
        Ok(())
    }
}

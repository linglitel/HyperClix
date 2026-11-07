//! Mirror desktop and independent application screens.

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, bail};
use tokio::sync::{Notify, broadcast, watch};

use super::audio;
pub use super::input::{MouseAction, android_key};
pub use super::types::{AppOptions, ScreenConfiguration, ScreenEvent};
use crate::{
    Result,
    protocol::messages::mirror::{
        FrameDecoder, ScreenAck, ScreenCommand, ScreenUpdate, TYPE_SCREEN,
    },
    session::{Channel, ChannelEvent, Session},
};

#[derive(Default)]
pub(crate) struct ScreenRegistry {
    pub(super) creating: tokio::sync::Mutex<()>,
    pub(super) active: Mutex<BTreeSet<Option<String>>>,
    pub(super) audio: Arc<Mutex<audio::PhoneAudioState>>,
}

pub(super) struct ScreenRegistration {
    pub(super) registry: Arc<ScreenRegistry>,
    pub(super) package: Option<String>,
    released: AtomicBool,
}

impl ScreenRegistration {
    fn acquire(registry: Arc<ScreenRegistry>, package: Option<String>) -> Result<Self> {
        if !registry.active.lock().unwrap().insert(package.clone()) {
            bail!("该应用或桌面投屏已开启，请先关闭现有窗口");
        }
        Ok(Self {
            registry,
            package,
            released: AtomicBool::new(false),
        })
    }

    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.registry.active.lock().unwrap().remove(&self.package);
        }
    }
}

impl Drop for ScreenRegistration {
    fn drop(&mut self) {
        self.release();
    }
}

pub const LEFT_HELD: u32 = 1;
pub const RIGHT_HELD: u32 = 512;
pub const MOUSE_CTRL: u32 = 1024;
pub const MOUSE_SHIFT: u32 = 2048;
pub const MOUSE_META: u32 = 16384;

pub(super) struct ScreenInner {
    session: Session,
    pub(super) channel: Channel,
    pub(super) package: Option<String>,
    configuration: watch::Receiver<ScreenConfiguration>,
    pub(super) events: broadcast::Sender<ScreenEvent>,
    stopped: AtomicBool,
    stop: Arc<Notify>,
    pub(super) registration: ScreenRegistration,
    pub(super) audio_focus: Arc<Mutex<Option<audio::AudioFocus>>>,
}

impl Drop for ScreenInner {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

#[derive(Clone)]
pub struct Screen {
    pub(super) inner: Arc<ScreenInner>,
}

impl Screen {
    pub async fn open_desktop(session: &Session) -> Result<Self> {
        Self::open(session, None, AppOptions::default()).await
    }

    pub async fn open_app(session: &Session, package: &str, options: AppOptions) -> Result<Self> {
        if package.is_empty() {
            bail!("App 包名不能为空");
        }
        if options.width == 0 || options.height == 0 || options.density == 0 {
            bail!("App 尺寸和 density 必须大于 0");
        }
        Self::open(session, Some(package.to_owned()), options).await
    }

    async fn open(session: &Session, package: Option<String>, options: AppOptions) -> Result<Self> {
        let registry = session.extension::<ScreenRegistry>();
        let _creating = registry.creating.lock().await;
        let registration = ScreenRegistration::acquire(registry.clone(), package.clone())?;
        let channel = session.open_cast().await?;
        let mut incoming = channel.subscribe();
        let requested_package = package.clone();
        let initial = ScreenConfiguration {
            phone_ip: session.peer_ip(),
            screen_id: 0,
            session_id: 0,
            port: 0,
            width: 0,
            height: 0,
            real_width: 0,
            real_height: 0,
            landscape: false,
            app_name: String::new(),
            configuration: 0,
            support_max: 0,
            supports_volume: false,
            volume_value: 0,
        };
        let (config_tx, config_rx) = watch::channel(initial);
        let (events, _) = broadcast::channel(128);
        let audio_registry = registry.clone();
        let audio_focus = Arc::new(Mutex::new(None));
        let screen = Self {
            inner: Arc::new(ScreenInner {
                session: session.clone(),
                channel: channel.clone(),
                package,
                configuration: config_rx.clone(),
                events: events.clone(),
                stopped: AtomicBool::new(false),
                stop: Arc::new(Notify::new()),
                registration,
                audio_focus: audio_focus.clone(),
            }),
        };
        let stop = screen.inner.stop.clone();
        tokio::spawn(async move {
            let mut decoder = FrameDecoder::default();
            let mut assigned = requested_package.is_none().then_some(0);
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    message = incoming.recv() => {
                        let bytes = match message {
                            Ok(ChannelEvent::Data(bytes)) => bytes,
                            Ok(ChannelEvent::Closed(reason)) => {
                                tracing::info!("[screen={assigned:?} cast={}] CAST channel closed", channel.id());
                                let _ = events.send(ScreenEvent::Closed(reason)); break;
                            }
                            Err(error) => { let _ = events.send(ScreenEvent::Error(error.to_string())); break; }
                        };
                        let frames = match decoder.feed(&bytes) {
                            Ok(frames) => frames,
                            Err(error) => { let _ = events.send(ScreenEvent::Error(format!("屏幕报文：{error:#}"))); break; }
                        };
                        for frame in frames {
                            if frame.message_type != TYPE_SCREEN {
                                match audio::receive(
                                    frame.message_type, &frame.payload, assigned,
                                    requested_package.as_deref(), &audio_registry, &audio_focus,
                                ) {
                                    Ok(Some(event)) => { let _ = events.send(event); continue; }
                                    Ok(None) => {}
                                    Err(error) => {
                                        let _ = events.send(ScreenEvent::Error(format!("音量/焦点报文：{error:#}")));
                                        continue;
                                    }
                                }
                                let _ = events.send(ScreenEvent::Message { message_type: frame.message_type, payload: frame.payload });
                                continue;
                            }
                            let update = match ScreenUpdate::decode(&frame.payload) {
                                Ok(update) => update,
                                Err(error) => {
                                    let _ = events.send(ScreenEvent::Error(format!("屏幕配置：{error:#}")));
                                    continue;
                                }
                            };
                            if update.update_flag != 0 { continue; }
                            let screen_id = update.screen_id;
                            let kind = update.configuration;
                            if let Some(id) = assigned {
                                if id != screen_id { continue; }
                            } else {
                                let package_matches = requested_package.as_deref() == Some(update.package.as_str());
                                if kind == 0 && screen_id != 0 && update.port != 0 && package_matches {
                                    assigned = Some(screen_id);
                                    tracing::info!("[screen={screen_id} cast={}] phone assigned App screen", channel.id());
                                } else if !(kind == 7 && (update.package.is_empty() || package_matches)
                                    || kind == 3 && package_matches)
                                {
                                    continue;
                                }
                            }
                            let mut config = config_tx.borrow().clone();
                            if let Some(id) = assigned { config.screen_id = id; }
                            config.configuration = update.configuration;
                            config.support_max = update.support_max;
                            if kind == 0 || update.has_supports_volume {
                                config.supports_volume = update.supports_volume;
                            }
                            if kind == 0 || update.has_volume {
                                config.volume_value = update.volume;
                            }
                            if update.session_id != 0 { config.session_id = update.session_id; }
                            if update.port != 0 { config.port = update.port; }
                            if update.width != 0 { config.width = update.width; }
                            if update.height != 0 { config.height = update.height; }
                            if update.real_width != 0 { config.real_width = update.real_width; }
                            if update.real_height != 0 { config.real_height = update.real_height; }
                            config.landscape = update.landscape;
                            if !update.app_name.is_empty() { config.app_name = update.app_name; }
                            tracing::info!(
                                "[screen={screen_id} cast={}] configuration={}{} session={} port={} size={}x{} support_max={}",
                                channel.id(), config.configuration,
                                if config.configuration == 3 { " ON_HIDE" } else { "" },
                                config.session_id, config.port, config.width, config.height, config.support_max
                            );
                            if config.configuration == 1 {
                                let ack = ScreenAck {
                                    session_id: config.session_id,
                                    screen_id,
                                }
                                .encode();
                                if let Err(error) = channel.send(&ack).await {
                                    let _ = events.send(ScreenEvent::Error(format!("尺寸 ACK：{error:#}")));
                                }
                            }
                            config_tx.send_replace(config.clone());
                            if config.configuration == 3 {
                                let _ = events.send(ScreenEvent::Closed("手机已结束屏幕".into()));
                            } else {
                                let _ = events.send(ScreenEvent::ConfigurationChanged(config));
                            }
                        }
                    }
                }
            }
        });
        let request = if let Some(package) = &screen.inner.package {
            ScreenCommand::App {
                package: package.clone(),
                width: options.width,
                height: options.height,
                density: options.density,
                screen_id: 0,
                resize: false,
                prefer_phone_ime: options.prefer_phone_ime,
            }
            .encode()
        } else {
            ScreenCommand::Desktop.encode()
        };
        tracing::info!(
            "[screen=pending cast={}] create requested: type={} package={:?} size={}x{} density={}",
            screen.inner.channel.id(),
            if screen.inner.package.is_some() {
                "app"
            } else {
                "desktop"
            },
            screen.inner.package,
            options.width,
            options.height,
            options.density
        );
        if let Err(error) = screen.inner.channel.send(&request).await {
            tracing::info!(
                "[screen=pending cast={}] create send failed: {error:#}",
                screen.inner.channel.id()
            );
            let _ = screen.close().await;
            return Err(error);
        }
        let mut ready = config_rx;
        let result = tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                {
                    let config = ready.borrow();
                    match config.configuration {
                        3 => bail!("手机已结束屏幕"),
                        7 if config.support_max != 0 => {
                            bail!("手机投屏窗口数已达上限（最多 {} 个）", config.support_max)
                        }
                        7 => bail!("手机投屏窗口数已达上限"),
                        _ if config.port != 0 => return Ok(()),
                        _ => {}
                    }
                }
                ready.changed().await.context("屏幕配置通道已关闭")?;
            }
        })
        .await
        .context("等待手机创建屏幕超时")
        .and_then(|r| r);
        if let Err(error) = result {
            tracing::info!(
                "[screen={} cast={}] create failed: {error:#}",
                screen.configuration().screen_id,
                screen.inner.channel.id()
            );
            let _ = screen.close().await;
            return Err(error);
        }
        tracing::info!(
            "[screen={} cast={}] create ready",
            screen.configuration().screen_id,
            screen.inner.channel.id()
        );
        Ok(screen)
    }

    /// Latest configuration from the phone. Its port opens the separate RTSP
    /// media connection; no port or screen/session ID is supplied by the UI.
    pub fn configuration(&self) -> ScreenConfiguration {
        self.inner.configuration.borrow().clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ScreenEvent> {
        self.inner.events.subscribe()
    }
    pub fn channel(&self) -> &Channel {
        &self.inner.channel
    }
    pub(crate) fn session(&self) -> &Session {
        &self.inner.session
    }

    /// Submit canvas geometry. Actual changes arrive as ConfigurationChanged.
    pub async fn close(&self) -> Result<()> {
        if self.inner.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let mut configuration = self.inner.configuration.clone();
        let cfg = configuration.borrow().clone();
        let assigned = cfg.port != 0 && (self.inner.package.is_none() || cfg.screen_id != 0);
        if assigned {
            self.set_audio_active(false).await?;
        }
        let send = if !assigned {
            tracing::info!(
                "[screen=pending cast={}] close: no assigned screen; skipping CLOSE_SCREEN",
                self.inner.channel.id()
            );
            Ok(())
        } else if cfg.configuration == 3 {
            tracing::info!(
                "[screen={} cast={}] close: ON_HIDE already received",
                cfg.screen_id,
                self.inner.channel.id()
            );
            Ok(())
        } else {
            tracing::info!(
                "[screen={} cast={}] CLOSE_SCREEN sending: session={}",
                cfg.screen_id,
                self.inner.channel.id(),
                cfg.session_id
            );
            self.inner
                .channel
                .send(
                    &ScreenCommand::Close {
                        session_id: cfg.session_id,
                        screen_id: cfg.screen_id,
                    }
                    .encode(),
                )
                .await
        };
        if let Err(error) = &send {
            tracing::info!(
                "[screen={} cast={}] CLOSE_SCREEN failed: {error:#}",
                cfg.screen_id,
                self.inner.channel.id()
            );
        }
        // CLOSE_SCREEN completes asynchronously on the phone. Keep the CAST
        // lease and reader alive for its matching ON_HIDE before releasing the
        // last SDK owner or allowing the same App to be opened again.
        let hidden = if send.is_ok() && assigned {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if configuration.borrow().configuration == 3 {
                        return Ok(());
                    }
                    configuration
                        .changed()
                        .await
                        .context("等待手机关闭屏幕时配置通道已断开")?;
                }
            })
            .await
            .context("等待手机关闭屏幕超时")
            .and_then(|result| result)
        } else {
            Ok(())
        };
        if send.is_ok() && assigned {
            match &hidden {
                Ok(()) => tracing::info!(
                    "[screen={} cast={}] close confirmed: ON_HIDE",
                    cfg.screen_id,
                    self.inner.channel.id()
                ),
                Err(error) => tracing::info!(
                    "[screen={} cast={}] close confirmation failed: {error:#}",
                    cfg.screen_id,
                    self.inner.channel.id()
                ),
            }
        }
        self.inner.stop.notify_one();
        tracing::info!(
            "[screen={} cast={}] releasing CAST lease",
            cfg.screen_id,
            self.inner.channel.id()
        );
        let close = self.inner.channel.close().await;
        self.inner.registration.release();
        match &close {
            Ok(()) => tracing::info!(
                "[screen={} cast={}] CAST lease released",
                cfg.screen_id,
                self.inner.channel.id()
            ),
            Err(error) => tracing::info!(
                "[screen={} cast={}] CAST lease release failed: {error:#}",
                cfg.screen_id,
                self.inner.channel.id()
            ),
        }
        let _ = self
            .inner
            .events
            .send(ScreenEvent::Closed("屏幕已关闭".into()));
        send.and(hidden).and(close)
    }
}

impl Session {
    pub async fn open_desktop(&self) -> Result<Screen> {
        Screen::open_desktop(self).await
    }
    pub async fn open_app(&self, package: &str, options: AppOptions) -> Result<Screen> {
        Screen::open_app(self, package, options).await
    }
}

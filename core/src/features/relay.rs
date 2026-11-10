//! Phone application relay over the Mirror CAST message channel.
//!
//! The official phone app advertises its foreground application as a
//! `RelayApplication` (Mirror type 22) on `com.xiaomi.mirror:cast`. Browser,
//! Office and generic intent payloads follow as types 32, 23 and 30.  The PC
//! keeps the CAST channel alive for the whole device session, so this feature
//! owns one lease and reads those messages independently from screen windows.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex as StdMutex, RwLock},
    time::Duration,
};

use anyhow::{Context, ensure};
use tokio::{
    sync::{Mutex, broadcast},
    task::JoinHandle,
};

use crate::{
    Result,
    features::desktop::{AppOptions, Screen},
    paths,
    protocol::messages::mirror::{
        FrameDecoder, RelayAppIntent, RelayApplication, RelayBrowser, RelayDataAck, RelayOffice,
        TYPE_RELAY_APP, TYPE_RELAY_APP_INTENT, TYPE_RELAY_BROWSER, TYPE_RELAY_OFFICE,
    },
    session::{Channel, ChannelEvent, Session},
};

const EVENT_CHANNEL_SIZE: usize = 64;
const MAX_OFFICE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayKind {
    /// A normal Android application that can be opened as a Mirror app screen.
    Mirror,
    Browser,
    Office,
    Intent,
}

impl RelayKind {
    fn from_proto(value: u32) -> Self {
        match value {
            1 => Self::Browser,
            2 => Self::Office,
            3 => Self::Intent,
            _ => Self::Mirror,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RelayApp {
    pub package: String,
    pub name: String,
    pub description: String,
    pub icon: Vec<u8>,
    pub kind: RelayKind,
    pub is_relay: bool,
    pub hide_icon: bool,
    pub support_sub_screen: bool,
    pub support_handoff: bool,
    pub support_pin: bool,
    pub unregister: bool,
    pub start_time: i64,
    pub send_time: i64,
}

#[derive(Clone, Debug)]
pub enum RelayContent {
    Browser {
        url: String,
        url_type: u32,
        cookie_len: usize,
        targets: Vec<String>,
        package: String,
        session_id: u64,
    },
    Office {
        office_type: u32,
        url: String,
        path: String,
        size: u64,
        progress_page: u32,
        progress_range: u32,
        progress_string: String,
        package: String,
        session_id: u64,
    },
    Intent {
        uri: String,
        signature_len: usize,
        targets: Vec<String>,
        package: String,
        session_id: u64,
    },
}

impl RelayContent {
    pub fn package(&self) -> &str {
        match self {
            Self::Browser { package, .. }
            | Self::Office { package, .. }
            | Self::Intent { package, .. } => package,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RelayState {
    pub app: Option<RelayApp>,
    pub content: Option<RelayContent>,
    pub channel_id: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum RelayEvent {
    AppChanged(Option<RelayApp>),
    Content(RelayContent),
    Error(String),
    Closed(String),
}

/// What `RelayMonitor::open` did for the current relay state.
pub enum RelayOpened {
    App(Screen),
    External(String),
    File(PathBuf),
}

struct RelayRuntime {
    start: Mutex<()>,
    channel: Mutex<Option<Channel>>,
    reader: StdMutex<Option<JoinHandle<()>>>,
    state: RwLock<RelayState>,
    events: broadcast::Sender<RelayEvent>,
}

impl RelayRuntime {
    fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_SIZE);
        Self {
            start: Mutex::new(()),
            channel: Mutex::new(None),
            reader: StdMutex::new(None),
            state: RwLock::new(RelayState::default()),
            events,
        }
    }

    fn emit(&self, event: RelayEvent) {
        let _ = self.events.send(event);
    }
}

/// One session-scoped relay listener.  It keeps the shared CAST lease alive
/// until `close`; dropping a clone alone does not stop the listener.
#[derive(Clone)]
pub struct RelayMonitor {
    runtime: Arc<RelayRuntime>,
    session: Session,
}

impl RelayMonitor {
    pub fn state(&self) -> RelayState {
        self.runtime.state.read().unwrap().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RelayEvent> {
        self.runtime.events.subscribe()
    }

    /// Open the current application in a standalone Mirror app window.
    pub async fn open_app(&self, options: AppOptions) -> Result<Screen> {
        let app = self.state().app.context("手机当前没有可接力的应用")?;
        ensure!(!app.package.is_empty(), "应用接力的包名为空，无法打开 App");
        tracing::info!(
            "[relay] open app package={} name={} kind={:?}",
            app.package,
            app.name,
            app.kind
        );
        Screen::open_app(&self.session, &app.package, options).await
    }

    /// Open the last relayed payload locally. Browser URLs and Office files are
    /// launched with `xdg-open`; ordinary apps fall back to an app screen.
    pub async fn open(&self, options: AppOptions) -> Result<RelayOpened> {
        let state = self.state();
        if let Some(content) = state.content.clone() {
            match content {
                RelayContent::Browser { url, .. } => {
                    if is_network_url(&url) {
                        open_external(&url)?;
                        tracing::info!("[relay] opened browser url={url}");
                        return Ok(RelayOpened::External(url));
                    }
                    tracing::info!(
                        "[relay] browser payload is not a network URL, fallback to app: {url}"
                    );
                }
                RelayContent::Office {
                    office_type,
                    url,
                    path,
                    size,
                    session_id,
                    ..
                } => {
                    ensure!(!url.is_empty(), "接力 WPS 消息没有下载地址");
                    let file = download_office(&url, &path, office_type, size, session_id).await?;
                    open_external(&file.to_string_lossy())?;
                    tracing::info!("[relay] opened office file={}", file.display());
                    return Ok(RelayOpened::File(file));
                }
                RelayContent::Intent { uri, .. } => {
                    if is_network_url(&uri) {
                        open_external(&uri)?;
                        tracing::info!("[relay] opened intent uri={uri}");
                        return Ok(RelayOpened::External(uri));
                    }
                    tracing::info!(
                        "[relay] intent URI is not supported by xdg-open, fallback to app: {uri}"
                    );
                }
            }
        }
        let screen = self.open_app(options).await?;
        Ok(RelayOpened::App(screen))
    }

    /// Stop the reader and release this feature's CAST lease.
    pub async fn close(&self) {
        if let Some(channel) = self.runtime.channel.lock().await.take() {
            tracing::info!("[relay] closing cast channel id={}", channel.id());
            let _ = channel.close().await;
        }
        if let Some(reader) = self.runtime.reader.lock().unwrap().take() {
            reader.abort();
        }
        let mut state = self.runtime.state.write().unwrap();
        state.app = None;
        state.content = None;
        state.channel_id = None;
    }
}

impl Session {
    /// Start (or reuse) the session scoped app-relay listener.
    pub async fn relay_monitor(&self) -> Result<RelayMonitor> {
        let runtime = self.extension::<RelayRuntime>();
        let _start = runtime.start.lock().await;
        if runtime
            .channel
            .lock()
            .await
            .as_ref()
            .is_some_and(|channel| !channel.is_closed())
        {
            return Ok(RelayMonitor {
                runtime: runtime.clone(),
                session: self.clone(),
            });
        }
        let channel = self.open_cast().await?;
        let incoming = channel.subscribe();
        {
            let mut state = runtime.state.write().unwrap();
            state.channel_id = Some(channel.id());
            state.app = None;
            state.content = None;
        }
        *runtime.channel.lock().await = Some(channel.clone());
        tracing::info!(
            "[relay] listening cast channel id={} service={}",
            channel.id(),
            channel.service()
        );
        let reader = tokio::spawn(read_loop(runtime.clone(), channel, incoming));
        *runtime.reader.lock().unwrap() = Some(reader);
        Ok(RelayMonitor {
            runtime: runtime.clone(),
            session: self.clone(),
        })
    }
}

async fn read_loop(
    runtime: Arc<RelayRuntime>,
    channel: Channel,
    mut incoming: broadcast::Receiver<ChannelEvent>,
) {
    let mut decoder = FrameDecoder::default();
    loop {
        let bytes = match incoming.recv().await {
            Ok(ChannelEvent::Data(bytes)) => bytes,
            Ok(ChannelEvent::Closed(reason)) => {
                tracing::info!("[relay] cast channel id={} closed: {reason}", channel.id());
                runtime.emit(RelayEvent::Closed(reason));
                break;
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                tracing::warn!(
                    "[relay] cast channel id={} lagged {count} events",
                    channel.id()
                );
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!(
                    "[relay] cast channel id={} event stream ended",
                    channel.id()
                );
                runtime.emit(RelayEvent::Closed("CAST 事件流已结束".into()));
                break;
            }
        };
        let frames = match decoder.feed(&bytes) {
            Ok(frames) => frames,
            Err(error) => {
                tracing::warn!(
                    "[relay] cast channel id={} framing failed: {error:#}",
                    channel.id()
                );
                runtime.emit(RelayEvent::Error(format!("接力报文分帧失败：{error:#}")));
                break;
            }
        };
        for frame in frames {
            match frame.message_type {
                TYPE_RELAY_APP => match RelayApplication::decode(&frame.payload) {
                    Ok(application) => {
                        tracing::info!(
                            "[relay] recv RelayApplication bytes={} package={} name={} kind={} is_relay={} hide_icon={} no_icon={} sub_screen={} handoff={} pin={} unregister={} icon_bytes={} start={} send={}",
                            frame.payload.len(),
                            application.process_name,
                            application.app_name,
                            application.kind,
                            application.is_relay,
                            application.is_hide_icon,
                            application.no_icon,
                            application.support_sub_screen,
                            application.support_handoff,
                            application.support_pin,
                            application.unregister_relay,
                            application.app_icon.len(),
                            application
                                .track_time
                                .as_ref()
                                .map_or(0, |time| time.start_time),
                            application
                                .track_time
                                .as_ref()
                                .map_or(0, |time| time.send_time),
                        );
                        apply_application(&runtime, application);
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[relay] RelayApplication parse failed bytes={} error={error:#}",
                            frame.payload.len()
                        );
                        runtime.emit(RelayEvent::Error(format!(
                            "应用接力消息解析失败：{error:#}"
                        )));
                    }
                },
                TYPE_RELAY_BROWSER => match RelayBrowser::decode(&frame.payload) {
                    Ok(browser) => {
                        tracing::info!(
                            "[relay] recv RelayBrowser bytes={} package={} url_type={} url={} cookie_len={} targets={:?} session={}",
                            frame.payload.len(),
                            browser.package,
                            browser.url_type,
                            browser.url,
                            browser.cookie.len(),
                            browser.target_packages,
                            browser.session_id,
                        );
                        let content = RelayContent::Browser {
                            url: browser.url.clone(),
                            url_type: browser.url_type,
                            cookie_len: browser.cookie.len(),
                            targets: browser.target_packages.clone(),
                            package: browser.package.clone(),
                            session_id: browser.session_id,
                        };
                        apply_content(&runtime, content, browser.session_id, &channel).await;
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[relay] RelayBrowser parse failed bytes={} error={error:#}",
                            frame.payload.len()
                        );
                        runtime.emit(RelayEvent::Error(format!(
                            "浏览器接力消息解析失败：{error:#}"
                        )));
                    }
                },
                TYPE_RELAY_OFFICE => match RelayOffice::decode(&frame.payload) {
                    Ok(office) => {
                        tracing::info!(
                            "[relay] recv RelayOffice bytes={} package={} office_type={} url={} path={} size={} progress_page={} progress_range={} progress={} session={} use_adv={}",
                            frame.payload.len(),
                            office.package,
                            office.office_type,
                            office.download_url,
                            office.download_path,
                            office.download_size,
                            office.progress_page,
                            office.progress_range,
                            office.progress_string,
                            office.session_id,
                            office.use_adv,
                        );
                        let content = RelayContent::Office {
                            office_type: office.office_type,
                            url: office.download_url.clone(),
                            path: office.download_path.clone(),
                            size: office.download_size,
                            progress_page: office.progress_page,
                            progress_range: office.progress_range,
                            progress_string: office.progress_string.clone(),
                            package: office.package.clone(),
                            session_id: office.session_id,
                        };
                        apply_content(&runtime, content, office.session_id, &channel).await;
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[relay] RelayOffice parse failed bytes={} error={error:#}",
                            frame.payload.len()
                        );
                        runtime.emit(RelayEvent::Error(format!(
                            "WPS 接力消息解析失败：{error:#}"
                        )));
                    }
                },
                TYPE_RELAY_APP_INTENT => match RelayAppIntent::decode(&frame.payload) {
                    Ok(intent) => {
                        tracing::info!(
                            "[relay] recv RelayAppIntent bytes={} package={} uri={} targets={:?} signature_len={} session={}",
                            frame.payload.len(),
                            intent.package,
                            intent.uri,
                            intent.target_packages,
                            intent.signature.len(),
                            intent.session_id,
                        );
                        let content = RelayContent::Intent {
                            uri: intent.uri.clone(),
                            signature_len: intent.signature.len(),
                            targets: intent.target_packages.clone(),
                            package: intent.package.clone(),
                            session_id: intent.session_id,
                        };
                        apply_content(&runtime, content, intent.session_id, &channel).await;
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[relay] RelayAppIntent parse failed bytes={} error={error:#}",
                            frame.payload.len()
                        );
                        runtime.emit(RelayEvent::Error(format!(
                            "应用 Intent 接力消息解析失败：{error:#}"
                        )));
                    }
                },
                other => tracing::debug!(
                    "[relay] ignore cast message type={} bytes={}",
                    other,
                    frame.payload.len()
                ),
            }
        }
    }
    let mut state = runtime.state.write().unwrap();
    state.channel_id = None;
}

fn apply_application(runtime: &RelayRuntime, application: RelayApplication) {
    let app = RelayApp {
        package: application.process_name.clone(),
        name: application.app_name.clone(),
        description: application.description.clone(),
        icon: application.app_icon,
        kind: RelayKind::from_proto(application.kind),
        is_relay: application.is_relay,
        hide_icon: application.is_hide_icon,
        support_sub_screen: application.support_sub_screen,
        support_handoff: application.support_handoff,
        support_pin: application.support_pin,
        unregister: application.unregister_relay,
        start_time: application
            .track_time
            .as_ref()
            .map_or(0, |time| time.start_time),
        send_time: application
            .track_time
            .as_ref()
            .map_or(0, |time| time.send_time),
    };
    let mut state = runtime.state.write().unwrap();
    let same_current = state
        .app
        .as_ref()
        .is_some_and(|current| current.package == app.package);
    if app.package == "com.miui.home" {
        tracing::info!("[relay] app cleared reason=home");
        state.app = None;
        state.content = None;
        runtime.emit(RelayEvent::AppChanged(None));
        return;
    }
    if app.unregister || app.hide_icon {
        if same_current {
            tracing::info!(
                "[relay] app cleared package={} reason={}",
                app.package,
                if app.unregister {
                    "unregister"
                } else {
                    "hide_icon"
                }
            );
            state.app = None;
            state.content = None;
            runtime.emit(RelayEvent::AppChanged(None));
        } else {
            tracing::info!(
                "[relay] ignore clear for package={} reason={} current={:?}",
                app.package,
                if app.unregister {
                    "unregister"
                } else {
                    "hide_icon"
                },
                state.app.as_ref().map(|current| current.package.as_str())
            );
        }
        return;
    }
    if app.package.is_empty() {
        tracing::warn!("[relay] ignore RelayApplication without package");
        return;
    }
    if app.name.is_empty() && app.icon.is_empty() && !app.is_relay && !app.support_sub_screen {
        tracing::info!(
            "[relay] ignore app without name/icon package={} is_relay={} sub_screen={}",
            app.package,
            app.is_relay,
            app.support_sub_screen
        );
        return;
    }
    let changed = state
        .app
        .as_ref()
        .is_none_or(|previous| previous.package != app.package || previous.name != app.name);
    if state
        .app
        .as_ref()
        .is_some_and(|previous| previous.package != app.package)
    {
        state.content = None;
    }
    state.app = Some(app.clone());
    tracing::info!(
        "[relay] current app package={} name={} kind={:?} is_relay={} sub_screen={} handoff={} changed={changed}",
        app.package,
        app.name,
        app.kind,
        app.is_relay,
        app.support_sub_screen,
        app.support_handoff,
    );
    runtime.emit(RelayEvent::AppChanged(Some(app)));
}

async fn apply_content(
    runtime: &Arc<RelayRuntime>,
    content: RelayContent,
    session_id: u64,
    channel: &Channel,
) {
    {
        let mut state = runtime.state.write().unwrap();
        if let Some(current) = state.app.as_ref() {
            if !content.package().is_empty() && content.package() != current.package {
                tracing::info!(
                    "[relay] ignore content package={} current={}",
                    content.package(),
                    current.package
                );
                return;
            }
        }
        state.content = Some(content.clone());
    }
    tracing::info!(
        "[relay] content stored session={} type={}",
        session_id,
        match &content {
            RelayContent::Browser { .. } => "browser",
            RelayContent::Office { .. } => "office",
            RelayContent::Intent { .. } => "intent",
        }
    );
    runtime.emit(RelayEvent::Content(content));
    if session_id == 0 {
        return;
    }
    let ack = RelayDataAck {
        session_id,
        result: 1,
    }
    .encode();
    match channel.send(&ack).await {
        Ok(()) => tracing::info!("[relay] ack session={session_id} result=1"),
        Err(error) => {
            tracing::warn!("[relay] ack session={session_id} failed: {error:#}");
            runtime.emit(RelayEvent::Error(format!(
                "应用接力确认发送失败：{error:#}"
            )));
        }
    }
}

fn is_network_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn open_external(target: &str) -> Result<()> {
    ensure!(!target.is_empty(), "打开目标为空");
    let mut command = Command::new("xdg-open");
    command
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .with_context(|| format!("启动 xdg-open 失败：{target}"))?;
    tracing::info!("[relay] xdg-open pid={} target={target}", child.id());
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

async fn download_office(
    url: &str,
    download_path: &str,
    office_type: u32,
    expected_size: u64,
    session_id: u64,
) -> Result<PathBuf> {
    ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "不支持的 WPS 下载地址：{url}"
    );
    let directory = paths::cache_dir().join("relay");
    tokio::fs::create_dir_all(&directory)
        .await
        .with_context(|| format!("创建接力缓存目录失败：{}", directory.display()))?;
    let file_name = office_file_name(download_path, office_type, session_id);
    let target = directory.join(&file_name);
    tracing::info!(
        "[relay] office download start url={url} path={} expected={} bytes",
        target.display(),
        expected_size
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("创建 WPS 下载客户端失败")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("下载 WPS 文件失败：{url}"))?;
    let status = response.status();
    ensure!(status.is_success(), "WPS 下载返回 HTTP {status}");
    if let Some(length) = response.content_length() {
        ensure!(length <= MAX_OFFICE_BYTES, "WPS 文件过大：{length} 字节");
    }
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("读取 WPS 文件失败：{url}"))?;
    ensure!(
        bytes.len() as u64 <= MAX_OFFICE_BYTES,
        "WPS 文件过大：{} 字节",
        bytes.len()
    );
    tokio::fs::write(&target, &bytes)
        .await
        .with_context(|| format!("写入 WPS 文件失败：{}", target.display()))?;
    tracing::info!(
        "[relay] office download complete path={} bytes={} session={session_id}",
        target.display(),
        bytes.len()
    );
    Ok(target)
}

fn office_file_name(download_path: &str, office_type: u32, session_id: u64) -> String {
    let extension = match office_type {
        0 => "doc",
        1 => "xls",
        2 => "ppt",
        3 => "pdf",
        4 => "txt",
        _ => "bin",
    };
    let base = Path::new(download_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let sanitized: String = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        format!("relay-{session_id}.{extension}")
    } else if Path::new(&sanitized).extension().is_none() {
        format!("{sanitized}.{extension}")
    } else {
        sanitized
    }
}

impl Default for RelayRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RelayOpened {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::App(screen) => formatter
                .debug_tuple("App")
                .field(&screen.configuration().screen_id)
                .finish(),
            Self::External(value) => formatter.debug_tuple("External").field(value).finish(),
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
        }
    }
}

impl RelayApp {
    /// True when the official client would offer this entry in the handoff
    /// taskbar: normal relay apps and sub-screen apps are eligible.
    pub fn is_openable(&self) -> bool {
        self.is_relay || self.support_sub_screen || self.support_handoff
    }
}

//! UniversalClipboard metadata on MessageCenter topic 2, content on its SDK channel.
mod fetch;
mod mime;
mod serve;

use fetch::{fetch, inline_content};
use serve::serve;

use crate::{
    Result,
    common::time::epoch_ms,
    protocol::messages::clipboard::TopicAdvertisement,
    session::{Channel, SdkPlatform, Session, SessionEvent},
};
use anyhow::{Context, ensure};
use std::{
    collections::BTreeMap,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, watch},
    task::{JoinHandle, JoinSet},
};

pub const PHONE_SERVICE: &str = crate::protocol::service::CLIPBOARD_PHONE;
pub const LOCAL_SERVICE: &str = crate::protocol::service::CLIPBOARD_LOCAL;
const TEXT: &str = "text/universal";
const HTML: &str = "text/universal-html";
const BOTH: &str = "text/universal-both-text-html";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFile {
    pub path: PathBuf,
    pub mime_type: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    pub text: Option<String>,
    pub html: Option<String>,
    pub files: Vec<ClipboardFile>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardState {
    Running,
    Closed(String),
    Failed(String),
}

#[derive(Clone)]
struct LocalData {
    content: ClipboardContent,
    timestamp: u64,
}
impl LocalData {
    fn mimes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.content.text.is_some() || self.content.html.is_some() {
            out.push(mime::code(match (&self.content.text, &self.content.html) {
                (Some(_), Some(_)) => BOTH,
                (_, Some(_)) => HTML,
                _ => TEXT,
            }));
        }
        out.extend(
            self.content
                .files
                .iter()
                .map(|file| mime::code(&file.mime_type)),
        );
        out
    }
    fn file(&self, item: usize) -> Option<&ClipboardFile> {
        let offset = usize::from(self.content.text.is_some() || self.content.html.is_some());
        item.checked_sub(offset)
            .and_then(|n| self.content.files.get(n))
    }
    fn text(&self, item: usize, code: u8) -> Option<&str> {
        if item != 0 {
            return None;
        }
        match mime::name(code) {
            TEXT => self.content.text.as_deref(),
            HTML => self.content.html.as_deref(),
            _ => None,
        }
    }
    fn advertisement(&self, sequence: u8) -> TopicAdvertisement {
        let mimes = self.mimes();
        let advanced = !self.content.files.is_empty()
            || match (&self.content.text, &self.content.html) {
                (_, Some(html)) => html.len() >= 20480,
                (Some(text), None) => text.len() >= 20480,
                _ => false,
            };
        let mut extended = Vec::new();
        if mimes.len() == 1 && mimes[0] == mime::code(TEXT) {
            extended.extend(self.content.text.as_deref().unwrap_or_default().as_bytes());
        } else if mimes.len() == 1 && mimes[0] == mime::code(HTML) {
            extended.extend(self.content.html.as_deref().unwrap_or_default().as_bytes());
        } else if let Some(text) = self.content.html.as_ref().or(self.content.text.as_ref()) {
            // Desktop extension: item index and UTF-8 byte length, little endian.
            extended.extend(0u32.to_le_bytes());
            extended.extend((text.len() as u32).to_le_bytes());
            extended.extend(text.as_bytes());
        }
        // MessageData.extendData is optional. Omit a preview that cannot fit
        // whole instead of cutting UTF-8 or a desktop TLV in the middle. The
        // receiver then requests complete content through this service.
        if extended.len() > 2048 {
            extended.clear();
        }
        TopicAdvertisement {
            sequence,
            advanced,
            mimes,
            extended,
        }
    }
}
#[derive(Clone)]
struct ContextData {
    session: Session,
    cache: PathBuf,
    local: Arc<Mutex<BTreeMap<u8, LocalData>>>,
    channels: Arc<Mutex<Vec<Channel>>>,
    contents: broadcast::Sender<ClipboardContent>,
    generation: Arc<AtomicU64>,
    updates: watch::Sender<()>,
    stop: watch::Sender<bool>,
    service_enabled: Arc<AtomicBool>,
}
struct ClipboardInner {
    context: ContextData,
    sequence: AtomicU8,
    initial: Mutex<Option<broadcast::Receiver<ClipboardContent>>>,
    state: watch::Receiver<ClipboardState>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}
impl Drop for ClipboardInner {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
#[derive(Clone)]
pub struct Clipboard {
    inner: Arc<ClipboardInner>,
}
impl Clipboard {
    pub async fn open(session: &Session) -> Result<Self> {
        Self::open_with_cache(session, std::env::temp_dir().join("hyperclix-clipboard")).await
    }
    pub async fn open_with_cache(session: &Session, cache: impl AsRef<Path>) -> Result<Self> {
        let capability = session.peer_service_data(PHONE_SERVICE).context(
            "手机尚未发布 universalClipboard 服务，请检查手机跨设备剪贴板开关并重新连接",
        )?;
        ensure!(
            capability.first().is_some_and(|value| value & 1 != 0),
            "手机 universalClipboard 能力位未开启"
        );
        let cache = cache.as_ref().to_path_buf();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&cache)?;
        let (contents, initial) = broadcast::channel(32);
        let (state_tx, state) = watch::channel(ClipboardState::Running);
        let (stop, stopped) = watch::channel(false);
        let incoming = session.events();
        session
            .set_service("pcmanager", "universalClipboard", Some(&[1]))
            .await
            .context("启用 UniversalClipboard 服务声明")?;
        let (updates, _) = watch::channel(());
        let context = ContextData {
            session: session.clone(),
            cache,
            local: Arc::new(Mutex::new(BTreeMap::new())),
            channels: Arc::new(Mutex::new(Vec::new())),
            contents,
            generation: Arc::new(AtomicU64::new(0)),
            updates,
            stop: stop.clone(),
            service_enabled: Arc::new(AtomicBool::new(true)),
        };
        let task = tokio::spawn(run(context.clone(), stopped, state_tx, incoming));
        session.report_phase("剪贴板：已启用 universalClipboard，文本/HTML/图片/文件走独立服务");
        Ok(Self {
            inner: Arc::new(ClipboardInner {
                context,
                sequence: AtomicU8::new((epoch_ms() % 127) as u8),
                initial: Mutex::new(Some(initial)),
                state,
                stop,
                task: Mutex::new(Some(task)),
                closed: AtomicBool::new(false),
            }),
        })
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ClipboardContent> {
        self.inner
            .initial
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| self.inner.context.contents.subscribe())
    }
    pub fn state(&self) -> ClipboardState {
        self.inner.state.borrow().clone()
    }
    pub fn closed(&self) -> watch::Receiver<ClipboardState> {
        self.inner.state.clone()
    }
    pub async fn send_text(&self, text: &str) -> Result<()> {
        self.publish(&ClipboardContent {
            text: Some(text.to_owned()),
            ..Default::default()
        })
        .await
    }
    pub async fn publish(&self, content: &ClipboardContent) -> Result<()> {
        ensure!(
            !self.inner.closed.load(Ordering::Acquire),
            "剪贴板同步已经停止"
        );
        ensure!(
            self.state() == ClipboardState::Running,
            "UniversalClipboard 接收已结束"
        );
        // Official SequenceIdGenerator uses (previous + 1) % 127.
        let sequence = self
            .inner
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some((n + 1) % 127))
            .unwrap();
        let local = LocalData {
            content: content.clone(),
            timestamp: epoch_ms(),
        };
        let advertisement = local.advertisement(sequence);
        ensure!(!advertisement.mimes.is_empty(), "剪贴板内容为空");
        ensure!(
            advertisement.mimes.len() <= 7,
            "UniversalClipboard 单次最多支持7个条目（文本/HTML合为一个条目）"
        );
        for file in &content.files {
            ensure!(
                tokio::fs::metadata(&file.path).await?.is_file(),
                "剪贴板文件不是普通文件"
            );
        }
        let frame = TopicAdvertisement::encode(&advertisement)?;
        self.inner
            .context
            .local
            .lock()
            .unwrap()
            .insert(sequence, local);
        self.inner.context.generation.fetch_add(1, Ordering::AcqRel);
        self.inner.context.updates.send_replace(());
        self.inner
            .context
            .session
            .send_networking(1, &frame)
            .await?;
        self.inner.context.session.report_phase(format!(
            "剪贴板：已发布 sequence={sequence}，{}个条目；保留内容供手机拉取",
            advertisement.mimes.len()
        ));
        Ok(())
    }
    pub async fn close(&self) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let withdrawn = withdraw(&self.inner.context).await;
        self.inner.stop.send_replace(true);
        let task = self.inner.task.lock().unwrap().take();
        if let Some(task) = task {
            task.await?;
        }
        withdrawn
    }
}

async fn run(
    context: ContextData,
    mut stopped: watch::Receiver<bool>,
    state: watch::Sender<ClipboardState>,
    mut incoming: broadcast::Receiver<SessionEvent>,
) {
    let mut jobs = JoinSet::new();
    let accept_session = context.session.clone();
    let mut acceptance = Box::pin(accept_session.accept_channel(LOCAL_SERVICE));
    let reason = loop {
        tokio::select! {
            _ = stopped.changed() => break "同步已停止".to_owned(),
            event = incoming.recv() => match event {
                Ok(SessionEvent::Disconnected(reason)) => break reason,
                Ok(SessionEvent::Networking { module: 1, data }) => match TopicAdvertisement::decode(&data) {
                    Ok(items) => for metadata in items {
                        context.session.report_phase(format!("剪贴板：手机元数据 sequence={} items={} extend={} advanced={}", metadata.sequence, metadata.mimes.len(), metadata.extended.len(), metadata.advanced));
                        let generation = context.generation.fetch_add(1, Ordering::AcqRel) + 1;
                        context.updates.send_replace(());
                        if let Some(content) = inline_content(&metadata) {
                            let _ = context.contents.send(content);
                            continue;
                        }
                        let next = context.clone(); let stop = stopped.clone(); let updates = context.updates.subscribe();
                        jobs.spawn(async move {
                            let content = fetch(&next, &metadata, stop.clone(), generation, updates).await?;
                            if !*stop.borrow() && next.generation.load(Ordering::Acquire) == generation {
                                if let Some(content) = content { let _ = next.contents.send(content); }
                            }
                            Ok::<(), anyhow::Error>(())
                        });
                    },
                    Err(error) => context.session.report_phase(format!("剪贴板元数据解析：{error:#}")),
                },
                Ok(_) => {}, Err(error) => break format!("UniversalClipboard Networking 接收结束：{error}"),
            },
            channel = &mut acceptance => {
                match channel {
                    Ok(channel) => {
                        context.channels.lock().unwrap().push(channel.clone());
                        let next = context.clone();
                        let stop = stopped.clone();
                        jobs.spawn(async move { serve(&next, channel, stop).await });
                    },
                    Err(error) => break format!("UniversalClipboard 内容接收结束：{error:#}"),
                }
                acceptance = Box::pin(accept_session.accept_channel(LOCAL_SERVICE));
            },
            result = jobs.join_next(), if !jobs.is_empty() => if let Some(result) = result {
                if let Err(error) = result.context("剪贴板内容任务中断").and_then(|v| v) { context.session.report_phase(format!("剪贴板内容处理：{error:#}")); }
            },
        }
    };
    if let Err(error) = withdraw(&context).await {
        context
            .session
            .report_phase(format!("撤销 UniversalClipboard 服务：{error:#}"));
    }
    context.stop.send_replace(true);
    let channels = std::mem::take(&mut *context.channels.lock().unwrap());
    for channel in channels {
        let _ = channel.close().await;
    }
    while jobs.join_next().await.is_some() {}
    state.send_replace(ClipboardState::Closed(reason));
}
async fn withdraw(context: &ContextData) -> Result<()> {
    if !context.service_enabled.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    context
        .session
        .set_service("pcmanager", "universalClipboard", None)
        .await
        .context("撤销 UniversalClipboard 服务声明")
}
async fn with_channel(
    context: &ContextData,
    stopped: &mut watch::Receiver<bool>,
    updates: &mut watch::Receiver<()>,
) -> Result<Option<Channel>> {
    if *stopped.borrow() {
        return Ok(None);
    }
    context.session.report_phase("剪贴板：打开手机内容服务");
    let channel = tokio::select! {
        _ = stopped.changed() => return Ok(None),
        _ = updates.changed() => return Ok(None),
        result = context.session.open_channel(PHONE_SERVICE, crate::session::TRUST_PAIR, SdkPlatform::Linux) => result?,
    };
    if *stopped.borrow() {
        let _ = channel.close().await;
        return Ok(None);
    }
    context.channels.lock().unwrap().push(channel.clone());
    Ok(Some(channel))
}
async fn release(context: &ContextData, channel: &Channel) {
    let _ = channel.close().await;
    context
        .channels
        .lock()
        .unwrap()
        .retain(|value| value.id() != channel.id());
}

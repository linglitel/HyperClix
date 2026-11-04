//! Normal-account MiShare negotiation and MiExpress file streams.
//! File metadata uses SHA-256; transfer keys and TCP lanes come from MiExpress.

mod express;
mod lane;
mod receive;
mod send;

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    sync::{broadcast, watch},
};
use uuid::Uuid;

use crate::{
    Result,
    protocol::{
        messages::mishare::{
            ACCEPT, CANCEL, COMPLETE_ACK, Control as WireControl, ERROR_ACK, FINISH,
            FileInfo as WireFileInfo, OFFER, PEER_CANCEL, PEER_ERROR, cancel_body, request_id_body,
        },
        pb::fields,
    },
    session::{ChannelEvent, SdkPlatform, Session},
};

pub const BASIC_SERVICE: &str = crate::protocol::service::MISHARE_BASIC;
pub const TRANSFER_SERVICE: &str = crate::protocol::service::MISHARE_TRANSFER;
pub const PC_BASIC_SERVICE: &str = crate::protocol::service::PC_MISHARE_BASIC;
pub const PC_TRANSFER_SERVICE: &str = crate::protocol::service::PC_MISHARE_TRANSFER;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub id: String,
    pub hash: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferredFile {
    pub info: FileInfo,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferSummary {
    pub files: Vec<TransferredFile>,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub enum TransferEvent {
    Preparing,
    /// The receiver is online and waiting for a phone to send a file task.
    WaitingForIncoming,
    /// An outgoing offer has been sent and the phone must accept it.
    WaitingForPhone,
    Offer(Vec<FileInfo>),
    Progress {
        filename: String,
        transferred: u64,
        total: u64,
        file_index: usize,
        file_count: usize,
    },
    FileComplete(TransferredFile),
    Complete(TransferSummary),
    Cancelled,
    Failed(String),
}

#[derive(Clone)]
pub struct TransferTask {
    events: broadcast::Sender<TransferEvent>,
    state: watch::Receiver<TransferEvent>,
    cancel: watch::Sender<bool>,
}

impl TransferTask {
    pub fn subscribe(&self) -> broadcast::Receiver<TransferEvent> {
        self.events.subscribe()
    }
    pub fn state(&self) -> TransferEvent {
        self.state.borrow().clone()
    }
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }
    pub async fn wait(&self) -> Result<TransferSummary> {
        let mut state = self.state.clone();
        loop {
            match state.borrow().clone() {
                TransferEvent::Complete(summary) => return Ok(summary),
                TransferEvent::Cancelled => bail!("文件传输已取消"),
                TransferEvent::Failed(error) => bail!("{error}"),
                _ => {}
            }
            state.changed().await.context("文件传输任务已退出")?;
        }
    }
}

#[derive(Clone)]
pub struct MiShare {
    session: Session,
    device_name: String,
}

impl MiShare {
    pub fn new(session: Session, device_name: impl Into<String>) -> Self {
        Self {
            session,
            device_name: device_name.into(),
        }
    }

    pub fn send_files(&self, paths: Vec<PathBuf>) -> TransferTask {
        let this = self.clone();
        Self::spawn(move |context| async move { this.send(paths, &context).await })
    }

    /// Listen for and accept one file task from the connected phone. Files are
    /// placed in a new task subdirectory under `destination`; the result keeps
    /// each actual path and the verified SHA-256.
    pub fn receive_files(&self, destination: PathBuf) -> TransferTask {
        let this = self.clone();
        Self::spawn(move |context| async move { this.receive(destination, &context).await })
    }

    fn spawn<F, Fut>(operation: F) -> TransferTask
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<TransferSummary>> + Send + 'static,
    {
        let (events, _) = broadcast::channel(128);
        let (state_tx, state) = watch::channel(TransferEvent::Preparing);
        let (cancel, cancel_rx) = watch::channel(false);
        let context = TaskContext {
            events: events.clone(),
            state: state_tx,
            cancel: cancel_rx,
        };
        tokio::spawn(async move {
            let result = operation(context.clone()).await;
            let event = match result {
                Ok(summary) => TransferEvent::Complete(summary),
                Err(_) if *context.cancel.borrow() => TransferEvent::Cancelled,
                Err(error) => TransferEvent::Failed(format!("{error:#}")),
            };
            context.emit(event);
        });
        TransferTask {
            events,
            state,
            cancel,
        }
    }
}

#[derive(Clone)]
struct TaskContext {
    events: broadcast::Sender<TransferEvent>,
    state: watch::Sender<TransferEvent>,
    cancel: watch::Receiver<bool>,
}

impl TaskContext {
    fn emit(&self, event: TransferEvent) {
        match &event {
            TransferEvent::WaitingForIncoming => {
                tracing::info!("[mishare receive] receiver online; waiting for incoming file task");
            }
            TransferEvent::Offer(files) => {
                tracing::info!(
                    "[mishare receive] file offer received: files={} bytes={}",
                    files.len(),
                    files.iter().map(|file| file.size).sum::<u64>()
                );
            }
            TransferEvent::FileComplete(file) => {
                tracing::info!("[mishare] file verified: bytes={}", file.info.size);
            }
            TransferEvent::Complete(summary) => {
                tracing::info!(
                    "[mishare] task completed: files={} bytes={}",
                    summary.files.len(),
                    summary.bytes
                );
            }
            TransferEvent::Cancelled => tracing::info!("[mishare] task cancelled"),
            TransferEvent::Failed(error) => tracing::info!("[mishare] task failed: {error}"),
            _ => {}
        }
        self.state.send_replace(event.clone());
        let _ = self.events.send(event);
    }
    fn check_cancel(&self) -> Result<()> {
        ensure!(!*self.cancel.borrow(), "文件任务已取消");
        Ok(())
    }
    async fn cancelled(&self) {
        let mut cancel = self.cancel.clone();
        if *cancel.borrow() {
            return;
        }
        let _ = cancel.changed().await;
    }
}

#[derive(Debug)]
pub(super) struct Control {
    pub tag: u64,
    pub body: Vec<u8>,
}

impl Control {
    fn request_id(&self) -> Option<u64> {
        fields(&self.body)
            .ok()
            .map(|fields| fields.uint(1))
            .filter(|id| *id != 0)
    }
}

pub(super) fn control(tag: u64, body: &[u8]) -> Vec<u8> {
    WireControl {
        tag,
        body: body.to_vec(),
    }
    .encode()
}

pub(super) fn decode_control(data: &[u8]) -> Result<Control> {
    let wire = WireControl::decode(data)?;
    Ok(Control {
        tag: wire.tag,
        body: wire.body,
    })
}

async fn wait_control(
    rx: &mut broadcast::Receiver<ChannelEvent>,
    request_id: u64,
    tags: &[u64],
    context: &TaskContext,
    timeout: Duration,
) -> Result<Control> {
    tokio::time::timeout(timeout, async {
        loop {
            let message = tokio::select! { value = rx.recv() => value?, _ = context.cancelled() => bail!("文件任务已取消") };
            match message {
                ChannelEvent::Data(bytes) => {
                    let control = decode_control(&bytes)?;
                    if tags.contains(&control.tag) && control.request_id() == Some(request_id) { return Ok(control); }
                }
                ChannelEvent::Closed(reason) => bail!("手机关闭文件通道：{reason}"),
            }
        }
    }).await.context("等待手机文件协商回包超时")?
}

impl From<&FileInfo> for WireFileInfo {
    fn from(info: &FileInfo) -> Self {
        Self {
            name: info.name.clone(),
            size: info.size,
            id: info.id.clone(),
            hash: info.hash.clone(),
            mime_type: info.mime_type.clone(),
        }
    }
}

impl From<WireFileInfo> for FileInfo {
    fn from(info: WireFileInfo) -> Self {
        Self {
            name: info.name,
            size: info.size,
            id: info.id,
            hash: info.hash,
            mime_type: info.mime_type,
        }
    }
}

async fn file_info(path: &Path, context: &TaskContext) -> Result<FileInfo> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("读取文件 {}", path.display()))?;
    ensure!(metadata.is_file(), "请选择普通文件：{}", path.display());
    let mut source = tokio::fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        context.check_cancel()?;
        let count = source.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let name = path
        .file_name()
        .context("文件名为空")?
        .to_string_lossy()
        .into_owned();
    let mime_type = match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" | "log" => "text/plain",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "mp4" => "video/mp4",
        "pdf" => "application/pdf",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
    .to_owned();
    Ok(FileInfo {
        name,
        size: metadata.len(),
        id: Uuid::new_v4().to_string(),
        hash: hex::encode(hash.finalize()),
        mime_type,
    })
}

fn request_id() -> u64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        & 0x7fff_ffff)
        .max(1)
}

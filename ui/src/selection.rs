//! Universal Clipboard bridge to Wayland's background, multi-MIME clipboard.
use super::{
    Event,
    native_clipboard::{self, NativeMonitor, SelectionData},
};
use anyhow::{Context, Result, bail};
use gtk::gio::{self, prelude::*};
use hyperclix_core::clipboard::{Clipboard, ClipboardContent, ClipboardFile, ClipboardState};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{sync::watch, task::JoinHandle};

pub(super) struct SelectionMonitor {
    id: u64,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
    status: watch::Receiver<SelectionStatus>,
}
#[derive(Clone, Debug)]
pub(super) enum SelectionStatus {
    Running,
    Stopped(Option<String>),
}
static NEXT_MONITOR: AtomicU64 = AtomicU64::new(1);
impl SelectionMonitor {
    pub async fn start(
        clipboard: Clipboard,
        events: async_channel::Sender<Event>,
        cache: PathBuf,
    ) -> Result<Self> {
        let id = NEXT_MONITOR.fetch_add(1, Ordering::Relaxed);
        let mut native = NativeMonitor::start().await?;
        let (stop, stopped) = watch::channel(false);
        let (status_tx, status) = watch::channel(SelectionStatus::Running);
        let task = tokio::spawn(async move {
            let result = monitor(id, clipboard, &mut native, stopped, &events, &cache).await;
            let cleanup = native.stop().await;
            let result = result.and(cleanup);
            let reason = result
                .as_ref()
                .err()
                .map(|error| format!("跨设备剪贴板：{error:#}"));
            status_tx.send_replace(SelectionStatus::Stopped(reason.clone()));
            let _ = events.send(Event::SelectionStopped(id, reason)).await;
            result
        });
        Ok(Self {
            id,
            stop,
            task: Some(task),
            status,
        })
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn status(&self) -> SelectionStatus {
        self.status.borrow().clone()
    }
    pub async fn stop(mut self) -> Result<()> {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await.context("剪贴板同步任务退出")??;
        }
        Ok(())
    }
}
impl Drop for SelectionMonitor {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
async fn monitor(
    id: u64,
    clipboard: Clipboard,
    native: &mut NativeMonitor,
    mut stop: watch::Receiver<bool>,
    events: &async_channel::Sender<Event>,
    cache: &Path,
) -> Result<()> {
    let mut phone = clipboard.subscribe();
    let mut channel_state = clipboard.closed();
    let mut last_value = None;
    ensure_running(&channel_state.borrow())?;
    loop {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            changed = channel_state.changed() => {
                ensure_running(&channel_state.borrow())?;
                changed.context("跨设备剪贴板接收任务已退出")?;
            }
            data = native.next() => {
                let data = data?.context("Wayland剪贴板监听已结束")?;
                let fingerprint = fingerprint(&data);
                if last_value.as_ref() == Some(&fingerprint) { continue; }
                let content = local_content(&data, cache).await?;
                clipboard.publish(&content).await?;
                last_value = Some(fingerprint);
                let _ = events.send(Event::Log(format!("本机剪贴板已发布到手机 · {}", summary(&content)))).await;
            }
            content = phone.recv() => {
                let content = content.context("手机跨设备剪贴板接收已结束")?;
                let data = remote_selection(&content).await?;
                let fingerprint = fingerprint(&data);
                if last_value.as_ref() != Some(&fingerprint) {
                    last_value = Some(fingerprint);
                    native_clipboard::write(data).await?;
                }
                let _ = events.send(Event::SelectionText(id, content.text.clone().unwrap_or_default())).await;
                let _ = events.send(Event::Log(format!("手机剪贴板已同步到本机 · {}", summary(&content)))).await;
            }
        }
    }
}
fn ensure_running(state: &ClipboardState) -> Result<()> {
    match state {
        ClipboardState::Running => Ok(()),
        ClipboardState::Closed(reason) => bail!("跨设备剪贴板已关闭：{reason}"),
        ClipboardState::Failed(reason) => bail!("跨设备剪贴板失败：{reason}"),
    }
}
fn fingerprint(data: &SelectionData) -> Vec<u8> {
    let mut hash = Sha256::new();
    for (mime, bytes) in &data.0 {
        hash.update((mime.len() as u64).to_be_bytes());
        hash.update(mime.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hash.finalize().to_vec()
}
async fn local_content(data: &SelectionData, cache: &Path) -> Result<ClipboardContent> {
    let mut content = ClipboardContent {
        text: data.text().map(str::to_owned),
        html: data
            .0
            .get("text/html")
            .map(|bytes| String::from_utf8(bytes.clone()))
            .transpose()
            .context("本机HTML不是UTF-8")?,
        files: Vec::new(),
    };
    if let Some(uris) = data
        .0
        .get("text/uri-list")
        .or_else(|| data.0.get("x-special/gnome-copied-files"))
    {
        for uri in std::str::from_utf8(uris)?.lines().map(str::trim) {
            if uri.is_empty() || uri.starts_with('#') || uri == "copy" || uri == "cut" {
                continue;
            }
            if let Some(path) = gio::File::for_uri(uri).path() {
                let (kind, _) = gio::content_type_guess(Some(&path), None);
                let mime = gio::content_type_get_mime_type(&kind)
                    .map(String::from)
                    .unwrap_or_else(|| "application/octet-stream".into());
                content.files.push(ClipboardFile {
                    path,
                    mime_type: mime,
                });
            }
        }
    }
    if content.files.is_empty() {
        if let Some((mime, bytes)) = data.0.iter().find(|(mime, _)| mime.starts_with("image/")) {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::create_dir_all(cache).await?;
            tokio::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o700)).await?;
            let extension = match mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => "bmp",
            };
            let path = cache.join(format!("local-{:x}.{extension}", Sha256::digest(bytes)));
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true).mode(0o600);
            let mut file = options.open(&path).await?;
            use tokio::io::AsyncWriteExt;
            file.write_all(bytes).await?;
            content.files.push(ClipboardFile {
                path,
                mime_type: mime.clone(),
            });
        }
    }
    Ok(content)
}
async fn remote_selection(content: &ClipboardContent) -> Result<SelectionData> {
    let mut data = SelectionData::default();
    if let Some(text) = &content.text {
        data.0
            .insert("text/plain;charset=utf-8".into(), text.as_bytes().to_vec());
    }
    if let Some(html) = &content.html {
        data.0.insert("text/html".into(), html.as_bytes().to_vec());
    }
    if !content.files.is_empty() {
        let uris = content
            .files
            .iter()
            .map(|file| gio::File::for_path(&file.path).uri().to_string())
            .collect::<Vec<_>>();
        data.0.insert(
            "text/uri-list".into(),
            format!("{}\r\n", uris.join("\r\n")).into_bytes(),
        );
        data.0.insert(
            "x-special/gnome-copied-files".into(),
            format!("copy\n{}", uris.join("\n")).into_bytes(),
        );
        if content.files.len() == 1 && content.files[0].mime_type.starts_with("image/") {
            let file = &content.files[0];
            let bytes = tokio::fs::read(&file.path).await?;
            if file.mime_type == "image/universal-bmp" {
                // Windows ReadBitmap writes a complete BMP file. The custom
                // protocol MIME is not a standard Wayland image offer.
                use gtk::gdk_pixbuf::prelude::*;
                let loader = gtk::gdk_pixbuf::PixbufLoader::new();
                loader.write(&bytes).context("解码手机剪贴板BMP图片")?;
                loader.close()?;
                let image = loader.pixbuf().context("剪贴板BMP没有可用像素")?;
                data.0
                    .insert("image/png".into(), image.save_to_bufferv("png", &[])?);
            } else {
                data.0.insert(file.mime_type.clone(), bytes);
            }
        }
    }
    Ok(data)
}
fn summary(content: &ClipboardContent) -> String {
    let mut kinds = Vec::new();
    if content.text.is_some() {
        kinds.push("文本".to_owned());
    }
    if content.html.is_some() {
        kinds.push("HTML".to_owned());
    }
    if !content.files.is_empty() {
        kinds.push(format!("{} 个图片/文件", content.files.len()));
    }
    kinds.join("、")
}

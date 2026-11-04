//! Wayland data-control offers retain all supported MIME representations.
use std::{collections::BTreeMap, io::Read};

use anyhow::{Context, Result};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use wl_clipboard_rs::{
    copy::{MimeSource, MimeType, Options, Source},
    paste::Seat,
    watch::{CancelHandle, ClipboardEvent, ClipboardType, Watcher},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SelectionData(pub BTreeMap<String, Vec<u8>>);

impl SelectionData {
    pub fn text(&self) -> Option<&str> {
        self.0
            .get("text/plain;charset=utf-8")
            .or_else(|| self.0.get("text/plain"))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }
}

pub(super) struct NativeMonitor {
    cancel: CancelHandle,
    events: mpsc::Receiver<SelectionData>,
    task: Option<JoinHandle<Result<()>>>,
}

impl NativeMonitor {
    pub async fn start() -> Result<Self> {
        let (events, receive) = mpsc::channel(8);
        let (ready, started) = oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let mut watcher = match Watcher::new(ClipboardType::Regular, Seat::Unspecified) {
                Ok(watcher) => watcher,
                Err(error) => {
                    let _ = ready.send(Err(anyhow::anyhow!("Wayland data-control：{error}")));
                    return Ok(());
                }
            };
            let cancel = watcher.cancel_handle();
            if ready.send(Ok(cancel)).is_err() {
                return Ok(());
            }
            let mut first = true;
            while let Some(event) = watcher.next_event().context("读取 Wayland 剪贴板事件")?
            {
                // Enabling sync must not publish the selection that predates it.
                if first {
                    first = false;
                    continue;
                }
                let ClipboardEvent::Changed {
                    mime_types,
                    mut offer,
                    ..
                } = event
                else {
                    continue;
                };
                let mut data = SelectionData::default();
                let plain = [
                    "text/plain;charset=utf-8",
                    "text/plain",
                    "UTF8_STRING",
                    "TEXT",
                ]
                .into_iter()
                .find(|mime| mime_types.iter().any(|offered| offered == mime));
                let image = mime_types.iter().find(|mime| mime.starts_with("image/"));
                for mime in mime_types.iter().filter(|mime| {
                    Some(mime.as_str()) == plain
                        || mime.as_str() == "text/html"
                        || mime.as_str() == "text/uri-list"
                        || mime.as_str() == "x-special/gnome-copied-files"
                        || Some(*mime) == image
                }) {
                    let mut bytes = Vec::new();
                    offer.receive(mime)?.read_to_end(&mut bytes)?;
                    let key = if Some(mime.as_str()) == plain {
                        "text/plain;charset=utf-8".to_owned()
                    } else {
                        mime.clone()
                    };
                    data.0.insert(key, bytes);
                }
                if !data.0.is_empty() && events.blocking_send(data).is_err() {
                    break;
                }
            }
            Ok(())
        });
        let cancel = started.await.context("Wayland 剪贴板监听启动中断")??;
        Ok(Self {
            cancel,
            events: receive,
            task: Some(task),
        })
    }

    pub async fn next(&mut self) -> Result<Option<SelectionData>> {
        if let Some(data) = self.events.recv().await {
            return Ok(Some(data));
        }
        if let Some(task) = self.task.take() {
            task.await.context("Wayland 剪贴板监听异常退出")??;
        }
        Ok(None)
    }

    pub async fn stop(mut self) -> Result<()> {
        self.cancel.cancel();
        self.events.close();
        if let Some(task) = self.task.take() {
            task.await.context("停止 Wayland 剪贴板监听")??;
        }
        Ok(())
    }
}

impl Drop for NativeMonitor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(super) async fn write(data: SelectionData) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut sources: Vec<_> = data.0.into_iter().map(|(mime, bytes)| MimeSource {
            source: Source::Bytes(bytes.into_boxed_slice()),
            mime_type: MimeType::Specific(mime),
        }).collect();
        if let Some(source) = sources.iter().find(|source| {
            matches!(&source.mime_type, MimeType::Specific(mime) if mime == "text/plain;charset=utf-8")
        }) {
            if let Source::Bytes(bytes) = &source.source {
                sources.push(MimeSource { source: Source::Bytes(bytes.clone()), mime_type: MimeType::Specific("text/plain".into()) });
            }
        }
        let mut options = Options::new();
        options.omit_additional_text_mime_types(true);
        options.copy_multi(sources).context("写入 Wayland 多格式剪贴板")
    }).await.context("Wayland 剪贴板写入任务退出")?
}

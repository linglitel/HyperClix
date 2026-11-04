//! File drops into an existing phone screen, including the file resource server.

use std::{collections::VecDeque, path::PathBuf, time::Duration};

use anyhow::{Context, bail, ensure};
use tokio::sync::broadcast;

use super::Screen;
use crate::{
    Result,
    features::files::resource::ResourceServer,
    protocol::messages::mirror::{DragMessage, FrameDecoder, ScreenUpdate, TYPE_DRAG, TYPE_SCREEN},
    session::{Channel, ChannelEvent},
};

const START: u64 = 0;
const ENTER: u64 = 1;
const MOVE: u64 = 3;
const DROP: u64 = 4;
const CANCEL: u64 = 5;
const FINISH: u64 = 6;
const ACCEPTABLE: u64 = 8;
const CLEAN: u64 = 9;

#[derive(Clone, Debug)]
pub struct AppDropOptions {
    /// Includes the phone App's file reads and release of drag permissions.
    pub timeout: Duration,
}

impl Default for AppDropOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DropReceipt {
    pub files: usize,
    /// Actual response bytes, including any repeated reads by the target App.
    pub bytes_sent: u64,
}

impl Screen {
    /// Drops files at phone-canvas coordinates. Completes after the phone releases
    /// the drag's file permissions, rather than merely acknowledging DROP.
    pub async fn drop_files(&self, paths: Vec<PathBuf>, x: i32, y: i32) -> Result<DropReceipt> {
        self.drop_files_with_options(paths, x, y, AppDropOptions::default())
            .await
    }

    pub async fn drop_files_with_options(
        &self,
        paths: Vec<PathBuf>,
        x: i32,
        y: i32,
        options: AppDropOptions,
    ) -> Result<DropReceipt> {
        ensure!(!paths.is_empty(), "请选择要拖入的文件");
        let config = self.configuration();
        ensure!(
            config.port != 0 && config.configuration != 3,
            "目标投屏已结束"
        );
        let session = self.session();
        let channel = session.open_cast().await?;
        let server = ResourceServer::start(session, config.screen_id, paths).await?;
        let clip_data = server.clip_data();
        let mut replies = DragReplies {
            incoming: channel.subscribe(),
            decoder: FrameDecoder::default(),
            pending: VecDeque::new(),
            session_id: rand::random::<u64>(),
            screen_id: config.screen_id,
        };
        let guard = DropGuard {
            channel,
            server: Some(server),
            session_id: replies.session_id,
            screen_id: config.screen_id,
            armed: true,
            runtime: tokio::runtime::Handle::current(),
        };
        let result = tokio::time::timeout(options.timeout, async {
            tracing::info!("[app-drop screen={}] drag starting", config.screen_id);
            guard.send(START, x, y, Some(&clip_data)).await?;
            guard.send(ENTER, x, y, Some(&clip_data)).await?;
            guard.send(MOVE, x, y, None).await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let reply = replies.next().await?;
                    match reply.action {
                        ACCEPTABLE if reply.acceptable => return Ok::<_, anyhow::Error>(()),
                        CANCEL | CLEAN => bail!("手机取消了文件拖入"),
                        _ => {}
                    }
                }
            }).await.context("目标 App 未接受文件拖入")??;
            tracing::info!("[app-drop screen={}] target accepted drag; sending DROP", config.screen_id);
            guard.send(DROP, x, y, Some(&clip_data)).await?;
            let mut finished = false;
            loop {
                match replies.next().await?.action {
                    CANCEL => bail!("目标 App 取消了文件拖入"),
                    FINISH => {
                        finished = true;
                        tracing::info!("[app-drop screen={}] phone reported FINISH; waiting for resource permission release", config.screen_id);
                    }
                    CLEAN => {
                        ensure!(finished, "手机释放了拖入资源，但没有确认拖放成功");
                        tracing::info!("[app-drop screen={}] phone released resource permissions: CLEAN", config.screen_id);
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }).await.context("等待 App 拖入文件完成超时").and_then(|result| result);
        guard.finish(result).await
    }
}

struct DragReply {
    action: u64,
    acceptable: bool,
}

struct DragReplies {
    incoming: broadcast::Receiver<ChannelEvent>,
    decoder: FrameDecoder,
    pending: VecDeque<DragReply>,
    session_id: u64,
    screen_id: u32,
}

impl DragReplies {
    async fn next(&mut self) -> Result<DragReply> {
        loop {
            if let Some(reply) = self.pending.pop_front() {
                return Ok(reply);
            }
            let bytes = match self.incoming.recv().await.context("等待手机拖放事件")? {
                ChannelEvent::Data(bytes) => bytes,
                ChannelEvent::Closed(reason) => bail!("拖放通道已关闭：{reason}"),
            };
            for frame in self.decoder.feed(&bytes)? {
                if frame.message_type == TYPE_SCREEN {
                    let config = ScreenUpdate::decode(&frame.payload)?;
                    if config.screen_id == self.screen_id && config.configuration == 3 {
                        bail!("手机已关闭目标投屏");
                    }
                }
                if frame.message_type != TYPE_DRAG {
                    continue;
                }
                let reply = DragMessage::decode(&frame.payload)?;
                // FINISH/CLEAN may omit screen_id; the drag session identifies them.
                if reply.session_id != self.session_id {
                    continue;
                }
                self.pending.push_back(DragReply {
                    action: reply.action,
                    acceptable: reply.action == ACCEPTABLE,
                });
            }
        }
    }
}

struct DropGuard {
    channel: Channel,
    server: Option<ResourceServer>,
    session_id: u64,
    screen_id: u32,
    armed: bool,
    runtime: tokio::runtime::Handle,
}

impl DropGuard {
    async fn send(&self, action: u64, x: i32, y: i32, data: Option<&[u8]>) -> Result<()> {
        self.channel
            .send(
                &DragMessage {
                    session_id: self.session_id,
                    screen_id: self.screen_id,
                    action,
                    x,
                    y,
                    data: data.map(Vec::from),
                }
                .encode(),
            )
            .await
    }

    async fn finish(mut self, result: Result<()>) -> Result<DropReceipt> {
        if result.is_err() {
            tracing::info!(
                "[app-drop screen={}] cancelling drag and resources",
                self.screen_id
            );
            let _ = self.send(CANCEL, 0, 0, None).await;
            let _ = self.send(CLEAN, 0, 0, None).await;
        }
        let resources = self.server.take().unwrap().close(result.is_err()).await;
        let closed = self.channel.close().await;
        self.armed = false;
        result?;
        let (files, completed, bytes_sent) = resources?;
        closed?;
        ensure!(
            completed == files,
            "手机结束了拖放，但仅完整读取 {completed}/{files} 个文件"
        );
        tracing::info!(
            "[app-drop screen={}] completed: files={files} bytes={bytes_sent}",
            self.screen_id
        );
        Ok(DropReceipt { files, bytes_sent })
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if self.armed {
            let channel = self.channel.clone();
            let session_id = self.session_id;
            let screen_id = self.screen_id;
            self.runtime.spawn(async move {
                let cancel = DragMessage {
                    session_id,
                    screen_id,
                    action: CANCEL,
                    x: 0,
                    y: 0,
                    data: None,
                };
                let _ = channel.send(&cancel.encode()).await;
                let clean = DragMessage {
                    action: CLEAN,
                    ..cancel
                };
                let _ = channel.send(&clean.encode()).await;
                let _ = channel.close().await;
            });
        }
    }
}

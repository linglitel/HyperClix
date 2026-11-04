//! UniversalClipboard content pulling.
use std::{collections::BTreeMap, path::Path, sync::atomic::Ordering};

use crate::Result;
use anyhow::{Context, bail, ensure};
use tokio::sync::{broadcast, watch};

use super::{
    BOTH, ClipboardContent, ClipboardFile, ContextData, HTML, TEXT, TIMEOUT, TopicAdvertisement,
    mime, release, with_channel,
};
use crate::{
    features::files::channel as channel_file,
    protocol::messages::clipboard::{
        ClipboardMessage, FileContentRequest, FileInfoItem, FileInfoRequest, TextRequest,
    },
    session::{Channel, ChannelEvent},
};
pub(super) fn inline_content(metadata: &TopicAdvertisement) -> Option<ClipboardContent> {
    // Official single TEXT/HTML advertisements carry the complete UTF-8 value
    // directly when shorter than the 2048-byte preview limit.
    if metadata.advanced
        || metadata.mimes.len() != 1
        || metadata.extended.is_empty()
        || metadata.extended.len() >= 2048
    {
        return None;
    }
    let value = String::from_utf8(metadata.extended.clone()).ok()?;
    match mime::name(metadata.mimes[0]) {
        TEXT => Some(ClipboardContent {
            text: Some(value),
            ..Default::default()
        }),
        HTML => Some(ClipboardContent {
            html: Some(value),
            ..Default::default()
        }),
        _ => None,
    }
}
async fn receive_message(
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    expected: u64,
) -> Result<ClipboardMessage> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match incoming.recv().await? {
                ChannelEvent::Closed(reason) => bail!("剪贴板内容通道已关闭：{reason}"),
                ChannelEvent::Data(data) => {
                    if channel_file::decode(&data)?.is_some() {
                        continue;
                    }
                    let message = ClipboardMessage::decode(&data)?;
                    if message.kind() == expected {
                        return Ok(message);
                    }
                }
            }
        }
    })
    .await
    .context("等待手机剪贴板内容响应超时")?
}
async fn request_text(
    channel: &Channel,
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    sequence: u8,
    item: usize,
    code: u8,
) -> Result<Option<String>> {
    channel
        .send(
            &ClipboardMessage::TextRequest(TextRequest {
                sequence,
                item,
                code,
            })
            .encode(),
        )
        .await?;
    let reply = receive_message(incoming, 2).await.with_context(|| {
        format!("UniversalClipboard 完整文本请求 sequence={sequence} item={item} mime={code}")
    })?;
    let ClipboardMessage::TextResponse(reply) = reply else {
        bail!("手机返回了非文本剪贴板响应");
    };
    ensure!(
        reply.sequence == sequence && reply.item == item,
        "手机剪贴板文本响应与请求不匹配"
    );
    match reply.status.unwrap_or(0) {
        0 => Ok(Some(reply.text.unwrap_or_default())),
        1 => Ok(None),
        value => bail!("手机拒绝剪贴板文本请求：status={value}"),
    }
}
pub(super) async fn fetch(
    context: &ContextData,
    metadata: &TopicAdvertisement,
    mut stopped: watch::Receiver<bool>,
    generation: u64,
    mut updates: watch::Receiver<()>,
) -> Result<Option<ClipboardContent>> {
    if context.generation.load(Ordering::Acquire) != generation {
        return Ok(None);
    }
    context.session.report_phase(format!(
        "剪贴板：准备拉取 sequence={} items={}",
        metadata.sequence,
        metadata.mimes.len()
    ));
    let Some(channel) = with_channel(context, &mut stopped, &mut updates).await? else {
        return Ok(None);
    };
    context.session.report_phase(format!(
        "剪贴板：内容通道已建立 sequence={} channel={}",
        metadata.sequence,
        channel.id()
    ));
    let mut incoming = channel.subscribe();
    if context.generation.load(Ordering::Acquire) != generation {
        release(context, &channel).await;
        return Ok(None);
    }
    let mut job = context.clone();
    job.cache = context.cache.join(uuid::Uuid::new_v4().to_string());
    let result = async {
        tokio::fs::create_dir(&job.cache).await?;
        tokio::select! {
            _ = stopped.changed() => Ok(None),
            _ = updates.changed() => Ok(None),
            result = fetch_contents(&job, &channel, &mut incoming, metadata) => result.map(Some),
        }
    }
    .await;
    release(context, &channel).await;
    if matches!(&result, Ok(None)) {
        context.session.report_phase(format!(
            "剪贴板：已取消旧内容拉取 sequence={}",
            metadata.sequence
        ));
    }
    if !matches!(&result, Ok(Some(content)) if !content.files.is_empty()) {
        let _ = tokio::fs::remove_dir_all(&job.cache).await;
    }
    result
}
async fn fetch_contents(
    context: &ContextData,
    channel: &Channel,
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    metadata: &TopicAdvertisement,
) -> Result<ClipboardContent> {
    let (mut content, files) = fetch_text_contents(channel, incoming, metadata).await?;
    content.files = fetch_files(context, channel, incoming, metadata, files).await?;
    context.session.report_phase(format!(
        "剪贴板：已取得完整内容 sequence={} text={} html={} files={}",
        metadata.sequence,
        content.text.is_some(),
        content.html.is_some(),
        content.files.len()
    ));
    Ok(content)
}

async fn fetch_text_contents(
    channel: &Channel,
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    metadata: &TopicAdvertisement,
) -> Result<(ClipboardContent, Vec<(usize, String)>)> {
    let mut content = ClipboardContent::default();
    let mut files = Vec::new();
    for (index, code) in metadata.mimes.iter().copied().enumerate() {
        match mime::name(code) {
            TEXT => {
                content.text = request_text(
                    channel,
                    incoming,
                    metadata.sequence,
                    index,
                    mime::code(TEXT),
                )
                .await?
            }
            HTML => {
                content.html = request_text(
                    channel,
                    incoming,
                    metadata.sequence,
                    index,
                    mime::code(HTML),
                )
                .await?
            }
            BOTH => {
                content.text = request_text(
                    channel,
                    incoming,
                    metadata.sequence,
                    index,
                    mime::code(TEXT),
                )
                .await?;
                content.html = request_text(
                    channel,
                    incoming,
                    metadata.sequence,
                    index,
                    mime::code(HTML),
                )
                .await?;
            }
            name => files.push((index, name.to_owned())),
        }
    }
    Ok((content, files))
}

async fn fetch_files(
    context: &ContextData,
    channel: &Channel,
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    metadata: &TopicAdvertisement,
    files: Vec<(usize, String)>,
) -> Result<Vec<ClipboardFile>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    channel
        .send(
            &ClipboardMessage::FileInfoRequest(FileInfoRequest {
                sequence: metadata.sequence,
            })
            .encode(),
        )
        .await?;
    let reply = receive_message(incoming, 3)
        .await
        .context("UniversalClipboard 文件信息请求")?;
    let ClipboardMessage::FileInfoResponse(info) = reply else {
        bail!("手机返回了非文件信息剪贴板响应");
    };
    ensure!(
        info.sequence == metadata.sequence,
        "手机剪贴板文件信息序号不匹配"
    );
    ensure!(
        info.status.unwrap_or(0) == 0,
        "手机剪贴板文件已不存在：status={}",
        info.status.unwrap_or(0)
    );
    let info: BTreeMap<usize, FileInfoItem> = info
        .items
        .into_iter()
        .map(|item| (item.index, item))
        .collect();
    let mut out = Vec::new();
    for (item, mime_type) in files {
        let file_info = info.get(&item).context("手机剪贴板缺少对应文件信息")?;
        let basename = Path::new(&file_info.name)
            .file_name()
            .context("手机剪贴板文件名为空")?;
        let directory = context.cache.join(uuid::Uuid::new_v4().to_string());
        tokio::fs::create_dir(&directory).await?;
        let destination = directory.join(basename);
        let mut file_rx = channel.subscribe();
        let mut failed_rx = channel.subscribe();
        channel
            .send(
                &ClipboardMessage::FileContentRequest(FileContentRequest {
                    sequence: metadata.sequence,
                    item,
                })
                .encode(),
            )
            .await?;
        let received = tokio::select! {
            result = channel_file::receive_file(channel, &mut file_rx, &destination, TIMEOUT) => result.with_context(|| format!("UniversalClipboard 文件接收 sequence={} item={item}", metadata.sequence))?,
            failure = receive_message(&mut failed_rx, 5) => {
                let ClipboardMessage::FileFailure(failure) = failure? else {
                    bail!("手机返回了非文件失败剪贴板响应");
                };
                bail!("手机拒绝剪贴板文件内容请求：status={}", failure.status);
            },
        };
        ensure!(
            received.bytes == file_info.size,
            "剪贴板文件长度与手机文件信息不一致"
        );
        out.push(ClipboardFile {
            path: received.path,
            mime_type,
        });
    }
    Ok(out)
}

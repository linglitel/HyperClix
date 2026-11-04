//! UniversalClipboard request serving.
use std::path::Path;

use crate::Result;
use anyhow::Context;
use tokio::sync::watch;

use super::{BOTH, ContextData, TIMEOUT, mime, release};
use crate::{
    features::files::channel as channel_file,
    protocol::messages::clipboard::{
        ClipboardMessage, FileContentRequest, FileFailure, FileInfoItem, FileInfoRequest,
        FileInfoResponse, HistoryItem, HistoryRequest, HistoryResponse, TextRequest, TextResponse,
        Thumbnail,
    },
    session::{Channel, ChannelEvent},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
pub(super) async fn serve(
    context: &ContextData,
    channel: Channel,
    mut stopped: watch::Receiver<bool>,
) -> Result<()> {
    let mut incoming = channel.subscribe();
    let result = async {
        loop {
            let event = tokio::select! {
                _ = stopped.changed() => return Ok(()),
                result = incoming.recv() => result?,
            };
            let data = match event {
                ChannelEvent::Closed(_) => return Ok(()),
                ChannelEvent::Data(data) => data,
            };
            if channel_file::decode(&data)?.is_some() {
                continue;
            }
            match ClipboardMessage::decode(&data) {
                Ok(message) => match message {
                    ClipboardMessage::TextRequest(request) => {
                        serve_text(context, &channel, request).await?;
                    }
                    ClipboardMessage::FileInfoRequest(request) => {
                        serve_file_info(context, &channel, request).await?;
                    }
                    ClipboardMessage::FileContentRequest(request) => {
                        serve_file_content(context, &channel, request).await?;
                    }
                    ClipboardMessage::HistoryRequest(request) => {
                        serve_history(context, &channel, request).await?;
                    }
                    _ => {}
                },
                Err(error) => {
                    context
                        .session
                        .report_phase(format!("剪贴板：手机消息解析失败 {error:#}"));
                }
            }
        }
    }
    .await;
    release(context, &channel).await;
    result
}

fn image_thumbnail(path: &Path) -> Result<String> {
    let image = image::open(path).with_context(|| format!("解码剪贴板图片 {}", path.display()))?;
    let thumbnail = image.thumbnail(160, 160);
    let mut bytes = std::io::Cursor::new(Vec::new());
    thumbnail
        .write_to(&mut bytes, image::ImageFormat::Png)
        .context("编码剪贴板缩略图")?;
    Ok(BASE64.encode(bytes.into_inner()))
}

async fn serve_text(context: &ContextData, channel: &Channel, request: TextRequest) -> Result<()> {
    let local = context
        .local
        .lock()
        .unwrap()
        .get(&request.sequence)
        .cloned();
    context.session.report_phase(format!(
        "剪贴板：手机请求文本 sequence={} item={}",
        request.sequence, request.item
    ));
    let text = local
        .as_ref()
        .and_then(|value| value.text(request.item, request.code));
    let response = TextResponse {
        sequence: request.sequence,
        item: request.item,
        code: request.code,
        status: text.is_none().then_some(1),
        text: text.map(str::to_owned),
    };
    channel
        .send(&ClipboardMessage::TextResponse(response).encode())
        .await?;
    Ok(())
}

async fn serve_file_info(
    context: &ContextData,
    channel: &Channel,
    request: FileInfoRequest,
) -> Result<()> {
    let local = context
        .local
        .lock()
        .unwrap()
        .get(&request.sequence)
        .cloned();
    context.session.report_phase(format!(
        "剪贴板：手机请求文件信息 sequence={}",
        request.sequence
    ));
    let mut items = Vec::new();
    if let Some(local) = local.as_ref() {
        for item in 0..local.mimes().len() {
            if let Some(file) = local.file(item)
                && let Ok(metadata) = tokio::fs::metadata(&file.path).await
            {
                items.push(FileInfoItem {
                    index: item,
                    name: file
                        .path
                        .file_name()
                        .context("剪贴板本地文件名为空")?
                        .to_string_lossy()
                        .into_owned(),
                    size: metadata.len(),
                });
            }
        }
    }
    let response = FileInfoResponse {
        sequence: request.sequence,
        status: items.is_empty().then_some(1),
        items,
    };
    channel
        .send(&ClipboardMessage::FileInfoResponse(response).encode())
        .await?;
    Ok(())
}

async fn serve_file_content(
    context: &ContextData,
    channel: &Channel,
    request: FileContentRequest,
) -> Result<()> {
    let local = context
        .local
        .lock()
        .unwrap()
        .get(&request.sequence)
        .cloned();
    context.session.report_phase(format!(
        "剪贴板：手机请求文件内容 sequence={} item={}",
        request.sequence, request.item
    ));
    if let Some(file) = local.as_ref().and_then(|value| value.file(request.item)) {
        let name = file
            .path
            .file_name()
            .context("剪贴板本地文件名为空")?
            .to_string_lossy()
            .into_owned();
        channel_file::send_file(channel, &file.path, &name, &file.mime_type, "", TIMEOUT).await?;
    } else {
        let response = FileFailure {
            sequence: request.sequence,
            item: request.item,
            status: 1,
        };
        channel
            .send(&ClipboardMessage::FileFailure(response).encode())
            .await?;
    }
    Ok(())
}

async fn serve_history(
    context: &ContextData,
    channel: &Channel,
    request: HistoryRequest,
) -> Result<()> {
    let all = context.local.lock().unwrap().clone();
    let mut items = Vec::new();
    for (sequence, local) in &all {
        if local.timestamp < request.timestamp {
            continue;
        }
        let mut thumbnails = Vec::new();
        for (item, code) in local.mimes().into_iter().enumerate() {
            let mut thumbnail = Thumbnail {
                code,
                name: None,
                size: None,
                text: None,
                complete: false,
            };
            if let Some(file) = local.file(item) {
                if let Some(name) = file.path.file_name() {
                    thumbnail.name = Some(name.to_string_lossy().into_owned());
                }
                if let Ok(metadata) = tokio::fs::metadata(&file.path).await {
                    thumbnail.size = Some(metadata.len());
                }
                if file.mime_type.starts_with("image/") {
                    // The phone renders this base64 PNG in its clipboard
                    // preview before it requests the original file content.
                    thumbnail.text = Some(image_thumbnail(&file.path).with_context(|| {
                        format!("生成剪贴板缩略图 sequence={sequence} item={item}")
                    })?);
                }
            } else if let Some(text) = local.content.html.as_ref().or(local.content.text.as_ref()) {
                thumbnail.size = Some(text.len() as u64);
                let end = text
                    .char_indices()
                    .take_while(|(index, ch)| index + ch.len_utf8() <= 2048)
                    .last()
                    .map(|(index, ch)| index + ch.len_utf8())
                    .unwrap_or(0);
                thumbnail.text = Some(text[..end].to_owned());
                thumbnail.complete = end == text.len() && code != mime::code(BOTH);
            }
            thumbnails.push(thumbnail);
        }
        items.push(HistoryItem {
            sequence: *sequence,
            timestamp: local.timestamp,
            thumbnails,
        });
    }
    let last_sequence = all
        .iter()
        .max_by_key(|(_, value)| value.timestamp)
        .map(|(sequence, _)| *sequence);
    let response = HistoryResponse {
        last_sequence,
        status: items.is_empty().then_some(1),
        items,
    };
    channel
        .send(&ClipboardMessage::HistoryResponse(response).encode())
        .await?;
    Ok(())
}

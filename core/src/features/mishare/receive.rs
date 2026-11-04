//! MiShare inbound file transfer.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::broadcast};
use uuid::Uuid;

use super::express::{Express, ExpressEvent};
use super::{
    ACCEPT, BASIC_SERVICE, CANCEL, COMPLETE_ACK, Control, ERROR_ACK, FINISH, FileInfo, MiShare,
    OFFER, PC_BASIC_SERVICE, PC_TRANSFER_SERVICE, PEER_CANCEL, PEER_ERROR, TRANSFER_SERVICE,
    TaskContext, TransferEvent, TransferSummary, TransferredFile, cancel_body, control,
    decode_control, request_id_body,
};
use crate::{
    Result,
    protocol::{
        messages::mishare::Offer,
        pb::{Message, fields},
    },
    session::{Channel, ChannelEvent, Session},
};

struct IncomingFile {
    info: FileInfo,
    path: PathBuf,
    output: tokio::fs::File,
    hash: Sha256,
    offset: u64,
    eof: Option<u64>,
    pending: BTreeMap<u64, Vec<u8>>,
    length: i64,
    index: usize,
}

impl MiShare {
    pub(super) async fn receive(
        &self,
        destination: PathBuf,
        context: &TaskContext,
    ) -> Result<TransferSummary> {
        context.emit(TransferEvent::WaitingForIncoming);
        let channel = tokio::select! {
            value = self.session.accept_channel(PC_TRANSFER_SERVICE) => value?,
            value = self.session.accept_channel(TRANSFER_SERVICE) => value?,
            value = self.session.accept_channel(PC_BASIC_SERVICE) => value?,
            value = self.session.accept_channel(BASIC_SERVICE) => value?,
            _ = context.cancelled() => bail!("接收已取消"),
        };
        tracing::info!(
            "[mishare receive] claimed incoming SDK channel: service={} channel={}",
            channel.service(),
            channel.id()
        );
        let result = self
            .receive_channel(channel.clone(), destination, context)
            .await;
        let _ = channel.close().await;
        result
    }

    pub(super) async fn receive_channel(
        &self,
        channel: Channel,
        destination: PathBuf,
        context: &TaskContext,
    ) -> Result<TransferSummary> {
        let (basic, offer, advanced) =
            if channel.service() == BASIC_SERVICE || channel.service() == PC_BASIC_SERVICE {
                accept_basic_offer(&self.session, &channel, context).await?
            } else {
                (None, None, channel.clone())
            };
        tracing::info!(
            "[mishare receive] preparing MiExpress receiver: channel={} basic_offer={}",
            advanced.id(),
            offer.is_some()
        );
        let mut link = Express::listen(
            advanced,
            self.session.local_ip(),
            self.session.peer_ip(),
            context.cancel.clone(),
        )
        .await?;
        let mut basic = basic;
        let mut active_request = None;
        let result = async {
            let offered = if let Some(offer) = offer { offer } else {
                tracing::info!("[mishare receive] MiExpress ready; waiting for file offer");
                loop {
                    if let ExpressEvent::Control(offer) = link.next(Duration::from_secs(60)).await? {
                        if offer.tag == 1 { break offer; }
                    }
                }
            };
            let offer_body = Offer::decode(&offered.body)?;
            let request_id = offer_body.request_id;
            active_request = Some(request_id);
            let announced: Vec<FileInfo> =
                offer_body.files.into_iter().map(FileInfo::from).collect();
            ensure!(!announced.is_empty(), "手机发送的不是普通文件任务");
            context.emit(TransferEvent::Offer(announced.clone()));
            let by_id: HashMap<_, _> = announced.iter().enumerate().map(|(n, i)| (i.id.clone(), (n, i.clone()))).collect();
            let destination = destination.join(format!("HyperClix-{}-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(), &Uuid::new_v4().to_string()[..8]));
            tokio::fs::create_dir_all(&destination).await?;
            if basic.is_none() {
                link.send_control(&control(ACCEPT, &request_id_body(request_id)))
                    .await?;
                tracing::info!("[mishare receive] accepted MiExpress offer");
            }
            let total = announced.iter().map(|i| i.size).sum();
            let mut transferred = 0;
            let mut files = HashMap::<i32, IncomingFile>::new();
            let mut received = HashSet::new();
            let mut complete_files = Vec::new();
            loop {
                let event = tokio::select! {
                    event = link.next(Duration::from_secs(90)) => event?,
                    control = async {
                        match &mut basic {
                            Some((_, rx)) => match rx.recv().await? {
                                ChannelEvent::Data(bytes) => Ok::<_, anyhow::Error>(decode_control(&bytes)?),
                                ChannelEvent::Closed(reason) => bail!("手机关闭文件协商通道：{reason}"),
                            },
                            None => std::future::pending().await,
                        }
                    } => ExpressEvent::Control(control?),
                };
                match event {
                    ExpressEvent::StreamSend { stream_id, length, tag } => {
                        let (index, info) = by_id.get(&tag).context("文件流不在已接受清单中")?;
                        ensure!(!received.contains(&tag), "手机重复发送已完成文件");
                        let name = Path::new(&info.name).file_name().context("手机文件名为空")?;
                        let path = destination.join(format!("{}-{}", index + 1, name.to_string_lossy()));
                        let output = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await
                            .with_context(|| format!("创建接收文件 {}", path.display()))?;
                        files.insert(stream_id, IncomingFile { info: info.clone(), path, output,
                            hash: Sha256::new(), offset: 0, eof: None, pending: BTreeMap::new(), length, index: *index });
                        link.begin(stream_id).await?;
                    }
                    ExpressEvent::Data { stream_id, offset, data_length, data } => {
                        let file = files.get_mut(&stream_id).context("文件数据没有对应的已接受文件流")?;
                        ensure!(data_length >= 0 && offset >= 0, "手机回报文件流错误");
                        if data_length == 0 { file.eof = Some(offset as u64); }
                        else if offset as u64 >= file.offset { file.pending.insert(offset as u64, data); }
                        while let Some(piece) = file.pending.remove(&file.offset) {
                            file.output.write_all(&piece).await?;
                            file.hash.update(&piece);
                            file.offset += piece.len() as u64;
                            transferred += piece.len() as u64;
                        }
                        context.emit(TransferEvent::Progress { filename: file.info.name.clone(), transferred, total,
                            file_index: file.index + 1, file_count: announced.len() });
                        if file.eof == Some(file.offset) {
                            let mut file = files.remove(&stream_id).expect("active incoming stream");
                            ensure!(file.offset == file.info.size && (file.length < 0 || file.offset == file.length as u64), "收到文件的大小与清单不一致");
                            let actual_hash = hex::encode(file.hash.finalize());
                            ensure!(file.info.hash.is_empty() || file.info.hash.eq_ignore_ascii_case(&actual_hash), "收到文件的 SHA-256 与清单不一致");
                            file.output.flush().await?;
                            file.info.hash = actual_hash;
                            received.insert(file.info.id.clone());
                            let complete = TransferredFile { info: file.info, path: file.path };
                            context.emit(TransferEvent::FileComplete(complete.clone()));
                            complete_files.push(complete);
                            link.end(stream_id, 1).await?;
                        }
                    }
                    ExpressEvent::Control(message) => {
                        let body = fields(&message.body)?;
                        if body.uint(1) != request_id { continue; }
                        match message.tag {
                            FINISH => {
                                ensure!(received.len() == announced.len(), "手机报告完成时仍有文件未接收完毕");
                                link.send_control(&control(COMPLETE_ACK, &request_id_body(request_id))).await?;
                                return Ok(TransferSummary { files: complete_files, bytes: total });
                            }
                            CANCEL => { link.send_control(&control(PEER_CANCEL, &cancel_body(request_id, body.uint(3)))).await?; bail!("手机取消文件任务"); }
                            PEER_ERROR => { link.send_control(&control(ERROR_ACK, &request_id_body(request_id))).await?; bail!("手机回报文件错误 {}", body.uint(5)); }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }.await;
        if result.is_err() && *context.cancel.borrow() {
            if let Some(request_id) = active_request {
                let _ = link
                    .send_control(&control(CANCEL, &request_id_body(request_id)))
                    .await;
            }
        }
        let _ = link.close().await;
        if let Some((channel, _)) = basic {
            let _ = channel.close().await;
        }
        result
    }
}

async fn accept_basic_offer(
    session: &Session,
    channel: &Channel,
    context: &TaskContext,
) -> Result<(
    Option<(Channel, broadcast::Receiver<ChannelEvent>)>,
    Option<Control>,
    Channel,
)> {
    let mut first_rx = channel.subscribe();
    let event = tokio::select! {
        event = first_rx.recv() => event?,
        _ = context.cancelled() => bail!("接收已取消"),
        _ = tokio::time::sleep(Duration::from_secs(60)) => bail!("等待手机文件清单超时"),
    };
    match event {
        ChannelEvent::Data(bytes) => {
            let offer = decode_control(&bytes)?;
            ensure!(offer.tag == OFFER, "收到非文件清单消息 {}", offer.tag);
            let body = Offer::decode(&offer.body)?;
            channel
                .send(&control(ACCEPT, &request_id_body(body.request_id)))
                .await?;
            tracing::info!(
                "[mishare receive] accepted basic offer; waiting for MiExpress SDK channel"
            );
            let advanced = tokio::select! {
                next = session.accept_channel(PC_TRANSFER_SERVICE) => next?,
                next = session.accept_channel(TRANSFER_SERVICE) => next?,
                _ = context.cancelled() => bail!("接收已取消"),
                _ = tokio::time::sleep(Duration::from_secs(60)) => bail!("等待手机文件数据通道超时"),
            };
            Ok((Some((channel.clone(), first_rx)), Some(offer), advanced))
        }
        ChannelEvent::Closed(reason) => bail!("手机关闭文件通道：{reason}"),
    }
}

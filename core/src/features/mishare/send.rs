//! MiShare outbound file transfer.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, ensure};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::express::{Express, ExpressEvent};
use super::{
    BASIC_SERVICE, CANCEL, FINISH, FileInfo, MiShare, OFFER, SdkPlatform, TRANSFER_SERVICE,
    TaskContext, TransferEvent, TransferSummary, TransferredFile, WireFileInfo, control, file_info,
    request_id, request_id_body, wait_control,
};
use crate::{
    Result,
    protocol::{
        messages::mishare::Offer,
        pb::{Message, fields},
    },
};

impl MiShare {
    pub(super) async fn send(
        &self,
        paths: Vec<PathBuf>,
        context: &TaskContext,
    ) -> Result<TransferSummary> {
        ensure!(!paths.is_empty(), "请选择要发送的文件");
        let mut info = Vec::new();
        for path in &paths {
            context.check_cancel()?;
            info.push(file_info(path, context).await?);
        }
        let request_id = request_id();
        let job_id = Uuid::new_v4().to_string();
        let offer = Offer {
            request_id,
            device_name: self.device_name.clone(),
            job_id: job_id.clone(),
            files: info.iter().map(WireFileInfo::from).collect(),
        }
        .encode();
        tracing::info!(
            "[mishare send] opening basic SDK channel: files={} bytes={}",
            info.len(),
            info.iter().map(|file| file.size).sum::<u64>()
        );
        let basic = self
            .session
            .open_channel(
                BASIC_SERVICE,
                crate::session::TRUST_MISHARE,
                SdkPlatform::Windows,
            )
            .await?;
        tracing::info!("[mishare send] basic SDK ready: channel={}", basic.id());
        let mut basic_rx = basic.subscribe();
        let mut express = None;
        let mut accepted = false;
        let result = async {
            context.emit(TransferEvent::WaitingForPhone);
            basic.send(&control(OFFER, &offer)).await?;
            tracing::info!("[mishare send] offer sent; waiting for phone acceptance");
            let response = wait_control(
                &mut basic_rx,
                request_id,
                &[2, 3, 5],
                context,
                Duration::from_secs(60),
            )
            .await?;
            ensure!(
                response.tag == 2,
                "手机终止文件任务（消息 {}）",
                response.tag
            );
            ensure!(fields(&response.body)?.uint(3) == 0, "手机拒绝接收文件");
            accepted = true;
            tracing::info!("[mishare send] phone accepted; opening MiExpress SDK channel");
            let channel = self
                .session
                .open_channel_with_data(
                    TRANSFER_SERVICE,
                    48,
                    SdkPlatform::Windows,
                    &crate::protocol::messages::express::user_data(),
                )
                .await?;
            express = Some(
                Express::connect(channel, self.session.peer_ip(), context.cancel.clone()).await?,
            );
            let link = express.as_mut().expect("assigned MiExpress link");
            let mut transferred_files = Vec::new();
            let total = info.iter().map(|i| i.size).sum();
            let mut transferred = 0;
            for (index, (path, item)) in paths.iter().zip(&info).enumerate() {
                let complete = send_file(
                    link,
                    path,
                    item,
                    index + 1,
                    info.len(),
                    total,
                    &mut transferred,
                    context,
                )
                .await?;
                context.emit(TransferEvent::FileComplete(complete.clone()));
                transferred_files.push(complete);
            }
            wait_for_complete(link, request_id, &job_id).await?;
            Ok(TransferSummary {
                files: transferred_files,
                bytes: total,
            })
        }
        .await;
        if result.is_err() && accepted {
            let cancel = control(CANCEL, &request_id_body(request_id));
            if let Some(link) = &mut express {
                let _ = link.send_control(&cancel).await;
            } else {
                let _ = basic.send(&cancel).await;
            }
        }
        if let Some(link) = express {
            let _ = link.close().await;
        }
        let _ = basic.close().await;
        result
    }
}

async fn send_file(
    link: &mut Express,
    path: &Path,
    item: &FileInfo,
    file_index: usize,
    file_count: usize,
    total: u64,
    transferred: &mut u64,
    context: &TaskContext,
) -> Result<TransferredFile> {
    context.check_cancel()?;
    let stream_id = (rand::random::<u32>() & 0x7fff_ffff).max(1) as i32;
    link.send_stream(stream_id, -1, &item.name, &item.id)
        .await?;
    loop {
        match link.next(Duration::from_secs(60)).await? {
            ExpressEvent::Begin {
                stream_id: id,
                mode,
            } if id == stream_id => {
                ensure!(mode == 1 || mode == 2, "未知 MiExpress 接收模式 {mode}");
                break;
            }
            ExpressEvent::End {
                stream_id: id,
                result,
            } if id == stream_id => bail!("手机提前结束文件流：{result}"),
            ExpressEvent::Control(control) if control.tag == 3 || control.tag == 5 => {
                bail!("手机终止文件任务（消息 {}）", control.tag)
            }
            _ => {}
        }
    }
    let mut source = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0; 65_536];
    let mut offset = 0;
    let mut last_progress = tokio::time::Instant::now() - Duration::from_secs(1);
    loop {
        context.check_cancel()?;
        let count = source.read(&mut buffer).await?;
        link.send_data(stream_id, offset, &buffer[..count]).await?;
        offset += count as u64;
        *transferred += count as u64;
        if last_progress.elapsed() >= Duration::from_millis(80) || count == 0 {
            context.emit(TransferEvent::Progress {
                filename: item.name.clone(),
                transferred: *transferred,
                total,
                file_index,
                file_count,
            });
            last_progress = tokio::time::Instant::now();
        }
        if count == 0 {
            break;
        }
    }
    ensure!(offset == item.size, "发送过程中源文件大小发生变化");
    loop {
        match link.next(Duration::from_secs(70)).await? {
            ExpressEvent::End {
                stream_id: id,
                result,
            } if id == stream_id => {
                ensure!(result == 1, "手机回报文件传输失败：{result}");
                break;
            }
            ExpressEvent::Control(control) if control.tag == 3 || control.tag == 5 => {
                bail!("手机终止文件任务（消息 {}）", control.tag)
            }
            _ => {}
        }
    }
    Ok(TransferredFile {
        info: item.clone(),
        path: path.to_owned(),
    })
}

async fn wait_for_complete(link: &mut Express, request_id: u64, job_id: &str) -> Result<()> {
    let mut finish = request_id_body(request_id);
    let mut job = crate::protocol::pb::Writer::new();
    job.bytes(2, job_id);
    finish.extend(job.finish());
    link.send_control(&control(FINISH, &finish)).await?;
    loop {
        if let ExpressEvent::Control(response) = link.next(Duration::from_secs(30)).await? {
            if fields(&response.body)?.uint(1) != request_id {
                continue;
            }
            ensure!(
                response.tag != 3 && response.tag != 5,
                "手机终止文件任务（消息 {}）",
                response.tag
            );
            if response.tag == 8 {
                return Ok(());
            }
        }
    }
}

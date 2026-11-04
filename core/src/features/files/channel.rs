//! SDK FILE transfers, distinct from MiExpress file streams.
use crate::{
    Result,
    protocol::messages::files::{self, ACCEPTED, CANCEL, FAILED, FileOffer, FilePacket, SUCCEEDED},
    session::{Channel, ChannelEvent},
};
use anyhow::{Context, bail, ensure};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicI32, Ordering},
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::broadcast,
};

pub use crate::protocol::messages::files::FilePacket as PublicFilePacket;
const CHUNK_SIZE: usize = 0xfc50;
static NEXT_TASK: AtomicI32 = AtomicI32::new(1);

#[derive(Clone, Debug)]
pub struct ReceivedFile {
    pub offer: FileOffer,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Returns None for application BYTES or non-FILE SDK frames.
pub fn decode(bytes: &[u8]) -> Result<Option<FilePacket>> {
    FilePacket::decode(bytes)
}

async fn next_packet(
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    task_id: Option<i32>,
    timeout: Duration,
) -> Result<FilePacket> {
    tokio::time::timeout(timeout, async {
        loop {
            match incoming
                .recv()
                .await
                .context("SDK FILE event stream ended")?
            {
                ChannelEvent::Closed(reason) => bail!("SDK FILE channel closed: {reason}"),
                ChannelEvent::Data(bytes) => {
                    if let Some(packet) = decode(&bytes)? {
                        if task_id.is_none_or(|id| packet.task_id() == id) {
                            return Ok(packet);
                        }
                    }
                }
            }
        }
    })
    .await
    .context("Timed out waiting for SDK FILE progress")?
}

/// Sends one fromFileStream transfer. SDK acknowledgement is distinct from
/// the application's protobuf response. The tag may be empty, as in the
/// official universalClipboard Server's Packet.fromFile branch.
pub async fn send_file(
    channel: &Channel,
    path: &Path,
    filename: &str,
    content_type: &str,
    tag: &str,
    timeout: Duration,
) -> Result<u64> {
    let mut file = File::open(path).await.context("Open SDK FILE source")?;
    let mut incoming = channel.subscribe();
    let task = NEXT_TASK.fetch_add(1, Ordering::Relaxed);
    channel
        .send_raw(&files::offer(task, filename, content_type, tag))
        .await?;
    let result = async {
        match next_packet(&mut incoming, Some(task), timeout).await? {
            FilePacket::Event {
                event: ACCEPTED, ..
            } => {}
            FilePacket::Event { event, .. } => bail!("SDK FILE offer rejected: event {event}"),
            _ => bail!("SDK FILE expected ACCEPTED acknowledgement"),
        }
        let mut offset = 0u64;
        let mut buffer = vec![0; CHUNK_SIZE];
        loop {
            // The peer can cancel while content is in flight, not just while
            // accepting the offer or reporting its final result.
            loop {
                match incoming.try_recv() {
                    Ok(ChannelEvent::Closed(reason)) => {
                        bail!("SDK FILE channel closed during send: {reason}");
                    }
                    Ok(ChannelEvent::Data(bytes)) => {
                        if let Some(FilePacket::Event { task_id, event }) = decode(&bytes)? {
                            if task_id == task && matches!(event, FAILED | CANCEL) {
                                bail!("SDK FILE peer stopped transfer: event {event}");
                            }
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(error) => {
                        return Err(error).context("SDK FILE control stream ended during send");
                    }
                }
            }
            let size = file
                .read(&mut buffer)
                .await
                .context("Read SDK FILE source")?;
            channel
                .send_raw(&files::data(task, offset as i64, &buffer[..size]))
                .await?;
            if size == 0 {
                break;
            }
            offset += size as u64;
        }
        match next_packet(&mut incoming, Some(task), timeout).await? {
            FilePacket::Event {
                event: SUCCEEDED, ..
            } => Ok(offset),
            FilePacket::Event { event, .. } => {
                bail!("SDK FILE peer did not complete transfer: event {event}")
            }
            _ => bail!("SDK FILE expected SUCCEEDED acknowledgement"),
        }
    }
    .await;
    if result.is_err() {
        let _ = channel.send_raw(&files::event(task, CANCEL)).await;
    }
    result
}

/// Receive the next FILE offer using a receiver created before the application
/// request. Destination is chosen by the caller, never by an untrusted filename.
/// One outstanding file request per channel is required when offer tags are empty.
pub async fn receive_file(
    channel: &Channel,
    incoming: &mut broadcast::Receiver<ChannelEvent>,
    destination: &Path,
    timeout: Duration,
) -> Result<ReceivedFile> {
    let offer = match next_packet(incoming, None, timeout).await? {
        FilePacket::Offer(offer) => offer,
        _ => bail!("Expected SDK FILE offer"),
    };
    let task = offer.task_id;
    let mut file = match File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            let _ = channel.send_raw(&files::event(task, FAILED)).await;
            return Err(error).context("Create SDK FILE destination");
        }
    };
    let result = async {
        channel.send_raw(&files::event(task, ACCEPTED)).await?;
        let mut total = 0u64;
        if offer.content_length != 0 {
            loop {
                match next_packet(incoming, Some(task), timeout).await? {
                    FilePacket::Data { offset, data, .. } => {
                        ensure!(
                            offset >= 0,
                            "SDK FILE sender ended with error offset {offset}"
                        );
                        ensure!(
                            offset as u64 == total,
                            "SDK FILE offset mismatch: got {offset}, expected {total}"
                        );
                        file.write_all(&data)
                            .await
                            .context("Write SDK FILE destination")?;
                        total += data.len() as u64;
                        if data.is_empty()
                            || offer.content_length >= 0 && total == offer.content_length as u64
                        {
                            break;
                        }
                    }
                    FilePacket::Event { event, .. } => bail!("SDK FILE sender event {event}"),
                    FilePacket::Offer(_) => bail!("SDK FILE sender repeated its offer"),
                }
            }
        }
        ensure!(
            offer.content_length < 0 || total == offer.content_length as u64,
            "SDK FILE ended before its declared length"
        );
        file.flush().await?;
        channel.send_raw(&files::event(task, SUCCEEDED)).await?;
        Ok(ReceivedFile {
            offer,
            path: destination.to_owned(),
            bytes: total,
        })
    }
    .await;
    if result.is_err() {
        let _ = channel.send_raw(&files::event(task, FAILED)).await;
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}

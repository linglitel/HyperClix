//! MiExpress TCP lane lifecycle and encrypted packet I/O.
use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
};

use crate::Result;
use crate::protocol::messages::express::{self, ExpressFrame};

pub(super) async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.changed().await;
}

pub(super) struct WritePacket {
    pub(super) bytes: Vec<u8>,
    pub(super) reply: oneshot::Sender<Result<()>>,
}

pub(super) fn spawn_outgoing_lane(
    stream: TcpStream,
    endpoint: SocketAddr,
    key: Vec<u8>,
    incoming: mpsc::Sender<Result<(ExpressFrame, Vec<u8>)>>,
    stop: watch::Receiver<bool>,
) -> mpsc::Sender<WritePacket> {
    let (sender, receiver) = mpsc::channel(2);
    tokio::spawn(outgoing_lane(
        stream, endpoint, key, incoming, receiver, stop,
    ));
    sender
}

/// Retain a partially submitted streamlet until its socket write completes.
/// A replacement TCP connection belongs to the same authenticated task/key;
/// restarting that packet is safe because the receiver indexes by offset.
pub(super) async fn outgoing_lane(
    mut stream: TcpStream,
    endpoint: SocketAddr,
    key: Vec<u8>,
    incoming: mpsc::Sender<Result<(ExpressFrame, Vec<u8>)>>,
    mut commands: mpsc::Receiver<WritePacket>,
    mut stop: watch::Receiver<bool>,
) {
    let mut pending = VecDeque::<(WritePacket, usize)>::new();
    let mut buffer = Vec::new();
    let mut scratch = vec![0; 65_536];
    let result: Result<()> = async {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(packet) => pending.push_back((packet, 0)),
                    None => return Ok(()),
                },
                _ = stop.changed() => return Ok(()),
                ready = stream.readable() => {
                    ready?;
                    let read = match stream.try_read(&mut scratch) {
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                        value => value,
                    };
                    match read {
                        Ok(count) if count > 0 => {
                            buffer.extend_from_slice(&scratch[..count]);
                            while buffer.len() >= 10 {
                                let size = u32::from_be_bytes(buffer[6..10].try_into()?) as usize;
                                ensure!((30..=16 * 1024 * 1024).contains(&size), "MiExpress TCP 包长度不正确");
                                if buffer.len() < size + 10 { break; }
                                let packet = express::decode_tcp(&key, &buffer[10..10 + size])?;
                                buffer.drain(..10 + size);
                                incoming.send(Ok(packet)).await.context("MiExpress 接收任务已结束")?;
                            }
                        }
                        Ok(_) | Err(_) => {
                            stream = reconnect(endpoint, &mut stop).await?;
                            buffer.clear();
                            if let Some((_, offset)) = pending.front_mut() { *offset = 0; }
                        }
                    }
                },
                ready = stream.writable(), if !pending.is_empty() => {
                    ready?;
                    let (packet, offset) = pending.front_mut().expect("pending TCP packet");
                    match stream.try_write(&packet.bytes[*offset..]) {
                        Ok(count) if count > 0 => {
                            *offset += count;
                            if *offset == packet.bytes.len() {
                                let (packet, _) = pending.pop_front().expect("submitted TCP packet");
                                let _ = packet.reply.send(Ok(()));
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {},
                        Ok(_) | Err(_) => {
                            stream = reconnect(endpoint, &mut stop).await?;
                            buffer.clear();
                            *offset = 0;
                        }
                    }
                },
            }
        }
    }.await;
    if let Err(error) = result {
        let reason = format!("{error:#}");
        for (packet, _) in pending {
            let _ = packet.reply.send(Err(anyhow::anyhow!(reason.clone())));
        }
        let _ = incoming.send(Err(error)).await;
    }
}

pub(super) async fn reconnect(
    endpoint: SocketAddr,
    stop: &mut watch::Receiver<bool>,
) -> Result<TcpStream> {
    let stream = tokio::select! {
        stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(endpoint)) => stream.context("MiExpress 同任务 TCP 槽重连超时")??,
        _ = stop.changed() => bail!("MiExpress 已关闭"),
    };
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Keep the listener alive for native same-task TCP slot replacement.
pub(super) async fn accept_replacements(
    listener: TcpListener,
    local: Ipv4Addr,
    peer: Ipv4Addr,
    key: Vec<u8>,
    sender: mpsc::Sender<Result<(ExpressFrame, Vec<u8>)>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            stream = listener.accept() => stream,
            _ = stop.changed() => return,
        };
        match accepted {
            Ok((stream, source)) => {
                if source.ip() != std::net::IpAddr::V4(peer)
                    && source.ip() != std::net::IpAddr::V4(local)
                {
                    continue;
                }
                if let Err(error) = stream.set_nodelay(true) {
                    let _ = sender.send(Err(error.into())).await;
                    return;
                }
                tokio::spawn(read_lane(stream, key.clone(), sender.clone(), stop.clone()));
            }
            Err(error) => {
                let _ = sender.send(Err(error.into())).await;
                return;
            }
        }
    }
}

pub(super) async fn read_lane(
    mut stream: TcpStream,
    key: Vec<u8>,
    sender: mpsc::Sender<Result<(ExpressFrame, Vec<u8>)>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let result = tokio::select! {
            result = read_packet(&mut stream, &key) => result,
            _ = stop.changed() => return,
        };
        if let Err(error) = &result {
            if error.downcast_ref::<std::io::Error>().is_some() {
                // The peer may replace this lane while the task, offsets and
                // file handles remain active on the authenticated SDK CCH.
                return;
            }
        }
        let failed = result.is_err();
        if sender.send(result).await.is_err() || failed {
            return;
        }
    }
}

pub(super) async fn read_packet(
    stream: &mut TcpStream,
    key: &[u8],
) -> Result<(ExpressFrame, Vec<u8>)> {
    let mut header = [0; 10];
    stream
        .read_exact(&mut header)
        .await
        .context("读取 MiExpress TCP 包头")?;
    let size = u32::from_be_bytes(header[6..10].try_into()?) as usize;
    ensure!(
        (30..=16 * 1024 * 1024).contains(&size),
        "MiExpress TCP 包长度不正确"
    );
    let mut body = vec![0; size];
    stream
        .read_exact(&mut body)
        .await
        .context("读取 MiExpress TCP 正文")?;
    express::decode_tcp(key, &body)
}

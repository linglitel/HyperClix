use std::{
    collections::VecDeque,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use rand::RngCore;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, oneshot, watch},
};

use super::lane::{WritePacket, accept_replacements, cancelled, read_lane, spawn_outgoing_lane};
use super::{Control, decode_control};
use crate::{
    Result,
    protocol::messages::express::{self, ExpressFrame, ExpressPacket},
    session::{Channel, ChannelEvent},
};

#[derive(Debug)]
pub(super) enum ExpressEvent {
    Control(Control),
    StreamSend {
        stream_id: i32,
        length: i64,
        tag: String,
    },
    Begin {
        stream_id: i32,
        mode: i32,
    },
    End {
        stream_id: i32,
        result: i32,
    },
    Data {
        stream_id: i32,
        offset: i64,
        data_length: i32,
        data: Vec<u8>,
    },
}

pub(super) struct Express {
    channel: Channel,
    incoming: broadcast::Receiver<ChannelEvent>,
    key: Vec<u8>,
    writers: Vec<mpsc::Sender<WritePacket>>,
    tcp: mpsc::Receiver<Result<(ExpressFrame, Vec<u8>)>>,
    stop: watch::Sender<bool>,
    cancel: watch::Receiver<bool>,
    next_slot: usize,
    serial: i32,
    bytes_serial: Option<i64>,
    bytes: Vec<u8>,
    pending: VecDeque<ExpressFrame>,
}

impl Express {
    pub async fn connect(
        channel: Channel,
        peer: Ipv4Addr,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let mut incoming = channel.subscribe();
        tracing::info!(
            "[miexpress channel={}] waiting for peer TCP handshake",
            channel.id()
        );
        let setup = async {
            let handshake = tokio::time::timeout(Duration::from_secs(30), async {
                let event = tokio::select! { event = incoming.recv() => event?, _ = cancelled(&mut cancel) => bail!("文件任务已取消") };
                match event {
                    ChannelEvent::Data(bytes) => {
                        let frame = express::parse_frame(&bytes)?;
                        let packet = frame.packet()?;
                        ensure!(matches!(packet, ExpressPacket::Handshake { .. }), "MiExpress 首帧不是握手");
                        Ok::<_, anyhow::Error>(packet)
                    }
                    ChannelEvent::Closed(reason) => bail!("MiExpress 通道已关闭：{reason}"),
                }
            }).await.context("等待 MiExpress 握手超时")??;
            let ExpressPacket::Handshake { ab, port, key, lanes } = handshake else {
                bail!("MiExpress 首帧不是握手");
            };
            ensure!(matches!(ab, 0 | 1), "不支持的 MiExpress ab 值");
            ensure!(key.len() == 16, "MiExpress 握手的密钥长度不正确");
            ensure!((1..=32).contains(&lanes), "MiExpress TCP 槽数量不正确");
            let count = lanes;
            tracing::info!(
                "[miexpress channel={}] peer TCP handshake received: port={port} lanes={count}",
                channel.id()
            );
            let mut sockets = Vec::new();
            for _ in 0..count {
                let stream = tokio::select! {
                    value = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((peer, port))) => value.context("连接 MiExpress TCP 超时")??,
                    _ = cancelled(&mut cancel) => bail!("文件任务已取消"),
                };
                stream.set_nodelay(true)?;
                sockets.push(stream);
            }
            tracing::info!("[miexpress channel={}] TCP lanes connected: count={count}", channel.id());
            Ok::<_, anyhow::Error>((key, sockets, SocketAddr::from((peer, port))))
        }.await;
        match setup {
            Ok((key, sockets, endpoint)) => Ok(Self::new(
                channel,
                incoming,
                key,
                sockets,
                cancel,
                VecDeque::new(),
                Some(endpoint),
                None,
            )),
            Err(error) => {
                let _ = channel.close().await;
                Err(error)
            }
        }
    }

    pub async fn listen(
        channel: Channel,
        local: Ipv4Addr,
        peer: Ipv4Addr,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let mut incoming = channel.subscribe();
        let setup = async {
            let listener = TcpListener::bind((local, 0)).await?;
            let mut key = vec![0; 16];
            rand::thread_rng().fill_bytes(&mut key);
            let handshake = express::handshake(listener.local_addr()?.port(), &key);
            channel.send(&handshake).await?;
            tracing::info!(
                "[miexpress channel={}] receiver TCP handshake sent: port={} lanes=8",
                channel.id(),
                listener.local_addr()?.port()
            );
            let mut sockets = Vec::new();
            let mut pending = VecDeque::new();
            tokio::time::timeout(Duration::from_secs(30), async {
                while sockets.len() < 8 {
                    tokio::select! {
                        stream = listener.accept() => {
                            let (stream, source) = stream?;
                            if source.ip() != std::net::IpAddr::V4(peer) && source.ip() != std::net::IpAddr::V4(local) { continue; }
                            stream.set_nodelay(true)?; sockets.push(stream);
                            tracing::info!("[miexpress channel={}] incoming TCP lane connected: {}/8", channel.id(), sockets.len());
                        }
                        event = incoming.recv() => match event? {
                            ChannelEvent::Data(bytes) => pending.push_back(express::parse_frame(&bytes)?),
                            ChannelEvent::Closed(reason) => bail!("MiExpress 通道已关闭：{reason}"),
                        },
                        _ = cancelled(&mut cancel) => bail!("文件任务已取消"),
                    }
                }
                Ok::<_, anyhow::Error>(())
            }).await.context("等待手机连接 MiExpress TCP 槽超时")??;
            tracing::info!("[miexpress channel={}] receiver TCP lanes ready: count=8", channel.id());
            Ok::<_, anyhow::Error>((key, sockets, pending, listener))
        }.await;
        match setup {
            Ok((key, sockets, pending, listener)) => Ok(Self::new(
                channel,
                incoming,
                key,
                sockets,
                cancel,
                pending,
                None,
                Some((listener, local, peer)),
            )),
            Err(error) => {
                let _ = channel.close().await;
                Err(error)
            }
        }
    }

    fn new(
        channel: Channel,
        incoming: broadcast::Receiver<ChannelEvent>,
        key: Vec<u8>,
        sockets: Vec<TcpStream>,
        cancel: watch::Receiver<bool>,
        pending: VecDeque<ExpressFrame>,
        endpoint: Option<SocketAddr>,
        listener: Option<(TcpListener, Ipv4Addr, Ipv4Addr)>,
    ) -> Self {
        let (sender, tcp) = mpsc::channel(128);
        let (stop, stop_rx) = watch::channel(false);
        let mut writers = Vec::new();
        for stream in sockets {
            if let Some(endpoint) = endpoint {
                writers.push(spawn_outgoing_lane(
                    stream,
                    endpoint,
                    key.clone(),
                    sender.clone(),
                    stop_rx.clone(),
                ));
            } else {
                tokio::spawn(read_lane(
                    stream,
                    key.clone(),
                    sender.clone(),
                    stop_rx.clone(),
                ));
            }
        }
        if let Some((listener, local, peer)) = listener {
            tokio::spawn(accept_replacements(
                listener,
                local,
                peer,
                key.clone(),
                sender,
                stop_rx,
            ));
        }
        Self {
            channel,
            incoming,
            key,
            writers,
            tcp,
            stop,
            cancel,
            next_slot: 0,
            serial: 0,
            bytes_serial: None,
            bytes: Vec::new(),
            pending,
        }
    }

    pub async fn send_control(&mut self, data: &[u8]) -> Result<()> {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1);
        self.channel.send(&express::control(serial, data)).await
    }

    pub async fn send_stream(
        &mut self,
        stream_id: i32,
        length: i64,
        filename: &str,
        tag: &str,
    ) -> Result<()> {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1);
        self.channel
            .send(&express::stream_send(
                serial, stream_id, length, filename, tag,
            ))
            .await
    }

    pub async fn begin(&mut self, stream_id: i32) -> Result<()> {
        self.channel
            .send(&express::begin(self.serial, stream_id))
            .await?;
        self.serial = self.serial.wrapping_add(1);
        Ok(())
    }

    pub async fn end(&mut self, stream_id: i32, result: i32) -> Result<()> {
        self.channel
            .send(&express::end(self.serial, stream_id, result))
            .await?;
        self.serial = self.serial.wrapping_add(1);
        Ok(())
    }

    pub async fn send_data(&mut self, stream_id: i32, offset: u64, data: &[u8]) -> Result<()> {
        let metadata = express::streamlet(stream_id, offset as i64, data.len() as i32);
        let packet = express::encode_tcp(&self.key, &metadata, data, None)?;
        ensure!(
            !self.writers.is_empty(),
            "MiExpress 入站任务不能作为发送任务使用"
        );
        let slot = self.next_slot;
        self.next_slot = (self.next_slot + 1) % self.writers.len();
        let (reply, wait) = oneshot::channel();
        tokio::select! {
            result = self.writers[slot].send(WritePacket { bytes: packet, reply }) => result.context("MiExpress TCP 槽已结束")?,
            _ = cancelled(&mut self.cancel) => bail!("文件任务已取消"),
        }
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(30), wait) => result.context("MiExpress TCP 写入超时")?.context("MiExpress TCP 写入已结束")?,
            _ = cancelled(&mut self.cancel) => bail!("文件任务已取消"),
        }
    }

    pub async fn next(&mut self, timeout: Duration) -> Result<ExpressEvent> {
        tokio::time::timeout(timeout, async {
            loop {
                let (packet, data) = if let Some(frame) = self.pending.pop_front() { (frame, Vec::new()) }
                else { tokio::select! {
                    event = self.incoming.recv() => match event? {
                        ChannelEvent::Data(bytes) => (express::parse_frame(&bytes)?, Vec::new()),
                        ChannelEvent::Closed(reason) => bail!("MiExpress 控制通道已关闭：{reason}"),
                    },
                    value = self.tcp.recv() => value.context("MiExpress TCP 接收已退出")??,
                    _ = cancelled(&mut self.cancel) => bail!("文件任务已取消"),
                }};
                match packet.packet()? {
                    ExpressPacket::Control { serial, data: part, finished } => {
                        if self.bytes_serial != Some(serial) {
                            self.bytes.clear();
                            self.bytes_serial = Some(serial);
                        }
                        self.bytes.extend_from_slice(&part);
                        if finished {
                            let control = decode_control(&self.bytes)?;
                            self.bytes.clear();
                            self.bytes_serial = None;
                            return Ok(ExpressEvent::Control(control));
                        }
                    }
                    ExpressPacket::StreamSend { stream_id, length, tag, .. } => {
                        return Ok(ExpressEvent::StreamSend { stream_id, length, tag });
                    }
                    ExpressPacket::Begin { stream_id, mode, .. } => {
                        return Ok(ExpressEvent::Begin { stream_id, mode });
                    }
                    ExpressPacket::End { stream_id, result, .. } => {
                        return Ok(ExpressEvent::End { stream_id, result });
                    }
                    ExpressPacket::Data { stream_id, offset, data_length, .. } => {
                        return Ok(ExpressEvent::Data { stream_id, offset, data_length, data });
                    }
                    ExpressPacket::Handshake { .. } => bail!("收到重复 MiExpress 握手"),
                }
            }
        }).await.context("等待 MiExpress 文件消息超时")?
    }

    pub async fn close(mut self) -> Result<()> {
        self.stop.send_replace(true);
        self.writers.clear();
        self.channel.close().await
    }
}

impl Drop for Express {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn loopback_slot_reconnect_replays_the_incomplete_streamlet() -> Result<()> {
        use crate::mishare::lane::read_packet;
        use crate::protocol::messages::express::streamlet;
        use tokio::io::AsyncReadExt;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = listener.local_addr()?;
        let stream = TcpStream::connect(endpoint).await?;
        // Force a multi-write packet so the first socket is interrupted before
        // submission completes. The replacement must start at its packet head.
        socket2::SockRef::from(&stream).set_send_buffer_size(1024)?;
        let (mut first, _) = listener.accept().await?;
        let key = vec![0x41; 16];
        let source = vec![0xa7; 2 * 1024 * 1024];
        let metadata = streamlet(123, 65_536, source.len() as i32);
        let packet = express::encode_tcp(&key, &metadata, &source, Some([0x23; 12]))?;
        let peer_key = key.clone();
        let peer = tokio::spawn(async move {
            let mut partial = [0; 128];
            first.read_exact(&mut partial).await?;
            socket2::SockRef::from(&first).set_linger(Some(Duration::ZERO))?;
            drop(first);
            let (mut replacement, _) = listener.accept().await?;
            read_packet(&mut replacement, &peer_key).await
        });
        let (incoming, _received) = mpsc::channel(8);
        let (stop, stop_rx) = watch::channel(false);
        let sender = spawn_outgoing_lane(stream, endpoint, key, incoming, stop_rx);
        let (reply, wait) = oneshot::channel();
        sender
            .send(WritePacket {
                bytes: packet,
                reply,
            })
            .await?;
        tokio::time::timeout(Duration::from_secs(10), wait).await???;
        let (decoded, received) = tokio::time::timeout(Duration::from_secs(10), peer).await???;
        stop.send_replace(true);
        match decoded.packet()? {
            ExpressPacket::Data {
                stream_id, offset, ..
            } => {
                assert_eq!(stream_id, 123);
                assert_eq!(offset, 65_536);
            }
            _ => bail!("unexpected MiExpress packet"),
        }
        assert_eq!(received, source);
        Ok(())
    }
}

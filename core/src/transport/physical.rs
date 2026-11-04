use std::{collections::VecDeque, io, net::SocketAddr, sync::Arc, time::Instant};

use crate::{
    Result,
    transport::ble::{BleChannel, IncomingBleChannel},
    transport::kcp::Kcp,
};
use tokio::{net::UdpSocket, sync::mpsc};

pub(crate) struct PhysicalIo {
    pub socket: Arc<UdpSocket>,
    pub peer: SocketAddr,
    pub incoming: Option<mpsc::Receiver<Vec<u8>>>,
}

impl PhysicalIo {
    async fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(incoming) = &mut self.incoming {
            let packet = incoming
                .recv()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "listener stopped"))?;
            buffer[..packet.len()].copy_from_slice(&packet);
            Ok(packet.len())
        } else {
            self.socket.recv(buffer).await
        }
    }
    async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        self.socket.send_to(packet, self.peer).await
    }
}

pub(crate) enum PhysicalTransport {
    Closed,
    Udp {
        io: PhysicalIo,
        kcp: Kcp,
        buffer: Vec<u8>,
        started: Instant,
        last_received: Instant,
        medium: u64,
    },
    Ble {
        channel: BleChannel,
        pending: VecDeque<Vec<u8>>,
        last_received: Instant,
    },
    IncomingBle {
        channel: IncomingBleChannel,
        pending: VecDeque<Vec<u8>>,
        last_received: Instant,
    },
}

impl PhysicalTransport {
    pub(crate) fn udp(io: PhysicalIo) -> Result<Self> {
        Self::udp_medium(io, 128)
    }

    pub(crate) fn udp_medium(io: PhysicalIo, medium: u64) -> Result<Self> {
        Ok(Self::Udp {
            io,
            kcp: Kcp::new()?,
            buffer: vec![0; 65535],
            started: Instant::now(),
            last_received: Instant::now(),
            medium,
        })
    }

    pub(crate) fn ble(channel: BleChannel) -> Self {
        Self::Ble {
            channel,
            pending: VecDeque::new(),
            last_received: Instant::now(),
        }
    }
    pub(crate) fn incoming_ble(channel: IncomingBleChannel) -> Self {
        Self::IncomingBle {
            channel,
            pending: VecDeque::new(),
            last_received: Instant::now(),
        }
    }

    pub(crate) fn medium(&self) -> u64 {
        match self {
            Self::Closed => 0,
            Self::Udp { medium, .. } => *medium,
            Self::Ble { .. } | Self::IncomingBle { .. } => 2,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    pub(crate) fn last_received(&self) -> Option<Instant> {
        match self {
            Self::Closed => None,
            Self::Udp { last_received, .. }
            | Self::Ble { last_received, .. }
            | Self::IncomingBle { last_received, .. } => Some(*last_received),
        }
    }

    pub(crate) fn queue(&mut self, message: &[u8]) -> Result<()> {
        match self {
            Self::Closed => anyhow::bail!("Physical链路已退休"),
            Self::Udp { kcp, .. } => kcp.send(message),
            Self::Ble { pending, .. } | Self::IncomingBle { pending, .. } => {
                pending.push_back(message.to_vec());
                Ok(())
            }
        }
    }

    pub(crate) async fn flush(&mut self) -> Result<()> {
        match self {
            Self::Closed => {}
            Self::Udp {
                io, kcp, started, ..
            } => {
                kcp.update(started.elapsed().as_millis() as u32);
                for packet in kcp.output() {
                    io.send(&packet).await?;
                }
            }
            Self::Ble {
                channel, pending, ..
            } => {
                while let Some(message) = pending.front() {
                    channel.sender().send(message).await?;
                    pending.pop_front();
                }
            }
            Self::IncomingBle {
                channel, pending, ..
            } => {
                while let Some(message) = pending.front() {
                    channel.sender().send(message).await?;
                    pending.pop_front();
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn recv_message(&mut self) -> Result<Vec<u8>> {
        match self {
            Self::Closed => std::future::pending().await,
            Self::Udp {
                io,
                kcp,
                buffer,
                last_received,
                ..
            } => loop {
                if let Some(message) = kcp.receive()? {
                    *last_received = Instant::now();
                    return Ok(message);
                }
                let count = io.recv(buffer).await?;
                kcp.input(&buffer[..count])?;
            },
            Self::Ble {
                channel,
                last_received,
                ..
            } => {
                let message = channel.recv().await?;
                *last_received = Instant::now();
                Ok(message)
            }
            Self::IncomingBle {
                channel,
                last_received,
                ..
            } => {
                let message = channel.recv().await?;
                *last_received = Instant::now();
                Ok(message)
            }
        }
    }

    pub(crate) async fn close(self) -> Result<()> {
        match self {
            Self::Closed => Ok(()),
            Self::Udp { .. } => Ok(()),
            Self::Ble { channel, .. } => channel.close().await,
            Self::IncomingBle { channel, .. } => channel.close().await,
        }
    }
}

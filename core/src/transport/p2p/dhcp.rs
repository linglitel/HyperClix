use crate::Result;
use anyhow::{Context, bail, ensure};
use std::{
    ffi::CString,
    io,
    net::Ipv4Addr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const IPV4: u16 = 0x0800;
const COOKIE: [u8; 4] = [99, 130, 83, 99];
const RETRIES: [u64; 3] = [4, 8, 16];

/// DHCP on the helper's dedicated GC interface; never changes host configuration.
pub(crate) struct DhcpClient {
    socket: OwnedFd,
    index: i32,
    mac: [u8; 6],
    interface: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DhcpLease {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub server: Ipv4Addr,
    pub lease_seconds: u32,
    acquired: Instant,
    renew: Duration,
    rebind: Duration,
}

impl DhcpLease {
    pub fn renewal_after(&self) -> Duration {
        self.renew.saturating_sub(self.acquired.elapsed())
    }

    pub fn remaining(&self) -> Duration {
        Duration::from_secs(self.lease_seconds as u64).saturating_sub(self.acquired.elapsed())
    }

    fn rebound(&self) -> bool {
        self.acquired.elapsed() >= self.rebind
    }
}

impl DhcpClient {
    pub fn new(interface: &str) -> Result<Self> {
        let name = CString::new(interface)?;
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        ensure!(index != 0, "DHCP interface unavailable: {interface}");
        let address = std::fs::read_to_string(format!("/sys/class/net/{interface}/address"))?;
        let octets = address
            .trim()
            .split(':')
            .map(|part| u8::from_str_radix(part, 16))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mac: [u8; 6] = octets
            .try_into()
            .map_err(|_| anyhow::anyhow!("DHCP requires an Ethernet interface"))?;
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                IPV4.to_be() as i32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("open DHCP packet socket");
        }
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = packet_address(index as i32);
        if unsafe {
            libc::bind(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_ll).cast(),
                size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error()).context("bind DHCP to GC interface");
        }
        Ok(Self {
            socket,
            index: index as i32,
            mac,
            interface: interface.to_owned(),
        })
    }

    pub fn acquire(&self, stop: &AtomicBool) -> Result<DhcpLease> {
        let xid = rand::random();
        let started = Instant::now();
        let mut offer = None;
        for seconds in RETRIES {
            self.send(
                xid,
                started,
                1,
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
                None,
            )?;
            tracing::info!("[nearby] DHCP discover: interface={}", self.interface);
            let deadline = Instant::now() + Duration::from_secs(seconds);
            while let Some(reply) = self.receive(xid, deadline, stop)? {
                if reply.kind == 2 && !reply.address.is_unspecified() && reply.server.is_some() {
                    offer = Some(reply);
                    break;
                }
            }
            if offer.is_some() {
                break;
            }
        }
        let offer = offer.context("DHCP offer timed out on GC interface")?;
        let server = offer.server.unwrap();
        for seconds in RETRIES {
            self.send(
                xid,
                started,
                3,
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
                Some((offer.address, server)),
            )?;
            let deadline = Instant::now() + Duration::from_secs(seconds);
            while let Some(reply) = self.receive(xid, deadline, stop)? {
                if reply.server != Some(server) {
                    continue;
                }
                match reply.kind {
                    5 => {
                        ensure!(
                            reply.address == offer.address,
                            "DHCP ACK changed offered address"
                        );
                        let lease = reply.lease(Some(&offer))?;
                        tracing::info!(
                            "[nearby] DHCP lease acquired: interface={} prefix={} lease_seconds={}",
                            self.interface,
                            lease.prefix_len,
                            lease.lease_seconds
                        );
                        return Ok(lease);
                    }
                    6 => bail!("DHCP server rejected GC address request"),
                    _ => {}
                }
            }
        }
        bail!("DHCP ACK timed out on GC interface")
    }

    /// Called at T1. Retries through T2 until ACK, NAK, lease expiry, or helper shutdown.
    pub fn renew(&self, lease: &DhcpLease, stop: &AtomicBool) -> Result<DhcpLease> {
        let xid = rand::random();
        let started = Instant::now();
        loop {
            ensure!(!lease.remaining().is_zero(), "DHCP GC lease expired");
            let destination = if lease.rebound() {
                Ipv4Addr::BROADCAST
            } else {
                lease.server
            };
            self.send(xid, started, 3, lease.address, destination, None)?;
            let boundary = if lease.rebound() {
                lease.remaining()
            } else {
                lease.rebind.saturating_sub(lease.acquired.elapsed())
            };
            let wait = (boundary / 2).max(Duration::from_secs(60)).min(boundary);
            let deadline = Instant::now() + wait;
            while let Some(reply) = self.receive(xid, deadline, stop)? {
                if !lease.rebound() && reply.server != Some(lease.server) {
                    continue;
                }
                match reply.kind {
                    5 => {
                        let fallback = Reply::from_lease(lease);
                        let renewed = reply.lease(Some(&fallback))?;
                        ensure!(
                            renewed.address == lease.address,
                            "DHCP renewal changed GC address"
                        );
                        tracing::info!(
                            "[nearby] DHCP lease renewed: interface={} lease_seconds={}",
                            self.interface,
                            renewed.lease_seconds
                        );
                        return Ok(renewed);
                    }
                    6 => bail!("DHCP GC lease revoked"),
                    _ => {}
                }
            }
        }
    }

    fn send(
        &self,
        xid: u32,
        started: Instant,
        kind: u8,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        selection: Option<(Ipv4Addr, Ipv4Addr)>,
    ) -> Result<()> {
        let mut bootp = vec![0; 240];
        bootp[..4].copy_from_slice(&[1, 1, 6, 0]);
        bootp[4..8].copy_from_slice(&xid.to_be_bytes());
        bootp[8..10].copy_from_slice(
            &(started.elapsed().as_secs().min(u16::MAX as u64) as u16).to_be_bytes(),
        );
        if source.is_unspecified() {
            bootp[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
        }
        bootp[12..16].copy_from_slice(&source.octets());
        bootp[28..34].copy_from_slice(&self.mac);
        bootp[236..240].copy_from_slice(&COOKIE);
        bootp.extend_from_slice(&[53, 1, kind, 61, 7, 1]);
        bootp.extend_from_slice(&self.mac);
        if let Some((address, server)) = selection {
            bootp.extend_from_slice(&[50, 4]);
            bootp.extend_from_slice(&address.octets());
            bootp.extend_from_slice(&[54, 4]);
            bootp.extend_from_slice(&server.octets());
        }
        bootp.extend_from_slice(&[55, 5, 1, 51, 54, 58, 59, 255]);
        bootp.resize(bootp.len().max(300), 0);

        let mut packet = vec![0; 28];
        packet[0] = 0x45;
        let length = (packet.len() + bootp.len()) as u16;
        packet[2..4].copy_from_slice(&length.to_be_bytes());
        packet[8..10].copy_from_slice(&[64, 17]);
        packet[12..16].copy_from_slice(&source.octets());
        packet[16..20].copy_from_slice(&destination.octets());
        let sum = checksum(&packet[..20]);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        packet[20..22].copy_from_slice(&68u16.to_be_bytes());
        packet[22..24].copy_from_slice(&67u16.to_be_bytes());
        packet[24..26].copy_from_slice(&((bootp.len() + 8) as u16).to_be_bytes());
        packet.extend(bootp);
        let address = packet_address(self.index);
        if unsafe {
            libc::sendto(
                self.socket.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&address as *const libc::sockaddr_ll).cast(),
                size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error()).context("send GC DHCP request");
        }
        Ok(())
    }

    fn receive(&self, xid: u32, deadline: Instant, stop: &AtomicBool) -> Result<Option<Reply>> {
        let mut buffer = [0; 2048];
        loop {
            ensure!(!stop.load(Ordering::Acquire), "DHCP stopped");
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let mut fd = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().min(250) as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("wait for GC DHCP reply");
            }
            if ready == 0 {
                continue;
            }
            ensure!(
                fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0,
                "GC DHCP socket closed"
            );
            let size = unsafe {
                libc::recv(
                    self.socket.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if size < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(error).context("read GC DHCP reply");
            }
            if let Some(reply) = Reply::parse(&buffer[..size as usize], xid, &self.mac) {
                return Ok(Some(reply));
            }
        }
    }
}

fn packet_address(index: i32) -> libc::sockaddr_ll {
    let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    address.sll_family = libc::AF_PACKET as u16;
    address.sll_protocol = IPV4.to_be();
    address.sll_ifindex = index;
    address.sll_halen = 6;
    address.sll_addr[..6].fill(0xff);
    address
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes
        .chunks(2)
        .map(|word| ((word[0] as u32) << 8) | word.get(1).copied().unwrap_or(0) as u32)
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

struct Reply {
    kind: u8,
    address: Ipv4Addr,
    server: Option<Ipv4Addr>,
    mask: Option<Ipv4Addr>,
    seconds: Option<u32>,
    renew: Option<u32>,
    rebind: Option<u32>,
}

impl Reply {
    fn parse(packet: &[u8], xid: u32, mac: &[u8; 6]) -> Option<Self> {
        if packet.len() < 28 || packet[0] >> 4 != 4 || packet[9] != 17 {
            return None;
        }
        let ip_length = ((packet[0] & 15) as usize) * 4;
        let total = u16::from_be_bytes(packet[2..4].try_into().ok()?) as usize;
        if ip_length < 20
            || total > packet.len()
            || total < ip_length + 8
            || packet[6] & 0x3f != 0
            || packet[7] != 0
        {
            return None;
        }
        let udp = &packet[ip_length..total];
        if udp[..4] != [0, 67, 0, 68] {
            return None;
        }
        let length = u16::from_be_bytes(udp[4..6].try_into().ok()?) as usize;
        if length < 248 || length > udp.len() {
            return None;
        }
        let body = &udp[8..length];
        if body[..3] != [2, 1, 6]
            || body[4..8] != xid.to_be_bytes()
            || body[28..34] != *mac
            || body[236..240] != COOKIE
        {
            return None;
        }
        let mut reply = Self {
            kind: 0,
            address: Ipv4Addr::new(body[16], body[17], body[18], body[19]),
            server: None,
            mask: None,
            seconds: None,
            renew: None,
            rebind: None,
        };
        let mut options = &body[240..];
        while let Some((&code, tail)) = options.split_first() {
            options = tail;
            match code {
                0 => continue,
                255 => break,
                _ => {}
            }
            let (&length, tail) = options.split_first()?;
            let value = tail.get(..length as usize)?;
            options = &tail[length as usize..];
            match (code, value.len()) {
                (53, 1) => reply.kind = value[0],
                (1 | 54, 4) => {
                    let address = Ipv4Addr::new(value[0], value[1], value[2], value[3]);
                    if code == 1 {
                        reply.mask = Some(address);
                    } else {
                        reply.server = Some(address);
                    }
                }
                (51 | 58 | 59, 4) => {
                    let seconds = u32::from_be_bytes(value.try_into().ok()?);
                    match code {
                        51 => reply.seconds = Some(seconds),
                        58 => reply.renew = Some(seconds),
                        _ => reply.rebind = Some(seconds),
                    }
                }
                _ => {}
            }
        }
        Some(reply)
    }

    fn lease(&self, fallback: Option<&Self>) -> Result<DhcpLease> {
        let address = if self.address.is_unspecified() {
            fallback.map(|r| r.address).unwrap_or(self.address)
        } else {
            self.address
        };
        let mask = self
            .mask
            .or_else(|| fallback.and_then(|r| r.mask))
            .context("DHCP ACK omitted subnet mask")?;
        let mask = u32::from(mask);
        let prefix_len = mask.leading_ones() as u8;
        ensure!(
            mask == u32::MAX.checked_shl(32 - prefix_len as u32).unwrap_or(0),
            "DHCP subnet mask is not contiguous"
        );
        let lease_seconds = self
            .seconds
            .or_else(|| fallback.and_then(|r| r.seconds))
            .context("DHCP ACK omitted lease time")?;
        ensure!(
            lease_seconds != 0 && !address.is_unspecified(),
            "DHCP ACK has no usable lease"
        );
        let renew = self
            .renew
            .filter(|&s| s > 0 && s < lease_seconds)
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(Duration::from_secs(lease_seconds as u64) / 2);
        let rebind = self
            .rebind
            .filter(|&s| Duration::from_secs(s as u64) > renew && s < lease_seconds)
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(Duration::from_secs(lease_seconds as u64) * 7 / 8);
        Ok(DhcpLease {
            address,
            prefix_len,
            server: self
                .server
                .or_else(|| fallback.and_then(|r| r.server))
                .context("DHCP ACK omitted server identifier")?,
            lease_seconds,
            acquired: Instant::now(),
            renew,
            rebind,
        })
    }

    fn from_lease(lease: &DhcpLease) -> Self {
        Self {
            kind: 5,
            address: lease.address,
            server: Some(lease.server),
            mask: Some(Ipv4Addr::from(
                u32::MAX
                    .checked_shl(32 - lease.prefix_len as u32)
                    .unwrap_or(0),
            )),
            seconds: Some(lease.lease_seconds),
            renew: None,
            rebind: None,
        }
    }
}

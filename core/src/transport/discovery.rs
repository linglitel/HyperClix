//! Lyra mDNS discovery on an explicitly chosen local IPv4 interface.
use crate::{Result, identity::Account};
use anyhow::{Context, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::{net::UdpSocket, sync::watch};
pub const SERVICE: &str = "_lyra-mdns._udp.local.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceEndpoint {
    pub local_ip: Ipv4Addr,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub name: String,
    pub device_type: u32,
    pub instance: String,
}
impl DeviceEndpoint {
    pub fn new(local_ip: Ipv4Addr, ip: Ipv4Addr, port: u16) -> Self {
        Self {
            local_ip,
            ip,
            port,
            name: ip.to_string(),
            device_type: 1,
            instance: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkInterface {
    pub name: String,
    pub address: Ipv4Addr,
}
pub fn interfaces() -> Result<Vec<NetworkInterface>> {
    let mut head = std::ptr::null_mut();
    ensure!(
        unsafe { libc::getifaddrs(&mut head) } == 0,
        "读取本机网络接口失败"
    );
    let mut current = head;
    let mut out = Vec::new();
    while !current.is_null() {
        unsafe {
            let item = &*current;
            if !item.ifa_addr.is_null() && (*item.ifa_addr).sa_family as i32 == libc::AF_INET {
                let addr = &*(item.ifa_addr as *const libc::sockaddr_in);
                let address = Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes());
                if !address.is_loopback() {
                    out.push(NetworkInterface {
                        name: std::ffi::CStr::from_ptr(item.ifa_name)
                            .to_string_lossy()
                            .into_owned(),
                        address,
                    });
                }
            }
            current = item.ifa_next;
        }
    }
    unsafe { libc::freeifaddrs(head) };
    Ok(out)
}

/// Discover current phone IP/KCP endpoints. The SRV port is not the KCP port.
pub async fn discover(local_ip: Ipv4Addr, duration: Duration) -> Result<Vec<DeviceEndpoint>> {
    let socket = multicast_socket(local_ip, false)?;
    let query = query();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);
    let mut response = Responses::default();
    let mut buffer = vec![0; 65535];
    loop {
        tokio::select! {
            _=&mut deadline=>break,
            _=ticker.tick()=>{socket.send_to(&query,(Ipv4Addr::new(224,0,0,251),5353)).await?;},
            received=socket.recv_from(&mut buffer)=>{let (n,source)=received?;response.add(&buffer[..n],source)?;}
        }
    }
    response.devices(local_ip)
}

fn multicast_socket(local_ip: Ipv4Addr, listen: bool) -> Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_multicast_if_v4(&local_ip)?;
    socket.set_multicast_ttl_v4(255)?;
    socket.bind(
        &SocketAddr::from((
            if listen {
                Ipv4Addr::UNSPECIFIED
            } else {
                local_ip
            },
            if listen { 5353 } else { 0 },
        ))
        .into(),
    )?;
    if listen {
        socket.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 251), &local_ip)?;
    }
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket.into())?)
}
fn name(out: &mut Vec<u8>, value: &str) {
    for part in value.trim_end_matches('.').split('.') {
        out.push(part.len() as u8);
        out.extend(part.as_bytes());
    }
    out.push(0);
}
fn query() -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    name(&mut out, SERVICE);
    out.extend([0, 12, 128, 1]);
    out
}

fn dns_name(data: &[u8], offset: &mut usize) -> Result<String> {
    let mut pos = *offset;
    let mut jumped = false;
    let mut labels = Vec::new();
    for _ in 0..128 {
        let size = *data.get(pos).context("截断的DNS名称")?;
        pos += 1;
        if size & 0xc0 == 0xc0 {
            let low = *data.get(pos).context("截断的DNS指针")?;
            pos += 1;
            if !jumped {
                *offset = pos;
                jumped = true;
            }
            pos = (((size & 0x3f) as usize) << 8) | low as usize;
            continue;
        }
        if size == 0 {
            if !jumped {
                *offset = pos;
            }
            return Ok(format!("{}.", labels.join(".")));
        }
        ensure!(size <= 63, "DNS名称长度错误");
        let part = data
            .get(pos..pos + size as usize)
            .context("截断的DNS标签")?;
        labels.push(String::from_utf8_lossy(part).to_string());
        pos += size as usize;
    }
    anyhow::bail!("DNS名称循环指针")
}
fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        data.get(offset..offset + 2)
            .context("截断的DNS记录")?
            .try_into()?,
    ))
}

#[derive(Default)]
struct Responses {
    instances: BTreeSet<String>,
    txt: BTreeMap<String, Vec<Vec<u8>>>,
    hosts: BTreeMap<String, String>,
    addresses: BTreeMap<String, Ipv4Addr>,
    sources: BTreeMap<String, Ipv4Addr>,
}
impl Responses {
    fn add(&mut self, data: &[u8], source: SocketAddr) -> Result<()> {
        if data.len() < 12 {
            return Ok(());
        }
        let questions = u16_at(data, 4)?;
        let count =
            u16_at(data, 6)? as usize + u16_at(data, 8)? as usize + u16_at(data, 10)? as usize;
        let mut offset = 12;
        for _ in 0..questions {
            dns_name(data, &mut offset)?;
            offset += 4;
        }
        for _ in 0..count {
            let record = dns_name(data, &mut offset)?.to_lowercase();
            let kind = u16_at(data, offset)?;
            let len = u16_at(data, offset + 8)? as usize;
            offset += 10;
            let end = offset + len;
            ensure!(end <= data.len(), "截断的DNS响应");
            match kind {
                12 if record == SERVICE => {
                    let mut p = offset;
                    self.instances
                        .insert(dns_name(data, &mut p)?.to_lowercase());
                }
                1 if len == 4 => {
                    self.addresses.insert(
                        record.clone(),
                        Ipv4Addr::new(
                            data[offset],
                            data[offset + 1],
                            data[offset + 2],
                            data[offset + 3],
                        ),
                    );
                }
                33 if record.ends_with(SERVICE) => {
                    let mut p = offset + 6;
                    self.hosts
                        .insert(record.clone(), dns_name(data, &mut p)?.to_lowercase());
                    self.instances.insert(record.clone());
                }
                16 if record.ends_with(SERVICE) => {
                    let mut p = offset;
                    let mut values = Vec::new();
                    while p < end {
                        let n = data[p] as usize;
                        p += 1;
                        ensure!(p + n <= end, "截断的DNS TXT");
                        values.push(data[p..p + n].to_vec());
                        p += n;
                    }
                    if values.iter().any(|s| s.starts_with(b"AppData=")) {
                        self.txt.insert(record.clone(), values);
                        self.instances.insert(record.clone());
                    }
                }
                _ => {}
            }
            if let std::net::IpAddr::V4(ip) = source.ip() {
                self.sources.insert(record, ip);
            }
            offset = end;
        }
        Ok(())
    }
    fn devices(&self, local_ip: Ipv4Addr) -> Result<Vec<DeviceEndpoint>> {
        let mut devices = Vec::new();
        for instance in &self.instances {
            let Some(txt) = self.txt.get(instance) else {
                continue;
            };
            let encoded: Vec<u8> = txt
                .iter()
                .filter_map(|s| s.strip_prefix(b"AppData="))
                .flatten()
                .copied()
                .collect();
            let data = STANDARD
                .decode(&encoded)
                .or_else(|_| STANDARD_NO_PAD.decode(&encoded))?;
            if data.len() < 7 {
                continue;
            }
            let flags = u16::from_be_bytes([data[0], data[1]]);
            let mut offset = 7;
            for (bit, width) in [(0, 2), (1, 2), (2, 3)] {
                if flags & (1 << bit) != 0 {
                    offset += width;
                }
            }
            let mut name = String::new();
            let mut port = 0;
            while offset + 2 <= data.len() {
                let kind = data[offset];
                let len = data[offset + 1] as usize;
                offset += 2;
                let value = data.get(offset..offset + len).context("截断的Lyra公告")?;
                if kind == 2 {
                    name = String::from_utf8_lossy(value).into_owned();
                }
                if kind == 10 && len >= 3 && value[0] & 1 != 0 {
                    port = u16::from_be_bytes([value[1], value[2]]);
                }
                offset += len;
            }
            if port == 0 {
                continue;
            }
            let ip = self
                .hosts
                .get(instance)
                .and_then(|host| self.addresses.get(host))
                .or_else(|| self.sources.get(instance));
            if let Some(ip) = ip {
                if *ip != local_ip {
                    devices.push(DeviceEndpoint {
                        local_ip,
                        ip: *ip,
                        port,
                        name,
                        device_type: data[2] as u32,
                        instance: instance.clone(),
                    });
                }
            }
        }
        Ok(devices)
    }
}

#[derive(Debug, Clone)]
pub struct AnnouncementOptions {
    pub local_ip: Ipv4Addr,
    pub name: String,
    pub device_type: u8,
    pub medium_type: u32,
    pub channel: u32,
    pub kcp_port: Option<u16>,
    pub interval: Duration,
}
impl Default for AnnouncementOptions {
    fn default() -> Self {
        Self {
            local_ip: Ipv4Addr::UNSPECIFIED,
            name: crate::DEFAULT_DEVICE_NAME.into(),
            device_type: 4,
            medium_type: 64,
            channel: 0,
            kcp_port: None,
            interval: Duration::from_secs(30),
        }
    }
}
pub struct Announcement {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
}
impl Announcement {
    pub async fn start(account: &Account, options: AnnouncementOptions) -> Result<Self> {
        let socket = multicast_socket(options.local_ip, true)?;
        let (stop, mut stopped) = watch::channel(false);
        let mut id = [0; 4];
        rand::thread_rng().fill_bytes(&mut id);
        let id = u32::from_be_bytes(id);
        let uid = account.uid.clone();
        let task = tokio::spawn(async move {
            let mut timer = tokio::time::interval(options.interval);
            let mut buffer = vec![0; 65535];
            let group = (Ipv4Addr::new(224, 0, 0, 251), 5353);
            loop {
                tokio::select! {
                    _=stopped.changed()=>break,
                    _=timer.tick()=>{socket.send_to(&announcement_packet(&uid,id,&options,120),group).await?;},
                    result=socket.recv_from(&mut buffer)=>{let(n,source)=result?;if n>=12&&buffer[2]&0x80==0{
                        let mut p=12;if dns_name(&buffer[..n],&mut p).ok().as_deref()==Some(SERVICE){let target=if source.port()!=5353{source}else{SocketAddr::from(group)};socket.send_to(&announcement_packet(&uid,id,&options,120),target).await?;}
                    }}
                }
            }
            socket
                .send_to(&announcement_packet(&uid, id, &options, 0), group)
                .await?;
            Ok(())
        });
        Ok(Self { stop, task })
    }
    pub async fn close(self) -> Result<()> {
        let _ = self.stop.send(true);
        self.task.await?
    }
}
fn announcement_packet(uid: &str, id: u32, o: &AnnouncementOptions, ttl: u32) -> Vec<u8> {
    let mut app = [
        vec![0, 1, o.device_type],
        id.to_be_bytes().to_vec(),
        Sha256::digest(uid.as_bytes())[..2].to_vec(),
    ]
    .concat();
    let mut tlv = |kind: u8, data: &[u8]| {
        app.extend([kind, data.len() as u8]);
        app.extend(data);
    };
    tlv(0, &[24, 0, 1, 3]);
    tlv(1, &[32]);
    tlv(35, &[]);
    tlv(35, &[0, 1]);
    tlv(2, o.name.as_bytes());
    if let Some(port) = o.kcp_port {
        tlv(10, &[vec![1], port.to_be_bytes().to_vec()].concat());
    }
    let instance = format!("{id:08X}.{SERVICE}");
    let host = format!("{id:08X}.local.");
    let mut out = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0];
    let mut ptr = Vec::new();
    name(&mut ptr, &instance);
    record(&mut out, SERVICE, 12, 1, ttl, &ptr);
    let mut srv = vec![0, 0, 0, 0, 0x14, 0xe9];
    name(&mut srv, &host);
    record(&mut out, &instance, 33, 0x8001, ttl, &srv);
    let mut txt = Vec::new();
    let encoded = STANDARD.encode(app);
    for part in encoded.as_bytes().chunks(192) {
        let item = [b"AppData=".as_slice(), part].concat();
        txt.push(item.len() as u8);
        txt.extend(item);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for item in [
        format!("MediumType={}", o.medium_type),
        format!("CH={}", o.channel),
        format!("TS={now}"),
    ] {
        txt.push(item.len() as u8);
        txt.extend(item.as_bytes());
    }
    record(&mut out, &instance, 16, 0x8001, ttl, &txt);
    record(&mut out, &host, 1, 0x8001, ttl, &o.local_ip.octets());
    out
}
fn record(out: &mut Vec<u8>, label: &str, kind: u16, class: u16, ttl: u32, data: &[u8]) {
    name(out, label);
    out.extend(kind.to_be_bytes());
    out.extend(class.to_be_bytes());
    out.extend(ttl.to_be_bytes());
    out.extend((data.len() as u16).to_be_bytes());
    out.extend(data);
}

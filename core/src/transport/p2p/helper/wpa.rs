//! WPA supplicant control socket and command helpers.
use crate::Result;
use crate::transport::dbus::proxy;
use anyhow::{Context, ensure};
use std::{
    fs,
    io::BufRead,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use zbus::zvariant::OwnedObjectPath;

pub(super) struct Ctrl {
    socket: UnixDatagram,
    local: PathBuf,
}
impl Ctrl {
    pub(super) fn connect(remote: &Path, directory: &Path) -> Result<Self> {
        let local = directory.join(format!("ctrl-{}", uuid::Uuid::new_v4().simple()));
        let socket = UnixDatagram::bind(&local)?;
        socket.connect(remote).context("连接 wpa 控制接口")?;
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;
        Ok(Self { socket, local })
    }
    pub(super) fn command(&self, command: &str) -> Result<String> {
        self.socket.send(command.as_bytes())?;
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let mut buf = [0u8; 8192];
            match self.socket.recv(&mut buf) {
                Ok(size) => {
                    let value = String::from_utf8_lossy(&buf[..size]).trim().to_owned();
                    if value.starts_with('<') || value.starts_with("IFNAME=") {
                        continue;
                    }
                    ensure!(
                        !value.starts_with("FAIL") && value != "UNKNOWN COMMAND",
                        "wpa 控制操作被拒绝"
                    );
                    return Ok(value);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) && Instant::now() < until => {}
                Err(error) => return Err(error).context("wpa 控制操作没有响应"),
            }
        }
    }
}
impl Drop for Ctrl {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.local);
    }
}

pub(super) fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

pub(super) fn command(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()?;
    ensure!(
        status.success(),
        "网络操作失败：{} ({status})",
        Path::new(program).file_name().unwrap().to_string_lossy()
    );
    Ok(())
}

pub(super) fn forward_wpa(stream: impl std::io::Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stream)
            .lines()
            .map_while(std::result::Result::ok)
        {
            tracing::info!("[p2p-helper] wpa: {}", redact_wpa(line));
        }
    });
}

fn redact_wpa(mut line: String) -> String {
    for key in ["ssid=", "passphrase=", "psk=", "password=", "ssid "] {
        let mut offset = 0;
        while let Some(found) = line[offset..].to_ascii_lowercase().find(key) {
            let start = offset + found + key.len();
            let bytes = line.as_bytes();
            let quoted = bytes
                .get(start)
                .copied()
                .filter(|c| *c == b'\'' || *c == b'"');
            if key == "ssid " && quoted.is_none() {
                offset = start;
                continue;
            }
            let mut end = start + usize::from(quoted.is_some());
            let mut escaped = false;
            while end < bytes.len() {
                let c = bytes[end];
                if !escaped && quoted == Some(c) {
                    end += 1;
                    break;
                }
                if quoted.is_none() && c.is_ascii_whitespace() {
                    break;
                }
                escaped = !escaped && c == b'\\';
                end += 1;
            }
            line.replace_range(start..end, "<redacted>");
            offset = start + "<redacted>".len();
        }
    }
    line
}

pub(super) fn ctrl_ssid(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().iter().copied();
    let mut decoded = Vec::new();
    while let Some(c) = bytes.next() {
        if c != b'\\' {
            decoded.push(c);
            continue;
        }
        match bytes.next() {
            Some(b'x') => {
                let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                if let Ok(value) = hex::decode(hex) {
                    decoded.extend(value);
                }
            }
            Some(b'n') => decoded.push(b'\n'),
            Some(b'r') => decoded.push(b'\r'),
            Some(b't') => decoded.push(b'\t'),
            Some(b'e') => decoded.push(27),
            Some(c) => decoded.push(c),
            None => {}
        }
    }
    decoded
}

pub(super) async fn check_idle(parent: &str) -> Result<()> {
    const NM: &str = "org.freedesktop.NetworkManager";
    const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
    let bus = zbus::Connection::system().await?;
    let manager = proxy(&bus, NM, "/org/freedesktop/NetworkManager", NM).await?;
    let devices: Vec<OwnedObjectPath> = manager.call("GetDevices", &()).await?;
    for path in devices {
        let device = proxy(&bus, NM, path.as_str(), DEVICE).await?;
        let interface: String = device.get_property("Interface").await?;
        if interface == format!("p2p-dev-{parent}") {
            let active: OwnedObjectPath = device.get_property("ActiveConnection").await?;
            ensure!(active.as_str() == "/", "现有 P2P 正在使用，保留当前连接");
        }
    }
    let wpa = proxy(
        &bus,
        "fi.w1.wpa_supplicant1",
        "/fi/w1/wpa_supplicant1",
        "fi.w1.wpa_supplicant1",
    )
    .await?;
    let interfaces: Vec<OwnedObjectPath> = wpa.get_property("Interfaces").await?;
    for path in interfaces {
        let interface = proxy(
            &bus,
            "fi.w1.wpa_supplicant1",
            path.as_str(),
            "fi.w1.wpa_supplicant1.Interface",
        )
        .await?;
        let name: String = interface.get_property("Ifname").await?;
        if name == parent
            || name == format!("p2p-dev-{parent}")
            || name.starts_with(&format!("p2p-{parent}-"))
        {
            let p2p = proxy(
                &bus,
                "fi.w1.wpa_supplicant1",
                path.as_str(),
                "fi.w1.wpa_supplicant1.Interface.P2PDevice",
            )
            .await?;
            let group: OwnedObjectPath = p2p.get_property("Group").await?;
            ensure!(group.as_str() == "/", "wpa 已有 P2P 组，保留当前连接");
        }
    }
    Ok(())
}

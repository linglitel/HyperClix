//! P2P helper WPA state and join/leave lifecycle.
use super::super::dhcp::{DhcpClient, DhcpLease};
use super::wpa::{Ctrl, check_idle, command, ctrl_ssid, forward_wpa, value};
use super::{ACTIVE_GROUP, Response, respond};
use crate::Result;
use anyhow::{Context, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::Ipv4Addr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub(super) struct State {
    directory: PathBuf,
    parent: String,
    system: Option<Ctrl>,
    old_disabled: Option<String>,
    interface: Option<String>,
    process: Option<Child>,
    control: Option<Ctrl>,
    management: Option<String>,
    group_interface: Option<String>,
    pub(super) group_id: u64,
    network: Option<u32>,
    dhcp: Option<(DhcpClient, DhcpLease, String)>,
    pub(super) address: String,
}

impl State {
    pub(super) fn prepare(parent: &str, stop: &AtomicBool) -> Result<Self> {
        let directory = PathBuf::from(format!(
            "/run/hyperclix-p2p-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let mut state = Self {
            directory,
            parent: parent.into(),
            system: None,
            old_disabled: None,
            interface: None,
            process: None,
            control: None,
            management: None,
            group_interface: None,
            group_id: 0,
            network: None,
            dhcp: None,
            address: String::new(),
        };
        tokio::runtime::Runtime::new()?.block_on(check_idle(parent))?;
        let system = Ctrl::connect(
            &PathBuf::from(format!("/run/wpa_supplicant/{parent}")),
            &state.directory,
        )?;
        let original = system.command("STATUS")?;
        tracing::info!(
            "[p2p-helper] original STA state={}",
            value(&original, "wpa_state").unwrap_or("unknown")
        );
        let old = system.command("GET p2p_disabled")?;
        ensure!(matches!(old.as_str(), "0" | "1"), "无法读取原 P2P 管理状态");
        state.system = Some(system);
        state.old_disabled = Some(old);
        state
            .system
            .as_ref()
            .unwrap()
            .command("SET p2p_disabled 1")?;
        tracing::info!("[p2p-helper] idle system P2P management paused; STA retained");

        let interface = format!("xcp{:x}", std::process::id());
        command(
            "/usr/bin/iw",
            &[
                "dev",
                parent,
                "interface",
                "add",
                &interface,
                "type",
                "managed",
            ],
        )?;
        state.interface = Some(interface.clone());
        fs::write(
            format!("/proc/sys/net/ipv6/conf/{interface}/disable_ipv6"),
            b"1",
        )
        .context("禁用自有近场接口的 IPv6 自动配置")?;
        command(
            "/usr/bin/nmcli",
            &["device", "set", &interface, "managed", "no"],
        )?;
        let ctrl = state.directory.join("wpa");
        fs::create_dir(&ctrl)?;
        let config_path = state.directory.join("wpa.conf");
        let mut config = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config_path)?;
        writeln!(
            config,
            "ctrl_interface={}\nupdate_config=0\ndevice_name=Linux PC\np2p_no_group_iface=1",
            ctrl.display()
        )?;
        let global = state.directory.join("global");
        let mut process = Command::new("/usr/bin/wpa_supplicant")
            .args(["-Dnl80211", "-i", &interface, "-c"])
            .arg(config_path)
            .arg("-g")
            .arg(&global)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("启动独立 wpa_supplicant")?;
        forward_wpa(process.stdout.take().unwrap());
        forward_wpa(process.stderr.take().unwrap());
        state.process = Some(process);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "近场准备已取消");
            ensure!(
                state.process.as_mut().unwrap().try_wait()?.is_none(),
                "独立 wpa_supplicant 初始化失败"
            );
            if global.exists() {
                let control = Ctrl::connect(&global, &state.directory)?;
                let interfaces = control.command("INTERFACES")?;
                if let Some(device) = interfaces.lines().find(|name| name.starts_with("p2p-dev-")) {
                    let status = control.command(&format!("IFNAME={device} STATUS"))?;
                    if let Some(address) =
                        value(&status, "p2p_device_address").or_else(|| value(&status, "address"))
                    {
                        state.address = address.to_owned();
                        state.management = Some(device.to_owned());
                        state.control = Some(control);
                        break;
                    }
                }
            }
            ensure!(Instant::now() < until, "独立 P2P 管理设备没有就绪");
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!("[p2p-helper] isolated P2P management ready");
        Ok(state)
    }

    fn private_command(&self, command: &str) -> Result<String> {
        let management = self
            .management
            .as_ref()
            .context("独立 P2P 管理接口未准备")?;
        self.control
            .as_ref()
            .context("独立 wpa 未准备")?
            .command(&format!("IFNAME={management} {command}"))
    }

    pub(super) fn join(
        &mut self,
        ssid: &str,
        password: &str,
        server_mac: &str,
        channel: u32,
        client_ip: Option<Ipv4Addr>,
        endpoint: Ipv4Addr,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<()> {
        ensure!(!stop.load(Ordering::Relaxed), "P2P 入组已取消");
        ensure!(self.group_interface.is_none(), "自有 P2P 组已连接");
        ensure!(
            server_mac.len() == 17
                && server_mac
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() || c == b':'),
            "认证 GO 地址格式无效"
        );
        let frequency = match channel {
            1..=13 => 2407 + channel * 5,
            14 => 2484,
            32..=177 => 5000 + channel * 5,
            _ => bail!("手机返回的 P2P 信道不支持：{channel}"),
        };
        let network = self.private_command("ADD_NETWORK")?;
        let id: u32 = network.parse().context("wpa 未返回网络编号")?;
        self.network = Some(id);
        let mut key = [0u8; 32];
        openssl::pkcs5::pbkdf2_hmac(
            password.as_bytes(),
            ssid.as_bytes(),
            4096,
            openssl::hash::MessageDigest::sha1(),
            &mut key,
        )?;
        for (field, encoded) in [
            ("ssid", hex::encode(ssid)),
            ("psk", hex::encode(key)),
            ("mode", "0".into()),
            ("disabled", "2".into()),
        ] {
            self.private_command(&format!("SET_NETWORK {id} {field} {encoded}"))
                .with_context(|| format!("设置临时 P2P 网络字段 {field}"))?;
        }
        // Xiaomi may supply a P2P Device MAC here, not the GO BSSID.
        // The authenticated SSID/PSK selects the group without a false BSSID pin.
        self.private_command(&format!("P2P_GROUP_ADD persistent={id} freq={frequency}"))?;
        // Reuse only our virtual interface; nl80211 changes it to P2P_CLIENT.
        let interface = self.interface.as_ref().unwrap().clone();
        self.group_interface = Some(interface.clone());
        tracing::info!(
            "[p2p-helper] authenticated OOB GC association requested: channel={channel}"
        );
        let until = Instant::now() + timeout;
        let mut previous_state = String::new();
        let mut previous_bss = None;
        let mut next_bss = Instant::now();
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "P2P 入组已取消");
            ensure!(Instant::now() < until, "P2P GC 关联超时");
            ensure!(
                self.process.as_mut().unwrap().try_wait()?.is_none(),
                "独立 wpa 已退出"
            );
            let status = self
                .control
                .as_ref()
                .unwrap()
                .command(&format!("IFNAME={interface} STATUS"))?;
            let state = value(&status, "wpa_state").unwrap_or("unknown");
            let freq = value(&status, "freq").and_then(|value| value.parse::<u32>().ok());
            let summary = format!("state={state} frequency={freq:?}");
            if summary != previous_state {
                tracing::info!("[p2p-helper] GC association: {summary}");
                previous_state = summary;
            }
            if Instant::now() >= next_bss {
                let results = self
                    .control
                    .as_ref()
                    .unwrap()
                    .command(&format!("IFNAME={interface} SCAN_RESULTS"))?;
                let mut matching_ssids = 0;
                let mut matching_bssid = false;
                let mut matching_frequency = false;
                for line in results.lines().skip(1) {
                    let columns: Vec<&str> = line.splitn(5, '\t').collect();
                    if columns.len() == 5 && ctrl_ssid(columns[4]) == ssid.as_bytes() {
                        matching_ssids += 1;
                        matching_bssid |= columns[0].eq_ignore_ascii_case(server_mac);
                        matching_frequency |= columns[1].parse::<u32>().ok() == Some(frequency);
                    }
                }
                let diagnostics = (matching_ssids, matching_bssid, matching_frequency);
                if previous_bss != Some(diagnostics) {
                    tracing::info!(
                        "[p2p-helper] GC BSS: authenticated_ssid_matches={matching_ssids} server_bssid_matches={matching_bssid} frequency_matches={matching_frequency}"
                    );
                    previous_bss = Some(diagnostics);
                }
                next_bss = Instant::now() + Duration::from_secs(2);
            }
            if state == "COMPLETED" {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let role = Command::new("/usr/bin/iw")
            .args(["dev", &interface, "info"])
            .stderr(Stdio::inherit())
            .output()?;
        ensure!(
            role.status.success()
                && String::from_utf8_lossy(&role.stdout)
                    .lines()
                    .any(|line| line.trim() == "type P2P-client"),
            "自有无线接口未进入真实 P2P-client 类型"
        );
        self.private_command(&format!("REMOVE_NETWORK {id}"))?;
        self.network = None;
        tracing::info!("[p2p-helper] real P2P GC associated: interface={interface}");
        if let Some(address) = client_ip {
            let configured = format!("{address}/32");
            command(
                "/usr/bin/ip",
                &["address", "add", &configured, "dev", &interface],
            )?;
            command(
                "/usr/bin/ip",
                &[
                    "route",
                    "add",
                    &format!("{endpoint}/32"),
                    "dev",
                    &interface,
                    "scope",
                    "link",
                ],
            )?;
            respond(&Response::Joined {
                group_id: self.group_id,
                interface,
                address,
            })?;
            return Ok(());
        }
        let dhcp = DhcpClient::new(&interface)?;
        let lease = dhcp.acquire(stop)?;
        let configured = format!("{}/{}", lease.address, lease.prefix_len);
        command(
            "/usr/bin/ip",
            &["address", "add", &configured, "dev", &interface],
        )?;
        respond(&Response::Joined {
            group_id: self.group_id,
            interface,
            address: lease.address,
        })?;
        self.dhcp = Some((dhcp, lease, configured));
        Ok(())
    }

    pub(super) fn renew(&mut self, stop: &AtomicBool) -> Result<()> {
        let Some((client, lease, configured)) = self.dhcp.as_mut() else {
            return Ok(());
        };
        if !lease.renewal_after().is_zero() || stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let interface = self
            .group_interface
            .as_ref()
            .context("GC interface missing")?;
        match client.renew(lease, stop) {
            Ok(renewed) => {
                ensure!(
                    renewed.address == lease.address,
                    "P2P DHCP 地址已变化，需要重新建立数据通道"
                );
                let next = format!("{}/{}", renewed.address, renewed.prefix_len);
                if *configured != next {
                    command(
                        "/usr/bin/ip",
                        &["address", "del", configured, "dev", interface],
                    )?;
                    command("/usr/bin/ip", &["address", "add", &next, "dev", interface])?;
                    *configured = next;
                }
                *lease = renewed;
                Ok(())
            }
            Err(error) => {
                if !stop.load(Ordering::Relaxed) {
                    let _ = command(
                        "/usr/bin/ip",
                        &["address", "del", configured, "dev", interface],
                    );
                }
                Err(error)
            }
        }
    }

    pub(super) fn leave(&mut self) -> Result<()> {
        self.dhcp.take();
        if let Some(interface) = &self.interface {
            command("/usr/bin/ip", &["-4", "route", "flush", "dev", interface])?;
            command("/usr/bin/ip", &["-4", "address", "flush", "dev", interface])?;
        }
        // All commands below address only our independent supplicant.
        if self.control.is_some() && self.management.is_some() {
            let _ = self.private_command("P2P_CANCEL");
            if let Some(interface) = &self.group_interface {
                let (configured, state) = self.group_state(interface)?;
                if configured || state == "COMPLETED" {
                    if let Err(error) =
                        self.private_command(&format!("P2P_GROUP_REMOVE {interface}"))
                    {
                        let (remaining, state) = self.group_state(interface)?;
                        ensure!(
                            !remaining && state != "COMPLETED",
                            "撤销自有 GC 失败且组仍存在：{error:#}"
                        );
                        tracing::info!("[p2p-helper] GC group already removed by supplicant");
                    }
                }
            }
            if let Some(network) = self.network.take() {
                self.private_command(&format!("REMOVE_NETWORK {network}"))?;
            }
        }
        self.group_interface = None;
        self.group_id = 0;
        ACTIVE_GROUP.store(0, Ordering::Relaxed);
        tracing::info!("[p2p-helper] own GC group released; prepared P2P management retained");
        Ok(())
    }

    fn group_state(&self, interface: &str) -> Result<(bool, String)> {
        let control = self.control.as_ref().context("独立 wpa 未准备")?;
        let status = control.command(&format!("IFNAME={interface} STATUS"))?;
        let networks = control.command(&format!("IFNAME={interface} LIST_NETWORKS"))?;
        // This private parent has only the non-persistent GC network generated by wpa.
        let configured = networks
            .lines()
            .skip(1)
            .any(|line| !line.contains("[P2P-PERSISTENT]"));
        Ok((
            configured,
            value(&status, "wpa_state").unwrap_or("unknown").to_owned(),
        ))
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = self.leave();
        if let Some(mut process) = self.process.take() {
            unsafe {
                libc::kill(process.id() as i32, libc::SIGTERM);
            }
            let until = Instant::now() + Duration::from_secs(3);
            while matches!(process.try_wait(), Ok(None)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            if matches!(process.try_wait(), Ok(None)) {
                let _ = process.kill();
            }
            let _ = process.wait();
        }
        if let Some(interface) = &self.interface {
            let _ = command("/usr/bin/iw", &["dev", interface, "del"]);
        }
        if let (Some(system), Some(old)) = (&self.system, &self.old_disabled) {
            match system.command(&format!("SET p2p_disabled {old}")) {
                Ok(_) => tracing::info!(
                    "[p2p-helper] original system P2P management restored: parent={}",
                    self.parent
                ),
                Err(error) => {
                    tracing::info!(
                        "[p2p-helper] restoring original P2P management failed: {error:#}"
                    )
                }
            }
        }
        self.control.take();
        self.system.take();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

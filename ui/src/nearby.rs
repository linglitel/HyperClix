use std::{net::Ipv4Addr, process::Command, time::Duration};

use anyhow::{Context, bail, ensure};
use async_channel::Sender;
use hyperclix_core::{
    Account, ConnectOptions, Result, Server, ServerOptions, Session, discovery,
    proximity::{BleDevice, BleListenerOptions, NearbyScanner, UpgradeClient},
};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};

use super::Event;

pub struct Background {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Background {
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    pub async fn close(self) -> Result<()> {
        self.stop();
        self.task.await.context("等待近场后台退出")?;
        Ok(())
    }
}

pub fn start(
    account: Account,
    interface: String,
    name: String,
    generation: u64,
    tx: Sender<Event>,
    runtime: &Runtime,
) -> Background {
    let (stop, stopped) = watch::channel(false);
    let task = runtime.spawn(async move {
        let result = listen(account, interface, name, generation, &tx, stopped).await;
        if let Err(error) = &result {
            crate::logging::write(&format!(
                "[nearby-ui] background stopped with error: {error:#}"
            ));
        }
        let _ = tx.send(Event::NearbyStopped(generation, result)).await;
    });
    Background { stop, task }
}

async fn status(tx: &Sender<Event>, generation: u64, text: &str) {
    crate::logging::write(&format!("[nearby-ui] {text}"));
    let _ = tx
        .send(Event::NearbyStatus(generation, text.to_owned()))
        .await;
}

async fn listen(
    account: Account,
    interface: String,
    name: String,
    generation: u64,
    tx: &Sender<Event>,
    mut stopped: watch::Receiver<bool>,
) -> Result<()> {
    status(tx, generation, "正在准备近场网络，可能需要系统授权…").await;
    let local_ip = discovery::interfaces()?
        .into_iter()
        .find(|item| item.name == interface)
        .map(|item| item.address);
    if let Some(ip) = local_ip {
        status(
            tx,
            generation,
            &format!("准备局域网 KCP 与 mDNS 公告：{interface} {ip}"),
        )
        .await;
    } else {
        status(
            tx,
            generation,
            &format!("{interface} 没有 IPv4，仅启动 BLE 后台；手机将无法通过局域网发现本机"),
        )
        .await;
    }
    let upgrader = match UpgradeClient::new(&interface).await {
        Ok(upgrader) => Some(upgrader),
        Err(error) => {
            crate::logging::write(&format!(
                "[nearby-ui] Wi-Fi Direct backend unavailable: {error:#}"
            ));
            status(
                tx,
                generation,
                &format!("Wi-Fi Direct 不可用：{error:#}；继续提供 LAN/BLE 接入，但手机主动 BLE 连接无法升级"),
            )
            .await;
            None
        }
    };
    if *stopped.borrow() {
        return Ok(());
    }
    let connection = ConnectOptions {
        device_name: name,
        wifi_capabilities: upgrader.as_ref().map(UpgradeClient::capabilities),
        ..Default::default()
    };
    let mut server = Server::start(
        &account,
        ServerOptions {
            local_ip: local_ip.unwrap_or(Ipv4Addr::UNSPECIFIED),
            ble: Some(BleListenerOptions {
                name: connection.device_name.clone(),
                ..Default::default()
            }),
            connection,
            ..Default::default()
        },
    )
    .await?;
    status(
        tx,
        generation,
        if local_ip.is_some() {
            "BLE + LAN 后台在线，等待手机主动连接"
        } else {
            "BLE 后台在线（无 LAN 公告），等待手机主动连接"
        },
    )
    .await;
    let result = async {
        loop {
            if *stopped.borrow() {
                break;
            }
            let session = tokio::select! {
                _ = stopped.changed() => break,
                session = server.accept() => session?,
            };
            if session.peer_ip().is_unspecified() {
                let Some(upgrader) = upgrader.as_ref() else {
                    let _ = session.close().await;
                    status(
                        tx,
                        generation,
                        "手机通过 BLE 接入，但本机没有可用的 Wi-Fi Direct helper；关闭本次连接，继续等待 LAN 连接",
                    )
                    .await;
                    continue;
                };
                status(
                    tx,
                    generation,
                    "手机已完成 BLE 认证，正在建立 Wi-Fi Direct…",
                )
                .await;
                let upgraded = tokio::select! {
                    result = upgrader.upgrade(&session, Duration::from_secs(30)) => result,
                    _ = stopped.changed() => {
                        let _ = session.close().await;
                        break;
                    },
                };
                if let Err(error) = upgraded {
                    let _ = session.close().await;
                    status(
                        tx,
                        generation,
                        &format!("手机近场连接失败：{error:#}；继续等待连接"),
                    )
                    .await;
                    continue;
                }
                if *stopped.borrow() {
                    let _ = session.close().await;
                    break;
                }
                status(tx, generation, "Wi-Fi Direct 已连接，后台继续监听").await;
            } else {
                status(
                    tx,
                    generation,
                    &format!("手机已通过局域网接入 {}，后台继续监听", session.peer_ip()),
                )
                .await;
            }
            if let Err(error) = tx.send(Event::Incoming(generation, session)).await {
                if let Event::Incoming(_, session) = error.0 {
                    let _ = session.close().await;
                }
                bail!("测试界面已关闭近场事件接收");
            }
        }
        Ok(())
    }
    .await;
    let closed = server.close().await;
    drop(upgrader);
    result.and(closed)
}

/// Managed wireless interfaces are available even without an IPv4 address.
pub fn wireless_interfaces() -> Vec<String> {
    let Ok(output) = Command::new("iw").arg("dev").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut interfaces = Vec::new();
    let mut current = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Interface ") {
            current = Some(name.to_owned());
        } else if line == "type managed" {
            if let Some(name) = current.take() {
                if !name.starts_with("xcp")
                    && !name.starts_with("xclab")
                    && !name.starts_with("p2p-")
                {
                    interfaces.push(name);
                }
            }
        }
    }
    interfaces.sort();
    interfaces.dedup();
    interfaces
}

pub async fn scan(account: &Account) -> Result<Vec<BleDevice>> {
    let mut scanner = NearbyScanner::start(account, Default::default()).await?;
    let result = scanner.wait().await;
    let closed = scanner.close().await;
    let devices = result?;
    closed?;
    Ok(devices)
}

pub async fn connect(
    account: Account,
    device: BleDevice,
    interface: String,
    name: String,
    mut cancelled: watch::Receiver<bool>,
) -> Result<Session> {
    ensure!(!*cancelled.borrow(), "连接已取消");
    crate::logging::write("[nearby-ui] preparing active BLE / P2P connection");
    let upgrader = UpgradeClient::new(&interface).await?;
    ensure!(!*cancelled.borrow(), "连接已取消");
    let options = ConnectOptions {
        device_name: name,
        wifi_capabilities: Some(upgrader.capabilities()),
        ..Default::default()
    };
    // The authentication future owns a running Physical actor. Let it complete
    // before cancellation, then explicitly close the returned session.
    let session = Session::connect_ble(&account, &device, options).await?;
    if *cancelled.borrow() {
        let _ = session.close().await;
        bail!("连接已取消");
    }
    crate::logging::write("[nearby-ui] BLE authenticated; upgrading to Wi-Fi Direct");
    let result = tokio::select! {
        result = upgrader.upgrade(&session, Duration::from_secs(30)) => result,
        _ = cancelled.changed() => Err(anyhow::anyhow!("连接已取消")),
    };
    if let Err(error) = result {
        if let Err(closed) = session.close().await {
            crate::logging::write(&format!(
                "[nearby-ui] close failed after connection error: {closed:#}"
            ));
        }
        return Err(error);
    }
    if *cancelled.borrow() {
        let _ = session.close().await;
        bail!("连接已取消");
    }
    crate::logging::write("[nearby-ui] active Wi-Fi Direct session ready");
    Ok(session)
}

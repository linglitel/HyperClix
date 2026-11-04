//! Headless host for the core's local presence and file receiver.
use anyhow::{Context, bail};
use hyperclix_core::{
    Account, ConnectOptions, DEFAULT_DEVICE_NAME, Result, Server, ServerOptions, Session,
    SessionEvent, default_account_dir, discovery,
    mishare::{MiShare, TransferEvent},
    proximity::{BleListenerOptions, NearbyScanner, UpgradeClient},
};
use std::{net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};

fn argument(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|item| item[0] == name)
        .map(|item| item[1].clone())
}
fn endpoint_for(args: &[String], local: Ipv4Addr) -> Result<Option<discovery::DeviceEndpoint>> {
    if let Some(endpoint) = argument(args, "--endpoint") {
        let (ip, port) = endpoint
            .rsplit_once(':')
            .context("--endpoint 需要 IP:端口")?;
        return Ok(Some(discovery::DeviceEndpoint::new(
            local,
            ip.parse()?,
            port.parse()?,
        )));
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--help") {
        println!(
            "hyperclix-service [--account <目录>] [--name <名称>]\n  --interface <网卡> 启动局域网后台，可加 --receive-dir <目录>\n  --listen-ble 允许手机通过 BLE 主动连接，可与局域网后台同时开启\n  --interface <网卡> --apps <手机IPv4> 读取最近应用及图标\n  --nearby 扫描 BLE，无需局域网\n  --ble-connect 认证唯一同账号近场设备；可加 --p2p-interface <无线接口> 升级链路，--recent-apps 读取最近应用，--send-file <文件> 发送文件到手机，--endpoint <IP:端口> 跳过发现直连，--reconnect 按回车关闭并重连"
        );
        return Ok(());
    }
    let log =
        hyperclix_core::logging::init(hyperclix_core::paths::logs_dir(), "HyperClix service")?;
    println!("核心服务日志：{}", log.display());
    let account = Account::load(
        argument(&args, "--account")
            .unwrap_or_else(|| default_account_dir().to_string_lossy().into_owned()),
    )
    .await?;
    let mut options = ConnectOptions {
        device_name: argument(&args, "--name").unwrap_or_else(|| DEFAULT_DEVICE_NAME.into()),
        ..Default::default()
    };
    let upgrader = if let Some(interface) = argument(&args, "--p2p-interface") {
        let upgrader = UpgradeClient::new(&interface).await?;
        options.wifi_capabilities = Some(upgrader.capabilities());
        Some(Arc::new(upgrader))
    } else {
        None
    };
    if args.iter().any(|arg| arg == "--ble-connect") {
        let reconnect = args.iter().any(|arg| arg == "--reconnect");
        let (tx, mut input) = tokio::sync::mpsc::unbounded_channel::<()>();
        if reconnect {
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines() {
                    if line.is_err() || tx.send(()).is_err() {
                        break;
                    }
                }
            });
        } else {
            drop(tx);
        }
        let mut scanner = NearbyScanner::start(&account, Default::default()).await?;
        let devices = scanner.wait().await?;
        scanner.close().await?;
        let candidates: Vec<_> = devices
            .into_iter()
            .filter(|device| device.advertisement.same_account == Some(true))
            .collect();
        anyhow::ensure!(
            candidates.len() == 1,
            "发现 {} 台同账号近场设备，需要唯一设备才能连接",
            candidates.len()
        );
        loop {
            let session = Session::connect_ble(&account, &candidates[0], options.clone()).await?;
            let result: Result<bool> = async {
                println!("BLE 同账号认证及 Networking 已完成");
                if let Some(upgrader) = &upgrader {
                    upgrader.upgrade(&session, Duration::from_secs(30)).await?;
                    println!("P2P 协商、链路认证及 SDK 通道已完成");
                }
                if args.iter().any(|arg| arg == "--recent-apps") {
                    let apps = session.recent_apps().await?;
                    println!("P2P 最近应用：{} 个，{} 个带图标", apps.len(),
                        apps.iter().filter(|app| !app.icon.is_empty()).count());
                }
                if reconnect { println!("按回车关闭当前会话并重新认证、建立 P2P；Ctrl+C 退出"); }
                let mut events = session.events();
                loop {
                    tokio::select! {
                        signal = tokio::signal::ctrl_c() => { signal?; return Ok(false); },
                        Some(()) = input.recv(), if reconnect => return Ok(true),
                        event = events.recv() => match event {
                            Ok(SessionEvent::Phase(phase)) => tracing::info!("[service] {phase}"),
                            Ok(SessionEvent::Disconnected(reason)) => bail!("手机连接已结束：{reason}"),
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => bail!("手机事件通道已关闭"),
                            _ => {},
                        }
                    }
                }
            }.await;
            let closed = session.close().await;
            let again = result?;
            closed?;
            if !again {
                return Ok(());
            }
            tracing::info!(
                "[service] previous session closed; reconnecting selected BLE device with fresh authentication and P2P negotiation"
            );
        }
    }
    if args.iter().any(|arg| arg == "--nearby") {
        let mut scanner = NearbyScanner::start(&account, Default::default()).await?;
        let devices = scanner.wait().await?;
        println!(
            "收到 {} 台小米近场设备，其中 {} 台匹配本机账号过滤器",
            devices.len(),
            devices
                .iter()
                .filter(|device| device.advertisement.same_account == Some(true))
                .count()
        );
        scanner.close().await?;
        return Ok(());
    }
    let local = if let Some(interface) = argument(&args, "--interface") {
        discovery::interfaces()?
            .into_iter()
            .find(|item| item.name == interface)
            .context("所选网卡没有 IPv4 地址")?
            .address
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    if let Some(file) = argument(&args, "--send-file") {
        anyhow::ensure!(
            !local.is_unspecified(),
            "发送文件请指定 --interface 和 --apps <手机IPv4>"
        );
        let phone: Ipv4Addr = argument(&args, "--apps")
            .context("发送文件需要 --apps <手机IPv4>")?
            .parse()?;
        let endpoint = if let Some(endpoint) = endpoint_for(&args, local)? {
            endpoint
        } else {
            discovery::discover(local, Duration::from_secs(4))
                .await?
                .into_iter()
                .find(|item| item.ip == phone)
                .context("当前发现结果中没有这台手机")?
        };
        let session = Session::connect(&account, &endpoint, options).await?;
        let task = MiShare::new(session.clone(), DEFAULT_DEVICE_NAME)
            .send_files(vec![PathBuf::from(file)]);
        let mut events = task.subscribe();
        let result = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        tracing::info!("[service] send event: {event:?}");
                        if matches!(
                            event,
                            TransferEvent::Complete(_)
                                | TransferEvent::Failed(_)
                                | TransferEvent::Cancelled
                        ) {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            task.wait().await
        })
        .await
        .context("发送文件超时")?;
        let close = session.close().await;
        let summary = result?;
        close?;
        println!(
            "发送完成：{} 个文件，{} 字节",
            summary.files.len(),
            summary.bytes
        );
        return Ok(());
    }
    if let Some(phone) = argument(&args, "--apps") {
        anyhow::ensure!(
            !local.is_unspecified(),
            "局域网读取请指定 --interface；近场读取使用 --ble-connect --p2p-interface <设备> --recent-apps"
        );
        let phone: Ipv4Addr = phone.parse()?;
        let endpoint = if let Some(endpoint) = endpoint_for(&args, local)? {
            endpoint
        } else {
            discovery::discover(local, Duration::from_secs(4))
                .await?
                .into_iter()
                .find(|item| item.ip == phone)
                .context("当前发现结果中没有这台手机")?
        };
        let session = Session::connect(&account, &endpoint, options).await?;
        let result = session.recent_apps().await;
        let close = session.close().await;
        let apps = result?;
        close?;
        println!(
            "官方协议返回 {} 个最近应用，其中 {} 个带图标",
            apps.len(),
            apps.iter().filter(|app| !app.icon.is_empty()).count()
        );
        return Ok(());
    }
    let directory = argument(&args, "--receive-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| hyperclix_core::paths::received_dir());
    let mut server = Server::start(
        &account,
        ServerOptions {
            local_ip: local,
            connection: options.clone(),
            ble: args
                .iter()
                .any(|arg| arg == "--listen-ble")
                .then(|| BleListenerOptions {
                    name: options.device_name.clone(),
                    ..Default::default()
                }),
            ..Default::default()
        },
    )
    .await?;
    let mut events = server.subscribe();
    println!(
        "核心后台在线：LAN={:?}，BLE={}；Ctrl+C 关闭",
        server.local_addr(),
        args.iter().any(|arg| arg == "--listen-ble")
    );
    let name = Arc::new(options.device_name);
    let mut receivers = tokio::task::JoinSet::new();
    let result: Result<()> = async {
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => { signal?; break; },
                event = events.recv() => match event {
                    Ok(event) => tracing::info!("[service] {event:?}"),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                    Err(_) => bail!("后台事件通道已关闭"),
                },
                Some(result) = receivers.join_next(), if !receivers.is_empty() => { if let Err(error) = result { tracing::info!("[service] receiver task: {error}"); } },
                session = server.accept() => {
                    let session = session?;
                    let name = name.clone();
                    let directory = directory.clone();
                    let upgrader = upgrader.clone();
                    receivers.spawn(async move {
                        if session.peer_ip().is_unspecified() {
                            if let Some(upgrader) = upgrader {
                                if let Err(error) = upgrader.upgrade(&session, Duration::from_secs(30)).await {
                                    tracing::info!("[service] incoming BLE P2P upgrade: {error:#}");
                                    return;
                                }
                            }
                        }
                        let share = MiShare::new(session, name.as_str());
                        loop {
                            let task = share.receive_files(directory.clone());
                            match task.wait().await {
                                Ok(files) => tracing::info!("[service] received {} files, {} bytes", files.files.len(), files.bytes),
                                Err(error) => { tracing::info!("[service] phone file receiver ended: {error:#}"); break; },
                            }
                        }
                    });
                }
            }
        }
        Ok(())
    }.await;
    let closed = server.close().await;
    while let Some(result) = receivers.join_next().await {
        let _ = result;
    }
    result.and(closed)
}

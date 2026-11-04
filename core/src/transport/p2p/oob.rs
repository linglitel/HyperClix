use super::helper::{Request, Response};
use crate::Result;
use crate::transport::upgrade::WifiGroup;
use anyhow::{Context, bail, ensure};
use std::{
    net::Ipv4Addr,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    runtime::Handle,
    sync::Mutex,
};

struct Process {
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    _child: Child,
    next_group: u64,
    group: Option<(u64, Weak<()>)>,
}

#[derive(Clone)]
pub(crate) struct OobClient {
    process: Arc<Mutex<Process>>,
    pub address: String,
}

pub(crate) struct OobLease {
    client: OobClient,
    group_id: u64,
    _owner: Arc<()>,
    active: bool,
    runtime: Handle,
}
impl OobLease {
    pub async fn close(mut self) -> Result<()> {
        let result = self.client.leave(self.group_id).await;
        self.active = false;
        result
    }
}
impl Drop for OobLease {
    fn drop(&mut self) {
        if self.active {
            let client = self.client.clone();
            let group_id = self.group_id;
            self.runtime.spawn(async move {
                let _ = client.leave(group_id).await;
            });
        }
    }
}

impl OobClient {
    pub async fn prepare(parent: &str) -> Result<Self> {
        let path = std::env::current_exe()?.with_file_name("hyperclix-p2p-helper");
        anyhow::ensure!(path.is_file(), "缺少近场网络 helper：{}", path.display());
        tracing::info!("[nearby] preparing isolated OOB P2P backend: parent={parent}");
        let mut child = Command::new("/usr/bin/pkexec")
            .arg(path)
            .args(["--parent", parent])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .process_group(0)
            .spawn()
            .context("启动授权近场网络 helper")?;
        let mut process = Process {
            input: child.stdin.take().context("helper stdin")?,
            output: BufReader::new(child.stdout.take().context("helper stdout")?).lines(),
            _child: child,
            next_group: 1,
            group: None,
        };
        let address = match receive(&mut process).await? {
            Response::Prepared { address } => address,
            Response::Failed { message, .. } => bail!("近场网络准备失败：{message}"),
            _ => bail!("近场网络 helper 未返回准备结果"),
        };
        Ok(Self {
            process: Arc::new(Mutex::new(process)),
            address,
        })
    }

    pub async fn join(
        &self,
        group: &WifiGroup,
        timeout: Duration,
    ) -> Result<(OobLease, String, Ipv4Addr)> {
        let mut guard = self.process.lock().await;
        let process = &mut *guard;
        ensure!(
            !process
                .group
                .as_ref()
                .is_some_and(|(_, owner)| owner.upgrade().is_some()),
            "已有会话正在使用此近场 P2P 组"
        );
        if let Some((previous, _)) = &process.group {
            let previous = *previous;
            tokio::time::timeout(Duration::from_secs(15), leave_process(process, previous))
                .await
                .context("等待上一次近场组退出超时")??;
        }
        let group_id = process.next_group;
        process.next_group += 1;
        let owner = Arc::new(());
        let lease = OobLease {
            client: self.clone(),
            group_id,
            _owner: owner.clone(),
            active: true,
            runtime: Handle::current(),
        };
        process.group = Some((group_id, Arc::downgrade(&owner)));
        drop(owner);
        let result = async {
            let request = Request::Join {
                group_id,
                ssid: group.ssid.clone(),
                password: group.password.clone(),
                server_mac: group.server_mac.clone(),
                channel: group.channel,
                client_ip: group.client_ip,
                endpoint_ip: *group.endpoint.ip(),
                timeout_seconds: timeout.as_secs(),
            };
            let mut bytes = serde_json::to_vec(&request)?;
            bytes.push(b'\n');
            process.input.write_all(&bytes).await?;
            process.input.flush().await?;
            tracing::info!("[nearby] P2P helper join requested: group_id={group_id}");
            loop {
                match receive(process).await? {
                    Response::Joined {
                        group_id: received,
                        interface,
                        address,
                    } if received == group_id => return Ok((interface, address)),
                    Response::Failed {
                        group_id: received,
                        message,
                    } if received == group_id || received == 0 => bail!("P2P 入组失败：{message}"),
                    _ => {}
                }
            }
        };
        let result = tokio::time::timeout(timeout, result).await;
        drop(guard);
        match result {
            Ok(Ok((interface, address))) => Ok((lease, interface, address)),
            result => {
                let error = match result {
                    Ok(Err(error)) => error,
                    Err(error) => anyhow::Error::new(error).context("P2P 入组超时"),
                    _ => unreachable!(),
                };
                let _ = lease.close().await;
                Err(error)
            }
        }
    }

    async fn leave(&self, group_id: u64) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut guard = self.process.lock().await;
            let process = &mut *guard;
            leave_process(process, group_id).await
        })
        .await
        .context("等待近场自有组清理超时")?
    }
}

async fn leave_process(process: &mut Process, group_id: u64) -> Result<()> {
    let mut bytes = serde_json::to_vec(&Request::Leave { group_id })?;
    bytes.push(b'\n');
    process.input.write_all(&bytes).await?;
    process.input.flush().await?;
    loop {
        match receive(process).await? {
            Response::Left { group_id: received } if received == group_id => {
                if process
                    .group
                    .as_ref()
                    .is_some_and(|(id, _)| *id == group_id)
                {
                    process.group = None;
                }
                tracing::info!("[nearby] P2P helper group released: group_id={group_id}");
                return Ok(());
            }
            Response::Failed {
                group_id: received,
                message,
            } if received == 0 => bail!("P2P helper 已退出：{message}"),
            _ => {}
        }
    }
}

async fn receive(process: &mut Process) -> Result<Response> {
    let line = process
        .output
        .next_line()
        .await?
        .context("近场网络 helper 已退出")?;
    serde_json::from_str(&line).context("读取近场 helper 响应")
}

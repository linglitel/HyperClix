//! Privileged entry point for the isolated, pipe-owned near-field link.

use crate::Result;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    net::Ipv4Addr,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

mod state;
mod wpa;
use state::State;

#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Join {
        group_id: u64,
        ssid: String,
        password: String,
        server_mac: String,
        channel: u32,
        client_ip: Option<Ipv4Addr>,
        endpoint_ip: Ipv4Addr,
        timeout_seconds: u64,
    },
    Leave {
        group_id: u64,
    },
}

#[derive(Serialize, Deserialize)]
pub(super) enum Response {
    Prepared {
        address: String,
    },
    Joined {
        group_id: u64,
        interface: String,
        address: Ipv4Addr,
    },
    Failed {
        group_id: u64,
        message: String,
    },
    Left {
        group_id: u64,
    },
}

static STOP: AtomicBool = AtomicBool::new(false);
static GROUP_STOP: AtomicBool = AtomicBool::new(false);
static ACTIVE_GROUP: AtomicU64 = AtomicU64::new(0);
static CANCELLED_THROUGH: AtomicU64 = AtomicU64::new(0);
extern "C" fn interrupted(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
    GROUP_STOP.store(true, Ordering::Relaxed);
}

/// Used only by the separate privileged helper executable.
pub fn run() -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "近场 helper 需要系统授权");
    unsafe {
        libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, interrupted as *const () as libc::sighandler_t);
        libc::signal(
            libc::SIGTERM,
            interrupted as *const () as libc::sighandler_t,
        );
    }
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 3 && args[1] == "--parent",
        "用法：hyperclix-p2p-helper --parent <无线接口>"
    );
    let parent = &args[2];
    ensure!(
        parent
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
        "无线接口名称无效"
    );
    ensure!(
        Path::new(&format!("/sys/class/net/{parent}/phy80211")).exists(),
        "指定接口不是无线接口"
    );

    let stop = &STOP;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let request = line.map_err(anyhow::Error::from).and_then(|line| {
                serde_json::from_str::<Request>(&line).context("helper 请求格式无效")
            });
            match &request {
                Ok(Request::Leave { group_id }) => {
                    CANCELLED_THROUGH.fetch_max(*group_id, Ordering::Relaxed);
                    if ACTIVE_GROUP.load(Ordering::Relaxed) == *group_id {
                        GROUP_STOP.store(true, Ordering::Relaxed);
                    }
                }
                Ok(Request::Join { .. }) => {}
                Err(_) => {}
            }
            if send.send(request).is_err() {
                break;
            }
        }
        STOP.store(true, Ordering::Relaxed);
        GROUP_STOP.store(true, Ordering::Relaxed);
    });

    let result = (|| {
        let mut state = State::prepare(parent, &stop)?;
        respond(&Response::Prepared {
            address: state.address.clone(),
        })?;
        while !stop.load(Ordering::Relaxed) {
            let request = match receive.recv_timeout(Duration::from_millis(250)) {
                Ok(request) => request?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = state.renew(&GROUP_STOP) {
                        if !GROUP_STOP.load(Ordering::Relaxed) {
                            let group_id = state.group_id;
                            state.leave()?;
                            respond(&Response::Failed {
                                group_id,
                                message: format!("{error:#}"),
                            })?;
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match request {
                Request::Join {
                    group_id,
                    ssid,
                    password,
                    server_mac,
                    channel,
                    client_ip,
                    endpoint_ip,
                    timeout_seconds,
                } => {
                    state.leave()?;
                    state.group_id = group_id;
                    GROUP_STOP.store(false, Ordering::Relaxed);
                    ACTIVE_GROUP.store(group_id, Ordering::Relaxed);
                    if CANCELLED_THROUGH.load(Ordering::Relaxed) >= group_id {
                        GROUP_STOP.store(true, Ordering::Relaxed);
                    }
                    if let Err(error) = state.join(
                        &ssid,
                        &password,
                        &server_mac,
                        channel,
                        client_ip,
                        endpoint_ip,
                        Duration::from_secs(timeout_seconds),
                        &GROUP_STOP,
                    ) {
                        state.leave()?;
                        if !GROUP_STOP.load(Ordering::Relaxed) {
                            respond(&Response::Failed {
                                group_id,
                                message: format!("{error:#}"),
                            })?;
                        }
                    }
                }
                Request::Leave { group_id } => {
                    if state.group_id == group_id {
                        state.leave()?;
                    }
                    respond(&Response::Left { group_id })?;
                }
            }
        }
        Ok(())
    })();
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    if let Err(error) = &result {
        let _ = respond(&Response::Failed {
            group_id: 0,
            message: format!("{error:#}"),
        });
    }
    result
}

fn respond(response: &Response) -> Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, response)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

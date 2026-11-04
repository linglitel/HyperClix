use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, Shutdown, TcpStream},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, ensure};
use openssl::ssl::{SslAcceptor, SslMethod, SslVersion};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

use crate::{
    Result,
    protocol::messages::files::{ResourceEntry, resource_list},
    session::Session,
};

struct Resource {
    file: File,
    name: String,
    size: u64,
    complete: AtomicBool,
}

struct Shared {
    files: BTreeMap<String, Resource>,
    sockets: Mutex<BTreeMap<u64, TcpStream>>,
    bytes: AtomicU64,
    completed: AtomicUsize,
    screen_id: u32,
}

pub(crate) struct ResourceServer {
    shared: Arc<Shared>,
    port: u16,
    address: Ipv4Addr,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl ResourceServer {
    pub(crate) async fn start(
        session: &Session,
        screen_id: u32,
        paths: Vec<PathBuf>,
    ) -> Result<Self> {
        let mut files = BTreeMap::new();
        for path in paths {
            let file = File::open(&path).context("打开拖入文件")?;
            let metadata = file.metadata()?;
            ensure!(metadata.is_file(), "拖入 App 当前需要普通文件");
            let name = path
                .file_name()
                .context("拖入文件没有名称")?
                .to_string_lossy()
                .into_owned();
            files.insert(
                uuid::Uuid::new_v4().simple().to_string(),
                Resource {
                    file,
                    name,
                    size: metadata.len(),
                    complete: AtomicBool::new(false),
                },
            );
        }
        let listener = TcpListener::bind((session.local_ip(), 0)).await?;
        let port = listener.local_addr()?.port();
        let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
        tls.set_min_proto_version(Some(SslVersion::TLS1_2))?;
        tls.set_certificate(&session.account().cert3)?;
        tls.set_private_key(&session.account().pkey)?;
        tls.add_extra_chain_cert(session.account().cert2.clone())?;
        tls.check_private_key()?;
        let tls = Arc::new(tls.build());
        let shared = Arc::new(Shared {
            files,
            sockets: Mutex::new(BTreeMap::new()),
            bytes: AtomicU64::new(0),
            completed: AtomicUsize::new(0),
            screen_id,
        });
        let (stop, mut stopped) = watch::channel(false);
        let peer_ip = session.peer_ip();
        let state = shared.clone();
        let task = tokio::spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    accepted = listener.accept() => {
                        let (socket, peer) = accepted?;
                        if peer.ip() != std::net::IpAddr::V4(peer_ip) { continue; }
                        let socket = socket.into_std()?;
                        socket.set_nonblocking(false)?;
                        socket.set_read_timeout(Some(Duration::from_secs(30)))?;
                        socket.set_write_timeout(Some(Duration::from_secs(30)))?;
                        let id = rand::random::<u64>();
                        state.sockets.lock().unwrap().insert(id, socket.try_clone()?);
                        let state = state.clone();
                        let tls = tls.clone();
                        requests.spawn_blocking(move || {
                            let result = serve(socket, &tls, &state);
                            state.sockets.lock().unwrap().remove(&id);
                            if let Err(error) = result {
                                tracing::info!("[app-drop screen={}] resource request failed: {error:#}", state.screen_id);
                            }
                        });
                    }
                    Some(result) = requests.join_next() => { result?; }
                }
            }
            drop(listener);
            while let Some(result) = requests.join_next().await {
                result?;
            }
            Ok(())
        });
        tracing::info!(
            "[app-drop screen={screen_id}] resource server started: port={port} files={}",
            shared.files.len()
        );
        Ok(Self {
            shared,
            port,
            address: session.local_ip(),
            stop,
            task: Some(task),
        })
    }

    pub(crate) fn clip_data(&self) -> Vec<u8> {
        let entries: Vec<ResourceEntry> = self
            .shared
            .files
            .iter()
            .map(|(id, file)| ResourceEntry {
                url: format!(
                    "https://{}:{}/api/1.0/file?id={id}",
                    self.address, self.port
                ),
                name: file.name.clone(),
                size: file.size,
            })
            .collect();
        resource_list(&entries)
    }

    pub(super) fn counts(&self) -> (usize, usize, u64) {
        (
            self.shared.files.len(),
            self.shared.completed.load(Ordering::Relaxed),
            self.shared.bytes.load(Ordering::Relaxed),
        )
    }

    pub(crate) async fn close(mut self, cancel: bool) -> Result<(usize, usize, u64)> {
        if cancel {
            self.disconnect();
        }
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(self.counts())
    }

    fn disconnect(&self) {
        for socket in self.shared.sockets.lock().unwrap().values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for ResourceServer {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.disconnect();
            self.stop.send_replace(true);
        }
    }
}

fn response(stream: &mut impl Write, status: &str, headers: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n"
    )?;
    Ok(())
}

fn serve(socket: TcpStream, tls: &SslAcceptor, shared: &Shared) -> Result<()> {
    let tls = tls.accept(socket).context("拖入文件 TLS 握手")?;
    let mut stream = BufReader::new(tls);
    let mut request = String::new();
    stream.read_line(&mut request)?;
    let mut parts = request.split_whitespace();
    let method = parts.next().context("缺少资源请求方法")?;
    let target = parts.next().context("缺少资源请求目标")?;
    let url = reqwest::Url::parse(&format!("http://localhost{target}"))?;
    let mut range = None;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_owned());
            }
        }
    }
    if method != "GET" && method != "HEAD" {
        return response(
            stream.get_mut(),
            "405 Method Not Allowed",
            "Content-Length: 0\r\n",
        );
    }
    let id = url
        .query_pairs()
        .find_map(|(key, value)| (key == "id").then(|| value.into_owned()));
    let file = id.as_ref().and_then(|id| shared.files.get(id));
    let Some(file) = file.filter(|_| url.path() == "/file" || url.path() == "/api/1.0/file") else {
        return response(stream.get_mut(), "404 Not Found", "Content-Length: 0\r\n");
    };
    let (start, length, partial) = if let Some(range) = range {
        let range = range
            .strip_prefix("bytes=")
            .context("不支持的文件 Range 单位")?;
        let (start, end) = range.split_once('-').context("不完整的文件 Range")?;
        let (start, end) = if start.is_empty() {
            (
                file.size.saturating_sub(end.parse::<u64>()?),
                file.size.saturating_sub(1),
            )
        } else {
            (
                start.parse::<u64>()?,
                if end.is_empty() {
                    file.size.saturating_sub(1)
                } else {
                    end.parse::<u64>()?
                },
            )
        };
        if start >= file.size || end < start || end >= file.size {
            return response(
                stream.get_mut(),
                "416 Range Not Satisfiable",
                &format!(
                    "Content-Length: 0\r\nContent-Range: bytes */{}\r\n",
                    file.size
                ),
            );
        }
        (start, end - start + 1, true)
    } else {
        (0, file.size, false)
    };
    let mut headers = format!(
        "Content-Length: {length}\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\n"
    );
    if partial {
        headers.push_str(&format!(
            "Content-Range: bytes {start}-{}/{}\r\n",
            start + length - 1,
            file.size
        ));
    }
    response(
        stream.get_mut(),
        if partial {
            "206 Partial Content"
        } else {
            "200 OK"
        },
        &headers,
    )?;
    if method == "HEAD" {
        return Ok(());
    }
    let mut buffer = [0; 64 * 1024];
    let mut sent = 0;
    while sent < length {
        let wanted = (length - sent).min(buffer.len() as u64) as usize;
        let count = file.file.read_at(&mut buffer[..wanted], start + sent)?;
        ensure!(count != 0, "拖入文件在传输期间缩短");
        stream.get_mut().write_all(&buffer[..count])?;
        sent += count as u64;
        shared.bytes.fetch_add(count as u64, Ordering::Relaxed);
    }
    stream.get_mut().flush()?;
    if start == 0 && length == file.size && !file.complete.swap(true, Ordering::AcqRel) {
        shared.completed.fetch_add(1, Ordering::Relaxed);
    }
    tracing::info!(
        "[app-drop screen={}] resource served: bytes={sent} completed_files={}/{}",
        shared.screen_id,
        shared.completed.load(Ordering::Relaxed),
        shared.files.len()
    );
    Ok(())
}

//! HyperClix run logs: structured application events and raw native stderr.
use std::{
    fs::{self, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

static PATH: OnceLock<PathBuf> = OnceLock::new();

const APP_KEEP: usize = 10;
const NATIVE_KEEP: usize = 5;

/// Initialize structured app logging and redirect native stderr to a separate
/// file. Safe to call once per process; later calls return the same path.
pub fn init(directory: impl AsRef<Path>, application: &str) -> anyhow::Result<PathBuf> {
    if let Some(path) = PATH.get() {
        return Ok(path.clone());
    }
    let directory = directory.as_ref();
    fs::create_dir_all(directory)
        .with_context(|| format!("创建日志目录 {}", directory.display()))?;
    prune(directory, "app-", APP_KEEP);
    prune(directory, "native-", NATIVE_KEEP);

    let pid = std::process::id();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0);
    let app_path = directory.join(format!("app-{stamp}-{pid}.log"));
    let native_path = directory.join(format!("native-{stamp}-{pid}.log"));

    let native = OpenOptions::new()
        .create_new(true)
        .append(true)
        .mode(0o600)
        .open(&native_path)
        .with_context(|| format!("创建 native 日志 {}", native_path.display()))?;
    // Native GTK/Mesa/libav/ALSA diagnostics go here, never into app log.
    if unsafe { libc::dup2(native.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error()).context("重定向 native stderr");
    }

    let writer = tracing_appender::rolling::never(directory, &app_path.file_name().unwrap());
    let filter = EnvFilter::try_from_env("HYPERCLIX_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_thread_ids(false)
        .with_thread_names(false);
    let console = fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .with_target(false)
        .with_filter(EnvFilter::new("warn"));
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console)
        .try_init()
        .context("初始化 tracing 日志")?;

    let _ = PATH.set(app_path.clone());
    tracing::info!(
        target: "app",
        application,
        version = env!("CARGO_PKG_VERSION"),
        pid,
        native = %native_path.display(),
        "HyperClix started"
    );
    Ok(app_path)
}

pub fn path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

/// UI-originated line; core and UI both use the same structured writer.
pub fn write(text: &str) {
    tracing::info!(target: "ui", "{text}");
}

pub fn error(text: &str) {
    tracing::error!(target: "ui", "{text}");
}

fn prune(directory: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(prefix) || !name.ends_with(".log") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    logs.sort_by_key(|(modified, _)| *modified);
    for (_, path) in logs.into_iter().rev().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

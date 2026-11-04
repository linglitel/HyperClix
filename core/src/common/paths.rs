//! XDG base directories used by HyperClix.
use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("hyperclix")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("hyperclix")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("hyperclix")
}

pub fn account_dir() -> PathBuf {
    data_dir().join("account-session")
}

pub fn cloud_dir() -> PathBuf {
    data_dir().join("account-micloud")
}

pub fn push_path() -> PathBuf {
    data_dir().join("push").join("runtime.private.json")
}

pub fn cloud_state_dir() -> PathBuf {
    data_dir().join("cloud")
}

pub fn received_dir() -> PathBuf {
    data_dir().join("received")
}

pub fn clipboard_cache_dir() -> PathBuf {
    cache_dir().join("clipboard")
}

pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

//! Thin UI binding for HyperClix structured logging.
use std::path::Path;

pub fn start(directory: impl AsRef<Path>) -> anyhow::Result<()> {
    let path = hyperclix_core::logging::init(directory, "HyperClix UI")?;
    println!("运行日志保存至 {}", path.display());
    Ok(())
}

pub fn path() -> Option<&'static Path> {
    hyperclix_core::logging::path()
}

pub fn write(text: &str) {
    hyperclix_core::logging::write(text);
}

pub fn error(text: &str) {
    hyperclix_core::logging::error(text);
}

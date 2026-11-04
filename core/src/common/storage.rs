//! Private file writes. Callers decide the content and destination.
use crate::Result;
use std::path::Path;

pub(crate) async fn save_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(data).await?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

pub(crate) fn json_path(path: &std::path::Path, name: &str) -> std::path::PathBuf {
    if path.is_dir() {
        path.join(name)
    } else {
        path.to_path_buf()
    }
}

pub(crate) async fn private_json(
    path: &std::path::Path,
    value: &serde_json::Value,
) -> crate::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    save_private(path, &bytes).await
}

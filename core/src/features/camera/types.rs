//! Public camera option and negotiated media value types.
use crate::Result;
use anyhow::{Context, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::{net::Ipv4Addr, time::Duration};

#[derive(Clone, Debug)]
pub struct CameraOptions {
    pub camera_id: u32,
    pub width: u32,
    pub height: u32,
    pub hardware_index: usize,
    pub timeout: Duration,
}
impl Default for CameraOptions {
    fn default() -> Self {
        Self {
            camera_id: 0,
            width: 1280,
            height: 720,
            hardware_index: 0,
            timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CameraStream {
    #[serde(rename = "type")]
    pub kind: u32,
    pub width: u32,
    pub height: u32,
    pub origin_width: u32,
    pub origin_height: u32,
}
impl CameraStream {
    pub fn preview(width: u32, height: u32) -> Self {
        Self {
            kind: 1,
            width,
            height,
            origin_width: width,
            origin_height: height,
        }
    }
}

/// The key is deliberately excluded from `Debug` and logs.
#[derive(Clone)]
pub struct CameraMediaConfig {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub key: [u8; 16],
    pub video_format: Json,
    pub output_streams: Vec<CameraStream>,
}
impl std::fmt::Debug for CameraMediaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraMediaConfig")
            .field("ip", &self.ip)
            .field("port", &self.port)
            .field("video_format", &self.video_format)
            .field("output_streams", &self.output_streams)
            .finish_non_exhaustive()
    }
}
impl CameraMediaConfig {
    pub fn from_result(result: &Json) -> Result<Self> {
        let raw = STANDARD.decode(
            result["encryptKey"]
                .as_str()
                .context("Camera configure has no encryptKey")?,
        )?;
        let key = raw
            .try_into()
            .map_err(|_| anyhow!("Camera configure key must be AES-128"))?;
        let port = u16::try_from(
            result["port"]
                .as_u64()
                .context("Camera configure has no port")?,
        )?;
        anyhow::ensure!(port != 0, "Camera configure returned port zero");
        Ok(Self {
            ip: result["ip"]
                .as_str()
                .context("Camera configure has no IP")?
                .parse()?,
            port,
            key,
            video_format: result["videoFormat"].clone(),
            output_streams: serde_json::from_value(
                result.get("outputStreams").cloned().unwrap_or(json!([])),
            )?,
        })
    }
}

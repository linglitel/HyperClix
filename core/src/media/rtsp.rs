//! RTSP message parsing and WFD RTSP request helpers.
use crate::Result;
use std::collections::BTreeMap;
use tokio::{io::AsyncWriteExt, net::TcpStream};

pub(super) struct RtspMessage {
    pub(super) start: String,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) body: String,
}
pub(super) fn parse_rtsp(buffer: &mut Vec<u8>) -> Result<Option<RtspMessage>> {
    let Some(head) = buffer.windows(4).position(|b| b == b"\r\n\r\n") else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&buffer[..head])?;
    let mut lines = text.split("\r\n");
    let start = lines.next().unwrap_or("").to_string();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        })
        .collect();
    let length = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    if buffer.len() < head + 4 + length {
        return Ok(None);
    }
    let body = String::from_utf8_lossy(&buffer[head + 4..head + 4 + length]).into_owned();
    buffer.drain(..head + 4 + length);
    Ok(Some(RtspMessage {
        start,
        headers,
        body,
    }))
}
pub(super) async fn rtsp_send(
    socket: &mut TcpStream,
    start: &str,
    headers: &[(&str, String)],
    body: &str,
) -> Result<()> {
    let mut message = format!("{start}\r\n");
    for (key, value) in headers {
        message.push_str(&format!("{key}: {value}\r\n"));
    }
    if !body.is_empty() {
        message.push_str("Content-Type: text/parameters\r\n");
    }
    message.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    socket.write_all(message.as_bytes()).await?;
    Ok(())
}
pub(super) async fn rtsp_request(
    socket: &mut TcpStream,
    sequence: &mut u32,
    sent: &mut BTreeMap<u32, String>,
    method: &str,
    url: &str,
    extra: Vec<(&str, String)>,
) -> Result<()> {
    *sequence += 1;
    sent.insert(*sequence, method.into());
    let mut headers = vec![("CSeq", sequence.to_string())];
    headers.extend(extra);
    rtsp_send(socket, &format!("{method} {url} RTSP/1.0"), &headers, "").await
}

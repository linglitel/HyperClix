//! WFD RTSP/RTP message handling and capability advertisement.
use super::super::rtsp::{rtsp_request, rtsp_send};
use super::{Clock, Control};
use crate::Result;
use anyhow::{Context, ensure};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::mpsc};

pub(super) fn wfd_capabilities(port: u16, key: Option<[u8; 16]>) -> BTreeMap<&'static str, String> {
    let mut caps = BTreeMap::from([
        ("wfd_audio_codecs", "AAC 00000001 00".to_string()),
        ("wfd_audio_codecs_v2", "0 2 0 0".into()),
        (
            "wfd_video_formats",
            "28 00 03 1f 00000020 00000000 00000000 00 0000 0000 00 none none".into(),
        ),
        (
            "wfd_client_rtp_ports",
            format!("RTP/AVP/UDP;unicast {port} 0 mode=play"),
        ),
        ("wfd_content_protection", "none".into()),
        ("wfd_display_edid", "none".into()),
        ("wfd_coupled_sink", "00 none".into()),
        ("wfd_connector_type", "05".into()),
        ("wfd_uibc_capability", "none".into()),
        ("wfd_uibc_setting", "disable".into()),
        ("wfd_standby_resume_capability", "none".into()),
        ("wfd_3d_video_formats", "none".into()),
    ]);
    if key.is_some() {
        caps.insert(
            "wfd_content_SP_protection",
            crate::media::camera_ts::WFD_PROTECTION.into(),
        );
    }
    caps
}

pub(super) async fn handle_rtp_packet(
    packet: &[u8],
    payload: &[u8],
    arrival: Instant,
    camera: &mut Option<crate::media::camera_ts::PreviewTsDecoder>,
    control: &Arc<Control>,
    tx: &mpsc::Sender<Vec<u8>>,
    rtp_sequence: &mut RtpSequence,
    last_rtp_arrival: &mut Option<Instant>,
) -> Result<()> {
    let interarrival_ms = last_rtp_arrival
        .replace(arrival)
        .map(|previous| arrival.duration_since(previous).as_secs_f64() * 1000.0);
    let (gaps, late) = rtp_sequence.observe(packet);
    if rtp_sequence.take_stale(arrival) {
        return Ok(());
    }
    let source_reset = rtp_sequence.take_reset();
    {
        let mut shared = control.shared.lock().unwrap();
        shared.stats.rtp_packets += 1;
        if let Some(elapsed_ms) = interarrival_ms {
            shared.stats.rtp_max_interarrival_ms =
                shared.stats.rtp_max_interarrival_ms.max(elapsed_ms);
        }
        shared.stats.rtp_sequence_gaps += gaps;
        shared.stats.rtp_late_packets += late;
    }
    if source_reset {
        let ssrc = u32::from_be_bytes(packet[8..12].try_into().unwrap());
        let timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        tracing::info!(
            "[media={:p}] RTP source/timestamp discontinuity ssrc={ssrc:#010x} ts={timestamp}; resetting video clock and queue",
            Arc::as_ptr(&control)
        );
        control.recover_video("RTP source/timestamp discontinuity");
        let mut shared = control.shared.lock().unwrap();
        shared.clock.initialized = false;
        shared.frames.clear();
    } else if gaps > 0 {
        control.recover_video("RTP sequence gap; compressed reference data may be incomplete");
    }
    if payload.first() != Some(&0x47) || payload.len() % 188 != 0 {
        return Ok(());
    }
    {
        let mut shared = control.shared.lock().unwrap();
        shared.stats.received_bytes += payload.len() as u64;
        if camera.is_none()
            && !shared.clock.initialized
            && let Some(pts) = video_pes_pts(payload)
        {
            shared.clock = Clock {
                pts,
                at: arrival,
                initialized: true,
                audio_master: false,
            };
            tracing::info!(
                "[media={:p}] live video clock anchored at first PES reception: pts={pts:.6}s",
                Arc::as_ptr(&control)
            );
        }
    }
    if let Some(decoder) = camera.as_mut() {
        let mut resuming = !control.is_recovering_video();
        for es in decoder.feed(payload)? {
            if !resuming {
                if !h264_has_keyframe(&es) {
                    continue;
                }
                tracing::info!(
                    "[media={:p}] camera switch IDR received; resuming video",
                    Arc::as_ptr(&control)
                );
                resuming = true;
            }
            match tx.try_send(es) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    control.recover_video("camera decoder queue full; dropping frame");
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    anyhow::bail!("camera decoder stopped");
                }
            }
        }
    } else {
        match tx.try_send(payload.to_vec()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                control.recover_video("video decoder queue full; dropping frame");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                anyhow::bail!("video decoder stopped");
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_rtsp(
    socket: &mut TcpStream,
    message: crate::media::rtsp::RtspMessage,
    caps: &BTreeMap<&str, String>,
    port: u16,
    rtcp_port: u16,
    url: &mut String,
    sequence: &mut u32,
    sent: &mut BTreeMap<u32, String>,
    session: &mut Option<String>,
    options_sent: &mut bool,
    setup_sent: &mut bool,
    control: &Arc<Control>,
) -> Result<()> {
    if message.start.starts_with("RTSP/") {
        let status = message.start.split_whitespace().nth(1).unwrap_or("");
        let cseq = message
            .headers
            .get("cseq")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        if sent
            .get(&cseq)
            .is_some_and(|method| method == "SET_PARAMETER")
        {
            tracing::info!(
                "[media={:p}] RTSP keyframe response: cseq={cseq} status={status}",
                Arc::as_ptr(&control)
            );
            return Ok(()); // Optional IDR rejection does not close a playing stream.
        }
        ensure!(status == "200", "RTSP rejected: {}", message.start);
        if sent.get(&cseq).is_some_and(|v| v == "SETUP") {
            *session = Some(
                message
                    .headers
                    .get("session")
                    .context("RTSP SETUP omitted Session")?
                    .split(';')
                    .next()
                    .unwrap()
                    .to_string(),
            );
            rtsp_request(
                socket,
                sequence,
                sent,
                "PLAY",
                &url,
                vec![("Session", session.clone().unwrap())],
            )
            .await?;
        }
        if sent.get(&cseq).is_some_and(|method| method == "PLAY") {
            control.recover_video("RTSP PLAY started; requesting complete video parameter sets");
        }
        return Ok(());
    }
    let method = message.start.split_whitespace().next().unwrap_or("");
    let mut headers = vec![(
        "CSeq",
        message
            .headers
            .get("cseq")
            .cloned()
            .unwrap_or_else(|| "0".into()),
    )];
    match method {
        "OPTIONS" => {
            headers.push((
                "Public",
                "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER".into(),
            ));
            rtsp_send(socket, "RTSP/1.0 200 OK", &headers, "").await?;
            if !*options_sent {
                rtsp_request(
                    socket,
                    sequence,
                    sent,
                    "OPTIONS",
                    "*",
                    vec![("Require", "org.wfa.wfd1.0".into())],
                )
                .await?;
                *options_sent = true;
            }
        }
        "GET_PARAMETER" => {
            let mut body = String::new();
            for line in message.body.lines() {
                let name = line.split(':').next().unwrap_or("").trim();
                if let Some(value) = caps.get(name) {
                    body.push_str(&format!("{name}: {value}\r\n"));
                }
            }
            rtsp_send(socket, "RTSP/1.0 200 OK", &headers, &body).await?;
        }
        "SET_PARAMETER" => {
            let values: BTreeMap<_, _> = message
                .body
                .lines()
                .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim(), v.trim())))
                .collect();
            if let Some(value) = values.get("wfd_presentation_URL") {
                *url = value
                    .split_whitespace()
                    .next()
                    .context("empty presentation URL")?
                    .into();
            }
            rtsp_send(socket, "RTSP/1.0 200 OK", &headers, "").await?;
            if values.get("wfd_trigger_method") == Some(&"SETUP") && !*setup_sent {
                rtsp_request(
                    socket,
                    sequence,
                    sent,
                    "SETUP",
                    &url,
                    vec![(
                        "Transport",
                        format!("RTP/AVP/UDP;unicast;client_port={port}-{rtcp_port}"),
                    )],
                )
                .await?;
                *setup_sent = true;
            }
        }
        _ => rtsp_send(socket, "RTSP/1.0 200 OK", &headers, "").await?,
    }
    Ok(())
}

fn video_pes_pts(payload: &[u8]) -> Option<f64> {
    for ts in payload.chunks_exact(188) {
        if ts[0] != 0x47 || ts[1] & 0xc0 != 0x40 || ts[3] & 0xc0 != 0 {
            continue;
        }
        let adaptation = (ts[3] >> 4) & 3;
        if adaptation & 1 == 0 {
            continue;
        }
        let offset = 4 + if adaptation & 2 != 0 {
            1 + ts[4] as usize
        } else {
            0
        };
        let Some(pes) = ts.get(offset..) else {
            continue;
        };
        if pes.len() < 14
            || pes[..3] != [0, 0, 1]
            || !(0xe0..=0xef).contains(&pes[3])
            || pes[6] & 0xc0 != 0x80
            || pes[7] & 0x80 == 0
            || pes[8] < 5
            || pes[9] >> 4 != pes[7] >> 6
            || pes[9] & 1 == 0
            || pes[11] & 1 == 0
            || pes[13] & 1 == 0
        {
            continue;
        }
        let ticks = ((u64::from(pes[9]) & 0x0e) << 29)
            | (u64::from(pes[10]) << 22)
            | ((u64::from(pes[11]) & 0xfe) << 14)
            | (u64::from(pes[12]) << 7)
            | (u64::from(pes[13]) >> 1);
        return Some(ticks as f64 / 90000.0);
    }
    None
}
pub(super) fn rtp_payload(packet: &[u8]) -> Result<&[u8]> {
    ensure!(
        packet.len() >= 12 && packet[0] >> 6 == 2,
        "invalid RTP header"
    );
    let mut offset = 12 + (packet[0] & 15) as usize * 4;
    if packet[0] & 16 != 0 {
        ensure!(packet.len() >= offset + 4, "truncated RTP extension");
        offset += 4 + u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize * 4;
    }
    let padding = if packet[0] & 32 != 0 {
        *packet.last().unwrap() as usize
    } else {
        0
    };
    ensure!(
        offset + padding <= packet.len(),
        "invalid RTP payload length"
    );
    Ok(&packet[offset..packet.len() - padding])
}

#[derive(Default)]
pub(super) struct RtpSequence {
    source: Option<(u32, u16)>,
    timestamp: Option<u32>,
    reset: bool,
    max_timestamp: Option<u32>,
    stale_before: Option<u32>,
    stale_until: Option<Instant>,
    stale: bool,
}
impl RtpSequence {
    pub(super) fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }
    pub(super) fn take_stale(&mut self, now: Instant) -> bool {
        if self.stale_until.is_some_and(|until| now >= until) {
            self.stale_before = None;
            self.stale_until = None;
        }
        std::mem::take(&mut self.stale)
    }
    /// Called after validating the RTP header. This observes arrival order only.
    pub(super) fn observe(&mut self, packet: &[u8]) -> (u64, u64) {
        let ssrc = u32::from_be_bytes(packet[8..12].try_into().unwrap());
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        if let (Some(floor), Some(until)) = (self.stale_before, self.stale_until)
            && Instant::now() < until
            && (timestamp.wrapping_sub(floor) as i32) < 0
        {
            self.stale = true;
            return (0, 0);
        }
        self.max_timestamp = Some(match self.max_timestamp {
            Some(previous) if (timestamp.wrapping_sub(previous) as i32) < 0 => previous,
            _ => timestamp,
        });
        if let Some((previous_source, next)) = self.source {
            if previous_source == ssrc {
                if let Some(previous_timestamp) = self.timestamp {
                    let delta = timestamp.wrapping_sub(previous_timestamp) as i32;
                    if delta.unsigned_abs() > 90_000 * 2 {
                        self.reset = true;
                        self.stale_before = Some(timestamp);
                        self.stale_until = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
                self.timestamp = Some(timestamp);
                let gap = sequence.wrapping_sub(next);
                if gap >= 0x8000 {
                    return (0, 1);
                }
                self.source = Some((ssrc, sequence.wrapping_add(1)));
                return (gap as u64, 0);
            }
            self.reset = true;
            self.stale_before = Some(timestamp);
            self.stale_until = Some(Instant::now() + Duration::from_secs(3));
        }
        self.source = Some((ssrc, sequence.wrapping_add(1)));
        self.timestamp = Some(timestamp);
        (0, 0)
    }
}

fn h264_has_keyframe(data: &[u8]) -> bool {
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let Some(position) = data[offset..]
            .windows(3)
            .position(|window| window == [0, 0, 1])
        else {
            return false;
        };
        let start = offset + position + 3;
        if start < data.len() {
            let nal = data[start] & 0x1f;
            if nal == 5 || nal == 7 || nal == 8 {
                return true;
            }
        }
        offset = start;
    }
    false
}

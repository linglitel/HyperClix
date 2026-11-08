//! WFD RTSP/RTP client and RTP sequence diagnostics.
use super::{
    Clock, Control,
    rtsp::{parse_rtsp, rtsp_request, rtsp_send},
};
use crate::Result;
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpStream, UdpSocket},
    sync::mpsc,
};

mod handlers;
use handlers::{RtpSequence, handle_rtp_packet, handle_rtsp, rtp_payload, wfd_capabilities};

pub(super) async fn wfd(
    mut socket: TcpStream,
    rtp: UdpSocket,
    rtcp: UdpSocket,
    peer: Ipv4Addr,
    key: Option<[u8; 16]>,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    control: Arc<Control>,
) -> Result<()> {
    let port = rtp.local_addr()?.port();
    let rtcp_port = rtcp.local_addr()?.port();
    let caps = wfd_capabilities(port, key);
    let mut camera = key.map(|key| super::camera_ts::PreviewTsDecoder::new(key, 0));
    let mut sequence = 0;
    let mut sent = BTreeMap::new();
    let mut buffer = Vec::new();
    let mut tcp_buf = vec![0; 65536];
    let mut udp_buf = vec![0; 65536];
    let mut rtcp_buf = vec![0; 2048];
    let mut rtp_sequence = RtpSequence::default();
    let mut last_rtp_arrival: Option<Instant> = None;
    let mut url = format!(
        "rtsp://{}:{}/wfd1.0/streamid=0",
        peer,
        socket.peer_addr()?.port()
    );
    let mut session: Option<String> = None;
    let mut options_sent = false;
    let mut setup_sent = false;
    let result = async {
    loop {
        if control.stopped.load(Ordering::Acquire) {
            break;
        }
        if let Some(session) = &session {
            if control.keyframe_requested.swap(false, Ordering::AcqRel) {
                sequence += 1;
                sent.insert(sequence, "SET_PARAMETER".to_string());
                rtsp_send(
                    &mut socket,
                    &format!("SET_PARAMETER {url} RTSP/1.0"),
                    &[("CSeq", sequence.to_string()), ("Session", session.clone())],
                    "wfd_idr_request\r\n",
                )
                .await?;
                control.shared.lock().unwrap().stats.requested_keyframes += 1;
                tracing::info!(
                    "[media={:p}] RTSP keyframe requested: cseq={sequence} session={session}",
                    Arc::as_ptr(&control)
                );
            }
        }
        tokio::select! {
            _=control.wake.notified()=>{break;}
            _=control.keyframe_wake.notified()=>{continue;}
            n=socket.read(&mut tcp_buf)=> {
                let n=n?;
                if n==0 {
                    tracing::info!("[media={:p}] phone closed RTSP session", Arc::as_ptr(&control));
                    break;
                }
                buffer.extend_from_slice(&tcp_buf[..n]);
            }
            packet=rtp.recv_from(&mut udp_buf)=> {
                let (n,sender)=packet?;if sender.ip()!=std::net::IpAddr::V4(peer) {continue;}
                let payload=rtp_payload(&udp_buf[..n])?;
                handle_rtp_packet(
                    &udp_buf[..n],
                    payload,
                    Instant::now(),
                    &mut camera,
                    &control,
                    &tx,
                    &mut rtp_sequence,
                    &mut last_rtp_arrival,
                )
                .await?;
            }
            packet=rtcp.recv_from(&mut rtcp_buf)=> {let (_,sender)=packet?;rtcp.send_to(&[0x80,201,0,1,0x58,0x43,0x4c,0x42],sender).await?;}
        }
        while let Some(message) = parse_rtsp(&mut buffer)? {
            handle_rtsp(
                &mut socket,
                message,
                &caps,
                port,
                rtcp_port,
                &mut url,
                &mut sequence,
                &mut sent,
                &mut session,
                &mut options_sent,
                &mut setup_sent,
                &control,
            )
            .await?;
        }
    }
    Ok::<(), anyhow::Error>(())
    }.await;
    if let Some(session) = session {
        let _ = rtsp_request(
            &mut socket,
            &mut sequence,
            &mut sent,
            "TEARDOWN",
            &url,
            vec![("Session", session)],
        )
        .await;
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::rtsp::RtspMessage;
    use anyhow::{Context, ensure};
    use std::time::Duration;
    use tokio::net::TcpListener;

    async fn read_rtsp(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<RtspMessage> {
        loop {
            if let Some(message) = parse_rtsp(buffer)? {
                return Ok(message);
            }
            ensure!(socket.read_buf(buffer).await? != 0, "RTSP peer closed");
        }
    }

    #[tokio::test]
    async fn full_decode_queue_still_sends_teardown_on_close() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let client = TcpStream::connect(listener.local_addr()?).await?;
            let (mut source, _) = listener.accept().await?;
            let rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let rtp_address = rtp.local_addr()?;
            let rtcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let (tx, mut rx) = mpsc::channel(1);
            let control = Control::new();
            let network = tokio::spawn(wfd(
                client,
                rtp,
                rtcp,
                Ipv4Addr::LOCALHOST,
                None,
                tx,
                control.clone(),
            ));
            let mut buffer = Vec::new();
            rtsp_send(
                &mut source,
                "SET_PARAMETER rtsp://127.0.0.1/wfd1.0 RTSP/1.0",
                &[("CSeq", "1".into())],
                "wfd_trigger_method: SETUP\r\n",
            )
            .await?;
            let _response = read_rtsp(&mut source, &mut buffer).await?;
            let setup = read_rtsp(&mut source, &mut buffer).await?;
            ensure!(setup.start.starts_with("SETUP "));
            rtsp_send(
                &mut source,
                "RTSP/1.0 200 OK",
                &[
                    ("CSeq", setup.headers["cseq"].clone()),
                    ("Session", "full-queue".into()),
                ],
                "",
            )
            .await?;
            let play = read_rtsp(&mut source, &mut buffer).await?;
            ensure!(play.start.starts_with("PLAY "));
            let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let mut packet = vec![0; 12 + 188];
            packet[0] = 0x80;
            packet[1] = 33;
            packet[12] = 0x47;
            udp.send_to(&packet, rtp_address).await?;
            packet[3] = 2;
            packet[13] = 2;
            udp.send_to(&packet, rtp_address).await?;
            while control.shared.lock().unwrap().stats.received_bytes < 376 {
                tokio::task::yield_now().await;
            }
            ensure!(rx.recv().await.unwrap()[1] == 0);
            packet[3] = 1;
            packet[13] = 1;
            udp.send_to(&packet, rtp_address).await?;
            while control.shared.lock().unwrap().stats.received_bytes < 564 {
                tokio::task::yield_now().await;
            }
            {
                let shared = control.shared.lock().unwrap();
                ensure!(shared.stats.rtp_sequence_gaps == 1);
                ensure!(shared.stats.rtp_late_packets == 1);
            }
            control.stopped.store(true, Ordering::Release);
            control.wake.notify_one();
            // A keyframe request already in flight may produce an optional
            // SET_PARAMETER response before TEARDOWN; drain it here.
            let teardown = loop {
                let message = read_rtsp(&mut source, &mut buffer).await?;
                if message.start.starts_with("TEARDOWN ") {
                    break message;
                }
                ensure!(
                    message.start.starts_with("SET_PARAMETER ")
                        || message.start.starts_with("RTSP/1.0 200 OK"),
                    "unexpected RTSP message: {}",
                    message.start
                );
            };
            ensure!(teardown.headers["session"] == "full-queue");
            network.await??;
            ensure!(
                rx.recv().await.unwrap()[1] == 2,
                "Diagnostics reordered RTP payloads"
            );
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Closing a full media queue hung or omitted TEARDOWN")?
    }
}

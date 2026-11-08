//! MediaPlayer lifecycle, WFD session setup and playback controls.
use super::{
    Control, MediaPlayer, MediaStats, PlaybackOptions, VideoFrame, decode::decode, wfd::wfd,
};
use crate::Result;
use anyhow::Context;
use ffmpeg_next as av;
use std::{
    io::{self, Read},
    net::Ipv4Addr,
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::mpsc,
};

impl MediaPlayer {
    pub async fn play_screen(
        local_ip: Ipv4Addr,
        peer: Ipv4Addr,
        port: u16,
        options: PlaybackOptions,
    ) -> Result<Self> {
        Self::play_wfd(
            local_ip,
            peer,
            port,
            None,
            "desktop.ts",
            options,
            Duration::ZERO,
        )
        .await
    }
    pub async fn play_camera(
        local_ip: Ipv4Addr,
        feed: crate::media::CameraFeed,
        options: PlaybackOptions,
    ) -> Result<Self> {
        let hint = match feed.codec {
            crate::media::CameraCodec::H264 => "camera.h264",
            crate::media::CameraCodec::H265 => "camera.hevc",
        };
        // The phone starts a new RTSP server during camera startup. At
        // 1440x1080 it can bind after the command RPC returns, so connecting
        // immediately can attach to the old/no-media listener.
        Self::play_wfd(
            local_ip,
            feed.ip,
            feed.port,
            Some(feed.key),
            hint,
            options,
            Duration::from_millis(1200),
        )
        .await
    }
    async fn play_wfd(
        local_ip: Ipv4Addr,
        peer: Ipv4Addr,
        port: u16,
        key: Option<[u8; 16]>,
        hint: &'static str,
        options: PlaybackOptions,
        start_delay: Duration,
    ) -> Result<Self> {
        if !start_delay.is_zero() {
            tokio::time::sleep(start_delay).await;
        }
        let rtsp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((peer, port)))
            .await
            .context("RTSP connection timed out")??;
        let rtp = UdpSocket::bind((local_ip, 0)).await?;
        socket2::SockRef::from(&rtp).set_recv_buffer_size(2 * 1024 * 1024)?;
        let rtp_port = rtp.local_addr()?.port();
        let rtcp = UdpSocket::bind((
            local_ip,
            rtp_port.checked_add(1).context("no RTP companion port")?,
        ))
        .await?;
        let control = Control::new();
        tracing::info!(
            "[media={:p}] RTSP={peer}:{port} RTP={local_ip}:{rtp_port} audio={}",
            Arc::as_ptr(&control),
            options.audio
        );
        let (tx, rx) = mpsc::channel(256);
        let c = control.clone();
        let network = tokio::spawn(async move {
            if let Err(e) = wfd(rtsp, rtp, rtcp, peer, key, tx, c.clone()).await {
                if !c.stopped.load(Ordering::Acquire) {
                    c.fail(format!("RTSP/RTP: {e:#}"));
                }
            }
        });
        let c = control.clone();
        let decoder = tokio::task::spawn_blocking(move || {
            let reader = ChannelReader {
                rx,
                pending: Vec::new(),
                position: 0,
            };
            let result = (|| {
                av::init()?;
                let io = av::format::context::StreamIo::from_read(reader)?;
                let mut dict = av::Dictionary::new();
                // A desktop's first video access unit can precede AAC by more
                // than 100ms. Discover both streams before choosing decoders.
                let desktop_audio = key.is_none() && options.audio;
                dict.set("probesize", if desktop_audio { "1048576" } else { "65536" });
                dict.set(
                    "analyzeduration",
                    if desktop_audio { "500000" } else { "100000" },
                );
                dict.set("fflags", "+discardcorrupt");
                let cancel = c.clone();
                let input = av::format::input_from_stream_with_interrupt(
                    io,
                    Some(hint),
                    Some(dict),
                    move || cancel.stopped.load(Ordering::Acquire),
                )
                .context("open/probe live input")?;
                decode(input, options, c.clone(), false)
            })();
            if let Err(e) = result {
                if !c.stopped.load(Ordering::Acquire) {
                    c.fail(format!("libav: {e:#}"));
                }
            }
            c.shared.lock().unwrap().stats.finished = true;
        });
        Ok(Self {
            control,
            network: Some(network),
            decoder: Some(decoder),
            audio_sync: None,
        })
    }
    /// Useful for inspecting a saved capture with the exact same decoding path.
    pub fn play_file(path: impl AsRef<Path>, options: PlaybackOptions) -> Self {
        let path = path.as_ref().to_owned();
        let control = Control::new();
        let c = control.clone();
        let decoder = tokio::task::spawn_blocking(move || {
            let result = (|| {
                av::init()?;
                let cancel = c.clone();
                let input = av::format::input_with_interrupt(&path, move || {
                    cancel.stopped.load(Ordering::Acquire)
                })?;
                decode(input, options, c.clone(), true)
            })();
            if let Err(e) = result {
                if !c.stopped.load(Ordering::Acquire) {
                    c.fail(format!("libav: {e:#}"));
                }
            }
            c.shared.lock().unwrap().stats.finished = true;
        });
        Self {
            control,
            network: None,
            decoder: Some(decoder),
            audio_sync: None,
        }
    }
    /// Takes the newest frame due at the audio clock, discarding stale frames.
    pub fn take_frame(&self) -> Option<VideoFrame> {
        let mut shared = self.control.shared.lock().unwrap();
        let now = shared.clock.now();
        let mut frame = None;
        while shared
            .frames
            .front()
            .is_some_and(|f| f.pts_seconds <= now + 0.015)
        {
            if frame.replace(shared.frames.pop_front().unwrap()).is_some() {
                shared.stats.dropped_video_frames += 1;
            }
        }
        if frame.is_some() {
            shared.stats.shown_frames += 1;
        }
        frame
    }
    pub fn stats(&self) -> MediaStats {
        let shared = self.control.shared.lock().unwrap();
        let mut stats = shared.stats.clone();
        stats.gain_basis_points = self.control.gain_basis_points.load(Ordering::Acquire);
        stats.focus_muted = self.control.focus_muted.load(Ordering::Acquire);
        stats.queued_audio_chunks = self.control.audio_queue_len.load(Ordering::Relaxed);
        stats.playback_clock_seconds = shared.clock.now();
        stats.clock_audio_master = shared.clock.audio_master;
        stats.queued_video_frames = shared.frames.len();
        stats.video_queue_head_pts_seconds = shared.frames.front().map(|frame| frame.pts_seconds);
        stats.video_queue_tail_pts_seconds = shared.frames.back().map(|frame| frame.pts_seconds);
        stats.video_queue_span_ms = stats
            .video_queue_head_pts_seconds
            .zip(stats.video_queue_tail_pts_seconds)
            .map(|(head, tail)| (tail - head) * 1000.0);
        stats.next_frame_delay_ms = shared
            .frames
            .front()
            .map(|frame| (frame.pts_seconds - shared.clock.now()) * 1000.0);
        stats
    }
    pub fn set_muted(&self, muted: bool) {
        self.control.muted.store(muted, Ordering::Release);
    }
    /// Adjust only this playback stream; never changes the desktop mixer.
    pub fn set_gain_basis_points(&self, gain: u32) {
        self.control
            .gain_basis_points
            .store(gain.min(10000), Ordering::Release);
    }
    /// Request a fresh video keyframe after a phone rotation or canvas change.
    pub fn request_keyframe(&self) {
        self.control.request_keyframe();
    }
    pub async fn close(mut self) -> Result<()> {
        if let Some(task) = self.audio_sync.take() {
            task.abort();
        }
        self.control.stopped.store(true, Ordering::Release);
        self.control.wake.notify_one();
        if let Some(task) = self.network.take() {
            task.await?;
        }
        if let Some(task) = self.decoder.take() {
            task.await?;
        }
        Ok(())
    }
}
impl Drop for MediaPlayer {
    fn drop(&mut self) {
        if let Some(task) = self.audio_sync.take() {
            task.abort();
        }
        self.control.stopped.store(true, Ordering::Release);
        self.control.wake.notify_one();
    }
}
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    position: usize,
}
impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.position == self.pending.len() {
            match self.rx.blocking_recv() {
                Some(data) => {
                    self.pending = data;
                    self.position = 0
                }
                None => return Ok(0),
            }
        }
        let len = out.len().min(self.pending.len() - self.position);
        out[..len].copy_from_slice(&self.pending[self.position..self.position + len]);
        self.position += len;
        Ok(len)
    }
}

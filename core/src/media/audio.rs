//! Audio decode/resample, ALSA output and per-stream gain/focus.
use super::{Clock, Control, audio_queue, decoder::send_decoder_packet};
use crate::Result;
use anyhow::{Context, ensure};
use ffmpeg_next as av;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

pub(super) const AUDIO_RATE: u32 = 48000;

pub(super) struct AudioDecoder {
    decoder: av::codec::decoder::Audio,
    frame: av::frame::Audio,
    resampler: Option<av::software::resampling::Context>,
    base: f64,
    output: audio_queue::Sender,
    control: Arc<Control>,
    live: bool,
    last_packet_pts: Option<f64>,
    sample_end: u64,
}
impl AudioDecoder {
    pub(super) fn new(
        parameters: av::codec::Parameters,
        base: f64,
        output: audio_queue::Sender,
        control: Arc<Control>,
        live: bool,
    ) -> Result<Self> {
        let decoder = av::codec::context::Context::from_parameters(parameters)?
            .decoder()
            .audio()?;
        tracing::info!(
            "[media={:p}] audio_decoder={:p}",
            Arc::as_ptr(&control),
            unsafe { decoder.as_ptr() }
        );
        Ok(Self {
            decoder,
            frame: av::frame::Audio::empty(),
            resampler: None,
            base,
            output,
            control,
            live,
            last_packet_pts: None,
            sample_end: 0,
        })
    }
    fn bad_packet(&self, stage: &str, counted: &mut bool) {
        let mut shared = self.control.shared.lock().unwrap();
        if !*counted {
            shared.stats.corrupt_audio_packets += 1;
            *counted = true;
        }
        shared.stats.audio_decode_warning = Some(format!(
            "Discarded invalid compressed audio during {stage}; continuing with subsequent packets"
        ));
    }
    pub(super) fn decode_packet(&mut self, packet: Option<&av::Packet>) -> Result<()> {
        if self.live
            && let Some(pts) = packet
                .and_then(av::Packet::pts)
                .map(|p| p as f64 * self.base)
            && let Some(previous) = self.last_packet_pts.replace(pts)
            && (pts - previous).abs() > 1.0
        {
            tracing::info!(
                "[media={:p}] audio source PTS jump: {previous:.6}s -> {pts:.6}s; live clock follows output samples",
                Arc::as_ptr(&self.control)
            );
        }
        let mut counted = false;
        loop {
            let accepted = match send_decoder_packet(&mut self.decoder, packet) {
                Ok(accepted) => accepted,
                Err(av::Error::InvalidData) if packet.is_some() => {
                    // libav consumes the damaged packet, including errors from
                    // send_packet's internal predecode. Drain; do not resubmit it.
                    self.bad_packet("send_packet", &mut counted);
                    true
                }
                Err(error) => return Err(error.into()),
            };
            let mut received = 0;
            loop {
                match self.decoder.receive_frame(&mut self.frame) {
                    Ok(()) => received += 1,
                    Err(av::Error::Eof) => break,
                    Err(av::Error::Other { errno }) if errno == libc::EAGAIN => break,
                    Err(av::Error::InvalidData) => {
                        self.bad_packet("receive_frame", &mut counted);
                        received += 1; // A buffered damaged packet was consumed.
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
                if self.resampler.is_none() {
                    self.resampler = Some(av::software::resampling::Context::get(
                        self.frame.format(),
                        self.frame.channel_layout(),
                        self.frame.rate(),
                        av::format::Sample::I16(av::format::sample::Type::Packed),
                        av::ChannelLayout::STEREO,
                        AUDIO_RATE,
                    )?);
                }
                let mut pcm = av::frame::Audio::empty();
                self.resampler
                    .as_mut()
                    .unwrap()
                    .run(&self.frame, &mut pcm)?;
                let samples = pcm.data(0)[..pcm.samples() * 4]
                    .chunks_exact(2)
                    .map(|b| i16::from_ne_bytes([b[0], b[1]]))
                    .collect();
                let pts = self
                    .frame
                    .timestamp()
                    .or_else(|| self.frame.pts())
                    .map(|p| p as f64 * self.base);
                self.sample_end += pcm.samples() as u64;
                {
                    let mut shared = self.control.shared.lock().unwrap();
                    shared.stats.decoded_audio_samples += pcm.samples() as u64;
                    shared.stats.audio_decoded_pts_seconds = pts;
                }
                let send_started = Instant::now();
                let sent = self.output.send(AudioChunk {
                    samples,
                    pts,
                    sample_end: self.sample_end,
                });
                let elapsed_ms = send_started.elapsed().as_secs_f64() * 1000.0;
                {
                    let mut shared = self.control.shared.lock().unwrap();
                    shared.stats.audio_queue_send_ms += elapsed_ms;
                    shared.stats.audio_queue_send_max_ms =
                        shared.stats.audio_queue_send_max_ms.max(elapsed_ms);
                }
                let discarded = sent.context("audio output stopped")?;
                if discarded.chunks > 0 {
                    let mut shared = self.control.shared.lock().unwrap();
                    if shared.stats.dropped_audio_chunks == 0 {
                        tracing::info!(
                            "[media={:p}] live audio queue full: discarding oldest PCM to keep video decoding current",
                            Arc::as_ptr(&self.control)
                        );
                    }
                    shared.stats.dropped_audio_chunks += discarded.chunks;
                    shared.stats.dropped_audio_samples += discarded.samples;
                }
            }
            if accepted {
                return Ok(());
            }
            ensure!(received > 0, "Audio decoder returned EAGAIN without output");
        }
    }
}
pub(super) struct AudioChunk {
    pub(super) samples: Vec<i16>,
    pub(super) pts: Option<f64>,
    pub(super) sample_end: u64,
}
/// Use the active desktop server without depending on the system's ALSA
/// `default` alias. Explicit ALSA device names are passed through unchanged.
fn resolve_audio_device(requested: &str) -> String {
    let requested = requested.trim();
    if !requested.is_empty() && requested != "auto" {
        return requested.to_owned();
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if runtime.join("pipewire-0").exists() {
            return "pipewire".into();
        }
        if runtime.join("pulse/native").exists() {
            return "pulse".into();
        }
    }
    "default".into()
}
pub(super) fn audio_playback(
    rx: audio_queue::Receiver,
    device: &str,
    control: Arc<Control>,
    live: bool,
) -> Result<()> {
    use alsa::{
        Direction, ValueOr,
        pcm::{Access, Format, HwParams, PCM},
    };
    let device = resolve_audio_device(device);
    control.shared.lock().unwrap().stats.audio_device = Some(device.clone());
    let pcm = PCM::new(&device, Direction::Playback, false)
        .with_context(|| format!("open audio device {device}"))?;
    {
        let hw = HwParams::any(&pcm)?;
        hw.set_channels(2)?;
        hw.set_rate(AUDIO_RATE, ValueOr::Nearest)?;
        hw.set_format(Format::s16())?;
        hw.set_access(Access::RWInterleaved)?;
        hw.set_buffer_time_near(80000, ValueOr::Nearest)?;
        pcm.hw_params(&hw)?;
        control.shared.lock().unwrap().stats.audio_output_rate = hw.get_rate()?;
    }
    let io = pcm.io_i16()?;
    let mut clock_offset = None;
    let mut recovered = false;
    while let Some(mut chunk) = rx.recv() {
        if control.stopped.load(Ordering::Acquire) {
            break;
        }
        if control.muted.load(Ordering::Acquire) || control.focus_muted.load(Ordering::Acquire) {
            chunk.samples.fill(0);
        } else {
            let gain = control.gain_basis_points.load(Ordering::Acquire) as i32;
            if gain != 10000 {
                for sample in &mut chunk.samples {
                    *sample = (i32::from(*sample) * gain / 10000) as i16;
                }
            }
        }
        let mut offset = 0;
        while offset < chunk.samples.len() {
            match io.writei(&chunk.samples[offset..]) {
                Ok(frames) => offset += frames * 2,
                Err(e) => {
                    pcm.try_recover(e, false)?;
                    clock_offset = None;
                    recovered = true;
                    tracing::info!(
                        "[media={:p}] audio output recovered: {e}",
                        Arc::as_ptr(&control)
                    );
                }
            }
        }
        let written = (chunk.samples.len() / 2) as u64;
        let queued = pcm.delay()?.max(0) as f64 / f64::from(AUDIO_RATE);
        let mut state = control.shared.lock().unwrap();
        state.stats.audio_output_pts_seconds = chunk.pts;
        state.stats.audio_output_delay_ms = Some(queued * 1000.0);
        if live && state.clock.initialized {
            let cursor = chunk.sample_end as f64 / f64::from(AUDIO_RATE) - queued;
            let offset = *clock_offset.get_or_insert_with(|| {
                let mapped = state.clock.now();
                tracing::info!(
                    "[media={:p}] live audio clock {}: source_pts={:?} mapped={mapped:.6}s delay_ms={:.1}",
                    Arc::as_ptr(&control),
                    if recovered { "reanchored" } else { "anchored" },
                    chunk.pts,
                    queued * 1000.0
                );
                recovered = false;
                mapped - cursor
            });
            state.clock = Clock {
                pts: cursor + offset,
                at: Instant::now(),
                initialized: true,
                audio_master: true,
            };
        } else if !live && let Some(pts) = chunk.pts {
            state.clock = Clock {
                pts: pts + written as f64 / f64::from(AUDIO_RATE) - queued,
                at: Instant::now(),
                initialized: true,
                audio_master: true,
            };
        }
        state.stats.played_audio_samples += written;
    }
    if control.stopped.load(Ordering::Acquire) {
        pcm.drop()?;
    } else {
        pcm.drain()?;
    }
    Ok(())
}

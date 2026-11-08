//! libav demux/decode loop, clocking and frame queue.
use super::{
    Clock, Control, PlaybackOptions, VideoFrame,
    audio::{AudioDecoder, audio_playback},
    audio_queue,
    decoder::send_decoder_packet,
    v4l2::VirtualCamera,
};
use crate::Result;
use anyhow::{Context, ensure};
use ffmpeg_next as av;
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

pub(super) fn decode(
    mut input: av::format::context::Input,
    options: PlaybackOptions,
    control: Arc<Control>,
    pace: bool,
) -> Result<()> {
    let video = input
        .streams()
        .best(av::media::Type::Video)
        .context("media has no video stream")?;
    let video_index = video.index();
    let video_base: f64 = video.time_base().into();
    let parameters = video.parameters();
    let raw_parameters = unsafe { &*parameters.as_ptr() };
    tracing::info!(
        "[media={:p}] demux={:p} video_stream={video_index} codec={:?} parameters={}x{} extradata={} time_base={video_base}",
        Arc::as_ptr(&control),
        unsafe { input.as_ptr() },
        parameters.id(),
        raw_parameters.width,
        raw_parameters.height,
        raw_parameters.extradata_size
    );
    let mut context = av::codec::context::Context::from_parameters(parameters)
        .context("create video decoder context")?;
    context.set_threading(av::codec::threading::Config {
        kind: av::codec::threading::Type::Slice,
        count: 2,
        ..Default::default()
    });
    let mut decoder = context.decoder().video().context("open video decoder")?;
    tracing::info!(
        "[media={:p}] video_decoder={:p}",
        Arc::as_ptr(&control),
        unsafe { decoder.as_ptr() }
    );
    let mut output = VideoOutput::default();
    let audio = if options.audio {
        input
            .streams()
            .best(av::media::Type::Audio)
            .map(|s| (s.index(), f64::from(s.time_base()), s.parameters()))
    } else {
        None
    };
    let (audio_tx, audio_rx) = audio_queue::channel(
        if pace { 12 } else { 3 },
        !pace,
        control.audio_queue_len.clone(),
    );
    let audio_control = control.clone();
    let audio_device = options.audio_device.clone();
    let mut audio_decoder =
        audio.as_ref().and_then(|(_, base, parameters)| {
            match AudioDecoder::new(parameters.clone(), *base, audio_tx, control.clone(), !pace) {
                Ok(decoder) => Some(decoder),
                Err(error) => {
                    control.audio_failed(format!("Audio decoder: {error:#}"));
                    None
                }
            }
        });
    let audio_thread = audio_decoder.as_ref().map(|_| {
        std::thread::spawn(move || {
            let result = audio_playback(audio_rx, &audio_device, audio_control.clone(), !pace);
            audio_control.shared.lock().unwrap().clock.audio_master = false;
            if let Err(e) = result {
                audio_control.audio_failed(format!("Audio: {e:#}"));
            }
        })
    });
    let mut packet = av::Packet::empty();
    let mut frame = av::frame::Video::empty();
    let mut first_video_frame = true;
    let mut generation = control.generation();
    while !control.stopped.load(Ordering::Acquire) {
        let current_generation = control.generation();
        if current_generation != generation {
            // Drop frames buffered by libav for the previous camera epoch.
            decoder.flush();
            generation = current_generation;
            tracing::info!(
                "[media={:p}] flushed video decoder for camera generation {generation}",
                Arc::as_ptr(&control)
            );
        }
        let eof = match packet.read(&mut input) {
            Ok(()) => false,
            Err(av::Error::Eof) => true,
            Err(e) => return Err(e).context("demux read_packet"),
        };
        if eof || packet.stream() == video_index {
            if !eof {
                control.shared.lock().unwrap().stats.demux_video_packets += 1;
            }
            let decode_started = Instant::now();
            let mut counted_invalid = false;
            loop {
                let accepted = match send_decoder_packet(&mut decoder, (!eof).then_some(&packet)) {
                    Ok(accepted) => accepted,
                    Err(av::Error::InvalidData) if !pace && !eof => {
                        control.bad_video_packet("send_packet", &mut counted_invalid);
                        true
                    }
                    Err(error) => return Err(error).context("video send_packet"),
                };
                let mut received = 0;
                loop {
                    match decoder.receive_frame(&mut frame) {
                        Ok(()) => {}
                        Err(av::Error::Eof) => break,
                        Err(av::Error::Other { errno }) if errno == libc::EAGAIN => break,
                        Err(av::Error::InvalidData) if !pace && !eof => {
                            control.bad_video_packet("receive_frame", &mut counted_invalid);
                            received += 1;
                            continue;
                        }
                        Err(error) => return Err(error).context("video receive_frame"),
                    }
                    let decoded_at = Instant::now();
                    let pts = frame
                        .timestamp()
                        .or_else(|| frame.pts())
                        .map(|p| p as f64 * video_base);
                    if first_video_frame {
                        first_video_frame = false;
                        let shared = control.shared.lock().unwrap();
                        tracing::info!(
                            "[media={:p}] first decoded video: pts={pts:?} clock={:?}",
                            Arc::as_ptr(&control),
                            shared.clock.initialized.then(|| shared.clock.now())
                        );
                    }
                    received += 1;
                    if frame.is_key() && control.recovering_video.swap(false, Ordering::AcqRel) {
                        control
                            .shared
                            .lock()
                            .unwrap()
                            .stats
                            .waiting_for_video_keyframe = false;
                        tracing::info!(
                            "[media={:p}] video keyframe decoded: {}x{} pts={:?}",
                            Arc::as_ptr(&control),
                            frame.width(),
                            frame.height(),
                            frame.pts()
                        );
                    }
                    // A switch can leave non-keyframes from the old camera in the
                    // decoder queue. Do not display them while waiting for the
                    // new stream's IDR.
                    if control.is_recovering_video() && !frame.is_key() {
                        continue;
                    }
                    output.push(&frame, pts, decoded_at, &control, &options, pace)?;
                }
                if accepted {
                    break;
                }
                ensure!(received > 0, "Video decoder returned EAGAIN without output");
            }
            let elapsed_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
            let mut shared = control.shared.lock().unwrap();
            shared.stats.video_decode_ms += elapsed_ms;
            shared.stats.video_decode_max_ms = shared.stats.video_decode_max_ms.max(elapsed_ms);
        }
        if (eof
            || audio
                .as_ref()
                .is_some_and(|(index, _, _)| *index == packet.stream()))
            && let Some(track) = audio_decoder.as_mut()
        {
            if let Err(error) = track.decode_packet((!eof).then_some(&packet)) {
                control.audio_failed(format!("Audio decoder: {error:#}"));
                audio_decoder = None;
            }
        }
        if eof {
            break;
        }
    }
    drop(audio_decoder);
    if let Some(task) = audio_thread {
        let _ = task.join();
    }
    Ok(())
}
#[derive(Default)]
struct VideoOutput {
    scaler: Option<(
        (av::format::Pixel, u32, u32),
        av::software::scaling::Context,
    )>,
    sink: Option<VirtualCamera>,
}

impl VideoOutput {
    fn push(
        &mut self,
        frame: &av::frame::Video,
        pts: Option<f64>,
        decoded_at: Instant,
        control: &Arc<Control>,
        options: &PlaybackOptions,
        pace: bool,
    ) -> Result<()> {
        let width = frame.width();
        let height = frame.height();
        let format = frame.format();
        let definition = (format, width, height);
        if self.scaler.as_ref().is_none_or(|(d, _)| *d != definition) {
            self.scaler = Some((
                definition,
                av::software::scaling::Context::get(
                    format,
                    width,
                    height,
                    av::format::Pixel::RGBA,
                    width,
                    height,
                    av::software::scaling::flag::Flags::BILINEAR,
                )
                .context("create video RGBA converter")?,
            ));
        }
        let mut rgba = av::frame::Video::empty();
        self.scaler
            .as_mut()
            .unwrap()
            .1
            .run(&frame, &mut rgba)
            .context("convert video frame to RGBA")?;
        // Saved captures can be decoded far faster than their clock.
        // Pace them before the bounded UI queue discards useful frames.
        if pace {
            if let Some(pts) = pts {
                loop {
                    let shared = control.shared.lock().unwrap();
                    let delay = pts - shared.clock.now() - 0.08;
                    if !shared.clock.initialized
                        || delay <= 0.0
                        || control.stopped.load(Ordering::Acquire)
                    {
                        break;
                    }
                    drop(shared);
                    std::thread::sleep(Duration::from_secs_f64(delay.min(0.01)));
                }
            }
        }
        let mut shared = control.shared.lock().unwrap();
        let pts = pts.unwrap_or_else(|| shared.clock.now());
        shared.stats.video_pts_seconds = Some(pts);
        if !shared.clock.initialized {
            shared.clock = Clock {
                pts,
                at: Instant::now(),
                initialized: true,
                audio_master: false,
            };
        }
        let queue_limit = if pace { 6 } else { 16 };
        let overflowed = shared.frames.len() >= queue_limit;
        // A live burst can outrun the initial video clock. If only
        // future frames fill the bounded queue, follow live video.
        // An actively written audio clock remains authoritative.
        if !pace
            && !shared.clock.audio_master
            && overflowed
            && shared
                .frames
                .front()
                .is_some_and(|frame| frame.pts_seconds > shared.clock.now() + 0.015)
        {
            shared.clock = Clock {
                pts,
                at: Instant::now(),
                initialized: true,
                audio_master: false,
            };
        }
        let due = shared.clock.now() + 0.015;
        while shared.frames.len() >= queue_limit
            && (pace
                || shared
                    .frames
                    .front()
                    .is_some_and(|frame| frame.pts_seconds <= due))
        {
            shared.frames.pop_front();
            shared.stats.dropped_video_frames += 1;
        }
        if shared.frames.len() < queue_limit {
            shared.frames.push_back(VideoFrame {
                width,
                height,
                stride: rgba.stride(0),
                rgba: rgba.data(0).to_vec(),
                pts_seconds: pts,
                decoded_at,
            });
        } else {
            shared.stats.dropped_video_frames += 1;
            if !shared.future_queue_warning_logged {
                shared.future_queue_warning_logged = true;
                tracing::info!(
                    "[media={:p}] video queue full with {queue_limit} future frames: head_delay_ms={:.1}; preserving earliest frame, dropping incoming frame",
                    Arc::as_ptr(&control),
                    (shared.frames.front().unwrap().pts_seconds - shared.clock.now()) * 1000.0
                );
            }
        }
        shared.stats.decoded_frames += 1;
        shared.stats.width = width;
        shared.stats.height = height;
        drop(shared);
        if let Some(path) = &options.v4l2_device {
            if self
                .sink
                .as_ref()
                .is_none_or(|s| s.width != width || s.height != height)
            {
                self.sink = Some(VirtualCamera::new(path, width, height)?);
            }
            self.sink.as_mut().unwrap().write(&frame)?;
        }
        Ok(())
    }
}

//! Direct libav demux/decode and Linux playback. No ffmpeg subprocess is used.
//! WFD negotiation stays in Rust because the phone also initiates RTSP requests.
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::{sync::Notify, task::JoinHandle};

mod audio;
mod audio_queue;
pub(crate) mod camera_ts;
mod decode;
mod decoder;
mod player;
mod rtsp;
mod v4l2;
mod wfd;

pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub rgba: Vec<u8>,
    pub pts_seconds: f64,
    pub decoded_at: Instant,
}
#[derive(Clone, Debug, Default)]
pub struct MediaStats {
    pub decoded_frames: u64,
    pub demux_video_packets: u64,
    pub corrupt_video_packets: u64,
    pub video_decode_warning: Option<String>,
    pub waiting_for_video_keyframe: bool,
    pub requested_keyframes: u64,
    pub decoded_audio_samples: u64,
    pub played_audio_samples: u64,
    pub dropped_audio_samples: u64,
    pub dropped_audio_chunks: u64,
    pub queued_audio_chunks: usize,
    pub corrupt_audio_packets: u64,
    pub audio_decode_warning: Option<String>,
    pub audio_device: Option<String>,
    pub audio_error: Option<String>,
    pub gain_basis_points: u32,
    pub focus_muted: bool,
    pub shown_frames: u64,
    pub dropped_video_frames: u64,
    pub queued_video_frames: usize,
    pub video_queue_head_pts_seconds: Option<f64>,
    pub video_queue_tail_pts_seconds: Option<f64>,
    pub video_queue_span_ms: Option<f64>,
    /// Queue-head PTS minus playback clock. Positive waits; negative is late.
    pub next_frame_delay_ms: Option<f64>,
    pub playback_clock_seconds: f64,
    pub clock_audio_master: bool,
    pub video_pts_seconds: Option<f64>,
    pub audio_decoded_pts_seconds: Option<f64>,
    pub audio_output_pts_seconds: Option<f64>,
    pub audio_output_rate: u32,
    pub audio_output_delay_ms: Option<f64>,
    pub audio_queue_send_ms: f64,
    pub audio_queue_send_max_ms: f64,
    pub video_decode_ms: f64,
    pub video_decode_max_ms: f64,
    pub received_bytes: u64,
    pub rtp_packets: u64,
    pub rtp_max_interarrival_ms: f64,
    /// Observed forward sequence gaps; late arrivals do not subtract from this.
    pub rtp_sequence_gaps: u64,
    /// Packets behind the highest received sequence, including duplicates.
    pub rtp_late_packets: u64,
    pub width: u32,
    pub height: u32,
    pub error: Option<String>,
    pub finished: bool,
}
#[derive(Clone, Copy, Debug)]
pub enum CameraCodec {
    H264,
    H265,
}

#[derive(Clone, Copy, Debug)]
pub struct CameraFeed {
    pub ip: std::net::Ipv4Addr,
    pub port: u16,
    pub key: [u8; 16],
    pub codec: CameraCodec,
}

#[derive(Clone, Debug)]
pub struct PlaybackOptions {
    pub audio: bool,
    pub audio_device: String,
    pub v4l2_device: Option<PathBuf>,
}
impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            audio: true,
            audio_device: "auto".into(),
            v4l2_device: None,
        }
    }
}

struct Clock {
    pts: f64,
    at: Instant,
    initialized: bool,
    audio_master: bool,
}
impl Clock {
    fn now(&self) -> f64 {
        self.pts + self.at.elapsed().as_secs_f64()
    }
}
struct Shared {
    frames: VecDeque<VideoFrame>,
    clock: Clock,
    stats: MediaStats,
    future_queue_warning_logged: bool,
}
pub(crate) struct Control {
    stopped: AtomicBool,
    muted: AtomicBool,
    pub(crate) focus_muted: AtomicBool,
    pub(crate) gain_basis_points: AtomicU32,
    wake: Notify,
    keyframe_requested: AtomicBool,
    keyframe_wake: Notify,
    recovering_video: AtomicBool,
    generation: AtomicU64,
    audio_queue_len: Arc<AtomicUsize>,
    shared: Mutex<Shared>,
}
impl Control {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            focus_muted: AtomicBool::new(false),
            gain_basis_points: AtomicU32::new(10000),
            wake: Notify::new(),
            keyframe_requested: AtomicBool::new(false),
            keyframe_wake: Notify::new(),
            recovering_video: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            audio_queue_len: Arc::new(AtomicUsize::new(0)),
            shared: Mutex::new(Shared {
                frames: VecDeque::new(),
                clock: Clock {
                    pts: 0.0,
                    at: Instant::now(),
                    initialized: false,
                    audio_master: false,
                },
                stats: MediaStats::default(),
                future_queue_warning_logged: false,
            }),
        })
    }
    fn fail(&self, error: impl std::fmt::Display) {
        self.shared.lock().unwrap().stats.error = Some(error.to_string());
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_one();
    }
    fn audio_failed(&self, error: impl std::fmt::Display) {
        let mut shared = self.shared.lock().unwrap();
        shared.clock.audio_master = false;
        shared
            .stats
            .audio_error
            .get_or_insert_with(|| error.to_string());
    }
    pub(crate) fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
        self.keyframe_wake.notify_one();
    }
    fn is_recovering_video(&self) -> bool {
        self.recovering_video.load(Ordering::Acquire)
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub(crate) fn advance_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
    fn recover_video(&self, reason: &str) {
        if !self.recovering_video.swap(true, Ordering::AcqRel) {
            self.shared.lock().unwrap().stats.waiting_for_video_keyframe = true;
            tracing::info!("[media={self:p}] waiting for video keyframe: {reason}");
            self.request_keyframe();
        }
    }
    fn bad_video_packet(&self, stage: &str, counted: &mut bool) {
        {
            let mut shared = self.shared.lock().unwrap();
            if !*counted {
                shared.stats.corrupt_video_packets += 1;
                *counted = true;
            }
            shared.stats.video_decode_warning = Some(format!(
                "Video {stage}: invalid compressed data; waiting for complete SPS/PPS and a keyframe"
            ));
        }
        self.recover_video("video decoder rejected compressed data");
    }
}

pub struct MediaPlayer {
    pub(crate) control: Arc<Control>,
    network: Option<JoinHandle<()>>,
    decoder: Option<JoinHandle<()>>,
    pub(crate) audio_sync: Option<JoinHandle<()>>,
}

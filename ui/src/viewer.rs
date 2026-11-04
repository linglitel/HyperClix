//! Native Wayland viewers: libav frames, phone input, and protocol cleanup.
use super::Event;
use adw::prelude::*;
use gtk::{gdk, glib};
use hyperclix_core::{
    camera::CameraPreview,
    desktop::{self, MouseAction, Screen, ScreenEvent},
    media::{MediaPlayer, MediaStats},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

// Test UI default measured on this phone: 64dp scroll factor at actual 480dpi.
// This is an Android display scale, not a CAST protocol constant.
const DEFAULT_REMOTE_SCROLL_PIXELS_PER_AXIS: f64 = 192.0;
const PROTOCOL_SCROLL_UNITS_PER_AXIS: f64 = 100.0;

enum Source {
    Screen(Screen),
    Camera(Arc<CameraPreview>),
    File,
}
enum Input {
    Mouse(MouseAction, i32, i32, u32, i32),
    Key(u32, bool, u32),
    Text(String),
    Resize(u32, u32, u32),
    Navigation(u32),
    Close(Option<MediaPlayer>, async_channel::Sender<()>),
}

struct QueuedInput {
    input: Input,
    queued_at: Instant,
}

#[derive(Clone)]
struct InputSender(async_channel::Sender<QueuedInput>);

impl InputSender {
    fn try_send(&self, input: Input) -> Result<(), async_channel::TrySendError<QueuedInput>> {
        self.0.try_send(QueuedInput {
            input,
            queued_at: Instant::now(),
        })
    }
}
#[derive(Default)]
struct InputState {
    buttons: u32,
    point: Option<(i32, i32)>,
    keys: Vec<(u32, u32, bool)>,
    scroll: [f64; 2],
    scroll_flags: u32,
}

#[derive(Clone, Copy)]
struct MetricSnapshot {
    at: Instant,
    bytes: u64,
    video_packets: u64,
    decoded: u64,
    displayed: u64,
    ticks: u64,
    wait_sum_ms: f64,
}

#[derive(Clone, Copy, Default)]
struct ViewerRates {
    mbps: f64,
    video_packets: f64,
    decoded_fps: f64,
    displayed_fps: f64,
    tick_hz: f64,
    wait_ms: f64,
}

impl MetricSnapshot {
    fn rates_since(self, previous: Self) -> ViewerRates {
        let seconds = self.at.duration_since(previous.at).as_secs_f64();
        let displayed = self.displayed - previous.displayed;
        ViewerRates {
            mbps: (self.bytes - previous.bytes) as f64 * 8.0 / seconds / 1_000_000.0,
            video_packets: (self.video_packets - previous.video_packets) as f64 / seconds,
            decoded_fps: (self.decoded - previous.decoded) as f64 / seconds,
            displayed_fps: displayed as f64 / seconds,
            tick_hz: (self.ticks - previous.ticks) as f64 / seconds,
            wait_ms: if displayed == 0 {
                0.0
            } else {
                (self.wait_sum_ms - previous.wait_sum_ms) / displayed as f64
            },
        }
    }
}

struct ViewerMetrics {
    ticks: u64,
    displayed: u64,
    wait_sum_ms: f64,
    wait_max_ms: f64,
    last_tick: Option<Instant>,
    tick_max_gap_ms: f64,
    status: MetricSnapshot,
    report: MetricSnapshot,
    rates: ViewerRates,
}

impl ViewerMetrics {
    fn new() -> Self {
        let initial = MetricSnapshot {
            at: Instant::now(),
            bytes: 0,
            video_packets: 0,
            decoded: 0,
            displayed: 0,
            ticks: 0,
            wait_sum_ms: 0.0,
        };
        Self {
            ticks: 0,
            displayed: 0,
            wait_sum_ms: 0.0,
            wait_max_ms: 0.0,
            last_tick: None,
            tick_max_gap_ms: 0.0,
            status: initial,
            report: initial,
            rates: ViewerRates::default(),
        }
    }

    fn snapshot(&self, now: Instant, stats: &MediaStats) -> MetricSnapshot {
        MetricSnapshot {
            at: now,
            bytes: stats.received_bytes,
            video_packets: stats.demux_video_packets,
            decoded: stats.decoded_frames,
            displayed: self.displayed,
            ticks: self.ticks,
            wait_sum_ms: self.wait_sum_ms,
        }
    }
}

struct ViewerState {
    player: RefCell<Option<MediaPlayer>>,
    image: Cell<(u32, u32)>,
    input: RefCell<InputState>,
    closed: Cell<bool>,
    last_error: RefCell<Option<String>>,
    audio_warning_logged: Cell<bool>,
    waiting_video_keyframe: Cell<bool>,
    metrics: RefCell<ViewerMetrics>,
    shown_pts: Cell<f64>,
    screen_events: RefCell<Option<tokio::sync::broadcast::Receiver<ScreenEvent>>>,
    window: gtk::Window,
    auto_close_finished: bool,
    auto_closed: Cell<bool>,
}
pub(super) fn open_screen(
    app: &adw::Application,
    parent: &adw::ApplicationWindow,
    rt: Arc<tokio::runtime::Runtime>,
    tx: async_channel::Sender<Event>,
    screen: Screen,
    player: MediaPlayer,
) -> gtk::Window {
    let config = screen.configuration();
    let title = if config.screen_id == 0 {
        "手机桌面".to_string()
    } else if !config.app_name.is_empty() {
        config.app_name.clone()
    } else {
        "手机应用".into()
    };
    open(app, parent, rt, tx, Source::Screen(screen), player, &title)
}
pub(super) fn open_camera(
    app: &adw::Application,
    parent: &adw::ApplicationWindow,
    rt: Arc<tokio::runtime::Runtime>,
    tx: async_channel::Sender<Event>,
    camera: Arc<CameraPreview>,
    player: MediaPlayer,
) -> gtk::Window {
    open(
        app,
        parent,
        rt,
        tx,
        Source::Camera(camera),
        player,
        "手机相机",
    )
}
pub(super) fn open_file(
    app: &adw::Application,
    parent: &adw::ApplicationWindow,
    rt: Arc<tokio::runtime::Runtime>,
    tx: async_channel::Sender<Event>,
    player: MediaPlayer,
) -> gtk::Window {
    open(app, parent, rt, tx, Source::File, player, "本地媒体预览")
}
fn open(
    app: &adw::Application,
    parent: &adw::ApplicationWindow,
    rt: Arc<tokio::runtime::Runtime>,
    tx: async_channel::Sender<Event>,
    source: Source,
    player: MediaPlayer,
    title: &str,
) -> gtk::Window {
    let screen = match &source {
        Source::Screen(screen) => Some(screen.clone()),
        _ => None,
    };
    let label = match &screen {
        Some(screen) => format!("{title} [screen={}]", screen.configuration().screen_id),
        None => title.to_owned(),
    };
    let portrait = screen.as_ref().is_some_and(|s| {
        let c = s.configuration();
        c.height > c.width
    });
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .transient_for(parent)
        .title(title)
        .default_width(if portrait { 600 } else { 1020 })
        .default_height(if portrait { 940 } else { 760 })
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(title, "原始分辨率 · libav")));
    let mute = gtk::ToggleButton::builder()
        .icon_name("audio-volume-high-symbolic")
        .tooltip_text("静音")
        .build();
    let fullscreen = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("全屏 / F11")
        .build();
    header.pack_end(&fullscreen);
    header.pack_end(&mute);
    let volume_label = gtk::Label::new(Some("手机音量 --"));
    volume_label.add_css_class("dim-label");
    volume_label.add_css_class("caption");
    header.pack_start(&volume_label);
    content.append(&header);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_vexpand(true);
    let overlay = gtk::Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.add_css_class("viewer-surface");
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_focusable(screen.is_some());
    if let Some(drop_screen) = screen.clone() {
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let picture_widget = picture.clone();
        let drop_runtime = rt.clone();
        let drop_events = tx.clone();
        drop.connect_drop(move |_, value, x, y| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<std::path::PathBuf> = files
                .files()
                .into_iter()
                .filter_map(|file| file.path())
                .collect();
            if paths.is_empty() {
                return false;
            }
            let configuration = drop_screen.configuration();
            if configuration.width == 0 || configuration.height == 0 {
                return false;
            }
            let widget_width = f64::from(picture_widget.width());
            let widget_height = f64::from(picture_widget.height());
            let scale = (widget_width / f64::from(configuration.width))
                .min(widget_height / f64::from(configuration.height));
            if !scale.is_finite() || scale <= 0.0 {
                return false;
            }
            let offset_x = (widget_width - f64::from(configuration.width) * scale) / 2.0;
            let offset_y = (widget_height - f64::from(configuration.height) * scale) / 2.0;
            let phone_x = ((x - offset_x) / scale)
                .clamp(0.0, f64::from(configuration.width.saturating_sub(1)))
                as i32;
            let phone_y = ((y - offset_y) / scale)
                .clamp(0.0, f64::from(configuration.height.saturating_sub(1)))
                as i32;
            let screen = drop_screen.clone();
            let events = drop_events.clone();
            drop_runtime.spawn(async move {
                super::logging::write(&format!(
                    "[app-drop] dropping {} file(s) at {},{}",
                    paths.len(),
                    phone_x,
                    phone_y
                ));
                if let Err(error) = screen.drop_files(paths, phone_x, phone_y).await {
                    let _ = events
                        .send(Event::Error(format!("拖放文件到手机：{error:#}")))
                        .await;
                }
            });
            true
        });
        picture.add_controller(drop);
    }
    overlay.set_child(Some(&picture));
    let waiting = gtk::Box::new(gtk::Orientation::Vertical, 12);
    waiting.set_halign(gtk::Align::Center);
    waiting.set_valign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.start();
    waiting.append(&spinner);
    waiting.append(&gtk::Label::new(Some("正在等待画面…")));
    overlay.add_overlay(&waiting);
    let preedit = gtk::Label::new(None);
    preedit.set_halign(gtk::Align::Center);
    preedit.set_valign(gtk::Align::End);
    preedit.set_margin_bottom(24);
    preedit.add_css_class("card");
    preedit.add_css_class("viewer-preedit");
    preedit.set_visible(false);
    overlay.add_overlay(&preedit);
    body.append(&overlay);
    let status = gtk::Label::new(Some("正在接收媒体"));
    status.set_xalign(0.0);
    status.add_css_class("dim-label");
    status.set_wrap(true);
    body.append(&status);
    let auto_close_finished = matches!(source, Source::Camera(_));
    let state = Rc::new(ViewerState {
        window: window.clone().upcast(),
        auto_close_finished,
        auto_closed: Cell::new(false),
        player: RefCell::new(Some(player)),
        image: Cell::new((0, 0)),
        input: RefCell::new(InputState::default()),
        closed: Cell::new(false),
        last_error: RefCell::new(None),
        audio_warning_logged: Cell::new(false),
        waiting_video_keyframe: Cell::new(false),
        metrics: RefCell::new(ViewerMetrics::new()),
        shown_pts: Cell::new(0.0),
        screen_events: RefCell::new(screen.as_ref().map(Screen::subscribe)),
    });
    let (input_tx, input_rx) = async_channel::unbounded();
    let input_tx = InputSender(input_tx);
    let event_tx = tx.clone();
    let input_label = label.clone();
    rt.spawn(async move {
        if let Err(error) =
            input_worker(source, input_rx, event_tx.clone(), input_label.clone()).await
        {
            let _ = event_tx
                .send(Event::Error(format!("{input_label} 控制：{error:#}")))
                .await;
        }
    });
    if let Some(screen) = screen.clone() {
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for (icon, tip, code) in [
            ("go-previous-symbolic", "返回", 4),
            ("go-home-symbolic", "主页", 3),
            ("view-grid-symbolic", "最近任务", 187),
        ] {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .build();
            let sender = input_tx.clone();
            let focus = picture.clone();
            button.connect_clicked(move |_| {
                let _ = sender.try_send(Input::Navigation(code));
                focus.grab_focus();
            });
            toolbar.append(&button);
        }
        toolbar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let config = screen.configuration();
        let width = spin(config.width.max(1), 160, 7680);
        let height = spin(config.height.max(1), 160, 7680);
        let density = spin(320, 72, 960);
        for (label, widget) in [("宽", &width), ("高", &height), ("DPI", &density)] {
            let text = gtk::Label::new(Some(label));
            text.add_css_class("dim-label");
            toolbar.append(&text);
            toolbar.append(widget);
        }
        let resize = gtk::Button::with_label("应用");
        resize.add_css_class("suggested-action");
        let sender = input_tx.clone();
        let focus = picture.clone();
        resize.connect_clicked(move |_| {
            let _ = sender.try_send(Input::Resize(
                width.value_as_int() as u32,
                height.value_as_int() as u32,
                density.value_as_int() as u32,
            ));
            focus.grab_focus();
        });
        toolbar.append(&resize);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&toolbar)
            .build();
        body.append(&scroll);
        let textrow = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let entry = gtk::Entry::builder()
            .placeholder_text("输入文字，按 Enter 发送到手机")
            .hexpand(true)
            .build();
        let send = gtk::Button::with_label("发送");
        let sender = input_tx.clone();
        let focus = picture.clone();
        entry.connect_activate(move |entry| {
            if !entry.text().is_empty() {
                let _ = sender.try_send(Input::Text(entry.text().to_string()));
                entry.set_text("");
                focus.grab_focus();
            }
        });
        let entry_clone = entry.clone();
        send.connect_clicked(move |_| {
            entry_clone.emit_activate();
        });
        textrow.append(&entry);
        textrow.append(&send);
        body.append(&textrow);
        attach_input(
            &picture,
            &preedit,
            &window,
            &screen,
            state.clone(),
            input_tx.clone(),
        );
        // Phone hardware volume keys own cast volume. The phone sends the
        // resulting media-volume event; the PC only follows that event.
    }
    if screen.is_none() {
        let keyboard = gtk::EventControllerKey::new();
        let weak_window = window.downgrade();
        keyboard.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::F11 {
                if let Some(window) = weak_window.upgrade() {
                    toggle_fullscreen(&window);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keyboard);
    }
    content.append(&body);
    window.set_content(Some(&content));
    let mute_state = state.clone();
    mute.connect_toggled(move |button| {
        if let Some(player) = mute_state.player.borrow().as_ref() {
            player.set_muted(button.is_active());
        }
        button.set_icon_name(if button.is_active() {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
    });
    let weak_window = window.downgrade();
    fullscreen.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            toggle_fullscreen(&window);
        }
    });
    let frame_state = state.clone();
    let frame_waiting = waiting.clone();
    let frame_input = input_tx.clone();
    picture.add_tick_callback(move |picture, _| {
        flush_scroll(&frame_state, &frame_input);
        let now = Instant::now();
        {
            let mut metrics = frame_state.metrics.borrow_mut();
            if let Some(previous) = metrics.last_tick.replace(now) {
                metrics.tick_max_gap_ms = metrics
                    .tick_max_gap_ms
                    .max(now.duration_since(previous).as_secs_f64() * 1000.0);
            }
            metrics.ticks += 1;
        }
        let player_ref = frame_state.player.borrow();
        let Some(player) = player_ref.as_ref() else {
            return glib::ControlFlow::Break;
        };
        if let Some(frame) = player.take_frame() {
            {
                let wait_ms = frame.decoded_at.elapsed().as_secs_f64() * 1000.0;
                let mut metrics = frame_state.metrics.borrow_mut();
                metrics.displayed += 1;
                metrics.wait_sum_ms += wait_ms;
                metrics.wait_max_ms = metrics.wait_max_ms.max(wait_ms);
            }
            frame_state.shown_pts.set(frame.pts_seconds);
            frame_state.image.set((frame.width, frame.height));
            let bytes = glib::Bytes::from_owned(frame.rgba);
            let texture = gdk::MemoryTexture::new(
                frame.width as i32,
                frame.height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &bytes,
                frame.stride,
            );
            picture.set_paintable(Some(&texture));
            frame_waiting.set_visible(false);
        }
        glib::ControlFlow::Continue
    });
    let render_state = state.clone();
    let render_tx = tx.clone();
    let render_window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let player_ref = render_state.player.borrow();
        let Some(player) = player_ref.as_ref() else {
            return glib::ControlFlow::Break;
        };
        let mut remote_closed = false;
        if let Some(events) = render_state.screen_events.borrow_mut().as_mut() {
            loop {
                match events.try_recv() {
                    Ok(ScreenEvent::ConfigurationChanged(config)) => {
                        if config.configuration == 1 {
                            player.request_keyframe();
                        }
                        super::logging::write(&format!(
                            "{label} 配置更新：session={} RTSP={}:{} · {}×{} · configuration={}",
                            config.session_id,
                            config.phone_ip,
                            config.port,
                            config.width,
                            config.height,
                            config.configuration
                        ));
                    }
                    Ok(ScreenEvent::PhoneVolumeChanged(volume)) => {
                        if let Some(basis) = volume.basis_points {
                            volume_label
                                .set_text(&format!("手机音量 {:.0}%", basis as f64 / 100.0));
                        }
                    }
                    Ok(ScreenEvent::AudioFocusChanged(focus)) => {
                        let suffix = if focus.muted { " · 静音" } else { "" };
                        let current = volume_label.text();
                        let base = current.split(" · ").next().unwrap_or("手机音量 --");
                        volume_label.set_text(&format!("{base}{suffix}"));
                    }
                    Ok(ScreenEvent::Error(error)) => {
                        let _ = render_tx.try_send(Event::Error(format!("{label}：{error}")));
                    }
                    Ok(ScreenEvent::Closed(reason)) => {
                        let _ = render_tx.try_send(Event::Log(format!("{label} 已结束：{reason}")));
                        remote_closed = true;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
        if remote_closed {
            drop(player_ref);
            if let Some(window) = render_window.upgrade() {
                window.close();
            }
            return glib::ControlFlow::Break;
        }
        let stats = player.stats();
        let auto_close = render_state.auto_close_finished
            && stats.finished
            && !render_state.auto_closed.replace(true);
        let now = Instant::now();
        let (refresh_status, rates, report) = {
            let mut metrics = render_state.metrics.borrow_mut();
            let snapshot = metrics.snapshot(now, &stats);
            let refresh_status = now.duration_since(metrics.status.at) >= Duration::from_secs(1);
            if refresh_status {
                metrics.rates = snapshot.rates_since(metrics.status);
                metrics.status = snapshot;
            }
            let report = if now.duration_since(metrics.report.at) >= Duration::from_secs(5) {
                let current_gap_ms = metrics
                    .last_tick
                    .map(|tick| now.duration_since(tick).as_secs_f64() * 1000.0)
                    .unwrap_or(0.0);
                let report = (
                    snapshot.rates_since(metrics.report),
                    metrics.tick_max_gap_ms.max(current_gap_ms),
                    metrics.wait_max_ms,
                );
                metrics.report = snapshot;
                metrics.tick_max_gap_ms = 0.0;
                metrics.wait_max_ms = 0.0;
                Some(report)
            } else {
                None
            };
            (refresh_status, metrics.rates, report)
        };
        if let Some((rates, tick_max_gap_ms, wait_max_ms)) = report {
            let mapped = render_window
                .upgrade()
                .is_some_and(|window| window.is_mapped());
            super::logging::write(&format!(
                "{label} 媒体状态：{}×{} rx_mbps={:.3} video_packets_per_s={:.1} decode_fps={:.1} display_fps={:.1} gtk_tick_hz={:.1} tick_max_gap_ms={:.1} display_wait_ms={:.1} display_wait_max_ms={:.1} mapped={} decoded={} shown={} dropped={} queued={} queue_head_pts={:?} queue_tail_pts={:?} queue_span_ms={:?} next_delay_ms={:?} pts={:.3}s clock={:.3}s audio_master={} video_pts={:?} audio_decoded_pts={:?} audio_output_pts={:?} audio_rate={:?} audio_delay_ms={:?} audio_queue_wait_ms={:.1} audio_queue_max_ms={:.1} video_decode_ms={:.1} video_decode_max_ms={:.1} bytes={} rtp_packets={} rtp_max_interarrival_ms={:.1} rtp_gaps={} rtp_late={} bad_video={} waiting_keyframe={} idr_requests={} audio_decoded={} audio_played={} audio_dropped_samples={} audio_dropped_chunks={} audio_queued_chunks={} bad_audio={} finished={}",
                stats.width,
                stats.height,
                rates.mbps,
                rates.video_packets,
                rates.decoded_fps,
                rates.displayed_fps,
                rates.tick_hz,
                tick_max_gap_ms,
                rates.wait_ms,
                wait_max_ms,
                mapped,
                stats.decoded_frames,
                stats.shown_frames,
                stats.dropped_video_frames,
                stats.queued_video_frames,
                stats.video_queue_head_pts_seconds,
                stats.video_queue_tail_pts_seconds,
                stats.video_queue_span_ms,
                stats.next_frame_delay_ms,
                render_state.shown_pts.get(),
                stats.playback_clock_seconds,
                stats.clock_audio_master,
                stats.video_pts_seconds,
                stats.audio_decoded_pts_seconds,
                stats.audio_output_pts_seconds,
                stats.audio_output_rate,
                stats.audio_output_delay_ms,
                stats.audio_queue_send_ms,
                stats.audio_queue_send_max_ms,
                stats.video_decode_ms,
                stats.video_decode_max_ms,
                stats.received_bytes,
                stats.rtp_packets,
                stats.rtp_max_interarrival_ms,
                stats.rtp_sequence_gaps,
                stats.rtp_late_packets,
                stats.corrupt_video_packets,
                stats.waiting_for_video_keyframe,
                stats.requested_keyframes,
                stats.decoded_audio_samples,
                stats.played_audio_samples,
                stats.dropped_audio_samples,
                stats.dropped_audio_chunks,
                stats.queued_audio_chunks,
                stats.corrupt_audio_packets,
                stats.finished
            ));
        }
        if render_state
            .waiting_video_keyframe
            .replace(stats.waiting_for_video_keyframe)
            != stats.waiting_for_video_keyframe
        {
            let message = if stats.waiting_for_video_keyframe {
                "视频流不完整，正在等待新的关键帧"
            } else {
                "已收到关键帧，视频解码恢复"
            };
            let _ = render_tx.try_send(Event::Log(format!("{label}：{message}")));
        }
        if !refresh_status {
            if auto_close {
                drop(player_ref);
                render_state.window.close();
                return glib::ControlFlow::Break;
            }
            return glib::ControlFlow::Continue;
        }
        if let Some(error) = &stats.error {
            status.set_text(error);
            waiting.set_visible(false);
            if render_state.last_error.borrow().as_ref() != Some(error) {
                *render_state.last_error.borrow_mut() = Some(error.clone());
                let _ = render_tx.try_send(Event::Error(format!("{label} 媒体：{error}")));
            }
        } else if stats.width > 0 {
            let mut text = format!(
                "{} × {}  ·  {:.2} Mbps  ·  解码 {:.1} fps  ·  显示 {:.1} fps\n视频包 {:.1}/s  ·  解码后等待 {:.0} ms  ·  队列 {} 帧  ·  累计 {:.1} MB{}",
                stats.width,
                stats.height,
                rates.mbps,
                rates.decoded_fps,
                rates.displayed_fps,
                rates.video_packets,
                rates.wait_ms,
                stats.queued_video_frames,
                stats.received_bytes as f64 / 1_000_000.0,
                if stats.finished {
                    "  ·  播放结束"
                } else {
                    ""
                }
            );
            if stats.waiting_for_video_keyframe {
                text.push_str("  ·  等待完整关键帧");
            }
            if stats.corrupt_audio_packets > 0 {
                text.push_str(&format!(
                    "  ·  已跳过损坏音频包 {}",
                    stats.corrupt_audio_packets
                ));
                if !render_state.audio_warning_logged.replace(true) {
                    let _ = render_tx.try_send(Event::Log(format!(
                        "{label} 音频：已跳过损坏的压缩音频包，继续解码后续音频和视频"
                    )));
                }
            }
            if let Some(error) = &stats.audio_error {
                text.push_str("  ·  声音不可用（见运行日志）");
                if render_state.last_error.borrow().as_ref() != Some(error) {
                    *render_state.last_error.borrow_mut() = Some(error.clone());
                    let _ = render_tx.try_send(Event::Error(format!("{label} 音频：{error}")));
                }
            }
            status.set_text(&text);
        } else if stats.waiting_for_video_keyframe {
            status.set_text("视频流不完整，正在等待新的关键帧");
        }
        drop(player_ref);
        if auto_close {
            render_state.window.close();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
    let close_state = state.clone();
    let close_input = input_tx.clone();
    window.connect_close_request(move |window| {
        if close_state.closed.replace(true) {
            return glib::Propagation::Stop;
        }
        release_input(&close_state, &close_input);
        window.set_sensitive(false);
        let player = close_state.player.borrow_mut().take();
        let (done_tx, done_rx) = async_channel::bounded(1);
        let _ = close_input.try_send(Input::Close(player, done_tx));
        let window = window.clone();
        glib::MainContext::default().spawn_local(async move {
            let _ = done_rx.recv().await;
            window.destroy();
        });
        glib::Propagation::Stop
    });
    if let Some(display) = gdk::Display::default() {
        let css = gtk::CssProvider::new();
        css.load_from_string(".viewer-surface { background: #111318; border-radius: 12px; } .viewer-preedit { padding: 8px 16px; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    window.present();
    window.upcast()
}
fn spin(value: u32, min: u32, max: u32) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(min as f64, max as f64, 1.0);
    spin.set_value(value as f64);
    spin.set_numeric(true);
    spin.set_width_chars(4);
    spin
}
fn toggle_fullscreen(window: &adw::ApplicationWindow) {
    if window.is_fullscreen() {
        window.unfullscreen();
    } else {
        window.fullscreen();
    }
}
async fn input_worker(
    source: Source,
    rx: async_channel::Receiver<QueuedInput>,
    events: async_channel::Sender<Event>,
    label: String,
) -> anyhow::Result<()> {
    let mut report = tokio::time::interval(Duration::from_secs(1));
    let mut processed = 0_u64;
    let mut queue_peak = 0;
    let mut queue_sum_ms = 0.0_f64;
    let mut queue_max_ms = 0.0_f64;
    let mut send_sum_ms = 0.0_f64;
    let mut send_max_ms = 0.0_f64;
    loop {
        let QueuedInput { input, queued_at } = tokio::select! {
            _ = report.tick() => {
                if processed != 0 {
                    super::logging::write(&format!(
                        "{label} 输入状态：processed={processed} queue_peak={queue_peak} queue_wait_ms={:.2} queue_wait_max_ms={queue_max_ms:.2} send_ms={:.2} send_max_ms={send_max_ms:.2}",
                        queue_sum_ms / processed as f64,
                        send_sum_ms / processed as f64,
                    ));
                    processed = 0;
                    queue_peak = 0;
                    queue_sum_ms = 0.0;
                    queue_max_ms = 0.0;
                    send_sum_ms = 0.0;
                    send_max_ms = 0.0;
                }
                continue;
            }
            input = rx.recv() => match input {
                Ok(input) => input,
                Err(_) => break,
            }
        };
        if let Input::Close(player, done) = input {
            super::logging::write(&format!("{label} 开始关闭媒体和业务"));
            if let Some(player) = player {
                if let Err(error) = player.close().await {
                    let _ = events
                        .send(Event::Error(format!("关闭 {label} 媒体：{error:#}")))
                        .await;
                }
            }
            let result = match &source {
                Source::Screen(screen) => screen.close().await,
                Source::Camera(camera) => camera.close().await,
                Source::File => Ok(()),
            };
            if let Err(error) = result {
                let _ = events
                    .send(Event::Error(format!("关闭 {label} 业务：{error:#}")))
                    .await;
            }
            super::logging::write(&format!("{label} 关闭流程结束"));
            let _ = done.send(()).await;
            return Ok(());
        }
        let Source::Screen(screen) = &source else {
            continue;
        };
        queue_peak = queue_peak.max(rx.len() + 1);
        let queue_ms = queued_at.elapsed().as_secs_f64() * 1000.0;
        queue_sum_ms += queue_ms;
        queue_max_ms = queue_max_ms.max(queue_ms);
        let send_started = Instant::now();
        let result = match input {
            Input::Mouse(action, x, y, state, delta) => {
                screen.mouse(action, x, y, state, delta).await
            }
            Input::Key(code, down, meta) => screen.key(code, down, meta).await,
            Input::Text(text) => screen.commit_text(&text).await,
            Input::Resize(width, height, density) => screen.resize(width, height, density).await,
            Input::Navigation(code) => {
                let down = screen.key(code, true, 0).await;
                let up = screen.key(code, false, 0).await;
                down.and(up)
            }
            Input::Close(..) => unreachable!(),
        };
        let send_ms = send_started.elapsed().as_secs_f64() * 1000.0;
        send_sum_ms += send_ms;
        send_max_ms = send_max_ms.max(send_ms);
        processed += 1;
        if let Err(error) = result {
            let _ = events
                .send(Event::Error(format!("{label} 输入：{error:#}")))
                .await;
        }
    }
    Ok(())
}
fn pointer(
    picture: &gtk::Picture,
    state: &ViewerState,
    screen: &Screen,
    x: f64,
    y: f64,
) -> Option<(i32, i32)> {
    let (iw, ih) = state.image.get();
    if iw == 0 || ih == 0 {
        return None;
    }
    let (w, h) = (picture.width() as f64, picture.height() as f64);
    let scale = (w / iw as f64).min(h / ih as f64);
    if scale <= 0.0 {
        return None;
    }
    let (px, py) = (
        (x - (w - iw as f64 * scale) / 2.0) / (iw as f64 * scale),
        (y - (h - ih as f64 * scale) / 2.0) / (ih as f64 * scale),
    );
    if !(0.0..=1.0).contains(&px) || !(0.0..=1.0).contains(&py) {
        return None;
    }
    let config = screen.configuration();
    Some((
        (px * config.width.saturating_sub(1) as f64).round() as i32,
        (py * config.height.saturating_sub(1) as f64).round() as i32,
    ))
}
fn mouse_meta(modifiers: gdk::ModifierType) -> u32 {
    let mut result = 0;
    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
        result |= desktop::MOUSE_CTRL;
    }
    if modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        result |= desktop::MOUSE_SHIFT;
    }
    if modifiers.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK) {
        result |= desktop::MOUSE_META;
    }
    result
}
fn key_meta(modifiers: gdk::ModifierType, shift: bool, code: u32) -> u32 {
    let mut result = 0;
    if shift || modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        result |= 1;
    }
    if modifiers.contains(gdk::ModifierType::ALT_MASK) {
        result |= 2;
    }
    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
        result |= 0x1000;
    }
    if modifiers.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK) {
        result |= 0x10000;
    }
    if modifiers.contains(gdk::ModifierType::LOCK_MASK) {
        result |= 0x100000;
    }
    if (144..=153).contains(&code) || code == 158 {
        result |= 0x200000;
    }
    result
}
fn release_input(state: &ViewerState, sender: &InputSender) {
    let mut input = state.input.borrow_mut();
    input.scroll = [0.0; 2];
    for (_, code, _) in input.keys.drain(..).rev() {
        let _ = sender.try_send(Input::Key(code, false, 0));
    }
    if let Some((x, y)) = input.point {
        for (bit, action) in [
            (desktop::LEFT_HELD, MouseAction::LeftUp),
            (desktop::RIGHT_HELD, MouseAction::RightUp),
        ] {
            if input.buttons & bit != 0 {
                input.buttons &= !bit;
                let _ = sender.try_send(Input::Mouse(action, x, y, input.buttons, 0));
            }
        }
    }
}

fn flush_scroll(state: &ViewerState, sender: &InputSender) {
    let mut input = state.input.borrow_mut();
    let Some((x, y)) = input.point else {
        return;
    };
    let flags = input.scroll_flags;
    for (axis, negative, positive) in [
        (0, MouseAction::WheelLeft, MouseAction::WheelRight),
        (1, MouseAction::WheelForward, MouseAction::WheelBackward),
    ] {
        let mut delta = input.scroll[axis].trunc() as i32;
        input.scroll[axis] -= delta as f64;
        while delta != 0 {
            let chunk = delta.clamp(-100, 100);
            let _ = sender.try_send(Input::Mouse(
                if chunk < 0 { negative } else { positive },
                x,
                y,
                flags,
                chunk.abs(),
            ));
            delta -= chunk;
        }
    }
}

fn attach_input(
    picture: &gtk::Picture,
    preedit: &gtk::Label,
    window: &adw::ApplicationWindow,
    screen: &Screen,
    state: Rc<ViewerState>,
    sender: InputSender,
) {
    let click = gtk::GestureClick::new();
    click.set_button(0);
    let (p, s, c, t) = (
        picture.clone(),
        state.clone(),
        screen.clone(),
        sender.clone(),
    );
    click.connect_pressed(move |gesture, _count, x, y| {
        let Some((x, y)) = pointer(&p, &s, &c, x, y) else {
            return;
        };
        p.grab_focus();
        let (bit, action) = match gesture.current_button() {
            1 => (desktop::LEFT_HELD, MouseAction::LeftDown),
            3 => (desktop::RIGHT_HELD, MouseAction::RightDown),
            _ => return,
        };
        let mut input = s.input.borrow_mut();
        input.buttons |= bit;
        input.point = Some((x, y));
        let flags = input.buttons | mouse_meta(gesture.current_event_state());
        let _ = t.try_send(Input::Mouse(action, x, y, flags, 0));
    });
    let (p, s, c, t) = (
        picture.clone(),
        state.clone(),
        screen.clone(),
        sender.clone(),
    );
    click.connect_released(move |gesture, _count, x, y| {
        let (bit, action) = match gesture.current_button() {
            1 => (desktop::LEFT_HELD, MouseAction::LeftUp),
            3 => (desktop::RIGHT_HELD, MouseAction::RightUp),
            _ => return,
        };
        let point = pointer(&p, &s, &c, x, y).or(s.input.borrow().point);
        let mut input = s.input.borrow_mut();
        if input.buttons & bit == 0 {
            return;
        }
        input.buttons &= !bit;
        if let Some((x, y)) = point {
            input.point = Some((x, y));
            let flags = input.buttons | mouse_meta(gesture.current_event_state());
            let _ = t.try_send(Input::Mouse(action, x, y, flags, 0));
        }
    });
    picture.add_controller(click);
    let motion = gtk::EventControllerMotion::new();
    let (p, s, c, t) = (
        picture.clone(),
        state.clone(),
        screen.clone(),
        sender.clone(),
    );
    motion.connect_motion(move |controller, x, y| {
        if let Some((x, y)) = pointer(&p, &s, &c, x, y) {
            let mut input = s.input.borrow_mut();
            input.point = Some((x, y));
            let flags = input.buttons | mouse_meta(controller.current_event_state());
            let _ = t.try_send(Input::Mouse(MouseAction::Move, x, y, flags, 0));
        }
    });
    picture.add_controller(motion);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let (p, s, c) = (picture.clone(), state.clone(), screen.clone());
    scroll.connect_scroll(move |controller, dx, dy| {
        let mut input = s.input.borrow_mut();
        if input.point.is_none() {
            return glib::Propagation::Proceed;
        }
        let units = match controller.unit() {
            gdk::ScrollUnit::Wheel => [PROTOCOL_SCROLL_UNITS_PER_AXIS; 2],
            gdk::ScrollUnit::Surface => {
                let (width, height) = s.image.get();
                let scale =
                    (p.width() as f64 / width as f64).min(p.height() as f64 / height as f64);
                if scale <= 0.0 {
                    return glib::Propagation::Proceed;
                }
                let config = c.configuration();
                let factor = PROTOCOL_SCROLL_UNITS_PER_AXIS / DEFAULT_REMOTE_SCROLL_PIXELS_PER_AXIS;
                [
                    config.width as f64 / (width as f64 * scale) * factor,
                    config.height as f64 / (height as f64 * scale) * factor,
                ]
            }
            _ => return glib::Propagation::Proceed,
        };
        input.scroll[0] += dx * units[0];
        input.scroll[1] += dy * units[1];
        input.scroll_flags = input.buttons | mouse_meta(controller.current_event_state());
        glib::Propagation::Stop
    });
    picture.add_controller(scroll);
    let im = gtk::IMMulticontext::new();
    im.set_client_widget(Some(picture));
    im.set_use_preedit(true);
    let (t, focused_picture) = (sender.clone(), picture.clone());
    im.connect_commit(move |_, text| {
        if focused_picture.has_focus() && !text.is_empty() {
            let _ = t.try_send(Input::Text(text.into()));
        }
    });
    let label = preedit.clone();
    im.connect_preedit_changed(move |context| {
        let (text, _, _) = context.preedit_string();
        label.set_text(&text);
        label.set_visible(!text.is_empty());
    });
    let keyboard = gtk::EventControllerKey::new();
    keyboard.set_propagation_phase(gtk::PropagationPhase::Capture);
    keyboard.set_im_context(Some(&im));
    let (s, t, weak_window) = (state.clone(), sender.clone(), window.downgrade());
    keyboard.connect_key_pressed(move |_, key, keycode, modifiers| {
        if key == gdk::Key::F11 {
            if let Some(window) = weak_window.upgrade() {
                toggle_fullscreen(&window);
            }
            return glib::Propagation::Stop;
        }
        let Some(name) = key.name() else {
            return glib::Propagation::Proceed;
        };
        let Some((code, shift)) = desktop::android_key(&name) else {
            return glib::Propagation::Proceed;
        };
        let mut input = s.input.borrow_mut();
        if !input
            .keys
            .iter()
            .any(|(hardware, _, _)| *hardware == keycode)
        {
            input.keys.push((keycode, code, shift));
        }
        let _ = t.try_send(Input::Key(code, true, key_meta(modifiers, shift, code)));
        glib::Propagation::Stop
    });
    let (s, t) = (state.clone(), sender.clone());
    keyboard.connect_key_released(move |_, _, keycode, modifiers| {
        let mut input = s.input.borrow_mut();
        if let Some(index) = input
            .keys
            .iter()
            .position(|(hardware, _, _)| *hardware == keycode)
        {
            let (_, code, shift) = input.keys.remove(index);
            let _ = t.try_send(Input::Key(code, false, key_meta(modifiers, shift, code)));
        }
    });
    picture.add_controller(keyboard);
    let focus = gtk::EventControllerFocus::new();
    let (context, p) = (im.clone(), picture.clone());
    focus.connect_enter(move |_| {
        context.focus_in();
        context.set_cursor_location(&gdk::Rectangle::new(16, (p.height() - 48).max(0), 1, 24));
    });
    let (s, t, label) = (state.clone(), sender.clone(), preedit.clone());
    focus.connect_leave(move |_| {
        release_input(&s, &t);
        im.reset();
        im.focus_out();
        label.set_visible(false);
    });
    picture.add_controller(focus);
    window.connect_is_active_notify(move |window| {
        if !window.is_active() {
            release_input(&state, &sender);
        }
    });
}

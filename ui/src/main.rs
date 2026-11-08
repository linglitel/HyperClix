mod logging;
mod native_clipboard;
mod nearby;
mod selection;
mod system_notifications;
mod viewer;
use selection::{SelectionMonitor, SelectionStatus};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use hyperclix_core::{
    Account, ConnectOptions, QrLogin, Result, Session,
    apps::PhoneApp,
    camera::{CameraOptions, CameraPreview},
    clipboard::Clipboard,
    desktop::{AppOptions, Screen},
    discovery::{self, Announcement, AnnouncementOptions, DeviceEndpoint},
    media::{MediaPlayer, PlaybackOptions},
    mishare::{MiShare, TransferEvent, TransferTask},
    notifications::{
        self, CloudClientInfo, CloudCredentials, NotificationReceiver, PhoneNotification,
        PushAppCredentials, PushRuntime,
    },
    proximity::BleDevice,
};
use std::{
    cell::RefCell, future::Future, net::Ipv4Addr, path::PathBuf, rc::Rc, sync::Arc, time::Duration,
};
use tokio::{runtime::Runtime, sync::watch};

enum Event {
    Log(String),
    Error(String),
    Account(Result<Account>),
    Devices(Vec<DeviceEndpoint>),
    NearbyDevices(Result<Vec<BleDevice>>),
    NearbyStatus(u64, String),
    Incoming(u64, Session),
    NearbyStopped(u64, Result<()>),
    Connection(Result<Session>),
    Disconnected,
    Scoped(u64, Box<Event>, tokio::sync::oneshot::Sender<()>),
    Notice(u64, Box<Event>),
    Qr(u64, &'static str, PathBuf, u64),
    LoginFinished(u64, &'static str, Result<String>),
    CancelLogin(u64),
    Screen(Screen, MediaPlayer),
    Camera(Arc<CameraPreview>, MediaPlayer),
    Clipboard(Clipboard, SelectionMonitor),
    SelectionText(u64, String),
    SelectionStopped(u64, Option<String>),
    ClipboardFailed(String),
    Transfer(TransferTask),
    TransferUpdate(TransferEvent),
    Apps(Result<Vec<PhoneApp>>),
    Notification(PhoneNotification),
    NotificationReady,
    NotificationAuthRequired,
    NotificationStopped,
    Announcement(u64, Result<Announcement>, tokio::sync::oneshot::Sender<()>),
    AnnouncementStopped,
    ShutdownRequest,
    Shutdown,
}
#[derive(Default)]
struct Model {
    account: Option<Account>,
    session: Option<Session>,
    clipboard: Option<Clipboard>,
    selection: Option<SelectionMonitor>,
    clipboard_pending: bool,
    last_remote: String,
    transfers: Vec<TransferTask>,
    notification_stop: Option<watch::Sender<bool>>,
    receiver_stop: Option<watch::Sender<bool>>,
    announcement: Option<Announcement>,
    windows: Vec<gtk::Window>,
    closing: bool,
    connecting: bool,
    disconnecting: bool,
    generation: u64,
    pending: Vec<tokio::task::JoinHandle<()>>,
    connection_cancel: Option<watch::Sender<bool>>,
    login_task: Option<tokio::task::AbortHandle>,
    login_generation: u64,
    login_window: Option<gtk::Window>,
    announcement_generation: u64,
    announcement_task: Option<tokio::task::JoinHandle<()>>,
    account_pending: bool,
    quit_after_disconnect: bool,
    nearby_scan_pending: bool,
    nearby_generation: u64,
    nearby: Option<nearby::Background>,
    nearby_stopping: bool,
    nearby_connection: bool,
}
struct Ui {
    app: adw::Application,
    window: adw::ApplicationWindow,
    runtime: Arc<Runtime>,
    tx: async_channel::Sender<Event>,
    model: RefCell<Model>,
    toast: adw::ToastOverlay,
    status: gtk::Label,
    account_status: gtk::Label,
    stack: gtk::Stack,
    log: gtk::TextBuffer,
    account_path: gtk::Entry,
    name: gtk::Entry,
    interface: gtk::DropDown,
    interfaces: Vec<discovery::NetworkInterface>,
    peer: gtk::Entry,
    port: gtk::Entry,
    devices: gtk::ListBox,
    wifi_interface: gtk::DropDown,
    nearby_status: gtk::Label,
    nearby_devices: gtk::ListBox,
    nearby_scan: gtk::Button,
    nearby_start: gtk::Button,
    nearby_stop: gtk::Button,
    nearby_refresh: gtk::Button,
    package: gtk::Entry,
    app_names: gtk::StringList,
    app_cache: Rc<RefCell<Vec<PhoneApp>>>,
    width: gtk::SpinButton,
    height: gtk::SpinButton,
    density: gtk::SpinButton,
    audio: gtk::Switch,
    audio_device: gtk::Entry,
    clipboard_switch: gtk::Switch,
    clipboard_text: gtk::TextBuffer,
    file_paths: RefCell<Vec<PathBuf>>,
    files_label: gtk::Label,
    destination: gtk::Entry,
    transfer_label: gtk::Label,
    progress: gtk::ProgressBar,
    lens: gtk::DropDown,
    camera_width: gtk::SpinButton,
    camera_height: gtk::SpinButton,
    v4l2: gtk::Entry,
    v4l2_enabled: gtk::Switch,
    packages: gtk::Entry,
    cloud_path: gtk::Entry,
    push_path: gtk::Entry,
    push_app: gtk::Entry,
    cloud_state: gtk::Entry,
    notification_list: gtk::ListBox,
    notification_status: gtk::Label,
}
fn entry(text: &str, placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .text(text)
        .placeholder_text(placeholder)
        .hexpand(true)
        .build()
}
fn button(label: &str, primary: bool) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    if primary {
        b.add_css_class("suggested-action");
    }
    b
}
fn row(label: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let r = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    let l = gtk::Label::new(Some(label));
    l.set_xalign(0.0);
    l.set_width_chars(12);
    r.append(&l);
    r.append(child);
    r
}
fn card() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 14);
    b.add_css_class("card");
    b
}
fn page(title: &str, subtitle: &str) -> (gtk::ScrolledWindow, gtk::Box) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 22);
    content.set_margin_start(30);
    content.set_margin_end(30);
    content.set_margin_top(28);
    content.set_margin_bottom(30);
    let t = gtk::Label::new(Some(title));
    t.add_css_class("title-1");
    t.set_xalign(0.0);
    content.append(&t);
    let s = gtk::Label::new(Some(subtitle));
    s.add_css_class("dim-label");
    s.set_wrap(true);
    s.set_xalign(0.0);
    content.append(&s);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    (scroll, content)
}
fn number(value: f64, min: f64, max: f64, step: f64) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, step);
    s.set_value(value);
    s
}
fn actions(buttons: &[&gtk::Button]) -> gtk::Box {
    let r = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    for b in buttons {
        r.append(*b);
    }
    r
}
fn text_view(buffer: &gtk::TextBuffer, height: i32) -> gtk::ScrolledWindow {
    let text = gtk::TextView::with_buffer(buffer);
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.set_left_margin(10);
    text.set_right_margin(10);
    text.set_top_margin(10);
    text.set_bottom_margin(10);
    gtk::ScrolledWindow::builder()
        .min_content_height(height)
        .child(&text)
        .build()
}

impl Ui {
    fn build(app: &adw::Application, runtime: Arc<Runtime>) -> Rc<Self> {
        let (tx, rx) = async_channel::unbounded();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("HyperClix")
            .default_width(1120)
            .default_height(780)
            .build();
        let toast = adw::ToastOverlay::new();
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(
            "HyperClix",
            "设备互联 · Linux",
        )));
        layout.append(&header);
        let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        split.set_vexpand(true);
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 14);
        sidebar.set_width_request(212);
        sidebar.add_css_class("navigation-sidebar");
        sidebar.set_margin_top(18);
        let brand = gtk::Image::from_icon_name("phone-symbolic");
        brand.set_pixel_size(40);
        sidebar.append(&brand);
        let label = gtk::Label::new(Some("连接，随手可用"));
        label.add_css_class("heading");
        sidebar.append(&label);
        let status = gtk::Label::new(Some("● 未连接"));
        status.add_css_class("dim-label");
        status.set_wrap(true);
        status.set_margin_start(12);
        status.set_margin_end(12);
        sidebar.append(&status);
        let stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let nav = gtk::StackSidebar::new();
        nav.set_stack(&stack);
        nav.set_vexpand(true);
        sidebar.append(&nav);
        let platform = gtk::Label::new(Some("Linux"));
        platform.add_css_class("caption");
        platform.add_css_class("dim-label");
        platform.set_margin_bottom(18);
        sidebar.append(&platform);
        split.append(&sidebar);
        split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        split.append(&stack);
        layout.append(&split);
        toast.set_child(Some(&layout));
        window.set_content(Some(&toast));

        let (connection_page, connection) = page(
            "我的设备",
            "加载已有身份后，选择蓝牙近场或局域网连接。蓝牙近场无需手机与电脑处于同一 Wi-Fi。",
        );
        let account_card = card();
        let account_path = entry(
            &hyperclix_core::paths::account_dir().to_string_lossy(),
            "账号与设备证书目录",
        );
        account_card.append(&row("本机身份", &account_path));
        let account_status = gtk::Label::new(Some("尚未加载本机证书"));
        account_status.set_xalign(0.0);
        account_status.add_css_class("dim-label");
        account_card.append(&account_status);
        let load = button("加载已有身份", true);
        let login = button("扫码登录互联", false);
        let register = button("签发本机证书", false);
        account_card.append(&actions(&[&load, &login, &register]));
        let name = entry(hyperclix_core::DEFAULT_DEVICE_NAME, "电脑显示名称");
        account_card.append(&row("电脑名称", &name));
        connection.append(&account_card);
        let proximity = card();
        let title = gtk::Label::new(Some("蓝牙与 Wi-Fi Direct"));
        title.add_css_class("title-3");
        title.set_xalign(0.0);
        proximity.append(&title);
        let hint = gtk::Label::new(Some(
            "蓝牙负责发现和认证，随后自动建立 Wi-Fi Direct 传输画面和文件。请保持电脑 Wi-Fi 与蓝牙开启；无需填写手机 IP。",
        ));
        hint.set_wrap(true);
        hint.set_xalign(0.0);
        hint.add_css_class("dim-label");
        proximity.append(&hint);
        let wifi_names = nearby::wireless_interfaces();
        let wifi_labels: Vec<&str> = wifi_names.iter().map(String::as_str).collect();
        let wifi_interface = gtk::DropDown::from_strings(&wifi_labels);
        wifi_interface.set_hexpand(true);
        let nearby_refresh = button("刷新网卡", false);
        let wifi_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        wifi_row.append(&wifi_interface);
        wifi_row.append(&nearby_refresh);
        proximity.append(&row("Wi-Fi 网卡", &wifi_row));
        let nearby_scan = button("蓝牙发现手机", true);
        let nearby_start = button("允许手机主动连接", false);
        let nearby_stop = button("停止后台监听", false);
        nearby_stop.set_sensitive(false);
        proximity.append(&actions(&[&nearby_scan, &nearby_start]));
        let nearby_disconnect = button("断开当前连接", false);
        proximity.append(&actions(&[&nearby_disconnect, &nearby_stop]));
        let nearby_status = gtk::Label::new(Some("近场服务未启动"));
        nearby_status.set_xalign(0.0);
        nearby_status.set_wrap(true);
        nearby_status.set_selectable(true);
        proximity.append(&nearby_status);
        let nearby_devices = gtk::ListBox::new();
        nearby_devices.set_selection_mode(gtk::SelectionMode::None);
        nearby_devices.add_css_class("boxed-list");
        proximity.append(&nearby_devices);
        connection.append(&proximity);
        let net = card();
        let title = gtk::Label::new(Some("局域网连接"));
        title.add_css_class("title-3");
        title.set_xalign(0.0);
        net.append(&title);
        let interfaces = discovery::interfaces().unwrap_or_default();
        let names: Vec<String> = interfaces
            .iter()
            .map(|i| format!("{} · {}", i.name, i.address))
            .collect();
        let labels: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
        let interface = gtk::DropDown::from_strings(&labels);
        interface.set_hexpand(true);
        net.append(&row("局域网接口", &interface));
        let discover = button("局域网发现手机", true);
        let announce = button("发布电脑公告", false);
        let stop_announce = button("停止公告", false);
        net.append(&actions(&[&discover, &announce, &stop_announce]));
        let devices = gtk::ListBox::new();
        devices.set_selection_mode(gtk::SelectionMode::None);
        devices.add_css_class("boxed-list");
        net.append(&devices);
        let peer = entry("", "手机 IP");
        let port = entry("", "KCP 端口");
        port.set_width_chars(8);
        let endpoints = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        endpoints.append(&peer);
        endpoints.append(&port);
        net.append(&row("连接端点", &endpoints));
        let connect = button("连接手机", true);
        let disconnect = button("断开连接", false);
        net.append(&actions(&[&connect, &disconnect]));
        connection.append(&net);
        stack.add_titled(&connection_page, Some("devices"), "我的设备");

        let (desktop_page, desktop) = page(
            "桌面与应用",
            "在独立窗口中操作手机。画布尺寸与 Android density 可调，画面按原始解码尺寸显示。",
        );
        let desk = card();
        let open_desktop = button("打开手机桌面", true);
        let local_capture = button("打开本地媒体", false);
        desk.append(&actions(&[&open_desktop, &local_capture]));
        let audio = gtk::Switch::builder()
            .active(true)
            .halign(gtk::Align::Start)
            .build();
        desk.append(&row("播放手机声音", &audio));
        let audio_device = entry("auto", "音频输出设备");
        desk.append(&row("音频输出", &audio_device));
        let audio_hint = gtk::Label::new(Some(
            "auto / pipewire / pulse / default，也可填写 ALSA 设备名",
        ));
        audio_hint.set_xalign(0.0);
        audio_hint.set_wrap(true);
        audio_hint.add_css_class("dim-label");
        audio_hint.add_css_class("caption");
        desk.append(&audio_hint);
        desktop.append(&desk);
        let apps = card();
        let package = entry("org.videolan.vlc", "Android App 包名");
        apps.append(&row("应用包名", &package));
        let width = number(720.0, 160.0, 7680.0, 40.0);
        let height = number(1280.0, 160.0, 7680.0, 40.0);
        let density = number(320.0, 120.0, 960.0, 40.0);
        let size = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        size.append(&width);
        size.append(&gtk::Label::new(Some("×")));
        size.append(&height);
        size.append(&gtk::Label::new(Some("density")));
        size.append(&density);
        apps.append(&row("应用画布", &size));
        let app_names = gtk::StringList::new(&[]);
        let app_dropdown = gtk::DropDown::new(Some(app_names.clone()), None::<gtk::Expression>);
        app_dropdown.set_hexpand(true);
        apps.append(&row("已安装应用", &app_dropdown));
        let app_cache = Rc::new(RefCell::new(Vec::<PhoneApp>::new()));
        let app_cache_choice = app_cache.clone();
        let package_choice = package.clone();
        app_dropdown.connect_selected_notify(move |dropdown| {
            if let Some(app) = app_cache_choice.borrow().get(dropdown.selected() as usize) {
                package_choice.set_text(&app.package);
            }
        });
        let load_apps = button("加载最近应用", false);
        let open_app = button("打开独立 App", true);
        apps.append(&actions(&[&load_apps, &open_app]));
        desktop.append(&apps);
        stack.add_titled(&desktop_page, Some("desktop"), "桌面与应用");

        let (clipboard_page, clipboard_page_box) = page(
            "跨设备剪贴板",
            "同步新复制的文本、HTML、图片与文件。下方显示文本内容，也可手动发送文本。",
        );
        let clip = card();
        let clipboard_switch = gtk::Switch::builder().halign(gtk::Align::Start).build();
        clip.append(&row("双向同步", &clipboard_switch));
        let clipboard_text = gtk::TextBuffer::new(None);
        clip.append(&text_view(&clipboard_text, 160));
        let send_text = button("发送文本到手机", true);
        clip.append(&send_text);
        clipboard_page_box.append(&clip);
        stack.add_titled(&clipboard_page, Some("clipboard"), "剪贴板");

        let (files_page, files) = page(
            "文件互传",
            "选择或拖入文件发送到手机。连接设备后自动接收手机发来的文件；接收目录在连接时生效。",
        );
        let filecard = card();
        let files_label = gtk::Label::new(Some("将文件拖到此处，或点击选择文件"));
        files_label.set_wrap(true);
        files_label.set_xalign(0.0);
        files_label.set_margin_top(18);
        files_label.set_margin_bottom(18);
        filecard.append(&files_label);
        let choose = button("选择文件", false);
        let send_files = button("发送到手机", true);
        filecard.append(&actions(&[&choose, &send_files]));
        let destination = entry(
            &hyperclix_core::paths::received_dir().to_string_lossy(),
            "接收文件夹",
        );
        filecard.append(&row("接收目录", &destination));
        let cancel = button("取消当前任务", false);
        filecard.append(&cancel);
        let progress = gtk::ProgressBar::new();
        progress.set_show_text(true);
        progress.set_text(Some("尚无传输任务"));
        filecard.append(&progress);
        let transfer_label = gtk::Label::new(None);
        transfer_label.set_wrap(true);
        transfer_label.set_xalign(0.0);
        transfer_label.add_css_class("dim-label");
        filecard.append(&transfer_label);
        files.append(&filecard);
        stack.add_titled(&files_page, Some("files"), "文件互传");

        let (camera_page, camera) = page(
            "跨设备相机",
            "在电脑预览手机镜头；打开虚拟摄像头输出后，可在 OBS 和浏览器中选择对应视频设备。",
        );
        let cam = card();
        let lens = gtk::DropDown::from_strings(&["后置镜头", "前置镜头"]);
        cam.append(&row("手机镜头", &lens));
        let camera_width = number(1440.0, 160.0, 3840.0, 160.0);
        let camera_height = number(1080.0, 120.0, 2160.0, 120.0);
        let csize = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        csize.append(&camera_width);
        csize.append(&gtk::Label::new(Some("×")));
        csize.append(&camera_height);
        cam.append(&row("预览尺寸", &csize));
        let v4l2_enabled = gtk::Switch::builder().halign(gtk::Align::Start).build();
        cam.append(&row("虚拟摄像头", &v4l2_enabled));
        let v4l2 = entry("/dev/video42", "已创建的 v4l2loopback 设备");
        cam.append(&row("输出设备", &v4l2));
        let open_camera = button("打开相机预览", true);
        cam.append(&open_camera);
        camera.append(&cam);
        stack.add_titled(&camera_page, Some("camera"), "手机相机");

        let (notifications_page, notifs) = page(
            "手机通知",
            "配置手机订阅后接收新通知。推送身份与云账号授权各自独立保存。",
        );
        let notification_card = card();
        let packages = entry("com.tencent.mm", "包名以逗号分隔");
        notification_card.append(&row("应用包名", &packages));
        let cloud_path = entry(
            &hyperclix_core::paths::cloud_dir().to_string_lossy(),
            "micloud 授权目录",
        );
        notification_card.append(&row("云账号目录", &cloud_path));
        let push_path = entry(
            &hyperclix_core::paths::push_path().to_string_lossy(),
            "自有 MiPush runtime 文件",
        );
        notification_card.append(&row("推送身份", &push_path));
        let start_notifications = button("配置订阅并接收", true);
        let stop_notifications = button("停止接收", false);
        let cloud_login = button("扫码授权通知", false);
        notification_card.append(&actions(&[
            &start_notifications,
            &stop_notifications,
            &cloud_login,
        ]));
        let notification_status =
            gtk::Label::new(Some("通知需要单独的云授权；普通互联登录不会更新此授权。"));
        notification_status.set_xalign(0.0);
        notification_status.set_wrap(true);
        notification_status.add_css_class("dim-label");
        notification_card.append(&notification_status);
        notifs.append(&notification_card);
        let advanced = gtk::Expander::new(Some("首次使用 · 推送与云设备登记"));
        let reg = card();
        let push_app = entry("", "已有推送身份可直接复用；首次登记选择应用凭据 JSON");
        let choose_push_app = button("选择文件", false);
        let app_input = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        app_input.append(&push_app);
        app_input.append(&choose_push_app);
        reg.append(&row("应用凭据文件", &app_input));
        let cloud_state = entry(
            &hyperclix_core::paths::cloud_state_dir().to_string_lossy(),
            "云设备状态目录",
        );
        reg.append(&row("云设备目录", &cloud_state));
        let register_push = button("登记推送身份", false);
        let register_cloud = button("登记云设备", false);
        reg.append(&actions(&[&register_push, &register_cloud]));
        advanced.set_child(Some(&reg));
        notifs.append(&advanced);
        let notification_list = gtk::ListBox::new();
        notification_list.set_selection_mode(gtk::SelectionMode::None);
        notification_list.add_css_class("boxed-list");
        notifs.append(&notification_list);
        stack.add_titled(&notifications_page, Some("notifications"), "手机通知");
        let (logs_page, logs) = page(
            "运行日志",
            "查看连接阶段、业务进度和具体错误。凭据与消息正文不写入运行日志。",
        );
        let log = gtk::TextBuffer::new(None);
        let view = text_view(&log, 400);
        view.set_vexpand(true);
        if let Some(text) = view.child().and_downcast::<gtk::TextView>() {
            text.set_editable(false);
            text.set_monospace(true);
        }
        logs.append(&view);
        let clear = button("清空日志", false);
        logs.append(&clear);
        if let Some(path) = logging::path() {
            let location = gtk::Label::new(Some(&path.to_string_lossy()));
            location.set_selectable(true);
            location.set_wrap(true);
            location.set_xalign(0.0);
            logs.append(&location);
            let open_log = button("打开当前日志文件", false);
            let parent = window.clone();
            let file = gio::File::for_path(path);
            open_log.connect_clicked(move |_| {
                let launcher = gtk::FileLauncher::new(Some(&file));
                let parent = parent.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) = launcher.launch_future(Some(&parent)).await {
                        logging::write(&format!("打开日志文件：{error}"));
                    }
                });
            });
            logs.append(&open_log);
        }
        stack.add_titled(&logs_page, Some("logs"), "运行日志");
        clipboard_switch.set_sensitive(false);
        let ui = Rc::new(Self {
            app: app.clone(),
            window,
            runtime,
            tx,
            model: RefCell::new(Model::default()),
            toast,
            status,
            account_status,
            stack,
            log,
            account_path,
            name,
            interface,
            interfaces,
            peer,
            port,
            devices,
            wifi_interface,
            nearby_status,
            nearby_devices,
            nearby_scan: nearby_scan.clone(),
            nearby_start: nearby_start.clone(),
            nearby_stop: nearby_stop.clone(),
            nearby_refresh: nearby_refresh.clone(),
            package,
            app_names,
            app_cache,
            width,
            height,
            density,
            audio,
            audio_device,
            clipboard_switch,
            clipboard_text,
            file_paths: RefCell::new(Vec::new()),
            files_label,
            destination,
            transfer_label,
            progress,
            lens,
            camera_width,
            camera_height,
            v4l2,
            v4l2_enabled,
            packages,
            cloud_path,
            push_path,
            push_app,
            cloud_state,
            notification_list,
            notification_status,
        });
        let weak = Rc::downgrade(&ui);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                ui.handle(event);
            }
        });
        macro_rules! click {
            ($button:ident,$u:ident,$body:block) => {{
                let owner = ui.clone();
                $button.connect_clicked(move |_| {
                    let $u = owner.clone();
                    $body
                });
            }};
        }
        click!(load, u, {
            u.load_account(false);
        });
        click!(login, u, {
            u.start_login(u.account_path.text().into(), "miconnect");
        });
        click!(register, u, {
            u.load_account(true);
        });
        click!(nearby_scan, u, {
            u.scan_nearby();
        });
        click!(nearby_start, u, {
            u.start_nearby();
        });
        click!(nearby_stop, u, {
            u.stop_nearby();
        });
        click!(nearby_disconnect, u, {
            u.append_log("手动断开当前设备会话");
            u.disconnect();
        });
        click!(nearby_refresh, u, {
            let names = nearby::wireless_interfaces();
            let labels: Vec<&str> = names.iter().map(String::as_str).collect();
            u.wifi_interface
                .set_model(Some(&gtk::StringList::new(&labels)));
            if names.is_empty() {
                u.error("没有发现可用的 Wi-Fi 网卡，请先开启 Wi-Fi");
            }
        });
        click!(discover, u, {
            if let Some(ip) = u.local_ip() {
                u.status.set_text("正在发现手机…");
                u.run("发现设备", async move {
                    Ok(Event::Devices(
                        discovery::discover(ip, Duration::from_secs(4)).await?,
                    ))
                });
            }
        });
        click!(connect, u, {
            u.connect_phone();
        });
        click!(disconnect, u, {
            u.append_log("手动断开当前设备会话");
            u.disconnect();
        });
        click!(announce, u, {
            u.start_announcement();
        });
        click!(stop_announce, u, {
            u.stop_announcement();
        });
        click!(open_desktop, u, {
            u.open_screen(false);
        });
        click!(load_apps, u, {
            u.load_apps();
        });
        click!(open_app, u, {
            u.open_screen(true);
        });
        click!(local_capture, u, {
            let dialog = gtk::FileDialog::builder().title("打开本地媒体").build();
            let parent = u.window.clone();
            glib::spawn_future_local(async move {
                if let Ok(file) = dialog.open_future(Some(&parent)).await {
                    if let Some(path) = file.path() {
                        if u.model.borrow().closing || u.model.borrow().disconnecting {
                            return;
                        }
                        let rt = u.runtime.enter();
                        let player = MediaPlayer::play_file(
                            path,
                            PlaybackOptions {
                                audio: u.audio.is_active(),
                                audio_device: u.audio_device.text().trim().to_owned(),
                                ..Default::default()
                            },
                        );
                        drop(rt);
                        let w = viewer::open_file(
                            &u.app,
                            &u.window,
                            u.runtime.clone(),
                            u.tx.clone(),
                            player,
                        );
                        u.model.borrow_mut().windows.push(w);
                    }
                }
            });
        });
        {
            let u = ui.clone();
            ui.clipboard_switch.connect_state_set(move |_, enabled| {
                u.toggle_clipboard(enabled);
                glib::Propagation::Proceed
            });
        }
        click!(send_text, u, {
            let text = u
                .clipboard_text
                .text(
                    &u.clipboard_text.start_iter(),
                    &u.clipboard_text.end_iter(),
                    false,
                )
                .to_string();
            if let Some(clip) = u.model.borrow().clipboard.clone() {
                u.run("发送剪贴板", async move {
                    clip.send_text(&text).await?;
                    Ok(Event::Log("文本已发送到手机".into()))
                });
            } else {
                u.error("请先开启剪贴板同步");
            }
        });
        click!(choose, u, {
            let dialog = gtk::FileDialog::builder().title("选择发送文件").build();
            let parent = u.window.clone();
            glib::spawn_future_local(async move {
                if let Ok(list) = dialog.open_multiple_future(Some(&parent)).await {
                    let paths = (0..list.n_items())
                        .filter_map(|i| {
                            list.item(i)
                                .and_downcast::<gio::File>()
                                .and_then(|f| f.path())
                        })
                        .collect();
                    u.set_files(paths);
                }
            });
        });
        {
            let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
            let u = ui.clone();
            drop.connect_drop(move |_, value, _, _| {
                if let Ok(list) = value.get::<gdk::FileList>() {
                    u.set_files(list.files().iter().filter_map(|f| f.path()).collect());
                    true
                } else {
                    false
                }
            });
            filecard.add_controller(drop);
        }
        click!(send_files, u, {
            u.start_transfer();
        });
        click!(cancel, u, {
            for task in &u.model.borrow().transfers {
                task.cancel();
            }
        });
        click!(open_camera, u, {
            u.open_camera();
        });
        click!(cloud_login, u, {
            u.start_login(u.cloud_path.text().into(), "micloud");
        });
        click!(start_notifications, u, {
            u.start_notifications();
        });
        click!(stop_notifications, u, {
            if let Some(stop) = u.model.borrow().notification_stop.as_ref() {
                stop.send_replace(true);
                u.notification_status.set_text("正在停止通知接收…");
            }
        });
        click!(choose_push_app, u, {
            let dialog = gtk::FileDialog::builder()
                .title("选择自有推送应用凭据")
                .build();
            let parent = u.window.clone();
            glib::spawn_future_local(async move {
                if let Ok(file) = dialog.open_future(Some(&parent)).await {
                    if let Some(path) = file.path() {
                        u.push_app.set_text(&path.to_string_lossy());
                    }
                }
            });
        });
        click!(register_push, u, {
            let path = u.push_app.text().to_string();
            let output = u.push_path.text().to_string();
            u.run("登记推送身份", async move {
                let app = if tokio::fs::try_exists(&output).await? {
                    let runtime = PushRuntime::load(&output).await?;
                    PushAppCredentials {
                        app_id: runtime.app_id,
                        app_token: runtime.app_token,
                        package: runtime.package,
                    }
                } else {
                    anyhow::ensure!(
                        !path.trim().is_empty(),
                        "首次登记请先选择推送应用凭据 JSON 文件；普通互联账号凭据不用于此步骤"
                    );
                    let bytes = tokio::fs::read(&path).await.map_err(|e| {
                        anyhow::anyhow!("读取推送应用凭据失败，请使用“选择文件”指定已有文件：{e}")
                    })?;
                    serde_json::from_slice::<PushAppCredentials>(&bytes)?
                };
                notifications::register_push(&app, output).await?;
                Ok(Event::Log("推送身份登记完成".into()))
            });
        });
        click!(register_cloud, u, {
            if let Some(session) = u.session() {
                let credentials = u.cloud_path.text().to_string();
                let runtime = u.push_path.text().to_string();
                let state = u.cloud_state.text().to_string();
                u.run("登记云设备", async move {
                    let credentials = CloudCredentials::load(credentials).await?;
                    let runtime = PushRuntime::load(runtime).await?;
                    notifications::register_cloud(
                        &session,
                        &credentials,
                        &runtime,
                        state,
                        CloudClientInfo::default(),
                    )
                    .await?;
                    Ok(Event::Log("云设备登记完成".into()))
                });
            }
        });
        click!(clear, u, {
            u.log.set_text("");
        });
        {
            let u = ui.clone();
            ui.window.connect_close_request(move |_| {
                u.shutdown();
                glib::Propagation::Stop
            });
        }
        ui.append_log("已准备就绪。加载已有身份后，可用蓝牙发现并连接手机，无需共同局域网。");
        ui.window.present();
        ui
    }
    fn run<F>(&self, action: &'static str, future: F)
    where
        F: Future<Output = Result<Event>> + Send + 'static,
    {
        self.append_log(&format!("{action}…"));
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let event = match future.await {
                Ok(e) => e,
                Err(e) => {
                    if notifications::is_authorization_error(&e) {
                        let _ = tx.send(Event::NotificationAuthRequired).await;
                    }
                    Event::Error(format!("{action}：{e:#}"))
                }
            };
            let _ = tx.send(event).await;
        });
    }
    fn run_session<F>(&self, action: &'static str, future: F)
    where
        F: Future<Output = Result<Event>> + Send + 'static,
    {
        let generation = self.model.borrow().generation;
        self.append_log(&format!("{action}…"));
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let event = match future.await {
                Ok(event) => event,
                Err(error) => Event::Error(format!("{action}：{error:#}")),
            };
            let (done, wait) = tokio::sync::oneshot::channel();
            if let Err(error) = tx
                .send(Event::Scoped(generation, Box::new(event), done))
                .await
            {
                dispose_event(error.0).await;
            }
            let _ = wait.await;
        });
        let mut model = self.model.borrow_mut();
        model.pending.retain(|task| !task.is_finished());
        model.pending.push(task);
    }

    fn append_display(&self, text: &str) {
        let time = glib::DateTime::now_local()
            .ok()
            .and_then(|t| t.format("%H:%M:%S").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.log
            .insert(&mut self.log.end_iter(), &format!("{time}  {text}\n"));
    }
    fn append_log(&self, text: &str) {
        logging::write(text);
        self.append_display(text);
    }
    fn error(&self, text: &str) {
        logging::error(text);
        self.append_display(text);
        self.toast.add_toast(adw::Toast::new(text));
    }
    fn local_ip(&self) -> Option<Ipv4Addr> {
        let result = self
            .interfaces
            .get(self.interface.selected() as usize)
            .map(|i| i.address);
        if result.is_none() {
            self.error("请选择一个有 IPv4 地址的网络接口");
        }
        result
    }
    fn session(&self) -> Option<Session> {
        let model = self.model.borrow();
        if model.closing || model.disconnecting {
            return None;
        }
        let value = model.session.clone();
        drop(model);
        if value.is_none() {
            self.error("请先连接手机");
        }
        value
    }
    fn connection_account(&self) -> Option<Account> {
        {
            let model = self.model.borrow();
            if model.closing || model.disconnecting {
                return None;
            }
            if model.account_pending || model.connecting || model.session.is_some() {
                drop(model);
                self.error("连接正在建立或已经连接，请先断开");
                return None;
            }
            if model.nearby.is_some() || model.nearby_stopping {
                drop(model);
                self.error("请先停止后台监听，再从电脑发起连接");
                return None;
            }
            if model.nearby_scan_pending {
                drop(model);
                self.error("蓝牙发现正在进行，请等待发现完成");
                return None;
            }
        }
        let account = self.model.borrow().account.clone();
        if account.is_none() {
            self.error("请先加载本机身份");
        }
        account
    }
    fn update_nearby_controls(&self) {
        let model = self.model.borrow();
        let listening = model.nearby.is_some();
        let idle = !model.closing
            && !model.disconnecting
            && !model.connecting
            && !model.account_pending
            && !model.nearby_scan_pending
            && !model.nearby_stopping
            && !listening
            && model.session.is_none();
        self.nearby_scan.set_sensitive(idle);
        self.nearby_start.set_sensitive(idle);
        self.nearby_stop
            .set_sensitive(listening && !model.nearby_stopping && !model.closing);
        self.wifi_interface.set_sensitive(idle);
        self.nearby_refresh.set_sensitive(idle);
        self.nearby_devices.set_sensitive(idle);
    }
    fn selected_wifi(&self) -> Option<String> {
        let interface = self
            .wifi_interface
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .map(|item| item.string().to_string());
        if interface.is_none() {
            self.error("请选择 Wi-Fi 网卡；即使不连接路由器，也需要开启 Wi-Fi");
        }
        interface
    }
    fn scan_nearby(&self) {
        let Some(account) = self.connection_account() else {
            return;
        };
        self.model.borrow_mut().nearby_scan_pending = true;
        self.update_nearby_controls();
        self.nearby_status.set_text("正在扫描蓝牙广播，约 12 秒…");
        while let Some(child) = self.nearby_devices.first_child() {
            self.nearby_devices.remove(&child);
        }
        self.append_log("开始 BLE 发现，无需共同局域网");
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let result = nearby::scan(&account).await;
            let _ = tx.send(Event::NearbyDevices(result)).await;
        });
        self.model.borrow_mut().pending.push(task);
    }
    fn connect_nearby(&self, device: BleDevice) {
        let Some(account) = self.connection_account() else {
            return;
        };
        let Some(interface) = self.selected_wifi() else {
            return;
        };
        let name = self.name.text().to_string();
        let (cancel, cancelled) = watch::channel(false);
        {
            let mut model = self.model.borrow_mut();
            model.connecting = true;
            model.nearby_connection = true;
            model.connection_cancel = Some(cancel);
        }
        self.status.set_text("● 蓝牙认证与 Wi-Fi Direct 协商中…");
        self.nearby_status
            .set_text("正在建立近场连接；若系统弹出授权，请允许创建 Wi-Fi Direct 接口");
        self.update_nearby_controls();
        self.run_session("蓝牙认证并建立 Wi-Fi Direct", async move {
            Ok(Event::Connection(
                nearby::connect(account, device, interface, name, cancelled).await,
            ))
        });
    }
    fn start_nearby(&self) {
        let Some(account) = self.connection_account() else {
            return;
        };
        let Some(interface) = self.selected_wifi() else {
            return;
        };
        let name = self.name.text().to_string();
        let mut model = self.model.borrow_mut();
        model.nearby_generation += 1;
        model.nearby = Some(nearby::start(
            account,
            interface,
            name,
            model.nearby_generation,
            self.tx.clone(),
            &self.runtime,
        ));
        drop(model);
        self.nearby_status
            .set_text("正在启动 BLE 广播与 Wi-Fi Direct 后台…");
        self.append_log("启动手机主动连接后台：BLE + Wi-Fi Direct");
        self.update_nearby_controls();
    }
    fn stop_nearby(&self) {
        let mut model = self.model.borrow_mut();
        let Some(background) = &model.nearby else {
            return;
        };
        background.stop();
        model.nearby_stopping = true;
        drop(model);
        self.nearby_status
            .set_text("正在停止后台监听并释放近场连接…");
        self.append_log("停止手机主动连接后台");
        self.update_nearby_controls();
    }
    fn connect_phone(&self) {
        let Some(account) = self.connection_account() else {
            return;
        };
        let Some(local_ip) = self.local_ip() else {
            return;
        };
        let peer: Ipv4Addr = match self.peer.text().parse() {
            Ok(ip) => ip,
            Err(_) => {
                self.error("请发现并选择手机，或填写有效的手机 IP");
                return;
            }
        };
        let port: u16 = match self.port.text().parse() {
            Ok(port) if port != 0 => port,
            _ => {
                self.error("请填写当前发现的 KCP 端口");
                return;
            }
        };
        let endpoint = DeviceEndpoint::new(local_ip, peer, port);
        let name = self.name.text().to_string();
        let (cancel, cancelled) = watch::channel(false);
        {
            let mut model = self.model.borrow_mut();
            model.connecting = true;
            model.nearby_connection = false;
            model.connection_cancel = Some(cancel);
        }
        self.update_nearby_controls();
        self.status.set_text("● 正在认证连接…");
        self.run_session("连接手机", async move {
            let options = ConnectOptions {
                device_name: name,
                ..Default::default()
            };
            Ok(Event::Connection(
                Session::connect_cancellable(&account, &endpoint, options, cancelled).await,
            ))
        });
    }
    fn disconnect(self: &Rc<Self>) {
        self.end_session(false);
    }
    fn open_screen(&self, app: bool) {
        let Some(session) = self.session() else {
            return;
        };
        let package = self.package.text().to_string();
        let options = AppOptions {
            width: self.width.value_as_int() as u32,
            height: self.height.value_as_int() as u32,
            density: self.density.value_as_int() as u32,
            ..Default::default()
        };
        let audio = self.audio.is_active();
        let audio_device = self.audio_device.text().trim().to_owned();
        self.run_session("打开手机画面", async move {
            let screen = if app {
                session.open_app(&package, options).await?
            } else {
                session.open_desktop().await?
            };
            let cfg = screen.configuration();
            match MediaPlayer::play_screen(
                session.local_ip(),
                cfg.phone_ip,
                cfg.port,
                PlaybackOptions {
                    audio,
                    audio_device,
                    ..Default::default()
                },
            )
            .await
            {
                Ok(mut player) => {
                    // Test-phone raw music range; normalized protocol values do not need it.
                    player.follow_screen_audio(&screen, Some(Default::default()))?;
                    Ok(Event::Screen(screen, player))
                }
                Err(e) => {
                    let _ = screen.close().await;
                    Err(e)
                }
            }
        });
    }
    fn open_camera(&self) {
        let Some(session) = self.session() else {
            return;
        };
        let options = CameraOptions {
            camera_id: self.lens.selected(),
            width: self.camera_width.value_as_int() as u32,
            height: self.camera_height.value_as_int() as u32,
            ..Default::default()
        };
        let device = if self.v4l2_enabled.is_active() {
            Some(PathBuf::from(self.v4l2.text().as_str()))
        } else {
            None
        };
        self.run_session("打开手机相机", async move {
            let preview = Arc::new(session.open_camera(options).await?);
            match MediaPlayer::play_camera(
                session.local_ip(),
                preview.feed(),
                PlaybackOptions {
                    audio: false,
                    v4l2_device: device,
                    ..Default::default()
                },
            )
            .await
            {
                Ok(player) => Ok(Event::Camera(preview, player)),
                Err(e) => {
                    let _ = preview.close().await;
                    Err(e)
                }
            }
        });
    }
    fn load_apps(&self) {
        let Some(session) = self.session() else {
            return;
        };
        self.run_session("加载应用列表", async move {
            Ok(Event::Apps(session.recent_apps().await))
        });
    }

    fn toggle_clipboard(&self, enabled: bool) {
        if enabled {
            if self.model.borrow().clipboard_pending || self.model.borrow().clipboard.is_some() {
                return;
            }
            let Some(session) = self.session() else {
                return;
            };
            self.model.borrow_mut().clipboard_pending = true;
            let tx = self.tx.clone();
            self.run_session("开启剪贴板", async move {
                let operation = async {
                    let cache = hyperclix_core::paths::clipboard_cache_dir();
                    let clip = Clipboard::open_with_cache(&session, &cache).await?;
                    match SelectionMonitor::start(clip.clone(), tx, cache).await {
                        Ok(monitor) => Ok(Event::Clipboard(clip, monitor)),
                        Err(error) => {
                            let _ = clip.close().await;
                            Err(error)
                        }
                    }
                }
                .await;
                Ok(match operation {
                    Ok(event) => event,
                    Err(error) => Event::ClipboardFailed(format!("开启剪贴板：{error:#}")),
                })
            });
        } else {
            let mut model = self.model.borrow_mut();
            let clip = model.clipboard.take();
            let monitor = model.selection.take();
            drop(model);
            if clip.is_some() || monitor.is_some() {
                self.run_session("关闭剪贴板", async move {
                    if let Some(monitor) = monitor {
                        let _ = monitor.stop().await;
                    }
                    if let Some(clip) = clip {
                        clip.close().await?;
                    }
                    Ok(Event::Log("剪贴板同步已停止".into()))
                });
            }
        }
    }
    fn set_files(&self, paths: Vec<PathBuf>) {
        let text = paths
            .iter()
            .filter_map(|p| p.file_name())
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        self.files_label
            .set_text(&format!("已选择 {} 个文件\n{text}", paths.len()));
        *self.file_paths.borrow_mut() = paths;
    }
    fn start_transfer(&self) {
        let Some(session) = self.session() else {
            return;
        };
        let name = self.name.text().to_string();
        let paths = self.file_paths.borrow().clone();
        self.run_session("文件互传", async move {
            let share = MiShare::new(session, name);
            Ok(Event::Transfer(share.send_files(paths)))
        });
    }
    fn receive_phone_files(&self, session: Session) {
        let generation = self.model.borrow().generation;
        let destination = PathBuf::from(self.destination.text().as_str());
        let name = self.name.text().to_string();
        let tx = self.tx.clone();
        let (stop, mut stopped) = watch::channel(false);
        self.model.borrow_mut().receiver_stop = Some(stop);
        self.append_log("手机文件接收已自动启动");
        let task = self.runtime.spawn(async move {
            let mut events = session.events();
            let share = MiShare::new(session, name);
            'listening: loop {
                if *stopped.borrow() {
                    break;
                }
                let transfer = share.receive_files(destination.clone());
                if tx
                    .send(Event::Notice(
                        generation,
                        Box::new(Event::Transfer(transfer.clone())),
                    ))
                    .await
                    .is_err()
                {
                    transfer.cancel();
                    let _ = transfer.wait().await;
                    break;
                }
                loop {
                    tokio::select! {
                        _ = stopped.changed() => {
                            transfer.cancel();
                            let _ = transfer.wait().await;
                            break 'listening;
                        },
                        event = events.recv() => match event {
                            Ok(hyperclix_core::SessionEvent::Disconnected(_))
                            | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                transfer.cancel();
                                let _ = transfer.wait().await;
                                break 'listening;
                            },
                            _ => {},
                        },
                        _ = transfer.wait() => break,
                    }
                }
            }
            logging::write("手机文件接收已停止");
        });
        self.model.borrow_mut().pending.push(task);
    }
    fn start_login(&self, directory: String, sid: &'static str) {
        if self.model.borrow().closing || self.model.borrow().account_pending {
            return;
        }
        if sid == "miconnect" && {
            let model = self.model.borrow();
            model.session.is_some()
                || model.connecting
                || model.nearby.is_some()
                || model.nearby_scan_pending
                || model.nearby_stopping
        } {
            self.error("请先断开手机并停止近场扫描或后台，再更换账号授权");
            return;
        }
        self.cancel_login(None);
        let generation = {
            let mut model = self.model.borrow_mut();
            model.login_generation += 1;
            if sid == "miconnect" {
                model.account = None;
            }
            model.login_generation
        };
        let tx = self.tx.clone();
        self.append_log("正在初始化扫码登录…");
        let task = self.runtime.spawn(async move {
            let result = async {
                let login = QrLogin::begin(&directory, sid).await?;
                tx.send(Event::Qr(
                    generation,
                    sid,
                    login.image_path.clone(),
                    login.expires_in.as_secs(),
                ))
                .await?;
                login.wait().await?;
                Ok(if sid == "miconnect" {
                    "互联账号授权完成，点击“签发本机证书”继续；通知云授权在“手机通知”页单独完成"
                } else {
                    "通知云授权已更新，可以重新启动通知接收"
                }
                .to_string())
            }
            .await;
            let _ = tx.send(Event::LoginFinished(generation, sid, result)).await;
        });
        self.model.borrow_mut().login_task = Some(task.abort_handle());
    }
    fn cancel_login(&self, generation: Option<u64>) {
        let mut model = self.model.borrow_mut();
        if generation.is_some_and(|id| id != model.login_generation) {
            return;
        }
        model.login_generation += 1;
        if let Some(task) = model.login_task.take() {
            task.abort();
        }
        let window = model.login_window.take();
        drop(model);
        if let Some(window) = window {
            window.destroy();
        }
    }
    fn load_account(&self, register: bool) {
        {
            let model = self.model.borrow();
            if model.closing || model.account_pending {
                return;
            }
            if model.session.is_some()
                || model.connecting
                || model.nearby.is_some()
                || model.nearby_scan_pending
                || model.nearby_stopping
            {
                drop(model);
                self.error("请先断开手机并停止近场扫描或后台，再加载或签发其他身份");
                return;
            }
        }
        self.model.borrow_mut().account_pending = true;
        self.update_nearby_controls();
        let path = self.account_path.text().to_string();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = if register {
                Account::register(path).await
            } else {
                Account::load(path).await
            };
            let _ = tx.send(Event::Account(result)).await;
        });
    }
    fn start_announcement(&self) {
        {
            let model = self.model.borrow();
            if model.closing || model.disconnecting {
                return;
            }
            if model.announcement.is_some() || model.announcement_task.is_some() {
                drop(model);
                self.error("电脑公告已经发布或正在启动");
                return;
            }
        }
        let Some(account) = self.model.borrow().account.clone() else {
            self.error("请先加载本机身份");
            return;
        };
        let Some(local_ip) = self.local_ip() else {
            return;
        };
        let generation = {
            let mut model = self.model.borrow_mut();
            model.announcement_generation += 1;
            model.announcement_generation
        };
        let options = AnnouncementOptions {
            local_ip,
            name: self.name.text().to_string(),
            ..Default::default()
        };
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let result = Announcement::start(&account, options).await;
            let (done, wait) = tokio::sync::oneshot::channel();
            let event = Event::Announcement(generation, result, done);
            if let Err(error) = tx.send(event).await {
                dispose_event(error.0).await;
            }
            let _ = wait.await;
        });
        self.model.borrow_mut().announcement_task = Some(task);
    }
    fn stop_announcement(&self) {
        let mut model = self.model.borrow_mut();
        model.announcement_generation += 1;
        let current = model.announcement.take();
        drop(model);
        if let Some(current) = current {
            self.run("停止公告", async move {
                current.close().await?;
                Ok(Event::AnnouncementStopped)
            });
        }
    }
    fn start_notifications(&self) {
        let Some(session) = self.session() else {
            return;
        };
        if self.model.borrow().notification_stop.is_some() {
            self.error("通知正在配置或接收中；停止完成后可重新启动");
            return;
        }
        let packages: Vec<String> = self
            .packages
            .text()
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect();
        let cloud = self.cloud_path.text().to_string();
        let runtime = self.push_path.text().to_string();
        let key_file = session
            .account()
            .directory
            .join("notification-key.private.json");
        let (stop, mut stop_rx) = watch::channel(false);
        self.model.borrow_mut().notification_stop = Some(stop);
        let tx = self.tx.clone();
        let generation = self.model.borrow().generation;
        self.notification_status
            .set_text("正在交换密钥并建立云订阅…");
        self.run_session("启动通知接收", async move {
            let operation = async {
                let setup = async {
                    let credentials = CloudCredentials::load(cloud).await?;
                    credentials.require_account(session.account())?;
                    let runtime = PushRuntime::load(runtime).await?;
                    let config = notifications::configure(&session, &packages).await?;
                    config.save_key(&key_file).await?;
                    notifications::subscribe_cloud(&session, &credentials, &packages).await?;
                    NotificationReceiver::connect(runtime, config.key_info).await
                };
                let mut receiver = tokio::select! {
                    result = setup => result?,
                    _ = stop_rx.changed() => return Ok::<_, anyhow::Error>(()),
                };
                tx.send(Event::Notice(generation, Box::new(Event::NotificationReady))).await?;
                loop {
                    tokio::select! {
                        changed = stop_rx.changed() => { if changed.is_err() || *stop_rx.borrow() { break; } },
                        notification = receiver.next() => { tx.send(Event::Notice(generation, Box::new(Event::Notification(notification?)))).await?; },
                    }
                }
                receiver.close().await?;
                Ok::<_, anyhow::Error>(())
            };
            let result = operation.await;
            tx.send(Event::Notice(generation, Box::new(Event::NotificationStopped))).await?;
            if let Err(error) = &result {
                if notifications::is_authorization_error(error) {
                    tx.send(Event::Notice(generation, Box::new(Event::NotificationAuthRequired))).await?;
                }
            }
            result?;
            Ok(Event::Log("通知接收已停止".into()))
        });
    }
    fn handle(self: &Rc<Self>, event: Event) {
        match event {
            Event::Scoped(generation, event, done) => {
                let stale = {
                    let model = self.model.borrow();
                    model.closing || model.disconnecting || generation != model.generation
                };
                if stale {
                    self.runtime.spawn(async move {
                        dispose_event(*event).await;
                        let _ = done.send(());
                    });
                } else {
                    self.handle(*event);
                    let _ = done.send(());
                }
            }
            Event::Notice(generation, event) => {
                if generation == self.model.borrow().generation && !self.model.borrow().closing {
                    self.handle(*event);
                }
            }
            Event::Log(text) => self.append_log(&text),
            Event::Error(error) => {
                self.error(&error);
                if self.model.borrow().session.is_none() {
                    self.status.set_text("● 未连接");
                }
            }
            Event::Account(result) => {
                self.model.borrow_mut().account_pending = false;
                if self.model.borrow().closing {
                    return;
                }
                match result {
                    Ok(account) => {
                        self.account_status
                            .set_text("本机证书已加载并校验 · OTHER_PC21");
                        self.model.borrow_mut().account = Some(account);
                        self.append_log("本机身份已准备好");
                    }
                    Err(error) => self.error(&format!("本机身份：{error:#}")),
                }
                self.update_nearby_controls();
            }
            Event::Devices(devices) => {
                while let Some(child) = self.devices.first_child() {
                    self.devices.remove(&child);
                }
                self.status
                    .set_text(&format!("发现 {} 台手机", devices.len()));
                for device in devices {
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                    row.set_margin_start(14);
                    row.set_margin_end(14);
                    row.set_margin_top(12);
                    row.set_margin_bottom(12);
                    row.append(&gtk::Image::from_icon_name("phone-symbolic"));
                    let label = gtk::Label::new(Some(&format!(
                        "{}\n{} · KCP {}",
                        device.name, device.ip, device.port
                    )));
                    label.set_xalign(0.0);
                    label.set_hexpand(true);
                    row.append(&label);
                    let choose = button("选择", false);
                    let u = self.clone();
                    choose.connect_clicked(move |_| {
                        u.peer.set_text(&device.ip.to_string());
                        u.port.set_text(&device.port.to_string());
                    });
                    row.append(&choose);
                    self.devices.append(&row);
                }
                self.append_log("局域网发现完成");
            }
            Event::NearbyDevices(result) => {
                self.model.borrow_mut().nearby_scan_pending = false;
                if self.model.borrow().closing {
                    return;
                }
                self.update_nearby_controls();
                let devices = match result {
                    Ok(devices) => devices,
                    Err(error) => {
                        self.nearby_status.set_text("BLE 发现失败，详见运行日志");
                        self.error(&format!("BLE 发现：{error:#}"));
                        return;
                    }
                };
                let same_account = devices
                    .iter()
                    .filter(|device| device.advertisement.same_account == Some(true))
                    .count();
                self.nearby_status.set_text(&format!(
                    "BLE 发现 {} 台设备，其中 {same_account} 台同账号；点击连接后自动建立 Wi-Fi Direct",
                    devices.len()
                ));
                self.append_log(&format!(
                    "BLE 发现完成：{} 台设备，{same_account} 台同账号",
                    devices.len()
                ));
                for device in devices {
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                    row.set_margin_start(14);
                    row.set_margin_end(14);
                    row.set_margin_top(12);
                    row.set_margin_bottom(12);
                    row.append(&gtk::Image::from_icon_name("bluetooth-symbolic"));
                    let account = match device.advertisement.same_account {
                        Some(true) => "同账号",
                        Some(false) => "其他账号",
                        None => "账号未识别",
                    };
                    let signal = device
                        .rssi
                        .map(|rssi| format!(" · {rssi} dBm"))
                        .unwrap_or_default();
                    let label =
                        gtk::Label::new(Some(&format!("{}\n{account}{signal}", device.name)));
                    label.set_xalign(0.0);
                    label.set_hexpand(true);
                    label.set_wrap(true);
                    row.append(&label);
                    let connect = button("蓝牙连接", true);
                    connect.set_sensitive(device.advertisement.same_account == Some(true));
                    let u = self.clone();
                    connect.connect_clicked(move |_| u.connect_nearby(device.clone()));
                    row.append(&connect);
                    self.nearby_devices.append(&row);
                }
            }
            Event::NearbyStatus(generation, text) => {
                let model = self.model.borrow();
                if generation != model.nearby_generation || model.closing || model.nearby_stopping {
                    return;
                }
                drop(model);
                self.nearby_status.set_text(&text);
                self.append_log(&text);
            }
            Event::Incoming(generation, session) => {
                let model = self.model.borrow();
                let accept = generation == model.nearby_generation
                    && model.nearby.is_some()
                    && !model.nearby_stopping
                    && !model.closing
                    && !model.disconnecting
                    && !model.connecting
                    && model.session.is_none();
                drop(model);
                if accept {
                    self.model.borrow_mut().nearby_connection = true;
                    self.handle(Event::Connection(Ok(session)));
                } else {
                    self.append_log("当前界面已有会话或正在清理，关闭本次入站连接");
                    self.runtime.spawn(async move {
                        let _ = session.close().await;
                    });
                }
            }
            Event::NearbyStopped(generation, result) => {
                if generation != self.model.borrow().nearby_generation {
                    return;
                }
                let mut model = self.model.borrow_mut();
                let background = model.nearby.take();
                model.nearby_stopping = false;
                drop(model);
                if let Some(background) = background {
                    self.runtime.spawn(async move {
                        if let Err(error) = background.close().await {
                            logging::write(&format!("结束近场后台任务：{error:#}"));
                        }
                    });
                }
                if self.model.borrow().closing {
                    return;
                }
                match result {
                    Ok(()) => {
                        self.nearby_status.set_text("近场后台已停止");
                        self.append_log("近场后台已停止，BLE 公告与 Wi-Fi Direct 资源已释放");
                    }
                    Err(error) => {
                        self.nearby_status
                            .set_text("近场后台启动或运行失败，详见运行日志");
                        self.error(&format!("近场后台：{error:#}"));
                    }
                }
                self.update_nearby_controls();
            }
            Event::Connection(result) => {
                self.model.borrow_mut().connecting = false;
                self.model.borrow_mut().connection_cancel = None;
                let session = match result {
                    Ok(session) => session,
                    Err(error) => {
                        self.status.set_text("● 未连接");
                        if self.model.borrow().nearby_connection {
                            self.nearby_status.set_text("近场连接失败，详见运行日志");
                        }
                        self.model.borrow_mut().nearby_connection = false;
                        self.update_nearby_controls();
                        self.error(&format!("连接手机：{error:#}"));
                        return;
                    }
                };
                self.clipboard_switch.set_sensitive(true);
                if self.model.borrow().nearby_connection {
                    self.status.set_text("● 已连接 · Wi-Fi Direct");
                    self.nearby_status.set_text(&format!(
                        "Wi-Fi Direct 已就绪 · 本机 {} → 手机 {} · 可使用投屏、文件和剪贴板页面",
                        session.local_ip(),
                        session.peer_ip()
                    ));
                    self.append_log("BLE 同账号认证、Wi-Fi Direct 与 SDK 通道已完成");
                } else {
                    self.status
                        .set_text(&format!("● 已连接 {}", session.peer_ip()));
                }
                let generation = self.model.borrow().generation;
                let mut events = session.events();
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    while let Ok(event) = events.recv().await {
                        match event {
                            hyperclix_core::SessionEvent::Phase(text) => {
                                let _ = tx
                                    .send(Event::Notice(generation, Box::new(Event::Log(text))))
                                    .await;
                            }
                            hyperclix_core::SessionEvent::Disconnected(reason) => {
                                let _ = tx
                                    .send(Event::Notice(
                                        generation,
                                        Box::new(Event::Log(format!("连接断开：{reason}"))),
                                    ))
                                    .await;
                                let _ = tx
                                    .send(Event::Notice(generation, Box::new(Event::Disconnected)))
                                    .await;
                                break;
                            }
                            _ => {}
                        }
                    }
                });
                self.model.borrow_mut().session = Some(session.clone());
                self.destination.set_sensitive(false);
                self.receive_phone_files(session);
                self.update_nearby_controls();
                self.append_log("账号认证与 Networking 上线完成");
            }
            Event::Disconnected => {
                if self.model.borrow().session.is_some() && !self.model.borrow().disconnecting {
                    self.disconnect();
                    return;
                }
                self.model.borrow_mut().disconnecting = false;
                self.model.borrow_mut().connecting = false;
                self.model.borrow_mut().clipboard = None;
                self.model.borrow_mut().session = None;
                let was_nearby = self.model.borrow().nearby_connection;
                self.model.borrow_mut().nearby_connection = false;
                self.clipboard_switch.set_active(false);
                self.clipboard_switch.set_sensitive(false);
                self.destination.set_sensitive(true);
                self.status.set_text("● 未连接");
                self.append_log("连接已结束");
                if self.model.borrow().nearby.is_some() && !self.model.borrow().nearby_stopping {
                    self.nearby_status
                        .set_text("BLE 后台在线，等待手机主动连接");
                } else if was_nearby && !self.model.borrow().nearby_stopping {
                    self.nearby_status
                        .set_text("近场连接已断开，可重新扫描或连接");
                }
                self.update_nearby_controls();
                if self.model.borrow().quit_after_disconnect {
                    self.model.borrow_mut().quit_after_disconnect = false;
                    self.shutdown();
                }
            }
            Event::Qr(generation, sid, path, seconds) => {
                if generation != self.model.borrow().login_generation || self.model.borrow().closing
                {
                    return;
                }
                let dialog = gtk::Window::builder()
                    .transient_for(&self.window)
                    .modal(true)
                    .title(if sid == "micloud" {
                        "扫码授权通知"
                    } else {
                        "互联账号授权"
                    })
                    .default_width(430)
                    .default_height(500)
                    .build();
                let box_ = card();
                let image = gtk::Picture::for_filename(path);
                image.set_content_fit(gtk::ContentFit::Contain);
                image.set_size_request(360, 360);
                box_.append(&image);
                let label = gtk::Label::new(Some(&format!(
                    "手机设置 → 小米账号 → 扫一扫\n二维码有效期约 {seconds} 秒"
                )));
                label.set_justify(gtk::Justification::Center);
                box_.append(&label);
                let close = button("取消登录", false);
                let d = dialog.clone();
                close.connect_clicked(move |_| d.close());
                box_.append(&close);
                dialog.set_child(Some(&box_));
                let tx = self.tx.clone();
                dialog.connect_close_request(move |_| {
                    let _ = tx.try_send(Event::CancelLogin(generation));
                    glib::Propagation::Proceed
                });
                self.model.borrow_mut().login_window = Some(dialog.clone());
                dialog.present();
            }
            Event::CancelLogin(generation) => self.cancel_login(Some(generation)),
            Event::LoginFinished(generation, sid, result) => {
                if generation != self.model.borrow().login_generation {
                    return;
                }
                self.model.borrow_mut().login_task = None;
                let window = self.model.borrow_mut().login_window.take();
                if let Some(window) = window {
                    window.destroy();
                }
                match result {
                    Ok(message) => {
                        if sid == "micloud" {
                            self.notification_status
                                .set_text("通知云授权已更新，请重新启动通知接收");
                        }
                        self.append_log(&message);
                    }
                    Err(error) => self.error(&format!("扫码登录：{error:#}")),
                }
            }
            Event::Screen(screen, player) => {
                let config = screen.configuration();
                self.append_log(&format!(
                    "画面已创建：{} · screen={} session={} RTSP={}:{} · {}×{}",
                    if config.app_name.is_empty() && config.screen_id == 0 {
                        "手机桌面"
                    } else if config.app_name.is_empty() {
                        "独立 App"
                    } else {
                        &config.app_name
                    },
                    config.screen_id,
                    config.session_id,
                    config.phone_ip,
                    config.port,
                    config.width,
                    config.height
                ));
                let window = viewer::open_screen(
                    &self.app,
                    &self.window,
                    self.runtime.clone(),
                    self.tx.clone(),
                    screen,
                    player,
                );
                self.model.borrow_mut().windows.push(window);
            }
            Event::Camera(camera, player) => {
                let window = viewer::open_camera(
                    &self.app,
                    &self.window,
                    self.runtime.clone(),
                    self.tx.clone(),
                    camera,
                    player,
                );
                self.model.borrow_mut().windows.push(window);
            }
            Event::Apps(result) => match result {
                Ok(apps) => {
                    let names: Vec<String> = apps
                        .iter()
                        .map(|app| {
                            if app.name.is_empty() {
                                app.package.clone()
                            } else {
                                format!("{} ({})", app.name, app.package)
                            }
                        })
                        .collect();
                    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                    self.app_names.splice(0, self.app_names.n_items(), &refs);
                    self.app_cache.replace(apps);
                    self.append_log(&format!("已加载 {} 个最近应用", names.len()));
                }
                Err(error) => self.error(&format!("加载应用列表：{error:#}")),
            },
            Event::Clipboard(clip, monitor) => {
                self.model.borrow_mut().clipboard_pending = false;
                if let SelectionStatus::Stopped(reason) = monitor.status() {
                    self.runtime.spawn(async move {
                        let _ = monitor.stop().await;
                        let _ = clip.close().await;
                    });
                    self.clipboard_switch.set_active(false);
                    if let Some(error) = reason {
                        self.error(&error);
                    }
                    return;
                }
                if !self.clipboard_switch.is_active() {
                    self.runtime.spawn(async move {
                        let _ = monitor.stop().await;
                        let _ = clip.close().await;
                    });
                    return;
                }
                let mut model = self.model.borrow_mut();
                model.clipboard = Some(clip);
                model.selection = Some(monitor);
                drop(model);
                self.append_log("跨设备剪贴板已开启 · universalClipboard · Wayland 多格式");
            }
            Event::SelectionText(id, text) => {
                if self
                    .model
                    .borrow()
                    .selection
                    .as_ref()
                    .is_some_and(|monitor| monitor.id() == id)
                {
                    self.model.borrow_mut().last_remote = text.clone();
                    self.clipboard_text.set_text(&text);
                }
            }
            Event::ClipboardFailed(error) => {
                self.model.borrow_mut().clipboard_pending = false;
                self.clipboard_switch.set_active(false);
                self.error(&error);
            }
            Event::SelectionStopped(id, reason) => {
                if !self
                    .model
                    .borrow()
                    .selection
                    .as_ref()
                    .is_some_and(|monitor| monitor.id() == id)
                {
                    return;
                }
                self.clipboard_switch.set_active(false);
                self.toggle_clipboard(false);
                if let Some(error) = reason {
                    self.error(&error);
                } else {
                    self.append_log("剪贴板同步已停止");
                }
            }
            Event::Transfer(task) => {
                let mut updates = task.subscribe();
                let generation = self.model.borrow().generation;
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    while let Ok(update) = updates.recv().await {
                        let finished = matches!(
                            update,
                            TransferEvent::Complete(_)
                                | TransferEvent::Failed(_)
                                | TransferEvent::Cancelled
                        );
                        let _ = tx
                            .send(Event::Notice(
                                generation,
                                Box::new(Event::TransferUpdate(update)),
                            ))
                            .await;
                        if finished {
                            break;
                        }
                    }
                });
                self.handle(Event::TransferUpdate(task.state()));
                let mut model = self.model.borrow_mut();
                model.transfers.retain(|task| {
                    !matches!(
                        task.state(),
                        TransferEvent::Complete(_)
                            | TransferEvent::Failed(_)
                            | TransferEvent::Cancelled
                    )
                });
                model.transfers.push(task);
            }
            Event::TransferUpdate(update) => match update {
                TransferEvent::Preparing => {
                    self.progress.set_fraction(0.0);
                    self.progress.set_text(Some("正在准备文件…"));
                }
                TransferEvent::WaitingForPhone => self.progress.set_text(Some("等待手机接受…")),
                TransferEvent::WaitingForIncoming => {
                    self.progress.set_text(Some("已就绪，等待手机发送文件…"))
                }
                TransferEvent::Offer(files) => self
                    .transfer_label
                    .set_text(&format!("接收 {} 个文件", files.len())),
                TransferEvent::Progress {
                    filename,
                    transferred,
                    total,
                    file_index,
                    file_count,
                } => {
                    self.progress.set_fraction(if total == 0 {
                        0.0
                    } else {
                        transferred as f64 / total as f64
                    });
                    self.progress.set_text(Some(&format!(
                        "{}/{} · {} / {}",
                        file_index,
                        file_count,
                        format_bytes(transferred),
                        format_bytes(total)
                    )));
                    self.transfer_label.set_text(&filename);
                }
                TransferEvent::FileComplete(file) => self
                    .transfer_label
                    .set_text(&format!("已完成 {}", file.info.name)),
                TransferEvent::Complete(summary) => {
                    self.progress.set_fraction(1.0);
                    self.progress.set_text(Some("传输完成"));
                    self.append_log(&format!(
                        "文件任务完成：{} 个文件，{}",
                        summary.files.len(),
                        format_bytes(summary.bytes)
                    ));
                }
                TransferEvent::Cancelled => self.progress.set_text(Some("已取消")),
                TransferEvent::Failed(error) => {
                    self.progress.set_text(Some("传输失败"));
                    self.error(&error);
                }
            },
            Event::Notification(notification) => {
                let r = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&notification.title))
                    .subtitle(glib::markup_escape_text(&notification.content))
                    .build();
                r.set_subtitle_lines(3);
                self.notification_list.prepend(&r);
                let tx = self.tx.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) =
                        system_notifications::show(&notification.title, &notification.content).await
                    {
                        let _ = tx
                            .send(Event::Error(format!("本机通知展示：{error:#}")))
                            .await;
                    }
                });
                self.notification_status.set_text("正在接收手机通知");
            }
            Event::NotificationStopped => {
                self.model.borrow_mut().notification_stop = None;
                self.notification_status.set_text("通知接收已停止");
            }
            Event::NotificationReady => {
                self.notification_status
                    .set_text("订阅成功，正在等待手机通知");
                self.append_log("通知订阅与接收连接已建立");
            }
            Event::NotificationAuthRequired => {
                self.notification_status
                    .set_text("云服务拒绝当前通知授权（401），请点击“扫码授权通知”后重试");
            }
            Event::Announcement(generation, result, done) => {
                self.model.borrow_mut().announcement_task = None;
                if self.model.borrow().closing
                    || generation != self.model.borrow().announcement_generation
                {
                    self.runtime.spawn(async move {
                        if let Ok(a) = result {
                            let _ = a.close().await;
                        }
                        let _ = done.send(());
                    });
                } else {
                    self.model.borrow_mut().announcement_task = None;
                    match result {
                        Ok(a) => {
                            self.model.borrow_mut().announcement = Some(a);
                            self.append_log("电脑公告已发布 · Windows");
                        }
                        Err(error) => self.error(&format!("发布公告：{error:#}")),
                    }
                    let _ = done.send(());
                }
            }
            Event::AnnouncementStopped => self.append_log("电脑公告已停止"),
            Event::ShutdownRequest => self.shutdown(),
            Event::Shutdown => {
                self.window.destroy();
                self.app.quit();
            }
        }
    }
    fn shutdown(self: &Rc<Self>) {
        self.end_session(true);
    }
    fn end_session(self: &Rc<Self>, quit: bool) {
        {
            let mut model = self.model.borrow_mut();
            if model.closing {
                return;
            }
            if model.disconnecting {
                if quit {
                    model.quit_after_disconnect = true;
                }
                return;
            }
        }
        if quit {
            self.cancel_login(None);
        }
        let mut model = self.model.borrow_mut();
        if quit {
            model.closing = true;
        }
        model.disconnecting = true;
        model.generation += 1;
        model.connecting = false;
        model.clipboard_pending = false;
        if let Some(cancel) = model.connection_cancel.take() {
            cancel.send_replace(true);
        }
        if let Some(stop) = model.notification_stop.take() {
            stop.send_replace(true);
        }
        if let Some(stop) = model.receiver_stop.take() {
            stop.send_replace(true);
        }
        for task in &model.transfers {
            task.cancel();
        }
        model.transfers.clear();
        let windows = std::mem::take(&mut model.windows);
        let pending = std::mem::take(&mut model.pending);
        for task in &pending {
            task.abort();
        }
        let clip = model.clipboard.take();
        let monitor = model.selection.take();
        let session = model.session.take();
        let nearby = if quit {
            model.nearby_generation += 1;
            let background = model.nearby.take();
            if let Some(background) = &background {
                background.stop();
            }
            background
        } else {
            None
        };
        let (announcement, announcement_task) = if quit {
            model.announcement_generation += 1;
            (model.announcement.take(), model.announcement_task.take())
        } else {
            (None, None)
        };
        drop(model);
        self.update_nearby_controls();
        self.clipboard_switch.set_active(false);
        self.clipboard_switch.set_sensitive(false);
        self.status.set_text("正在结束业务并关闭会话…");
        let ui = self.clone();
        glib::spawn_future_local(async move {
            for window in &windows {
                window.close();
            }
            // Window close handlers may themselves wait for a CAST lease. Do
            // not let that block session teardown indefinitely.
            for _ in 0..100 {
                if !windows.iter().any(|window| window.is_visible()) {
                    break;
                }
                glib::timeout_future(Duration::from_millis(20)).await;
            }
            let tx = ui.tx.clone();
            ui.runtime.spawn(async move {
                for task in pending {
                    let _ = task.await;
                }
                if let Some(monitor) = monitor {
                    let _ = monitor.stop().await;
                }
                if let Some(clip) = clip {
                    let _ = clip.close().await;
                }
                if let Some(session) = session {
                    if let Err(error) = session.close().await {
                        let _ = tx.send(Event::Error(format!("关闭连接：{error:#}"))).await;
                    }
                }
                if let Some(a) = announcement {
                    let _ = a.close().await;
                }
                if let Some(task) = announcement_task {
                    let _ = task.await;
                }
                if let Some(background) = nearby {
                    if let Err(error) = background.close().await {
                        logging::write(&format!("停止近场后台：{error:#}"));
                    }
                }
                let _ = tx
                    .send(if quit {
                        Event::Shutdown
                    } else {
                        Event::Disconnected
                    })
                    .await;
            });
        });
    }
}
async fn dispose_event(event: Event) {
    match event {
        Event::Scoped(_, event, done) => {
            Box::pin(dispose_event(*event)).await;
            let _ = done.send(());
        }
        Event::Connection(Ok(session)) | Event::Incoming(_, session) => {
            let _ = session.close().await;
        }
        Event::Screen(screen, player) => {
            let _ = player.close().await;
            let _ = screen.close().await;
        }
        Event::Camera(camera, player) => {
            let _ = player.close().await;
            let _ = camera.close().await;
        }
        Event::Clipboard(clip, monitor) => {
            let _ = monitor.stop().await;
            let _ = clip.close().await;
        }
        Event::Transfer(task) => {
            task.cancel();
            let _ = task.wait().await;
        }
        Event::Announcement(_, result, done) => {
            if let Ok(a) = result {
                let _ = a.close().await;
            }
            let _ = done.send(());
        }
        _ => {}
    }
}

fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / 1048576.0)
    } else if n >= 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn main() -> glib::ExitCode {
    if let Err(error) = logging::start(hyperclix_core::paths::logs_dir()) {
        eprintln!("创建磁盘日志失败：{error:#}");
    }
    let runtime = Arc::new(Runtime::new().expect("create network runtime"));
    let app = adw::Application::builder()
        .application_id("org.hyperclix.HyperClix")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let hold: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));
    let args: Vec<String> = std::env::args().collect();
    let screenshot = args
        .windows(2)
        .find(|a| a[0] == "--screenshot")
        .map(|a| PathBuf::from(&a[1]));
    let selected_page = args
        .windows(2)
        .find(|a| a[0] == "--page")
        .map(|a| a[1].clone());
    let preview_media = args
        .windows(2)
        .find(|a| a[0] == "--preview-media")
        .map(|a| PathBuf::from(&a[1]));
    let quit = args.iter().any(|a| a == "--quit-after-screenshot");
    let held = hold.clone();
    app.connect_activate(move |app| {
        if let Some(ui) = held.borrow().as_ref() { ui.window.present(); return; }
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let css=gtk::CssProvider::new();
        css.load_from_string(".hero { background: linear-gradient(125deg, #163d65, #223447); color: #f2f8ff; padding: 24px; } .card { padding: 18px; border-radius: 14px; } .navigation-sidebar { background: alpha(@window_fg_color, 0.025); } entry { min-height: 32px; } .title-1 { letter-spacing: -0.5px; }");
        if let Some(display)=gdk::Display::default() { gtk::style_context_add_provider_for_display(&display,&css,gtk::STYLE_PROVIDER_PRIORITY_APPLICATION); }
        let ui=Ui::build(app,runtime.clone());
        let capture_window: gtk::Window = if let Some(path)=preview_media.clone() {
            let enter=runtime.enter();
            let player=MediaPlayer::play_file(path,PlaybackOptions { audio:false,..Default::default() });
            drop(enter);
            let window=viewer::open_file(app,&ui.window,runtime.clone(),ui.tx.clone(),player);
            ui.model.borrow_mut().windows.push(window.clone());
            window
        } else { ui.window.clone().upcast() };
        if let Some(page)=&selected_page { ui.stack.set_visible_child_name(page); }
        let tx=ui.tx.clone();
        runtime.spawn(async move { if tokio::signal::ctrl_c().await.is_ok() { let _=tx.send(Event::ShutdownRequest).await; } });
        if let Some(path)=screenshot.clone() {
            let ui=ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(800),move || {
                if let Err(e)=save_screenshot(&capture_window,&path) { logging::write(&format!("screenshot: {e:#}")); }
                if quit { ui.shutdown(); }
            });
        }
        *held.borrow_mut()=Some(ui);
    });
    app.run_with_args::<&str>(&[])
}
fn save_screenshot(window: &gtk::Window, path: &std::path::Path) -> Result<()> {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
    let node = snapshot
        .to_node()
        .ok_or_else(|| anyhow::anyhow!("window has no render node"))?;
    let renderer = window
        .renderer()
        .ok_or_else(|| anyhow::anyhow!("window has no renderer"))?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path)?;
    Ok(())
}

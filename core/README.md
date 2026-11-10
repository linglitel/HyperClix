# HyperClix core

Linux Rust library：`hyperclix-core`，导入名 `hyperclix_core`，媒体直接调用 libav。

构建需要 Rust 1.92+、C 编译器、pkg-config、libclang、OpenSSL、ALSA 和 FFmpeg 9 开发库。
BLE 需要 BlueZ；P2P 另需 NetworkManager、iw、wpa_supplicant 和 Polkit。

```sh
cargo build --workspace --release
```

## 项目结构

```text
core/src/
  lib.rs / prelude.rs   公共入口与常用导入
  common/               errors、time、storage、crypto
  protocol/             wire 编解码；messages/ 是类型化消息，字段号只在那里（含 Auth/Upgrade）
  identity/             账号、扫码登录、证书
  transport/            KCP、认证、BLE、P2P、mDNS
  session/              一条手机连接、Channel、入站 Server
  features/             desktop / apps / relay / camera / clipboard / files / mishare / notifications
  media/                libav、ALSA、v4l2、WFD
  bin/                  hyperclix-service、hyperclix-p2p-helper
```

依赖方向：`common ← protocol ← transport ← session ← features`；`media` 由 feature/UI 组合。

消息编码约定：

- `pb::Message`：自包含的 protobuf body，需要对称 `encode/decode`，用于嵌套/重复消息。
- inherent `encode/decode`：带路由或消息类型的 wire envelope，例如 Lyra physical/logical、Mirror type 帧、SDK control header、带判别字段的 union；也用于明确单向的消息。
- free function：固定 wire packet 的构造函数（心跳、frame wrapper、TLV/media packet）；只能放在 `protocol/messages`，feature 不得直接按字段号拼包。

常用类型从 crate 根导入，具体功能用对应模块：

```rust
use hyperclix_core::prelude::*;
use hyperclix_core::camera::CameraOptions;
use hyperclix_core::clipboard::Clipboard;
```

## 主要 API

| 入口 | 主要 API | 参数与结果 |
| --- | --- | --- |
| crate 根 | `Account::load`、`QrLogin`、`Session::connect`、`Server::start` | 账号目录、手机端点、连接选项；会话与入站服务 |
| `discovery` | `interfaces`、`discover`、`Announcement` | 本机 IPv4；发现手机端点、发布电脑公告 |
| `proximity` | `NearbyScanner`、`BleListener`、`Session::connect_ble`、`UpgradeClient` | BlueZ 近场发现、主动/被动认证、Wi-Fi Direct 建链 |
| `desktop` | `Session::open_desktop`、`open_app`、`Screen`、`Screen::drop_files` | App 包名及画布；输入、尺寸、音频、拖入文件 |
| `apps` | `Session::recent_apps` | 官方最近应用列表，含包名、名称和图标，当前最多四项 |
| `relay` | `Session::relay_monitor`、`RelayMonitor::state/subscribe/open` | CAST 通道中的应用接力：当前 App、浏览器 URL、WPS 文件和 App Intent；自动确认数据接收 |
| `clipboard` | `Clipboard::open_with_cache`、`publish`、`subscribe` | 缓存目录；文本、HTML、图片和文件 |
| `mishare` | `MiShare::send_files`、`receive_files` | 文件列表或接收目录；返回可取消的传输任务 |
| `notifications` | `configure`、`subscribe_cloud`、`register_push`、`NotificationReceiver` | 手机应用包名、云授权、推送身份；接收手机通知 |
| `camera` | `Session::open_camera`、`CameraPreview`、`CameraPreview::feed` | 镜头和尺寸；返回媒体配置及关闭句柄 |
| `media` | `MediaPlayer::play_screen/play_camera`、`CameraFeed` | 媒体播放；`follow_screen_audio` 见 `desktop` |

App 默认 720×1280、density 320；相机默认后置 1280×720；音频设备默认 `auto`。

典型顺序：加载账号 → 发现手机 → 建立 Session → 打开业务 → 消费事件／视频帧。
关闭顺序：`MediaPlayer::close` → 对应业务 `close` → `Session::close`。

日志位于 `$XDG_STATE_HOME/hyperclix/logs/`：`app-*.log` 保存结构化业务阶段与媒体状态，`native-*.log` 保存 libav/GTK 原始 stderr。

后台宿主：`cargo run --release -p hyperclix-core --bin hyperclix-service -- --interface wlp1s0`。
无局域网后台使用 `--listen-ble`；`--apps <手机IPv4>` 读取局域网手机的最近应用。
`--nearby` 扫描 BLE；`--ble-connect` 连接唯一同账号近场设备，均不需要 IPv4。
P2P 使用 `--p2p-interface wlp1s0` 选择无线接口；加 `--recent-apps` 在建链后读取最近应用，`--reconnect` 允许按回车关闭并重新认证、协商 P2P。
应用接力监听：`--ble-connect --p2p-interface wlp1s0 --relay`、`--interface wlp1s0 --apps <手机IPv4> --relay`，
或在后台 `--interface wlp1s0 --relay` 对每台被动连入的手机启动监听；
加 `--relay-open` 时收到内容后自动打开（浏览器 URL 用 `xdg-open`，WPS 文件下载到缓存后打开，普通 App 打开独立投屏）。
P2P 权限助手 `hyperclix-p2p-helper` 与宿主放在同一目录；系统授权后管理自有接口及独立 wpa_supplicant，退出时恢复原 P2P 管理状态。
库调用先创建 `UpgradeClient`，把其 `capabilities()` 放入 `ConnectOptions.wifi_capabilities`，再进行 BLE 认证和 `upgrade()`。
`follow_screen_audio` 的原始音量范围由调用方指定；当前手机使用 `0..150`，不修改电脑系统音量。

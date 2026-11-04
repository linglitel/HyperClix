# TODO List

更新：2026-11-02

## 已完成

- [x] Rust core、GTK4 测试界面、libav 解码及日志落盘。
- [x] 主账号扫码登录、证书加载与同账号认证。
- [x] 局域网和 BLE 发现、电脑主动连接、手机主动连接及后台监听。
- [x] 无共同局域网的 Wi-Fi Direct 建链、断开与重连。
- [x] 桌面及多 App 投屏、键鼠/触摸板控制、中文输入、关闭重开。
- [x] 关闭投屏窗口保留近场连接。
- [x] universalClipboard 双向文字同步。
- [x] 手机→电脑文件自动接收，快速建链修复已真机通过。
- [x] 手机相机流转与预览。
- [x] 官方最近应用列表及图标核心接口（目前最多四项）。
- [x] Core 分层重构：common/protocol/identity/transport/session/features/media，公共 API 根导出。
- [x] 协议消息层重写：类型化 Lyra/Mirror/SDK/文件/剪贴板/通知/Auth/Upgrade 消息；session 按职责拆分，移除 mirror/lyra 双模型。
- [x] Core 重写完成，进入真机回归阶段；协议字节、连接顺序、服务名和字段号按基线保留。
- [x] 清理旧协议兼容层、过时字节测试和未使用依赖（`protocol/lyra`、`FeatureSet`/`Handshake`、`bytes`/`hmac`）。

## 已实现，待真机确认

- [ ] 电脑→手机文件发送复验。
- [ ] 图片、文件、HTML 剪贴板同步。
- [ ] 手机→电脑通知的授权及完整接收流程。
- [x] 手机音量键与投屏音量/焦点联动，仅调节投屏声音；视图显示手机音量/静音状态（真机待验）。
- [x] 相机 metadata/command/P2P 全部会话级复用与 `-20001/-20002` 重关联重试（按官方 `CloseCamera` 只关预览，真机待验）。
- [x] 最近应用使用会话级 App 通道并复用；完整应用列表仍受 mirror `system/message` 传输限制。
- [x] 文件拖入投屏 App 的核心协议、资源传输与 GTK 拖放（真机待验）。
- [x] v4l2loopback 虚拟摄像头输出；新增 `tools/v4l2loopback.sh`（真机待验）。
- [ ] 临时热点脚本的实际启停。

## 下一轮重构（已记录）

- [x] 统一编码风格：已确定并写入 `core/README.md` 与 `protocol/messages/mod.rs`：`Message` 只用于自包含 body；inherent 用于带路由/类型的 envelope；free function 只做固定 packet 构造。
- [x] 完成 MiExpress 内层建模：新增 `ExpressPacket`，feature 不再按字段号读取；handshake/control/stream/data 均类型化。
- [x] 瘦身大文件：原 9 个 >500 行文件已拆分到 <500 行（clipboard、p2p/helper、desktop/screen、mishare、messages/lyra、notifications/slim、session/receive、ble/listener、mishare/express）。
- [x] `media/` 函数拆分：`decode` 291→181 行（`VideoOutput`），`wfd` 259→106 行，RTSP/RTP/能力表拆成 `handle_rtsp`/`handle_rtp_packet`/`wfd_capabilities`；真机媒体回归通过（26 帧显示、144 帧解码、无错误）。
- [ ] 上述重构完成并测试后：给仓库改名、建立 git 仓库并提交初始版本。

## 真机回归已知问题（下次修）

- [x] 跨设备相机：14:18 失败是手机 `com.milink.service` 残留状态；强停后真机通过，UI 日志显示 1280×720、23.4fps、117 帧解码/115 帧显示，cameraP2P 通道正常。互传同理需先强停小米互传。
- [x] 相机重开 30 秒超时根因：旧实现每次 `close` 发 `RESETDEFAULT` 并关闭 `distributedHardware` 通道；官方只关预览、会话内保持 association。已改为会话级复用，见 [报告](artifacts/camera-official/REPORT.md)，待真机复验。
- [x] 投屏音量键方向修正：手机侧音量键控制投屏音量并同步手机媒体音量；PC 媒体键映射已移除，视图显示手机音量/静音，待真机验证 `VolumeBasis`/`VolumeIndex` → PC 播放增益链路。
- [x] 剪贴板图片预览延迟：已抓到 13:14:56 时序，PC 传输 78ms；延迟在手机 `DialogActivity` 启动/最小展示，属于手机官方 UI 行为，不再改 PC 侧（不接受 `isCompleteData` 伪直传）。

## 未完成 / 暂缓

- [ ] 排查近场首次启动偶发“P2P 管理设备没有就绪”。
- [ ] 完整已安装应用列表：手机 `SearchApp` 走 mirror `system/message` 通道，当前 Lyra 通道只支持最近应用；需新增该传输层。
- [x] 应用列表点选启动、投屏窗口拖放；通知点击打开 App 仍未做。
- [ ] 性能优化、硬件解码与零拷贝（暂缓）。

当前范围外：远程互传、电脑→手机通知、Windows/macOS 兼容。

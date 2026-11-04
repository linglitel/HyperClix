# HyperClix GTK4 界面

在项目根目录启动：

```sh
./ui/run.sh
```

开发启动：`cargo run -p hyperclix-ui`。UI 依赖 GTK4 4.12+、libadwaita 1.5+；core 依赖见 [core README](../core/README.md)。

加载已有身份后，在“我的设备 → 蓝牙与 Wi-Fi Direct”选择无线网卡，点“蓝牙发现手机”，再连接发现的同账号手机；两端无需共同局域网，但都需开启 Wi-Fi 与蓝牙。首次创建近场接口时按系统提示授权。
点“允许手机主动连接”可持续广播并等待手机。关闭投屏窗口保留设备连接；“断开当前连接”保留监听，“停止后台监听”关闭近场服务。连接显示“Wi-Fi Direct 已就绪”后，可用下面各页验证实际功能。
局域网连接仍使用同页下方的网络接口与发现入口。首次登录使用界面中的扫码入口。
桌面与应用页按包名打开 App，提供键鼠、中文输入、尺寸调整、全屏和静音。
剪贴板页启用 universalClipboard；连接后自动接收手机文件，文件页可设置接收目录（连接时生效）和发送文件；通知页接收手机通知。
相机页选择镜头和尺寸，可输出到已有的 v4l2loopback 节点。

每次启动生成 `artifacts/hyperclix/logs/app-*.log`（结构化业务日志）和 `native-*.log`（GTK/libav/ALSA 原始 stderr）；运行日志页打开 app 文件。
图片及文件剪贴板缓存位于 `artifacts/hyperclix/clipboard-cache/`。

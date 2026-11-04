# HyperClix

Linux 专用。Rust library core、GTK4 测试界面，媒体直接调用 libav。

```sh
./ui/run.sh
```

- `core/`：账号、连接与业务协议，见 [接口入口](core/README.md)。
- `ui/`：测试界面，见 [启动说明](ui/README.md)。
- `tools/lab_hotspot.sh`：临时热点管理。
- `tools/v4l2loopback.sh start 42`：创建相机虚拟摄像头设备；界面中填 `/dev/video42`。
- `artifacts/hyperclix/logs/`：`app-*.log` 业务日志，`native-*.log` 只收 GTK/libav/ALSA 原始 stderr；`HYPERCLIX_LOG=debug` 调整级别。
- `artifacts/`：本机账号、缓存、接收文件及原始样本。

旧 Python 和研究资料已移至 `artifacts/archive/legacy-20261030/`，不参与当前构建。

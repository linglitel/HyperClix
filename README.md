# HyperClix

Linux 专用。Rust library core、GTK4 测试界面，媒体直接调用 libav。

```sh
./ui/run.sh
```

- `core/`：账号、连接与业务协议，见 [接口入口](core/README.md)。
- `ui/`：测试界面，见 [启动说明](ui/README.md)。
- `tools/lab_hotspot.sh`：临时热点管理。
- `tools/v4l2loopback.sh start 42`：创建相机虚拟摄像头设备；界面中填 `/dev/video42`。
- `$XDG_STATE_HOME/hyperclix/logs/`（默认 `~/.local/state/hyperclix/logs/`）：`app-*.log` 业务日志，`native-*.log` 只收 GTK/libav/ALSA 原始 stderr；`HYPERCLIX_LOG=debug` 调整级别。
- `~/.local/share/hyperclix/`：账号、接收文件、云/推送状态（`$XDG_DATA_HOME`）。
- `~/.cache/hyperclix/`：剪贴板缓存（`$XDG_CACHE_HOME`）。
- `artifacts/`：研究样本和旧资料，不含运行凭据。

旧 Python 和研究资料已移至 `artifacts/archive/legacy-20261030/`，不参与当前构建。

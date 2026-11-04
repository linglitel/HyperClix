# 开发约定

- 失败必须明确提示并清理受影响资源；禁止用占位、静默 fallback 或伪造成功掩盖失败。
- Linux 专用；`core/` 是 Rust library，`ui/` 是 GTK4 测试界面（不需要投入时间和精力），媒体直接调用 libav。
- 按功能拆模块，参数采用合理默认值，保持实现直接。
- 以真实落盘日志定位故障；只做必要编译，不新增或运行合成测试替代真机诊断。
- 文档只写当前用法和必要参数，保持简短。旧 Python、流程和研究资料在 ignored 的 `artifacts/archive/legacy-20261030/`，不作为开发入口。
- 剪贴板使用 universalClipboard；CAST 保留投屏，旧 CAST 文本能力默认关闭。
- 通知范围是手机→电脑；远程互传、硬解和零拷贝暂不做。
- 当前只补核心协议能力：BLE/P2P、被动连接与后台在线、App 列表/图标、投屏音量/焦点；前端拖放与通知点击交互跳过。
- 使用现有自有账号和 OTHER_PC21 证书。SDK Linux3、MiShare Windows2 是平台枚举；入站 Lyra 公告身份与证书类型 21 一致。
- 凭据、剪贴板内容和个人数据保存在 XDG 目录（`$XDG_DATA_HOME/hyperclix`、`$XDG_CACHE_HOME/hyperclix`），不写入源码或聊天。
- 日志位置：`$XDG_STATE_HOME/hyperclix/logs/`（默认 `~/.local/state/hyperclix/logs/`）。账号及推送状态在 `$XDG_DATA_HOME/hyperclix`。
- 手机端点按当前网络重新发现；扫码、云登记和手机设置由用户明确操作。
- `tools/lab_hotspot.sh` 管理独立 xclab0 临时热点，保留原网络连接。
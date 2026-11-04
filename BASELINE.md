# Core rewrite baseline

建立时间：2026-11-02 19:35（Asia/Shanghai）

- 源码快照：`artifacts/baseline-20261102-1935/core-source.tar.gz`
- 快照 SHA-256：见 `artifacts/baseline-20261102-1935/core-source.sha256`
- 真实抓包校验和：`artifacts/baseline-20261102-1935/captures.sha256`
- 工具链：`artifacts/baseline-20261102-1935/toolchain.txt`
- 结构统计：`artifacts/baseline-20261102-1935/structure.txt`
- 基线测试：13 passed，见 `artifacts/baseline-20261102-1935/tests.txt`
- `cargo fmt --all --check` 通过。

这份快照用于重写前后的字节级/行为级对照。重写期间：协议字节、连接顺序、服务名、字段号不得变化；真机测试留到重写完成后统一进行。

重写后记录：`artifacts/baseline-20261102-1935/REFACTOR-RESULT.txt`。

Core 重写完成记录：消息层/会话架构已迁移，`cargo test -p hyperclix-core --lib` 21 passed；真机回归待执行。

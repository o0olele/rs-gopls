# Go LSP JSONL 诊断日志

| 项目 | 内容 |
| --- | --- |
| 状态 | 已完成 |
| 创建日期 | 2026-11-27 |
| 用户确认 | 已确认（2026-11-27 用户明确选择先实现方案 1：仅增加 RS_GOPLS_LOG_FILE 轻量 JSONL 诊断日志并编译发布版，不改业务语义） |
| 更新日期 | 2026-11-27 |
| 完成日期 | 2026-11-27 |

## 目标与范围

通过 `RS_GOPLS_LOG_FILE` 收集大型 GOPATH 项目的包图加载、请求排队、跳转卡顿与两次查找引用的诊断数据。默认关闭，开启入口为进程环境或 `initializationOptions.env`。记录时序、分类、计数和脱敏路径，保持现有语义、缓存和调度决策。

## 验收标准

- [x] 关闭时不创建文件，日志调用门控无锁、无分配。两种配置入口生成合法 JSONL，支持确定性路径哈希，有界队列满时累计丢弃并继续处理请求。
- [x] JSONL 覆盖会话与配置、请求及通知、失效与包图加载、定义与引用、主循环长操作。关键开始事件及时交给写线程刷新，正常退出排空。日志不含源码或个人绝对路径。
- [x] 合成 stdio 会话使用发布版运行初始化、定义、两次引用和退出，逐行解析 JSON，核对核心事件与请求结果。验证日志关闭和脱敏。
- [x] Cargo 检查、测试、发布构建零告警，Python 测试、格式和差异检查通过，记录发布版路径、大小与 SHA-256。

## 实现依据

- 已检查 GoLand `GO-263.7307.354` 的安装包 `plugins/go-plugin/lib/modules/intellij.go.impl.jar`，SHA-256 为 `2a13f1a753d5e15db70c5758e17db1dc28cc790e1d329e574d8b6a078d636784`。类文件包含 `com.goide.psi.impl.GoReference` 的 `processUnqualifiedResolve`、`processQualifier`，以及 `com.goide.usages.GoFindUsagesHandlerFactory.createFindUsagesHandler` 和 `GoFindUsagesHandler.getFindUsagesOptions`。已有运行态采样记录了 Go 专用 Find Usages 处理器及搜索范围。[定义计划](2026-11-25-go-definition.md) · [引用基线](../goland-references-baseline.md)
- Rust 在现有请求、调度、包图加载、语义分支及实际 stdio 写出点采样。所有日志数据通过独立有界通道交给写线程。发布版会话验证结果与处理阶段的对应关系。

## 实施步骤

1. 实现进程与初始化配置入口、会话元数据、路径过滤、异步 JSONL 与退出刷新。
2. 在现有请求、包图和语义查询中记录阶段与计数，在可能阻塞的操作边界采样。
3. 验证两种开关、默认关闭和路径脱敏，运行发布版会话与全量检查，记录产物。

## 验证计划

沿用环境配置的 Cargo 产物目录。先运行 `cargo check -p rs-gopls --locked`，再执行 `cargo check --workspace --locked`、`cargo test --workspace --locked`、`cargo build -p rs-gopls --release --locked`、`python -B -m unittest discover -s tools -p 'test_*.py'`、`cargo fmt --all -- --check` 与 `git diff --check`。用合成 Go 工作区核对定义及两次引用的完整目标和日志字段。

## 完成记录

- 2026-11-27 用户确认实施，建立计划。
- 日志使用容量为 4096 的队列，写线程追加 JSONL 并报告 `log_dropped`。单会话的 `ts_ms` 从进程启动开始计算。进程环境优先于初始化配置。工作区内使用相对路径，根外使用文件名。`RS_GOPLS_LOG_REDACT=1` 用确定性短哈希替代路径，字符串请求 ID 始终记录哈希。主循环长操作阈值为 20 ms，stdout 写出阈值为 10 ms。
- `rpc_done` 从请求读取计到 stdout 刷新，拆分执行、序列化、写出和调度等待。引用的 `wall_ms` 截至响应就绪，包含等图和等待下一批扫描的时间。包图安装、元数据校验和请求重放分别记录耗时。
- Rust 单测使用实际 Go 包图验证两种开启入口、关闭不创建文件、路径哈希、包图失效和满队列丢弃。发布版 stdio 测试使用 Go 1.24.3 在合成双 GOPATH 夹具上逐项核对跨包定义与两次引用的完整结果。首个定义等图，首次引用等测试视图，第二次引用直接查询。暂停 `go list` 时可读取已经刷新的 PID 和开始事件，等待中的引用返回 `-32800`。逐行解析了完整 JSONL，确认日志追加、正常退出、脱敏一致性和源码标记过滤。

| 检查 | 实际结果 |
| --- | --- |
| `cargo check -p rs-gopls --locked` | 通过，零编译告警 |
| `cargo check --workspace --locked` | 通过，零编译告警 |
| `cargo test --workspace --locked` | 307 项通过，零编译告警 |
| `cargo build -p rs-gopls --release --locked` | 通过，零编译告警 |
| `python -B -m unittest discover -s tools -p 'test_*.py'` | 42 项通过，包含对最终发布版的 4 项 stdio 验证 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |

发布版为 `target/release/rs-gopls.exe`，大小为 8,111,616 字节，SHA-256 为 `6316b8825037fdb4be1abe3124face8928d4334cd6421f06b0d4928af2f1ee0d`。

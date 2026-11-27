# 支持 LSP initializationOptions.env、配置热更新与 GOPATH 自动回退

| 项目 | 内容 |
| --- | --- |
| 状态 | 进行中（代码、构建与自动化验收已通过，待实机对照） |
| 创建日期 | 2026-11-27 |
| 用户确认 | 已确认（2026-11-27 用户确认按计划实施） |
| 更新日期 | 2026-11-27 |
| 完成日期 | 待填写 |

## 目标与范围

Zed 项目的 `.zed/settings.json` 可以用 `initialization_options.env` 为 Go 语言服务设置环境。用户仅在此处配置多段 `GOPATH`（如 `<WORKSPACE>\serverbase;<WORKSPACE>\server`）时，官方 `gopls` 能跨包跳转，`rs-gopls` 当前未应用这份环境。启动时应用该配置，并允许在同一会话内修改配置后恢复正确的包解析、导航和诊断。

1. **读取会话配置并热更新**。在 `src/main.rs` 从 `initialize.params.initializationOptions` 和 `workspace/didChangeConfiguration` 的 `params.settings` 提取 `env`。按顶层 `env`、`rs-gopls.env`、`gopls.env`、`settings.env` 的顺序采用第一个对象，只保留字符串条目，按键固定顺序。以服务器启动时继承的进程环境为底层（含 Zed `binary.env`），会话 `env` 覆盖同名键。空串原样传给 Go，例如 `GOWORK=""` 清空继承的显式值，由 Go 恢复自动发现。
   - 通知未包含有效 `env` 对象时保留当前配置，显式 `env: {}` 撤销会话覆盖并恢复启动环境。配置通知中的有效 `env` 对象整体替换会话覆盖。比较合并环境及影响自动回退的会话显式键，确认有效配置变化后由 `Engine` 更新快照和包图代次，清除过期包图、诊断、补全 SDK 与 `gofmt` 缓存，取消依赖旧环境的在途查询，调度后台加载。仅当前代次可安装包图和刷新诊断，保留打开的编辑缓冲区。
2. **让 Go 子进程和缓存共享同一环境快照**。
   - 根发现覆盖 `src/workspace_roots.rs` 的 `folder_groups`、`folder_context`、`list_roots_for_folders`、`list_roots`，以及 `go env -json`、`go work edit -json`。根发现的环境判定从会话快照及 Go 探测读取。
   - 包图与变更检测覆盖 `src/workspace.rs` 的 `Workspace::{load, load_roots, load_roots_for_rename, load_graph}`、`go list` 及 builtin 回退的 `go env GOROOT`，`src/workspace/inputs.rs` 的 `PackageInputs::{configuration, current}`，以及 `src/scheduler.rs` 的后台加载。包图及输入指纹使用同一份快照，重命名的刷新检查能识别环境变更。
   - `src/semantic/completion/{sdk,imports}.rs` 与 `src/semantic/actions/formatting.rs` 的 `go env`、`go list`、`gofmt` 及工具查找使用会话环境。`ProbeInputs`、`BuildKey` 和 `GofmtContext` 纳入该环境或能唯一标识该环境的键；配置切换前后的探测、SDK 包清单 和格式化工具位置各自对应有效环境。
3. **兼容官方 `gopls` 的 `GO111MODULE=auto` 回退**。会话 `env` 提供非空 `GOPATH` 且没有显式 `GO111MODULE` 时，按构建组用合并环境运行 `go env -json GO111MODULE GOWORK GOMOD GOPATH`。同时满足下列条件时，仅对该组用于加载包图的 `go list` 自动补充 `GO111MODULE=auto`。
   - 有效 `GO111MODULE` 为空，`GOMOD` 为空或为 `NUL`、`/dev/null`，且 `GOWORK` 为空或为 `off`。
   - 构建组目录等于或位于任一有效 `GOPATH` 条目下，或为任一有效条目的直接父目录。按平台拆分 `GOPATH`，保留条目顺序，按路径分量比较。支持单开 `server`、双根开 `server` 与 `serverbase`、以及打开共同父目录。
   - 尊重合并环境和 Go 探测给出的显式模式，包括来自启动进程、`go env -w` 或会话配置的非空 `GO111MODULE`。活动 `go.mod`、`go.work` 沿用现有探测、归组及加载路径。
4. **对齐 Zed 扩展与 README**。在 `zed-extension/src/lib.rs` 评估并接入 `language_server_workspace_configuration`，通过当前语言服务器 ID 读取并转发 `lsp.rs-gopls.settings`。在 `README.md` 给出两种脱敏示例。使用 opt-in 扩展时配置 `lsp.rs-gopls.initialization_options.env`。使用 Zed 内置 Go 适配器时，用 `lsp.gopls.binary.path` 指向 `<RS_GOPLS_BINARY>` 并复用 `lsp.gopls.initialization_options.env`。说明运行时设置与既有 `binary.env` 的优先级。

## 验收标准

- [x] 无 `go.mod`、`go.work` 的双 GOPATH 夹具只通过 `initializationOptions.env.GOPATH` 传入路径，启动进程和 Go 持久化配置均未设置模块模式。跨包定义、引用、文档及工作区符号、调用层级与类型层级返回预期目标与 UTF-16 范围。合法导入的 `import.invalid` 数量为零。
- [x] 分别单开 `<WORKSPACE>\server`、打开 `<WORKSPACE>\server\src` 下的包目录、双根开 `<WORKSPACE>\server` 与 `<WORKSPACE>\serverbase`、打开共同父目录 `<WORKSPACE>`。逐项检查对应搜索范围内的跨包目标、完整引用集、符号集、层级边和诊断。覆盖组目录恰等于 GOPATH 条目、位于条目之下和为条目直接父目录三种关系。
- [x] 在打开文档的同一 LSP 会话内用 `workspace/didChangeConfiguration` 将 `GOPATH` 从可解析路径切到缺少依赖的路径，再切回。包图、定义、引用和诊断随新配置刷新，拉取诊断刷新或推送诊断符合客户端能力。暂停旧加载后切换配置，验证旧结果无法安装、过期查询仅取消一次、最新图最终可用。
- [x] 四种 `env` 对象位置解析一致，重叠时按既定顺序选择，忽略非字符串条目。合并环境及回退条件相同的通知、`{}`、`null` 和缺少 `env` 的通知保留当前环境与包图代次。显式 `env: {}` 恢复启动环境。覆盖 `binary.env` 继承、同名值覆盖及 `GOWORK=""` 原样传递。
- [x] 执行环境变更后，核对根发现、加载和重命名输入检测、builtin 定位、补全 SDK 和格式化使用最新环境。相同环境可命中缓存，变化后命中对应新环境，旧 SDK 探测和 `gofmt` 路径不再参与请求。
- [x] 来自进程、持久化 `go env -w` 或 LSP 的有效 `GO111MODULE=on` / `off` 均保留。活动模块和 `go.work` 包图、导航与诊断通过现有回归。无活动模块及工作区但目录不匹配 GOPATH 的对照用例不触发回退。
- [ ] 在实际 Zed 中验证两种 README 配置都能启动正确二进制并读到 GOPATH。扩展路径下修改 `lsp.rs-gopls.settings.env` 后，确认配置通知、包图结果与诊断变化匹配。
- [x] Go 1.24 与 Go 1.27 的专项回归通过。`cargo check --workspace --locked` 与 `cargo test --workspace --locked` 均通过且零告警，Python 单测和全量工作区回归通过。

## 实现依据

### 官方 gopls v0.24.0

- `server/general.go` 的 `Initialize` 通过 `options.Set(params.InitializationOptions)` 应用初始化配置。`settings/settings.go` 将顶层 `env` 转为环境映射，并兼容历史整数值。本计划接收指定位置的字符串值。[Initialize](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/server/general.go#L42-L68) · [env 解析](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/settings/settings.go#L1100-L1121)
- `cache/view.go` 的 `FetchGoEnv` 使用配置覆盖探测 GOPATH 和模块模式。`adjustedGO111MODULE` 保留非空的有效模式，空值返回 `auto`。`View.Env` 依次组装进程、配置、模块模式和视图覆盖。`defineView` 优先识别工作区或模块，其 GOPATH 视图检查目录是否位于任一 `<GOPATH>/src` 下。Rust 按构建组实现回退，额外覆盖编辑器打开 GOPATH 根及其直接父目录的场景。[模式调整](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/cache/view.go#L218-L226) · [View.Env](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/cache/view.go#L334-L341) · [defineView 与 FetchGoEnv](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/cache/view.go#L874-L963)
- `cache/load.go` 对 GOPATH 视图使用 `./...`，并将 `View.Env()` 传入 `packages.Config.Env`。`server/workspace.go` 收到配置变更通知后按客户端能力请求配置并重建视图，随后刷新诊断。本计划从通知的 `settings` 直接取值以适配 Zed。[加载范围](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/cache/load.go#L95-L128) · [传递环境](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/cache/load.go#L329-L355) · [配置更新](https://github.com/golang/tools/blob/gopls/v0.24.0/gopls/internal/server/workspace.go#L59-L133)

### GoLand GO-263.7307.354

已通过 `build.txt` 确认构建号。检查的安装包为 `<GOLAND_HOME>/plugins/go-plugin/lib/modules/intellij.go.impl.jar`，SHA-256 为 `2a13f1a753d5e15db70c5758e17db1dc28cc790e1d329e574d8b6a078d636784`。可用发行包中的 `jbr/bin/java.exe` 和 `lib/intellij.libraries.asm.jar` 的 ASM `Textifier` 复核以下字节码。

| 类与方法 | 字节码观察 |
| --- | --- |
| `com.goide.project.DefaultGoRootsProvider.getGoPathRoots`，行 34–50 | 优先收集模块或项目配置的 GOPATH 根，按设置纳入环境 GOPATH，集合为空时取默认根。`GoEnvironmentGoPathRootsKt.parseGoPathEntries`（行 117–124）拆分并规范化绝对路径。`getInnerGoPathSources`、`getRoots`（行 106–114）逐根寻找 `src` 并过滤空结果。 |
| `com.goide.project.DefaultGoRootsProvider$RetrieveSubDirectoryOrSelfFunction.apply`，行 126 | 根目录名已匹配时使用根本身，否则对有效根调用 `findChild` 查找同名直接子目录。 |
| `com.goide.execution.extension.GoExecutorExtension.getGoSpecificEnvironment`，行 32–45 | 将各个 GOPATH 根转为系统路径，以 `File.pathSeparator` 拼接到 `GOPATH`，再合并执行扩展环境。 |
| `com.goide.vgo.cli.VgoExecutorExtension.getExtraEnvironment`，行 21–29 | 输入环境缺少 `GO111MODULE` 键时，模块集成启用则注入 `on`。模块集成关闭且 SDK 默认启用模块模式时注入 `auto`。 |

Rust 采用平台路径列表和显式模块模式优先的行为。GoLand 的注入判断来自 IDE 设置和 SDK，Rust 以 `cmd/go` 的有效配置及构建组目录为依据。运行验证需检查共同父目录下 `go list` 产出的包 ID 是否沿用 GOPATH 导入路径，并确认搜索范围内的目标完整。

### Zed 适配器与 Rust 锚点

- Zed 源码提交 `46375fbf6457` 按语言服务器 adapter ID 读取 `lsp.<id>`。仓库扩展在 `extension.toml` 注册 `rs-gopls`，内置 Go adapter 的 ID 为 `gopls`。`binary.path` 选择二进制，Zed 仍从原 adapter 的配置项合入 `initialization_options`，并把 `binary.env` 交给进程。Zed 启动后发送 `workspace/didChangeConfiguration`，之后从 adapter 的 workspace configuration 取通知内容。扩展配置 hook 的默认空值可能转为空对象，服务端须保留未被显式替换的初始化配置。[Go adapter](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/languages/src/go.rs#L211-L269) · [初始化与通知](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L653-L723) · [启动环境](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L790-L838) · [扩展配置 hook](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/language_extension/src/extension_lsp_adapter.rs#L331-L355) · [配置变更通知](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L9878-L9936)
- 扩展锁定的 `zed_extension_api` 0.6.0 提供 `LspSettings::for_worktree` 和 `language_server_workspace_configuration`，可按语言服务器 ID 读取并返回 worktree 根的 JSON 设置。当前扩展从 `worktree.shell_env()` 继承启动环境。验证时记录实际 Zed 版本及其发出的初始通知、变更通知和空配置结果。[扩展注册](../../zed-extension/extension.toml) · [启动命令](../../zed-extension/src/lib.rs#L12) · [依赖版本](../../zed-extension/Cargo.toml)

| 范围 | 现有 Rust 入口 |
| --- | --- |
| 初始化、通知与诊断 | `initialize`、`handle_notification`、`Running::workspace_loaded`。[main.rs](../../src/main.rs#L297) |
| 引擎与后台包图 | `Engine::{with_roots, install_workspace}`、`Scheduler::{start_workspace_load, workspace_modified}` 已有代次检查及取消入口。[semantic.rs](../../src/semantic.rs#L171) · [scheduler.rs](../../src/scheduler.rs#L75) |
| 根发现、加载及输入指纹 | `folder_groups`、`go_json`、`Workspace::load_graph`、`list_packages`、`PackageInputs::{configuration, current}`。[workspace_roots.rs](../../src/workspace_roots.rs#L28) · [workspace.rs](../../src/workspace.rs#L95) · [inputs.rs](../../src/workspace/inputs.rs#L22) |
| SDK 与格式化 | `ProbeInputs`、`BuildKey`、`ImportCache::prewarm`、`GofmtContext` 和 `Engine::format_document`。[sdk.rs](../../src/semantic/completion/sdk.rs#L85) · [imports.rs](../../src/semantic/completion/imports.rs#L119) · [formatting.rs](../../src/semantic/actions/formatting.rs#L62) · [semantic.rs](../../src/semantic.rs#L518) |

## 实施步骤

1. 建立确定顺序的会话环境模型，捕获启动环境并合并配置覆盖。接入 `initialize` 和 `workspace/didChangeConfiguration`，复用 `Engine` 代次、调度取消和诊断刷新路径。交付可测试的解析、覆盖与环境切换契约。
2. 把不可变环境快照传给根发现、`Workspace` 各加载入口、`PackageInputs`、后台调度、补全和格式化，以快照约束 Go 命令及缓存。实现按组探测及自动回退，核对三种打开方式的包身份、图范围和过期结果处理。
3. 在 Zed 扩展中读取并转发当前 worktree 的 `lsp.rs-gopls.settings`。更新 README 的两种初始化配置示例及热更新入口，核对选择适配器与选择可执行文件的行为。
4. 增补 Rust 单测及 Python LSP 回归。扩展 `tools/lsp_harness.py` 支持传入初始化选项，复用多根会话和受控加载设施，在 `tools/workspace_checks/graph_refresh.py` 覆盖回退与热更新。使用实际 Go 1.24、Go 1.27 和 Zed 验证，再运行全量工作区回归并更新本计划的验收结果。

## 验证计划

### 构建与自动化回归

沿用环境配置的 Cargo 产物目录及项目约定的工具链、profile、features 和 target。先检查受影响包，再执行专项测试及全量回归。禁止传入 `--target-dir`、覆盖 `CARGO_TARGET_DIR` 或 `CARGO_BUILD_BUILD_DIR`，或另建编译目录重试。

```powershell
cargo check -p rs-gopls --locked
cargo check -p rs-gopls-zed --locked
cargo test -p rs-gopls --locked scheduler
cargo test -p rs-gopls --locked workspace
cargo test -p rs-gopls --locked semantic::completion
cargo test -p rs-gopls --locked semantic::actions
cargo check --workspace --locked
cargo test --workspace --locked
cargo build -p rs-gopls --release --locked
cargo build -p rs-gopls-zed --release --locked --target wasm32-wasip2

$targetDirectory = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
$server = Join-Path $targetDirectory 'release\rs-gopls.exe'
python -B -m unittest discover -s tools -p 'test_*.py'
python -B .\tools\check_workspace_updates.py $server --module graph_refresh
python -B .\tools\check_workspace_updates.py $server
```

- 生成两个独立 GOPATH 下的合成包，布局为 `<WORKSPACE>\server\src\example.test\server` 与 `<WORKSPACE>\serverbase\src\example.test\serverbase`。用跨包调用、接口实现及嵌入覆盖导航、符号和层级，核对完整目标集和范围。复用 `check_gopath` 的进程环境基线与 `check_async_refresh` 的受控 `go list`。自动回退用例移除 harness 默认的 `GO111MODULE=on`，隔离继承的 GOPATH、GOWORK 和 Go 持久化环境。单独用受控 `GOENV` 文件验证 `go env -w` 的优先级。
- 分别用实际 Go 1.24 与 Go 1.27 运行专项，记录 `go version`、无模块时的 `go env -json GO111MODULE GOWORK GOMOD GOPATH`、有无回退的加载结果，以及模块与 `go.work` 对照。验证 Windows 多段 GOPATH，并为路径列表拆分、分量匹配、空值和 `NUL`、`/dev/null` 增补单测。记录夹具和环境时使用占位路径。

### Zed 与 GoLand 实机对照

- 用固定的合成夹具测试两种 Zed 配置，核对生效的 adapter ID、启动二进制、初始配置及运行时通知。以官方 `gopls` v0.24.0 的结果复核初始化 GOPATH 场景，记录打开根、搜索范围及结果差异。
- 在同一主机运行 GoLand `GO-263.7307.354` 和 rs-gopls 发布构建，配置相同的两段 GOPATH。固定夹具修订、Go SDK、搜索范围、标识符位置、打开文件及编辑序列，核对完整结果与 UTF-16 范围。对结果一致的导航样本记录索引就绪首查、重复查询、编辑后查询的 P50/P95 和样本数，注明缓存状态、计时起止和界面开销。启动到首次正确结果以相同起止点单列。逐项记录范围、落点或错误差异。[导航性能对照](../PLANS.md#导航性能对照)

## 完成记录

- 2026-11-27 用户确认按计划实施，启动第一阶段的 Rust 核心、Zed 扩展、README 和 Rust 单测工作。GoLand 字节码、上游源码和仓库入口已在计划阶段核对。
- 第一阶段实现会话快照、配置热更新、按组 `GO111MODULE=auto` 回退、补全 SDK 与 `gofmt` 缓存失效，以及 Zed worktree 配置转发和 README 示例。调度器沿用单后台加载线程，旧加载返回后开始当前代次的加载，并丢弃旧代次结果。
- Rust 专项验证覆盖环境解析与覆盖、失效代次、单次取消、未保存缓冲区及两种诊断更新方式。Go 1.24.3 通过包图测试验证包子目录、单 GOPATH 根、双根和共同父目录四种打开方式的规范导入路径。测试还核对了外部 `GOWORK` 的重命名输入追踪、持久化模块模式，以及补全 SDK 和格式化的缓存行为。
- 第一阶段 Cargo 检查、测试及发布构建均通过，零编译告警。`cargo check -p rs-gopls --locked`、`cargo check -p rs-gopls-zed --locked` 和 `cargo check --workspace --locked` 通过。`cargo test -p rs-gopls --locked` 四组专项分别通过 17 项（`scheduler`）、28 项（`workspace`）、42 项（`semantic::completion`）和 43 项（`semantic::actions`）。`cargo test --workspace --locked` 共通过 304 项。`cargo fmt --all -- --check` 和 `git diff --check` 通过。
- `cargo build -p rs-gopls --release --locked` 产物 `rs-gopls.exe` 的 SHA-256 为 `89d2d007b10815d3dc37c1b403840eb2f05f8464ac65f619d8ca68cbce050492`。`cargo build -p rs-gopls-zed --release --locked --target wasm32-wasip2` 产物 `rs_gopls_zed.wasm` 的 SHA-256 为 `835cb153fa198dbd20a3262f558335e48ec48b0d0f1aeca2fe7cd8ed97caae27`。
- 第二阶段新增三项端到端用例，覆盖双 GOPATH 的四种打开方式、`cmdmsg` 和 `base/glog` 的完整导航目标及 UTF-16 范围、配置生效顺序以及推送和拉取诊断。受控 `go list` 验证了等效或无有效 `env` 的通知保留待处理查询和加载代次，过期查询收到一次 `ContentModified`，旧加载结束时未触发诊断刷新。切回完整 GOPATH 后恢复跨包定义和引用，未保存缓冲区中的查询仍可用。[工作区回归](../../tools/workspace_checks/graph_refresh.py)
- 使用本机两套 SDK，`go version` 分别报告 `go1.24.3 windows/amd64` 和 `go1.27.4 windows/amd64`。`python -B .\tools\check_workspace_updates.py <server> --module graph_refresh` 在每套 SDK 下均通过 7 项。两套 SDK 在匹配 GOPATH 的无模块目录中使用 `GOENV=off` 和 `GOTOOLCHAIN=local`，移除继承的模块模式和 `GOWORK` 后，`go env -json GO111MODULE GOWORK GOMOD GOPATH` 的脱敏输出一致。

  | 模式 | `GO111MODULE` | `GOWORK` | `GOMOD` | `GOPATH` | `go list -deps ./...` |
  | --- | --- | --- | --- | --- | --- |
  | SDK 默认 | `""` | `""` | `"NUL"` | `<WORKSPACE>\server;<WORKSPACE>\serverbase` | 退出 1，找不到活动主模块 |
  | 显式设置 `auto` | `"auto"` | `""` | `""` | `<WORKSPACE>\server;<WORKSPACE>\serverbase` | 退出 0，列出 `cmdmsg` 和 `example.test/server` |

- 两套 SDK 的临时 LSP 对照夹具均覆盖启用 `go.mod` 和 `go.work` 时与 GOPATH 匹配的根，跨包定义和引用正确，两端文件的拉取诊断均为空。在发布版的同一 LSP 会话内通过 `PATH` 将 SDK 从 Go 1.24.3 切到 Go 1.27.4 再切回，`int` 的类型定义命中当前 SDK 的 `builtin.go`，`crypto/hpke` 仅在 Go 1.27.4 出现于导入补全中，三次格式化编辑均可正确应用。Rust 单测验证补全 SDK 和 `gofmt` 缓存的命中与更换，以及重命名输入跟随会话环境。
- 第二阶段重新运行 `cargo check --workspace --locked` 和 `cargo test --workspace --locked`，通过 304 项 Rust 测试，零编译告警。两项发布构建均成功，零编译告警，复核 `release/rs-gopls.exe` 的 SHA-256 为 `89d2d007b10815d3dc37c1b403840eb2f05f8464ac65f619d8ca68cbce050492`，`wasm32-wasip2/release/rs_gopls_zed.wasm` 的 SHA-256 为 `835cb153fa198dbd20a3262f558335e48ec48b0d0f1aeca2fe7cd8ed97caae27`。Python 单测 38 项通过，Go 1.27.4 下全量工作区回归 60 项通过。`cargo fmt --all -- --check` 和 `git diff --check` 通过。
- 待用户在 Zed 实机验证两种 README 启动配置和扩展热更新，验收项保持待勾选。计划所列的 GoLand 运行态导航性能对照与官方 `gopls` 实机对照仍待执行。

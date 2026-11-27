# Track F · 基座、层级与性能开发计划

| 项目 | 内容 |
| --- | --- |
| 状态 | 已完成 |
| 创建日期 | 2026-11-27 |
| 用户确认 | 已确认（2026-11-27 确认 F + B + D 三线联合开工，Codex 使用 full-access 模式，严格路径脱敏） |
| 更新日期 | 2026-11-27 |
| 完成日期 | 2026-11-27 |

## 目标与范围

按 [并行协作与模块拆分](../COLLABORATION.md#track-f--基座层级与性能) 统筹实现 **Track F · 基座、层级与性能**，与 Track B（实时诊断）及 Track D（格式化与代码操作）协同推进，构成零外部依赖的 `F → B → D` 完整链路，同时为并行开发 Track A / C / E 的成员提供稳定的公共基座与跨线契约。[项目愿景](../vision.md)

### 本阶段覆盖范围

1. **公共基座演进与 `F → B` 正式契约（`src/workspace.rs`、`src/workspace_roots.rs`、`src/main.rs`）**
   - **多 Workspace Folder 与动态变更**：解除 `main.rs` 中 `folders.len() <= 1` 限制，支持多根工作区初始化与 `workspace/didChangeWorkspaceFolders` 增删目录（声明 `workspace.workspaceFolders` 能力），统一合并各工作区根下的 `go.work` / Go Modules / GOPATH 根目录并触发包图刷新与受影响查询失效。
   - **结构化 `go list` 包错误（`F → B` 契约）**：扩展 `go list -e -json=...` 采集 `Error` 与 `DepsErrors`，解析 `PackageLoadError`（包含工作区根、包 ID、`for_test` 身份、错误类别、消息、`ImportStack`、可选文件绝对路径与 1-based 行列），通过 `Workspace::package_errors(&self) -> &[PackageLoadError]` 只读暴露，供 Track B 直接消费。
   - **薄接线护栏**：维护 `src/dispatch.rs`、`src/scheduler.rs`、`src/main.rs`、`src/semantic.rs` 的统一注册与生命周期钩子，确保 Track B/D 及其他平行 Track（A/C/E）可低冲突接入。
2. **Call Hierarchy（`textDocument/prepareCallHierarchy`、`callHierarchy/incomingCalls`、`callHierarchy/outgoingCalls`）**
   - 独占 `src/semantic/hierarchy/call.rs`：
     - `prepareCallHierarchy`：从函数/方法声明或调用点解析目标可调用声明，返回 `CallHierarchyItem`（含 `name`, `kind`, `detail`, `uri`, `range`, `selectionRange`, `data`）。
     - `incomingCalls`：只读复用活动包图与 `references` 扫描，定位所有调用点并归并到外层调用函数/方法声明，返回按调用方聚合的 `CallHierarchyIncomingCall`（含 `from` 与精确 `fromRanges`），通过统一 `SearchTask` 分批执行并支持 `$/cancelRequest` 与过期取消。
     - `outgoingCalls`：遍历目标函数/方法体内的调用表达式，复用 `Resolver` 解析被调声明（含方法调用、接口方法、泛型实例化、同包/跨包调用），按被调目标聚合返回 `CallHierarchyOutgoingCall`（含 `to` 与 `fromRanges`）。
3. **Type Hierarchy（`textDocument/prepareTypeHierarchy`、`typeHierarchy/supertypes`、`typeHierarchy/subtypes`）**
   - 独占 `src/semantic/hierarchy/type_hierarchy.rs`：
     - `prepareTypeHierarchy`：从类型声明或类型引用定位 Go 命名类型（接口、结构体、定义类型、类型别名），返回 `TypeHierarchyItem`。
     - `supertypes`：对具体类型返回其满足的接口（复用 `implementation` 正反向方法集判定）及结构体嵌入类型；对接口返回其直接嵌入的父接口。
     - `subtypes`：对接口返回实现该接口的具体类型及嵌入该接口的子接口；对具体类型返回直接嵌入该类型的结构体类型。通过统一 `SearchTask` 分批推进，支持取消。
4. **Phase 4 具体类型冷反查性能优化**
   - 针对 [第四阶段计划](2026-11-25-go-implementation.md#已索引的具体类型反查优化) 遗留的跨类型冷反查延迟（例如先预热一个具体类型后首次查询另一无关具体类型仍达 `1772 ms`），优化 `src/semantic/implementation/` 磁盘接口探测与候选包索引：建立跨查询复用的工作区级轻量接口方法名倒排索引（按包增量构建与失效），显著降低首次切换具体类型时的冷反查 P50/P95，同时保证 49 项既有实现对照全绿。
5. **统一基准与脱敏采集工具**
   - 沉淀可复用的 GoLand 对照与计时辅助模块（`tools/bench_common.py`），统一 P50/P95 最近秩计算、Windows 工作集采样与无硬编码个人路径的 `$env:GOLAND_EXE` / CLI 参数约定，供 Track F、B、D 共用。

### 明确不在本阶段范围

- 跨进程持久化磁盘索引缓存（本阶段保持纯内存增量索引，复用现有包图代次与文件指纹失效）。
- 函数字面量/闭包作为独立 Call Hierarchy 顶层节点展开。为便于 LSP 客户端展示，rs-gopls 将闭包内调用归属其外层命名函数或方法。GoLand 的 `GoCallHierarchyTreeStructure.getEnclosingElement` 将函数字面量视为独立的 `GoSignatureOwner`，`GoCalleeHierarchyTreeStructure` 的被调方遍历跳过函数字面量体。[第二阶段计划的 A1 决策](2026-11-27-go-hierarchy-foundation-hardening.md)
- 外部依赖（`GOMODCACHE` / `GOROOT`）全量反向 `incomingCalls` 扫描（默认对齐项目活动包图范围，记录有意差异）。

### 并行协作与写入边界

- **独占路径**：`src/semantic/hierarchy/*`、`tools/workspace_checks/hierarchy.py`、`tools/fixtures/hierarchy/*`、`tools/*hierarchy*`、`docs/goland-hierarchy-baseline.md`。
- **公共文件**：`src/workspace.rs`、`src/workspace_roots.rs`、`src/main.rs`、`src/dispatch.rs`、`src/scheduler.rs`、`src/semantic.rs`、`src/semantic/implementation/*`（F 为 Owner，按 Wave 0 先冻结契约与多工作区基座，避免干扰并行的 A/C/E）。

## 验收标准

### 基座与契约
- [x] **多 Workspace Folder 与动态变更**：初始化支持 0/1/多个 `workspaceFolders`；`workspace/didChangeWorkspaceFolders` 增删独立 Go 模块文件夹后，包图自动刷新，跨文件夹的声明/符号/诊断/层级查询准确反映最新根目录集合，移除目录后不再命中已移除模块。
- [x] **`F → B` 结构化包错误**：`Workspace::package_errors(&self) -> &[PackageLoadError]` 覆盖直接包错误（`Error`）与依赖错误（`DepsErrors`），正确保留测试包身份、`ImportStack`、文件路径与位置，且不退化既有 Go Modules / `go.work` / GOPATH / 测试包行为。

### Call Hierarchy 与 Type Hierarchy
- [x] **Call Hierarchy 完整性**：初始化声明 `callHierarchyProvider`。覆盖函数/方法声明及调用处 `prepareCallHierarchy`、跨文件/跨包/测试包 `incomingCalls` 与 `outgoingCalls`（含多调用点聚合 `fromRanges`、未保存编辑即时更新、空结果、`$/cancelRequest` 返回 `-32800` 及编辑过期返回 `-32801`）。
- [x] **Type Hierarchy 完整性**：初始化声明 `typeHierarchyProvider`。覆盖接口/具体类型/嵌入结构体/嵌入接口/类型别名的 `prepareTypeHierarchy`、`supertypes` 与 `subtypes`，结果去重且顺序稳定，支持未保存编辑、关闭恢复与取消。

### 性能、GoLand 对照与回归
- [x] **Phase 4 冷反查优化验证**：在多包合成夹具与标准库对照下，先预热一个具体类型再首次反查另一具体类型的耗时显著下降，且既有 `implementation-navigation` 29/29 项与工作区实现回归全部通过。
- [x] **GoLand 基线与性能对照**：使用脱敏脚本 `tools/capture_goland_hierarchy.groovy` 在 GoLand `GO-263.7307.354` 上采集 `tools/fixtures/hierarchy/` 基线，归档至 `docs/goland-hierarchy-baseline.md`；`tools/check_goland_hierarchy.py` 与 `tools/compare_hierarchy.py` 按 [导航性能对照](../PLANS.md#导航性能对照) 完成正确性与 P50/P95 性能对照。
- [x] **全量构建与历史回归**：`cargo check --workspace --locked`、`cargo test --workspace --locked` 零告警；`tools/test_lsp_harness.py`、`tools/test_workspace_checks.py` 与 `tools/check_workspace_updates.py`（含新增 `hierarchy` 模块）全部通过，既有导航与符号功能无退化。

## 实现依据

### GoLand 安装包静态核查（`GO-263.7307.354`）

只读检查 GoLand `2026.3.0.1` 安装目录 `<GOLAND_HOME>` 的 `plugins/go-plugin/lib/modules/intellij.go.impl.jar`（SHA-256 `2A13F1A753D5E15DB70C5758E17DB1DC28CC790E1D329E574D8B6A078D636784`），通过 `jbr/bin/java.exe` 与 ASM `Textifier` 核查：

| GoLand 类 / 注册 | 已确认的处理流程 |
| --- | --- |
| `intellij.go.impl.xml` | 注册 `<callHierarchyProvider language="go" implementationClass="com.goide.hierarchy.GoCallHierarchyProvider"/>` 与 `<typeHierarchyProvider language="go" implementationClass="com.goide.hierarchy.GoTypeHierarchyProvider"/>`。 |
| `com.goide.hierarchy.GoCallHierarchyProvider` & `GoCallHierarchyBrowser` & `GoCallHierarchyTreeStructure` | `getTarget` 定位 `GoNamedSignatureOwner`（函数/方法声明）。`GoCallHierarchyTreeStructure#buildChildren` 通过 `search` 查找调用点或被调目标。`getEnclosingElement` 返回最近的 `GoSignatureOwner`，包括函数字面量。`GoCalleeHierarchyTreeStructure` 的被调方遍历跳过函数字面量体。 |
| `com.goide.hierarchy.GoTypeHierarchyProvider` & `GoTypeHierarchyBrowser` | `isInterface` / `isInterfaceType` 区分接口与具体类型；超类型复用 `GoGotoSuperHandler.SUPER_SEARCH`，子类型复用 `GoInheritorsSearch.INHERITORS_SEARCH`。 |
| `com.goide.vgo.GoListOutputParser` & `GoInheritorsSearch` | `go list` 元数据提供包结构与模块归属；接口与实现搜索按方法签名索引先筛选候选，再校验完整方法集满足关系。 |

### Rust 实现方案与取舍

1. **多工作区模型**：将 `EngineState::root: PathBuf` 演进为 `roots: Vec<PathBuf>`，保留主根兼容默认行为；`workspace_roots::list_roots_for_folders(&[PathBuf])` 对每个文件夹分别按 `go.work` / `go.mod` / GOPATH 规则展开并去重；`Workspace::load` 针对不同独立模块根目录分组执行 `go list` 并合并包图与 `PackageLoadError`（同模块内单次 `go list` 不增加额外开销）。
2. **`PackageLoadError` 设计（`F → B`）**：
   - 解析 `go list -e -json=Dir,ImportPath,Name,GoFiles,CgoFiles,Imports,ImportMap,Error,DepsErrors,ForTest,Goroot,Module`。
   - `PackageLoadError` 包含：`package_id: String`、`for_test: String`、`is_dep_error: bool`、`message: String`、`import_stack: Vec<String>`、`path: Option<PathBuf>`、`line: Option<u32>`、`column: Option<u32>`（从 Go `Pos` 如 `path/to/file.go:12:5` 解析），附带原始位置解析单测（覆盖 Windows 盘符 `C:\...` 冒号）。
3. **层级查询任务化**：
   - `CallHierarchyItem.data` / `TypeHierarchyItem.data` 编码轻量定位键（`uri` + `selectionRange` + `kind`），收到 `incomingCalls` / `outgoingCalls` / `supertypes` / `subtypes` 后按最新快照重新解析符号身份，不依赖陈旧指针。
   - `incomingCalls` / `supertypes` / `subtypes` 实现 `SemanticTask`，接入现有 `Scheduler` 分批执行队列，保证不阻塞编辑器主循环且完整支持取消与代次检查。
4. **冷反查优化**：在 `src/semantic/implementation/` 为已加载包图引入惰性工作区级 `InterfaceMethodIndex`（方法名 → 候选接口所在包/文件列表），利用已有磁盘字节预筛快速建表并随 `file_changed` / `install_workspace` 增量失效，消除跨具体类型切换时的全量重扫。

## 实施步骤（Codex 委派规划）

在独立分支上按 Wave 推进（Codex 使用 `modeId: "full-access"`）：

1. **F-WP1 · 多工作区基座与 `F → B` 结构化包错误**（Wave 0，**最先落地，解锁 Track B**）
   - 改动：`src/workspace_roots.rs`、`src/workspace.rs`、`src/semantic.rs`、`src/main.rs`、`src/scheduler.rs`。
   - 产出：多 Workspace Folder + `didChangeWorkspaceFolders` + `Workspace::package_errors()` + Rust 单元测试与工作区用例。
2. **F-WP2 · Call Hierarchy & Type Hierarchy 实现**（Wave 2，与 Track B/D 核心逻辑并行）
   - 改动：`src/semantic/hierarchy/{mod,call,type_hierarchy,tests}.rs` + `src/dispatch.rs` / `src/protocol.rs` 薄接线 + `tools/workspace_checks/hierarchy.py`。
   - 产出：6 个 LSP 层级请求、分批取消任务、完整 Rust 单测与工作区集成用例。
3. **F-WP3 · Phase 4 冷反查优化 & 统一基准工具**（Wave 2–3）
   - 改动：`src/semantic/implementation/*`、`tools/bench_common.py`、`tools/fixtures/hierarchy/*`、`tools/capture_goland_hierarchy.groovy`、`tools/check_goland_hierarchy.py`、`tools/compare_hierarchy.py`、`docs/goland-hierarchy-baseline.md`。
   - 产出：冷反查加速验证、GoLand 层级基线采集与性能对照、全量回归验收。

## 验证计划

沿用环境配置的 Cargo 产物目录与工具链（禁用 `--target-dir` 及覆盖 `CARGO_TARGET_DIR`），所有脚本不含硬编码个人路径：

```powershell
cargo check -p rs-gopls --locked
cargo test -p rs-gopls --locked workspace
cargo test -p rs-gopls --locked semantic::hierarchy
cargo test -p rs-gopls --locked semantic::implementation
cargo check --workspace --locked
cargo test --workspace --locked
cargo build -p rs-gopls --release --locked

$target = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
$server = Join-Path $target 'release\rs-gopls.exe'
python -B .\tools\check_workspace_updates.py $server --module hierarchy
python -B .\tools\check_workspace_updates.py $server
python -B .\tools\check_goland_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
python -B .\tools\compare_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
```

## 完成记录

- 2026-11-27 完成 GoLand `GO-263.7307.354` 层级与基座相关类静态核查，确定与 Track B / Track D 的 `F → B → D` 协同架构，创建待确认计划。
- 2026-11-27 验证 Wave 0 层级夹具与采集脚本。夹具通过 `go test ./...`，GoLand `GO-263.7307.354` 的 20 个查询各完成 30 次重复计时，结果一致。采集使用 GoLand 的 All 搜索范围，输出夹具内的相对路径。GoLand 略去递归自调用边，`Base` 的子类型结果为空。[层级夹具](../../tools/fixtures/hierarchy/) · [采集脚本](../../tools/capture_goland_hierarchy.groovy)
- [x] 2026-11-27 验证 Wave 3 统一基准工具。GoLand `GO-263.7307.354` 和 Go `1.26.3` 的层级对照通过 20 项严格校验，其中 10 项可直接对照首查与热查耗时。rs-gopls 完成 600 次插入注释后的查询及回滚校验。三条线的 `--capture-goland` 均通过现场采集和工作集采样验证。共用辅助模块与 LSP harness 的 22 项单测通过。[共用辅助模块](../../tools/bench_common.py) · [层级校验](../../tools/check_goland_hierarchy.py) · [层级基准](../../tools/compare_hierarchy.py)
- 2026-11-27 完成 F-WP1（`9ec6891`，多 Workspace Folder + `Workspace::package_errors`）、F-WP2（`4cdf87b`，Call/Type Hierarchy）、F-WP3（`1fc91d2`，具体类型冷反查跨类型切换 P50 降 88.7% / P95 降 90.8%，99 包合成切换 59→24 ms，不相交方法集新增探测 0 次）与 Wave 3 统一 LSP 接线（`5c2dd50`）。最终发布版 SHA-256 `0A0532A807D96E98D0301E527CA2E082F6D08A356B43AC58A64690B98768C362`，通过 `cargo check/test --workspace --locked`（120/120，零告警）、32/32 Python 单测、29/29 工作区回归、20/20 层级严格对照及既有历史回归。归档基线文档 [GoLand 层级基线](../goland-hierarchy-baseline.md)，状态更新为「已完成」。

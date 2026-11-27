# 大文件响应性与多包引用冷查性能优化（P0–P3）

| 项目 | 内容 |
| --- | --- |
| 状态 | 已完成 |
| 创建日期 | 2026-11-27 |
| 用户确认 | 已确认（2026-11-27 用户确认按计划实施 P0–P3） |
| 更新日期 | 2026-11-27 |
| 完成日期 | 2026-11-27 |

## 目标与范围

脱敏遥测覆盖 536–568 个包、2,107–2,131 个项目文件，其中 `D06` 是约 20 万行的生成 Go 文件。用同量级多包工作区和 200,002 行合成文件复现冷查，消除主循环持续数秒乃至逾一分钟的冻结，以及冷引用查询逐文件完整解析的瓶颈。大文件和多包负载下，`textDocument/definition` 等交互导航应能持续响应。冷重任务的计算移出主循环或按预算让出主循环，完整保留 Go 绑定语义与 UTF-16 范围。

### P0 · 诊断去二次复杂度、按实际包视图复用缓存与移出主循环

- 把 struct 分类随声明提取写入 `TypeDetails` 或类型索引，消除 `types::shape` 线性扫描声明文件 `outline().symbols` 带来的 `O(类型检查数 × D)`。每个源码修订只提取一次导入和转义路径绑定，移除 `unresolved::check` 反复用 `imports::specs` 收集整根子声明的 `O(未解析数 × D)`。其中 `D` 为相关文件的声明规模。
- 在一次或可共享的 cursor 遍历中携带父链、词法作用域和函数返回槽上下文，避开宽平坦子树上反复从根定位的 `Node::parent()`。区分零参数 getter 和参数尚未加载的签名，按需生成补全展示文本，复用同修订源码、源码指纹和 `Resolver` 查找缓存。分析中保留字节范围，输出时统一转换为 UTF-16。
- 以实际选中的包视图和语义输入标识诊断报告。键覆盖选中文件、打开缓冲区修订、传递依赖、导入解析、构建环境及组错误，保留部分报告与完整报告的区分。`include_tests` 由 `false` 升级为 `true` 时，实际输入相同的生产包沿用报告和 `resultId`，持旧 `previousResultId` 的请求继续收到 `unchanged`。真正变化的包视图按影响范围失效，包含受影响的测试变体。
- 用有界后台 worker 处理冷语义诊断及可能超出预算的大文件准备工作。主循环提交不可变、可跨线程传递的 `Send` 快照，worker 使用私有 `Parser`、索引和查找缓存，在解析、AST 遍历、作用域检查和类型检查的可中断边界检查取消。相同版本键合并 pull、push 和 QuickFix。主循环收到完成消息后复核文档与有效包视图版本，再缓存、发布或回包。`workspace_install` 的挂起诊断重放进入同一队列。取消单请求只移除其订阅，最后一个订阅取消或任务版本过期时停止计算并释放任务槽。

### P1 · 大文件 `documentHighlight` 提速与去阻塞

- 将 `Editor` 初始化时的 `raw_imports` 和重名表按源码修订缓存，移除每次请求的两次全根扫描和 `descendants`、`next_named_sibling`、`parent` 驱动的整树遍历。局部变量、常量、参数、接收者、类型参数、控制结构初始化、`select` 接收、`type switch` alias 和闭包捕获从声明确定可见范围，只遍历有关词法子树或按字节范围切分已有偏移。20 万行文件的小作用域热路径目标为 `< 1 ms`。
- 文件级、包级和成员符号复用 `Source::name_offsets`，跳到同名偏移核验绑定。补足偏移表覆盖或沿用对应专用分支处理标签、`true`、`false`、`nil`、`iota`、隐式导入路径、控制流和返回槽，保留读写分类。冷建表与超大候选集可取消，按预算分片或移入后台。结果按 `(文档修订, 规范光标目标)` 缓存，同一绑定内位置复用结果，编辑后重新核验。以并发定义查询排队时长验证主循环收益。

### P2 · `references` 冷查字节与标识符预筛

- 在 `reference_source`、`FileIndex` 构建和标识符偏移索引之前，依次从 overlay、缓存原文、一次磁盘读取获取当前修订字节。目标字节拼写未出现时跳过该文件的引用扫描，无 AST 解析和偏移表构建。只在能证明等价时使用标识符边界排除。Unicode、语法恢复、注释和字符串等需要进一步判定的命中交给现有 AST 绑定核验。
- 命中文件直接核验候选偏移，按需复用已有偏移表，冷路径省去构建全名表。仅对能证明必须出现目标拼写的查询分支应用缺字节淘汰，隐式名称等分支沿用已有语义处理。文件内搜索状态保存字节游标、跨片拼写边界和待核验偏移。沿用现有的 `2 ms` 调度预算，将检查点推进文件内。可能一次超预算的读盘、解析或索引工作交给可取消的后台任务。保留 `includeDeclaration`、顺序、去重和完整 UTF-16 落点。

### P3 · 包图安装与 WP2 `build_postings` 让出主循环

- 将测试视图升级时逐路径 `metadata()` 检查移到后台加载线程。同次加载归并重叠的路径观察，每个路径校验一次。主循环安装阶段只核对加载版本和文件观察记录，遇到交错的编辑、watcher 更新或新一代包图时按当前版本丢弃或重新安排过时结果。
- WP2 `TypePostings` 最终构建改为带游标的增量累加器，或由后台构建并按版本原子发布。主循环单步设计预算为 `<= 5 ms`，发布验收上限为 `<= 10 ms`，保留首次层级查询的总耗时和目标正确性。
- 仅在开启 `RS_GOPLS_LOG_FILE` 时记录轻量诊断分阶段、高亮、候选文件和主循环分片指标，便于核验延迟和缓存命中。报告只保留脱敏样本 ID、计数和耗时。

## 验收标准

- [x] **P0 大文件诊断速度与非阻塞**。200,002 行合成文件含 5,000 组 struct/const/var/getter，冷诊断基线为 `5,938 ms`，候选目标为 `< 1,000 ms`。比较约 5 万、10 万、20 万行的阶段耗时和遍历计数，证实重复全文件扫描已消除，主要分析阶段随规模近线性增长。冷诊断运行期间并发发送 `textDocument/definition`，排队延迟 `< 20 ms`，每次返回完整且正确的落点。取消最后一个订阅后在下一个协作检查点退出，验证仅回复一次、无过期发布或缓存写入、任务槽及持有的快照可以回收。
- [x] **P0 测试视图升级缓存复用**。包图从 `include_tests=false` 升级至 `true` 时，核对实际语义输入未变的生产包文档报告及 `resultId` 不变，持原 `previousResultId` 的 pull 返回 `unchanged`，且无重复冷分析。分别修改兄弟文件、依赖 overlay、参与对应视图的测试文件和构建环境，验证受影响报告失效并重算，输入不变的生产包报告继续命中。覆盖逐项导入解析、组错误及部分报告升级。
- [x] **P1 大文件高亮与零阻塞跳转**。200,002 行文件的局部符号高亮从 `1,338–1,520 ms` 降至 `< 5 ms`，热查 `< 1 ms`。包级常量从 `1,969–3,798 ms` 降至 `< 50 ms`，重复同坐标 `< 2 ms`。高亮期间并发 `definition` 的排队延迟 `< 10 ms`。既有高亮语义单测与 GoLand 高亮基线全部通过，逐项核对闭包捕获、标签、预定义常量、隐式导入、控制流、返回槽和读写分类。
- [x] **P2 冷引用预筛与零漏报**。在含大量无目标拼写文件、大文件、Unicode 标识符、注释、字符串和语法恢复节点的多包夹具上，冷查对无目标拼写文件的引用扫描产生 `0` 次 AST 解析和 `0` 次偏移表构建。记录候选文件、实际解析文件、预筛跳过文件数及 P50/P95，确认解析文件数和冷查耗时较前序构建下降，毫秒级热查无退化。逐条核对新旧实现完整目标、去重顺序和 UTF-16 范围，两种 `includeDeclaration` 设置均 100% 一致。大文件混合负载下逐步守住主循环预算。
- [x] **P3 消除主循环长帧**。测试视图升级的安装阶段从 `166.07 ms`（含 `145.71 ms` metadata）降至 `< 25 ms`。WP2 `build_postings` 各步主循环占用 `<= 10 ms`，基线为 `46–62 ms`，另记 `<= 5 ms` 设计预算的达成情况。`reader-subtypes` 的首次查询性能、完整目标和 49 项实现跳转对照无退化。
- [x] **全量构建、回归与 GoLand 对照**。`cargo check --workspace --locked` 和 `cargo test --workspace --locked` 零告警通过。Python 全部单测、`check_workspace_updates.py` 全量模块、层级与实现对照以及重命名回放全通过。用运行中的 GoLand 核对大文件导航和高亮的完整结果、范围及适用的读写种类，记录可比用例的计时、已知差异和各自的观测条件。[导航性能对照](../PLANS.md#导航性能对照)

## 实现依据

### GoLand 安装包字节码

安装包 `<GOLAND_HOME>/build.txt` 确认为 GoLand 2026.3.0.1（`GO-263.7307.354`）。按 REA 的只读流程定位 JAR 条目并核对 JVM 指令。复核时把下表中对应 JAR 和 `<GOLAND_HOME>/lib/intellij.libraries.asm.jar` 加入类路径，用 `<GOLAND_HOME>/jbr/bin/java.exe` 运行 `org.jetbrains.org.objectweb.asm.util.Textifier`，参数传入目标类全名，内部类使用 JVM 的 `$` 拼写。Go 插件主 JAR 的 SHA-256 为 `2A13F1A753D5E15DB70C5758E17DB1DC28CC790E1D329E574D8B6A078D636784`。

| 范围 | 安装包中的文件 |
| --- | --- |
| Go 插件 | `<GOLAND_HOME>/plugins/go-plugin/lib/modules/intellij.go.impl.jar` |
| 高亮、后台 pass 和导航入口 | `<GOLAND_HOME>/lib/intellij.platform.lang.impl.jar`、`<GOLAND_HOME>/lib/intellij.platform.ide.jar` |
| 注解与检查访问器 | `<GOLAND_HOME>/lib/intellij.platform.analysis.impl.jar` |
| 文本扫描、引用索引与 PSI 接口 | `<GOLAND_HOME>/lib/intellij.platform.indexing.impl.jar`、`<GOLAND_HOME>/lib/intellij.platform.core.jar`、`<GOLAND_HOME>/lib/intellij.platform.core.impl.jar` |

- **高亮入口与作用域**。`com.goide.highlighting.exitpoint.GoHighlightExitPointsHandlerFactory.createHighlightUsagesHandler` 将函数出口（含 `panic`）、`break`/`continue` 和返回槽交给专用处理器。`com.goide.comments.GoCommentHighlightUsagesHandlerFactory.createHighlightUsagesHandler` 命中 `PsiComment` 时创建禁用高亮的处理器。`com.goide.psi.impl.GoNamedElementImpl.getUseScope` 将导入限定在文件，局部变量、常量和标签限定在最近的 `GoBlock`。参数与接收者取 `GoSignatureOwner` 的函数块或所有者，类型参数取签名所有者。`com.intellij.codeInsight.daemon.impl.IdentifierHighlightingComputer.computeRanges` 要求后台读访问，先尝试专用处理器，收集读写范围并按处理器定义补查引用。
- **高亮候选与取消**。`com.intellij.psi.impl.search.LowLevelSearchUtil.getTextOccurrencesInScope` 从文件文本和 PSI 作用域范围取边界，`getTextOccurrences` 缓存范围内的扫描结果，调用 `StringSearcher.scan`、标识符边界检查及循环内的 `ProgressManager.checkCanceled`。`com.intellij.codeInsight.daemon.impl.IdentifierHighlightingAccessorImpl.getMarkupData` 在后台协程的 `readAction` 中复核 PSI 与编辑器有效性，经 `blockingContextToIndicator` 调用 `computeRanges`，将协作取消传入读取路径。Rust P1 采用词法作用域、候选偏移和修订缓存，实际语义由现有高亮回放和新增边界用例验证。[GoLand 编辑器基线](../goland-editor-baseline.md)
- **后台诊断与交互导航**。`com.intellij.codeInsight.daemon.impl.DaemonCodeAnalyzerImpl.submitInBackground` 在后台读操作中创建分析 pass，再调用 `PassExecutorService.submitPasses`。`PassExecutorService.ScheduledPass.doRun` 检查 `DaemonProgressIndicator`，在进度上下文中执行，随后应用结果。`AnnotatorRunner.runAnnotatorsAsync` 经 `JobLauncher.processConcurrentlyAsync` 分派注解器，`runAnnotator` 和 `InspectionVisitorOptimizer.acceptElements` 在元素访问间检查取消。`com.goide.highlighting.GoAnnotator.annotate` 核对 Go PSI、模块信息和 `shouldProcessFile` 后运行检查访问器。`com.intellij.codeInsight.navigation.actions.GotoDeclarationOrUsageHandler2.invoke` 经 `ActionUtil.underModalProgress` 进入 `com.intellij.openapi.progress.ProgressManager.computePrioritized`，由 `CoreProgressManager` 为目标解析设置并恢复当前线程的优先状态。Rust P0 将诊断分派至独立 worker，让主循环持续处理导航，并通过版本复核约束结果发布。
- **生成文件与大小条件**。`com.goide.project.GoGeneratedSourcesFilter.isGeneratedSource` 在文件有效、类型合适且允许访问 PSI 时查询 `GoFilePropertiesGist` 的生成属性。`com.goide.psi.GoFileSizeLimit.getLimits` 使用 `GoConstants.GO_FILE_INTELLISENSE_LIMIT`，该值取平台默认上限与 15 MiB 中的较大值。现场对照记录夹具字节数、生成标记和 IDE 限制是否生效，使用双方均返回完整结果的样本比较延迟。
- **引用候选文件预筛**。`com.goide.psi.GoReferencesSearch.createSearchParameters` 选用 `com.goide.usages.GoNoCommentsOptimizer`，清除注释搜索上下文。平台 `com.intellij.psi.impl.search.CachesBasedRefSearcher.processQuery` 用目标名和有效范围提交 `searchWord`。`PsiSearchHelperImpl.TextIndexQuery.toFileBasedIndexQueries` 通过 `com.intellij.psi.impl.cache.impl.id.IdIndex.NAME` 生成 `FileBasedIndex.AllKeysQuery`。外层 `computeQueries` 先用 `processFilesContainingAllKeys` 找候选文件，再经 `com.intellij.psi.impl.PsiManagerEx.findFile` 取得 PSI 并核验。Rust P2 复用同修订文本和精确 AST 绑定，通过保守预筛减少需要解析的候选文件。

字节码确认的是执行分支及调用关系。GoLand 在生成标记、大小限制和具体查询下的响应及耗时，按现场对照采集。Rust 去二次复杂度、缓存等价、预筛完备性和调度上限由下列回归与测量验证。

### Rust 源码锚点

| 工作包 | 当前路径与拟验证的改变 |
| --- | --- |
| P0 类型和导入 | `src/symbols.rs:152` 的 `TypeDetails`、`src/semantic/diagnostics/types.rs:34` 的 `shape`、`src/semantic/diagnostics/unresolved.rs:101` 与 `src/semantic/diagnostics/imports.rs:16` 的重复导入扫描。 `Cargo.lock` 固定 Tree-sitter 0.25.10，其 `src/node.c` 的 `ts_node_parent` 从 `ts_tree_root_node` 重定位。验证宽平坦子树、别名与匿名 struct、零参数和未知签名、转义导入及错误恢复。 |
| P0 报告与重放 | `src/semantic.rs:489` 附近的安装尾部无条件清空诊断缓存。`src/semantic/diagnostics/cache.rs:44` 和 `src/semantic/diagnostics/model.rs:24` 的失效与快照匹配使用包图代数、全局测试视图和文本指纹。入口为 `src/dispatch.rs:703`、`src/main.rs:98`、`src/semantic/actions/code_action.rs:39`，安装及请求重放在 `src/scheduler.rs`。验证三入口合并、版本错序与取消。 |
| P1 文档高亮 | `src/semantic/editor/mod.rs:81` 构造 `Editor`。`src/semantic/editor/document_highlight.rs:44` 遍历候选，`src/semantic/source.rs:268` 提供按名偏移。验证同名遮蔽、控制结构及闭包的可见范围和全部专用分支。 |
| P2 引用冷查 | `src/semantic/references.rs:108` 在完成整文件之后才检查分批预算，`src/semantic/references.rs:258` 先取引用源再查偏移。`src/semantic/index.rs:423` 提供缓存原文，`src/semantic/index.rs:468` 进入 `reference_source`。验证负文件只经过预筛，正文件逐候选核验；单列绑定解析可能触发的依赖加载。 |
| P3 安装与索引 | `src/semantic.rs:454` 开始安装前的元数据校验，`src/semantic.rs:466` 直接调用 `metadata()`，`src/semantic.rs:479` 核对导航原文观察。`src/semantic/implementation/candidates.rs:123` 进入同步 `build_postings`。验证重复观察合并、失效和发布的一致性，以及 WP2 查询的首查耗时。 |

### 已有脱敏基线

| 来源 | 查询或阶段 | 已有观测 |
| --- | --- | --- |
| 200,002 行合成 Go 文件，5,000 组声明与 getter | 冷诊断 | `5,938 ms` |
| 同一合成文件 | 局部符号高亮 | `1,338–1,520 ms` |
| 同一合成文件 | 包级常量高亮 | `1,969–3,798 ms` |
| `D06` 工作区脱敏遥测 | 启动诊断、引用触发测试视图升级后重放诊断 | 启动主循环冻结 `10–15 s`，引用期间主循环冻结 `63.5 s` |
| `D06` 工作区脱敏遥测 | 高亮与定义并发 | `definition` 排队约 `3.9 s` |
| `D06` 工作区脱敏遥测 | 引用逐文件处理 | 单文件主循环占用约 `295 ms` |
| `D06` 工作区脱敏遥测 | 测试视图升级安装 | `166.07 ms`，其中元数据校验 `145.71 ms` |
| `D06` 工作区脱敏遥测 | WP2 `build_postings` | 主循环连续占用 `46–62 ms` |

表中数值来自本计划输入的脱敏遥测和合成基线。执行时先用固定发布版复测合成工作区，补齐指标口径、样本数和冷缓存状态，再与候选版对比。

## 实施步骤

1. **P0**。先用同一生成器和调度探针固定前序发布版的基线，再完成类型分类、导入绑定和 cursor 遍历改造，按三档文件验证阶段规模。缓存键与 worker 的设计可同时准备，先落实有效包视图和修订等价规则，再接入 pull、push、QuickFix 及包图安装后的重放。波次出口为冷诊断、缓存等价、取消与并发定义验收。
2. **P1**。以 P0 的源码修订、作用域及队列约定为基础，实现高亮局部快路、共享偏移和有界慢路。高亮专用语义用例、GoLand 采集与 P0 可并行准备。接入后先核对范围、读写分类以及编辑失效，再跑冷、热和并发基准。
3. **P2**。先固定原实现的引用结果基线，再将预筛接入现有文本层并切分文件内步进。预筛与原结果集比对可同 P1 并行开发，涉及共享 `Source`、`IndexReader` 和调度设施的变更在 P1 接口稳定后集成。验证负文件零解析、Unicode 与恢复、取消及引用期间导航。
4. **P3**。以 P0 生效的包视图和失效规则为准，移交安装阶段的元数据工作并推进 WP2 分片或后台发布。可提前准备层级与实现回放，待 P2 的共享调度路径验证后合并主循环变更。最终测量安装和构建最长步进，跑全量回归及 GoLand 现场对照，并登记实际结果。

## 验证计划

### 构建与回归命令

沿用项目配置的 toolchain、profile、features、target 和 Cargo 产物目录。先检查受影响的包，按波次执行专项测试，最后运行全量检查与发布版构建。禁止传入 `--target-dir`、覆盖 `CARGO_TARGET_DIR` 或 `CARGO_BUILD_BUILD_DIR`，以及新建编译目录重试。

```powershell
cargo check -p rs-gopls --locked
cargo test -p rs-gopls --locked semantic::diagnostics
cargo test -p rs-gopls --locked semantic::editor
cargo test -p rs-gopls --locked semantic::references
cargo test -p rs-gopls --locked semantic::implementation
cargo test -p rs-gopls --locked semantic::hierarchy
cargo test -p rs-gopls --locked scheduler
cargo test -p rs-gopls --locked workspace
cargo check --workspace --locked
cargo test --workspace --locked
cargo build -p rs-gopls --release --locked
cargo fmt --all -- --check
git diff --check
```

按前序实现跳转验收配置 `RS_GOPLS_REAL_PROJECT` 为固定修订的工作区，`RS_GOPLS_IMPLEMENTATION_BASELINE` 为对应 20 项归档语料的位置。另运行仓内 29 项实现补充语料。所有检查使用同一发布版，逐条核对退出状态和结果。[前序验收语料](2026-11-27-go-hierarchy-foundation-hardening.md#验证计划)

```powershell
$cargoTarget = (cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json).target_directory
$server = Join-Path $cargoTarget 'release/rs-gopls.exe'
$hierarchy = 'tools/fixtures/hierarchy'
$hierarchyFixture = Join-Path $hierarchy 'goland-hierarchy.json'
$implementation = 'tools/fixtures/implementation-navigation'
$implementationFixture = Join-Path $implementation 'goland-implementations.json'

python -B -m unittest discover -s tools -p 'test_*.py'
python -B tools/check_workspace_updates.py $server
python -B tools/check_goland_editor.py $server --capability highlight
python -B tools/check_goland_definitions.py $server --workspace $env:RS_GOPLS_REAL_PROJECT
python -B tools/check_goland_references.py $server --workspace $env:RS_GOPLS_REAL_PROJECT
python -B tools/check_goland_hierarchy.py $server --workspace $hierarchy --fixture $hierarchyFixture
python -B tools/compare_hierarchy.py $server --workspace $hierarchy --fixture $hierarchyFixture
python -B tools/check_goland_implementations.py $server --workspace $env:RS_GOPLS_REAL_PROJECT --fixture $env:RS_GOPLS_IMPLEMENTATION_BASELINE
python -B tools/check_goland_implementations.py $server --workspace $implementation --fixture $implementationFixture
python -B tools/fixtures/rename/replay.py lsp $server
```

### 200k 行基准脚本方案

1. 实现阶段编写基准入口并复用 `tools/lsp_harness.py` 的 `send_batch` 和按请求 ID 收集响应能力。用每组 40 行和 2 行公共前缀的固定生成规则，在临时工作区生成 1,250、2,500、5,000 组等形声明，得到 50,002、100,002、200,002 行三档。断言最大档行数，固定换行、种子、字节数、内容摘要和查询锚点。另生成 536–568 包、2,107–2,131 项目文件量级的工作区，区分包含目标的文件、同名异绑或文本伪命中、完全无目标拼写文件及 overlay、Unicode、残缺语法和非 BMP 范围样本。保留正常生成标记的对照样本，记录 IDE 对它的处理。
2. 冷样本用独立进程，先以就绪探针固定包图状态，并检查被测功能缓存确实未命中。另测从初始化到首次正确结果和从生产视图升级到测试视图的过程。热样本在同一会话预热 5 次后重复查询，编辑档按固定序列发送 overlay、查询、恢复并重查。每档记录 30 个样本，输出 P50/P95、最大值和样本数。诊断与高亮请求耗时目标按 P95 校验，并发定义排队、包图安装和主循环单步上限按最大值校验。
3. 把冷诊断、高亮、引用和 `definition` 批量发送到同一 stdio 会话，并以开始事件确认重任务已运行后继续发出定义查询。每个定义请求核对完整落点。排队延迟按传输接收至主循环开始处理的单调时钟差计量，另记端到端往返、包图等待和单步主循环占用。针对正在运行、待调度和有多个订阅的任务发出取消，检查回包次数、任务回收、后续请求恢复和旧版本结果丢弃。
4. 用 `RS_GOPLS_LOG_FILE` 采集各诊断阶段和高亮候选阶段的计数、耗时及命中情况，记录诊断报告 `resultId`、引用预筛跳过数、解析数、偏移表构建数、读取次数、安装和 postings 步进最大值。固定两版日志开关，另测关闭日志时的端到端延迟。将阶段采样与目标完整性断言合入同一报告，以匿名样本 ID 输出机器可读数据。冷引用对照将候选文件自身扫描与绑定核验触发的依赖加载分别计数。
5. 在同一主机、同一 SDK、同一夹具修订、搜索范围、打开文件和编辑序列上运行 GoLand 现场采集与 rs-gopls 发布版，记录双方构建号或摘要、索引就绪条件和缓存状态。GoLand 在 Smart Mode 下采集标识符高亮、声明与查找用法，按共用锚点核对完整目标集合、UTF-16 范围及适用的读写类型。各自记录计时起止点、是否包含界面操作以及索引就绪和启动到首次正确结果。只对目标、范围和搜索范围一致的样本比较耗时，逐条记录其他产品差异及生成文件限制是否生效。[导航性能对照](../PLANS.md#导航性能对照)
6. 复跑 WP2 的 `reader-subtypes`，每个进程在图就绪后以该查询作为首个子类型查询。按前序计时边界测量准备加展开，分别记录构建、发布等待和查询耗时，比较 P50/P95 与完整目标。另核对 49 项实现跳转与已定层级目标和范围。[前序层级性能基线](../goland-hierarchy-baseline.md)

## 完成记录

- 2026-11-27 创建计划，完成模板、Rust 源码锚点及 GoLand 安装包依据核对。等待用户确认。实现、基准复测和发布验收结果待记录。
- 2026-11-27 用户确认后启动 Wave 1，实施 P0 诊断热点优化、按实际包视图缓存及后台调度。P0 验收结果在本节登记。
- Wave 1 缓存回归通过。测试视图升级后，生产包持原 `previousResultId` 在主循环直接收到 `unchanged`，此时测试包的冷分析仍在后台。兄弟文件、依赖 overlay、测试文件、导入目标、构建环境和组错误按实际输入失效。部分报告独立缓存，完整报告就绪后发布新结果。调度测试覆盖 pull、push、QuickFix 合并、逐订阅取消、过期结果丢弃和任务槽回收。

### Wave 1 发布版验收（2026-11-27）

- Windows amd64，Go `1.24.3`。旧发布版 SHA-256 为 `6316b8825037fdb4be1abe3124face8928d4334cd6421f06b0d4928af2f1ee0d`，候选发布版为 `5aa6830c1ed89e15002a30aa08a109d6b98bab4f63f2e7bc327f23abb1f679ac`。旧版按原脚本顺序单次复测诊断处理阶段，三档分别为 1,136.15、1,427.74、3,155.57 ms。历史 20 万行样本为 5,938.11 ms。
- 原生成器使用 UTF-8 与 LF，每组 40 行。三档文件分别为 733,637、1,476,137、2,961,137 字节。最大档 SHA-256 为 `4db986d844a499a4d3d7940c70123b37b6f0d1ff20cd9d4afad47c55ee4a7b40`。
- 候选版每次使用独立 LSP 进程，打开编辑器缓冲区，等待生产包图就绪，从首个诊断请求计时。请求开始时诊断报告和文件声明索引均为冷态。日志开启组在确认 worker 开始后发出定义查询，核对唯一目标、URI 与完整 UTF-16 范围，按 `dispatch_queue_ms` 计量传输接收到主循环开始的时间。并发请求可复用同一修订发布的不可变索引。日志关闭组单独量取冷诊断往返。
- Rust 专项依次通过诊断 33、操作 43、调度 22、工作区 28 项测试。包和全工作区的锁定检查、全工作区 322 项测试及发布构建均通过，Cargo 零告警。

下表为开启日志且并发定义的结果，单位为 ms。每档 30 个独立样本，90 个定义结果与预期完全一致。冷诊断均返回空的完整报告，持原 `resultId` 复查均返回 `unchanged`。

| Go 文件行数 | 冷诊断往返 P50 / P95 / 最大 | worker P50 / P95 | 定义排队 P95 / 最大 | 语义检查节点数 |
| ---: | ---: | ---: | ---: | ---: |
| 50,002 | 172.636 / 187.362 / 190.170 | 163.545 / 179.559 | 0.048 / 0.053 | 166,253 |
| 100,002 | 342.597 / 368.910 / 379.545 | 326.243 / 353.185 | 0.035 / 0.069 | 332,503 |
| 200,002 | 694.322 / 743.721 / 750.437 | 660.649 / 710.935 | 0.060 / 0.162 | 665,003 |

声明收集 P50 为 83.298、171.227、335.749 ms。类型检查 P50 为 20.420、41.319、85.565 ms。未解析检查 P50 为 9.724、19.827、40.462 ms。最大档热诊断主循环执行 P95 为 0.028 ms。

关闭日志后，每档独立测量 30 次，诊断 worker 自行构建冷文件索引。50,002 行往返 P50 / P95 / 最大为 231.624 / 251.010 / 266.745 ms，100,002 行为 456.146 / 494.418 / 505.797 ms，200,002 行为 942.990 / 992.727 / 995.837 ms。并发组可复用定义请求在同一修订上提交的索引。

同机使用 GoLand `GO-263.7307.354`、Go `1.24.3` 和 200,002 行夹具。GoLand 在 Smart Mode 下用运行中 IDE 的声明导航计算链采集。双方各核对 38 次，覆盖首查、5 次预热、30 次热查、未保存的首行注释插入与回退。基准定义请求位于 `generated.go` 的 `199998:11`，落点为同文件 `[199991:6, 199991:17)`，均为零基 UTF-16 范围。编辑时按插入行数顺移，回退后恢复原范围。

| 定义查询 | GoLand 目标计算（ms） | rs-gopls 服务端处理（ms） |
| --- | ---: | ---: |
| 索引就绪后首查 | 2,653.625 | 198.501 |
| 热查 P50 / P95 | 11.927 / 14.442 | 0.024 / 0.040 |
| 插入未保存注释后首查 | 2,252.958 | 196.838 |
| 回退编辑后首查 | 2,125.181 | 203.669 |

GoLand 计时覆盖声明解析与落点提取。rs-gopls 服务端采用 `rpc_done.exec_ms`，LSP 首查往返为 232.252 ms，热查往返 P50 / P95 为 0.168 / 0.230 ms。

- 最终发布版的 `check_workspace_updates.py` 四个模块全部通过，共 13 个场景。Python 单测通过 42 项，包含传输接收至派发的排队遥测校验。Rust 格式与 Git 差异检查通过。
- 2026-11-27 完成 Wave 1。P0 两项验收通过，临时夹具、日志和基准脚本均已清理。

### Wave 2 发布版验收（2026-11-27）

- Windows amd64，Go `1.24.3`。发布版 SHA-256 为 `04ec824e72aa98c98b6d1302708c8e0ef533ccbdaa65f2bc1119299eb053f4bf`。用 Wave 1 原生成器重建夹具，核对 UTF-8、LF、200,002 行、2,961,137 字节及 SHA-256 `4db986d844a499a4d3d7940c70123b37b6f0d1ff20cd9d4afad47c55ee4a7b40`。
- 导入、文件声明和重名表按源码修订复用。局部候选限定在词法子树中。文件、包和成员候选复用已有的按名偏移表，冷查按目标拼写分片收集偏移后核验绑定。打开文档按修订和规范目标缓存结果，返回槽上下文纳入目标键。冷查询和大结果序列化在高亮 worker 执行，未打开的文档由 worker 按高亮请求读取磁盘版本。
- 每个冷查样本使用独立 LSP 进程，打开文档并等待生产包图及实现索引就绪。冷查后预热 5 次，再测同坐标热查。各项每种日志配置各 30 样本，校验局部符号 4 个和包级常量 5 个有序 UTF-16 范围及 `Read`、`Write`、`Text` 分类。开启日志的样本同时核对同坐标和同一词元内另一坐标的缓存命中。

下表为 LSP 往返耗时，单位为 ms。

| 请求 | 关闭日志 P50 / P95 / 最大 | 开启日志 P50 / P95 / 最大 |
| --- | ---: | ---: |
| 局部符号冷查 | 0.697 / 0.960 / 0.998 | 0.741 / 0.943 / 1.100 |
| 局部符号热查 | 0.095 / 0.177 / 0.178 | 0.131 / 0.232 / 0.327 |
| 包级常量冷查 | 1.218 / 1.701 / 1.783 | 1.229 / 1.805 / 1.806 |
| 包级常量热查 | 0.109 / 0.194 / 0.206 | 0.135 / 0.201 / 0.247 |

- `Field0` 压测有 15,000 个同名候选。30 次高亮均准确返回 3 个范围，往返 P50 / P95 / 最大为 `470.978 / 489.872 / 492.338 ms`。按 worker 开始及完成事件确认 30 次定义查询均与高亮重叠，全部落在 `generated.go` 的 `[199991:6, 199991:17)`。传输接收到开始派发的延迟 P50 / P95 / 最大为 `0.024 / 0.046 / 0.050 ms`，定义自身往返为 `202.210 / 215.560 / 228.973 ms`。
- 未保存编辑和回退的 10 次查询均返回预期结果并核对修订缓存的冷、热切换。运行中取消只收到一次 `-32800`，重试完整返回，后续请求命中缓存。调度单测另覆盖排队取消和过期修订结果丢弃。

同机运行 GoLand `GO-263.7307.354`，使用相同 Go SDK 和夹具，在 Smart Mode 下通过 `IdentifierHighlightingComputer.computeRanges` 采集计算耗时。计时覆盖高亮计算与范围提取，LSP 表格包含协议往返。下表为每个锚点的首查和 5 次预热后的 30 次热查。按回放的高亮种类映射核对完整 UTF-16 范围，局部、常量、成员分别匹配 4、5、3 个范围。

| GoLand 查询 | 首查（ms） | 热查 P50 / P95 / 最大（ms） |
| --- | ---: | ---: |
| 局部符号 | 452.440 | 52.929 / 85.785 / 97.905 |
| 包级常量 | 184.193 | 60.469 / 178.027 / 3,215.577 |
| `Field0` 成员 | 3,564.220 | 165.182 / 184.937 / 186.112 |

- 现场编辑对照覆盖局部符号锚点，插入未保存注释及回退后均核对范围和分类，GoLand 计算耗时分别为 `1,808.259` 和 `523.518 ms`。包级常量和成员采用原始夹具的对照样本。
- 锁定的包检查、editor 专项 21 项、工作区 328 项及发布版构建均通过，Cargo 零告警。GoLand 高亮回放 69 项通过，匹配 301 个范围并保留基线记录的 4 项单文件语义边界。工作区 editor 模块 4 项通过。Rust 格式与 Git 差异检查通过。
- 已清理本波次的临时夹具、基准脚本、日志、GoLand 临时数据和二进制副本。

### Wave 3 发布版验收（2026-11-27）

- 文件查找按 overlay、同修订缓存原文、磁盘读取顺序取字节。预筛以 32 KiB 为扫描窗口，保留跨窗拼写和待核验偏移。已有全名偏移表直接复用，冷查仅核验命中偏移。ASCII 边界由 AST 确认，Unicode、数字邻接、错误恢复、注释和字符串交由精确 AST 节点与绑定核验。隐式导入名称沿用原声明解析。
- 候选范围枚举、排序和去重在后台执行，结果按源码及包图修订复用。冷读盘、解析、声明索引和大文件绑定核验进入有界后台队列。主循环按 `2 ms` 预算推进扫描，接收工作结果时复核修订，取消时释放任务。包图就绪后的搜索步进计时至结果交给 LSP 序列化为止。
- 使用配置的 Cargo 产物目录通过 `cargo check -p rs-gopls --locked`、引用专项 5 项、层级专项 12 项、重命名专项 40 项、全工作区 333 项及 `cargo build -p rs-gopls --release --locked`，Cargo 零告警。`cargo fmt --all -- --check` 与 `git diff --check` 通过。
- Windows amd64，Go `1.24.3`。Wave 2 与 Wave 3 发布版 SHA-256 分别为 `04ec824e72aa98c98b6d1302708c8e0ef533ccbdaa65f2bc1119299eb053f4bf` 和 `50da49a403b58476ffff2bd1d9a83635963c2f86120d249de6b6fe5fb1ab3733`。同一夹具有 536 包、2,107 个 Go 文件，其中大文件 200,002 行、2,843,786 字节，SHA-256 为 `9e50d53c9e21b41b9c90aa9702f57de3c58e830bceb6b065eedfd5672035060d`。夹具包含大文件末行和跨块用法、同名异绑、注释、字符串、Unicode、非 BMP 字符前缀和恢复节点。
- 开启 `RS_GOPLS_LOG_FILE`，交替运行 30 对独立服务进程。每个进程只打开小声明文件，预热测试包图及 WP2 原文缓存后对被测符号发起首查。每版两种 `includeDeclaration` 选项各 15 个冷样本，5 次预热后再各测 30 个热样本。每次核对有序、去重的完整 URI 和 UTF-16 范围，两版均匹配 6 个用法与可选的 1 个声明。overlay 删除、范围移动、运行中取消及重试也匹配预期。

| 指标（ms） | Wave 2 P50 / P95 / 最大 | Wave 3 P50 / P95 / 最大 |
| --- | ---: | ---: |
| 冷引用 LSP 往返 | 2,665.574 / 2,887.377 / 3,005.959 | 214.199 / 234.782 / 240.061 |
| 热引用 LSP 往返 | 25.755 / 35.768 / 37.964 | 4.820 / 7.133 / 8.736 |
| 冷引用期间并发定义排队 | 1.207 / 2.552 / 297.812 | 0.019 / 0.031 / 0.279 |

- 定义排队按传输接收到主循环派发计时。两版分别核对 16,887 和 1,452 次重叠定义请求，落点与范围全部正确。Wave 2 的最长单文件步进为 `298.359 ms`，Wave 3 的 `max_step_ms` 冷查最大 `0.129 ms`，热查最大 `0.303 ms`。
- 每次冷查候选均为 2,107 个。Wave 2 解析 2,106 个文件并建 2,106 张偏移表。Wave 3 跳过 2,101 个无拼写文件，负文件在引用扫描中产生 0 次 AST 解析、0 次声明索引和 0 次偏移表构建。5 个候选文件冷解析，查询锚点已打开，绑定依赖额外解析为 0。全名偏移表构建为 0。热查的 AST 解析和偏移表构建均为 0。成对计时组使用 WP2 缓存的原文，预筛额外磁盘读取为 0。单测另覆盖正负文件各读取一次、正文件复用所读原文解析及同修订全名表热路径。
- 同机运行 GoLand `GO-263.7307.354`，使用相同 Go SDK、同一无生成标记的大文件和项目作用域。在 Smart Mode 下由查找用法处理器执行搜索并枚举目标，首查、5 次预热和 30 次热查均匹配完整用法与 UTF-16 范围。GoLand 计时覆盖范围及目标解析与用法枚举，首查 `3,710.427 ms`，热查 P50 / P95 / 最大 `10.111 / 11.650 / 12.744 ms`。
- 补测每版每档 2 个独立进程，两种声明选项各 1 次。初始化后直接查引用，从进程启动到首次正确结果的中位数为 `5,711.571 → 2,754.953 ms`。Wave 3 与 WP2 预热重叠，WP2 提供原文，本组预筛额外读盘为 0。Wave 3 引用步进最大 `0.098 ms`，含包图和 WP2 并发阶段的定义排队最大 `4.052 ms`。
- 从未打开的大文件末行发起引用查询，冷查中位数为 `3,078.366 → 211.540 ms`，热查为 `278.760 → 4.784 ms`。Wave 3 后台解析 1 个锚点文件和 4 个候选文件，绑定依赖额外解析与偏移表构建均为 0，预筛跳过 2,101 个文件。主循环步进最大 `0.069 ms`，95 次定义请求全部正确，排队最大 `0.032 ms`。两档引用结果逐项核对完整 UTF-16 范围。
- 关闭日志补测每版 6 个独立冷样本和 30 个热样本，两种声明选项各占一半，全部返回预期结果。LSP 冷查 P50 / P95 为 `2,783.957 / 2,823.588 → 221.674 / 231.587 ms`，热查为 `25.709 / 27.177 → 6.158 / 7.397 ms`。
- 最终发布版工作区回放 34 项通过，其中 `hierarchy` 2 项、`references` 4 项、`rename` 18 项、`graph_refresh` 7 项、`symbols` 3 项。固定修订 `532228d3` 的真实项目 GoLand 引用回放匹配 34 / 34，层级归档回放匹配 20 / 20，重命名语料回放通过 12 / 12。
- 2026-11-27 完成 Wave 3，P2 验收通过。基准和回放进程已退出。自动工具策略拒绝删除 Wave 3 专用临时目录，合成夹具、统计文件和二进制副本保留在系统临时目录，清理待有删除权限后完成。

### Wave 4 发布版验收（2026-11-27）

- 包图加载线程按规范路径合并补全名称、导航原文和磁盘声明索引的观察。选中文件复用包图已采集的文件戳。其余需要校验的路径每次加载采样一次。安装时复核请求、图版本、缓存身份及加载期间的文件事件，保留 SDK 例外和下游索引失效规则。磁盘声明索引需有同次读取的文件戳或匹配的缓存内容作为依据。
- `TypePostings` 由单个有界 worker 基于不可变包图和源码摘要构建。主循环提交任务，等完成信号唤醒后原子发布。文件变更和包图替换取消旧任务，保留缺失摘要、包加载错误、本地类型和未解析嵌入的保守候选行为。聚合阶段在当前图内用数字包 ID 表示类型所有者，仅在类型边用到限定名时构建导入映射。
- 发布产物 `target/release/rs-gopls.exe` 为 8,840,192 字节，SHA-256 为 `e54cd24161c4c873f29233958347ca8d852fa3b73bfe50e12809617d35ab7e9f`。沿用环境的 Cargo 产物目录，`cargo build -p rs-gopls --release --locked` 和 `cargo build -p rs-gopls-zed --release --locked --target wasm32-wasip2` 均通过，零告警。

| 回归 | 结果 |
| --- | --- |
| `cargo check -p rs-gopls --locked`、`cargo check --workspace --locked` | 通过，零告警 |
| `cargo test --workspace --locked` | 335 项单测、2 项 CLI 测试通过，零告警。安装观察与旧图拒绝、postings 取消与原子发布的两组专项各 2 项通过。 |
| `python -B -m unittest discover -s tools -p 'test_*.py'` | 42 项全通过。用本发布版启用 4 项 stdio 遥测用例。 |
| `check_workspace_updates.py` | 全模块 60 项通过 |
| `check_goland_editor.py --capability highlight` | 69/69 通过，301 个匹配范围，4 项既有单文件边界 |
| `check_goland_hierarchy.py`、`compare_hierarchy.py` | 20/20 通过。另用归档对应的 Go 1.27.4 复跑计时，12 个共同样本可比。8 项已记录的调用范围、递归边和直接嵌入差异按归档预期核验。 |
| 仓内及固定修订 `532228d3` 的实现跳转 | 29/29、20/20 完整 UTF-16 落点匹配 |
| 固定修订 `532228d3` 的定义与引用 | 定义 19 项匹配，1 项采用归档的包级目标规则。引用 34/34 匹配。 |
| `tools/fixtures/rename/replay.py lsp` | 12/12 通过 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |

#### P3 性能复测

Windows amd64，Go 1.24.3。以前序 Wave 3 发布版 `50da49a403b58476ffff2bd1d9a83635963c2f86120d249de6b6fe5fb1ab3733` 为对照，在同一夹具上分批运行两版各 30 个独立进程，并开启脱敏日志。每个进程只打开小声明文件，先验证跨包定义并等待生产视图 postings 就绪，再执行引用请求触发测试视图升级。两张图均有 536 包、2,107 个 Go 文件，包含 Wave 3 的 200,002 行文件。每次升级均匹配完整、有序的 6 个用法和 1 个声明及其 UTF-16 范围。

| 指标（ms） | Wave 3 P50 / P95 / 最大 | Wave 4 P50 / P95 / 最大 |
| --- | ---: | ---: |
| 测试视图安装主循环 | 116.248 / 127.226 / 135.174 | 8.104 / 12.115 / 12.243 |
| 安装主循环元数据阶段 | 104.671 / 113.425 / 123.401 | 0 / 0 / 0 |
| 测试视图升级及首个引用 LSP 往返 | 2,957.645 / 3,034.728 / 3,132.024 | 2,755.811 / 2,842.640 / 2,899.520 |

每次升级后台预验证 2,106 个路径，全部复用新包图采集的戳，额外 `metadata()` 查询为 0。预验证 P50 / P95 / 最大为 `1.487 / 2.378 / 3.120 ms`。主循环按身份和变更记录校验缓存的最大耗时为 `1.714 ms`，含索引保留和切图在内的安装最大 `12.243 ms`，通过 `< 25 ms` 门槛。

Wave 4 共记录 60 次生产或测试视图 postings 发布及 120 个主循环步骤。提交最大 `0.074 ms`，发布最大 `0.545 ms`，通过 `<= 5 ms` 设计预算及 `<= 10 ms` 发布门槛。后台构建 P50 / P95 / 最大为 `5.168 / 7.102 / 8.412 ms`，完成至发布等待为 `0.027 / 0.041 / 0.103 ms`。Wave 3 在此夹具的观测精度由 `20 ms` 长操作日志阈值限定，计划中的 `46–62 ms` 来自此前的 D06 遥测。

`reader-subtypes` 使用前序层级夹具和查询顺序。每个独立进程打开相同两份文档，等待 `workspace/symbol` 就绪，将 Reader 作为首个子类型展开，计量 `prepareTypeHierarchy` 加展开的 stdio 往返。图有 120 包、702 个 Go 文件和 1 个生成条目。每个样本核对 20 项层级结果，Reader 的 `ReadCloser`、`Service`、`Base`、`Derived` 和 `Alias` 五个目标及范围一致。桌面后台 CPU 多次超过前序的 6.25% 门槛，最终两种日志配置各交替采 30 对独立进程，保留全部样本及 CPU 观测。关闭结构化日志组两版外部 CPU P50 分别为 9.43% 和 9.89%，开启组为 9.12% 和 9.40%。两组均使用既有 WP2 阶段计数。

| Reader 指标（ms） | Wave 3 P50 / P95 / 最大 | Wave 4 P50 / P95 / 最大 |
| --- | ---: | ---: |
| 首查往返，关闭结构化日志 | 31.671 / 43.542 / 58.838 | 31.013 / 38.515 / 46.808 |
| 首查往返，开启结构化日志 | 30.305 / 37.112 / 38.761 | 30.803 / 39.367 / 39.660 |
| 索引调度至就绪，开启结构化日志 | 85.579 / 94.844 / 96.523 | 86.027 / 92.552 / 99.103 |

按默认关闭结构化日志的首查评估，P50 和 P95 分别下降 0.658 ms 和 5.028 ms。开启日志的观测组 P50 和 P95 分别上升 0.497 ms 和 2.256 ms。带日志组中 Wave 4 的 Reader postings 后台构建 P50 / P95 / 最大为 `3.264 / 4.743 / 4.853 ms`，完成至主循环发布等待为 `0.034 / 0.050 / 0.052 ms`。

#### GoLand 现场对照

GoLand 现场使用 `GO-263.7307.354` 和 Go 1.24.3。两端使用同一 536 包夹具中的 200,002 行文件，大小 2,843,786 字节，SHA-256 为 `9e50d53c9e21b41b9c90aa9702f57de3c58e830bceb6b065eedfd5672035060d`，文件无生成标记。GoLand 在 Smart Mode 下计量查询计算和范围提取。rs-gopls 在生产包图和 postings 就绪后计量 stdio 往返。两端均排除启动、索引等待和界面操作，LSP 编辑档包含前置编辑通知的排队耗时。

每端每项校验 38 次，覆盖首查、5 次预热、30 次热查、插入一行未保存注释及恢复。定义从 `p000/00.go` 的 `200001:15` 落到 `target/target.go` 的 `[2:4, 2:16)`。三项高亮各匹配 4 个完整 UTF-16 范围及适用的读写种类。GoLand 返回槽专用处理器的两处 `Read` 按仓内规则映射到 LSP `Text`。夹具源码的磁盘字节和修改时间校验一致。

| 查询 | GoLand 首查 / 热 P50 / 热 P95 / 编辑 / 恢复（ms） | rs-gopls 首查 / 热 P50 / 热 P95 / 编辑 / 恢复（ms） |
| --- | ---: | ---: |
| 定义 | 5,044.066 / 8.816 / 10.193 / 1,573.020 / 1,411.656 | 0.930 / 0.475 / 0.742 / 38.632 / 43.848 |
| 局部符号高亮 | 1,408.087 / 19.988 / 24.806 / 411.218 / 433.876 | 6.364 / 0.226 / 0.332 / 46.929 / 49.678 |
| 包级常量高亮 | 791.324 / 124.641 / 138.631 / 574.391 / 1,008.117 | 0.629 / 0.213 / 0.349 / 48.990 / 46.279 |
| 成员高亮 | 5,145.117 / 114.995 / 137.181 / 2,969.520 / 2,730.776 | 132.493 / 0.299 / 0.518 / 179.531 / 179.165 |

# 诊断、补全与文件监听尾延迟优化

| 项目 | 内容 |
| --- | --- |
| 状态 | 受阻 |
| 创建日期 | 2026-11-28 |
| 用户确认 | 2026-11-28 已确认（含三项默认设计：Stage 2 防抖 200 ms、Stage 1 Fast 1 槽 + Stage 2 并行 2 槽、P2.3 优先可取消读快照后台 worker） |
| 更新日期 | 2026-11-28 |
| 完成日期 | 待填写 |

## 目标与范围

- 在真实工作区连续编辑、保存、打开 QuickFix、触发补全和交替导航时，优先交付当前版本的快诊断、已有结果和轻请求。减少旧版本任务消耗的 worker 时间、普通保存引起的图重载，以及冷依赖处理占用主循环的时间。
- 按 P0 → P1 → P2 推进。P0 隔离 Stage 1 与 Stage 2、局部生成代码操作、收敛 watcher 失效和接口候选重建。P1 将可共享的轻量声明索引与完整 AST 解耦，复用包级声明和表达式类型。P2 把导航、Hover、补全和补全项解析的冷路径移出或切分主循环，补齐遥测并现场对照。
- 保持当前快照、打开代次、文件修订、包视图及 UTF-16 坐标校验。维持诊断报告及刷新协议、QuickFix 安全性、补全候选及其排序与编辑、导航目标及范围、Hover 内容，以及构建环境和测试包的正确性。存疑的文件选择、声明相等性及局部修复数据采用保守回退。
- 沿用进行中的前序计划已落地的共享磁盘缓存、诊断阶段与版本交付、引用候选、测试视图预热、watcher 事件合并及版本级遥测。引用自身的候选算法和 GoLand 引用现场验收归前序计划，新缓存接入时复跑引用回归。前序计划的客户端能力矩阵、刷新、结果 ID 和防闪烁规则继续作为回归约束。确认 P0.3 后，QuickFix 按各操作所需证据选择路径。能在当前快照局部验证的操作可立即生成，确需完整语义的操作继续使用完整报告或等价的已验证修复数据。前序 S3 基线与本计划的 11 月 28 日混合负载基线分别记录。[实时诊断与引用查找尾延迟优化](2026-11-27-diagnostics-and-references-perf.md)
- 补全复用前序冷路径优化，并保留既有的排序、接受候选编辑、Resolve 和冷热性能回归。待确认 Stage 2 独立防抖的具体取值、快慢槽容量和公平性策略，以及补全采用可恢复时间片还是可取消快照 worker。[Go 上下文代码补全首次冷触发尾延迟优化](2026-11-26-go-completion-cold-latency.md)

## 验收标准

### Phase P0

- [x] **P0.1 · 保存与重载**。回放已打开已选中 Go 文件的连续正文保存，首次建立或回填磁盘包选择签名后，相同选择的保存触发的 `cold_file` 重载和 `go list` 均为 0。覆盖开关文件、未保存覆盖层、文件新增/删除/重命名、构建约束、导入、cgo/嵌入及 Go 环境变化，稳定包图与同条件强制全量加载一致。以人为放慢的 Go 命令验证新 generation 取消旧普通加载、旧结果安装为 0，同有效配置下重复的 `go env` 执行合并。
- [x] **P0.2 · 阶段隔离与调度**。阻塞 Stage 2 worker 并交错大文件任务、交互 Pull 与小文件编辑，Stage 1 可独立占用 Fast 槽并交付当前版本。Stage 2 在并行上限内启动，最新交互任务优先且普通任务可推进。连续输入按独立 Stage 2 防抖合并，Stage 1 保持 `25 ms` 编辑防抖。对照记录 Stage 2 排队 P95、编辑到稳定版本完整报告 P95、取消任务时间占比和交付延迟，较同机基线降低。过期报告安装及重复回包为 0。
- [x] **P0.3 · 局部 QuickFix**。人为延迟 Stage 2，用当前快照内与 `range` 相交的已交付/缓存诊断或可验证的 `context.diagnostics` 发起 `textDocument/codeAction`，可局部确认的 QuickFix 在 Stage 2 完成前返回。按 `context.only` 校验动作类型并实际应用编辑，核对 UTF-16 范围、诊断归属和修改后的正文。覆盖旧版本、乱序、空诊断、未知码、缺修复数据及重开文档。Organize Imports 独立完成轻量 import 检查。混合负载 `codeAction` 等待时间和 RPC P95 低于同机基线。
- [x] **P0.4 · 接口候选倒排**。对可证明 `InterfaceSpellings` 等价的函数体或注释编辑，跳过全图 `postings_build`，实现查询结果及范围与全量重建一致。修改局部类型、import 绑定、签名或引入不可靠语法恢复时，按影响正确失效并与全量结果对拍。

### Phase P1

- [x] **P1.1 · 轻量索引跨任务复用**。清空完整 AST LRU，在同源修订上交错诊断、导航和补全。可由声明回答的请求只构建一次共享轻量产物，完整 AST 解析为 0。需要函数体时按需完整解析。覆盖 overlay 与磁盘来源切换、文件修订/包视图切换、淘汰、并发等候及生产者/订阅者取消，确认无陈旧数据发布，内存与在途工作有界。
- [x] **P1.2 · 包索引与 Outline**。大包反复诊断时，同包符号查询按包声明修订复用，单文件 Outline 按文件修订复用。正文、签名、导入、测试视图和构建变体修改分别命中对应失效路径。与完整基准逐项比较诊断码、级别、消息、UTF-16 范围与 QuickFix，核查 `declarations_ms` 和 `unresolved_ms` 中的逐文件重复扫描消失，分段耗时低于基线。
- [x] **P1.3 · 类型推断复用**。用包含泛型替换、局部遮蔽、嵌入成员、别名和不完整语法的文件对拍，`unresolved` 与 `types` 对同一表达式和作用域复用相同推断结果，不同泛型上下文分开。验证取消、递归防护及不完整结果的处理，按节点规模报告完整 Stage 2 和 `types_ms` 的分位数，两档中位数均低于同机基线。

### Phase P2

- [x] **P2.1–P2.2 · 导航与文档**。冷态定义、类型定义和 Hover 触发共享依赖时，后台 worker 生产，主循环继续处理其他请求和变更。完成唤醒后按当前快照重放，取消和过期响应至多交付一次终止结果。对齐定义及类型定义完整目标与 UTF-16 范围，核对 Hover/Resolve 文档、源修订和补全编辑。仅取文档注释的 Resolve 完整 AST 重解析为 0。
- [x] **P2.3 · 补全交互**。用自动化夹具在候选首次收集、单文件展开和最终渲染暂停时，交错 `didChange`、轻导航和就绪诊断写出，验证前台事件可在补全结束前推进，且取消和过期请求只交付一次终态。当前快照的候选、排序、替换范围、接受编辑及 Resolve 结果通过既有行为对拍。
- [ ] **P2.4 · 归因与现场对照**。日志可从 RPC/诊断任务定位读盘、解析模式、淘汰原因、共享在途的 producer/join、`mutex` 等待及补全/Hover/Resolve 分段。验证计数、样本关联、默认关闭及脱敏。回放 P2.3 混合负载，对照补全 RPC P95、主循环同步执行占比、`-32801` 已消耗时间和相邻请求的排队及交付尾延迟，核对相对基线是否均降低。通过全量回归，按模板现场运行 GoLand，对每个计时样本核对适用的结果及范围并记录双方指标。[导航性能对照](../PLANS.md#导航性能对照)

## 实现依据

### 混合负载日志基线

日志来源 `C:\Users\rick\Downloads\rs.jsonl.txt`。2026-11-28 15:07:23–15:20:54（UTC+8），3.79 MB（3,792,741 字节），13,554 行，2,566 对 RPC，`rpc_done.total_ms` 累计 612.26 s。SHA-256 `1969de96d14dd2369951c672b032b8f0495daaa9c65d7e93210f6688cf956d69`。累计时长按各 RPC 求和，允许并发样本重叠。表中分位数沿用前期统计值，复测固定同一种算法并按同口径重算基线。前序计划 S3 的线性插值分位数单独保留。本次日志记录了服务版本 `0.1.0` 和脱敏状态，发布二进制哈希与工作区修订需在复测时固定并记录。

| RPC | 次数 | 累计时长（占全部 RPC） | P50 / P95 / Max | 执行与等待 |
| --- | ---: | ---: | --- | --- |
| `textDocument/codeAction` | 356 | 333.08 s（54.40%） | 188.24 / 5,004.03 / 11,905.49 ms | `exec_ms` P50 0.25 ms，累计排队时间占 RPC 累计时长 99.97%。 |
| `textDocument/diagnostic` | 465 | 202.98 s（33.15%） | 31.16 / 2,310.41 / 7,210.47 ms | Stage 2 执行、排队和取消见下文。 |
| `textDocument/completion` | 92 | 53.08 s（8.67%） | P95 2,433.02 ms，Max 2,936.95 ms | 17 次 ≥ 1 s，主循环同步 `exec_ms` 占 92.04%。63 次 `-32801` 累计消耗 37.79 s 的同步执行时间。 |

- **Stage 2 诊断**。156 次中 130 次记录到 `cold_dep_parses`，计数累计 22,323。该字段包含冷生产和在途加入。完成语义检查的 21 次中，19 次 `cold_dep_reads = 0`，21 次的分析中位仍为 6.322 s。完成样本的分段中位为 `types_ms` 约 3.53 s、`unresolved_ms` 约 1.73 s、`declarations_ms` 约 1.06 s。这三段在全部 156 次分析的累计耗时占比分别为 42.11%、33.20% 和 24.33%。5,528 节点的 `job=145` 和 27,392 节点的 `job=12` 各约 6.3 s，两者的 `cold_dep_reads` 均为 0。113/156 次取消，累计消耗 111.16 worker·秒，占 Stage 2 总分析时间 47.03%。前台 `job=626` 的 6.575 s Stage 2 占槽期间，执行仅 2.62 ms 的 Stage 1 `job=628` 排队 6.268 s。后台单槽串行样本 `codeAction #401` 对应 `job=40/run=37`，Stage 2 排队 5.865 s、分析 6.241 s。
- **补全阻塞时间线**。`#1650`（2.724 s）期间 `definition #1656` 排队 1.179 s。`#1189`（2.761 s）期间，记录到的相邻主循环事件至 `didChange v58` 处理相隔约 2.533 s。现有通知事件在处理时记录，通知入队至处理的精确时长留待复测。`#2144`（1.787 s）与诊断 `#2122` worker 完成至写出的 967 ms 交付尾延迟重叠。
- **导航、Hover 与 Resolve 同步长尾**。740 次 `definition_trace` 中，734 次零解析，内部执行均 ≤ 3.03 ms。6 次前台冷解析样本的内部执行时长占该事件累计执行的 98.59%。`#452`（1.462 s）期间 `#453` 排队 1.462 s，`#214` 为 948 ms，`#68` 为 674 ms。Hover `#82` / `#149` 为 1.892 s / 1.751 s。`completionItem/resolve #956` / `#2412` 为 1.527 s / 1.060 s。
- **Watcher 与全图倒排**。14 次 watcher 加载均执行 `go list`，加载中位为 656.7 ms，每次还执行两次 `go env`。13 次由已打开文件 `h:f098296b0b06562d` 的 `cold_file` 触发。5 次完成时已过期，累计 3.476 s，新 generation 到来后仍占槽累计 2.889 s。反复重载打断 Stage 2，`v88` 样本从编辑到 `job=779` / `req_id=2503` 完整诊断写出最迟 14.341 s。145 次 `postings_build` 重建全图 568 包 / 5,220 文件，中位 41.02 ms，累计 5.975 s。

### GoLand 安装包依据

安装目录 `E:\Apps\goland-2026.3.0.1.win`。`build.txt` 与 `product-info.json` 核对为 GoLand **2026.3.0.1**，构建 **`GO-263.7307.354`**。Go JAR 的 SHA-256 为 `2a13f1a753d5e15db70c5758e17db1dc28cc790e1d329e574d8b6a078d636784`。下表路径相对于安装目录，沿用并复核前期 REA 清单。

| 产物 | 相对路径 | REA 清单 ID |
| --- | --- | --- |
| Go | `plugins\go-plugin\lib\modules\intellij.go.impl.jar` | `ev_6c7c39a219d2e02923e7b819150326c434f3135777e22df0b1f7e49c6cdda5ee` |
| 高亮平台 | `lib\intellij.platform.analysis.impl.jar` | `ev_fbd13afc8847bc97a6edfe5b6298e6d22e4143b74b4503fd7b10cc2cd8a52dd9` |
| 语言平台 | `lib\intellij.platform.lang.impl.jar` | `ev_0f68e2c379dad456d286dc369987fd766ebe332baf579f8742ac8e74b1e5a1af` |
| 外部系统 | `lib\intellij.platform.externalSystem.impl.jar` | `ev_a7f47f4566f6f796b29624e744e8a971ebb5294c460a9619470ff26ee01a2819` |

清单以 REA 6.7.0 的 `inspect-artifact` 复核。方法流程使用安装包 `jbr\bin\java.exe` 与 `lib\intellij.libraries.asm.jar` 的 `org.jetbrains.org.objectweb.asm.util.Textifier` 读取对应 JAR 的类字节码，内部类用 `$` 拼写。以下机制是安装包静态证据，实际快路径覆盖率、延迟与样本结果按运行中的 GoLand 采集。

1. **跨文件 Stub 索引与零完整 AST 查询**。Go JAR 的 `com.goide.GoLightStubBuilder.skipChildProcessingWhenBuildingStubs` 跳过函数体 `BLOCK`。`com.goide.stubs.types.GoNamedStubElementType.indexStub` 和 `com.goide.stubs.types.GoMethodDeclarationStubElementType.indexStub` 建立声明名和方法索引。`com.goide.psi.impl.GoReferenceBase.getVirtualFileIdsWithSymbol` 用 StubIndex 按符号获取候选文件，`com.goide.psi.impl.GoPsiImplUtil.calcMethods` 查询 Go 方法索引。`com.goide.psi.impl.GoReference.processExistingType` / `processAnonymousRefs` 处理既有类型与匿名嵌入，`com.goide.stubs.GoVarOrConstSpecStub.getExpressionList` 在需要时从 Stub 已保存的文本恢复初始化式片段。对应 P1 的 Rust 声明摘要加必要片段，支持可由 Stub 回答的跨文件查询零完整 AST 解析。
2. **失效与类型缓存**。Go JAR 的 `com.goide.GoCodeBlockElementType.parseContents` 支持局部解析，`com.goide.psi.GoPsiTreeChangeProcessor.incPackageTrackers` 以目录关联包修改跟踪器，`com.goide.psi.impl.GoPsiImplUtil.getGoTypeRecursionSafe` 使用递归保护与缓存值。语言平台的 `com.intellij.psi.stubs.PerFileElementTypeStubModificationTracker.preciseCheck` 调用 Stub 差异比较。对应 P0/P1 的正文、声明和包级失效边界及带作用域和泛型上下文的类型 memo。
3. **诊断并发、取消与 QuickFix**。高亮平台含 `com.intellij.codeInsight.daemon.impl.DefaultHighlightVisitor`、`GeneralHighlightingPass` 和 `AnnotatorRunner.runAnnotatorsAsync`，高亮 pass 组织范围并异步执行注解器。语言平台同一包内的 `LocalInspectionsPassFactory` 和 `InspectionRunner.inspect` 组织独立检查，按脏范围与可见优先范围处理。`DaemonListeners$2.beforeDocumentChange` 在适用编辑中停止旧分析，`PassExecutorService.cancelAll` 取消旧 pass。`ShowIntentionsPass.getActionsToShow` 从光标及当前行的高亮信息收集动作，高亮平台的 `LazyQuickFixUpdaterImpl.startComputingNextQuickFixes` 从光标推进懒计算，语言平台的 `DaemonCodeAnalyzerImpl.waitForLazyQuickFixesUnderCaret` 等待光标相关修复。对应 P0 的阶段隔离、早取消与局部 QuickFix。
4. **补全、导航和 Hover 的调度与快路径**。语言平台的 `com.intellij.codeInsight.completion.CodeCompletionHandlerBase.scheduleContributorsAfterAsyncCommit` 通过非阻塞读操作等文档提交后调度贡献者，`com.intellij.codeInsight.completion.CompletionThreadingKt.tryReadOrCancel` 在读操作无法进入时取消。Go JAR 的 `com.goide.completion.GoReferenceCompletionScopeProcessor.createLookupElement` 先过滤候选，再算类型兼容。语言平台的 `com.intellij.openapi.editor.EditorMouseHoverPopupManager.showInfoTooltip` 使用可合并、可过期的非阻塞读任务。`lib\intellij.platform.ide.impl.jar` 的 `com.intellij.openapi.application.impl.ApplicationImpl.runProcessWithProgressSynchronously` 是带取消处理的同步进度入口。Go JAR 的 `com.goide.sdk.GoSdkUtil.findBuiltinFile` 通过缓存服务复用内建文件查询，`com.goide.documentation.GoDocumentationProvider.generateHoverDoc` 调用文档生成。对应 P1/P2 的过滤顺序、共享声明产物、有界后台冷生产、取消和缓存文档。
5. **保存、VFS 与 go list 边界**。Go JAR 的 `com.goide.vgo.VgoStatusTracker` 与 `com.goide.vgo.VgoModulesAware.getSettingsFiles` 在默认模块设置监听中只收集 `go.mod` / `go.work`，普通 `.go` 保存经此路径不会触发 `go list -m`。`com.goide.vgo.VgoStatusTracker$GoModFileCrcCalculator.isIgnoredToken` 忽略空白和注释，`com.goide.vgo.VgoAsyncModulesAware.reloadProject` 处理外部系统设置文件的重载，`com.goide.vgo.VgoModuleInfoProviderImpl.createTask` 构造 `go list -m -json`。`com.goide.vgo.imports.VgoImportResolver` 按解析的目录寻找 Go 包，`com.goide.psi.impl.GoPackage.processBuildableFiles` 在目录级包视图中遍历并筛选可构建文件。这两条局部路径为 P0 的 watcher 包选择判定提供参考。Rust 保留对新增/删除、构建约束、导入、cgo/嵌入和环境变化的必要加载。

### 当前 Rust 实现与对应取舍

源码核对基于提交 `62bbe7eb802fd863d379052d1814c566e56aa0a0`。`CancelToken`、`decl_rev` 与 `DiagnosticOutline` 是本计划的接口或键值概念。

| 范围 | 现有路径与取舍 |
| --- | --- |
| P0.1 · 磁盘签名与普通加载 | watcher 在已有磁盘签名时才走比较和签名回填，无法比较时可落到 `cold_file`。回填独立于打开的覆盖层。普通工作区加载尚未接上测试预热已有的取消路径。Go 根发现的两次环境查询有重叠键，缓存须校验工作目录及实际配置。[watcher 签名来源](../../src/semantic.rs#L817) · [回退分支](../../src/semantic.rs#L886) · [调度与加载](../../src/scheduler.rs#L315) · [图加载](../../src/workspace.rs#L145) · [环境查询一](../../src/workspace_roots.rs#L82) · [环境查询二](../../src/workspace_roots.rs#L223) · [进程取消](../../src/workspace/process.rs#L35) |
| P0.2 · 阶段调度 | 当前 `25 ms` 编辑防抖与 `25 ms` Stage 2 快报等待预算是两个时钟。槽位选择依据任务优先级与大小，阶段隔离需显式接入。保留结果晋级及节流，增加 Stage 2 独立启动条件。[计时常量](../../src/scheduler/diagnostics.rs#L29) · [现有槽位](../../src/scheduler/diagnostics.rs#L315) · [空槽选择](../../src/scheduler/diagnostics.rs#L822) |
| P0.3 · 局部代码操作 | RPC 在诊断路径登记 `DiagnosticConsumer::Actions`，动作引擎读取完整报告。现有 Organize Imports 也使用报告中的未用导入数据。用请求范围与快照验证可用修复数据，并在轻量路径复核现有 import 使用规则。[RPC 入口](../../src/dispatch.rs#L837) · [报告查找](../../src/semantic.rs#L929) · [动作构建](../../src/semantic/actions/code_action.rs#L17) |
| P0.4 · 倒排失效 | 编辑时即使声明摘要相等仍失效接口候选源。现有 `InterfaceSpellings` 包含方法、类型、导入和是否存在局部类型等信息。扩展等价性依据，核验局部声明与 import 绑定，语法恢复存疑时沿用保守失效，正文位置单独复核。[编辑入口](../../src/semantic.rs#L357) · [候选快照失效](../../src/semantic/implementation/candidates.rs#L80) · [重建遥测](../../src/semantic/implementation/candidates.rs#L266) · [InterfaceSpellings](../../src/semantic/implementation/interface_probe.rs#L8) |
| P1.1 · 缓存与冷计数 | `SharedDiskCache` 已区分完整与省略函数体的解析键，解析产物共享 `256 MiB` LRU 的淘汰容量。轻量产物独立缓存后复用现有源验证和加载合并。`cold_dep_parses` 统计包含生产和加入，后续明确区分。[缓存预算](../../src/semantic/shared_disk.rs#L31) · [解析键](../../src/semantic/shared_disk.rs#L246) · [声明载入](../../src/semantic/shared_disk.rs#L410) · [冷解析计数](../../src/semantic/shared_disk.rs#L862) |
| P1.2–P1.3 · 包级和查询内复用 | 诊断声明收集现有 `collect_diagnostic_outline` 逐文件调用，现有 `FileOutline` 提供可复用输入。`unresolved` 与 `types` 的检查共处一次 AST 遍历，缓存针对重复推断和名称查询。待验证包声明修订对隐式推断、构建变体、别名、嵌入、泛型替换、函数体及语法恢复的失效边界。[诊断轮廓收集](../../src/semantic/diagnostics/declarations.rs#L662) · [FileOutline](../../src/symbols.rs#L256) · [语义检查](../../src/semantic/diagnostics/mod.rs#L377) |
| P2.1 · 导航与 Hover | 导航源冷 miss 可以同步读盘/建索引，调度器已有 `SharedLoadPending` 重放。将冷 miss 生产转交同一有界后台机制，保持共享唤醒和代次复核，Resolver 查询内名称/包 memo 与 Hover 文档缓存按实际依赖校验。[导航冷源](../../src/semantic/index.rs#L852) · [重放处理](../../src/scheduler.rs#L685) · [Resolver](../../src/semantic/resolver.rs#L176) · [Hover 入口](../../src/semantic/hover.rs#L120) |
| P2.2–P2.4 · 补全、Resolve 与遥测 | 当前补全有独立冷文本读取路径，Resolve 通过声明源码取得文档。候选收集和渲染仍有需要切分的同步区段。用共享 flight、轻量文档和可取消推进规则复用上述缓存，沿用已有 RPC 和补全分段埋点补关联。[冷文本路径](../../src/semantic/index.rs#L995) · [Resolve 入口](../../src/semantic/completion/mod.rs#L276) · [文档读取](../../src/semantic/completion/render.rs#L391) · [补全收集](../../src/semantic/completion/mod.rs#L427) · [补全遥测](../../src/semantic/completion/profile.rs#L65) · [RPC 遥测](../../src/rpc_telemetry.rs#L72) |

## 实施步骤

### Phase P0 · 消除队头阻塞与无效重算

1. **P0.1 · 给 watcher 建立磁盘基线并取消过期加载**。为已打开文件维护独立于 overlay 的轻量磁盘包选择签名，覆盖首次观察及重载安装后的签名回填。基于前后已核查的磁盘观察比较包选择，遇新增、删除、重命名或选择不确定时沿用保守重载。普通工作区加载透传 `CancelToken` 到 Go 命令和加载步骤，合并并按有效配置缓存两次 `go env`。通过重复保存、环境切换、命令取消和安装代次回放。
2. **P0.2 · 隔离 Stage 1 / Stage 2 槽位**。Fast 槽专留 Stage 1。Stage 2 采用独立的 2 槽并行、交互优先级重排和可被新版本取消的 `200 ms` 编辑防抖，保留 Stage 1 的 `25 ms` 编辑防抖。区分编辑防抖、等待任务执行和现有快报交付预算，复用版本复核、阶段晋级及客户端能力规则。验证快槽不受 Stage 2 或过期任务占用。
3. **P0.3 · 依请求范围生成代码操作**。优先用当前版本已交付或缓存的诊断，或可在当前快照校验的 `context.diagnostics`，限制到请求 `range` 和 `context.only`。把可局部校验的 QuickFix 从整文件 Stage 2 等待队列中移出。完整修复数据缺失时仅做必要的局部验证或交付已可证明的操作。Organize Imports 独立做轻量 import 检查。用过期、伪造或不完整上下文验证安全回退。
4. **P0.4 · 保护接口候选倒排复用**。在 `Engine::edit` 比较编辑前后的 `InterfaceSpellings`，纳入局部类型、import 绑定和语法恢复状态。确认等价时跳过全图 `postings_build`，已扫描正文、语义验证和位置映射仍按其修订判断。以接口实现查询和全量重建对拍。

### Phase P1 · 拆分轻量声明与完整 AST，复用包级和表达式结果

5. **P1.1 · 共享依赖轻量声明/Stub 索引**。从 `SharedDiskCache` 的 `256 MiB` 解析产物 LRU 拆出独立的轻量声明/Stub 缓存，保存签名、字段、方法、接收者、嵌入、文档注释及必要初始化式片段，排除函数体 CST。按经过验证的源修订全局共享轻量索引，合并同一修订的在途生产。诊断 job、导航、补全和跨包语义优先使用轻量产物，需要函数体时按需取得完整 AST。保留按实际包视图绑定的语义及按正文修订核对的位置。
6. **P1.2 · 包级符号与同包 Outline 缓存**。建设按 `(package_id, decl_rev)` 维护的包级名称/接收者符号索引，及按文件修订复用的同包 `DiagnosticOutline`。让诊断按索引查询声明和未解析名，消除 `declarations_ms`（基线约 `1.06 s`）及 `unresolved_ms`（约 `1.73 s`）中的逐文件线性扫描。对改包声明、构建条件、恢复语法和仅移动位置的编辑验证各自失效规则。
7. **P1.3 · 共享诊断内的类型推断**。在单轮诊断或语义查询内按表达式和泛型替换上下文记录类型，并缓存按正确作用域标识的局部绑定类型。`unresolved` 与 `types` 复用结果，消除 `types_ms`（基线约 `3.53 s`）中的重复推断。验证递归、取消、不完整输入和泛型上下文隔离，再进行完整阶段的同机回放。

### Phase P2 · 消除主循环队头阻塞并补齐观测

8. **P2.1 · 导航与 Hover 冷生产移出主循环**。`definition`、`typeDefinition` 和 `hover` 的冷依赖读盘、解析及轻量索引构建统一走有界后台 worker。沿用 `SharedLoadPending` 唤醒重放，由 worker 负责冷 miss 生产，主循环复核版本并交付。给前台 `Resolver` 加查询内包符号 memo，Hover 加按包语义及文档源修订校验的包级文档缓存。用冷依赖、取消和并发轻请求核对进展及目标。
9. **P2.2 · Resolve 读取轻量文档**。`completionItem/resolve` 优先从声明摘要/Stub 取得文档注释，避免为注释同步重读重解析整份目标文件。校验解析项的来源和修订，保持已有 Resolve 编辑和 Markdown 输出。
10. **P2.3 · 补全共享在途加载与有界推进**。按已确认方案，将首次候选收集、单文件展开、最终渲染和响应值编码迁入可取消的读快照 worker，并接入 `SharedDiskCache` 的共享 flight。保留每段的取消检查、版本复核、候选排序及稳定输出。验证已完成诊断和轻请求可在补全计算期间及时返回。
11. **P2.4 · 细化遥测并验收**。沿用版本级诊断关联和脱敏开关，增加解析模式、淘汰原因、producer/join 次数、`mutex` 等待、关联 `req_id` 的补全分段及 Hover/Resolve 分段耗时。完成相同工作负载回放、专项及全量回归、GoLand 现场对照，按阶段回填结果和验收勾选。

## 验证计划

### 构建与回归

确认后在环境配置的 Cargo 产物目录先执行包检查，再执行对应专项和发布回放。沿用项目约定的 toolchain、profile、features 和 target。发布二进制路径通过 `cargo metadata --no-deps --format-version 1` 获取。

~~~powershell
cargo check -p rs-gopls --locked
cargo test -p rs-gopls --locked scheduler::diagnostics
cargo test -p rs-gopls --locked semantic::diagnostics
cargo test -p rs-gopls --locked semantic::actions
cargo test -p rs-gopls --locked semantic::completion
cargo test -p rs-gopls --locked semantic::implementation
cargo test -p rs-gopls --locked semantic::shared_disk
cargo test -p rs-gopls --locked semantic::references
cargo test -p rs-gopls --locked workspace
cargo test -p rs-gopls --locked
cargo fmt --all -- --check
git diff --check
cargo build -p rs-gopls --release --locked
~~~

复用现有工作区增量回放、诊断脱敏遥测、GoLand 行为语料和 stdio 工具。`$server` 指向被测发布版，`$workspace` 指向固定修订的共同工作区。

~~~powershell
python -B tools/check_workspace_updates.py $server --module diagnostics --module completion --module graph_refresh --module actions
python -B tools/test_diagnostic_telemetry.py $server
python -B tools/check_goland_diagnostics.py $server
python -B tools/check_goland_actions.py $server
python -B tools/check_goland_completion.py $server --workspace tools/fixtures/completion --fixture tools/fixtures/completion/goland-completion.json
python -B tools/check_goland_definitions.py $server --workspace $workspace
python -B tools/check_goland_definitions.py $server --workspace $workspace --method typeDefinition
python -B tools/check_goland_hover.py $server --workspace $workspace
~~~

### 混合负载与验收口径

- 固定优化前后发布版及 SHA-256、工作区修订、Go SDK、构建环境、客户端版本、打开文件、请求顺序及编辑/保存节奏。原始脱敏日志用于定位慢序列，复现用相同工作区动作或受控夹具，记录与原始采样条件的差异。回放普通编辑与连续编辑、大包与生成文件、冷/热依赖、重叠 Pull 与 QuickFix、连续保存加命令延迟及过期 generation。
- 复刻基线口径。`rpc_done` 按方法、状态和请求 ID 统计 N/P50/P95/Max 与总时长，独立记录同步执行、队列、等待、序列化和写出。诊断按文档、打开代次、版本、job、stage 和 lane 关联编辑到首个正确快报、稳定到完整报告、队列时间、取消请求至退出及淘汰。取消任务开销按各 worker 的任务耗时累加。Watcher 区分命令启动/取消/完成、过期完成和最终图安装。补全、导航、Hover 和 Resolve 用相邻请求和通知时间戳核对主循环阻塞及交付尾延迟。复测记录 `didChange` 的发送与处理时间，核对补全期间的通知延迟。
- 用一致操作比较各阶段优化前后差异。关日志测端到端门槛，开脱敏日志核对机制并记录开销。区分当轮首次准备、重复热查和编辑后复查，保留样本数、P50/P95/Max、错误及未完成数。前序计划中的 Zed Pull、VS Code Push/Pull、刷新及诊断呈现回归继续按其能力矩阵判定。[前序验证计划](2026-11-27-diagnostics-and-references-perf.md#验证计划)

### 运行中的 GoLand 对照

同一主机运行 GoLand `GO-263.7307.354`，记录 Smart Mode、rs-gopls 发布版及哈希、共同项目修订、Go SDK、构建与搜索范围、打开文件和编辑序列。采用仓库已有入口采集实际导航、类型导航、Hover、补全、诊断和代码操作。逐次核对定义和类型定义的完整目标与标识符 UTF-16 范围，每档沿用前序计划至少 30 次有效测量的口径。另核对落点、Hover 要点、补全候选/排序/接受编辑、诊断范围及可共同比较的修复。分别记录双方结果差异及其样本 ID，使用结果一致样本比较耗时。索引就绪后的首查、重复查询和编辑后查询分组报告 N/P50/P95/Max，注明缓存状态、计时起止点及 UI、LSP 传输边界。若测启动到首次正确结果，用双方相同起止点另记。若采峰值工作集，记录进程范围、采样间隔和工具。[导航性能对照](../PLANS.md#导航性能对照)

[定义](../goland-definition-baseline.md) · [类型定义](../goland-type-definition-baseline.md) · [Hover](../goland-hover-signature-baseline.md) · [补全](../goland-completion-baseline.md) · [诊断](../goland-diagnostics-baseline.md) · [代码操作](../goland-actions-baseline.md)

## 完成记录

- 2026-11-28 创建计划，核对模板、前序计划、基线日志、Rust 路径及 GoLand 安装包依据。同日收到用户确认，计划进入「进行中」。
- 2026-11-28 交付 P0 第一批。P0.1 将磁盘包选择基线与打开文档的覆盖层分开，在打开、保存、监听与加载安装路径建立和回填。连续正文保存和文件监听的 LSP 日志中，`cold_file` 重载与 `go list` 启动均为 0。测试覆盖覆盖层切换、文件新增删除和重命名、包名、导入、构建约束、cgo 和 embed 变化，并核对稳定包图与实跑 `go list` 重建的结果。
- 普通加载与测试预热共用取消路径。本地模块代理挂住旧代次的 `go list`，切换配置后连接关闭、旧结果安装数为 0，后续代次安装成功。根发现和 GOPATH 回退共用四项 `go env` 查询，测试核对重复加载的命令次数以及环境变量、`GOENV` 和 `go.work` 变化后的重新查询。
- P0.4 测试确认正文和普通注释编辑保留已发布或在途的候选倒排，查询目标及范围与全量重建一致。局部类型、导入绑定、签名和语法恢复变化触发失效并与重建结果对拍。另验证异步打开大文件完成时淘汰基于磁盘内容构建的倒排。
- Windows 下沿用环境配置的 Cargo 产物目录，使用 Go `1.24.3`。`cargo check -p rs-gopls --locked` 通过。`cargo test -p rs-gopls --locked workspace` 通过 53 项，`cargo test -p rs-gopls --locked semantic::implementation` 通过 35 项，`cargo test -p rs-gopls --locked scheduler` 通过 58 项，`cargo test -p rs-gopls --locked semantic` 通过 329 项，`cargo test -p rs-gopls --locked telemetry::tests::configured_sessions_emit_private_jsonl` 通过 1 项。测试过滤器有重叠。
- 完整 P0 混合负载回放与运行中的 GoLand 对照待执行。验收项在完整回放后统一更新。
- 2026-11-28 交付 P0 第二批。P0.2 将普通 Stage 1 限定在 Fast 槽和仅在 Fast 取消退出期间可用的 Reserve 槽，大文件快报与完整分析分别使用独立 worker。Stage 2 全局最多同时执行两项，按最新活跃订阅排序，连续派发三项交互任务后优先执行最早可运行的普通任务。包图安装和打开解析完成后统一重排就绪任务。连续编辑立即取消旧版本，Stage 1 保留 `25 ms` 编辑防抖和独立的 `25 ms` 快报交付预算，Stage 2 在最后一次编辑后独立防抖 `200 ms`。遥测关联 `lane`、`stage`、`job`、`run`，并记录实际报告阶段 `report_stage`。
- P0.3 优先查询当前快照的已交付或缓存报告，按请求范围及 `context.only` 筛选。客户端诊断的码、归属、消息和 UTF-16 范围经当前源码复核后可触发局部修复。缺少包的修复使用当前报告中的语义证据。显式仅请求 QuickFix、诊断上下文为空且当前没有可证修复时保留完整报告回退。Organize Imports 按当前文件的词法绑定和包图中的导入名称独立检查使用情况，精确移除已证实未用的导入项。包上下文不足时仍可排序，保留无法确认的导入及别名。
- Windows 使用 Go `1.24.3` 并沿用环境配置的 Cargo 产物目录。`cargo check -p rs-gopls --locked` 通过，随后执行 `cargo test -p rs-gopls --locked` 的专项过滤器，`scheduler::diagnostics` 通过 18 项、`semantic::diagnostics` 通过 50 项、`semantic::actions` 通过 46 项、`dispatch` 通过 1 项、`telemetry` 通过 4 项、`scheduler` 通过 64 项。全包测试通过 443 项单元测试和 2 项 CLI 集成测试。过滤器有重叠。
- 受控测试覆盖阻塞两个 Stage 2 任务时的快槽和大文件槽隔离、图加载后的交互重排、后台任务推进及连续编辑合并。过期报告安装和重复回包均为 0。协议测试验证 Stage 2 阻塞期间直接交付 QuickFix 和 Organize Imports，应用 UTF-16 编辑核对正文，并覆盖旧版本、乱序、未知码、空诊断、缺修复数据及重开文档。
- 当前调试版通过 `python -B tools/check_workspace_updates.py target/debug/rs-gopls.exe --module actions --module diagnostics` 的 4 个 stdio 用例、`python -B tools/test_diagnostic_telemetry.py target/debug/rs-gopls.exe` 的 8 项遥测测试，以及 `python -B tools/check_goland_actions.py target/debug/rs-gopls.exe` 的 9 个代码操作夹具。遥测测试校验槽位与 worker 阶段对应关系，并对 Windows 临时工作区的短暂占用做限时清理重试。`cargo fmt --all -- --check` 与 `git diff --check` 通过。
- P0.2 与 P0.3 的代码级验证已完成。同机混合负载回放、排队和交付 P95、取消耗时占比，以及运行中的 GoLand 对照留待 P0 汇总验收。
- 2026-11-28 交付 P1.1。独立的无树 `SharedDeclarations` LRU 限额为 4,096 项 / 64 MiB，计入内容键保留的正文和声明产物，超过 1 MiB 的单项只交付本轮请求。完整 AST LRU 限额为 1,024 项 / 256 MiB。内容键同时校验指纹和字节，可靠的完整和省略解析共享一份声明，完整恢复产物使用单独的投影标记。完整和声明 flight 共用注册锁。声明回退时将发布权交给已有完整解析，或在当前加载线程完成，避免加载池内互等。取消只移除当前等待者。沿用 GoLand 安装包中跳过函数体和保留声明及初始化式的 Stub 依据。[缓存与路径票据](../../src/semantic/shared_disk.rs) · [解析协调](../../src/semantic/shared_disk/parsing.rs) · [投影和计重](../../src/semantic/shared_disk/declarations.rs) · [省略范围校验](../../src/semantic/source.rs)
- 定义、`file_index`、补全筛选与展示、诊断兄弟与依赖查询均先取声明。磁盘路径通过读票据验证后绑定，长期 `DiskIndex` 使用弱引用，单次查询按预算持有产物。引用及实现继续依据原有观测选择精确字节。诊断目标和需要正文的查询按需取完整语法。`SharedDeclarations::outline` 保留诊断声明的源码顺序、重复声明和直接字段，`summary` 提供 P1.2 的声明等价依据。`InitStub` 与 `declaration_syntax` 为 P1.3 提供原文范围、默认类型和按需语法入口。`DeclPresentation` 保留文档、detail、kind、弃用和可调用签名，供补全、Resolve 与后续 Hover 使用。[索引入口](../../src/semantic/index.rs) · [补全展示](../../src/semantic/completion/render.rs) · [诊断轮廓](../../src/symbols.rs) · [诊断 worker](../../src/semantic/diagnostics/worker.rs) · [引用 worker](../../src/semantic/references/worker.rs)
- P1.1 回归夹具清空完整 AST LRU 后，在同一磁盘修订交错诊断、定义与补全，观察到 1 次声明构建、0 次完整解析。关闭覆盖层后重用同一声明，修改磁盘文档后消费新修订，选择该文件为诊断目标后增加 1 次完整解析并检出函数体类型错误。切换包视图后该文件退出查询结果。另覆盖完整解析先到、声明回退并入、淘汰与超限交付、注册上限、取消和 watcher 期间的修订变更，验证文档、签名、初始化式范围及 UTF-16 坐标。[跨请求夹具](../../src/semantic/diagnostics/worker.rs) · [协调与容量测试](../../src/semantic/shared_disk/parsing/tests.rs)
- 完整 AST 淘汰后，watcher 复用旧修订的声明摘要并读取新修订的声明。闭合文件的纯函数体保存保留包图和诊断语义修订，完整 AST 解析为 0。[包选择和声明判定](../../src/semantic.rs) · [工作区验证](../../src/semantic/workspace_install.rs)
- Windows 沿用环境配置的 Cargo 产物目录。`cargo check -p rs-gopls --locked` 通过。`cargo test -p rs-gopls --locked` 的专项过滤器全部通过，`semantic::shared_disk` 17 项、`semantic::diagnostics` 53 项、`semantic::completion` 42 项、`semantic::references` 16 项、`semantic::implementation` 35 项、`scheduler` 64 项，另有 `semantic::workspace_install` 9 项。全包通过 453 项单元测试和 2 项 CLI 集成测试。`cargo fmt --all -- --check` 和 `git diff --check` 通过。一次调度器复跑遇到 Windows 测试临时目录已存在，重跑和最终全包均通过。P1.2、P1.3 和 P2 仍在计划范围内，运行中的 GoLand 计时对照安排在 P2.4。

### P1.2 与 P1.3

- P1.2 在 Stage 2 准备同包名称、类型和接收者候选，并共享诊断的全局名称、首条声明归属及直接字段集合。包键覆盖构建与测试视图、选中的有序文件集合、导入目标、overlay 代次与声明修订、磁盘声明等价及恢复可靠性。正文等价编辑复用候选，符号和 `DiagnosticOutline` 的坐标从当前精确修订获取。包索引缓存预算为 256 项 / 32 MiB，单项超出 4 MiB 只在本轮使用。查询仅在当前来源与已缓存候选一致时使用热索引，冷态、未验证来源及持有独立字节快照的引用路径沿用原查找。覆盖层同一修订发布一份 outline，同时冷启动的 worker 可以各自完成投影后共享获胜产物。[包索引](../../src/semantic/package_index.rs) · [文件修订与查找](../../src/semantic/index.rs) · [诊断声明](../../src/semantic/diagnostics/declarations.rs)
- 磁盘声明产物被淘汰时，可从已捕获的完整 AST 经共享加载流程回填 `SharedDeclarations`。测试验证普通与恢复投影都能回填，共享相同产物，且完整解析次数保持不变。[投影回填](../../src/semantic/shared_disk/parsing.rs) · [回填测试](../../src/semantic/shared_disk/parsing/tests.rs)
- P1.3 由 `Resolver` 持有单轮查询的 `InferenceMemo`，Stage 2 的 `unresolved` 与 `types` 共享同一实例。键区分源语法修订、包视图、推断模式、结果槽、局部绑定作用域和泛型实例化上下文。精确的 callee 替换环境保留未绑定 `None` 与缺项的差别。取消、深度 32、重入、恢复及依赖降级通过 `active` 和 `transient_epoch` 约束，受影响的外层计算即使 fallback 返回类型也跳过写入。恢复 `Locals` 前清空 memo。预算为 65,536 项 / 8 MiB。[推断缓存](../../src/semantic/inference_memo.rs) · [Resolver](../../src/semantic/resolver.rs) · [Stage 2](../../src/semantic/diagnostics/mod.rs)
- 74 文件夹具逐轮对拍索引与线性基准，覆盖签名、导入、正文及位置变化、文件选择、测试视图和来源切换。完整报告及生成的 QuickFix 一致，位置移动后的声明跳转使用当前 UTF-16 范围。memo 开关对拍验证泛型、局部和 type switch 遮蔽、别名与嵌入、递归初始化及损坏语法，观察到表达式及局部绑定命中。另验证了替换身份、深度约束、取消、瞬态结果传播和预算。[对拍夹具](../../src/semantic/diagnostics/tests/caches.rs)
- Windows 使用环境配置的 Cargo 产物目录。`cargo check -p rs-gopls --locked` 通过。专项通过诊断 56、共享磁盘 17、补全 42、引用 16、实现 35、调度 64、代码操作 46、包索引 1、推断 memo 4 项。`cargo test -p rs-gopls --locked` 通过 461 项单元测试和 2 项 CLI 集成测试，另有 1 项独立运行的计时测试。`cargo fmt --all -- --check` 和 `git diff --check` 通过。
- stdio 语料验证通过诊断 11/11、编辑及 QuickFix 9/9。诊断脚本记录了自导入和无效导入两项既有 GoLand 语料差异。

同机 Release 对拍使用 98 文件包，按 5,538 和 27,432 个访问节点分档。每档以线性路径、仅包索引、包索引加 memo 三种模式预热后交错运行，每种模式 30 次，逐次比较完整报告。计时覆盖热态 `analyze` 调用。下表单元格为 P50 / P95 / Max，单位为毫秒，分位数按线性插值。复测命令为 `cargo test -p rs-gopls --release --locked --bin rs-gopls stage2_cache_timings_by_ast_size -- --ignored --nocapture`。[计时夹具](../../src/semantic/diagnostics/tests/caches.rs)

| 访问节点 | 模式 | Stage 2 | `declarations_ms` | `unresolved_ms` | `types_ms` |
| ---: | --- | ---: | ---: | ---: | ---: |
| 5,538 | 线性 | 140.624 / 149.921 / 150.156 | 10.505 / 13.149 / 13.373 | 41.566 / 44.108 / 44.780 | 85.892 / 90.409 / 90.972 |
| 5,538 | 包索引 | 137.240 / 150.361 / 154.736 | 9.841 / 12.091 / 12.785 | 38.696 / 42.349 / 43.470 | 84.758 / 93.767 / 96.515 |
| 5,538 | 包索引与 memo | 50.378 / 57.646 / 58.706 | 9.805 / 13.105 / 13.525 | 1.679 / 1.940 / 1.976 | 35.166 / 39.472 / 39.916 |
| 27,432 | 线性 | 699.523 / 749.291 / 769.449 | 20.320 / 26.612 / 28.515 | 217.627 / 231.639 / 242.328 | 447.777 / 477.899 / 490.426 |
| 27,432 | 包索引 | 678.894 / 722.905 / 737.382 | 19.732 / 24.535 / 26.996 | 201.944 / 214.471 / 217.951 | 443.943 / 469.946 / 485.815 |
| 27,432 | 包索引与 memo | 507.210 / 556.456 / 578.348 | 20.371 / 27.095 / 30.614 | 121.926 / 131.536 / 139.655 | 347.054 / 379.071 / 404.845 |

仅启用包索引时，两档 `declarations_ms`、`unresolved_ms` 的 P50 均下降。加入 memo 后，两档完整 Stage 2 和 `types_ms` 的 P50 继续下降，大档 `declarations_ms` 基本持平。P1.2 和 P1.3 已完成夹具验收并勾选。原混合负载日志的现场回放及运行中的 GoLand 对照仍按整项计划验收。

### 2026-11-28 · P2.1 和 P2.2

- 定义、类型定义和 Hover 的冷文本、声明、完整 AST 和 SDK 内建路径由共享加载池生产。挂起请求通过 `QueryReadMemo` 持有在途、已完成或失败的产物。存量 LRU 拒收时可继续消费飞行结果。队列腾出容量时唤醒等待者。重放和交付复核已安装图、参与查询的 overlay 开启代次及内容修订，以及磁盘失效代次。冷解析和冷声明产物完成后在后台复核文本读票。收到 watcher 通知后，沿用原有合并窗口，待磁盘失效应用后再处理前台查询。[调度](../../src/scheduler.rs) · [共享加载](../../src/semantic/shared_disk/foreground.rs) · [队列](../../src/semantic/shared_disk/queue.rs)
- 前台 Resolver 复用包成员查找和类型详情的成功结果与完整扫描后的未命中。包文档缓存按安装图、包身份和实际查询到的有序来源前缀校验，保留首个文档之前的空项或缺失文件。符号 Hover 复用可保真呈现的轻量声明，依赖完整语法的情况由同一加载池取 AST。[Resolver](../../src/semantic/resolver.rs) · [Hover](../../src/semantic/hover.rs) · [包文档缓存](../../src/semantic/hover/package_docs.rs)
- 补全候选与 SDK 导出项从实际签发产物读取 `SourceRev`。`completionItem/resolve` 在后台验证磁盘目标，按完整 UTF-16 范围从轻量声明取注释。打开的目标复用当前 overlay 语法树。旧数据缺少目标修订时仍处理已有详情和延迟导入编辑。目标修订不符返回 `-32801`。轻量语法无法可靠投影的冷目标会省略可选文档。[补全项签发与文档](../../src/semantic/completion/render.rs) · [Resolve](../../src/semantic/completion/mod.rs)
- 调度测试阻塞未打开查询文件、跨文件声明及 Hover 完整语法回退的后台生产。编辑和轻定义先完成，释放加载后能按原目标与 UTF-16 范围返回。覆盖 watcher 合并、队列满、超大拒收、删除、不可读内容、解析期间无通知的源替换、同代次重装图和取消。Resolve 对拍清空完整 AST 和声明缓存后的跨文件文档，记录到一次轻量解析及零次完整 AST 解析，核对 Markdown、源修订、匿名成员范围和延迟导入编辑。[调度验收](../../src/scheduler/tests/foreground_navigation.rs) · [补全验收](../../src/semantic/completion/tests.rs)
- Windows 沿用环境配置的 Cargo 产物目录。`cargo check -p rs-gopls --locked`、`cargo build -p rs-gopls --release --locked`、`cargo fmt --all -- --check` 和 `git diff --check` 通过。专项通过调度 67、共享磁盘 23、补全 43、引用 16、诊断 56、代码操作 46、实现 35、Hover 11、Resolver 6 项。全包通过 474 项单元测试和 2 项 CLI 集成测试，另有 1 项既有计时测试按标记跳过。发布版 stdio 固定语料通过定义 20/20（含 1 项已记录的包对象差异）、类型定义 34/34、Hover 21/21。

在同机运行的 GoLand `GO-263.7307.354` Smart Mode 中重采集 `localchat/server` 的 54 个固定光标。双方使用项目修订 `532228d3d36bd9f0fffb4bda35c180485b2eadd6` 和 Go 1.27.4。GoLand 使用只读编辑器视图，rs-gopls 查询磁盘版本，双方均无编辑。每个光标和方法先做一次预热，再测 30 次重复查询，并逐次比对完整目标集合及其 UTF-16 范围。GoLand 计时覆盖 IDE 动作目标计算及范围提取，rs-gopls 计时覆盖 stdio 请求到响应。Release 二进制 SHA-256 为 `2d4c26f5114e2f9fc5e0ec1596f61e089b98c504f6c926b9f212ab7fd66fd34b`。

| 方法 | 一致光标 | 每方样本数 | GoLand P50 / P95 / Max（ms） | rs-gopls P50 / P95 / Max（ms） |
| --- | ---: | ---: | ---: | ---: |
| 定义 | 49 / 54 | 1,470 | 0.287 / 1.119 / 58.507 | 0.189 / 1.796 / 12.310 |
| 类型定义 | 50 / 54 | 1,500 | 0.195 / 0.756 / 18.418 | 0.219 / 0.597 / 2.697 |

定义的 `package-qualifier`、`import-qualifier` 返回 GoLand 包对象，`context-pointer-binder`、`package-clause`、`blank-identifier` 的 GoLand 文件位置在 rs-gopls 为空。类型定义的 `writeerror`、`inituser`、`send`、`range-member` 也有同类差异。现有发布基线 `532df9a7` 按相同采集结果复跑，差异集合和结果相同。以上对照覆盖重复查询，索引就绪后首查、编辑后查询和混合负载回放仍归 P2.4。

### 2026-11-28 · P2.3 补全 worker 与共享 flight

- `textDocument/completion` 在主循环捕获工作区、overlay、SDK 和共享磁盘的只读快照。专用 worker 使用容量均为 1 的工作及结果通道，调度器另保留至多一个最新请求。编辑、关闭、图替换或显式取消会抢占在途任务。交付按任务序号复核文档版本、打开代次、内容修订、工作区图和磁盘代次。版本失效返回 `-32801`，显式取消和同修订的请求取代返回 `-32800`。图或打开文档解析就绪后可重新捕获快照，图加载失败走后台本地补全。[分派](../../src/dispatch.rs) · [主循环](../../src/main.rs) · [调度器](../../src/scheduler/completion.rs) · [快照 worker](../../src/semantic/completion/worker.rs)
- worker 跨请求保留图目录、SDK 导出、名称预筛和预期类型权重，共享已预热的 SDK 清单。按图、overlay 修订和磁盘失效路径更新缓存。取消检查覆盖光标修复及增量 Tree-sitter、词法和递归类型分析、候选及单文件展开、SDK 与预筛等待、稳定排序与全局去重、渲染和响应值编码。[快照](../../src/semantic/snapshot.rs) · [光标](../../src/semantic/completion/cursor.rs) · [期望类型](../../src/semantic/completion/expected.rs) · [排序](../../src/semantic/completion/ranking.rs) · [渲染](../../src/semantic/completion/render.rs)
- 预筛、索引、候选包名探测和关闭文档的补全读取复用共享文本及声明产物。预筛维持每批最多 8 源、16 MiB，复核后将同一文本和读票交给当前请求。订阅者先等到队列可用，再登记新 flight。取消只解除自身订阅，已有 producer 继续服务其他请求。已有图序下的同名声明优先级继续由首条声明决定。[预筛](../../src/semantic/completion/prefilter.rs) · [索引](../../src/semantic/index.rs) · [共享队列及读取](../../src/semantic/shared_disk.rs)
- 自动化覆盖主循环在候选首次收集、单文件展开、渲染三个暂停点持续交付定义和 Pull 诊断，并在处理编辑后立即返回唯一的过期终态。另覆盖最新优先、显式取消、图失败回退、预筛订阅者取消及字节预算。后台和既有语义入口逐项对拍补全数据、排序、UTF-16 替换范围、应用主要与附加编辑和 Resolve 结果。[调度用例](../../src/scheduler/tests/completion_worker.rs) · [补全用例](../../src/semantic/completion/tests.rs)
- Windows 沿用环境配置的 Cargo 产物目录。`cargo check -p rs-gopls --locked`、`cargo fmt --all -- --check` 和 `git diff --check` 通过。专项通过补全 47 项、调度 70 项、共享磁盘 24 项、诊断 56 项和引用 16 项。全包 `cargo test -p rs-gopls --locked` 通过 483 项单元测试及 2 项 CLI 集成测试，1 项既有计时测试按标记跳过。
- P2.4 从 `completion_worker` 的 `req_id`、`job_seq`、`queue_ms`、`worker_ms`、`result_queue_ms`、`cancel_to_exit_ms` 关联 RPC。现有补全 profile 增加或延续 `local_candidates`、`candidate_expansion`、`import_wait`、`rank`、`render` 和 `encode`。可选旧 profile 仍同步落盘，迁入异步脱敏日志和共享 flight 详细计数随 P2.4 验证。实际混合负载端到端回放、P95 对照及 GoLand 全量对照随 P2.4 完成。[RPC 埋点](../../src/scheduler/completion.rs) · [补全 profile](../../src/semantic/completion/profile.rs)

### 2026-11-28 · P2.4 第一批遥测

- 共享磁盘按产物记录 `shared_disk_lookup` 的 `cache_hit`、`producer_requested`、`join`，并用 `operation=source|validation` 标注工作类型。通过 `flight_id` 关联 `shared_disk_flight` 的 metadata、入池排队、读盘、解析和索引，以及 `shared_disk_wait`。解析模式覆盖 `full`、`body_elided` 和 `fallback_full`，产物覆盖 `text`、`declarations` 和 `full_ast`。完整解析接手声明产物时记录 `producer_flight_id`。缓存释放锁后投递 `shared_disk_mutex` 和 `shared_disk_lru`，记录锁等待、前台 `try_lock_busy`、条目和字节上限、超大拒收、替换及 watcher 等失效原因。[共享缓存](../../src/semantic/shared_disk.rs) · [事件字段](../../src/semantic/shared_disk/trace.rs)
- RPC 与补全 worker、预筛、引用 worker 通过会话内脱敏 `req_id` 关联，共享加载池继承 producer 的关联。补全任务附带 `job_seq`，诊断 worker 及其共享加载附带 `job/run/stage/lane`。保留 `rpc_done.exec_ms` 的主循环口径，记录 `workspace_wait_ms`、`shared_source_wait_ms` 和 `shared_source_replay_count`。补全 worker 另记待发、通道排队、执行、结果排队及取消到退出耗时。诊断结果被主循环选中时记录 `diagnostic_worker_result.result_queue_ms`。[RPC](../../src/rpc_telemetry.rs) · [补全调度](../../src/scheduler/completion.rs) · [诊断调度](../../src/scheduler/diagnostics.rs)
- `completion_worker.phases_ms` 记录当次任务阶段，`completion_profile.phases_ms` 按请求累计预期类型、同包、导入、单文件展开、排序、渲染及编码。Hover 按 `anchor/symbol/package_doc/markup` 汇总，Resolve 按 `rev_check/detail_doc/import_edit` 汇总，两者跨重放累计活动时间。默认关闭遥测，`RS_GOPLS_COMPLETION_PROFILE` 可作为进程日志路径的兼容入口，复用异步脱敏 JSONL writer。路径和字符串请求 ID 使用会话哈希，SDK 回退只保留固定原因。stdio 从解帧到主循环选中计算 `didChange.dispatch_queue_ms`，包含版本检查忽略的通知。[补全 profile](../../src/semantic/completion/profile.rs) · [传输](../../src/transport.rs)
- [x] 专项验证计数、LRU 原因、唯一 flight ID 及回退交接、RPC 和诊断关联、默认关闭、脱敏、补全与 Resolve 分段、有效和旧版本 `didChange` 的排队耗时。遥测专项通过 6 项，共享磁盘专项通过 26 项，stdio 遥测专项通过 1 项。[会话测试](../../src/telemetry/tests.rs) · [stdio 测试](../../tests/cli_telemetry.rs)
- [x] `cargo check -p rs-gopls --locked`、`cargo fmt --all -- --check` 和 `git diff --check` 通过。`cargo test -p rs-gopls --locked` 通过 488 项单元测试和 3 项 CLI 集成测试，1 项既有 Stage 2 计时测试按标记跳过。
- [x] `cargo build -p rs-gopls --release --locked` 通过。`cargo metadata --no-deps --format-version 1` 返回产物目录 `E:\Code\rs-gopls\target`。Release 为 `E:\Code\rs-gopls\target\release\rs-gopls.exe`，`version -json` 返回 `0.1.0`，SHA-256 为 `28c88ce977ca767062c659fe4d5d3f6a5a9fc284514a013715d1f5ce3cb79b49`。
- Release 全量回放、混合负载及 GoLand 现场对照见下节。

### 2026-11-28 · P2.4 Release 最终验收

验证产物为 `E:\Code\rs-gopls\target\release\rs-gopls.exe`，`version -json` 返回 `0.1.0`，SHA-256 复核为 `28c88ce977ca767062c659fe4d5d3f6a5a9fc284514a013715d1f5ce3cb79b49`。共同工作区为 `E:\Code\localchat\server`，Git 修订 `532228d3d36bd9f0fffb4bda35c180485b2eadd6`，项目子树验收前后均干净。沿用本计划已记录的 Cargo 验证与 Release 构建。

#### Python 回归

用 `python -B` 逐条执行本计划命令。默认 shell 为 Go 1.24.3，`localchat/server` 自动选择 Go 1.25.0。耗时覆盖 Python 进程启动和退出，单位为秒。

| 脚本 | 退出码 | 通过数及差异 | 墙钟 s |
| --- | ---: | --- | ---: |
| `check_workspace_updates.py`（四模块） | 1 | 14/16。失败 `async-refresh`、`completion_cancellation_and_stale_tasks` | 81.231 |
| `test_diagnostic_telemetry.py` | 0 | 8/8，测试本身 10.107 s | 10.499 |
| `check_goland_diagnostics.py` | 0 | 9 PASS、2 已知差异，共 11/11 · `self-import`、`invalid-import` 容许附加 `package.load`。11 次 `previousResultId` unchanged、2 次未保存修复与恢复通过 | 1.869 |
| `check_goland_actions.py` | 0 | 9/9，0 差异 | 0.995 |
| `check_goland_completion.py` | 0 | 126 核心样本，58 匹配、68 预期差异、0 失败 | 6.418 |
| `check_goland_definitions.py` | 0 | 19 个文件位置一致，1 个 `package-qualifier` 包对象差异 | 6.741 |
| `check_goland_definitions.py --method typeDefinition` | 1 → 0 | 默认 Go 1.25.0 的 `net/http.Request` 源码坐标与 Go 1.27.4 夹具不同，查询前退出。固定 Go 1.27.4 后 34/34，32 文件位置及 2 预期空结果一致 | 0.436 → 1.724 |
| `check_goland_hover.py` | 0 | 21/21，20 匹配、1 预期空、0 差异 | 1.188 |

诊断与代码操作的默认命令读取仓库查询规格。类型定义重跑按 [SDK 固定方式](../goland-type-definition-baseline.md) 将 Go 1.27.4 的 `bin` 放在 `PATH` 首位，设置 `GOTOOLCHAIN=local`。

临时测试驱动复核两个增量用例，2/2 通过，进程总耗时 4.620 s。原命令仍返回 1，需要更新仓库脚本中的两处假设。

- `async-refresh`（2.306 s）。旧驱动等待已被终止的 `go list` relay 写入 `done`。临时驱动用 Win32 进程句柄确认 gate 仍关闭、`done` 未写出时旧进程已退出，从屏障后等待句柄 1.749 ms。最新图精确落点、命令计数、无关包、失败恢复和唯一终态全部通过。[用例](../../tools/workspace_checks/graph_refresh.py) · [命令取消](../../src/workspace/process.rs)
- `completion_cancellation_and_stale_tasks`（1.971 s）。旧驱动假定同修订的两个补全可同时等待，最新请求会立即以 `-32800` 结束旧请求。临时驱动分别验证重复显式取消、同修订取代、编辑失效 `-32801`、重试 `BuildWidget` 及唯一终态。这两项证据支持小修测试驱动。[用例](../../tools/workspace_checks/completion.py) · [补全调度](../../src/scheduler/completion.rs)

另用 Go 1.27.4 执行实现及父类型导航真实语料，合成工作区 29/29 精确一致（0.794 s）。共同工作区引用语料含两种 includeDeclaration 配置，34/34 精确一致（1.621 s）。

#### 保存、加载和实现候选

- Release 临时工作区连续 12 次正文保存，间隔 85 ms，每次发送 `didSave` 及 watcher 通知并验证当前词法落点，12/12 精确一致。基线建立后记录 `cold_file` = 0、`go list` = 0（命令中继和日志各自核对）。人为暂停 Go 1.27.4 命令后，旧 generation 的命令在 262.434 ms 取消，旧结果安装为 0，仅 generation 2 安装，跨包目标一致。
- 独立 `go.work` 夹具中初次加载执行 1 次 `go env`，相同有效输入下连续 3 次工作区图装载和预热均为 0，更改 `go.work` 内容后执行 1 次。开关文件、结构变更及强制全量装载对拍沿用前述专项结果。
- P0.4 在 Go 1.24.3 临时工作区顺序应用 6 次编辑，逐项用独立新进程、同一覆盖层的首次倒排构建对拍 URI 与完整 UTF-16 范围。6/6 精确一致。函数体、普通注释的增量 `postings_build` 为 0 · 局部类型、import 绑定、签名、损坏语法各为 1。独立进程每例均构建 1 次，换绑导致 `Receiver` 消失，匹配签名后恢复。脚本与结果位于 `%TEMP%\rs-gopls-p24-postings-20261128`。


#### 原始实况基线重算

对 13,554 行原始脱敏 JSONL 复核 SHA-256 `1969de96d14dd2369951c672b032b8f0495daaa9c65d7e93210f6688cf956d69`，用 `(N−1)p` 线性插值重算。此日志用于定位现场长尾。原会话的二进制哈希、完整请求及编辑序列未保留，受控重放另用可识别的发布版做同负载比较。

| 事件 | N | P50 / P95 / Max（ms） |
| --- | ---: | ---: |
| Code Action RPC 总耗时 | 356 | 188.276 / 4,682.471 / 11,905.492 |
| Code Action RPC 等待 | 356 | 188.200 / 4,682.299 / 11,905.017 |
| 诊断 Pull RPC 总耗时 | 465 | 31.155 / 2,310.364 / 7,210.473 |
| 补全 RPC 总耗时 | 92 | 237.946 / 2,177.234 / 2,936.947 |
| 补全 `-32801` 同步执行 | 63 | 379.395 / 1,834.795 / 2,761.026 |
| Stage 2 `queue_wait_ms` | 156 | 0.024 / 1,258.350 / 6,182.605 |
| Stage 2 `runnable_queue_wait_ms` | 156 | 0.023 / 11.871 / 6,182.605 |
| 服务端应用编辑至同版本首条完整 Stage 2 写出 | 25 | 65.031 / 6,408.713 / 14,341.272 |
| Stage 2 worker 退出至完整报告写出 | 40 | 0.380 / 2.605 / 966.547 |
| Stage 2 语义分析 | 21 | 6,322.294 / 6,574.959 / 6,714.886 |
| Stage 2 `declarations_ms` | 21 | 1,059.643 / 1,100.091 / 1,224.693 |
| Stage 2 `unresolved_ms` | 21 | 1,733.736 / 1,797.980 / 1,822.944 |
| Stage 2 `types_ms` | 21 | 3,530.173 / 3,631.020 / 3,698.545 |

语义分析样本为有访问节点且正常完成的 Stage 2 任务。原始会话补全的同步执行时间占 RPC 总耗时 92.036%，63 次 `-32801` 累计同步执行 37,786.914 ms。Stage 2 取消 113/156 次，分析消耗 111,161.302 / 236,360.090 ms（47.03%）。186 条 `didChange` 记录均无通知排队字段。


#### Release 混合负载 · 阶段、局部修复与补全

同机对比发布版 `target\rs-gopls-m2-phase4-532df9a7.exe`（`0.1.0`，SHA-256 `532df9a755ca15759a64d50da5a84d5a6627492d9e2c06408e76063a51ff01b1`）。固定 Go 1.27.4 windows/amd64、`GOTOOLCHAIN=local`、`GOENV=off`、`GOWORK=off`、`GOPROXY=off`、`CGO_ENABLED=0`，每会话新建临时 Go 模块，其中依赖包与诊断包各 90 个兄弟文件。四个生成诊断目标覆盖泛型、嵌入、别名和三档访问节点。由仓库 Python stdio 客户端驱动，启用诊断 Pull/refresh 与 watcher。交错编辑、连续诊断 Pull 和代码操作，穿插冷/热补全、编辑中止、定义、Hover、Resolve。连续保存与 watcher 使用独立的命令夹具。正式回放轮换 8 个新进程，每进程 10 轮，候选关/开遥测各 30 轮，对照开遥测 20 轮。接收时间取客户端读取并解码完整响应时刻，服务端 RPC 与分段取遥测。分位数均为线性插值。

候选 60/60 个补全编辑打断均返回 `-32801`，240/240 个相邻轻导航目标正确，QuickFix 和 Organize Imports 各应用 120/120 次并核对正文，60/60 个最终版本取得完整报告。候选开日志的 30 轮中，两类动作各 30 次在同版本 Stage 2 完整报告前返回，其余对应版本在报告完成前已被后续编辑取代。两版均未观察到重复终态，候选开日志窗口内过期报告交付为 0。报告安装校验另由前述调度测试覆盖。

| 服务端或客户端指标 | 对照开日志 N · P50 / P95 / Max（ms） | 候选开日志 N · P50 / P95 / Max（ms） |
| --- | ---: | ---: |
| Code Action RPC | 80 · 36.468 / 247.368 / 265.242 | 120 · 5.274 / 19.701 / 23.428 |
| Code Action RPC 等待 | 80 · 36.431 / 247.316 / 265.183 | 120 · 0.723 / 10.020 / 11.693 |
| Stage 1 可运行队列等待 | 163 · 0.484 / 83.124 / 238.107 | 262 · 3.357 / 22.031 / 51.165 |
| Stage 2 总排队 | 145 · 0.052 / 217.229 / 238.013 | 184 · 159.452 / 204.077 / 217.151 |
| Stage 2 合并等待 | 145 · 0 / 0 / 0 | 184 · 148.774 / 170.847 / 174.253 |
| Stage 2 可运行队列等待 | 145 · 0.052 / 217.229 / 238.013 | 184 · 9.100 / 61.242 / 75.680 |
| Stage 2 取消至退出 | 74 · 0.645 / 8.213 / 9.087 | 54 · 0.711 / 2.634 / 2.997 |
| 最终编辑发送至完整报告接收 | 20 · 167.663 / 346.770 / 391.510 | 30 · 271.047 / 548.987 / 553.832 |
| 全部补全 RPC | 44 · 2.542 / 6.853 / 80.771 | 66 · 22.389 / 68.286 / 105.468 |
| 冷补全 RPC | 2 · 79.189 / 80.613 / 80.771 | 3 · 105.181 / 105.439 / 105.468 |
| 编辑后热补全 RPC | 20 · 3.871 / 6.196 / 6.977 | 30 · 55.169 / 67.416 / 68.529 |
| 相邻定义 RPC | 80 · 0.197 / 0.581 / 1.357 | 120 · 0.447 / 6.669 / 9.393 |
| `didChange.dispatch_queue_ms` | 字段未记录 | 360 · 0.094 / 19.593 / 30.536 |
| 诊断 worker 结果就绪至主循环选中 | 字段未记录 | 446 · 0.040 / 82.099 / 109.090 |
| 诊断 worker 退出至关联响应写出 | 172 · 26.348 / 42.337 / 86.326 | 177 · 30.917 / 126.403 / 138.866 |
| Stage 2 worker 退出至完整响应写出 | 42 · 0.141 / 0.272 / 19.763 | 57 · 0.458 / 26.841 / 39.376 |

候选 Stage 1 有 42 次小文件任务在 Stage 2 仍占槽时开始，22 次报告在重叠的 Stage 2 退出前写出，Stage 2 并行峰值为 2。Stage 1 合并等待 P95 为 24.030 ms。Stage 2 取消比例从 74/145（51.03%）降为 54/184（29.35%），取消任务分析时间占比从 5,846.440 / 10,239.664 ms（57.10%）降为 6,014.208 / 16,626.083 ms（36.17%）。在能关联编辑派发与 Stage 2 启动的 184 个样本里，最小间隔为 200.252 ms，P50 / P95 / Max 为 209.109 / 261.245 / 275.681 ms。候选最终编辑至完整报告 P95 增加 202.217 ms，P0.2 的端到端条件未达。

补全同步执行占 RPC 总时长从 50.859% 降为 0.662%。`-32801` 样本候选 30 次，同步累计 3.747 ms、RPC 累计 13.585 ms，分别为每次 0.125 / 0.453 ms · 对照 20 次分别为 2.309 / 6.877 ms，均值 0.115 / 0.344 ms。候选取消到 worker 退出 P50/P95/Max 为 0.011 / 0.746 / 0.896 ms。相邻定义队列 P95 从 0.526 升至 6.469 ms。补全热路径 worker P95 为 60.806 ms，结果队列 P95 为 0.036 ms。P2.4 的同负载补全 RPC、取消开销及相邻定义尾延迟未达下降标准。

#### Stage 2 分段与共享声明

只统计正常完成且确实访问语义节点的诊断，每档为 N 与 P50 / P95 / Max，单位 ms。对照与候选使用相同生成文件和请求顺序，各版本完成/取消时机影响 N。

| 访问节点 | 版本（开日志） | N | 整轮分析 | `declarations_ms` | `unresolved_ms` | `types_ms` |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 3,727 | 对照 | 10 | 20.169 / 25.368 / 26.300 | 8.097 / 11.028 / 11.106 | 2.959 / 4.662 / 4.709 | 7.353 / 9.132 / 9.167 |
| 3,727 | 候选 | 15 | 40.278 / 42.258 / 42.708 | 16.941 / 25.662 / 25.971 | 4.532 / 5.664 / 5.673 | 14.719 / 17.473 / 18.135 |
| 17,787 | 对照 | 10 | 66.253 / 82.653 / 86.879 | 12.244 / 13.750 / 13.763 | 36.689 / 46.978 / 49.408 | 12.592 / 16.292 / 17.220 |
| 17,787 | 候选 | 15 | 92.169 / 106.346 / 107.680 | 17.200 / 20.444 / 21.920 | 44.397 / 54.358 / 54.963 | 22.220 / 24.172 / 24.312 |
| 36,287 | 对照 | 18 | 186.148 / 237.001 / 237.751 | 16.396 / 19.442 / 20.001 | 135.198 / 175.783 / 177.315 | 20.329 / 28.562 / 28.589 |
| 36,287 | 候选 | 40 | 214.648 / 258.910 / 295.601 | 22.236 / 26.702 / 29.329 | 148.766 / 183.894 / 218.560 | 29.125 / 34.542 / 35.942 |

P1.2/P1.3 的包索引和 memo 隔离对拍保留原结果。本轮开日志的发布版整轮与分段中位数均高于同负载对照，需确认开销及并发差异后才能确认发布版性能。Stage 2 的声明缓存 21,228 次命中、223 次加入在途和 270 次请求生产。Stage 1 命中 15,268 次，补全命中 33 次、Resolve 命中 6 次。两版诊断的 `cold_dep_reads` 均为 0，`cold_dep_parses` 包含声明生产与加入在途。


#### 遥测关联、脱敏和开关开销

候选三份开启日志共 238,023 条事件，丢弃为 0。逐会话比对 `rpc_recv`、`rpc_done` 与 wire ID，缺失为 0。诊断的 start、stages、done、result 按 `(job, run)` 对拍，无差集。共享事件检查 `req_id`、`(job, run)` 以及生产、加入与等待的 `flight_id` 引用，断链为 0。自然后台事件保留自己的关联上下文。360/360 条 `didChange` 有 `dispatch_queue_ms` 并匹配文件和版本，其中 27 条为同版本帧。扫描三个会话的临时路径、文件名、源码标记和应哈希字段，原值泄漏为 0。默认关闭行为由 stdio 专项及关日志回放覆盖。

| 事件 | 关联 RPC | 关联诊断任务 | 独立后台 |
| --- | ---: | ---: | ---: |
| `shared_disk_lookup` | 1,509 | 58,710 | 1,596 |
| `shared_disk_flight` | 2,471 | 810 | 4,143 |
| `shared_disk_wait` | 871 | 493 | 1,508 |
| `shared_disk_mutex` | 3,774 | 130,996 | 4,358 |
| `shared_disk_lru` | 9 | 21,450 | 0 |
| `completion_worker` | 66 | 0 | 0 |
| `completion_profile` | 66 | 0 | 0 |
| Hover 分段 | 6 | 0 | 0 |
| Resolve 分段 | 6 | 0 | 0 |

Hover 分段涵盖 `anchor/symbol/package_doc/markup`，Resolve 涵盖 `rev_check/detail_doc/import_edit`。仅解析文档的 Resolve 查询全部匹配预期，本轮未产生 `full_ast` 解析产物。候选跨请求声明总命中 36,538 次，其中 Hover 3 次。成功生产 `declarations` 的 `body_elided` 39 次、`full` 模式 540 次，后者为无函数体文件。两版 Stage 2 的 `cold_dep_parses` P95 均为 0、最大为对照 90 / 候选 85，旧版缺少按产物拆分的日志，完整 AST 下降量不能用此字段计算。

按候选开关三对轮换会话统计相同请求的客户端 stdio 端到端耗时。单位为毫秒。差值以开启减关闭计算，三次冷启动的差值受样本量限制。

| 请求或区间 | N（各组） | 关闭 P50 / P95 / Max | 脱敏开启 P50 / P95 / Max | P95 差值 |
| --- | ---: | ---: | ---: | ---: |
| 冷补全 | 3 | 102.424 / 103.951 / 104.121 | 105.470 / 105.896 / 105.944 | +1.945 |
| 编辑后热补全 | 30 | 55.807 / 63.784 / 65.510 | 55.541 / 67.816 / 69.035 | +4.032 |
| 编辑中止的补全 | 30 | 11.986 / 27.187 / 31.515 | 12.594 / 27.488 / 29.865 | +0.301 |
| 相邻定义 | 120 | 36.942 / 58.190 / 69.007 | 37.780 / 57.915 / 64.441 | −0.275 |
| 小文件语法 Pull | 30 | 119.811 / 150.027 / 151.836 | 170.458 / 216.571 / 219.157 | +66.544 |
| Stage 2 活动期间的小文件 Pull | 30 | 74.323 / 89.882 / 92.691 | 75.703 / 89.680 / 93.856 | −0.202 |
| QuickFix | 60 | 39.187 / 56.614 / 60.392 | 38.516 / 53.392 / 56.330 | −3.222 |
| Organize Imports | 60 | 45.260 / 60.311 / 68.999 | 43.914 / 59.604 / 64.210 | −0.707 |
| 最终编辑至完整报告 | 30 | 264.939 / 500.174 / 541.229 | 271.047 / 548.987 / 553.832 | +48.813 |

关闭与开启遥测的 30 个最终编辑区间累计分别为 8,925.795 / 9,270.402 ms，相差 3.86%。从首个冷请求发送至最后一个热响应解码的各组会话中位为 7,033.138 / 7,211.673 ms（+2.54%，各 N=3）。以上为三对会话的观测差值，含运行间变异。

混合回放入口为 `%TEMP%\rs-gopls-p24-mixed.py` 和 `rs-gopls-p24-analyze.py`。正式输出位于 `%TEMP%\rs-gopls-p24-mixed-release` 的 `result.json`、`mixed-wire.json`、三组候选及两组对照 JSONL、watcher 与 Go 环境小夹具日志。


#### 双方关闭遥测的补全复测

同负载补测对照版关闭遥测两会话、每会话 10 轮。候选关闭遥测仍使用正式回放的三会话。端到端单位为 ms。

| 指标 | 对照关闭 N · P50 / P95 / Max | 候选关闭 N · P50 / P95 / Max |
| --- | ---: | ---: |
| 冷补全 | 2 · 78.733 / 79.009 / 79.040 | 3 · 102.424 / 103.951 / 104.121 |
| 编辑后热补全 | 20 · 4.474 / 6.534 / 9.224 | 30 · 55.807 / 63.784 / 65.510 |
| 最终编辑至完整报告 | 20 · 558.397 / 802.505 / 888.276 | 30 · 264.939 / 500.174 / 541.229 |

关闭时对照两会话最终编辑至完整报告的 P95 约为 474 和 848 ms，开启时的版本方向相反。P0.2 仍缺可重复的尾延迟改善。关日志时补全编辑打断与相邻定义 P95 分别为对照 36.431 / 63.601 ms、候选 27.187 / 58.190 ms。编辑使补全过期的 30 条通知的派发 P50 / P95 / Max 为 0.100 / 0.123 / 0.156 ms。

再以无并发诊断、两版均关遥测的八个会话对拍补全，逐次比较有序候选 label、sortText 与 textEdit 哈希，全部一致。

| 未打开兄弟文件数 | 补全阶段 | N（各组） | 对照 P50 / P95 / Max | 候选 P50 / P95 / Max |
| ---: | --- | ---: | ---: | ---: |
| 6 | 首查 | 2 | 27.217 / 28.896 / 29.083 | 92.772 / 97.087 / 97.566 |
| 6 | 重复 | 30 | 3.016 / 3.758 / 3.971 | 38.208 / 45.868 / 49.082 |
| 6 | 编辑后 | 30 | 3.009 / 4.260 / 4.458 | 38.939 / 51.629 / 93.548 |
| 180 | 首查 | 2 | 33.203 / 33.396 / 33.418 | 97.482 / 101.620 / 102.079 |
| 180 | 重复 | 30 | 3.192 / 8.358 / 12.977 | 42.121 / 46.494 / 49.735 |
| 180 | 编辑后 | 30 | 3.255 / 3.983 / 4.469 | 40.763 / 45.773 / 50.614 |

候选热补全的 `plan_batch` P50 / P95 / Max 为 47.851 / 58.097 / 58.512 ms，占 worker 大部分耗时。该路径逐候选文件调用 `cached_completion_names`，缓存命中时仍经 `FileNames::matches_disk` 与 `FileObservation::read` 读取元数据。无编辑复测仍再现长尾，表明逐文件重验是修复切入点，需保持 watcher 失效及无通知改盘时的正确性并通过针对性对拍确认贡献。包 Hover 另观察到 `package_doc` 中位 27.638 ms，包级候选来源校验的重放值得一并核查。[导入规划](../../src/semantic/completion/imports.rs#L890) · [索引检查](../../src/semantic/index.rs#L1521) · [元数据](../../src/semantic/shared_disk.rs#L148)

关日志对照与隔离补全数据位于 `%TEMP%\rs-gopls-p24-previous-off` 的 `result.json`、`completion-ab-summary.json` 和各轮 wire 数据。

#### 运行中 GoLand · 定义与类型定义

GoLand 2026.3.0.1、`GO-263.7307.354` 的 `ideScript` 入口将采集脚本转发到已打开 `localchat/server` 的可见 IDE（PID 2544）。脚本确认 Smart Mode、Go 1.27.4 SDK 和活动页 `internal/api/api.go`。双方固定上述工作区修订，纳入测试视图与可达项目/SDK/模块依赖。Go 环境为 Windows amd64、`GOWORK=off`、`GOFLAGS` 空、`CGO_ENABLED=1`。Go 源与模块清单指纹 `f780a68597af57ff9cab0051fd484fc51d44466d4801e7c185986702610ed6a4` 在测量前后相同。

合并定义与类型定义语料的 54 个查询光标，在相同 10 个文件中逐个调用两种导航方法。GoLand 使用关联文件的只读编辑器视图。rs-gopls 关闭遥测，在 `didOpen` 同一组文件与 `workspace/symbol Sanitize` 验证图就绪后开始。全组先首查，再每例预热 5 次并重复 30 次。编辑档逐例在文件首行插入未保存注释 `// rs-gopls benchmark overlay`，查询后撤回并核验恢复结果。对同文件落点校正编辑引起的一行位移，比较完整目标集合及零基 UTF-16 标识符范围。GoLand 逐文件校验磁盘字节及修改时间。

GoLand 计时覆盖导航动作的目标计算与范围提取，排除 Smart Mode 读操作获取、锚点定位、编辑的 PSI 提交和 UI。rs-gopls 计时覆盖 LSP stdio 请求发送到响应解码，编辑档可能包含尚在队列中的 `didChange` 处理，编辑发送在计时外。首查使用已就绪索引与系统缓存，重复和编辑档沿用同一会话。未测启动和工作集。双方共配对 4,104 次，含预热及撤回，3,762 次精确一致，342 次来自下列 9 个方法/样本的稳定差异，RPC 错误 0。只用全程一致样本计算下表，分位按 `(N−1)p` 线性插值，单位 ms。

| 方法 | 阶段 | N | GoLand P50 / P95 / Max | rs-gopls P50 / P95 / Max |
| --- | --- | ---: | ---: | ---: |
| 定义 | 索引就绪后首查 | 49 | 1.055 / 16.141 / 827.450 | 0.189 / 17.202 / 26.138 |
| 定义 | 重复 | 1,470 | 0.252 / 0.544 / 2.276 | 0.165 / 0.283 / 1.605 |
| 定义 | 编辑后 | 49 | 16.218 / 56.756 / 95.245 | 0.637 / 1.784 / 1.864 |
| 类型定义 | 索引就绪后首查 | 50 | 0.737 / 13.249 / 234.015 | 0.203 / 12.044 / 182.743 |
| 类型定义 | 重复 | 1,500 | 0.182 / 0.383 / 39.341 | 0.194 / 0.357 / 1.492 |
| 类型定义 | 编辑后 | 50 | 1.352 / 30.567 / 37.907 | 0.258 / 1.109 / 1.331 |

定义首查的 rs-gopls P95 高 1.061 ms，重复与编辑后 P95 均较低。类型定义三档 P95 均较低。

| 方法 | 样本 ID | GoLand | rs-gopls |
| --- | --- | --- | --- |
| 定义 | `package-qualifier`、`import-qualifier` | `gorilla/mux@v1.8.1` 包对象 | `internal/api/api.go [10:1,10:25)` 导入字面量 |
| 定义 | `context-pointer-binder` | `internal/api/api.go [206:2,206:3)` | 空 |
| 定义 | `package-clause` | `internal/api/api.go [0:8,0:11)` | 空 |
| 定义 | `blank-identifier` | `internal/api/api.go [155:11,155:12)` | 空 |
| 类型定义 | `writeerror` | `internal/api/context.go [107:15,107:16)` | 空 |
| 类型定义 | `inituser` | `internal/api/user.go [11:24,11:25)` | 空 |
| 类型定义 | `send` | `internal/web/websocket.go [62:23,62:24)` | 空 |
| 类型定义 | `range-member` | `internal/model/user.go [102:23,102:24)` | 空 |

上述差异与前期现场采集集合相同。原始采集、回放和逐样本对照存于 `%TEMP%\rs-gopls-p24-navigation-20261128-174344`，入口为 `capture.groovy` 与 `compare.py`，报告为 `goland.json`、`sample-comparison.json`、`summary.json`。

#### 同一 GoLand 进程的补充行为采集

继续用 PID 2544 和 Go 1.27.4 执行仓库采集器。导航表中的时间只取前述定义与类型定义查询。合成 IDE 工程在临时目录打开，Release 检查按语料需要选用仓库的只读固定根；真实项目沿用共同工作区。

| 语料 | GoLand 本轮采集 | 候选 Release 对拍 |
| --- | --- | --- |
| Hover | 21 例、源 token 的 UTF-16 范围与归档一致 | 21/21 通过，`hover-pkg-qualifier`、`hover-import-path`、`hover-embedded-field-decl` 为已记录的呈现差异 |
| 诊断 | 11 文件、24 个高亮。当前 IDE 使用中文消息 | 英文正则的原检查器 2/11。临时 adapter 把中文消息映射到既有语义码并保留原范围后 11/11，6 个已记录差异 ID 为 `missing-paren`、`missing-expression`、`self-import`、`invalid-import`、`declarations`、`types`，依次涉及 1、1、3、2、2、2 条 |
| 代码操作 | 4 个格式化与整理导入查询 | 9/9，含独立的 5 个 Release QuickFix。整理导入的两项既有差异为 `organize-imports`、`organize-cgo-imports`，各 1 项 |
| 合成补全 | 126 例、3,152 个候选。前缀、替换范围和接受后正文与归档一致 | 用固定夹具根重跑为 125/126 核心约束，`keyword-range` 受本轮 IDE 候选变化影响，有 11 个建议差异，对照版同样复现 |
| 真实项目补全 | 22 例、1,073 个候选，候选列表与归档相同 | 新鲜及归档语料均为 19/22，`local-inferred`、`local-same-package`、`local-edit-type` 分别缺 129、3、5 个期望建议，对照版同样复现 |

合成 GoLand 原始候选发生变化的 ID 为 `selector-eof`、`selector-before-paren`、`shadow-local-package`、`selector-type-method`、`selector-pointer-method-expression`、`keyword-range`。用临时克隆根直接跑 Release 时还有 `package-production` 和 `package-test` 两个失败，归档语料可在同一克隆根复现，固定夹具根通过。补充采集、诊断本地化映射、前后版本对照输出保留在 `%TEMP%\rs-gopls-p24-navigation-20261128-174344\supplement`。

采集后 IDE 仅保留原共同工程和活动编辑器 internal/api/api.go，仍处于 Smart Mode。项目子树的 Git 状态为空。

#### 验收结论与恢复条件

- P0.1、P0.3、P0.4 通过本轮发布验收。P1.1、P2.1–P2.2、P2.3 的既有正确性验证在本轮结果中得到支持。GoLand 现场导航的既有差异集合稳定。
- P0.2 的阶段隔离、生效防抖和取消行为通过，最终编辑到完整诊断及完整报告交付的延迟未稳定低于同负载对照。P1.2/P1.3 的隔离夹具仍通过，因本轮发布版开日志下声明、未解析、类型与整轮分析分段均慢于对照，重新打开其性能验收。
- P2.4 遥测事件、关联、默认关闭与脱敏通过，补全端到端尾延迟、部分相邻请求与诊断交付尾延迟存在明确回退。发布收尾需要修正工作区回放两处测试驱动，保持默认语料与补充现场差异记录，对补全导入规划与诊断交付开展定向对拍，取得稳定且满足门槛的同负载结果。
- 计划状态为「受阻」，更新日期 2026-11-28，完成日期待填写。本次仓库交付范围为计划文档，采集与回放产物留在临时目录，沿用此前通过的 Cargo 结果。

### 2026-11-28 · P2.4 阻塞修复与复验

- **补全**。Windows SDK 名称筛选按 `src` 子树的内容变更代次复用。Windows 项目文件在同一次查询内批量读取同目录元数据，失效、重解析点和读目录失败按文件严格校验。限定冷扫描并行预取命中的声明，按原候选顺序消费。新增回归覆盖无通知改盘、`didSave` 和原生监听重挂接。初测初始冷命中曾串行生产 11 份声明，`file_index` 累计 41.7 ms。热补全的快照捕获和安装各不超过 0.015 ms，主要回退来自 `plan_batch` 的逐文件校验。[规划与预取](../../src/semantic/completion/imports.rs) · [磁盘观察](../../src/semantic/completion/disk_observation.rs)
- **Stage 2 与交付**。旧回放的一个完整报告在 worker 发布后等了 25.55 ms，占到写出的 25.76 ms 中的绝大部分。同轮大型全量文本 `didChange` 占用主循环，后续 Pull 的派发队列为 32.55 ms。全量编辑现以最小 UTF-8 变更范围更新语法树；局部导入操作在可证明使用关系时直接完成，作用域不确定时走完整校验。诊断按快照批量核查兄弟文件并共享已验证的声明，泛型导入调用复用已有词法绑定，避免每次回扫前序语句。旧回放的一次热诊断记有 181 次文本与声明命中各一组、1,087 条锁事件及 181 条重复包名覆盖事件，锁等待合计仅 0.056 ms。[增量语法树](../../src/semantic/source.rs) · [局部导入](../../src/semantic/diagnostics/imports.rs) · [类型检查](../../src/semantic/diagnostics/types.rs)
- **遥测与驱动**。热命中和锁等待汇总到 RPC、补全任务、诊断 `(job, run)` 或冷 `flight_id`，保留 producer/join 的在途边及解析和实际淘汰事件。`async-refresh` 在闸门关闭时用进程句柄验证旧代次退出及新代次交付。补全回放用受控图加载分别验证重复取消、同修订替换、编辑失效与唯一终态。[共享缓存](../../src/semantic/shared_disk.rs) · [汇总](../../src/telemetry.rs) · [工作区回放](../../tools/workspace_checks/graph_refresh.py)

第一轮正式回放二进制 `c1f41ca2…` 在匹配的候选三组关/开日志会话中，最终编辑到完整报告 P95 为 324.993 / 325.479 ms，同轮对照为 810.483 / 619.845 ms。热补全关日志 P95 8.702 ms，仍高于对照 6.206 ms；继续按 SDK 包、完整匹配条件和监听代次复用已筛文件。开日志的终端完整报告首次写出 P95 0.655 ms，其中主循环选中后的处理及写出是主要开销；进一步缓存遥测路径哈希并复用同一派发周期的已验证打开文档修订。17.8k 档 `types_ms` 两个长尾样本同时遇到大文件并发分析耗时近翻倍，各样本命中数相同、锁等待低于 0.1 ms。后续缩短了仅声明新单变量的 `:=` 分支，交由已有调用表达式检查验证右侧参数。[筛选缓存](../../src/semantic/completion/imports.rs) · [诊断交付](../../src/scheduler/diagnostics.rs)

#### 最终 Release 混合回放

`target/release/rs-gopls.exe` · `0.1.0` · SHA-256 `6b0e9ac98744dc73174083fa7dfe395a907bcb699cd0fcc5cb22e681c894d6a8`。按前一轮的 Go 1.27.4、模块生成、请求、编辑、计时和线性插值口径，轮换候选开/关各 3 个进程和 `532df9a7` 对照开/关各 2 个进程，每进程 10 轮。客户端计时取完整响应解码，服务端字段由开日志会话采集。冷补全每进程一次。原始数据为 `%TEMP%\rs-gopls-p24-final-replay4` 下的 `mixed-wire.json`、`final-analysis.json`、五份 JSONL 和 watcher 记录。

| 指标 P95（ms） | 对照关闭 | 候选关闭 | 对照开启 | 候选开启 | 候选开启 − 关闭 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 冷补全客户端 | 78.540 | 63.446 | 81.156 | 67.794 | +4.348 |
| 编辑后热补全客户端 | 5.968 | 6.467 | 5.207 | 6.525 | +0.058 |
| 编辑中止补全客户端 | 28.750 | 1.827 | 30.615 | 1.855 | +0.028 |
| 相邻定义客户端 | 49.220 | 2.465 | 51.731 | 2.482 | +0.017 |
| 小文件语法 Pull | 343.636 | 73.363 | 208.072 | 71.828 | −1.535 |
| Stage 2 活动期间小文件 Pull | 261.781 | 80.045 | 84.692 | 84.861 | +4.816 |
| QuickFix 客户端 | 518.156 | 0.865 | 337.872 | 0.941 | +0.076 |
| Organize Imports 客户端 | 518.091 | 0.878 | 337.819 | 0.924 | +0.046 |
| 最终编辑到完整报告客户端 | 600.826 | 320.626 | 406.743 | 324.831 | +4.205 |
| 全部补全 RPC | — | — | 4.890 | 6.302 | — |
| 热补全 RPC | — | — | 4.775 | 6.055 | — |
| `-32801` RPC 同步执行 | — | — | 0.141 | 0.247 | — |
| 相邻定义 RPC | — | — | 0.562 | 0.376 | — |
| 相邻定义 RPC 派发队列 | — | — | 0.464 | 0.228 | — |
| Code Action RPC / 等待 | — | — | 290.675 / 290.554 | 0.308 / 0.214 | — |
| Stage 2 总排队 / 可运行排队 | — | — | 218.401 / 218.401 | 185.926 / 18.818 | — |
| Stage 2 退出至首次完整写出 | — | — | 0.250 | 0.252 | — |
| Stage 2 退出至每次完整写出 | — | — | 3.716 | 0.252 | — |

热补全 `plan_batch` P95 从旧候选 58.097 ms 降到 3.422 ms，worker P95 为 5.529 ms。正式回放中测试图首次安装热点由约 1.5–1.6 ms 降为约 0.27 ms。补全同步执行占比由 52.47% 降为 1.53%，`-32801` 同步中位由 0.127 降为 0.045 ms。候选 60/60 编辑中止返回 `-32801`，240/240 相邻定义目标正确，两类动作各应用 120/120 次，60/60 最终版本取得完整报告。过期诊断写出和重复终态均为 0。Stage 2 取消分析时间占比由 54.81% 降为 9.46%，取消至退出 P95 由 7.341 降为 0.488 ms。

热态仅统计已完成、访问语义节点且本轮 `cold_dep_reads` 和 `cold_dep_parses` 均为 0 的 Stage 2。每格为 P50 / P95，单位 ms。

| 节点 | 版本 | N | 整轮分析 | `declarations_ms` | `unresolved_ms` | `types_ms` |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 3,727 | 对照 | 10 | 19.807 / 20.927 | 9.042 / 9.923 | 2.873 / 2.990 | 6.989 / 7.612 |
| 3,727 | 候选 | 12 | 7.577 / 8.315 | 2.628 / 2.915 | 1.726 / 1.939 | 2.082 / 2.357 |
| 17,787 | 对照 | 10 | 66.633 / 68.902 | 13.323 / 14.301 | 35.027 / 36.653 | 12.786 / 13.990 |
| 17,787 | 候选 | 15 | 27.296 / 29.548 | 6.745 / 8.528 | 7.700 / 8.889 | 7.123 / 8.025 |
| 36,287 | 对照 | 20 | 188.150 / 198.740 | 17.815 / 21.133 | 130.106 / 136.749 | 21.629 / 23.694 |
| 36,287 | 候选 | 30 | 54.378 / 58.820 | 13.139 / 15.368 | 16.204 / 17.366 | 13.701 / 14.705 |

加入各档首次冷生产后，候选三档完成样本 N 为 15 / 15 / 33，分析 P95 为 31.305 / 29.548 / 74.886 ms，声明 P95 为 25.568 / 8.528 / 33.772 ms。冷声明仍影响小档整轮和小、大档声明分位。

三份候选开启日志合计 19,485 条事件，旧候选同轮数为 238,023 条，减少 91.8%。合计记录 46,444 次缓存命中、2,487 次生产请求、372 次加入在途及 7.504 ms 锁等待（36,016 次获取）。逐命中行和逐锁行均为 0。抽取 14,204 个 flight 引用，RPC、诊断 `(job, run)` 和 flight 关联断链均为 0，丢弃和原值泄漏均为 0。诊断小文件 Pull 及最终报告的开日志 P95 差值为 −1.535 / +4.205 ms，原候选分别为 +66.544 / +48.813 ms。

#### 运行中 GoLand 复验

运行中的 GoLand `GO-263.7307.354`（PID 2544）针对同一 LocalChat 工作区、Go 1.27.4 和相同编辑及撤回序列重新采集。对照和候选分别复放全部 4,104 次查询，各 3,762 次目标和 UTF-16 范围精确一致，342 次对应相同的 9 个已知差异，RPC 错误 0。计时以相同范围精确匹配的样本统计 P95，单位 ms。

| 方法 | 阶段 | N | GoLand | `532df9a7` | 最终 Release |
| --- | --- | ---: | ---: | ---: | ---: |
| 定义 | 首查 | 49 | 2.912 | 19.980 | 20.053 |
| 定义 | 重复 | 1,470 | 0.285 | 0.299 | 0.349 |
| 定义 | 编辑后 | 49 | 24.248 | 1.299 | 2.327 |
| 类型定义 | 首查 | 50 | 0.971 | 20.379 | 13.728 |
| 类型定义 | 重复 | 1,500 | 0.222 | 0.323 | 0.445 |
| 类型定义 | 编辑后 | 50 | 16.241 | 0.587 | 1.763 |

本轮现场数据和两版逐样本回放位于 `%TEMP%\rs-gopls-p24-recheck-navigation-180123`，工作区指纹保持 `f780a68597af57ff9cab0051fd484fc51d44466d4801e7c185986702610ed6a4`。

#### 最终回归

| 验证 | 结果 |
| --- | --- |
| `cargo check -p rs-gopls --locked`、`cargo build -p rs-gopls --release --locked`、`cargo fmt --all -- --check`、`git diff --check` | 通过 |
| `cargo test -p rs-gopls --locked` | 492 单元测试、3 CLI 集成测试通过，1 已标记忽略，0 失败 |
| 四模块 `check_workspace_updates.py` | 16/16，退出 0，26.610 s，两个修订用例均通过 |
| `test_diagnostic_telemetry.py` | 8/8，退出 0，12.102 s 含进程启动与退出 |
| `check_goland_diagnostics.py` / `check_goland_actions.py` | 11/11、9/9，退出 0 |
| `check_goland_completion.py` | 126 核心通过，58 匹配、68 预期差异、0 失败，退出 0 |
| `check_goland_definitions.py` | 定义 19 文件落点及 1 既有包对象差异；Go 1.27.4 类型定义 32 文件落点及 2 预期空，均退出 0 |
| `check_goland_hover.py` | 21/21，退出 0 |

最终 Release 复验 12 次普通正文保存，`cold_file` 和 `go list` 均为 0。Go 环境夹具的 `go env` 次数为首次 1、相同设置下后续三轮各 0、修改 `go.work` 后 1。实现候选用全量重建对拍 6/6，正文和注释 `postings_build` 各为 0，其余 4 项各为 1。验证输出及中间版本的统计摘要在 `%TEMP%\rs-gopls-p24-final-validation`。

#### 恢复条件

- P0.2 的最终报告、可运行排队和每次完整写出达标，P1.2 和 P1.3 的三档热态整轮与分段达标。首次完整写出从旧候选 26.841 ms 恢复到 0.252 ms，与本轮对照 0.250 ms 相差 0.002 ms。
- P2.4 仍为受阻。关日志热补全达到旧记录的 6.534 ms 门槛，本轮关/开日志热补全及服务端补全 RPC P95 仍高于同轮对照。`-32801` 同步 P95 及现场重复导航和编辑后导航 P95 也高于保留版。恢复条件为按相同口径取得这些尾延迟的稳定对照，完成日期待填写。

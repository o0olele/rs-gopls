# 实时诊断与引用查找尾延迟优化

| 项目 | 内容 |
| --- | --- |
| 状态 | 进行中 |
| 创建日期 | 2026-11-27 |
| 用户确认 | 已确认（2026-11-27 确认优先落地 Milestone 1 Phase 1+2 及分阶段降级、防闪烁和首轮默认参数。2026-11-28 确认实施 Milestone 2 Phase 3 的步骤 6、7 和 Phase 4 的步骤 8，并按独立审查修复 4 项 P1、4 项 P2，重建发布版后正式复测） |
| 更新日期 | 2026-11-28 |
| 完成日期 | 待填写 |

## 目标与范围

- **Milestone 1（优先执行，Phase 1 + Phase 2）**。打字时尽快显示变量名不存在等 `name.unresolved` 及语法错误，消除诊断和引用反复冷读盘、冷解析的共用瓶颈。后台生成大文件正在分析时，小文件编辑和导航仍能响应。先保证 Zed 的 `textDocument/diagnostic`，以相同快照和报告模型兼容后续 VS Code 集成的 Pull 和 `textDocument/publishDiagnostics` Push 模式，按会话能力选择主动交付通道。
- **Milestone 2（后续规划，Phase 3 + Phase 4）**。消除已观察到的引用尾延迟来源，包括成员引用串行加载依赖、首次需要测试视图、文件监听造成的过期重载。贯通版本级诊断遥测，并以运行中的 GoLand `GO-263.7307.354` 建立可重复的对照。
- 完整报告保持现有 Go 绑定、错误分类、UTF-16 范围、QuickFix 输入和引用集合，覆盖未保存覆盖层、构建约束、内外测试包和语法恢复。实时打字路径无需等待外部 Go 编译器或包加载命令，现有后台包图加载继续可用。VS Code 扩展实现和发布、工作区 Pull 诊断接口及 GoLand 专属检查规则均在本计划范围之外。VS Code 的验收范围为实际 VS Code 宿主中使用固定 LSP 客户端接入两种能力配置。
- 待确认两项取舍。第一，只有能可靠获得后一阶段结果的客户端才分阶段交付，Pull 无刷新能力及 Push 无版本支持的客户端采用一次完整报告。第二，Stage 1 对无法证明缺名或无法安全替换已有诊断的版本允许推迟到 Stage 2，优先保证语义与显示稳定。预算、刷新间隔和下列性能门槛为待确认的首轮参数，固定客户端版本实测后记录调整依据。

## 验收标准

### Milestone 1（Phase 1 + Phase 2）

- [x] **跨任务复用与失效范围**。对同一内容同时启动诊断与引用，同一需要完整解析的磁盘内容冷读和冷解析各至多一次，消费者取消后其他仍可用，后续同内容任务各新增 0 次。只编辑函数体时，同包/依赖诊断输入 memo 和引用候选范围继续命中，编辑文档本身的报告、引用落点和已扫描内容按正文修订重算。分别修改 import、签名、隐式推断的包级变量类型、构建约束、测试文件及文件内位置，断言受影响的缓存刷新、独立包继续命中，改回原内容、并发取消、删除/重建和关闭/重开均无旧结果。旧版完整诊断和引用回归逐项通过。
- [ ] **前台延迟与取消**。在约 536 包、2,100 个 Go 文件及含约 210 万 AST 节点的大文件夹具上，打开小编辑文件与后台大文件，普通编辑与连续输入分别采至少 100 次稳定版本。用当前版本的正确 `name.unresolved` 或清除该诊断作为首个有用结果，在 Zed 的编辑到成功 Pull P50 `≤ 80 ms`、P95 `≤ 250 ms`，混合负载最大 `≤ 1 s`。活跃任务排队 P95 `≤ 25 ms`，后台大文件占满期间前台仍可启动。稳定后小文件 Stage 2 交付 P95 `≤ 2 s`。在慢 I/O、解析、声明收集和 AST 步进注入取消，取消信号至停止 P95 `≤ 50 ms`，分别报告不可中断 I/O 的尾部及前台不等旧槽位的证据。稳定版本 `-32801` 与重复回包均为 0。同回放被取消任务消耗的 worker 时间占比较基线减半，绝对值目标 `≤ 11%`。
- [x] **Stage 1 正确性与收敛**。对可证实未解析的裸名、语法错误及修复各核对诊断码、级别和 UTF-16 范围。覆盖词法遮蔽、泛型参数、跨文件声明、点导入、复合字面量键、损坏子表达式旁的有效语句及不可靠顶层签名。阶段之间稳定保留可验证的诊断，Stage 1 新产出不出现误报。Stage 2 与未分阶段的完整报告、对应 QuickFix 一致，包含空报告和关闭清理。
- [ ] **Zed Pull 与 VS Code Push/Pull**。协议回放覆盖刷新有无、双能力、仅 Push、无版本能力及无诊断能力，之后在固定版本 Zed 和 VS Code 宿主验证。Stage 2 在预算内完成、可见内容相同、版本过期、关闭或客户端已拉到终态时，产生 0 次额外阶段更新。需要更新时，Zed 和 VS Code Pull 收到合并刷新，重拉命中完整缓存，服务端缓存查找至报告就绪 P95 `< 1 ms`，持 Stage 1 ID 返回新 `full`，再用终态 ID 返回 `unchanged`。支持版本的 Push 会话收到同版本完整替换通知，最多一次快报和一次有差异的终态。VS Code 的 Push 配置按实际 `versionSupport` 验证对应路径。双能力会话中主动 Push 为 0。加入刷新 ACK 延迟、编辑抢在发送前后、请求乱序、同 LSP 版本依赖变化及关闭后重开用例。每个请求最多一个响应，旧任务写入或反向覆盖为 0，并在实际编辑器记录无重复标记及无清空再重现的闪烁。

### Milestone 2（Phase 3 + Phase 4）

- [ ] **引用集合与性能**。按 [导航性能对照](../PLANS.md#导航性能对照) 对局部、同包、跨包导出顶层符号、跨包成员、接口方法、测试用法逐次核对 `includeDeclaration` 两档的完整去重集合和 UTF-16 范围。增加 Unicode、注释、字符串、子串、别名、语法恢复、隐式名称和未保存编辑用例，确认预筛零漏报及绑定无误报。索引就绪、结果缓存未命中的成员重负载档 P95 `≤ 500 ms`、每个样本 `≤ 1 s`，三个 S3 类慢样本的同机回放相对优化前各至少快 2 倍。固定混合档 P95 `≤ 300 ms`，仓内热查 P95 保持 `≤ 30 ms`，分组同时报告 GoLand 和前序 rs-gopls 数值。
- [x] **测试视图与监听重载**。生产包图就绪后后台预热。在测试视图预热已完成的档位，引用的 `missing_test_view` 等待 0 次。在未完成时发请求，提升或沿用同一在途加载并校验完整结果，不重复启动。固定 15 组 watcher 连发和并发编辑的回放，窗口内每组最多一次新加载，过期安装 0 次，预期无过期重载。窗口外事件独立生效。覆盖文件新增、删除、重命名、签名变化和构建环境变化，确认稳定后的包图与强制全量加载等价。
- [ ] **遥测与 GoLand 对照**。每个记录到的诊断任务能关联 `job`、`doc_version`、`stage`、`queue_wait_ms`、`cancel_lag_ms`、`cold_dep_reads`、`cold_dep_parses` 和脱敏 `diagnostic_code` 计数，区分取消请求和真正退出时间，并可追到有效报告及 RPC 写出。合成 stdio 会话覆盖两种客户端模式、阶段晋级、缓存命中、取消和过期丢弃，校验字段、计数、默认关闭及脱敏。完成 GoLand 现场运行，两端共同样本的每档至少 30 次有效测量，完整记录差异、缓存状态和时间口径。

## 实现依据

### S3 日志基线

直接读取 `C:\Users\rick\Downloads\rs.jsonl.txt`，3,724,355 字节（3.55 MiB）、19,574 行、3 个 `session_start`。S3 从第 4,876 行到文件末尾，共 14,699 行，`started_at_unix_ms=1795788762637`。日志 SHA-256 为 `cf62b1e14622b6d8f974c1f4287e306d7cfe89b4e633ad044c9e43bd384377f5`，分位数使用线性插值。

| S3 指标 | 样本与结果 |
| --- | --- |
| 编辑到首个成功诊断 Pull | 按同文档 `notification` 的 `textDocument/didChange` 到下一编辑前首条 `rpc_done(outcome=ok)` 配对，33 项，P50 **64 ms**、P95 **22.244 s**、Max **23.487 s**。按同文档 `500 ms` 间隔合并输入、只保留各段末次编辑，19 项，P50 **70 ms**、P95 **22.555 s**。当前日志缺少响应的 `doc_version`、`stage` 和诊断码关联，该配对只测成功回包时序。 |
| `type_check`（`types_ms`） | 168 个 `diagnostic_stages` 样本，P50 **1,208 ms**、P95 **7,647 ms**、Max **27,444 ms**，占阶段总耗时 **66.46%**。 |
| `decl_collect`（`declarations_ms`） | 同组，P50 **970 ms**、P95 **1,301 ms**、Max **2,141 ms**，占 **15.91%**。 |
| `unresolved`（`unresolved_ms`） | 同组，P50 **370 ms**、P95 **3,042 ms**、Max **4,422 ms**，占 **17.56%**。`syntax_ms` Max **0.10 ms**。 |
| 取消与大文件 | `diagnostic_worker_done` 中 184/348（**52.9%**）取消，取消样本累计 **255.44 worker·秒（21.9%）**。612 个 Pull 完成事件中 302 个 `-32801`、7 个 `-32800`、303 个成功。`h:5a0844b143b4f51e` 共 7 个 2,106,961 节点样本，类型检查 P50 **24.478 s**。 |
| 工作区重载 | `workspace_load_done` 中 15 次 `stale=true` 全由 `watched_files` 触发。 |
| 引用查找 | 13 个成功 `textDocument/references`，按 `rpc_done.total_ms` 计，P50 **27.14 ms**、P90 **734.81 ms**、P95 **2.382 s**、Max **4.839 s**。以 `references_trace` 关联下列 3 个慢成员查询。 |

| 成员请求 | RPC 耗时 | 冷路径证据 |
| --- | ---: | --- |
| `#755` | 4.839 s | 26 次依赖解析合计 3.701 s，最慢单文件 1.372 s。 |
| `#3196` | 744 ms | 168 次冷依赖读取对应 168 次解析，解析合计 610 ms，读盘另计约 38 ms。 |
| `#91` | 699 ms | `wait_reason=missing_test_view`，首次 `include_tests=true` 的包图加载 642 ms。 |

### GoLand 安装包依据

在 `E:\Apps\goland-2026.3.0.1.win` 核对 `build.txt` 和 `product-info.json`，版本 **GoLand 2026.3.0.1**，构建 **`GO-263.7307.354`**。`plugins/go-plugin/lib/modules/intellij.go.impl.jar` 的 SHA-256 为 `2a13f1a753d5e15db70c5758e17db1dc28cc790e1d329e574d8b6a078d636784`。用安装包 `jbr/bin/java.exe` 和 `lib/intellij.libraries.asm.jar` 的 `org.jetbrains.org.objectweb.asm.util.Textifier` 复核类字节码，将对应 JAR 加到类路径，内部类使用 `$` 拼写。下表中 JAR 路径相对于安装目录。

| JAR | 类、方法和处理流程 |
| --- | --- |
| `plugins/go-plugin/lib/intellij.go.frontback.jar` | `com.goide.parser.GoParser#BlockWithConsume` 先尝试 `GoParserUtil#consumeDeclarationBlock`，必要时回退到 `Block`。 |
| `plugins/go-plugin/lib/modules/intellij.go.impl.jar` | `com.goide.GoCodeBlockElementType#parseContents` 局部解析 `BLOCK`，`com.goide.GoLightStubBuilder#skipChildProcessingWhenBuildingStubs` 跳过 `BLOCK`。Rust 复用已有增量树和声明提取，按 Go 声明与推断语义判定跨文件修订。 |
| `lib/intellij.platform.indexing.impl.jar` | `com.intellij.psi.stubs.StubCumulativeInputDiffBuilder#differentiate` 对比 Stub 序列化内容，等价时避免更新索引。Rust 分离正文修订与包声明语义修订，并保留正文位置映射的独立失效。 |
| `lib/intellij.platform.lang.impl.jar` | `com.intellij.codeInsight.daemon.impl.DaemonCodeAnalyzerImpl#scheduleIfNotRunning` 配合分析重启的编辑合并，`com.intellij.codeInsight.daemon.impl.LocalInspectionsPassFactory` 注册独立检查 pass，可单独调度。`com.intellij.psi.stubs.StubTreeLoaderImpl#readOrBuild` 优先复用索引 Stub。 |
| `lib/intellij.platform.analysis.impl.jar` | `com.intellij.codeInsight.daemon.DaemonCodeAnalyzerSettings#getEffectiveAutoReparseDelay` 给出分析重启延迟，`com.intellij.codeInsight.daemon.impl.GeneralHighlightingPass#collectInformationWithProgress` 按脏范围和优先范围组织分析，`com.intellij.codeInsight.daemon.impl.AnnotatorRunner#runAnnotatorsAsync` 并发执行注解器。Rust 单独调度交互、后台与大文件任务。 |
| `lib/intellij.platform.core.jar`、`lib/intellij.platform.core.impl.jar` | 前者的 `com.intellij.openapi.progress.ProgressManager.checkCanceled()` 委托给后者的 `com.intellij.openapi.progress.impl.CoreProgressManager#doCheckCanceled`，分析循环频繁调用。Rust 按显式快照和检查点取消。 |
| `plugins/go-plugin/lib/modules/intellij.go.impl.jar` | `com.goide.highlighting.GoAnnotator#buildCommonVisitors` 纳入 `com.goide.highlighting.legacyErrorInspections.GoUnresolvedReferenceInspection#visitReferenceExpression` 驱动的检查。`com.goide.psi.impl.GoReference#processUnqualifiedResolve` 按词法块、类型参数、当前文件、命名导入、同包 Stub、内建、点导入短路，`com.goide.psi.impl.GoReferenceBase#getPackageFilePredicate` 从包上下文过滤同包文件。Rust 沿用当前绑定规则，仅提前交付可证实的诊断。 |
| `plugins/go-plugin/lib/modules/intellij.go.impl.jar` | `com.goide.psi.impl.GoNamedElementImpl#getUseScope`、`com.goide.util.GoPathUseScope#contains`、`com.goide.usages.GoUseScopeOptimizer#restrictedScopeForPackage` 和 `com.goide.index.GoImportPathToAliasIndexKt#getImportPathsUsages` 将适用的包级导出顶层符号缩到直接导入文件及声明所在包的候选范围，保留相对导入等特殊路径。字段、方法保守跳过此项缩限。Rust 保留成员、接口与测试变体的保守候选。 |
| `plugins/go-plugin/lib/modules/intellij.go.impl.jar` | `com.goide.usages.GoFindUsagesProvider#getWordsScanner` 按标识符、注释及字符串上下文分类。`com.goide.go.GoInheritorsSearch`、`com.goide.go.GoMethodInheritorsSearch` 使用 `com.goide.stubs.index.GoMethodFingerprintIndex`、`com.goide.stubs.index.GoMethodSpecFingerprintIndex`，从方法名和参数个数预筛接口实现候选，再核验语义。Rust 复用现有包图和类型/声明摘要，以保守条件缩小候选。 |
| `lib/intellij.platform.indexing.impl.jar` | `com.intellij.psi.impl.cache.impl.id.ScanningIdIndexer#map` 与 `com.intellij.psi.impl.search.PsiSearchHelperImpl$TextIndexQuery#toFileBasedIndexQueries` 按词法词项与搜索上下文预筛，排除本场景的注释、字符串与子串误报。`com.intellij.psi.impl.search.PsiSearchHelperImpl` 的 `collectFiles`、`computePriorities` 和 `processFilesConcurrentlyDespiteWriteActions` 按声明文件、同目录和容器名命中组织候选并分批并发验证。Rust 复用现有字节预筛与切片，增加词法倒排和稳定的语义验证。 |

GoLand 字节码是机制依据，编辑器标注和实际查询耗时按运行时对照采集。重点验证未显式标注类型的包级变量，包括多返回值、别名、复合字面量、泛型实例和依赖声明变化。无法稳定证明包级推断类型等价时，推进相关包的语义修订。还需校验构建标签、生成文件大小阈值、测试视图可见性及接口层级搜索选项是否在双方有效。

### 当前 Rust 实现与对应取舍

| 路径 | 当前行为与拟改变的边界 |
| --- | --- |
| `src/semantic/diagnostics/worker.rs:57,116`、`src/semantic/index.rs:39,92,181,721`、`src/semantic/source.rs:186,258,351,378` | 诊断捕获包图和 overlay 修订，每任务用私有 `Engine`、空磁盘索引表。现有 `DiskIndex`、`Arc` 修订及声明解析可复用，`Source` 已支持省略函数体的声明解析、取消和打开文档的增量编辑。现有 `FileIndex::package_signature` 只覆盖包子句、导入与构建约束，用于包图判定。另建需要覆盖声明类型的语义摘要。共享不可变原文、可跨线程的解析产物及摘要，worker 继续持有自己的 `Parser`，可变索引状态留在原所有者。 |
| `src/semantic/diagnostics/inputs.rs:55,98,155`、`cache.rs:16,55`、`src/semantic/snapshot.rs:259`、`src/semantic.rs:265,659` | 报告自身键含正文/版本及包视图，语义输入 hash 包含依赖 overlay 版本和正文指纹，输入 memo 受全局 completion source generation 失效。分离文档正文、声明语义、导入者范围拓扑与位置修订，按照实际依赖和必要的传递边校验。保留测试视图升级后等价输入的既有报告复用。 |
| `src/scheduler/diagnostics.rs:163,235,332,354,427`、`src/semantic/diagnostics/mod.rs:55,222,298` | 当前有界 worker 共享队列，待运行任务按 FIFO 取出，完成后复核快照。当前一次分析内收集声明并交织未解析、未使用、类型检查，AST 主遍历定期检查取消，带错误子树可能整体跳过。按阶段分派，缩短取消间隔，并细化错误恢复。 |
| `src/semantic/diagnostics/unresolved.rs:28,100,179`、`declarations.rs:99,466`、`syntax.rs:9` | 复用现有的词法和包级声明、已知缺名和签名可靠性分支，先在脏函数重查可证实的裸名和语法，选择器、复杂推断及不确定的点导入交给完整分析。避免为 Stage 1 全量读取依赖 AST。 |
| `src/semantic/references.rs:86,287,405,562,624`、`references/candidates.rs:36,54,94`、`references/worker.rs:67,215`、`references/large_file.rs:222,253` | 当前已有候选缓存、字节拼写预筛和按批推进。主路径一次挂起一个准备任务，已完成的准备可回填主索引。大文件验证重建本地 `DiskIndex`/读取器，其内部冷依赖与诊断隔离。让跨任务缓存覆盖两条路径，批量解析和稳定回填。函数体编辑保留 `candidate_scope` 时，查询锚点、结果和参与验证的文件正文仍需复核，覆盖新加入或移动的用法。 |
| `src/main.rs:569`、`src/semantic.rs:408,481`、`src/scheduler.rs:112,148,163,509`、`src/workspace.rs:131,349,544`、`src/semantic/workspace_install.rs:68,173` | watcher 通知到达后逐路径处理并触发工作区失效，无已知摘要的冷文件走包图重载。需要测试视图的请求可能等待加载。现有安装环节核对当前版本。合并 watcher 批次，复用观察和声明签名，预热任务可取消、可提升优先级，安装时仍检查最新输入。 |
| `src/scheduler/diagnostics.rs:172,354`、`src/semantic/diagnostics/mod.rs:390`、`src/rpc_telemetry.rs:48,56,193,241` | 现有 worker 事件有任务编号，阶段日志以路径统计，RPC 已记录接收、等待和写出。增加版本、阶段、有效报告与请求的关联，并把过程开销与编辑器显示时序分别记录。 |

### 当前诊断协议与客户端依据

- `src/main.rs:369–399` 先读取规范的 `capabilities.workspace.diagnostics.refreshSupport`，缺失时兼容 `lsp-types 0.97` 的单数 `workspace.diagnostic`。声明 `textDocument.diagnostic` 且支持刷新时选择 `Refresh`。无 Pull 且有 `textDocument.publishDiagnostics` 时选择 `Publish`。其余会话禁用主动更新。即使客户端同时声明两种能力，Pull 会话也只走 Pull。`src/dispatch.rs:180–190,710–725` 对所有会话声明 `diagnosticProvider`（`identifier=rs-gopls`、`interFileDependencies=true`、`workspaceDiagnostics=false`），接收 `textDocument/diagnostic` 及 `previousResultId`。Pull 无刷新能力仍可主动拉取。
- `src/main.rs:83–132,259,540–676` 和 `src/scheduler.rs:194–253` 在调度器确认新包图被接纳安装或工作区根实际变化时请求 `workspace/diagnostic/refresh`。包图接纳包括可供诊断使用的加载失败信息。刷新使用序列 ID，当前忽略客户端响应且无防抖。open/change/save/close、watch 通知及 worker 完成本身没有直接的 Pull 刷新。watch/save 若引起后续新包图安装，可能间接刷新。Push 模式在打开、编辑、watch 或工作区变化后提交相应的打开文档，关闭时发空数组。`src/scheduler/diagnostics.rs:83–105,354–403,427–480` 的 Push 发送完整列表与快照版本，Pull 精确匹配 `previousResultId` 才返回 `unchanged`。任务完成复核快照和任务票据，过期请求返回 `-32801`，客户端取消返回 `-32800`。
- `zed-extension/src/lib.rs:12–45` 配置服务端进程和工作区配置，诊断能力由 Zed 宿主决定。固定提交 `46375fbf64574f01dc08fd2d2998aa7bbb7f0377` 中，Zed 启用 Pull 时声明文档诊断与 workspace refresh 支持，同时可声明 Push。全局刷新将服务器对应的打开缓冲区加入后台重拉队列，ACK 可先于重拉完成。后台队列按缓冲区去重，各项开始前等待 `100 ms` 并串行等待完成。编辑时 `interFileDependencies=true` 也会触发该服务器其他打开缓冲区重拉。现场分别计量编辑自带和全局刷新引起的 Pull，并记录队首和队尾的 UI 延迟。刷新路径兼容等待重拉完成才 ACK 的客户端。[Zed 初始化能力](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/lsp/src/lsp.rs#L913-L917) · [Zed 刷新处理](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L1231-L1261) · [后台间隔](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L180) · [后台重拉队列](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L5318-L5364) · [编辑与全局刷新触发](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/project/src/lsp_store.rs#L14521-L14575)
- VS Code 的 `vscode-languageclient` 10.1.0 是后续接入的可核查样本。该版本声明 Pull 刷新支持，Push 协商明确上报 `versionSupport=false`，因此按方案在这个 Push 会话中一次交付完整结果。实施时以测试所用客户端的 `initialize` 与 UI 记录核查两条路径。[VS Code Pull 能力](https://github.com/microsoft/vscode-languageserver-node/blob/release/client/10.1.0/client/src/common/diagnostic.ts#L1118-L1144) · [VS Code Push 能力](https://github.com/microsoft/vscode-languageserver-node/blob/release/client/10.1.0/client/src/common/client.ts#L2117-L2122)
- 按 LSP 3.17，`textDocument/diagnostic` 针对当前同步文档，线上没有文档 `version` 字段。`workspace/diagnostic/refresh` 是全局无参数请求，客户端可决定重拉优先级。Push 每次通知替换整份列表，可显式发送空列表清除。客户端声明 `versionSupport` 时才可以依赖其解释版本。Stage 1 与 Stage 2 分别是不同时间点的完整替换快照，文档 `partialResultToken` 留给规范定义的关联文档流式结果。现有 Rust `is_partial` 表示包上下文降级，目前可用语法报告回退，普通 Pull 会等包图。独立记录上下文完整度和诊断阶段，避免把过渡语法报告当成完整类型检查。[LSP 3.17 Pull](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/pullDiagnostics.md) · [LSP 3.17 Push](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/publishDiagnostics.md)

## 实施步骤

### Milestone 1（Phase 1 + Phase 2）

1. **Phase 1 · 跨 worker 缓存**。在 `index.rs`、`source.rs` 复用现有文件观察与修订表示，建立线程安全、有容量上限的磁盘原文、解析产物及包声明摘要缓存。先以路径和磁盘观察判定能否复用，再用内容指纹复用同内容不可变产物。路径、工作区、有效包视图、构建条件等上下文敏感摘要独立校验。相同内容的在途加载归并，缓存锁外执行 I/O、解析和索引构建。安装前复核观察和失效代数，且单一订阅取消不阻断其他使用者。诊断、常规引用和大文件引用共用，打开文档优先取自身不可变 overlay 修订。
2. **Phase 1 · 拆分修订**。构造只包含跨文件语义所需信息的包声明摘要，覆盖包名、imports、顶层签名、常量、类型和包级变量推断类型。函数体变化且该摘要等价时，只推进正文修订。让 `diagnostics/inputs.rs` 的跨文件 memo 跟随实际语义依赖，让引用候选范围跟随包图和导入者边。当前文档报告、引用位置、依赖声明的位置映射和已扫描正文各自复核实际修订。摘要缺失、语法恢复不可靠或推断不确定时保守失效。
3. **Phase 2 · 调度和取消**。在 `scheduler/diagnostics.rs` 为最近编辑的文档与交互 Pull 保留快槽，通过最近编辑和请求推断活跃度，优先最新版本。刷新引起的成批 Pull 依当前活动与缓存命中处理。隔离后台/大文件槽并对普通后台任务提供有界推进机会。按文档合并约 `25 ms` 的连续编辑，设置最长等待窗口，已在快槽中的旧任务收到取消，依完成票据释放槽位。解析、读盘、顶层声明前及每 128–256 节点检查取消，复用解析器的取消回调。对不可中断的 I/O 使用独立有界槽，新版本的前台工作可进预留容量，旧任务完成后统一回收。
4. **Phase 2 · 分阶段诊断**。Stage 1 用当前增量语法树、脏函数作用域、当前文件导入与共享同包声明摘要优先计算语法和可证实的 `name.unresolved`。包图或摘要尚未就绪时，仍可产出已有可靠依据的诊断，等后台上下文补足其余项。损坏的子表达式按最小不可靠范围跳过，继续检查同函数中其余可信节点。将未改范围中经版本、依赖和位置校验的报告一并纳入 Stage 1 替换列表，受影响且无法安全复用的诊断等 Stage 2。若快报会导致已呈现的诊断无依据地消失再出现，则等待完整快照。Stage 2 执行完整类型检查、跨包成员校验和其余诊断，排序、去重并形成完整替换集合。QuickFix 等需要完整语义的消费者只取 Stage 2 或现有等价的完整结果。报告交付后，作为当前诊断的 Stage 2 仍可继续。新编辑、输入过期、关闭、退出或失去全部有效消费者时按任务用途取消。
5. **Phase 2 · 交付和版本一致性**。每次缓存晋级、排队发送与实际提交给传输前，在主循环核对 URI 的打开代次、同步版本、内容指纹、语义输入键及票据，并保证同一快照的阶段单调推进。以稳定排序后的协议可见内容判断变化，包含码、范围、级别、消息、关联信息和必要数据。缓存报告的 `resultId` 为不透明身份，关联可复核快照与内容。同一快照两阶段可见内容相同，提升内部完成度并保留 ID。实际输入等价时沿用既有测试视图升级复用。过期结果与刷新记号丢弃，已取消 Pull 仅响应一次。

| 会话或状态 | Phase 2 协议流 |
| --- | --- |
| Stage 2 在快速交付预算内 | 从符合条件的 Stage 1 就绪起最多再等 `25 ms`，Stage 2 若就绪则只交付终态。Pull 按请求返回一次 `full` 或已知 ID 的 `unchanged`，Push 发送一次完整通知。该预算与编辑合并分别计量。 |
| Pull 且声明 `refreshSupport` | 超出预算时，首个请求可收到 Stage 1 `full` 替换报告。Stage 2 改善实际已交付的可见内容时，先安装完整缓存，再排入合并刷新。若普通重拉已取得终态、文档已关闭、版本过期或内容等价，则清除排队记号。客户端重拉时，携 Stage 1 ID 取得完整 `full` 和新 ID，携当前 ID 取得 `unchanged`。未交付过快报时，Stage 2 供下次 Pull 直接读取。 |
| Pull 无 `refreshSupport` | 只应答当前快照的完整报告（或同 ID 的 `unchanged`），保留优先调度和共享缓存收益。通过正常编辑请求与取消机制推进，保持零主动 Push 和零全局刷新。 |
| 仅 Push 且声明 `versionSupport` | 超出预算时按当前 URI、`version` 发 Stage 1 完整列表。Stage 2 若同版本且有可见变化，再发相同 `version` 的完整列表。受影响诊断归零时按语义发送 `[]`。同一 URI 的通知由主循环顺序发送，关闭时清理，重开用新打开代次。 |
| 仅 Push，未声明 `versionSupport` | 同步后按服务端版本筛选，稳定版本只发送一次完整报告，保持现有关闭清理行为。 |

全局刷新由各 URI 待交付记号聚合，包含已有工作区变化来源。初始参数为 `75 ms` 合并窗口、同会话两次刷新至少相隔 `250 ms`、最多一个在途刷新。发送前重新筛掉已消费或过期的记号。ACK 只释放刷新请求的状态，Pull 响应与诊断工作独立推进。超时或错误做有界清理并记录，当前缓存继续服务正常 Pull，禁止形成重试风暴。客户端取消继续用 `-32800`，已应用同步更改且快照确已过期时用 `-32801`。若需表达服务器暂时无法计算，按 LSP 诊断 `ServerCancelled` 与 `retriggerRequest` 约定验证两端重试。查阅的 Zed 提交只对符合条件的 `ServerCancelled` 重试，分阶段正常交付无需依赖取消。[LSP 3.17 取消与刷新](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/pullDiagnostics.md) · [Zed 重试条件](https://github.com/zed-industries/zed/blob/46375fbf64574f01dc08fd2d2998aa7bbb7f0377/crates/lsp/src/lsp.rs#L399-L408)

### Milestone 2（Phase 3 + Phase 4）

6. **Phase 3 · 引用候选与冷热路径**。在现有包图、直接导入者和 `candidate_scope` 上保守缩限，复用已有字节预筛。冷依赖按有界批次并发读取、解析并回填共享缓存，合并重复 miss，优先准备声明文件、同目录和名称容器命中的文件。给常见引用构建按修订保存的标识符倒排，使用 Go 词法及 Unicode 边界，排除纯注释、字符串和子串。可能通过隐式名称、语法恢复或已有语义分支命中的文件交给原有回退。分批并发做精确绑定验证，主循环依现有顺序规约、去重和换算 UTF-16，保持取消、版本检查以及原有 `includeDeclaration` 行为。
7. **Phase 3 · 包图预热与文件监听**。首次生产包图就绪后低优先级预热测试视图，交互请求需要该图时接管同一加载。新修订可取消旧预热，仍按现有图版本和观察签名原子安装。对 `didChangeWatchedFiles` 以短窗口合并最终路径事件，复用同观察到的声明签名和已解析摘要。结合覆盖层、文件种类、包目录与构建约束判断最小失效范围，保留无法证明等价时的重载。以 S3 风格连发回放验证 15 次过期重载能收敛。
8. **Phase 4 · 遥测与对照**。用现有 `RS_GOPLS_LOG_FILE` 门控贯通诊断任务、文档版本、阶段、排队、取消信号至实际退出、冷读/冷解析、诊断码计数和是否真正交付。沿用哈希文档标识，诊断码只记录既有枚举的计数，省去源码、诊断文本及个人路径。补齐 `rpc_telemetry.rs` 关联的缓存命中、刷新合并与请求写出，并按固定 GoLand 样本完成现场对照。

## 验证计划

确认实施后先运行受影响的 Cargo 包检查，再跑专项与发布回放。沿用环境配置的目标目录、工具链、profile、features 和 target。

~~~powershell
cargo check -p rs-gopls --locked
cargo test -p rs-gopls --locked semantic::diagnostics
cargo test -p rs-gopls --locked scheduler::diagnostics
cargo test -p rs-gopls --locked configuration_reload_emits_diagnostics_only_for_the_installed_generation
cargo test -p rs-gopls --locked semantic::references
cargo test -p rs-gopls --locked workspace
cargo test -p rs-gopls --locked
cargo build -p rs-gopls --release --locked
cargo fmt --all -- --check
git diff --check
~~~

通过 `cargo metadata --no-deps --format-version 1` 读取环境实际产物路径，下列 `$server` 指向待测发布版，`$workspace` 为固定修订 `532228d3d36bd9f0fffb4bda35c180485b2eadd6` 的 `localchat/server`，配套 Go 1.27.4。若实现确实改动扩展包，按既有目标追加 `cargo check -p rs-gopls-zed --locked --target wasm32-wasip2`，现场记录扩展与服务端二进制版本和哈希。

~~~powershell
python -B tools/check_workspace_updates.py $server --module diagnostics
python -B tools/check_workspace_updates.py $server --module references
python -B tools/check_workspace_updates.py $server --module graph_refresh
python -B tools/check_goland_diagnostics.py $server
python -B tools/check_goland_references.py $server --workspace $workspace
python -B tools/test_diagnostic_telemetry.py $server
~~~

Phase 3 的合成回放入口为 `python -B tools/check_phase3_replay.py $server --prior $prior --samples 5 --output $report`。`$prior` 指向任务开始时保存的 Milestone 1 发布版。同机交替启动两版服务端，计时范围为单次 stdio 请求到完整响应。两个冷路径档均在测试包图就绪后查询未缓存的成员，首次测试视图档比较交互按需加载与后台预热完成后的查询。开启现有日志以复核依赖解析计数、包图接管和过期安装。每轮核对两种 `includeDeclaration` 设置的完整 UTF-16 集合，并回放 15 组监听事件及并发编辑。

日志回归复刻 S3 的回包配对、阶段计时、累计取消 worker 时间和引用 RPC 分位数，另按新版本级遥测核对编辑是否真正得到正确诊断并区分客户端去抖、服务端排队、Stage 1、Stage 2、刷新、缓存重拉与渲染。对普通编辑和连续输入分别记录样本分母、过期/取消、无结果次数、P50/P95/Max。连续输入档固定同文档间隔阈值 `500 ms`，追加可重复的连续合成编辑序列。首次诊断正确性由当前版本的诊断码及范围确认，空结果只在预期清除时计为成功。关闭日志运行性能门槛，开启脱敏日志重复相同操作以归因，记录两组差异。

客户端现场回放固定 Zed、VS Code 宿主及语言客户端版本，保留 `initialize` 能力矩阵、诊断流和时间戳。让 Stage 2 分别落在预算内、超预算且有内容变化、超预算且内容等价三档。在编辑、切换缓冲区、关闭、重开、延迟/丢失刷新 ACK 及包图升级时核对客户端呈现顺序。记录 Zed Pull 设置、实际刷新半径及 `previousResultId` 行为。VS Code 用两个能力配置覆盖 Push 和 Pull，另用协议桩覆盖实际宿主难以构造的错误与乱序条件。

### 运行中的 GoLand 对照

沿用 [导航性能对照](../PLANS.md#导航性能对照)。现场固定同一主机、项目修订和 Go SDK、GoLand `GO-263.7307.354`、优化前后的 rs-gopls 发布版和 SHA-256、构建环境、搜索范围、打开文件、查询顺序和编辑序列。GoLand 运行于 Smart Mode，从实际 Go Find Usages 处理器取得目标并比较 UTF-16 范围。固定项目范围、关闭普通文本搜索、方法限定当前声明，按 `includeDeclaration` 选项对应声明位置。除了既有 `localchat` 查询，加入三类 S3 慢样本的可重放夹具（26 次依赖解析含大文件、168 次冷依赖、测试视图首次使用）。诊断记录同一编辑序列中首个正确缺名或语法标注和完整检查就绪，注明客户端 UI 指标与服务端回包指标。使用已有采集脚本作为现行入口。[GoLand 引用采集](../goland-references-baseline.md) · [GoLand 诊断采集](../goland-diagnostics-baseline.md)

每次计时都核对完整目标集合与标识符范围。把 GoDoc 语义注释、层级扩展、搜索范围、落点或加载错误等差异连同样本 ID 单独列账，性能比较使用结果一致的样本。索引就绪后计时从执行 Find Usages 或发 LSP 请求开始，到完整结果枚举或响应收齐为止，记录是否含 UI、LSP 传输和范围规范化。编辑后档注明从前置 `didChange` 或 IDE 编辑事件开始的端到端指标及单独的查询时长。启动到首个正确结果按双方相同起止点另记。如采峰值工作集，记录 IDE/服务端及子进程范围、采样间隔与采样工具。

引用耗时为 N / P50 / P95 / Max，单位为 ms。每档每种声明设置 30 次，逐次核对完整 UTF-16 集合。GoLand 的历史源码集合已用于本轮 `localchat` 验证，两个 GoDoc 用法单列。这些样本的 GoLand 现场计时仍待采集。S3 夹具无对应的 GoLand 历史集合。

| 样本 | 计时条件 | Phase 4 关日志 | 加固版关日志 | 加固版开日志 |
| --- | --- | ---: | ---: | ---: |
| `localchat` 就绪首查 | 测试包图就绪，从请求到完整响应 | 1,020 / 20.75 / 175.77 / 229.33 | 1,020 / 18.69 / 145.50 / 187.25 | 1,020 / 18.75 / 150.54 / 185.35 |
| `localchat` 17 例热查 | 同一会话，重复同一查询 | 1,020 / 7.11 / 55.30 / 63.01 | 1,020 / 4.01 / 15.32 / 19.29 | 1,020 / 4.38 / 18.00 / 20.67 |
| `localchat` 未保存编辑 | 插入首行注释至完整响应 | 1,020 / 9.32 / 56.65 / 63.78 | 1,020 / 5.71 / 16.60 / 27.23 | 1,020 / 6.09 / 18.41 / 27.65 |
| `localchat` 未保存撤销 | 移除首行注释至完整响应 | 1,020 / 8.30 / 61.07 / 72.49 | 1,020 / 4.96 / 20.10 / 24.52 | 1,020 / 5.34 / 22.51 / 31.66 |
| `localchat` 启动首查 | 新进程到首次正确 GetSession，复用宿主 Go 与系统缓存 | 60 / 563.61 / 587.89 / 602.30 | 60 / 563.48 / 592.48 / 599.32 | 60 / 567.11 / 588.70 / 598.45 |
| S3 26 文件，含大文件 | 测试视图就绪后的成员首次查找 | 60 / 498.72 / 535.80 / 560.57 | 60 / 56.09 / 60.57 / 64.31 | 60 / 56.28 / 59.77 / 60.58 |
| S3 168 个冷依赖 | 测试视图就绪后的成员首次查找 | 60 / 16.21 / 18.41 / 19.64 | 60 / 19.38 / 21.59 / 22.56 | 60 / 19.97 / 22.39 / 27.49 |
| S3 预热就绪 | 测试视图已安装后发请求 | 60 / 24.14 / 35.13 / 39.85 | 60 / 22.56 / 36.42 / 39.12 | 60 / 23.29 / 32.83 / 38.96 |
| S3 尚未启动预热 | 生产视图就绪后立即请求测试用法 | 60 / 202.89 / 212.23 / 223.02 | 60 / 203.73 / 217.16 / 223.45 | 60 / 203.75 / 212.04 / 230.70 |
| S3 预热在途接管 | 继承在途测试图加载，含 25 ms 中继留窗 | 60 / 102.69 / 107.74 / 113.96 | 60 / 101.19 / 107.91 / 112.73 | 60 / 102.78 / 110.13 / 112.75 |
| S3 26 文件与大文件诊断并发 | 大文件打开解析完成，诊断启动后留 60 ms 再发引用请求 | 60 / 482.14 / 508.60 / 543.20 | 60 / 39.43 / 41.79 / 42.25 | 60 / 39.81 / 42.32 / 43.53 |

## 完成记录

- 2026-11-27 创建计划，复核仓库协议与调度路径、S3 日志、GoLand 安装包构建和相关 JAR，记录两项待确认的交付取舍。实现、构建、测试和客户端/GoLand 现场计时待计划确认后执行，各验收项待填写实际结果。
- 2026-11-27 记录用户确认并完成 Milestone 1 Phase 1 的步骤 1、2。原文及不可变解析和声明摘要按条目数和估算字节数做 LRU，按路径观察和失效代数校验，按内容复用解析。正文修订标识当前报告与查询位置，声明修订用于跨文件诊断输入。引用候选范围按包图及实际参与查询的导入者边和临时文件分组校验。
- 并发诊断与引用测试在普通文件和大文件上均观测到 1 次冷读、1 次冷解析，重复分析和查询各新增 0 次。覆盖生产者取消、跨路径复用、淘汰、删建竞态和超限文本续用。修订回归覆盖函数体编辑、导入、签名、包级变量推断、构建约束、内外测试包、位置及 UTF-16、内容改回和关闭重开，并核对独立包继续命中。实现查找的精确匹配继续使用预筛捕获的同一文本修订。
- 使用环境配置的 Cargo 产物目录验证。 `cargo check -p rs-gopls --locked` 通过。诊断 36 项、诊断调度 3 项、引用 9 项、工作区 30 项专项均通过，全量 351 项单元测试和 2 项 CLI 集成测试通过。`cargo fmt --all -- --check` 和 `git diff --check` 通过。
- Phase 2 使用当前正文修订和已选视图的语义输入校验交付。共享读取或解析由发起的 worker 完成，等待者可独立取消。阶段调度需要实测发起者完成在途共享工作所产生的取消尾延迟。其余验收项继续开放。
- 2026-11-28 完成 Milestone 1 Phase 2 的步骤 3、4、5。调度使用前台、取消预留、普通后台和大文件独立槽位。编辑合并窗口为 25 ms，最长 100 ms。两阶段交付单独使用 25 ms 预算，全局刷新以 75 ms 合并、250 ms 最小间隔和单在途请求节流。
- 35 万条语句的大文件夹具验证在大文件、普通后台及已取消的旧前台票据仍占槽时，小文件新版本能在预留槽完成真实 Stage 1。共享读盘和解析等待、私有解析、声明收集与 AST 步进的取消注入检查通过 50 ms 断言。共享生产者按 Phase 1 约定完成在途工作。
- 诊断专项 48 项、诊断调度 10 项、配置重载 1 项均通过。诊断协议回放 9 项通过，覆盖客户端能力矩阵、阶段晋级、版本与打开代次校验、刷新延迟与错误 ACK。刷新计时与超时单测 2 项通过。全量 383 项单测及 2 项 CLI 集成测试通过。沿用环境配置的 Cargo 产物目录，`cargo check -p rs-gopls --locked`、`cargo fmt --all -- --check` 和 `git diff --check` 均通过。
- 2026-11-28 完成 Milestone 1 的 13 项代码审查与压测加固修复。全量 398 项单元测试及 2 项 CLI 集成测试通过，6/6 个 Python 回归脚本的 62/62 项用例通过。复核发布版 SHA-256 为 `BD1D42C7259AF8C98DC408B86A0AE85CFBBC83D3073D4A01406C424A0F8854E0`。
- 536 包、2,104 个 Go 文件及约 210 万 AST 节点大文件的自动化 stdio 压测中，400/400 个稳定版本取得正确诊断。分组 P50 为 34.11–35.08 ms，P95 为 40.80–41.63 ms，最大耗时由 1,224.85 ms 降至 45.49 ms。打开大文件期间，小文件定义跳转为 15.4–35.2 ms。
- Zed Pull 与 VS Code Push/Pull 能力矩阵的自动化协议回放已通过。前台性能与宿主交付验收留待固定 Zed、VS Code 图形宿主现场确认，现场需核对 UI 标记与闪烁、终态缓存重拉耗时及取消工作时间占比。
- 2026-11-28 完成 Milestone 2 Phase 3 的步骤 6、7。引用使用按文件修订缓存的 Go 标识符倒排，导出顶层符号结合直接导入缩限，语义验证和冷依赖准备在独立的有界 worker 池中执行。先处理声明和同包或同目录，再处理容器名命中项。主循环复核快照和文件观察，稳定规约引用集合及 UTF-16 范围。
- 首次生产包图就绪后延迟 150 ms 调度测试视图。交互消费沿用同一在途 `go list`，包图失效可取消旧预热。监听事件按 75 ms 固定窗口归并最终路径，复用已观察的包选择与声明语义；包选择无法确认等价时按受影响范围重载。
- 新增 12 项 Rust 测试，覆盖词法倒排和修订缓存、跨包直接导入与候选优先级、依赖并发和热复用、预热接管与取消、15 组事件窗口及正文等价缓存。`cargo check -p rs-gopls --locked`、引用专项 13 项、工作区专项 39 项和全量 410 项单测及 2 项 CLI 集成测试均通过。`cargo fmt --all -- --check`、`git diff --check` 和 `cargo build -p rs-gopls --release --locked` 通过。发布版 `E:\Code\rs-gopls\target\release\rs-gopls.exe` 的 SHA-256 为 `F99BDD3DB056821FDCBA957061260282977202C65E5E343277725F49835BD6DC`。
- 使用 Go 1.27.4 和 `GOTOOLCHAIN=local` 运行 6/6 个 Python 回归脚本，62/62 项通过。其中工作区诊断 2 项、引用 4 项、包图刷新 7 项、GoLand 诊断基线 11 项、GoLand 引用基线 34 组、现有遥测回归 4 项。GoLand 诊断保留 `self-import` 和 `invalid-import` 两项基线已允许差异。
- 发布版 stdio 回放中，预热完成后 5 次首查的 `missing_test_view` 等待均为 0。刻意暂停预热时，请求接管同一任务，整个会话共一次生产加载及一次测试加载。构建环境修改取消一次旧预热，旧图安装为 0，切换构建标签后的精确引用集合与强制重载一致。15 组 watcher 连发及并发编辑每组恰好一次加载、过期安装为 0，稳定集合与全量重载一致。

三类 S3 合成回放各交替运行两个发布版 5 次。两档 `includeDeclaration` 的完整 UTF-16 结果均匹配。表中时间为客户端请求墙钟毫秒。

| 样本 | Milestone 1 P50 / P95 | Phase 3 P50 / P95 / Max | P50 提速 |
| --- | ---: | ---: | ---: |
| 26 文件冷解析，含大文件 | 3,200.37 / 3,244.38 | 464.38 / 470.69 / 472.06 | 6.89 倍 |
| 168 个依赖文件冷解析 | 558.62 / 607.66 | 13.13 / 15.27 / 15.35 | 42.54 倍 |
| 首次使用测试视图 | 207.23 / 220.44 | 38.21 / 39.00 / 39.00 | 5.42 倍 |

26 文件档的依赖 AST 解析由 26 次降至 17 次，168 文件档由 168 次降至 1 次。两版合成冷解析档均记录 `disk_reads=0`。共享缓存单测独立核对冷读、并发冷解析与热复用，每个依赖修订各读取一次、解析一次。测试视图档在前版计入按需加载，Phase 3 从预热完成后发请求，因此计量的是交互等待。

固定 `localchat/server` 修订 `532228d3d36bd9f0fffb4bda35c180485b2eadd6`，关闭日志并轮换启动两个发布版。各版通过 5 次启动首查、400 次无编辑热查及 400 次范围编辑后的混合查询。无编辑热查 P95 从 7.79 ms 增至 19.32 ms，编辑后混合查询 P95 从 10.24 ms 增至 21.47 ms，均低于仓内 30 ms 目标。编辑后跨包组 P95 从 11.12 ms 增至 23.66 ms。

Phase 4 现场对照需固定测试视图未启动、在途接管、已完成三档，并区分 watcher 合并等待、实际包图加载及 RPC 回包时间。记录热查尾延迟变化，分别核对冷读次数和并行解析累计耗时的计时口径。Milestone 2 仍待独立审查及全部验收后统一勾选。

### Phase 4（步骤 8）记录

诊断字段沿用 `RS_GOPLS_LOG_FILE` 门控。诊断链路的 `document` 和既有 `path` 使用同一文档哈希。`result_id` 和字符串 `req_id` 使用哈希，整数请求 ID 保持原样。`diagnostic_codes` 只记录实际出现的 `DiagnosticCode` 和数量，省略的计数为 0。

| 事件 | 字段与口径 |
| --- | --- |
| `diagnostic_worker_start`、`diagnostic_worker_done`、`diagnostic_stages` | `job` 标识一次 worker 执行，`run` 关联同一诊断轮次的阶段，`doc_version` 是 LSP 版本，磁盘文档为 `null`。`stage` 为 `stage1` / `stage2`，`context_completeness` 为 `full` / `partial`，`lane` 为 `foreground` / `reserve` / `background` / `large`。 |
| Worker 等待与取消 | `queue_wait_ms` 从调度入队量到 worker 开工，由 `context_wait_ms`、`merge_wait_ms` 和 `runnable_queue_wait_ms` 组成。`cancel_requested_ms` 和 `worker_exited_ms` 来自同一会话时钟，`cancel_lag_ms` 是两者之差。`rpc_done.queue_wait_ms` 保持既有的 RPC 口径。 |
| `cold_dep_reads`、`cold_dep_parses` | 从共享缓存的实际发起和在途加入点按诊断任务累计，排除目标文档与热命中。两个任务等待一次共享工作时，各记一次，底层仍只执行一次。 |
| `diagnostic_delivery`、Pull `rpc_done`、`diagnostic_push_write` | 通过 `job`、`run`、`doc_version`、`req_id` 和 `result_id` 关联计算、报告和写出。`delivery` 包含 `delivered`、`cache_hit`、`superseded`、`discarded`。写出结果记录 `write_ok`，Pull 区分 `full` 和 `unchanged`，Push 单列线上 `push_version` 和关闭后的 `clear`。 |
| 刷新生命周期 | `diagnostic_refresh_merge`、`diagnostic_refresh_send`、`diagnostic_refresh_ack`、`diagnostic_refresh_timeout` 和 `diagnostic_refresh_drop` 用 `refresh_id` 及 `req_id` 串联。数据包含合并来源、关联报告、实际写出结果、ACK 状态及错误码。 |

- `cargo check -p rs-gopls --locked`、`cargo test -p rs-gopls --locked`（411 项单测、2 项 CLI 集成测试）、`cargo fmt --all -- --check`、`git diff --check`、`cargo build -p rs-gopls --release --locked` 均通过。发布版位于 `E:\Code\rs-gopls\target\release\rs-gopls.exe`，SHA-256 为 `532DF9A755CA15759A64D50DA5A84D5A6627492D9E2C06408E76063A51FF01B1`。
- 固定 Go 1.27.4 和 `GOTOOLCHAIN=local`，按顺序运行 6/6 个 Python 回归脚本，65/65 项通过。遥测 7 项、工作区诊断 2 项、工作区引用 4 项、包图刷新 7 项、GoLand 诊断 11 项、GoLand 引用 34 组。GoLand 诊断保留历史允许差异 `self-import`、`invalid-import`。
- 独立诊断语料使用同一 Go 1.27.4 关闭日志重复测量。对每个样本验证诊断码和范围后，候选暖态 330 次的 P50 / P95 / Max 为 0.10 / 2.10 / 2.38 ms，编辑后 330 次为 30.87 / 31.84 / 41.67 ms。Phase 3 分别为 0.17 / 2.09 / 3.95 ms 和 30.92 / 32.25 / 39.57 ms。数据保存在 `target/diagnostics-phase4-comparison.json` 和 `target/diagnostics-phase3-comparison.json`。
- GoLand 引用历史原始采集与仓内语料逐条相符，共 17 个声明、111 个源码用法及 2 个 GoDoc 用法。构建 `GO-263.7307.354`、Go 1.27.4 和 `localchat/server` 修订 `532228d3d36bd9f0fffb4bda35c180485b2eadd6` 与目标条件一致。历史 GoLand 诊断语料使用独立模块和 Go 1.26.3。历史引用耗时表记录 rs-gopls 和 gopls。
- 已准备 GoLand 引用的现场 30 次重复计时入口。脚本在各次执行后对照完整去重集合，记录两种声明设置的 UTF-16 结果和时间。GoLand 进程存在，当前环境未检测到目标项目窗口，目标项目最近状态为已关闭。需在该构建中打开对应项目并等待 Smart Mode，才能补采匹配的 GoLand 运行时耗时。三类 S3 合成夹具无对应 GoLand 历史采集。
- `localchat/server` 关日志每案例和声明设置各 30 次，累计 4,080 次查询与 60 次进程启动测量，源码集合全部精确匹配。全量扩展样本热查 P95 为 55.30 ms，历史九例热查 P95 为 19.05 ms。逐例检查发现仅 `interface-method` 热查两种设置的 P95 超过 30 ms，分别为 62.20 ms 和 60.29 ms。
- S3 各档关日志每种声明设置 30 次，累计 360 次计时，且逐次复核另一种声明设置的结果。历史 Milestone 1 同机 5 次回放的 P50 / P95 / Max 分别为 3,200.37 / 3,244.38 / 3,244.41 ms、558.62 / 607.66 / 617.19 ms 和 207.23 / 220.44 / 223.00 ms。最后一项的历史结果计入按需加载。旧回放启用日志，候选门槛采用上表关日志口径。候选的 26 文件独立档和大文件诊断并发档 P95 分别为 535.80 ms 和 508.60 ms，均超出 500 ms。各次均在 1 s 内完成。
- 开脱敏日志的 4,080 次 `localchat` 就绪查询均无包图等待，60 次启动首查均记录 `no_graph`。接口方法热查的 60 次记录全部命中候选范围缓存，冷读盘、AST 解析及包图等待均为 0。
- 开日志的 S3 两个冷路径档各 60 次，底层引用的磁盘读取均为 0，依赖 AST 解析分别固定为 17 次和 1 次，累计解析耗时中位数分别为 1,539.24 ms 和 2.98 ms。该累计指标相加了并行解析耗时。预热就绪档 60 次均无 `missing_test_view`，其中 56 次为 0 冷读，1 次为 9 次冷读，3 次为 10 次冷读，依赖 AST 解析均为 2 次。尚未启动档 60 次均按需加载测试视图。在途档 60 次均接管已有预热，每次会话的 `go list` 只各启动一次生产加载和测试加载。
- `localchat/server` 诊断另测普通输入和连打的末个稳定版本。每种输入分别对缺名出现、清除采 30 次，开关日志合计 240 次。全部验证当前版本的 `name.unresolved` UTF-16 范围或清除。120 次开日志回包逐次关联正确的 `doc_version` 和 `stage2/full/delivered`，稳定版本错误响应为 0。计时从末次 `didChange` 开始，到完整 Pull JSON-RPC 回包，适用于服务端与传输计时。

| `localchat` 诊断输入 | 关日志 N / P50 / P95 / Max | 开日志 N / P50 / P95 / Max |
| --- | ---: | ---: |
| 普通输入，缺名出现 | 30 / 46.51 / 49.19 / 49.45 ms | 30 / 47.17 / 48.89 / 48.99 ms |
| 普通输入，缺名清除 | 30 / 46.41 / 47.97 / 49.02 ms | 30 / 46.01 / 47.08 / 47.99 ms |
| 连续输入，缺名出现 | 30 / 46.99 / 48.89 / 49.64 ms | 30 / 46.74 / 48.37 / 48.90 ms |
| 连续输入，缺名清除 | 30 / 45.54 / 47.93 / 63.01 ms | 30 / 45.79 / 46.95 / 47.42 ms |

开日志按同样档位和样本数复跑引用，逐次核对精确集合。并发档 60/60 次在引用计时内有版本 1 的 `large` lane Stage 1 运行。实际取消信号到分析退出的 P50 / P95 / Max 为 0.0655 / 0.1008 / 0.1647 ms，前置队列等待为 0.2940 / 0.3972 / 0.5187 ms。以下为同一发布版的开日志耗时。

| 引用档 | 开日志 N / P50 / P95 / Max |
| --- | ---: |
| `localchat` 就绪首查 | 1,020 / 20.69 / 174.50 / 228.31 ms |
| `localchat` 热查 | 1,020 / 7.20 / 54.54 / 61.40 ms |
| `localchat` 编辑 | 1,020 / 9.02 / 55.68 / 64.24 ms |
| `localchat` 撤销 | 1,020 / 8.39 / 60.22 / 71.38 ms |
| `localchat` 启动首查 | 60 / 566.34 / 582.34 / 613.83 ms |
| S3 26 文件 | 60 / 489.98 / 509.34 / 565.85 ms |
| S3 168 依赖 | 60 / 14.77 / 17.38 / 18.18 ms |
| S3 测试视图就绪 | 60 / 24.94 / 39.83 / 41.22 ms |
| S3 测试视图尚未启动 | 60 / 204.83 / 215.84 / 220.91 ms |
| S3 测试视图在途 | 60 / 102.63 / 108.13 / 111.30 ms |
| S3 26 文件与大文件诊断并发 | 60 / 482.20 / 500.69 / 520.00 ms |

逐例、逐次数据与归因保存在 `target/phase4-performance-off.json`、`target/phase4-performance-on.json` 和 `target/phase4-performance-localchat-diagnostics.json`。引用每个文件有 4,140 条 `localchat` 计时和 360 条 S3 计时，分位数使用线性插值。以下为复跑命令，沿用验证计划的 `$server`、`$workspace` 和 Go 1.27.4。

~~~powershell
python -B target/phase4-performance.py $server --workspace $workspace --samples 30 --attribution-samples 0 --output target/phase4-performance-off.json
python -B target/phase4-performance.py $server --workspace $workspace --samples 0 --attribution-samples 30 --output target/phase4-performance-on.json
python -B target/phase4-performance.py $server --workspace $workspace --suite local-diag --samples 30 --attribution-samples 30 --output target/phase4-performance-localchat-diagnostics.json
~~~

### Milestone 2 独立审查加固

- 包图加载记录合并前的文件事件。若加载期间发生过结构或文件事件，丢弃该结果并安排重载，覆盖窗口内 A→B→A 的往返变化。`didSave` 保留同路径已经排队的新增或删除事件。
- 测试视图预热取消覆盖 `go env`、`go work`、`go list` 和输出管道收集。直接子进程回收与仍由后代占用的管道读取可以独立结束，取消后加载槽可继续使用。未被交互接管的预热在补全空闲时重新核对版本及健康生产分组覆盖，再安装测试视图。
- 引用 worker 捕获任务 panic、重置 parser，向调度器发送并唤醒终态。引用查询消费同修订的热声明索引和词法判负，只分发冷依赖。大文件依赖在安全时省略函数体，完整候选校验和需要推断的初始化表达式按完整源码处理。声明摘要改为惰性计算，共享加载池按可用并行度设为 4–8 线程。交付前复核候选和依赖的磁盘观察，发现修订变化时重试查询。
- 所有 JSONL 事件的路径统一采用完整规范路径的会话内带密钥摘要。测试覆盖同名文件跨根区分和全量日志脱敏。同步缓存命中执行时间计入 RPC 执行，刷新先记录 `diagnostic_refresh_send` 再入传输队列，实际写出由 `diagnostic_refresh_write` 记录。
- 2026-11-28 在环境配置的 Cargo 产物目录通过 `cargo check -p rs-gopls --locked`、`cargo test -p rs-gopls --locked`（421 项单测、2 项 CLI 集成测试）、`cargo fmt --all -- --check`、`git diff --check` 和 `cargo build -p rs-gopls --release --locked`。发布版为 `E:\Code\rs-gopls\target\release\rs-gopls.exe`，SHA-256 为 `9CBD3C9048F9917345A65D1FBB54928F249A1600059CB501728D8E6E5A872F3E`。
- 使用 Go 1.27.4（`GOTOOLCHAIN=local`）运行上述发布版。6/6 个 Python 回归入口共 63/63 项通过，分别为遥测 8 项、工作区诊断 2 项、工作区引用 4 项、包图刷新 4 项、GoLand 诊断基线 11 项，以及 GoLand 引用基线 34 组精确集合。
- 正式回放每档、每种声明设置各 30 次，开关日志各一轮。`localchat` 8,280 次和 S3 720 次计时查询全部通过 UTF-16 集合核对；S3 还逐次核对了另一种声明设置。上表记录各档耗时。`localchat` 历史九例热查 P95 由 Phase 4 关日志的 19.05 ms 降至 7.44 ms，开日志为 8.18 ms。`interface-method` 两种声明设置的关日志热查 P95 为 17.78 ms 和 18.12 ms，开日志为 19.69 ms 和 20.23 ms。
- S3 26 文件独立和并发两档均达到 P95 `≤ 500 ms`，全部样本 `≤ 1 s`。开日志独立档每次依赖 AST 解析为 17 次，并行耗时之和的中位数由 Phase 4 的 1,539.24 ms 降至 70.35 ms。并发档开日志 60/60 次证实大文件 Stage 1 与引用计时重叠，全部在引用后取消。诊断队列等待 P95 为 0.3758 ms，取消信号至 worker 退出 P95 为 0.1209 ms。168 冷依赖档关日志 P95 从 18.41 ms 升至 21.59 ms。
- 预热就绪档开日志 60/60 次无 `missing_test_view`。在途档开日志 60/60 次沿用在途预热，每次会话各启动一次生产 `go list` 和一次测试 `go list`。未启动预热档开日志 60/60 次按需加载测试视图。
- Phase 3 回放完成了 15 组 watcher 连发与并发编辑，每个窗口各一次新加载，过期安装和过期重载均为 0，稳定后的集合与强制全量加载一致。构建环境变化取消了一次旧预热，刷新后的带构建标签用法与强制加载一致。独立 Rust 回归覆盖了窗口内 A→B→A 导致的必要重载。
- 同机开日志以 Phase 3 入口交替回放加固版和 Milestone 1 版（SHA-256 `BD1D42C7259AF8C98DC408B86A0AE85CFBBC83D3073D4A01406C424A0F8854E0`），每档每版各 5 次。26 文件、168 依赖、测试视图首用的 P50 分别从 3,320.59、568.09、219.87 ms 降至 42.43、16.86、38.63 ms，相当于原耗时的 1/78.27、1/33.70、1/5.69。测试视图首用旧版计入按需加载，加固版在预热后发起查询。

`localchat` 自动化 stdio 诊断回放的 240/240 个稳定版本都获得正确缺名或清除结果。耗时从末次 `didChange` 到完整 Pull 回包。

| 编辑输入 | 关日志 N / P50 / P95 / Max（ms） | 开日志 N / P50 / P95 / Max（ms） |
| --- | ---: | ---: |
| 普通输入，缺名出现 | 30 / 47.15 / 48.85 / 49.11 | 30 / 47.07 / 48.82 / 50.00 |
| 普通输入，缺名清除 | 30 / 46.07 / 48.29 / 49.50 | 30 / 46.47 / 48.71 / 50.15 |
| 连续输入，缺名出现 | 30 / 47.12 / 49.65 / 57.16 | 30 / 46.59 / 48.89 / 61.34 |
| 连续输入，缺名清除 | 30 / 46.50 / 48.05 / 48.96 | 30 / 46.59 / 55.95 / 62.68 |

~~~powershell
python -B target/phase4-performance.py $server --workspace $workspace --samples 30 --attribution-samples 30 --output target/milestone2-final-performance.json
python -B target/phase4-performance.py $server --workspace $workspace --suite local-diag --samples 30 --attribution-samples 30 --output target/milestone2-final-localchat-diagnostics.json
python -B tools/check_phase3_replay.py $server --prior $prior --samples 5 --output target/milestone2-final-phase3-replay.json
~~~

#### 剩余现场验收

- 在 GoLand `GO-263.7307.354` 打开固定修订的 `localchat/server` 和对应 S3 夹具，等待 Smart Mode，再补采每档 30 次现场计时并逐次核对 UTF-16 集合。本轮检查时 GoLand 进程存在，目标项目窗口尚未打开。保留「引用集合与性能」及「遥测与 GoLand 对照」的现场验收。
- 在固定版本的 Zed 及 VS Code 图形宿主回放 Pull 和 Push 能力配置，核对编辑到正确标记、完整报告重拉、取消 worker 时间占比及阶段切换时的 UI 闪烁。保留「前台延迟与取消」及「Zed Pull 与 VS Code Push/Pull」的现场验收。

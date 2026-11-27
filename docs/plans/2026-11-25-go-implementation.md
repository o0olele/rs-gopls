# Go 导航第四阶段开发计划

| 项目 | 内容 |
| --- | --- |
| 状态 | 受阻 |
| 创建日期 | 2026-11-25 |
| 用户确认 | 2026-11-25 已确认，开始实施 |
| 更新日期 | 2026-11-27 |
| 完成日期 | 待填写 |

## 目标与范围

在 Zed 中从 Go 接口类型或接口方法跳到实现，从具体类型或方法反查其满足的接口与对应方法。支持从声明和用法位置发起请求，通过 `textDocument/implementation` 返回实际声明位置。查询目标、搜索范围和落点以 GoLand 2026.3 在 `E:\Code\localchat\server` 的实测为准。接口到实现使用 **Go to Implementation**，具体实现反查接口时对照 **Go to Super**，并在同一光标位置记录 **Go to Implementation** 的行为。

延续第三阶段的真实项目基线和未保存缓冲区优先策略。先采集 GoLand 基线，再实现并在 Zed 验收。后续性能测量直接对照 GoLand 的实际导航查询。[项目愿景](../vision.md) · [第三阶段验收记录](2026-11-25-go-navigation-phase-three.md)

## 验收标准

- [x] GoLand 实测记录可在固定项目修订下重放。覆盖接口类型和方法、具体类型和方法、声明和调用处、同包和跨包、测试实现，以及单目标、多目标和空结果。
- [x] LSP 初始化声明 `implementationProvider`。发布版严格匹配 GoLand 确认的目标集合与 UTF-16 标识符范围，结果稳定且去重。无匹配项返回空结果。
- [x] 补充夹具验证值与指针接收者、嵌入接口与提升方法、同名异签名、跨包私有方法、别名、泛型及空接口和约束接口。按 GoLand 实测确认各场景中出现的候选和落点。
- [x] 未保存的接口或实现改动能即时改变结果，关闭覆盖缓冲区后恢复磁盘结果。工作区包图更新后，搜索使用当前构建条件与测试包视图。长查询可取消。
- [ ] Zed 的隔离配置加载发布版 `rs-gopls`。在 `localchat/server` 实际调用实现跳转，核对单目标落点、多目标选择列表、空结果和实现反查接口。现场抽查声明跳转、类型定义跳转和查找引用。
- [x] 第三阶段记录的声明、类型定义、引用及工作区回归全部通过。阶段发布时的首次正确响应、暖态和编辑后 P50/P95、工作集及差异已归档。[历史性能记录](../goland-implementation-baseline.md#性能对照)
- [x] 在相同项目及 Go SDK 下，直接测量已索引的 GoLand 与候选版。记录完整目标一致的四个查询，以及三项搜索范围差异。[GoLand 直接对照](../goland-implementation-baseline.md#与-goland-直接对照)

## 实现依据

只读检查 GoLand 安装目录 `E:\Apps\goland-2026.3.0.1.win` 的 `build.txt`、插件注册和字节码，构建号为 `GO-263.7307.354`。Go 插件文件为 `plugins/go-plugin/lib/modules/intellij.go.impl.jar`，SHA-256 为 `2A13F1A753D5E15DB70C5758E17DB1DC28CC790E1D329E574D8B6A078D636784`。可使用同目录下 `jbr/bin/java.exe`、`lib/intellij.libraries.asm.jar` 中的 `org.jetbrains.org.objectweb.asm.util.Textifier` 和对应 JAR 重查下列类。

| 安装包中的类或注册 | 已确认的调用流程 |
| --- | --- |
| `lib/intellij.platform.lang.impl.jar` 中的 `com.intellij.codeInsight.navigation.GotoImplementationHandler` 和 `ImplementationSearcher` | 光标目标由 `TargetElementUtil` 解析，实现候选通过 `DefinitionsScopedSearch.search` 查询。 |
| Go 插件的 `intellij.go.impl.xml` 和 `com.goide.go.GoInheritorsSearch.processQuery` | 插件为 `DefinitionsScopedSearch` 注册 Go 类型和方法搜索。类型入口取得接口的直接及完整方法列表，在搜索范围内查找候选，再用 `GoTypeUtil.isImplementsInterface` 验证。 |
| `com.goide.go.GoMethodInheritorsSearch.processQuery` 和 `GoGotoUtil` | 方法入口从所属接口查候选，再通过 `getSameNamedMethodOfTypeSpec` 取得方法位置。`GoGotoUtil` 用方法及方法规格指纹索引查候选，并追踪嵌入类型和别名索引。 |
| `intellij.go.impl.xml`、`com.goide.go.GoGotoSuperHandler`、`GoSuperMethodSearch` | `codeInsight.gotoSuper` 注册了独立的 Go 入口。类型和方法搜索复用接口候选查询与满足关系检查，具体方法可映射到对应接口方法。 |

Rust 方案复用 [`src/main.rs`](../../src/main.rs) 的 LSP 分派和分批取消流程、[`src/symbols.rs`](../../src/symbols.rs) 的声明索引、[`src/semantic.rs`](../../src/semantic.rs) 的符号解析和文件覆盖，以及 [`src/workspace.rs`](../../src/workspace.rs) 的包图和测试包视图。用方法名及可安全过滤的签名信息筛选候选，覆盖没有直接导入接口包的结构化实现。用完整的值或指针方法集、签名类型身份及不可导出方法的包身份验证满足关系，并处理嵌入产生的歧义。类型查询返回类型声明，方法查询以实测的方法声明为落点。索引更新沿用现有文件和包图失效机制。空接口、约束接口、别名、提升方法、局部和依赖范围、原声明是否包含在候选中，均以运行中的 GoLand 结果确定。

## 实施步骤

1. 确认后记录确认结果并将状态改为「进行中」。在 GoLand 打开固定修订的 `localchat/server`，等索引完成后采集双向跳转。优先选用 `internal/store` 的 `Store`、`UserStore`、`UserStore.GetAll` 和 `internal/store/sqlstore` 的对应实现，在 `internal/api` 和 `internal/app` 补充调用处和测试实现。交叉核对 IDE 中的实际动作与采集输出。提交可重放的采集入口、真实项目和补充夹具，以及 `docs/goland-implementation-baseline.md` 中的实测记录。
2. 接入 `textDocument/implementation` 和能力声明，按 GoLand 基线实现接口类型、接口方法到实现的查询。用严格的 stdio 检查器验证目标集合与范围，运行受影响的回归后提交。
3. 实现具体类型和方法到接口的查询，按基线补齐嵌入、提升、别名、泛型及作用域边界。验证缓冲区覆盖、包图变更和取消，并运行前三项导航回归后提交。
4. 构建发布版，记录与 GoLand 的同场景性能对照及 Zed 实际跳转结果，附上版本、提交号、二进制摘要、功能结果和性能数据。验证通过后提交验收记录及最终计划状态。

每个验证通过的步骤各提交一次本地 Git，同步勾选本计划的验收项并补记实际结果。目标、范围或验收条件有实质变化时，更新本计划并重新确认。

## 验证计划

- **GoLand 基线**　沿用第三阶段 `localchat/server` 修订 `532228d3d36bd9f0fffb4bda35c180485b2eadd6` 和 Go 1.27.4，采集时复核并记录实际环境。每个查询记录动作、查询路径、零起点 UTF-16 光标、目标路径和标识符范围。依赖和 SDK 路径按 `GOMODCACHE` 与 `GOROOT` 归一化。记录 UI 中的多候选和空结果，补充可重放的语义边界夹具。
- **构建与回归**　Rust 改动先运行 `cargo check -p rs-gopls --locked`，再按需运行 `cargo test -p rs-gopls --locked`。发布验收运行 `cargo build -p rs-gopls --release --locked`，通过 `cargo metadata` 读取配置的产物目录。严格运行新增实现跳转对照，并复用 `tools/check_definitions.py --stage 6`、`tools/check_goland_definitions.py`、`tools/check_goland_references.py` 和 `tools/check_workspace_updates.py` 的现有参数与语料。核对第三阶段的 21 个 GoLand 声明位置、124 个合成声明位置、16 个项目锚点、58 个类型定义位置、34 组引用结果及 12 组工作区回归。[类型定义验收](../goland-type-definition-baseline.md#类型定义发布验收) · [引用验收](../goland-references-baseline.md#引用发布验收)
- **性能对照**　直接测量 GoLand 的 **Go to Implementation** 和 **Go to Super**，与发布版 `textDocument/implementation` 的对应查询比较。固定项目修订、Go SDK、搜索范围和计时边界。按缓存状态分场景记录 P50/P95，目标集合有差异的样本单列。已有导航的性能回归可用上一阶段 rs-gopls 发布版复测。[导航性能对照](../PLANS.md#导航性能对照)
- **Zed 实测**　先运行 `tools/test_zed.ps1 -PrepareOnly` 准备发布二进制，再用输出的隔离配置打开 `localchat/server`，确认 Go 文件使用 `rs-gopls`。在已采集的光标位置触发跳转，核对编辑器落点或多目标列表。记录 Zed 版本、已加载的发布二进制摘要、LSP 能力、查询位置和可见结果。沿用仓库约定的工具链、profile、features 和 target。

## 首次响应优化

2026-11-26 用户确认先将候选筛选移到声明解析之前。用接口的必需方法名或反查的具体方法名筛选包，再通过活动包图保留可能获得提升方法的包。打开缓冲区和当前包图无法提供充分证据的情况采用保守范围。候选仍由完整方法集验证。

- [x] `Store` 首次完整结果快于优化前的发布版。用固定 SDK、项目修订及相同请求比较至少五次独立进程，记录 P50/P95 和方向差异。
- [x] 49 项 GoLand 对照和已有导航回归通过，额外验证跨包提升、别名和未保存导入对候选范围的影响。

2026-11-26 用户确认继续优化具体类型反查。使用具体类型自身和提升的方法筛选可能的接口声明，保留别名、嵌入及未保存缓冲区涉及的候选。按接口的必需方法建立包内索引，精确结果由完整方法集匹配确认。

- [x] 用 `SqlUserStore` 等具体类型做同条件五轮首查对照，精确命中互有不同必需方法的接口，记录优化前后的 P50/P95。[历史首查对照](../goland-implementation-baseline.md#具体类型反查首查优化)
- [x] 原有 49 项 GoLand 对照与导航回归通过。补充验证跨包的接口别名、提升后的方法及未保存接口造成的候选集合变化。

## 已索引的具体类型反查优化

2026-11-26 用户确认按 GoLand 实测继续优化。复用已经探测过的磁盘接口声明和已经建立的候选包范围，减少新光标或切换类型时重复读取源码。文件变化、未保存缓冲区和包图切换按各自的快照失效规则处理。

- [x] 对照 `SqlStore`、`LocalFileBackend` 和 `Store` 的完整目标，记录换光标、先查另一类型和同光标重复查询的 P50/P95。两种具体类型换光标时的 P50 分别为 0.84 ms 和 0.34 ms，GoLand 分别为 20.12 ms 和 4.43 ms。仅预热 `SqlStore` 后首次查 `LocalFileBackend` 的 P50 仍为 1772.83 ms。[GoLand 复测数据](../goland-implementation-baseline.md#复用具体类型反查的候选范围)
- [x] 49 项 GoLand 对照及声明跳转、类型定义、引用回归通过。18 项工作区回归验证了接口源码变化、未保存覆盖和关闭恢复的结果。

## 2026-11-27 数组长度常量的签名匹配

用户要求修复并提交完整签名匹配中的漏查。接口方法接收 `[4]byte`，两个包的实现分别使用 `[4]byte` 和 `[Width]byte`，其中 `const Width = 4`。Go 编译器接受两种实现，修复前的发布版只找到前者。将接口本身改为使用导入的长度常量后，新增工作区用例期望两个目标，修复前实际为零。[Go 规范中的类型一致性](https://go.dev/ref/spec#Type_identity)

只读核对 GoLand `GO-263.7307.354` 的 `plugins/go-plugin/lib/modules/intellij.go.impl.jar`。`GoTypeUtil.equalTypes` 比较两侧数组的 `getLength()` 后递归比较元素类型。`GoPsiImplUtil.calcLength` 对显式长度调用 `GoExpression.getValue()`，`GoExpressionEvaluator.evaluateConstDefinition` 从常量声明取初始化表达式及其 `iota` 行号。

Rust 修改沿用现有方法集匹配和工作区索引。在数组类型的声明上下文中解析长度，对已知整数值比较，对无法求值的长度保留未决结果。验收包含正反向类型及方法查询、不同长度排除、类型别名的声明作用域、变更未保存的常量值并关闭文件后的结果恢复。

- [x] VS Code 1.148.0 和官方 Go 扩展 0.58.0 只打开 `api/sink.go`，在 `Sink` 和 `Push` 上查询。旧版两次都只找到字面量实现，修复后的 release 两次都准确返回字面量及具名常量的两个实现，排除了长度为 5 的实现。直接 stdio 回放的文件和 UTF-16 范围一致。
- [x] 工作区回归核对正反向结果以及未保存编辑 `4 → 5` 和关闭恢复。Rust 用例覆盖别名的声明作用域、分组 `iota`、内建整数类型转换、不同长度及循环或变量，另确认 `[0+10]byte` 只匹配十进制长度 10。
- [x] 263 项 Rust 测试、7 组实现导航工作区回归、29 组导航夹具与 2 组跨包示例、20 组 LocalChat 既有 GoLand 对照均通过。复验 GoLand 对照使用仓库保存的采集结果。整数计算超出支持的精度，或表达式使用当前不支持的形式时，求值仍可能返回未知。

## 完成记录

- 2026-11-25 创建待确认计划。起点为 `main` 的 `7960707`，工作区干净，本地领先 `origin/main` 33 个提交。第三阶段发布版为 `be7641d`，验收记录提交为 `e0b6d2f`。
- 2026-11-25 用户确认计划，开始采集 GoLand 基线。
- 2026-11-26 在指定 GoLand 中完成 `localchat/server` 的 20 个查询和两个动作共 108 个目标的采集。记录了调用处包含接口声明的行为，以及活动包图外的依赖目标。[GoLand 实现跳转基线](../goland-implementation-baseline.md)
- 2026-11-26 发布构建通过 12 个 GoLand 正向查询、缓冲区覆盖与活动包图更新、待加载和扫描取消检查。声明跳转 124 个合成目标与 16 个真实项目锚点、类型定义 34 个真实项目位置、引用 34 组结果通过同轮回归。
- 2026-11-26 补充模块通过 Go 1.27.4 的 `go test ./...`。GoLand 双向重放与语料一致，共 29 个补充查询，已核验真实项目和补充模块全部 203 个原始目标的 UTF-16 范围。
- 2026-11-26 双向查询发布构建通过真实项目 20 项和补充模块 29 项。三个 GoLand 依赖 `testdata` 结果按活动包图处理，原始目标与依据已记录。全量导航检查和 15 组工作区回归通过，包含 3 组实现跳转更新及取消检查。
- 2026-11-26 优化后的发布版再次通过全部精确对照和回归。五轮性能测试每套通过 805 次响应，正向与反向暖态 P95 分别为 0.53 和 0.22 ms，达到计划目标。首次正确响应 P50 为 3.90 秒，gopls 为 1.09 秒。[性能记录](../goland-implementation-baseline.md#性能对照)
- 2026-11-26 已验证 Zed 1.29.3 隔离窗口加载测试发布版。Windows UI 自动化启动失败 `CreateProcessWithLogonW 1920`，实际菜单动作和界面落点待从恢复后的自动化工具或用户现场结果核对。[Zed 验收位置](../goland-implementation-baseline.md#zed-加载与现场验收)
- 2026-11-26 「受阻」只涉及 Zed 现场动作。恢复条件为 Windows UI 自动化可连接该窗口，或取得隔离配置下的现场落点、候选列表、空结果及已有导航抽查结果。
- 2026-11-26 首次候选筛选验证通过。`Store` 新旧版同轮 P50 从 3.66 秒降到 0.77 秒，具体方法反查从 3.60 秒降到 0.85 秒，具体类型反查为 3.54 秒。49 个 GoLand 对照、16 项工作区回归、声明跳转、类型定义和引用回归全部通过。[同轮首查与暖态结果](../goland-implementation-baseline.md#首次响应优化)
- 2026-11-26 具体类型反查索引验收通过。`SqlUserStore` 同轮首查 P50 从 3.74 秒降到 1.30 秒，`SqliteUserStore` 和 `memoryUserStore` 分别下降至 1.31 和 1.36 秒。45 次具体类型首查均精确匹配，49 项 GoLand 对照和 17 项工作区回归通过。[具体类型首查验收](../goland-implementation-baseline.md#具体类型反查首查优化)
- 2026-11-26 用户指定以后直接与 GoLand 对比性能。已记录 GoLand `GO-263.7307.354` 与 rs-gopls `76aa881` 在已索引项目中的查询耗时，分别列出 rs-gopls 的新光标查询和同光标结果缓存命中。[GoLand 直接对照](../goland-implementation-baseline.md#与-goland-直接对照)
- 2026-11-26 复用文件探测和候选范围通过发布版回归。三个共同查询的所有目标范围一致，`SqlUserStore` 的活动包图范围差异单列。两个具体类型换光标时的 P50 分别从 197.24 ms 降至 0.84 ms、从 160.12 ms 降至 0.34 ms。Zed 界面复验时自动化进程在初始化阶段退出，阶段状态仍为「受阻」。[本轮验收](../goland-implementation-baseline.md#复用具体类型反查的候选范围)

# Track F 第二阶段 · 多根错误隔离、层级语义对标与接口反查索引加速

| 项目 | 内容 |
| --- | --- |
| 状态 | 已完成 |
| 创建日期 | 2026-11-27 |
| 用户确认 | 已确认（2026-11-27 批准 WP1–WP4 全部实施；事项 A 选 A1：保留闭包归属外层命名函数并更正文档差异说明；事项 B 选 B2：继续按显式 workspaceFolders + go.work 识别构建组） |
| 更新日期 | 2026-11-27 |
| 完成日期 | 2026-11-27 |

## 目标与范围

前序 Track F 已完成 F-WP1、F-WP2、F-WP3 并经 `d7e08e2` 合入 `main`。以当前 HEAD `6e6f0a7` 为 Rust 基线，复测和 GoLand `2026.3.0.1`（`GO-263.7307.354`）的 REA 字节码核查发现四项缺口，按以下工作包处理。[前序计划](2026-11-27-go-hierarchy-foundation.md)

1. **WP1 · 多工作区构建组隔离与包错误精确归属**
   - 修复已复现的同名独立模块串扰。健康模块 A 与带无 `Pos` 的 `import cycle` 的同名模块 B 同时加载时，A 误报 `package.load`、B 漏报。A 通过本地 `replace` 可解析的未保存导入，在加入缺少该依赖的 B 后误报 `import.invalid`。
   - 独立发现和加载各构建组。一个组加载失败时保留健康组的包图及导航，仅为失败组报告组级加载错误。按组暂存加载结果，在包 ID 重映射确定后，将 `PackageLoadError` 统一绑定到构建组和对应的规范包 ID，并保留原始 `import_path`、`for_test`、`import_stack`，按组去重。
   - 同步修正 `src/semantic/diagnostics/package_errors.rs`、`src/semantic/diagnostics/imports.rs` 与 `Workspace::resolve_import` 的组内回退。将无位置错误归属到正确包和测试变体，防止测试变体的错误落到共享普通文件上。扩展 `tools/workspace_checks/hierarchy.py`，验证动态增删同名独立模块时的错误归属及调用和类型层级边。
2. **WP2 · 接口查实现冷查询索引加速**
   - 含测试的夹具包图有 120 包、702 文件，`Reader` 的首个 `typeHierarchy/subtypes` 耗时 `1,076.34 ms`。主进程约有 1,515 次读取、6.73 MB。移除测试包后包图缩至 2 包，耗时为 `0.87–1.23 ms`。当前查询选单个锚点方法名（如 `Read`），以 `text.contains` 扫描导航包磁盘文本，保守扩张传递导入者，并在粗筛与精确匹配阶段重复读盘。
   - 复用 `FileIndex`、`InterfaceSources` 的包图代次与文件指纹失效机制。用轻量方法签名指纹（方法名、参数与返回值计数，结合已有变参形状）维护跨查询倒排索引，定位候选包、文件和类型。维护直接嵌入和命名类型 RHS 的反向记录，复用同一内容版本的读取和解析结果。保留未保存 overlay、测试变体、不完整包及外部依赖的保守兜底，最终由完整方法集判定。
   - 含测试夹具的 `typeHierarchy/subtypes` 首查 P95 较同条件基线降低至少一个数量级，目标 `<100 ms`。在多包合成项目、真实项目及 49 项既有实现跳转对照中验证。分项输出候选探测、解析、方法集匹配和嵌入扫描耗时。
3. **WP3 · Call / Type Hierarchy 语义与 GoLand 2026.3 对标收敛**
   - 接口上下行使用显式方向的结构方法集满足关系。查询接口 I 的超类型时，验证 I 满足候选接口的方法集。查询子类型时，验证候选类型满足 I 的方法集。覆盖传递嵌入和结构兼容接口。`reader-subtypes` 补齐经 `ReadCloser` 传递嵌入的 `Service`，`service-supertypes` 补齐 `Reader` 和结构兼容的 `Resetter`。
   - 产品有意差异。具体类型保留结构体直接嵌入导航（`Base ↔ Derived`）。Call Hierarchy 保留递归自边，`fromRanges` 使用标识符范围。空接口省略全量类型枚举。
4. **WP4 · 可重放基线、GoLand 编辑后计时与文档收口**
   - 归档脱敏的 `tools/fixtures/hierarchy/goland-hierarchy.json`，统一 `docs/goland-hierarchy-baseline.md` 与两支层级验证脚本的 `--fixture` 路径。
   - 扩展 `tools/capture_goland_hierarchy.groovy`，在未保存文档中插入注释、提交 PSI、查询并计时，然后恢复文档并再次提交 PSI。记录双方的首查、热查、编辑后 P50/P95 和样本数，归档完整逐项差异表。[导航性能对照](../PLANS.md#导航性能对照)
   - 修正前序计划不可解析的旧提交 SHA 注释。更新 `README.md` 与 `docs/vision.md` 的 Track C 安全重命名入口及基线链接，以及 Track F 冷反查优化说明和层级验证命令。

### 待用户确认事项

| 事项 | 选项 1 | 选项 2 |
| --- | --- | --- |
| A · Call Hierarchy 闭包归属 | **推荐**。保留当前归属外层命名函数或方法的 LSP 行为，并更正前序计划第 39 行“与 GoLand 一致”的记载。 | 对齐 GoLand，独立处理函数字面量，不将闭包调用合并到外层命名函数或方法。 |
| B · 单根目录内嵌套独立 `go.mod` 自动发现 | 在显式文件夹内扫描嵌套的独立 `go.mod`，参考 `VgoStatusTracker.findAllGoModuleSettingsFiles`。 | **推荐**。继续根据显式 `workspaceFolders` 的 `GOWORK`、`GOMOD` 与 `go.work` 成员识别构建组，控制变更范围。 |

## 验收标准

- [x] WP1 · 同时加载两个同名独立模块，A 的 `package.load` 与 `import.invalid` 均为空，A 的未保存本地 `replace` 导入仍可解析。B 的无 `Pos` 循环错误只在 B 的对应包报告。仅测试变体的错误只归属相应变体，普通共享文件无误报。
- [x] WP1 · 注入组级发现或加载失败，健康组的包及导航结果保持完整，仅失败组有组级错误。核对已归属的错误使用包图规范 ID、保留原始字段，同组重复上报去重，同名异组错误独立保留。
- [x] WP1 · 通过 `workspace/didChangeWorkspaceFolders` 增加、移除和重新增加工作区，逐步核对诊断、`incomingCalls`、`outgoingCalls`、`supertypes` 和 `subtypes` 的目标与范围。合法新增边出现，移除根的边消失，同名根之间保持隔离。
- [x] WP2 · 含测试夹具的 `Reader` 首查 P95 较同条件 `6e6f0a7` 降低至少 10 倍且低于 `100 ms`。报告相同口径下的包数、文件数、磁盘读取次数及字节数与四项耗时，并单列索引构建和就绪耗时。同一内容版本的粗筛与精确解析复用已读入的源码。
- [x] WP2 · 多包合成项目和固定修订真实项目的候选与精确结果通过复测，49 项实现跳转对照（真实项目 20 项、补充夹具 29 项）的完整目标及 UTF-16 范围全部通过。复测 overlay 修改和关闭后回滚、文件和包图变更、外部和不完整包、测试变体与取消，既有导航结果无退化。
- [x] WP3 · `reader-subtypes` 的集合为 `ReadCloser`、`Service`、`Base`、`Derived`、`Alias`，`service-supertypes` 的集合为 `Reader`、`ReadCloser`、`Resetter`。覆盖别名、传递嵌入和独立声明的兼容接口，并核对去重、顺序与标识符范围。
- [x] WP3 · 为结构体嵌入、递归自边、`fromRanges` 和空接口保留夹具或回归断言。记录 A、B 的确认结果，分别以含函数字面量的调用树和单根嵌套模块用例验证所选行为。层级严格对照覆盖既有 20 项及新增用例，每个差异均有明确预期和理由。
- [x] WP4 · 从仓内 JSON 重放层级校验和计时对照成功。现场采集后，GoLand 文档与磁盘夹具均恢复，编辑后结果与回滚结果按 UTF-16 范围逐项校验。对完整结果匹配的同条件样本报告双方三档 P50/P95 与样本数，逐项表列出搜索范围、目标、范围或错误的差异。
- [x] WP4 · 前序计划引用的实现提交均可由 Git 解析。README、愿景、基线说明、夹具和验证命令链接可用。发布版通过所有 Python 单测及全量工作区回归，`cargo check --workspace --locked`、`cargo test --workspace --locked` 均通过且零告警。

## 实现依据

### GoLand 安装包 REA 字节码证据

GoLand `2026.3.0.1` 构建号为 `GO-263.7307.354`。检查对象为 `<GOLAND_HOME>/plugins/go-plugin/lib/modules/intellij.go.impl.jar`，SHA-256 为 `2A13F1A753D5E15DB70C5758E17DB1DC28CC790E1D329E574D8B6A078D636784`。可用安装包的 `jbr/bin/java.exe` 和 `lib/intellij.libraries.asm.jar` 中的 ASM `Textifier` 复核下列类与方法。

| 范围 | 类与方法 | 字节码确认的处理流程 |
| --- | --- | --- |
| 模块发现与归组 | `VgoStatusTracker.findAllGoModuleSettingsFiles`、`VgoModuleInfoProviderImpl.getModuleSettingsMerged`、`VgoWorkspaceModelUpdater.buildEntities` | 遍历 IDE 内容根，按 SDK 工作区支持情况发现 `go.mod` 和 `go.work`。合并关联的工作区设置并去重，为独立模块及工作区成员分别生成实体并绑定模块 ID。 |
| 导入与包身份 | `VgoImportResolver.innerResolve`、`VgoDependencyImpl.getRoot`、`GoPackage.equals/hashCode` | 先尝试解析到 SDK 源码，再依据来源文件所属的模块或工作区解析依赖。`getRoot` 优先使用替换项。包相等性与哈希以目录集合及包名为依据。 |
| 并发加载与失败边界 | `VgoOptimizedModuleInfoProviderImpl.getInfosAsync` | `forEachConcurrent` 按模块配置处理，配置内暂存 `BuildList`，通过 `ConcurrentLinkedQueue` 汇集结果。无效根被跳过，JSON 解析失败记日志后继续，独立模块缺有效依赖列表时可按声明建模。其他异常可在清理后上抛。 |
| 模块元数据与诊断 | `GoListOutputParser.readDependencies`、`GoUnresolvedReferenceInspection.reportUnresolvedImportReference` | 此依赖元数据链路运行 `go list -m -json all`，解析模块字段，无 `DepsErrors` 或 `ImportStack` 字段。未解析导入由检查器单独报告。Rust 保留包级 `go list` 的结构化错误格式。 |
| 调用层级目标与边 | `GoCallHierarchyProvider.getTarget`、`GoCallerHierarchyTreeStructure.search`、`GoCalleeHierarchyTreeStructure$1.visitExpression/visitFunctionLit` | 从声明或引用解析 `GoNamedSignatureOwner`。调用者搜索消费 `GoReferencesSearch`，也包括函数值和方法值引用。被调方遍历表达式，跳过函数字面量体。 |
| 调用归属与展示 | `GoCallHierarchyTreeStructure.getEnclosingElement/buildChildren`、`GoCallHierarchyNodeDescriptor.navigate/highlightUsages` | 最近的 `GoSignatureOwner` 可为函数字面量。构建树时过滤直接自边，默认关闭 `WithInterfaceCallsAction`。节点可导航到首个引用，引用的父 PSI 元素用于高亮，可覆盖限定引用或完整调用表达式。 |
| 类型层级方向 | `GoTypeHierarchyProvider.getTarget`、`GoTypesHierarchyTreeStructure.buildChildren`、`GoInheritorsSearch.processQuery/getInterfaceType`、`GoGotoSuperHandler$GoSuperSearch.processQuery` | 解析 `GoTypeSpec` 并在指定搜索范围内收集唯一节点。`getInterfaceType` 取底层接口，下行校验候选是否满足目标接口，上行校验目标是否满足候选接口。两边使用直接和完整方法集，纳入传递嵌入和结构兼容接口。 |
| 精确匹配 | `GoPsiImplUtil.calcUnderlyingType/calcAllMethodsWithSubstitutionSource`、`GoTypeUtil.checkMethod` | 沿命名类型、别名与实例化计算底层类型，递归收集嵌入方法时防环并保留泛型替换来源。`checkMethod` 校验实例化签名和指针接收者约束。 |
| 候选与反向索引 | `GoGotoUtil.processMethodStructs/processMethodInterfaces/processTypeSpec/processInheritedTypes` | 分别使用 `GoMethodFingerprintIndex` 与 `GoMethodSpecFingerprintIndex` 查具体方法和接口方法声明，键由方法名和 `getArity`（输入参数计数）组成。结合 `GoTypeSpecInheritanceIndex`、`GoMethodSpecInheritanceIndex`、`GoTypeAliasIndex` 沿命名类型、嵌入和别名扩展。 |

### Rust 对应模块与实现取舍

下列行号以 `main` 的 `6e6f0a7` 为准。

| 工作包 | 现状与方案 | 源码锚点 |
| --- | --- | --- |
| WP1 | 显式目录先按 Go 环境归组，当前失败会沿 `Result` 提前退出，包错误的记录早于规范 ID 重映射。先按组暂存，成功后再提交包与别名，最后绑定错误和组成员。相同构建复用规范包时保留各组成员关系。维持安全重命名入口对完整包图的要求。 | [workspace_roots.rs 12–43](../../src/workspace_roots.rs#L12-L43) · [workspace_roots.rs 46–146](../../src/workspace_roots.rs#L46-L146) · [workspace.rs 30–50](../../src/workspace.rs#L30-L50) · [workspace.rs 72–151](../../src/workspace.rs#L72-L151) · [workspace.rs 473–522](../../src/workspace.rs#L473-L522) · [workspace.rs 567–585](../../src/workspace.rs#L567-L585) · [workspace.rs 648–776](../../src/workspace.rs#L648-L776) · [scheduler.rs 73–103](../../src/scheduler.rs#L73-L103) |
| WP1 | 裸包 ID 和导入栈末项的匹配约束到来源组及兼容测试变体。组内回退保留共享依赖的显式成员关系和映射。缺乏归属信息的错误保留原始字段并在组内报告。 | [Workspace::resolve_import 253–272](../../src/workspace.rs#L253-L272) · [package_errors.rs 10–62](../../src/semantic/diagnostics/package_errors.rs#L10-L62) · [imports.rs 30–116](../../src/semantic/diagnostics/imports.rs#L30-L116) · [hierarchy.py 10–59](../../tools/workspace_checks/hierarchy.py#L10-L59) |
| WP2 | `CandidatePackages` 的接口查实现路径仍扫描磁盘文本并扩张导入者，随后 `ImplementationSearch` 逐文件索引并精确匹配。复用已有 `MethodShape`、轻量形状提取、`InterfaceSources` 和 `FileIndex`，在 GoLand 方法名与输入参数计数的思路上补充返回值计数与变参形状。用反向记录覆盖提升方法及命名 RHS。在包图安装、文件监听或覆盖内容变化时更新索引，保留完整方法集判定。 | [implementation.rs 294–359](../../src/semantic/implementation.rs#L294-L359) · [implementation.rs 386–487](../../src/semantic/implementation.rs#L386-L487) · [candidates.rs 15–123](../../src/semantic/implementation/candidates.rs#L15-L123) · [candidates.rs 267–365](../../src/semantic/implementation/candidates.rs#L267-L365) · [interface_probe.rs 193–240](../../src/semantic/implementation/interface_probe.rs#L193-L240) · [method_sets.rs 20–95](../../src/semantic/implementation/method_sets.rs#L20-L95) · [index.rs 24–94](../../src/semantic/index.rs#L24-L94) · [index.rs 141–168](../../src/semantic/index.rs#L141-L168) · [semantic.rs 370–436](../../src/semantic.rs#L370-L436) |
| WP3 | 接口上行目前只取直接嵌入父项，下行的实现结果过滤其他接口。复用完整方法集计算及精确匹配，为 Type Hierarchy 传入显式关系方向，保留具体类型的直接嵌入分支。Call Hierarchy 目前按命名声明聚合，遍历闭包体并记录标识符范围，最终闭包行为依 A 定案。 | [type_hierarchy.rs 251–302](../../src/semantic/hierarchy/type_hierarchy.rs#L251-L302) · [type_hierarchy.rs 320–385](../../src/semantic/hierarchy/type_hierarchy.rs#L320-L385) · [type_hierarchy.rs 410–529](../../src/semantic/hierarchy/type_hierarchy.rs#L410-L529) · [method_sets.rs 259–451](../../src/semantic/implementation/method_sets.rs#L259-L451) · [call.rs 105–126](../../src/semantic/hierarchy/call.rs#L105-L126) · [call.rs 168–237](../../src/semantic/hierarchy/call.rs#L168-L237) · [call.rs 252–310](../../src/semantic/hierarchy/call.rs#L252-L310) · [hierarchy/tests.rs 229–447](../../src/semantic/hierarchy/tests.rs#L229-L447) |
| WP4 | 采集器目前要求保存态文档且只产出 GoLand 首查和热查数据。比较器已支持共享编辑前缀及 `afterEditTimingsMs`。更新采集与校验预期，逐项记录改为一致的结果和有意差异。 | [capture_goland_hierarchy.groovy 83–183](../../tools/capture_goland_hierarchy.groovy#L83-L183) · [check_goland_hierarchy.py 55–94](../../tools/check_goland_hierarchy.py#L55-L94) · [check_goland_hierarchy.py 217–235](../../tools/check_goland_hierarchy.py#L217-L235) · [bench_common.py 456–546](../../tools/bench_common.py#L456-L546) |

用精确结果、失效回归和独立计时验证两点。反向记录要覆盖泛型别名、跨包提升、外部依赖及不完整源码的候选。专门测量索引建表对就绪延迟的影响。GoLand 的模块配置与 Rust 的构建组用于对照隔离边界，Rust 包级错误归属以实际 Go 工具输出和规范包图为准。

## 实施步骤

1. **WP1**。先为同名独立模块、无位置错误、测试变体和单组失败建立回归，修正按组暂存、包 ID 映射、导入回退及两处诊断消费方。交付健康与失败组可同时存在的包图契约，以及动态增删的层级回归。
2. **WP2**。在稳定的组和规范包身份上，为接口查实现建立方法形状倒排与嵌入反向记录，接入增量失效和精确匹配。交付含测试的首查基准、分阶段数据及 49 项实现跳转对照。
3. **WP3**。复用 WP2 的候选索引和完整方法集判定，实现接口关系的显式上下行方向。按 A、B 的确认结果补齐行为、用例和差异说明。
4. **WP4**。冻结最终结果集后采集 GoLand 编辑后计时，验证恢复和脱敏，归档 JSON、双边指标及逐项差异。用 `9ec6891`、`4cdf87b`、`1fc91d2`、`5c2dd50` 修正前序计划中不可解析的 `a17df2b`、`32f0c8a`、`97fc13d`、`a5a8e45`，同步 README、愿景和基线文档，执行全量验证。

WP1 的多根集成用例可与包图和诊断实现并行。WP1 的组身份契约稳定后，WP2 的指纹提取和 WP3 的语义夹具可分别准备，由 WP2 先合入索引接口。WP4 的采集器编辑与恢复逻辑可独立开发，最终快照和文档以 WP3 的验收结果为准。

## 验证计划

### `6e6f0a7` 专项复测基线

| 用例 | 包图 | 首查耗时（ms） | 主进程读取 |
| --- | --- | ---: | --- |
| `reader-subtypes`，含测试夹具 | 120 包 / 702 文件 | 1,076.34 | 约 1,515 次 / 6.73 MB |
| `reader-subtypes`，移除测试包 | 2 包 | 0.87–1.23 | 待补测 |

`1,076.34 ms` 是本轮专项首查观测值。验收前在同条件下分别采集基线和发布候选的独立进程样本，采用最近秩计算首查 P50/P95。前序归档另有全体 `typeHierarchy/subtypes` 三用例汇总，首查 P50/P95 为 `135.3790 / 1230.1994 ms`，热查为 `0.5177 / 0.7043 ms`，编辑后为 `0.6931 / 0.8600 ms`，样本数为 `3 / 90 / 90`，属于前序发布版口径。既有层级校验通过 20 项，其中 10 项结果一致，可直接比较双边耗时。[前序 GoLand 层级基线](../goland-hierarchy-baseline.md)

### 构建与回归命令

沿用项目工具链、profile、features、target 和环境配置的 Cargo 产物目录。禁止传入 `--target-dir`、覆盖 `CARGO_TARGET_DIR` 或 `CARGO_BUILD_BUILD_DIR`，以及另建编译目录重试。从仓库根目录执行。

```powershell
cargo check -p rs-gopls --locked
cargo test -p rs-gopls --locked workspace
cargo test -p rs-gopls --locked semantic::diagnostics
cargo test -p rs-gopls --locked semantic::implementation
cargo test -p rs-gopls --locked semantic::hierarchy
cargo check --workspace --locked
cargo test --workspace --locked
cargo build -p rs-gopls --release --locked

$target = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
$server = Join-Path $target 'release\rs-gopls.exe'
$hierarchy = '.\tools\fixtures\hierarchy'
$fixture = Join-Path $hierarchy 'goland-hierarchy.json'
$realProject = $env:RS_GOPLS_REAL_PROJECT
$implementationFixture = '.\tools\fixtures\implementation-navigation'

Push-Location $hierarchy
try {
    go test ./...
} finally {
    Pop-Location
}
python -B -m unittest discover -s tools -p 'test_*.py'
python -B .\tools\check_workspace_updates.py $server --module hierarchy
python -B .\tools\check_workspace_updates.py $server --module implementation
python -B .\tools\check_workspace_updates.py $server
python -B .\tools\check_goland_implementations.py $server --workspace $realProject --fixture .\tools\fixtures\localchat-goland-implementations.json
python -B .\tools\check_goland_implementations.py $server --workspace $implementationFixture --fixture (Join-Path $implementationFixture 'goland-implementations.json')
python -B .\tools\fixtures\rename\replay.py lsp $server
cargo fmt --all -- --check
git diff --check
```

`RS_GOPLS_REAL_PROJECT` 指向既有真实项目语料的固定修订（`532228d3d36bd9f0fffb4bda35c180485b2eadd6`），其 20 个查询与 29 个补充查询均核对完整落点。合成项目另覆盖同名方法不同形状、跨包嵌入、命名 RHS 与 overlay。[GoLand 实现跳转基线](../goland-implementation-baseline.md)

### 首查采样与 GoLand 现场对照

- 先在 `6e6f0a7` 再在发布候选上各记录 30 个独立进程样本，使用各自修订的严格校验预期，以同一夹具、打开文件和查询顺序复测。保留含测试包的原始夹具，另用相同源码的无测试副本单独回放 `reader-subtypes` 做诊断性对照。每次等包图就绪并让 `Reader` 成为首个子类型查询，保持结果缓存未命中。记录 `prepareTypeHierarchy` 加展开的端到端往返，单列展开及索引建设耗时，首查指标沿用同一计时边界。
- 在同一主机固定发布版 SHA-256、项目修订、Go SDK（层级前序基线为 Go `1.26.3 windows/amd64`，`CGO_ENABLED=1`）、GoLand 构建号（`GO-263.7307.354`）、`All` 搜索范围、打开文件和编辑顺序。GoLand 在 Smart Mode 下计时，覆盖光标解析、树展开和范围提取。rs-gopls 计时覆盖 stdio 请求往返。双方都排除界面操作和进程启动，索引就绪耗时单列。
- 热查按 5 次预热后 30 次采样。编辑档在同一文档插入相同的首行注释，提交 PSI 或发送 LSP 变更，计时查询后恢复，并校验原始内容与落点。按逐项目标及 UTF-16 范围匹配筛选 GoLand 计时样本，差异及原因逐项入表。记录结果缓存状态，采集峰值工作集时注明采样周期及进程范围。[导航性能对照](../PLANS.md#导航性能对照)

```powershell
$queries = Join-Path $hierarchy 'hierarchy-queries.json'
$revision = git rev-parse --short HEAD
$readerFirst = foreach ($run in 1..30) {
    $sample = Join-Path $target "hierarchy-first-$revision-$run.json"
    python -B .\tools\compare_hierarchy.py $server --workspace $hierarchy --fixture $queries --iterations 1 --json $sample | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Hierarchy sample $run failed" }
    (Get-Content -Raw $sample | ConvertFrom-Json).cases.'reader-subtypes'.timings_ms.first[0]
}
$ordered = @($readerFirst | Sort-Object)
$firstSummary = [pscustomobject]@{
    Revision = $revision
    Samples = $ordered.Count
    P50Ms = $ordered[([int][math]::Ceiling(0.50 * $ordered.Count) - 1)]
    P95Ms = $ordered[([int][math]::Ceiling(0.95 * $ordered.Count) - 1)]
}
$firstSummary | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $target "hierarchy-first-$revision-summary.json")

$env:GOLAND_EXE = '<GOLAND_HOME>\bin\goland64.exe'
$capture = Join-Path $target 'goland-hierarchy-timing.json'
python -B .\tools\compare_hierarchy.py $server --workspace $hierarchy --fixture $queries --capture-goland --goland-bench $capture --iterations 30 --json (Join-Path $target 'hierarchy-live.json')
python -B .\tools\check_goland_hierarchy.py $server --workspace $hierarchy --fixture $fixture --json (Join-Path $target 'hierarchy-check.json')
python -B .\tools\compare_hierarchy.py $server --workspace $hierarchy --fixture $fixture --json (Join-Path $target 'hierarchy-replay.json')
```

现场采集前用 `<GOLAND_HOME>` 对应的本机安装目录设置启动器，在 GoLand 打开夹具并等索引就绪。`$capture` 经脱敏和结果复核后归档为 `$fixture`。上述首查采样使用各修订自身的 `hierarchy-queries.json` 和验证器，生成的报告按修订保存在 Cargo 产物目录内。发布候选另记录端到端的索引就绪时间和 WP2 的分项报告。层级脚本使用 `GO111MODULE=on`、`GOWORK=off`、`GOPROXY=off`，双边计时以 SDK 一致的样本为准。

## 完成记录

- 2026-11-27 创建计划，核对 `main` 基线、前序记录、GoLand 构建号及插件字节码。既有专项复测数据已入表。等待用户确认及 A、B 选项，本阶段实施和验收待执行。
- 2026-11-27 用户已确认计划及 A1/B2 选项，状态更新为「进行中」，启动 Wave 0 基线采样。
- 2026-11-27 完成 Wave 0A 首查基线。HEAD 为 `6e6f0a7`，发布版 SHA-256 为 `4EB233783DC24CFE4FD314082E65035075B4233F9153B900B837FB137ED04962`。使用 Go `1.24.3 windows/amd64`（`CGO_ENABLED=1`），两组均串行启动独立进程并通过 20 项严格校验。含测试夹具（120 包 / 702 个 Go 源文件）的 30 次首查最近秩 P50 / P95 / Min / Max 为 `942.4979 / 981.9315 / 907.9094 / 992.4958 ms`。以一个逻辑处理器等效负载（整机 6.25%）为门槛，依据可观测常驻进程 CPU 用量补采 7 个样本。无测试副本（2 包 / 2 个 Go 源文件）的 10 次首查 P50 / P95 为 `0.7260 / 0.8737 ms`。首查主进程 I/O 中位数为读取 2 次 / 925 B、写入 4 次 / 2,610 B，含 stdio 并排除子进程。临时副本已删除。摘要及逐次报告位于 Cargo 产物目录，摘要文件为 `hierarchy-first-6e6f0a7-summary.json`。
- 2026-11-27 完成 WP1，前三项验收均通过。B2 回归确认，单根下未注册的嵌套 `go.mod` 不进入包图，显式注册后才加载。两个同名模块的无位置循环、未保存本地 `replace` 导入和测试变体分别按组验证。缺失目录、无效 `go.work` 和 `go list` 退出失败均只归属故障组，健康组仍能跳转，安全重命名会拒绝缺组的包图。另在发布版注入正常 JSON 后追加截断数据，确认该组已输出的包也不会进入图，健康组定义跳转和诊断保持正确。
  - Rust 在配置的 Cargo 产物目录通过 `cargo check -p rs-gopls --locked` 和 `cargo build -p rs-gopls --release --locked`。`workspace`、`semantic::diagnostics`、`semantic::rename`、`semantic::hierarchy` 分别通过 22、25、40、6 个测试，`cargo test -p rs-gopls --locked` 全部 267 个测试通过，构建和测试零告警。
  - 以 `target/release/rs-gopls.exe` 运行 `check_workspace_updates.py`。`hierarchy` 和 `diagnostics` 各通过 2 个用例，`rename` 通过 18 个，`graph_refresh` 与 `actions` 分别通过 4 个和 2 个。动态根回归逐步核对调用及类型层级目标、UTF-16 范围和诊断。全部组加载失败时，文档报告组级错误，导航请求返回加载失败，后续查询可以重试。
- 2026-11-27 WP4 全量工作区回归发现 `overlay-disk-restore` 的最终定义落点仍使用旧行号。该用例在保留大小与修改时间的情况下替换磁盘内容，关闭缓冲区后，旧的导航源码缓存重建了已失效的声明索引。修复后，关闭缓冲区采用同一次磁盘读取校验导航源码和声明索引，保留内容一致的缓存。发布版随之重建，采样与回归以新 SHA-256 为准。
- 2026-11-27 发布版 SHA-256 为 `FE09E83C3DD4ECDA20A36D1F0BE80BF239C994ABFDB94ABA9EE8F13952A5D33A`。使用 GoLand `GO-263.7307.354` 和双方同一 Go `1.27.4 windows/amd64` SDK（`CGO_ENABLED=1`）现场采集。`All` 范围内的夹具结果通过 20/20 项严格校验，12 项完全一致、8 项有意差异。GoLand 的 600 次未保存编辑查询和 600 次回滚查询逐项通过目标及 UTF-16 范围校验，磁盘源码字节和修改时间均一致。归档每项含 1 次首查、30 次热查和 30 次编辑后采样，两个重放入口均通过。[脱敏归档](../../tools/fixtures/hierarchy/goland-hierarchy.json) · [逐项对照](../goland-hierarchy-baseline.md)
  - 12 个共同用例的 P50 / P95（ms）依首查、热查、编辑后顺序，样本数分别为每端 12、360、360。GoLand 为 `2.2250 / 225.4815`、`0.2310 / 53.5711`、`2.7195 / 76.5874`，rs-gopls 为 `1.6411 / 70.6945`、`0.2987 / 0.6628`、`4.9546 / 9.4070`。
  - 50 ms 间隔测主进程及可观测后代。GoLand 主进程生命周期峰值 / 进程树采样峰值为 `2,621,825,024 / 2,653,528,064 B`（333 次采样），rs-gopls 为 `31,727,616 / 41,177,088 B`（173 次采样）。
- 2026-11-27 用最终发布版及与 `6e6f0a7` 基线相同的 Go `1.24.3 windows/amd64`（`CGO_ENABLED=1`）重采 `Reader`。30 个合格独立进程均通过 20 项严格校验。`prepareTypeHierarchy` 与 `subtypes` 的合计首查 P50 / P95 由 `942.4979 / 981.9315 ms` 降为 `29.6240 / 35.3380 ms`（`31.82× / 27.79×`）。
  - 完整包图为 120 包、702 个 Go 源文件及 1 个生成条目。最大包图的索引构建 / 从调度到就绪为 `75.247 / 82.314 ms`、`79.422 / 86.893 ms`，从创建进程到 `workspace/symbol` 探针完成为 `448.7054 / 493.2175 ms`。请求中推进索引为 `25.570 / 31.162 ms`。候选探测、额外解析、方法集匹配及嵌入扫描分别为 `26 / 38`、`0 / 0`、`75 / 104`、`19 / 32 µs`。
  - 索引内部每次读取源码 701 次、6,244,682 B。首查期间的 Windows 主进程 I/O 读取为 `427 / 526` 次、`2,999,582 / 3,426,726 B`（包含 stdio）。Rust 专项测试验证同一内容版本的候选探测和精确解析复用已读源码。
  - 无测试副本 2 包、2 个 Go 源文件，10 个样本首查为 `0.8860 / 1.2553 ms`，索引构建、就绪以及探针为 `0.055 / 0.091`、`0.222 / 0.310`、`89.5513 / 97.6498 ms`。索引读取 0 次，主进程首查 I/O 每样本读取 2 次、857 B。副本已清理。
  - 50 ms 采样并用进程创建时间验证后代归属。含测试与无测试的主进程 / 进程树峰值分别为 `24,354,816 / 48,726,016 B`（338 次）和 `8,364,032 / 28,131,328 B`（33 次）。Cargo 产物目录保留 `hierarchy-first-wp4-final-summary.json`、`hierarchy-first-wp4-final-no-tests-summary.json`、`hierarchy-wp4-live.json` 和 `hierarchy-wp4-replay.json`。[性能基线](../goland-hierarchy-baseline.md)
- 2026-11-27 层级严格对照确认 `Reader` 的子类型为 `ReadCloser`、`Service`、`Base`、`Derived`、`Alias`，`Service` 的超类型为 `Reader`、`ReadCloser`、`Resetter`。Rust 与工作区回归覆盖结构体嵌入、递归自边、UTF-16 调用范围、空接口、测试变体、覆盖修改与恢复、取消及包图刷新。A1 闭包归属和 B2 嵌套模块显式注册均通过专项测试。
- 2026-11-27 最终回归结果如下。

  | 门禁 | 结果 |
  | --- | --- |
  | `cargo check --workspace --locked` | 通过，零告警 |
  | `cargo test --workspace --locked` | 286 个测试通过，零告警 |
  | `cargo build -p rs-gopls --release --locked` | 通过，零告警，发布版 SHA-256 如上 |
  | 层级夹具 `go test ./...` | 通过 |
  | `python -B -m unittest discover -s tools -p 'test_*.py'` | 35 / 35 通过 |
  | `check_workspace_updates.py` | 全部 12 个模块，56 / 56 通过，含 `overlay-disk-restore` |
  | `check_goland_implementations.py` | 补充夹具 29 / 29，固定修订 `532228d3` 真实项目 20 / 20。按语料记录核验目标和 UTF-16 范围，三个依赖 `testdata` 落点使用约定的排除项。 |
  | `fixtures/rename/replay.py lsp` | 12 / 12 通过 |
  | `check_goland_hierarchy.py` 与 `compare_hierarchy.py` | 两条仓内归档重放均通过，20 / 20 严格对照，12 个共同计时用例 |
  | `cargo fmt --all -- --check` 与 `git diff --check` | 均通过 |

- 2026-11-27 核验前序计划的四个实现提交及相关文档的 Markdown 相对链接与锚点。补齐 Track C 安全重命名入口、Track F 两项索引优化与归档重放命令，核对改动范围内的 JSON、Markdown 和脚本已脱敏。[README](../../README.md) · [项目愿景](../vision.md)

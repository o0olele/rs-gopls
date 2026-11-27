# GoLand 层级导航与性能基线

Track F 发布候选通过 20 项调用与类型层级严格校验。12 项的完整目标集合、标识符落点和适用的调用范围与 GoLand 一致，8 项保持已确认的产品差异。[Track F 前序计划](plans/2026-11-27-go-hierarchy-foundation.md) · [Track F 加固计划](plans/2026-11-27-go-hierarchy-foundation-hardening.md)

## 现场条件

| 条件 | 2026-11-27 现场值 |
| --- | --- |
| 主机与 GoLand | 同一台 Windows amd64 主机，GoLand 2026.3.0.1，构建号 `GO-263.7307.354`，启动器 `<GOLAND_HOME>/bin/goland64.exe`，启动器 SHA-256 `473688EA3B43F3F52844028DCA95833DFDC6A407DC5F874FF7E3055733C9D7BF` |
| rs-gopls 发布版 | 配置的 Cargo 产物目录下的 `release/rs-gopls.exe`，SHA-256 `FE09E83C3DD4ECDA20A36D1F0BE80BF239C994ABFDB94ABA9EE8F13952A5D33A` |
| Go SDK | 两端使用同一安装的 Go `1.27.4 windows/amd64`，`CGO_ENABLED=1`。GoLand 注册的唯一 SDK 根目录与 LSP 侧通过 `PATH` 和 `GOROOT` 选中的目录一致，采集器逐项检查项目 SDK 版本。LSP 使用 `GOTOOLCHAIN=local`，夹具环境设置 `GO111MODULE=on`、`GOWORK=off`、`GOPROXY=off`。 |
| 夹具与搜索范围 | [层级模块](../tools/fixtures/hierarchy)，源码和模块文件集合 SHA-256 `04409DD7BA600E322961E9967C22246A7D6258598D6F45F55CB2E2A0283C6615`。GoLand 使用 `All`，对照记录夹具内的目标。含测试包的源码保持原样。 |
| 打开的源文档 | 按清单顺序加载 `hierarchy.go` 和 `worker/worker.go`。GoLand 预载文档和 PSI，rs-gopls 在 stdio 会话发送 `didOpen`。 |
| 索引与缓存 | 正在运行的 GoLand 进入 Smart Mode，保留既有 IDE 缓存。rs-gopls 在新进程通过 `workspace/symbol` 就绪探针后开始测量，保留包图、索引和查询缓存。双方先按清单顺序各查询 20 项一次，再依序为每项执行 5 次校验预热、30 次热查和 30 次编辑循环。首查指本轮每项第一次查询，两个工具均可能复用此前查询形成的缓存。 |
| 未保存编辑 | 每次在查询源文档首行插入 `// rs-gopls benchmark overlay` 与换行，GoLand 提交 PSI，rs-gopls 接收 `didChange`。查询完成后恢复文档，撤掉一行偏移来核对 UTF-16 范围，并用原位置重查。 |
| 计时边界 | GoLand 在编辑并提交 PSI 后计时，覆盖 Smart Mode 中的目标解析、树展开及范围提取。rs-gopls 在客户端发出 `didChange` 后计时，从 stdio 请求到 JSON 响应，展开计入前置的 `prepare*` 请求。若服务端尚在处理变更通知或失效索引，相应等待会计入往返。三档均排除进程启动、界面操作、客户端的编辑发送、回滚和计时后的结果校验。索引就绪等待单列在独立进程实验中。 |

GoLand 每次编辑都禁止落盘并检查文档仍为未保存态。600 次编辑查询及 600 次回滚查询的完整目标和 UTF-16 范围均与原查询一致。回滚后检查编辑器文本、已保存及 PSI 已提交状态，并核对磁盘字节和修改时间。Git 检查确认夹具目录只新增脱敏归档。[归档](../tools/fixtures/hierarchy/goland-hierarchy.json) · [GoLand 采集器](../tools/capture_goland_hierarchy.groovy) · [导航性能对照](PLANS.md#导航性能对照)

## 逐项正确性对照

严格校验按目标相对路径、UTF-16 `selectionRange` 和调用边的 `fromRanges` 比较，并检查声明外层范围包含标识符。两端完整结果和有意差异各自按预期验证，20/20 项通过。[查询清单](../tools/fixtures/hierarchy/hierarchy-queries.json) · [层级校验器](../tools/check_goland_hierarchy.py)

### 完全一致的 12 项

| 用例 | 查询锚点 | GoLand 与 rs-gopls 共同结果 |
| --- | --- | --- |
| `prepare-function` | 函数声明 | `Dispatch` |
| `prepare-cross-package-call` | 跨包调用 | `Leaf` |
| `prepare-interface-call` | 接口方法调用 | `ReadCloser.Close` |
| `prepare-method-call` | 具体类型方法调用 | `Derived.Run` |
| `leaf-callees` | `Leaf` | 空集合 |
| `prepare-interface` | 接口声明 | `Service` |
| `prepare-struct` | 结构体声明 | `Derived` |
| `prepare-alias-use` | 跨包类型别名引用 | `Alias` |
| `reader-subtypes` | `Reader` | `ReadCloser`、`Service`、`Base`、`Derived`、`Alias` |
| `read-closer-subtypes` | `ReadCloser` | `Service`、`Derived`、`Alias` |
| `service-supertypes` | `Service` | `Reader`、`ReadCloser`、`Resetter` |
| `read-closer-supertypes` | `ReadCloser` | `Reader` |

### 有意差异的 8 项

范围以零基 UTF-16 位置记为 `行:[起始列,结束列)`。两端在同一夹具上得到相同的准备目标，请求均成功。LSP 以标识符作为调用点，保留递归自边和结构体直接嵌入导航。

| 用例 | GoLand | rs-gopls 与差异原因 |
| --- | --- | --- |
| `leaf-callers` | `Dispatch`、`TestLeaf`、`Recursive`、`Run`。限定引用范围覆盖 `Dispatch` 中的 `6:[41,52)`、`6:[66,77)` 和 `TestLeaf` 中的 `17:[17,28)`。 | 相同调用者，三个 `Leaf` 标识符范围为 `6:[48,52)`、`6:[73,77)`、`17:[24,28)`。 |
| `recursive-callers` | 调用者 `Recurse`，限定引用范围 `10:[8,24)`。GoLand 调用树过滤直接自边。 | `Recurse` 的标识符范围为 `10:[15,24)`，并保留 `Recursive` 自边 `49:[22,31)`。 |
| `interface-method-callers` | 调用者 `Dispatch`，限定引用范围 `6:[8,20)`。 | 相同调用者，`Read` 标识符范围为 `6:[16,20)`。 |
| `dispatch-callees` | `Read`、`Close`、`Leaf`、`Run`。完整调用表达式范围依次为 `6:[8,22)`、`5:[1,16)`、`6:[41,63)` 和 `6:[66,80)`、`6:[25,38)`。 | 相同被调目标，标识符范围依次为 `6:[16,20)`、`5:[9,14)`、`6:[48,52)` 和 `6:[73,77)`、`6:[33,36)`。 |
| `method-callees` | 被调目标 `Base.Read`、`Leaf`，完整调用表达式范围分别为 `53:[13,27)`、`53:[8,28)`。 | 相同被调目标，标识符范围分别为 `53:[21,25)`、`53:[8,12)`。 |
| `recursive-callees` | 被调目标 `Leaf`，完整调用表达式范围为 `47:[9,20)`、`49:[8,19)`，调用树过滤直接自边。 | `Leaf` 标识符范围为 `47:[9,13)`、`49:[8,12)`，并保留 `Recursive` 自边 `49:[22,31)`。 |
| `base-subtypes` | 空集合。类型树按接口满足关系搜索。 | `Derived` 直接嵌入 `Base`，下行保留该结构体嵌入边。 |
| `derived-supertypes` | `Reader`、`ReadCloser`、`Service`、`Resetter`。 | 保留同一组满足的接口，并额外返回直接嵌入的结构体 `Base`。 |

闭包体调用归属外层命名函数或方法，空接口省略全量类型枚举。这两项行为另由 Rust 专项测试覆盖。单根内的独立嵌套模块经显式注册后加入构建组，由工作区回归覆盖。[确认项和验证计划](plans/2026-11-27-go-hierarchy-foundation-hardening.md)

## 同 SDK 层级计时

以下为 12 项结果完全一致用例的现场采样。样本数为每端的首查／热查／编辑后次数，P50/P95 用最近秩计算，单位为 ms。三个调用者查询均有范围或目标差异，从耗时汇总排除。重放使用归档 GoLand 样本和本次运行的 LSP 样本。

| 请求 | 样本数 | GoLand 首查 | GoLand 热查 | GoLand 编辑后 | rs-gopls 首查 | rs-gopls 热查 | rs-gopls 编辑后 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `textDocument/prepareCallHierarchy` | 4 / 120 / 120 | 2.2250 / 4.7391 | 0.1553 / 0.3881 | 2.6907 / 3.2534 | 1.7382 / 2.2199 | 0.2503 / 0.3673 | 3.4815 / 5.4116 |
| `callHierarchy/outgoingCalls` | 1 / 30 / 30 | 0.7021 / 0.7021 | 0.2511 / 0.3860 | 0.6410 / 0.6894 | 4.0924 / 4.0924 | 0.3746 / 0.4960 | 5.4763 / 9.5350 |
| `textDocument/prepareTypeHierarchy` | 3 / 90 / 90 | 0.1994 / 0.2643 | 0.1534 / 0.2740 | 0.5388 / 2.7536 | 1.4242 / 1.4274 | 0.2205 / 0.2988 | 4.2460 / 7.0254 |
| `typeHierarchy/subtypes` | 2 / 60 / 60 | 14.1484 / 225.4815 | 4.2652 / 73.0231 | 38.0427 / 91.2230 | 0.9064 / 70.6945 | 0.6137 / 0.7611 | 6.8495 / 10.6053 |
| `typeHierarchy/supertypes` | 2 / 60 / 60 | 10.6941 / 11.3060 | 8.0411 / 13.4466 | 16.8412 / 24.2633 | 0.6175 / 6.2559 | 0.5079 / 0.6351 | 7.5035 / 11.4260 |
| 全部共同用例 | 12 / 360 / 360 | 2.2250 / 225.4815 | 0.2310 / 53.5711 | 2.7195 / 76.5874 | 1.6411 / 70.6945 | 0.2987 / 0.6628 | 4.9546 / 9.4070 |

Windows 采样间隔为 50 ms。主进程峰值取系统记录的进程生命周期峰值，进程树峰值取同一次采样中主进程和可访问后代工作集之和的最大值，共享页可能重复计入。GoLand 采样覆盖运行中的 IDE 现场抓取区间，该主进程的生命周期早于采样。rs-gopls 采样覆盖 benchmark 会话的就绪探针和查询。

| 现场进程 | 主进程峰值 | 进程树峰值 | 样本数与观测后代 |
| --- | ---: | ---: | --- |
| GoLand | 2,621,825,024 B（2,500.37 MiB） | 2,653,528,064 B（2,530.60 MiB） | 333，`conhost.exe`、`fsnotifier.exe`、`git.exe` |
| rs-gopls | 31,727,616 B（30.26 MiB） | 41,177,088 B（39.27 MiB） | 173，`go.exe` |

## Reader 子类型独立进程首查

Rust 基线与候选均使用 Go `1.24.3 windows/amd64`、`CGO_ENABLED=1` 与同一源码夹具摘要。每个样本串行新建发布版进程，打开相同两个源文档，等待 `workspace/symbol` 就绪并执行同序查询序列，令 `Reader` 成为首个子类型展开。首查时间合计 `prepareTypeHierarchy` 与 `subtypes` 的 stdio 往返，每轮用各版本预期检查 20 项。采样只按可观测常驻进程聚合 CPU 占用筛选，门槛为整机的 6.25%。下表 P50/P95 使用最近秩。

| 发布版 | 合格样本 | 包图 | 首查 P50 / P95（ms） |
| --- | ---: | --- | ---: |
| `6e6f0a7` | 30 | 120 包、702 个 Go 源文件 | 942.4979 / 981.9315 |
| 最终发布候选 | 30 | 120 包、702 个 Go 源文件，另有 1 个生成的非 `.go` 文件条目 | 29.6240 / 35.3380 |
| 候选加速倍数 | | | 31.82× / 27.79× |

WP4 修复文档关闭后的缓存失效并重新构建发布版后，重采 30 个完整夹具样本，另有 9 个候选超出 CPU 门槛。在临时副本移除 `hierarchy_test.go` 后的 10 个诊断样本均通过 CPU 门槛。无测试包的图有 2 包及 2 个 Go 源文件，`TestLeaf` 调用边按该副本调整预期。无测试的 `6e6f0a7` 基线首查 P50/P95 为 0.7260 / 0.8737 ms，10 个样本。

| 指标（P50 / P95） | WP2 完整夹具，30 样本 | WP3 完整夹具，30 样本 | 最终候选完整夹具，30 样本 | 最终候选无测试副本，10 样本 |
| --- | ---: | ---: | ---: | ---: |
| `Reader` 首查（ms） | 34.5841 / 41.1174 | 33.6898 / 36.8735 | 29.6240 / 35.3380 | 0.8860 / 1.2553 |
| 进程创建到探针就绪（ms） | 453.3481 / 497.3845 | 446.0819 / 472.6028 | 448.7054 / 493.2175 | 89.5513 / 97.6498 |
| 索引构建活动耗时（ms） | 77.122 / 83.638 | 75.885 / 80.254 | 75.247 / 82.314 | 0.055 / 0.091 |
| 索引调度到就绪（ms） | 81.295 / 88.483 | 80.051 / 84.405 | 79.422 / 86.893 | 0.222 / 0.310 |
| 查询中推进索引（ms） | 30.388 / 36.711 | 29.590 / 32.692 | 25.570 / 31.162 | 0 / 0 |
| 候选探测（µs） | 26 / 38 | 28 / 47 | 26 / 38 | 31 / 54 |
| 额外解析（µs） | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| 方法集匹配（µs） | 78 / 102 | 76 / 116 | 75 / 104 | 62 / 99 |
| 嵌入扫描（µs） | 91 / 127 | 19 / 33 | 19 / 32 | 13 / 19 |
| 索引源码读取（每样本） | 701 次 / 6,244,682 B | 701 次 / 6,244,682 B | 701 次 / 6,244,682 B | 0 次 / 0 B |
| 首查主进程读取（次数） | 506 / 558 | 496 / 542 | 427 / 526 | 2 / 2 |
| 首查主进程读取（B） | 3,357,250 / 3,652,267 | 3,345,351 / 3,509,604 | 2,999,582 / 3,426,726 | 857 / 857 |

源码读取来自索引内部计数。首查主进程读取取 Windows I/O 计数器在准备和展开两次请求前后的差，包含 stdio，进程范围限 rs-gopls。基线单次前期观测约 1,515 次 / 6.73 MB。无测试基线的 10 个样本均为 2 次 / 925 B。同一内容版本的候选探测与精确解析复用已读取的源码。

WP4 独立进程实验每 50 ms 采样，按进程创建时间校验父子关系。完整夹具的主进程和进程树峰值分别为 24,354,816 B（23.23 MiB）和 48,726,016 B（46.47 MiB），共 338 次采样，观测到 `go.exe` 和 `git.exe`。无测试副本的峰值分别为 8,364,032 B（7.98 MiB）和 28,131,328 B（26.83 MiB），共 33 次采样，观测到 `go.exe`。[加固计划及产物说明](plans/2026-11-27-go-hierarchy-foundation-hardening.md)

## 具体类型冷反查

前序 Track F 在 `1fc91d2` 加入工作区级接口方法名倒排索引。清除结果缓存并切换到方法集互不相交的具体类型时，新增磁盘探测为 0 次。含 `io/fs` 的 99 包合成项目在预热一种具体类型后切换到另一种，P50/P95 由 59 / 63 ms 降至 24 / 30 ms。既有真实项目用六轮交替独立进程核对完整落点及 UTF-16 范围，跨类型切换 P50/P95 分别降低 88.7% / 90.8%。第二阶段的接口子类型索引进一步用方法形状及嵌入反向记录缩小候选，由完整方法集判定关系。最终发布版在关闭覆盖文档时重新读取磁盘，覆盖等长且修改时间相同的磁盘更新回归。[实现跳转基线](goland-implementation-baseline.md) · [加固计划](plans/2026-11-27-go-hierarchy-foundation-hardening.md)

## 重放与现场采集

从仓库根目录运行。成对计时前，将 `<GO_SDK_HOME>` 替换为 GoLand 项目所用的 Go `1.27.4` SDK 安装目录。

~~~powershell
$goSdk = '<GO_SDK_HOME>'
$env:PATH = (Join-Path $goSdk 'bin') + ';' + $env:PATH
$env:GOROOT = $goSdk
$env:GOTOOLCHAIN = 'local'
$env:CGO_ENABLED = '1'
go env GOVERSION GOOS GOARCH CGO_ENABLED

$target = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
$server = Join-Path $target 'release\rs-gopls.exe'
python -B .\tools\check_goland_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
python -B .\tools\compare_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
~~~

现场重采需要在运行中的 GoLand 打开夹具目录，配置同一个 Go SDK 并等待 Smart Mode。将 `<GOLAND_HOME>` 替换为本机 GoLand 安装目录。校验结果与脱敏后，将临时输出归档至上述夹具路径。

~~~powershell
$env:GOLAND_EXE = '<GOLAND_HOME>\bin\goland64.exe'
python -B .\tools\compare_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\hierarchy-queries.json --capture-goland --iterations 30 --timeout 180 --goland-bench (Join-Path $target 'goland-hierarchy-live.json') --json (Join-Path $target 'hierarchy-live.json')
~~~

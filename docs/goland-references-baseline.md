# GoLand 查找引用基线

采集自 GoLand 2026.3 构建 `GO-263.7307.354` 中实际打开的 `localchat/server`。项目修订为 `532228d3d36bd9f0fffb4bda35c180485b2eadd6`，GoLand 选择 Go 1.27.4。脚本在运行中的项目通过 Go 专用 Find Usages 处理器查找当前符号，使用项目范围，关闭文本搜索，方法按当前声明查找。[GoLand 查找用法说明](https://www.jetbrains.com/help/go/find-highlight-usages.html)

[引用语料](../tools/fixtures/localchat-goland-references.json)包含 17 个查询，合计 111 条源码结果。几组查询从声明和用法分别开始，同一符号的集合应一致。所有位置均使用零起点 UTF-16 的半开区间。

| 样本 | GoLand 返回的源码用法 |
| --- | ---: |
| 局部变量 `execPath`、`authHeader`、`session` | 1、2、1 |
| 同文件函数 `corsMiddleware` | 1 |
| 同包 `GetMuxVars` | 3 |
| 跨文件字段 `APIRoot` | 30 |
| 跨包方法 `GetSession` | 1 |
| 跨包类型 `ReviewItemFilters` | 17 |
| 字段 `AttachmentID`，含测试使用位置 | 4 |
| `SessionStore.Get`、`UserStore.GetAll` | 1、2 |
| 具体方法 `App.Config`、`App.GetSessions` | 23、0 |

源码用法存入 `usages`，声明位置单独存入 `declaration`，供 `includeDeclaration` 验证。GoLand 还把 `GetMuxVars` 的 GoDoc 名称识别为语义注释链接，它在两种光标查询中都作为 `ide_usages` 保留。GoLand 的接口层级选项也可扩大方法用法的搜索范围，本基线固定为当前方法。

## 重新采集

在 GoLand 打开项目并等待索引完成。用以下命令把语料中的查询传给正在运行的 GoLand。脚本依次取得 Go 处理器、执行项目范围搜索、输出准确源码、声明和额外用法。它沿用普通工作区信任状态。

~~~powershell
$project = 'E:\Code\localchat\server'
$goland = 'E:\Apps\goland-2026.3.0.1.win\bin\goland64.exe'
$directory = Join-Path $env:TEMP 'rs-gopls-goland-baseline'
New-Item -ItemType Directory -Force -Path $directory | Out-Null
$fixture = Get-Content .\tools\fixtures\localchat-goland-references.json -Raw | ConvertFrom-Json
$queries = $fixture.cases | ForEach-Object {
    @($_.id, $_.query.path, $_.query.line, $_.query.character, $_.query.token) -join [char]9
}
@($project) + @($queries) | Set-Content -LiteralPath (Join-Path $directory 'reference-queries.tsv') -Encoding utf8NoBOM
$script = (Resolve-Path .\tools\capture_goland_references.groovy).Path
$started = [DateTime]::UtcNow
$capture = Start-Process -FilePath $goland -ArgumentList @('ideScript', ('"{0}"' -f $script)) -WindowStyle Hidden -Wait -PassThru
$locations = Get-Item -LiteralPath (Join-Path $directory 'reference-locations.tsv') -ErrorAction Stop
if ($capture.ExitCode -ne 0 -or $locations.LastWriteTimeUtc -lt $started) {
    throw 'GoLand reference capture did not produce a fresh result'
}
Get-Content -LiteralPath $locations.FullName -Encoding utf8
~~~

输出首行为 GoLand 构建号。每个结果由类别、查询 ID、文件、起始行列、结束行列和源码记号组成。

现场复查耗时时，将写入查询 TSV 的数组表达式改为 `` @($project, "# iterations`t30") + @($queries) ``。脚本在 Smart Mode 取得一份初始结果，然后按 `includeDeclaration` 两档各执行 30 次。每次核对完整的去重 UTF-16 集合与初始结果，采样输出为 `reference-timings.tsv`，包含查询 ID、声明选项、序号、毫秒耗时及源码、IDE 额外用法计数。用同轮产出的 `reference-locations.tsv` 核对同一构建和修订的历史集合，有集合差异的样本单列。计时从创建专用 Find Usages 搜索器开始，到去重结果枚举和坐标转换完成为止，包含相应的声明归并。这组数据对应索引就绪后的重复查询。启动和编辑到可见结果需另行计时。

## 检查源码引用

用 GoLand 基线所用的 Go 1.27.4 工具链运行候选版。检查器分别比较 `includeDeclaration=false` 的源码用法与 `includeDeclaration=true` 的源码用法加声明，并报告多余、缺失、重复结果。

~~~powershell
$sdk = Join-Path $env:USERPROFILE 'go\pkg\mod\golang.org\toolchain@v0.0.1-go1.27.4.windows-amd64'
$env:PATH = (Join-Path $sdk 'bin') + ';' + $env:PATH
$env:GOTOOLCHAIN = 'local'
$workspace = 'E:\Code\localchat\server'
$server = '.\target\release\rs-gopls.exe'
python -B .\tools\check_goland_references.py $server --workspace $workspace
~~~

### 历史性能对照

以下命令用于复现第三阶段的引用性能记录。新的性能测量直接使用 GoLand 的实际查询。[导航性能对照](PLANS.md#导航性能对照)

~~~powershell
python -B .\tools\compare_workspace_references.py $server --workspace $workspace --sessions 5 --iterations 80 --json .\target\references-performance.json
~~~

gopls v0.24.0 与 GoLand 在 34 组设置中有 32 组的集合一致。在 `UserStore.GetAll` 上，gopls 还返回 `internal/app/user_avatar_test.go` 中的一条具体实现调用。性能采样使用已有共同结果的九个案例，包含局部、同包和跨包各三例。

## 引用发布验收

发布版代码为 `be7641d`。17 个 GoLand 查询在包含和排除声明时共通过 34 组精确结果集。12 组工作区回归通过，覆盖 Go Modules、`go.work`、GOPATH、内外测试包、遮蔽、未保存新文件、磁盘和缓冲区变化；在包图加载中和分批扫描中也验证了取消。最终发布版同时通过 58 个 GoLand 类型定义用例、21 个 GoLand 声明位置、124 个合成声明位置和 16 个项目锚点。

在同一 Windows 主机上固定 Go 1.27.4，以 GoLand 源码集合为共同预期，轮换运行 rs-gopls 和 gopls v0.24.0。每套服务通过 5 次首次查询与 400 次范围编辑后的查询。其中局部与跨包各 135 次，同包 130 次，包含声明的选项交替使用。gopls 采用 `Save` 诊断配置，启动样本共享 Go 和系统缓存。

| 指标 | rs-gopls | gopls v0.24.0 |
| --- | ---: | ---: |
| 首次正确引用集 P50 | 592.63 ms | 1116.03 ms |
| 局部引用编辑后 P95 | 0.57 ms | 88.57 ms |
| 同包引用编辑后 P95 | 3.74 ms | 70.81 ms |
| 跨包引用编辑后 P95 | 3.44 ms | 92.37 ms |
| 采样进程树峰值工作集 | 47.1 MiB | 576.4 MiB |

旧功能同轮性能回归继续通过。声明跳转的范围编辑 P95 为 0.58 ms，第二阶段版本为 0.57 ms。类型定义最终版的 P95 为 0.56 ms，类型定义验收版本为 0.58 ms。

Zed 1.29.3 隔离配置的 `server — api.go` 窗口实际启动了 `rs-gopls-d6dcebec709f037f.exe`，与发布版 SHA-256 `D6DCEBEC709F037F2A6DA6B0F00A327793B346EE36C9188982290C04764BC359` 一致。该二进制的 LSP 初始化报告 `definitionProvider`、`typeDefinitionProvider` 和 `referencesProvider`，功能落点和集合由 stdio 套件验收。

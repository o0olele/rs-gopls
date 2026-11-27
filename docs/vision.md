# rs-gopls 项目愿景

用 Rust 实现 Go 语言服务器，为 Zed 提供准确、低延迟的代码导航。在真实 Go 项目中，即使编辑尚未保存，用户也能跳到正确的声明或类型，并找到绑定到同一符号的引用。

## 设计原则

- 以 GoLand 安装包中的 Go 语义引擎实现为设计蓝本。参考与功能相关的解析、索引和查询策略，结合 Go 规范与 Zed 的 LSP 接口在 Rust 中独立实现。
- Rust 负责增量解析、声明索引和语义解析，Go 工具链提供包图和构建元数据。编辑器中的未保存缓冲区优先于磁盘内容。
- 功能以 GoLand 在固定修订的真实项目中的可复现行为为参照。验收核对实际落点、完整结果集合和编辑提示，记录有意采用的行为差异。
- 性能直接对照 GoLand 的实际查询。用相同项目、Go SDK 和导航样本核对完整目标，再分场景比较启动、已索引查询和编辑后耗时。新能力应满足已有导航的正确性和响应速度要求。[导航性能对照](PLANS.md#导航性能对照)

## 当前能力与方向

已交付的 Go LSP 能力覆盖 Go Modules、`go.work`、GOPATH 和内外测试包。

- 声明跳转、类型定义跳转和查找引用已在 Zed 隔离配置中验证。[声明跳转基线](goland-definition-baseline.md) · [类型定义基线](goland-type-definition-baseline.md) · [引用基线](goland-references-baseline.md) · [第三阶段验收记录](plans/2026-11-25-go-navigation-phase-three.md)
- 接口与实现之间的双向导航。[实现跳转基线](goland-implementation-baseline.md)
- Track C 安全重命名支持 `textDocument/prepareRename` 与 `textDocument/rename`，校验重命名后的引用绑定和接口契约，并向支持版本化编辑的客户端返回工作区改动。[安全重命名基线](goland-rename-baseline.md) · [VS Code 试用与重放](../README.md#try-safe-rename-in-vs-code-on-windows)
- 悬停提示（Hover）与函数签名提示（SignatureHelp）。[Hover 与 SignatureHelp 基线](goland-hover-signature-baseline.md)
- 单文件嵌套与扁平大纲（DocumentSymbol），以及项目符号搜索（WorkspaceSymbol）。[文档与工作区符号基线](goland-symbols-baseline.md)
- 单文件语义高亮（SemanticTokens）、文档高亮（DocumentHighlight）、内联提示（InlayHint）与折叠范围（FoldingRange）。[单文件编辑器基线](goland-editor-baseline.md)
- 上下文补全覆盖作用域、成员和可导入符号，支持补全项解析及自动导入。[补全基线](goland-completion-baseline.md)
- 实时诊断覆盖语法、包加载、名称和类型错误，支持诊断推送与拉取。[诊断基线](goland-diagnostics-baseline.md)
- Track F 支持调用者、被调用者、接口关系与结构体嵌入的层级查询。客户端可在会话中增减工作区文件夹。具体类型冷反查复用接口方法名索引，接口子类型查找按方法签名指纹与嵌入反向记录筛选候选，再核对完整方法集。[层级与多工作区基线](goland-hierarchy-baseline.md) · [Track F 第二阶段计划](plans/2026-11-27-go-hierarchy-foundation-hardening.md)
- 格式化对未保存缓冲区运行 `gofmt`。代码操作支持整理导入，以及修复缺失或未使用的导入、未使用变量和短声明。[格式化与代码操作基线](goland-actions-baseline.md)

后续扩大真实项目的回归覆盖。实现跳转的 Zed 界面验收仍由第四阶段计划跟踪。[第四阶段计划](plans/2026-11-25-go-implementation.md)

## 层级专项校验与基准

从仓库根目录使用 README 中定位的发布版 `$server`，校验层级结果并重放 GoLand 归档计时。要复现双方计时对照，请使用与采集时一致的 Go SDK，确认 `go version` 报告 `go1.27.4 windows/amd64`。[构建与验证](../README.md#build-and-verify)

```powershell
python -B .\tools\check_goland_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
python -B .\tools\compare_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
```

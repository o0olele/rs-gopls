# rs-gopls

Go language server prototype in Rust. It follows the GoLand Go plugin's separation between syntax, declaration indexes, and reference resolution.

- `src/main.rs` manages the stdio LSP session, negotiates capabilities, and receives document and workspace notifications. `src/dispatch.rs` registers request handlers and their capabilities.
- `src/scheduler.rs` coordinates background package loads and cancellable searches, including requests waiting for a package graph or SDK index.
- `src/workspace.rs` uses `go list` to select files, track package identities and load errors, and resolve imports across workspace folders.
- `src/semantic.rs` manages document snapshots and cache invalidation. The modules in `src/semantic/` share Tree-sitter syntax, declaration indexes, and name and type resolution. Ranged edits update the open buffer and its UTF-16 line index.

## Build and verify

From this repository, with Rust, Go, Python and a C toolchain installed:

```powershell
cargo check -p rs-gopls --locked
cargo test -p rs-gopls --locked
cargo build -p rs-gopls --release --locked
$target = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
$server = Join-Path $target 'release\rs-gopls.exe'
python -B .\tools\compare_definitions.py $server --iterations 60
```

Launch the resulting executable as a stdio language server with a Go module, a go.work directory, or a GOPATH project as its LSP workspace root. The command above is a local latency check that verifies exact definition locations after unsaved edits. Add `--stdlib` to test a generated caller that resolves into the installed Go SDK's `net/http` package.

## Navigation performance

Use actual GoLand navigation queries as the performance reference. Fix the project revision, Go SDK, search scope, and query positions. Compare complete matching result sets and report startup, new queries on ready indexes, repeated queries, and edits as separate workloads. [Measurement protocol](docs/PLANS.md#导航性能对照) · [GoLand implementation timings](docs/goland-implementation-baseline.md#与-goland-直接对照) · [GoLand hierarchy timings](docs/goland-hierarchy-baseline.md)

## Definition coverage

Definition lookup covers imports, lexical and package names, and common field and method selections. The resolver follows named and anonymous types, explicit arguments on generic types, aliases, promoted members, container elements, and callable results. It covers range and select bindings, local callbacks, method values and expressions, and package variables initialized by calls. It uses the Go toolchain for active source files and package identities.

The server supports Go Modules, go.work, GOPATH, and test package variants. On clients that support dynamic file-watch registration, it refreshes indexes after changes to Go files and module/workspace files. Open editor buffers take precedence over disk content, including build-neutral new files in an already loaded package. The server refreshes Go toolchain metadata in the background and keeps local bindings and unaffected packages available while the graph loads.

Generic function calls cover common explicit, partial and inferred arguments, result substitution, constraint methods, and slice and map shape constraints. See the GoLand baseline for the validated editor behavior and workspace revisions.

Run the semantic and workspace acceptance checks with the release executable:

~~~powershell
python -B .\tools\check_definitions.py $server --stage 6
python -B .\tools\check_workspace_updates.py $server
~~~

Use the optional workspace argument on the first command to check the real project revision recorded in the navigation baseline. See the [first-stage definition roadmap](docs/plans/2026-11-25-go-definition.md), [second-stage plan](docs/plans/2026-11-25-go-definition-phase-two.md), and [third-stage navigation plan](docs/plans/2026-11-25-go-navigation-phase-three.md). The [GoLand navigation baseline](docs/goland-definition-baseline.md) records real-project targets.

## Type definition

The same Rust resolver handles `textDocument/typeDefinition`. It follows the type of a name, field, or inferred value to its Go declaration. The GoLand baseline records exact targets for named and anonymous types, aliases, function results, and common generic cases. [Type definition checks and performance comparison](docs/goland-type-definition-baseline.md)

## Find references

The server provides `textDocument/references` for local, package and cross-package Go uses, including test variants and open buffers. It verifies each candidate against the selected declaration, and larger searches run in cancellable batches. [GoLand reference sets and benchmarks](docs/goland-references-baseline.md)

## Go to implementation

Use Zed's **Go to Implementation** on a Go interface type or method to find implementations. From a concrete type or method, the same action finds matching interfaces or interface methods. Searches use the active build and test packages, including open buffers. [GoLand implementation sets and benchmarks](docs/goland-implementation-baseline.md)

## Hover and signature help

The server provides `textDocument/hover` and `textDocument/signatureHelp` (triggered on `(` and `,`). Hover renders Markdown Go signatures with inferred types, struct tags, generic parameters, package docs, and GoDoc comments (filtering compiler directives). Signature help resolves functions, methods, interface methods, generic instantiations, method expressions, function values, and builtins with expanded grouped parameters, variadic clamping, UTF-16 parameter highlight offsets, and unclosed-call editing tolerance. [GoLand hover and signature help baseline and benchmarks](docs/goland-hover-signature-baseline.md)

## Document outline and workspace symbols

`textDocument/documentSymbol` lists the current file's declarations in source order as a nested or flat outline, according to client capabilities. `workspace/symbol` searches active project and test packages by simple or qualified name and returns exact declaration locations. Both use unsaved editor buffers. [GoLand symbol baseline and benchmarks](docs/goland-symbols-baseline.md)

## Single-file editor features

The server provides semantic highlighting (`textDocument/semanticTokens/full`), document highlights for symbols and control flow (`textDocument/documentHighlight`), inlay hints (`textDocument/inlayHint`), and folding ranges (`textDocument/foldingRange`). All four use the current file, including unsaved edits. [GoLand single-file editor baseline](docs/goland-editor-baseline.md)

## Context completion

Completion offers names in scope, struct fields, methods, and importable packages and symbols. Clients can resolve documentation and auto-import edits on demand. The server checks that the document and package graph still match before returning an edit. [GoLand completion baseline](docs/goland-completion-baseline.md)

## Real-time diagnostics

Clients can pull diagnostics or receive published reports for open Go files. Reports cover syntax, package loading, name resolution, and type errors. Quick fixes use the same diagnostic data. [GoLand diagnostics baseline](docs/goland-diagnostics-baseline.md)

## Call and type hierarchy across workspace folders

Call hierarchy groups incoming and outgoing calls by declaration. Type hierarchy follows interface satisfaction and struct embedding. Adding or removing a workspace folder refreshes the active package graph. [GoLand hierarchy and multi-folder baseline](docs/goland-hierarchy-baseline.md)

Cold lookups from a concrete type to matching interfaces reuse an index of interface method names across queries. Interface subtype searches use method names, parameter and result counts, variadic shape, and reverse embedding links to narrow the candidate set before verifying full method sets. [Track F performance plan](docs/plans/2026-11-27-go-hierarchy-foundation-hardening.md)

From the repository root, use the release executable `$server` to check exact hierarchy results and replay the archived GoLand timings. For paired timings, use the same Go SDK as the capture and confirm that `go version` reports `go1.27.4 windows/amd64`. [Build and verify](#build-and-verify)

```powershell
python -B .\tools\check_goland_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
python -B .\tools\compare_hierarchy.py $server --workspace .\tools\fixtures\hierarchy --fixture .\tools\fixtures\hierarchy\goland-hierarchy.json
```

## Formatting and code actions

Document formatting runs the Go SDK's `gofmt` on the unsaved buffer and preserves its line endings. Organize Imports groups standard, third-party, and local imports while preserving comments and cgo preambles. Quick fixes can add missing imports, remove unused imports, and repair unused variables or short declarations. [GoLand formatting and code actions baseline](docs/goland-actions-baseline.md)

## Try rs-gopls in VS Code on Windows

### Official Go extension

With the Go extension (`golang.go` 0.58.0), set `go.alternateTools.gopls` to the absolute path of the server. From this repository, build the release executable and print the corresponding VS Code workspace setting.

```powershell
cargo build -p rs-gopls --release --locked
$target = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
@{ 'go.alternateTools' = @{ gopls = (Join-Path $target 'release/rs-gopls.exe') } } | ConvertTo-Json
```

Open the Go project in a trusted workspace. Run **Preferences: Open Workspace Settings (JSON)** and merge the printed entry into the workspace settings. Run **Go: Restart Language Server**, then use **Go: Locate Configured Go Tools** to check the path shown for `gopls`.

To try cross-package implementation navigation, open `tools/fixtures/implementation-navigation` as the Go workspace and apply the setting above. In `api/notifier.go`, press **Ctrl+F12** on `Notifier` to see the two `Sender` types in the `email` and `sms` packages. Press it on `Notify` to see their value- and pointer-receiver methods. [Example interface](tools/fixtures/implementation-navigation/api/notifier.go)

### Generic LSP Client

Clients that support versioned workspace edits can use `textDocument/rename` and, when supported, `textDocument/prepareRename`. The server checks that the edits preserve reference bindings and interface contracts, including across packages, test files, and unsaved buffers. Replay the acceptance fixture with `python -B .\tools\fixtures\rename\replay.py lsp $server`. [Build and verify](#build-and-verify) · [GoLand and rs-gopls rename results](docs/goland-rename-baseline.md)

After building the release executable, run these commands from the repository root. They create a fresh copy of the two-package Go example and open it in a dedicated VS Code profile. The Generic LSP Client starts this repository's release binary over stdio.

```powershell
$target = (cargo metadata --format-version 1 --no-deps --locked | ConvertFrom-Json).target_directory
$demo = python -B tools/fixtures/rename/replay.py copy
$workspace = Join-Path (Split-Path -Parent $demo) 'rs-gopls.code-workspace'
@{
    folders = @(@{ path = 'workspace' })
    settings = @{
        'glspc.server.command' = (Join-Path $target 'release/rs-gopls.exe')
        'glspc.server.languageId' = @('go')
    }
    extensions = @{ recommendations = @('zsol.vscode-glspc') }
} | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 -LiteralPath $workspace
code --profile 'rs-gopls Demo' --new-window $workspace --goto "${demo}/client/client.go:9:26"
code --profile 'rs-gopls Demo' --install-extension zsol.vscode-glspc
```

Trust the example workspace when prompted. In `client/client.go`, press **F12** on `Bonus` to jump to its declaration, or press **F2** and rename it to `Award` to update three files. To try a rejected rename, open `Capture` in `api/api.go` and rename `result` to `total`. The server reports the reference whose binding would change. Run `go test ./...` in the example's terminal to check your edits.

## Test in Zed on Windows

The extension uses Zed's existing Go language mode and registers `rs-gopls` as an opt-in language server. It accepts the worktree environment, including Go toolchain and GOPATH settings.

The release launcher opens the ready-to-test example using a separate Zed profile:

```powershell
.\tools\test_zed.ps1
```

In that window, trust the example project if Zed shows Restricted Mode. Open `main.go` from the project tree and use **Go to Definition** on `Hello`. The target is `greeting/greeting.go`. The separate Zed profile uses `rs-gopls` for `.go` files and disables language servers for `go.mod` and `go.work`.

To rebuild both artifacts using the configured Cargo output directory:

```powershell
rustup target add wasm32-wasip2
cargo build -p rs-gopls --release --locked
cargo build -p rs-gopls-zed --release --locked --target wasm32-wasip2
.\tools\test_zed.ps1
```

Use `-PrepareOnly` to stage a new server build in the isolated profile without opening a window. The launcher stages each build under a versioned filename so Zed can switch while an older binary is running. To use your own Go project, open its folder in the same test profile. In a different Zed profile with the extension installed, explicitly opt in to `rs-gopls` in that profile's user settings:

```json
{
  "languages": {
    "Go": { "language_servers": ["rs-gopls"] }
  }
}
```

For a legacy GOPATH project, also set `lsp.rs-gopls.binary.env.GO111MODULE` to `"off"` and set `lsp.rs-gopls.binary.env.GOPATH` if needed. Zed can use `lsp.rs-gopls.binary.path` when you want to point the extension at a different local build.

"""Shared legacy Go workspace fixtures and tests for LSP initialization."""

from collections import Counter
from dataclasses import dataclass
import os
from pathlib import Path

from check_goland_symbols import lsp_span
from lsp_harness import Site, path_from_uri, site, write

from ._assertions import check_location, check_references
from .diagnostics import capability_session, full_report, pull
from .hierarchy import prepare_hierarchy


@dataclass(frozen=True)
class DualGopath:
    root: Path
    server: Path
    serverbase: Path
    caller: Path
    glog: Path
    cmdmsg: Path

    @property
    def gopath(self):
        return os.pathsep.join(map(str, (self.server, self.serverbase)))

    @property
    def reference(self) -> Site:
        return site(self.caller, "glog.[[Run]](")

    @property
    def target(self) -> Site:
        return site(self.glog, "/*😀*/ [[Run]](")


def isolated_go_environment():
    environment = {
        name: value for name, value in os.environ.items()
        if name.upper() not in {"GO111MODULE", "GOENV", "GOPATH", "GOPROXY", "GOWORK"}
    }
    return {**environment, "GOENV": "off", "GOPROXY": "off"}


def dual_gopath(root):
    server, serverbase = root / "server", root / "serverbase"
    caller = write(
        server / "src" / "example.test" / "server" / "caller.go",
        'package server\nimport /*😀*/ "base/glog"\nimport /*😀*/ "cmdmsg"\n'
        "type /*😀*/ LegacyServer struct{ glog.LegacyBase }\n"
        "func /*😀*/ LegacyMain() { glog.Run(cmdmsg.Message{}) }\n",
    )
    glog = write(
        serverbase / "src" / "base" / "glog" / "glog.go",
        'package glog\nimport /*😀*/ "cmdmsg"\n'
        "type /*😀*/ LegacyBase struct{}\n"
        "func (LegacyBase) /*😀*/ Mark() {}\n"
        "func /*😀*/ Run(cmdmsg.Message) {}\n",
    )
    cmdmsg = write(
        serverbase / "src" / "cmdmsg" / "cmdmsg.go",
        "package cmdmsg\n"
        "type /*😀*/ LegacyContract interface{ /*😀*/ Mark() }\n"
        "type /*😀*/ Message struct{}\n",
    )
    return DualGopath(root, server, serverbase, caller, glog, cmdmsg)


@dataclass(frozen=True)
class _Symbol:
    target: Site
    declaration: Site
    kind: int
    container: str
    children: tuple["_Symbol", ...]
    document_name: str | None

    @property
    def name(self):
        return self.target.text[self.target.start:self.target.end]


def _symbol(path, anchor, kind, container, *children, document_name=None):
    declaration = anchor.replace("[[", "").replace("]]", "")
    return _Symbol(site(path, anchor), site(path, f"[[{declaration}]]"), kind, container, children, document_name)


def _check_outline(client, path, *symbols):
    def actual(rows):
        return [
            (row["name"], row["kind"], row["range"], row["selectionRange"], actual(row.get("children", [])))
            for row in rows
        ]

    def expected(symbols):
        return [
            (symbol.document_name or symbol.name, symbol.kind, symbol.declaration.selection(),
             symbol.target.selection(), expected(symbol.children))
            for symbol in symbols
        ]

    result = client.request("textDocument/documentSymbol", textDocument={"uri": path.as_uri()})
    if not isinstance(result, list) or actual(result) != expected(symbols):
        raise RuntimeError(f"unexpected outline for {path}: {result!r}")


def _check_workspace_symbols(client, query, *symbols):
    result = client.request("workspace/symbol", query=query)
    if not isinstance(result, list):
        raise RuntimeError(f"expected a workspace symbol list, got {result!r}")
    actual = Counter(
        (path_from_uri(row["location"]["uri"]), lsp_span(row["location"]["range"]),
         row["name"], row["kind"], row["containerName"])
        for row in result
    )
    expected = Counter(
        (symbol.target.path, lsp_span(symbol.target.selection()), symbol.name, symbol.kind, symbol.container)
        for symbol in symbols
    )
    if actual != expected:
        raise RuntimeError(f"workspace symbols for {query!r}: missing {expected - actual!r}, extra {actual - expected!r}")


def _prepare(client, kind, symbol, *, reference=None):
    item = prepare_hierarchy(client, kind, reference or symbol.target, symbol.target)
    if (item["kind"], item["range"]) != (symbol.kind, symbol.declaration.selection()):
        raise RuntimeError(f"unexpected {kind} hierarchy declaration for {symbol.name}: {item!r}")
    return item


def _check_edges(client, method, origin, expected, key):
    result = client.request(method, item=origin)
    rows = [] if result is None else result
    if not isinstance(rows, list) or sorted(rows, key=key) != sorted(expected, key=key):
        raise RuntimeError(f"{method} for {origin['name']}: expected {expected!r}, got {result!r}")


def check_gopath_initialization(executable_path, root, timeout):
    fixture = dual_gopath(root)
    caller, glog, cmdmsg = fixture.caller, fixture.glog, fixture.cmdmsg
    embedded = _symbol(caller, "glog.[[LegacyBase]]", 8, "server.LegacyServer")
    server_type = _symbol(caller, "type /*😀*/ [[LegacyServer]] struct{ glog.LegacyBase }", 23, "server", embedded)
    main = _symbol(caller, "func /*😀*/ [[LegacyMain]]() { glog.Run(cmdmsg.Message{}) }", 12, "server")
    base = _symbol(glog, "type /*😀*/ [[LegacyBase]] struct{}", 23, "glog")
    run = _symbol(glog, "func /*😀*/ [[Run]](cmdmsg.Message) {}", 12, "glog")
    base_mark = _symbol(glog, "func (LegacyBase) /*😀*/ [[Mark]]() {}", 6, "glog.LegacyBase",
                        document_name="(LegacyBase).Mark")
    method = _symbol(cmdmsg, "[[Mark]]()", 6, "cmdmsg.LegacyContract")
    contract = _symbol(cmdmsg, "type /*😀*/ [[LegacyContract]] interface{ /*😀*/ Mark() }", 11, "cmdmsg", method)
    message = _symbol(cmdmsg, "type /*😀*/ [[Message]] struct{}", 23, "cmdmsg")

    def extra_caller(outer, name):
        package = name.lower()
        anchor = f"func /*😀*/ [[Legacy{name}]]() {{ glog.Run(cmdmsg.Message{{}}) }}"
        declaration = anchor.replace("[[", "").replace("]]", "")
        path = write(outer / "src" / "example.test" / package / "caller.go",
                     f'package {package}\nimport "base/glog"\nimport "cmdmsg"\n{declaration}\n')
        return _symbol(path, anchor, 12, package)

    observer = extra_caller(fixture.server, "Observer")
    standby = extra_caller(fixture.serverbase, "Standby")
    capabilities = {
        "general": {"positionEncodings": ["utf-16"]},
        "textDocument": {"diagnostic": {}, "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}},
        "workspace": {"workspaceFolders": True},
    }
    for label, folders, callers, base_in_scope in (
        ("GOPATH root", [fixture.server], (main, observer), False),
        ("GOPATH descendant", [caller.parent], (main,), False),
        ("two GOPATH roots", [fixture.server, fixture.serverbase], (main, observer, standby), True),
        ("common parent", [fixture.root], (main, observer, standby), True),
    ):
        try:
            with capability_session(
                executable_path, folders[0], timeout, capabilities, folders=folders,
                environment=isolated_go_environment(), initialization_options={"env": {"GOPATH": fixture.gopath}},
            ) as (client, server_capabilities):
                if server_capabilities.get("positionEncoding", "utf-16").lower() != "utf-16":
                    raise RuntimeError("server did not negotiate UTF-16 positions")
                client.open_document(caller, fixture.reference.text)
                call_sites = tuple(site(symbol.target.path, "glog.[[Run]](") for symbol in callers)
                message_sites = tuple(site(symbol.target.path, "cmdmsg.[[Message]]{}") for symbol in callers)
                for reference in call_sites:
                    check_location(client, reference, run.target)
                for reference in message_sites:
                    check_location(client, reference, message.target)
                check_location(client, embedded.target, base.target)
                check_references(client, fixture.reference, call_sites)
                check_references(client, fixture.reference, (*call_sites, run.target), include_declaration=True)
                message_references = (*message_sites, message.target)
                base_references = (embedded.target, base.target)
                if base_in_scope:
                    message_references += (site(glog, "cmdmsg.[[Message]])"),)
                    base_references += (site(glog, "func ([[LegacyBase]])"),)
                check_references(client, message_sites[0], message_references, include_declaration=True)
                check_references(client, embedded.target, base_references, include_declaration=True)

                _check_outline(client, caller, server_type, main)
                _check_outline(client, glog, base, base_mark, run)
                _check_outline(client, cmdmsg, contract, message)
                for symbol in callers[1:]:
                    _check_outline(client, symbol.target.path, symbol)
                legacy_symbols = (server_type, embedded, *callers)
                if base_in_scope:
                    legacy_symbols += (base, contract)
                _check_workspace_symbols(client, "Legacy", *legacy_symbols)
                for query, symbols in (("glog.Run", (run,)), ("cmdmsg.Message", (message,)),
                                       ("Mark", (method, base_mark))):
                    _check_workspace_symbols(client, query, *(symbols if base_in_scope else ()))

                callee = _prepare(client, "Call", run, reference=fixture.reference)
                incoming = []
                for symbol, reference in zip(callers, call_sites):
                    owner = _prepare(client, "Call", symbol)
                    ranges = [reference.selection()]
                    _check_edges(client, "callHierarchy/outgoingCalls", owner, [{"to": callee, "fromRanges": ranges}],
                                 lambda row: row["to"]["uri"])
                    incoming.append({"from": owner, "fromRanges": ranges})
                _check_edges(client, "callHierarchy/incomingCalls", callee, incoming, lambda row: row["from"]["uri"])
                _check_edges(client, "callHierarchy/outgoingCalls", callee, [], lambda row: row["to"]["uri"])

                derived = _prepare(client, "Type", server_type)
                parent = _prepare(client, "Type", base, reference=embedded.target)
                interface = _prepare(client, "Type", contract)
                for method_name, origin, targets in (
                    ("supertypes", derived, [parent, interface]),
                    ("subtypes", derived, []),
                    ("supertypes", parent, [interface]),
                    ("subtypes", parent, [derived]),
                    ("supertypes", interface, []),
                    ("subtypes", interface, [derived, parent]),
                ):
                    _check_edges(client, f"typeHierarchy/{method_name}", origin, targets, lambda row: row["uri"])

                for path in (glog, cmdmsg, *(symbol.target.path for symbol in callers)):
                    full_report(pull(client, path), [])
        except RuntimeError as exc:
            raise RuntimeError(f"{label}: {exc}") from exc

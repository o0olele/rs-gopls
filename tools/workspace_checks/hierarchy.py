"""Exercise workspace roots and cancellable call and type hierarchies over stdio."""

import time

from lsp_harness import PausedGoList, expect_error, next_message, path_from_uri, session, site, write
from ._assertions import check_location, expect_workspace_symbols
from .diagnostics import capability_session, folder, full_report, pull


def prepare_hierarchy(client, kind, reference, target, *, name=None):
    result = client.request(f"textDocument/prepare{kind}Hierarchy",
                            textDocument={"uri": reference.path.as_uri()}, position=reference.cursor())
    name = name if name is not None else target.text[target.start:target.end]
    if (not result or len(result) != 1 or path_from_uri(result[0]["uri"]) != target.path
            or result[0]["selectionRange"] != target.selection()
            or result[0]["name"] != name):
        raise RuntimeError(f"{target.path}: unexpected {kind} hierarchy target {result!r}")
    return result[0]


def check_hierarchy_folders(executable, root, timeout):
    # The healthy group sorts first, so the added group's same-path packages need remapping.
    base, first, extra = (root / name for name in ("base", "first", "second"))
    shared_call_line = "func /*😀*/ SharedCall() {}"
    shared_base_line = "type /*😀*/ SharedBase struct{}"
    local_call_line = "func /*😀*/ LocalCall() {}"
    local_base_line = "type /*😀*/ LocalBase struct{}"
    run_line = "func /*😀*/ Run() { base.SharedCall(); local.LocalCall() }"
    derived_line = "type /*😀*/ Derived struct{ base.SharedBase; local.LocalBase }"
    write(base / "go.mod", "module example.test/base\n\ngo 1.22\n")
    base_file = write(base / "base.go", "package symbols\nconst FolderTokenBase = 1\n"
                      f"{shared_call_line}\n{shared_base_line}\n")
    base_token = site(base_file, "const [[FolderTokenBase]]")

    def consumer(path, name):
        write(path / "go.mod", "module example.test/consumer\n\ngo 1.22\n"
              "require example.test/base v0.0.0\nreplace example.test/base => ../base\n")
        source = write(path / "consumer.go", 'package symbols\nimport base "example.test/base"\n'
                       'import "example.test/consumer/local"\n'
                       f'const FolderToken{name.title()} = base.FolderTokenBase\n'
                       f'{run_line}\n{derived_line}\n')
        local = write(path / "local" / "local.go", "package local\n"
                      f"{local_call_line}\n{local_base_line}\n")
        return source, local

    first_source, first_local = consumer(first, "first")
    extra_source, extra_local = consumer(extra, "extra")
    first_ref = site(first_source, "base.[[FolderTokenBase]]")
    first_token = site(first_source, "const [[FolderTokenFirst]]")
    extra_ref = site(extra_source, "base.[[FolderTokenBase]]")
    extra_token = site(extra_source, "const [[FolderTokenExtra]]")
    first_cycle = write(first / "cycle" / "cycle.go", "package cycle\n")
    extra_cycle = write(extra / "cycle" / "cycle.go",
                        'package cycle\nimport /*😀*/ _ "example.test/consumer/cycle"\n')
    cycle_range = site(extra_cycle, "[[package cycle]]").selection()
    import_range = site(extra_cycle, '[["example.test/consumer/cycle"]]').selection()
    caps = {"textDocument": {"diagnostic": {}, "publishDiagnostics": {}},
            "workspace": {"workspaceFolders": True, "diagnostics": {"refreshSupport": True}}}
    with capability_session(executable, first, timeout, caps, folders=[first, base]) as (client, capabilities):
        if capabilities.get("positionEncoding", "utf-16").lower() != "utf-16":
            raise RuntimeError("server did not negotiate UTF-16 positions")
        refresh_ids = set()

        def refresh():
            request = next_message(client, time.monotonic() + timeout)
            request_id = request.get("id")
            if (request.get("method") != "workspace/diagnostic/refresh"
                    or not isinstance(request_id, str) or request_id in refresh_ids):
                raise RuntimeError(f"expected a diagnostic refresh with a fresh ID, got {request!r}")
            refresh_ids.add(request_id)

        def symbols(*targets):
            expect_workspace_symbols(client.request("workspace/symbol", query="FolderToken"), *targets, kind=14)

        def declaration(kind, path, text, name):
            target = site(path, f"/*😀*/ [[{name}]]")
            item = prepare_hierarchy(client, kind, target, target)
            if item["range"] != site(path, f"[[{text}]]").selection():
                raise RuntimeError(f"{path}: incorrect declaration range for {name}: {item!r}")
            return item

        def check_edges(method, anchor, expected, uri):
            actual = client.request(method, item=anchor)
            rows = [] if actual is None else actual
            if not isinstance(rows, list) or sorted(rows, key=uri) != sorted(expected, key=uri):
                raise RuntimeError(f"{method} from {anchor['uri']}: expected {expected!r}, got {actual!r}")

        def call_edges(direction, anchor, *targets):
            key = "from" if direction == "incoming" else "to"
            expected = [{key: target, "fromRanges": [position.selection()]} for target, position in targets]
            check_edges(f"callHierarchy/{direction}Calls", anchor, expected, lambda row: row[key]["uri"])

        def type_edges(direction, anchor, *targets):
            check_edges(f"typeHierarchy/{direction}types", anchor, targets, lambda row: row["uri"])

        def items(source, local):
            run = declaration("Call", source, run_line, "Run")
            shared_use = site(source, "base.[[SharedCall]]()")
            local_use = site(source, "local.[[LocalCall]]()")
            local_call = declaration("Call", local, local_call_line, "LocalCall")
            derived = declaration("Type", source, derived_line, "Derived")
            local_base = declaration("Type", local, local_base_line, "LocalBase")
            return run, shared_use, local_use, local_call, derived, local_base

        def check_consumers(adding):
            active = (first_items, extra_items) if adding else (first_items,)
            shared_callers, shared_subtypes = [], []
            for run, shared_use, local_use, local_call, derived, local_base in active:
                call_edges("outgoing", run, (shared_call, shared_use), (local_call, local_use))
                call_edges("incoming", local_call, (run, local_use))
                type_edges("super", derived, shared_base, local_base)
                type_edges("sub", local_base, derived)
                shared_callers.append((run, shared_use))
                shared_subtypes.append(derived)
            call_edges("incoming", shared_call, *shared_callers)
            type_edges("sub", shared_base, *shared_subtypes)
            if not adding:
                run, _, _, local_call, derived, local_base = extra_items
                call_edges("outgoing", run)
                call_edges("incoming", local_call)
                type_edges("super", derived)
                type_edges("sub", local_base)

        def check_diagnostics(adding):
            for path in (first_source, first_cycle, extra_source, extra_cycle):
                report = pull(client, path)
                codes = ["import.self", "package.load"] if adding and path == extra_cycle else []
                try:
                    full_report(report, codes)
                except RuntimeError as exc:
                    raise RuntimeError(f"{path}: {exc}") from exc
                if not codes:
                    continue
                by_code = {item["code"]: item for item in report["items"]}
                load = by_code["package.load"]
                if (load["message"], load["range"]) != ("import cycle not allowed", cycle_range):
                    raise RuntimeError(f"{path}: incorrect cycle diagnostic {load!r}")
                self_import = by_code["import.self"]
                if self_import["range"] != import_range:
                    raise RuntimeError(f"{path}: incorrect self-import range {self_import!r}")

        client.open_document(first_ref.path, first_ref.text)
        symbols(base_token, first_token)
        refresh()
        shared_call = declaration("Call", base_file, shared_call_line, "SharedCall")
        shared_base = declaration("Type", base_file, shared_base_line, "SharedBase")
        first_items = items(first_source, first_local)
        extra_items = items(extra_source, extra_local)
        check_location(client, first_ref, base_token)
        check_location(client, extra_ref, None)
        check_consumers(False)
        check_diagnostics(False)
        for adding in (True, False, True):
            add, remove = ([folder(extra)], []) if adding else ([], [folder(extra)])
            client.notify("workspace/didChangeWorkspaceFolders", event={"added": add, "removed": remove})
            refresh()
            symbols(base_token, first_token, *([extra_token] if adding else []))
            refresh()
            check_location(client, extra_ref, base_token if adding else None)
            check_location(client, first_ref, base_token)
            check_consumers(adding)
            check_diagnostics(adding)
        try:
            push = client.get_notification(timeout=0)
        except TimeoutError:
            pass
        else:
            raise RuntimeError(f"client offering both diagnostic modes received {push!r}")


def check_call_type_hierarchy(executable, root, timeout):
    project = root / "project"
    write(project / "go.mod", "module example.test/hierarchy\n\ngo 1.22\n")
    worker = write(project / "worker" / "worker.go", "package worker\n"
                   "func Leaf() {}\nfunc Next() {}\n"
                   "type Contract interface{ Mark() }\n"
                   "type Refined interface{ Contract; Reset() }\n"
                   "type Resetter interface{ Reset() }\n"
                   "type Base struct{}\nfunc (Base) Mark() {}\n"
                   "type Other struct{}\n")
    original = ('package sample\nimport "example.test/hierarchy/worker"\n'
                'type /*😀*/ Service interface{ worker.Refined; Flush() }\n'
                'type Derived struct{ worker.Base }\ntype /*😀*/ Alias = Derived\n'
                'func Run() { worker.Leaf(); func() { /*😀Run*/ worker.Leaf() }() }\n'
                'func (Derived) Tick() { func() { /*😀Tick*/ worker.Leaf() }() }\n')
    caller = write(project / "caller.go", original)
    edited = original.replace("worker.Base", "worker.Other").replace("worker.Leaf", "worker.Next")
    gate = PausedGoList(root / "control", timeout)
    with session(executable, project, timeout, **gate.environment) as client:
        def declaration(kind, target, *, name=None):
            return prepare_hierarchy(client, kind, target, target, name=name)

        client.open_document(caller, original)
        leaf = declaration("Call", site(worker, "func [[Leaf]]"))
        base = declaration("Type", site(worker, "type [[Base]]"))
        contract = declaration("Type", site(worker, "type [[Contract]]"))
        refined = declaration("Type", site(worker, "type [[Refined]]"))
        resetter = declaration("Type", site(worker, "type [[Resetter]]"))
        run = declaration("Call", site(caller, "func [[Run]]"))
        with gate.pause():
            client.send_batch(
                {"id": "cancel-incoming", "method": "callHierarchy/incomingCalls", "params": {"item": leaf}},
                {"id": "cancel-subtypes", "method": "typeHierarchy/subtypes", "params": {"item": base}},
                {"id": "cancel-interface-subtypes", "method": "typeHierarchy/subtypes", "params": {"item": contract}},
                {"id": "stale-outgoing", "method": "callHierarchy/outgoingCalls", "params": {"item": run}},
            )
            gate.wait_for_snapshot()
            for request_id in ("cancel-incoming", "cancel-subtypes", "cancel-interface-subtypes"):
                client.notify("$/cancelRequest", id=request_id)
                expect_error(client.wait_response(request_id), -32800)
            client.change_document(caller, 2, original + "\n")
            expect_error(client.wait_response("stale-outgoing"), -32801)

        def check_edges(text, callee, parent):
            call_target = site(worker, f"func [[{callee}]]")
            direct = site(caller, f"worker.[[{callee}]]();", text)
            run_closure = site(caller, f"/*😀Run*/ worker.[[{callee}]]()", text)
            tick_closure = site(caller, f"/*😀Tick*/ worker.[[{callee}]]()", text)
            target = prepare_hierarchy(client, "Call", direct, call_target)
            run = declaration("Call", site(caller, "func [[Run]]", text))
            tick = declaration("Call", site(caller, "func (Derived) [[Tick]]", text), name="(Derived).Tick")
            # A1 groups each closure's call with its outer named declaration.
            callers = []
            for owner, sites in ((run, (direct, run_closure)), (tick, (tick_closure,))):
                ranges = [position.selection() for position in sites]
                outgoing = client.request("callHierarchy/outgoingCalls", item=owner)
                if outgoing != [{"to": target, "fromRanges": ranges}]:
                    raise RuntimeError(f"incorrect {owner['name']} outgoing calls: {outgoing!r}")
                callers.append({"from": owner, "fromRanges": ranges})
            incoming = client.request("callHierarchy/incomingCalls", item=target)
            if incoming != callers:
                raise RuntimeError(f"incorrect grouped incoming calls: {incoming!r}")

            parent_item = prepare_hierarchy(client, "Type", site(caller, f"worker.[[{parent}]]", text),
                                            site(worker, f"type [[{parent}]]"))
            derived = declaration("Type", site(caller, "type [[Derived]]", text))
            supers = client.request("typeHierarchy/supertypes", item=derived)
            subs = client.request("typeHierarchy/subtypes", item=parent_item)
            expected = [contract, parent_item] if parent == "Base" else [parent_item]
            if supers != expected or subs != [derived]:
                raise RuntimeError(f"incorrect {parent} hierarchy: supertypes {supers!r}, subtypes {subs!r}")

            service = declaration("Type", site(caller, "type /*😀*/ [[Service]]", text))
            alias = declaration("Type", site(caller, "type /*😀*/ [[Alias]]", text))
            supers = client.request("typeHierarchy/supertypes", item=service)
            subs = client.request("typeHierarchy/subtypes", item=contract)
            expected = [service, derived, alias, refined, base] if parent == "Base" else [service, refined, base]
            if supers != [contract, refined, resetter] or subs != expected:
                raise RuntimeError(f"incorrect interface hierarchy: supertypes {supers!r}, subtypes {subs!r}")

        check_edges(original, "Leaf", "Base")
        client.change_document(caller, 3, edited)
        check_edges(edited, "Next", "Other")
        if client.request("callHierarchy/incomingCalls", item=leaf) or client.request("typeHierarchy/subtypes", item=base):
            raise RuntimeError("unsaved edit retained a removed call or embedding")
        client.close_document(caller)
        check_edges(original, "Leaf", "Base")


CASES = [
    ("hierarchy-workspace-folders", check_hierarchy_folders),
    ("call-type-hierarchy", check_call_type_hierarchy),
]

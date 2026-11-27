"""Check forward and reverse implementations, including cancellation."""

from pathlib import Path

from check_goland_definitions import normalize_locations
from check_goland_implementations import METHOD as IMPLEMENTATION_METHOD
from check_goland_references import Location
from lsp_harness import PausedGoList, expect_error, notify_changes, session, site, wait_for_response, write


def implementation_params(query):
    return {"textDocument": {"uri": query.path.as_uri()}, "position": query.cursor()}


def check_implementations(client, query, *expected):
    response = client.request(IMPLEMENTATION_METHOD, **implementation_params(query))
    actual = tuple(
        Location.from_range(Path(location["path"]), location["range"])
        for location in normalize_locations(response)
    )
    targets = {Location.from_range(target.path, target.selection()) for target in expected}
    locations = set(actual)
    if len(actual) != len(targets) or locations != targets:
        missing = sorted(targets - locations)
        extra = sorted(locations - targets)
        raise RuntimeError(
            f"implementations from {query.path} at {query.cursor()}: expected {len(targets)} locations, "
            f"got {len(actual)}. Missing {len(missing)} {missing[:3]!r}, extra {len(extra)} {extra[:3]!r}, "
            f"duplicates {len(actual) - len(locations)}"
        )
    return actual


def check_implementation_updates(executable_path, root, timeout):
    write(root / "go.mod", "module example.test/implupdates\n\ngo 1.22\n")
    origin = write(root / "worker.go", "package jobs\n\ntype Worker interface { Work(int) }\n")
    method = site(origin, "{ [[Work]](int)")
    interface = site(origin, "type [[Worker]] interface")
    implementation = write(
        root / "impl" / "worker.go", "package impl\n\ntype Live struct{}\nfunc (*Live) Work(int) {}\n",
    )
    internal = write(
        implementation.with_name("worker_test.go"),
        "package impl\n\ntype Internal struct{}\nfunc (Internal) Work(int) {}\n",
    )
    external = write(
        implementation.with_name("external_test.go"),
        "package impl_test\n\ntype External struct{}\nfunc (External) Work(int) {}\n",
    )
    live_type = site(implementation, "type [[Live]] struct")
    live = site(implementation, "(*Live) [[Work]](int)")
    internal_method = site(internal, "(Internal) [[Work]](int)")
    external_method = site(external, "(External) [[Work]](int)")
    draft_path = implementation.with_name("draft.go")
    draft = site(draft_path, "(Draft) [[Work]](int)", "package impl\n\ntype Draft struct{}\nfunc (Draft) Work(int) {}\n")
    prefix = "// shifted in the unsaved implementation\n"
    first = {"line": 0, "character": 0}
    shifted = site(implementation, "(*Live) [[Work]](int)", prefix + live.text)
    initial = (live, internal_method, external_method)

    with session(executable_path, root, timeout, watch_files=True) as client:
        client.open_document(origin, method.text)
        original_order = check_implementations(client, method, *initial)
        if check_implementations(client, method, *initial) != original_order:
            raise RuntimeError("identical implementation queries returned different orders")
        check_implementations(
            client, interface, live_type, site(internal, "type [[Internal]] struct"),
            site(external, "type [[External]] struct"),
        )

        client.open_document(implementation, live.text)
        client.change_document_range(implementation, 2, first, first, prefix)
        check_implementations(client, method, shifted, internal_method, external_method)
        client.change_document(implementation, 3, live.text.replace("Work(int)", "Work(string)"))
        client.open_document(draft_path, draft.text)
        check_implementations(client, method, draft, internal_method, external_method)
        client.close_document(draft_path)
        client.close_document(implementation)
        check_implementations(client, method, *initial)

        changed_source = method.text.replace("Work(int)", "Work(string)")
        client.change_document(origin, 2, changed_source)
        check_implementations(client, site(origin, "{ [[Work]](string)", changed_source))
        client.close_document(origin)
        check_implementations(client, method, *initial)

        write(implementation, "//go:build fixture && !fixture\n\n" + live.text)
        notify_changes(client, (implementation, 2))
        check_implementations(client, method, internal_method, external_method)
        write(implementation, live.text)
        notify_changes(client, (implementation, 2))
        check_implementations(client, method, *initial)

    if draft_path.exists():
        raise RuntimeError(f"test overlay unexpectedly wrote {draft_path}")


def check_implementation_candidate_packages(executable_path, root, timeout):
    module = "example.test/implpruning"
    write(root / "go.mod", f"module {module}\n\ngo 1.22\n")
    interface = site(
        write(root / "contract" / "worker.go", "package contract\n\ntype Worker interface { DeliverParcel() }\n"),
        "type [[Worker]] interface",
    )
    direct = site(
        write(
            root / "courier" / "delivery.go",
            "package courier\n\ntype Courier struct{}\nfunc (Courier) DeliverParcel() {}\n",
        ),
        "type [[Courier]] struct",
    )
    wrapper = site(
        write(
            root / "bridge" / "bridge.go",
            f'package bridge\n\nimport "{module}/courier"\n\ntype Bridge struct {{ courier.Courier }}\n',
        ),
        "type [[Bridge]] struct",
    )
    alias = site(
        write(
            root / "exported" / "alias.go",
            f'package exported\n\nimport "{module}/bridge"\n\ntype Exported = bridge.Bridge\n',
        ),
        "type [[Exported]] =",
    )
    editable = write(root / "switcher" / "switcher.go", "package switcher\n\ntype Switch struct{}\n")
    importer = site(
        write(
            root / "transit" / "transit.go",
            f'package transit\n\nimport "{module}/switcher"\n\ntype Transit struct {{ switcher.Switch }}\n',
        ),
        "type [[Transit]] struct",
    )
    draft = site(
        editable,
        "type [[Switch]] struct",
        f'package switcher\n\nimport "{module}/exported"\n\ntype Switch struct {{ exported.Exported }}\n',
    )
    method = site(direct.path, "(Courier) [[DeliverParcel]]()")
    interface_method = site(interface.path, "{ [[DeliverParcel]]()")
    on_disk = (direct, wrapper, alias)

    with session(executable_path, root, timeout) as client:
        client.open_document(interface.path, interface.text)
        check_implementations(client, interface, *on_disk)
        check_implementations(client, method, interface_method)

        client.open_document(editable, draft.text)
        check_implementations(client, interface, *on_disk, draft, importer)
        client.close_document(editable)
        check_implementations(client, interface, *on_disk)


def check_implementation_array_lengths(executable_path, root, timeout):
    module = "example.test/arraylengths"
    write(root / "go.mod", f"module {module}\n\ngo 1.22\n")
    width_text = "package sizes\n\nconst Width = 4\n"
    width = write(root / "sizes" / "width.go", width_text)
    contract = write(
        root / "api" / "frames.go",
        f'package api\n\nimport frame "{module}/sizes"\n\n'
        "type Framer interface { SendFrame([frame.Width]byte) }\n",
    )
    interface = site(contract, "type [[Framer]] interface")
    method = site(contract, "{ [[SendFrame]](")

    def sender(package, length):
        path = write(
            root / package / "sender.go",
            f"package {package}\n\ntype Sender struct{{}}\n"
            f"func (Sender) SendFrame([{length}]byte) {{}}\n",
        )
        return site(path, "type [[Sender]] struct"), site(path, "(Sender) [[SendFrame]](")

    literal = sender("literal", "4")
    named = sender("named", "Width")
    write(root / "named" / "width.go", "package named\n\nconst Width = 4\n")
    wide = sender("wide", "5")

    with session(executable_path, root, timeout) as client:
        client.open_document(contract, interface.text)

        def check_matches(*matching):
            check_implementations(client, interface, *(typ for typ, _ in matching))
            check_implementations(client, method, *(decl for _, decl in matching))
            for candidate in (literal, named, wide):
                typ, decl = candidate
                if candidate in matching:
                    check_implementations(client, typ, interface)
                    check_implementations(client, decl, method)
                else:
                    check_implementations(client, typ)
                    check_implementations(client, decl)

        check_matches(literal, named)
        client.open_document(width, width_text)
        client.change_document(width, 2, width_text.replace("= 4", "= 5"))
        check_matches(wide)
        client.close_document(width)
        check_matches(literal, named)


def check_implementation_reverse_candidates(executable_path, root, timeout):
    module = "example.test/reversecandidates"
    write(root / "go.mod", f"module {module}\n\ngo 1.22\n")
    write(
        root / "stamper" / "stamper.go",
        "package stamper\n\ntype Stamper struct{}\nfunc (*Stamper) SealParcel(string) error { return nil }\n",
    )
    courier = site(
        write(
            root / "courier" / "courier.go",
            f'package courier\n\nimport "{module}/stamper"\n\n'
            "type Courier struct { stamper.Stamper }\nfunc (Courier) DeliverParcel(int) bool { return true }\n",
        ),
        "type [[Courier]] struct",
    )
    delivery = site(
        write(root / "delivery" / "delivery.go", "package delivery\n\ntype Delivery interface { DeliverParcel(int) bool }\n"),
        "type [[Delivery]] interface",
    )
    sealing = site(
        write(root / "sealing" / "sealing.go", "package sealing\n\ntype Sealing interface { SealParcel(string) error }\n"),
        "type [[Sealing]] interface",
    )
    embedding = site(
        write(
            root / "publicseal" / "seal.go",
            f'package publicseal\n\nimport "{module}/sealing"\n\ntype PublicSeal interface {{ sealing.Sealing }}\n',
        ),
        "type [[PublicSeal]] interface",
    )
    alias = site(
        write(
            root / "exported" / "alias.go",
            f'package exported\n\nimport "{module}/publicseal"\n\ntype ExportedSeal = publicseal.PublicSeal\n',
        ),
        "type [[ExportedSeal]] =",
    )
    write(
        root / "wrong" / "wrong.go",
        "package wrong\n\ntype WrongSeal interface { SealParcel(string) bool }\n",
    )
    write(root / "special" / "special.go", "package special\n\nconst enabled = true\n")
    draft = site(
        root / "special" / "draft.go",
        "type [[Combined]] interface",
        "package special\n\ntype Combined interface { DeliverParcel(int) bool; SealParcel(string) error }\n",
    )
    on_disk = (delivery, sealing, embedding, alias)

    with session(executable_path, root, timeout) as client:
        client.open_document(courier.path, courier.text)
        check_implementations(client, courier, *on_disk)
        client.open_document(draft.path, draft.text)
        check_implementations(client, courier, *on_disk, draft)
        client.close_document(draft.path)
        check_implementations(client, courier, *on_disk)

    if draft.path.exists():
        raise RuntimeError(f"test overlay unexpectedly wrote {draft.path}")


def check_implementation_reverse_watched_sources(executable_path, root, timeout):
    write(root / "go.mod", "module example.test/reversewatch\n\ngo 1.22\n")
    provider = write(
        root / "provider.go",
        "package provider\n\ntype Starter struct{}\nfunc (Starter) StartParcel() {}\n"
        "type Inspector struct{}\nfunc (Inspector) InspectParcel() {}\n",
    )
    starter = site(provider, "type [[Starter]] struct")
    inspector = site(provider, "type [[Inspector]] struct")
    active = site(
        write(root / "active" / "active.go", "package active\n\ntype Starting interface { StartParcel() }\n"),
        "type [[Starting]] interface",
    )
    pending = write(root / "pending" / "pending.go", "package pending\n\ntype Pending interface { DispatchParcel() }\n")

    with session(executable_path, root, timeout, watch_files=True) as client:
        client.open_document(provider, starter.text)
        check_implementations(client, starter, active)
        check_implementations(client, inspector)

        changed = site(
            write(active.path, "package active\n\ntype Inspection interface { InspectParcel() }\n"),
            "type [[Inspection]] interface",
        )
        notify_changes(client, (active.path, 2))
        check_implementations(client, starter)
        check_implementations(client, inspector, changed)

        # This file was previously pruned from both reverse searches.
        added = site(
            write(pending, "package pending\n\ntype Inspection interface { InspectParcel() }\n"),
            "type [[Inspection]] interface",
        )
        notify_changes(client, (pending, 2))
        check_implementations(client, inspector, changed, added)

        client.open_document(pending, added.text)
        saved = site(
            write(pending, "package pending\n\ntype Starting interface { StartParcel() }\n"),
            "type [[Starting]] interface",
        )
        check_implementations(client, inspector, changed, added)
        client.close_document(pending)
        check_implementations(client, starter, saved)
        check_implementations(client, inspector, changed)


def check_implementation_cancel(executable_path, root, timeout):
    gate = PausedGoList(root / "control", timeout)
    project = root / "project"
    go_mod = write(project / "go.mod", "module example.test/implcancel\n\ngo 1.22\n")
    origin = site(write(project / "worker.go", "package jobs\n\ntype Worker interface { Work() }\n"), "{ [[Work]]()")
    target = site(
        write(project / "impl" / "worker.go", "package impl\n\ntype Worker struct{}\nfunc (Worker) Work() {}\n"),
        "(Worker) [[Work]]()",
    )
    shifted = site(origin.path, "{ [[Work]]()", "// changed while metadata loaded\n" + origin.text)

    with session(executable_path, project, timeout, watch_files=True, **gate.environment) as client:
        client.open_document(origin.path, origin.text)
        check_implementations(client, origin, target)
        with gate.pause():
            write(go_mod, go_mod.read_text(encoding="utf-8") + "\n// refresh the package graph\n")
            notify_changes(client, (go_mod, 2))
            client.send(id="cancel-impl", method=IMPLEMENTATION_METHOD, params=implementation_params(origin))
            gate.wait_for_snapshot()
            client.notify("$/cancelRequest", id="cancel-impl")
            expect_error(wait_for_response(client, "cancel-impl", min(timeout, 5)), -32800)

            client.send(id="changed-impl", method=IMPLEMENTATION_METHOD, params=implementation_params(origin))
            client.change_document(origin.path, 2, shifted.text)
            expect_error(wait_for_response(client, "changed-impl", min(timeout, 5)), -32801)
        check_implementations(client, shifted, target)


def check_active_implementation_cancel(executable_path, root, timeout):
    write(root / "go.mod", "module example.test/implbusy\n\ngo 1.22\n")
    origin = site(write(root / "worker.go", "package jobs\n\ntype Worker interface { Work() }\n"), "{ [[Work]]()")
    methods = []
    for number in range(384):
        worker = f"Worker{number:03d}"
        path = write(root / f"worker_{number:03d}.go", f"package jobs\ntype {worker} struct{{}}\nfunc ({worker}) Work() {{}}\n")
        methods.append(site(path, f"({worker}) [[Work]]()"))

    def start_active_search(client, request_id):
        # The barrier response confirms that the search was dispatched and is still pending.
        barrier_id = f"{request_id}-barrier"
        client.send_batch(
            {"id": request_id, "method": IMPLEMENTATION_METHOD, "params": implementation_params(origin)},
            {"id": barrier_id, "method": "rs-gopls/testUnknownMethod", "params": {}},
        )
        expect_error(wait_for_response(client, barrier_id), -32601)

    with session(executable_path, root, timeout) as client:
        client.open_document(origin.path, origin.text)
        check_implementations(client, origin, *methods)
        # Keep the loaded package graph and invalidate the previous query result.
        client.change_document(origin.path, 2, origin.text + "\ntype Unrelated struct{}\n")
        start_active_search(client, "active-impl")
        client.notify("$/cancelRequest", id="active-impl")
        expect_error(wait_for_response(client, "active-impl"), -32800)

        first = methods[0]
        prefix = "// shifted in the open buffer\n"
        shifted = site(first.path, "(Worker000) [[Work]]()", prefix + first.text)
        start_active_search(client, "dirty-impl")
        client.open_document(first.path, shifted.text)
        expect_error(wait_for_response(client, "dirty-impl"), -32801)
        check_implementations(client, origin, shifted, *methods[1:])


CASES = [
    ("implementation-updates", check_implementation_updates, 18),
    ("implementation-candidate-packages", check_implementation_candidate_packages, 19),
    ("implementation-reverse-candidates", check_implementation_reverse_candidates, 20),
    ("implementation-reverse-watched-sources", check_implementation_reverse_watched_sources, 21),
    ("implementation-cancel", check_implementation_cancel, 22),
    ("implementation-active-cancel", check_active_implementation_cancel, 23),
    ("implementation-array-lengths", check_implementation_array_lengths),
]

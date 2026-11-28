"""Check completion capabilities, overlays, resolve, and package updates."""

from check_goland_completion import (
    COMPLETION, RESOLVE, RESOLVE_PROPERTIES, DocumentText, apply_completion, completion_items,
    completion_params, completion_session, primary_edit, warm_package_graph,
)
from lsp_harness import PausedGoList, expect_error, notify_changes, position_at, wait_for_response, write


MODULE = "example.test/completionupdates"
WIDGETS = f"{MODULE}/widgets"
DEPRECATED_DOC = "Deprecated: use NewWidget instead."


def cursor(text, anchor):
    point = position_at(text, text.index(anchor) + len(anchor))
    return point["line"], point["character"]


def complete(client, path, point, trigger_character=None):
    result = client.request(COMPLETION, **completion_params(path, point, trigger_character))
    try:
        return completion_items(result)
    except ValueError as exc:
        raise RuntimeError(f"invalid completion result for {path} at {point}: {result!r}") from exc


def expect_item(items, label):
    matches = [item for item in items if item.get("label") == label]
    if len(matches) != 1:
        raise RuntimeError(f"expected one {label!r}, got {[item.get('label') for item in items]!r}")
    return matches[0]


def expect_labels(items, *expected):
    labels = [item.get("label") for item in items]
    if sorted(labels) != sorted(expected):
        raise RuntimeError(f"expected completion labels {expected!r}, got {labels!r}")


def expect_edit(item, text, point, new_text, replacement, insertion=None):
    try:
        actual = primary_edit(item, DocumentText(text), point)
    except (KeyError, TypeError, ValueError) as exc:
        raise RuntimeError(f"invalid completion edit for {item!r}: {exc}") from exc
    if actual != (new_text, insertion, replacement):
        raise RuntimeError(f"wrong edit for {item['label']!r}: {actual!r}")


def expect_applied(item, text, point, *fragments):
    try:
        actual = apply_completion(DocumentText(text), point, item)
    except (KeyError, TypeError, ValueError) as exc:
        raise RuntimeError(f"invalid accepted completion {item!r}: {exc}") from exc
    if any(actual.count(fragment) != 1 for fragment in fragments):
        raise RuntimeError(f"expected each of {fragments!r} once in accepted text, got {actual!r}")


def setup_module(root):
    return write(root / "go.mod", f"module {MODULE}\n\ngo 1.22\n")


def check_completion_unsaved_edits_and_incremental_typing(server, root, timeout):
    setup_module(root)
    disk = 'package sample\ntype record struct { Bar int; Box int; Leaf int }\nfunc use(foo record) { _ = "😀"; _ = foo.Bar }\n'
    path = write(root / "source.go", disk)
    disk_point = cursor(disk, "_ = foo.")
    edited = disk.replace("Bar int", "Baz int").replace("foo.Bar", "foo")
    offset = edited.index("_ = foo") + len("_ = foo")
    with completion_session(server, root, timeout) as (client, _):
        client.open_document(path, disk)
        expect_labels(complete(client, path, disk_point), "Bar", "Box", "Leaf")
        client.change_document(path, 2, edited)
        for version, character, labels in (
            (3, ".", ("Baz", "Box", "Leaf")), (4, "B", ("Baz", "Box")), (5, "a", ("Baz",)),
        ):
            start = position_at(edited, offset)
            client.change_document_range(path, version, start, start, character)
            edited = edited[:offset] + character + edited[offset:]
            offset += 1
            point = cursor(edited, "_ = foo." + "Ba"[:version - 3])
            items = complete(client, path, point, "." if character == "." else None)
            expect_labels(items, *labels)
            expect_edit(expect_item(items, "Baz"), edited, point, "Baz", (*disk_point, *point))
        client.close_document(path)
        restored = complete(client, path, disk_point)
        expect_labels(restored, "Bar", "Box", "Leaf")
        expect_edit(expect_item(restored, "Bar"), disk, disk_point, "Bar", (*disk_point, *cursor(disk, "_ = foo.Bar")))
        if path.read_bytes() != disk.encode("utf-8"):
            raise RuntimeError(f"completion changed the disk source {path}")


def widget_project(root):
    setup_module(root)
    write(root / "widgets/widgets.go", "package widgets\n\n// BuildWidget creates a **widget**.\n"
          f"//\n// {DEPRECATED_DOC}\nfunc BuildWidget(value string) int {{ return len(value) }}\n")
    text = "package sample\nfunc run() { _ = BuildWid }\n"
    return write(root / "source.go", text), text, cursor(text, "_ = BuildWid")


def expect_provider(capabilities):
    provider = capabilities.get("completionProvider")
    if not isinstance(provider, dict) or provider.get("resolveProvider") is not True or "." not in provider.get("triggerCharacters", ()):
        raise RuntimeError(f"expected completion, dot trigger, and resolve support, got {provider!r}")


def expect_documentation(item, kind):
    documentation = item.get("documentation")
    if (not isinstance(documentation, dict) or documentation.get("kind") != kind or
            DEPRECATED_DOC not in documentation.get("value", "")):
        raise RuntimeError(f"expected {kind} documentation mentioning {DEPRECATED_DOC!r}, got {documentation!r}")


def check_completion_capability_negotiation(server, root, timeout):
    path, text, point = widget_project(root)
    saved_import = f'package sample\nimport _ "{WIDGETS}"\n'
    import_path = write(root / "import_probe.go", saved_import)
    edited_import = saved_import.replace("/widgets", "/widSuffix")
    prefix = f'"{MODULE}/wid'
    start, caret, end = (cursor(edited_import, anchor) for anchor in ('"', prefix, f'{prefix}Suffix'))
    replacement = (*start, *end)
    symbol_span = (*cursor(text, "_ = "), *point)
    expected_order = None

    for full in (True, False):
        with completion_session(server, root, timeout, full=full) as (client, capabilities):
            expect_provider(capabilities)
            if full and not capabilities["completionProvider"].get("completionItem", {}).get("labelDetailsSupport"):
                raise RuntimeError("server did not advertise completion labelDetails support")
            client.open_document(path, text)
            items = complete(client, path, point)
            order = [(item["label"], item["sortText"], item["filterText"]) for item in items]
            if expected_order is not None and order != expected_order:
                raise RuntimeError("completion ordering changed between client sessions")
            expected_order = order
            widget = expect_item(items, "BuildWidget")
            if full:
                details = widget.get("labelDetails") or {}
                if details.get("description") != WIDGETS or "value string" not in details.get("detail", ""):
                    raise RuntimeError(f"expected package and parameter labelDetails, got {widget!r}")
                if widget.get("insertTextFormat") != 2 or 1 not in widget.get("tags", ()):
                    raise RuntimeError(f"expected snippet format and Deprecated tag, got {widget!r}")
                if any(widget.get(field) is not None for field in RESOLVE_PROPERTIES):
                    raise RuntimeError(f"expected lazily resolved fields, got {widget!r}")
                expect_edit(widget, text, point, "widgets.BuildWidget($1)$0", symbol_span)
            else:
                if any(widget.get(field) for field in ("labelDetails", "tags", "deprecated")) or widget.get("insertTextFormat", 1) != 1:
                    raise RuntimeError(f"client did not negotiate snippet, labelDetails or deprecation, got {widget!r}")
                if not widget.get("detail") or not widget.get("additionalTextEdits"):
                    raise RuntimeError(f"expected eager detail and import edit, got {widget!r}")
                expect_documentation(widget, "plaintext")
                expect_edit(widget, text, point, "widgets.BuildWidget()", symbol_span)
                expect_applied(widget, text, point, f'import "{WIDGETS}"', "_ = widgets.BuildWidget()")

            client.open_document(import_path, saved_import)
            client.change_document(import_path, 2, edited_import)
            item = expect_item(complete(client, import_path, caret), WIDGETS)
            expect_edit(item, edited_import, caret, WIDGETS, replacement, (*start, *caret) if full else None)

    with completion_session(server, root, timeout, resolve_properties=("detail", "documentation")) as (client, _):
        client.open_document(path, text)
        widget = expect_item(complete(client, path, point), "BuildWidget")
        if widget.get("detail") or widget.get("documentation") or not widget.get("additionalTextEdits"):
            raise RuntimeError(f"only fields named in resolveSupport.properties should be deferred: {widget!r}")
        expect_applied(widget, text, point, f'import "{WIDGETS}"', "_ = widgets.BuildWidget()")


def expect_stale_item(client, item):
    response = wait_for_response(client, client.start_request(RESOLVE, **item))
    expect_error(response, -32801)
    data = response["error"].get("data")
    if "result" in response or isinstance(data, dict) and ("textEdit" in data or "additionalTextEdits" in data):
        raise RuntimeError(f"stale completion returned edits: {response!r}")


def check_completion_resolve_and_stale_protection(server, root, timeout):
    path, text, point = widget_project(root)
    with completion_session(server, root, timeout) as (client, _):
        client.open_document(path, text)
        item = expect_item(complete(client, path, point), "BuildWidget")
        if any(item.get(field) is not None for field in RESOLVE_PROPERTIES):
            raise RuntimeError(f"expected deferred completion fields: {item!r}")
        resolved = client.request(RESOLVE, **item)
        for key in ("label", "sortText", "filterText", "textEdit"):
            if not item.get(key) or resolved.get(key) != item[key]:
                raise RuntimeError(f"resolve must preserve {key}: {item!r} -> {resolved!r}")
        if "BuildWidget(value string) int" not in resolved.get("detail", "") or not resolved.get("additionalTextEdits"):
            raise RuntimeError(f"resolve did not supply function detail and import edits: {resolved!r}")
        expect_documentation(resolved, "markdown")
        expect_applied(resolved, text, point, f'import "{WIDGETS}"', "_ = widgets.BuildWidget()")

        client.change_document(path, 2, "// buffer changed\n" + text)
        expect_stale_item(client, item)
        updated = expect_item(complete(client, path, (point[0] + 1, point[1])), "BuildWidget")
        fresh = client.request(RESOLVE, **updated)
        expect_applied(fresh, "// buffer changed\n" + text, (point[0] + 1, point[1]),
                       f'import "{WIDGETS}"', "_ = widgets.BuildWidget()")


def check_completion_graph_refresh_auto_import(server, root, timeout):
    setup_module(root)
    write(root / "initial/initial.go", "package initial\nfunc InitialGraphWitness() {}\n")
    text = "package sample\nfunc run() { InitialGraphWit; FreshGraphWit }\n"
    path = write(root / "source.go", text)
    import_text = f'package sample\nimport "{MODULE}/fresh"\n'
    probe = write(root / "import_probe.go", "package sample\n")
    fresh_path = f"{MODULE}/fresh"
    fresh_point = cursor(text, "FreshGraphWit")
    import_point = cursor(import_text, fresh_path)

    with completion_session(server, root, timeout, watch_files=True) as (client, _):
        client.open_document(path, text)
        client.open_document(probe, import_text)
        old = expect_item(complete(client, path, cursor(text, "InitialGraphWit")), "InitialGraphWitness")
        expect_applied(client.request(RESOLVE, **old), text, cursor(text, "InitialGraphWit"),
                       f'import "{MODULE}/initial"', "initial.InitialGraphWitness()")
        for target, point, label in ((path, fresh_point, "FreshGraphWitness"), (probe, import_point, fresh_path)):
            if any(item.get("label") == label for item in complete(client, target, point)):
                raise RuntimeError(f"{label!r} appeared before the package existed")

        added = write(root / "fresh/fresh.go", "package fresh\nfunc FreshGraphWitness() {}\n")
        notify_changes(client, (added, 1))
        warm_package_graph(client)
        expect_item(complete(client, probe, import_point), fresh_path)
        item = expect_item(complete(client, path, fresh_point), "FreshGraphWitness")
        expect_applied(client.request(RESOLVE, **item), text, fresh_point,
                       f'import "{fresh_path}"', "fresh.FreshGraphWitness()")
        # The original document and its version did not change during the refresh.
        expect_stale_item(client, old)


def check_completion_cancellation_and_stale_tasks(server, root, timeout):
    project = root / "project"
    path, text, point = widget_project(project)
    module = project / "go.mod"
    gate = PausedGoList(root / "control", timeout)
    params = completion_params(path, point)
    with completion_session(server, project, timeout, watch_files=True, environment=gate.environment) as (client, _):
        client.open_document(path, text)
        warm_package_graph(client)
        with gate.pause():
            write(module, module.read_text(encoding="utf-8") + "// refresh the package graph\n")
            notify_changes(client, (module, 2))
            request_id = client.start_request(COMPLETION, **params)
            gate.wait_for_snapshot()
            client.notify("$/cancelRequest", id=request_id)
            client.notify("$/cancelRequest", id=request_id)
            expect_error(wait_for_response(client, request_id, min(timeout, 5)), -32800)

            # Hold the graph so both requests stay pending until their respective invalidations.
            older = client.start_request(COMPLETION, **params)
            latest = client.start_request(COMPLETION, **params)
            expect_error(wait_for_response(client, older, min(timeout, 5)), -32800)

            client.change_document(path, 2, "// edit during completion\n" + text)
            expect_error(wait_for_response(client, latest, min(timeout, 5)), -32801)

        retried = client.start_request(COMPLETION, **completion_params(path, (point[0] + 1, point[1])))
        retry = wait_for_response(client, retried)
        if "error" in retry:
            raise RuntimeError(f"completion retry failed: {retry!r}")
        try:
            items = completion_items(retry["result"])
        except ValueError as exc:
            raise RuntimeError(f"invalid completion retry response: {retry!r}") from exc
        expect_item(items, "BuildWidget")
        try:
            extra = client.get_next_response(timeout=0.2)
        except TimeoutError:
            return
        raise RuntimeError(f"unexpected or duplicate completion response: {extra!r}")


CASES = (
    ("completion_unsaved_edits_and_incremental_typing", check_completion_unsaved_edits_and_incremental_typing),
    ("completion_capability_negotiation", check_completion_capability_negotiation),
    ("completion_resolve_and_stale_protection", check_completion_resolve_and_stale_protection),
    ("completion_graph_refresh_auto_import", check_completion_graph_refresh_auto_import),
    ("completion_cancellation_and_stale_tasks", check_completion_cancellation_and_stale_tasks),
)

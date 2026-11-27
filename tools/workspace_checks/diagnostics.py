"""Check push and pull diagnostics against a real stdio session and package loader."""

from contextlib import contextmanager
import os
import subprocess
import time

from lsp_harness import LspClient, PausedGoList, WORKSPACE_ENVIRONMENT, notify_changes, path_from_uri, site, write


PULL = {"textDocument": {"diagnostic": {}}}
PUBLISH = "textDocument/publishDiagnostics"


def folder(path):
    return {"uri": path.as_uri(), "name": path.name}


@contextmanager
def capability_session(executable, root, timeout, capabilities, *, folders=None, environment=None,
                       initialization_options=None, **env):
    if environment is None:
        environment = {**os.environ, **WORKSPACE_ENVIRONMENT}
    client = LspClient([executable], root, timeout, environment={**environment, **env})
    try:
        if folders is not None:
            client.folders = [folder(path) for path in folders]
        options = {} if initialization_options is None else {"initializationOptions": initialization_options}
        result = client.request("initialize", rootUri=root.as_uri(), workspaceFolders=client.folders,
                                capabilities=capabilities, **options)
        client.notify("initialized")
        yield client, result["capabilities"]
        client.shutdown()
    except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
        raise RuntimeError(f"{root}: {exc}\n{client.stderr_tail()}") from exc
    finally:
        client.close()


def pull(client, path, previous=None):
    params = {"textDocument": {"uri": path.as_uri()}}
    if previous is not None:
        params["previousResultId"] = previous
    return client.request("textDocument/diagnostic", **params)


def full_report(report, codes):
    if report.get("kind") != "full" or not report.get("resultId"):
        raise RuntimeError(f"expected a full report with a resultId, received {report!r}")
    items = report.get("items")
    if not isinstance(items, list) or sorted(item["code"] for item in items) != sorted(codes):
        raise RuntimeError(f"expected diagnostics {codes!r}, received {items!r}")
    return report["resultId"]


def expect_publications(client, expected):
    pending = dict(expected)
    deadline = time.monotonic() + client.timeout
    while pending:
        message = client.get_notification(timeout=max(0, deadline - time.monotonic()))
        if message["method"] != PUBLISH:
            raise RuntimeError(f"unexpected notification {message!r}")
        params = message["params"]
        path = path_from_uri(params["uri"])
        if path not in pending:
            raise RuntimeError(f"unexpected diagnostics for {path}, waiting for {list(pending)}")
        version, codes = pending.pop(path)
        actual = sorted(item["code"] for item in params["diagnostics"])
        if params.get("version") != version or actual != sorted(codes):
            raise RuntimeError(f"{path}: expected version {version}, codes {codes!r}, got {params!r}")


def check_push_diagnostics(executable, root, timeout):
    project = root / "project"
    write(project / "go.mod", "module example.test/push\n\ngo 1.22\n")
    clean = "package sample\nvar Value = 1\n"
    syntax = write(project / "syntax.go", clean)
    semantic = write(project / "sem" / "sem.go",
                     'package sem\nimport "strings"\nfunc f() int { unused := 42; return Missing }\n')
    invalid = write(project / "invalid" / "invalid.go",
                    'package invalid\nimport _ "example.test/push/absent"\n')
    gate = PausedGoList(root / "control", timeout)
    codes = ["import.unused", "var.unused", "name.unresolved"]
    with capability_session(executable, project, timeout, {"textDocument": {"publishDiagnostics": {}}},
                            **gate.environment) as (client, _):
        with gate.pause():
            client.open_document(syntax, "package sample\nvar Value = (1 + )\n")
            expect_publications(client, {syntax: (1, ["syntax.error"])})
            gate.wait_for_snapshot()
            client.open_document(semantic, semantic.read_text(encoding="utf-8"))
            expect_publications(client, {semantic: (1, [])})
            client.change_document(syntax, 2, clean)
            expect_publications(client, {syntax: (2, [])})
        expect_publications(client, {syntax: (2, []), semantic: (1, codes)})

        client.open_document(invalid, invalid.read_text(encoding="utf-8"))
        expect_publications(client, {invalid: (1, ["import.invalid", "package.load"])})
        client.close_document(invalid)
        expect_publications(client, {invalid: (None, [])})

        with gate.pause():
            new_file = write(semantic.parent / "value.go", "package sem\nconst Missing = 1\n")
            notify_changes(client, (new_file, 1))
            expect_publications(client, {syntax: (2, []), semantic: (1, [])})
            gate.wait_for_snapshot()
        complete = {syntax: (2, []), semantic: (1, ["import.unused", "var.unused"])}
        expect_publications(client, complete)

        extra = root / "extra"
        write(extra / "go.mod", "module example.test/extra\n\ngo 1.22\n")
        write(extra / "extra.go", "package extra\n")
        client.notify("workspace/didChangeWorkspaceFolders", event={"added": [folder(extra)], "removed": []})
        expect_publications(client, {syntax: (2, []), semantic: (1, [])})
        expect_publications(client, complete)
        client.close_document(syntax)
        client.close_document(semantic)
        expect_publications(client, {syntax: (None, []), semantic: (None, [])})


def check_pull_diagnostics(executable, root, timeout):
    project = root / "project"
    write(project / "go.mod", "module example.test/pull\n\ngo 1.22\n")
    text = "package sample\nvar Value = missing\n"
    path = write(project / "value.go", text)
    clean = write(project / "other.go", "package sample\nconst Other = 42\n")
    with capability_session(executable, project, timeout, PULL) as (client, caps):
        expected = {"identifier": "rs-gopls", "interFileDependencies": True, "workspaceDiagnostics": False}
        if caps.get("diagnosticProvider") != expected:
            raise RuntimeError(f"unexpected diagnosticProvider {caps.get('diagnosticProvider')!r}")
        client.open_document(path, text)
        report = pull(client, path)
        first = full_report(report, ["name.unresolved"])
        item = report["items"][0]
        if (item["range"], item["source"], item["severity"]) != (site(path, "[[missing]]").selection(), "rs-gopls", 1):
            raise RuntimeError(f"unexpected unresolved name diagnostic {item!r}")
        if pull(client, path, first) != {"kind": "unchanged", "resultId": first}:
            raise RuntimeError("repeat pull lost its resultId")
        client.change_document(path, 2, text.replace("missing", "Other"))
        fixed = full_report(pull(client, path, first), [])
        if fixed == first:
            raise RuntimeError("edited document kept its stale resultId")
        full_report(pull(client, clean), [])
        write(clean, "package sample\nconst Renamed = 42\n")
        notify_changes(client, (clean, 2))
        if full_report(pull(client, path, fixed), ["name.unresolved"]) == fixed:
            raise RuntimeError("a watched declaration change kept its dependent's stale resultId")
        client.close_document(path)
        client.request("textDocument/documentSymbol", textDocument={"uri": clean.as_uri()})
        try:
            message = client.get_notification(timeout=0)
        except TimeoutError:
            pass
        else:
            raise RuntimeError(f"pull client received an unsolicited notification {message!r}")

    gate = PausedGoList(root / "control", timeout)
    with capability_session(executable, project, timeout, PULL, **gate.environment) as (client, _):
        gate.fail_next()
        client.open_document(path, "package sample\nvar Value = (1 + )\n")
        report = pull(client, path)
        failed = full_report(report, ["package.load", "syntax.error"])
        error = next(item for item in report["items"] if item["code"] == "package.load")
        if (error["range"] != site(path, "[[package sample]]").selection()
                or not error["message"].endswith("): injected go list failure")):
            raise RuntimeError(f"incorrect package load diagnostic {error!r}")
        if pull(client, path, failed) != {"kind": "unchanged", "resultId": failed}:
            raise RuntimeError("repeat pull lost its group failure resultId")

        recovered = write(project / "recovered.go", "package sample\n")
        notify_changes(client, (recovered, 1))
        if full_report(pull(client, path, failed), ["syntax.error"]) == failed:
            raise RuntimeError("successful package reload kept the group failure resultId")


CASES = [
    ("diagnostics-push", check_push_diagnostics),
    ("diagnostics-pull", check_pull_diagnostics),
]

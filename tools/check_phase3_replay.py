#!/usr/bin/env python3
"""Replay the Phase 3 cold reference, test-view, and watcher workloads over stdio."""

import argparse
from collections import Counter
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import statistics
import tempfile
import time

from lsp_harness import (
    LspClient, PausedGoList, WORKSPACE_ENVIRONMENT, executable, path_from_uri, site, write,
)
from workspace_checks._assertions import expect_location, reference_params


MODULE = "example.test/phase3"
REFERENCE = "textDocument/references"
WATCHER = "workspace/didChangeWatchedFiles"


def events(log, event):
    if not log.exists():
        return []
    complete = log.read_bytes().rpartition(b"\n")[0]
    return [row for line in complete.splitlines() if (row := json.loads(line))["event"] == event]


def wait_event(log, event, predicate, timeout):
    deadline = time.monotonic() + timeout
    while True:
        for row in reversed(events(log, event)):
            if predicate(row):
                return row
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for {event} in {log.name}")
        time.sleep(0.01)


@contextmanager
def session(binary, root, log, timeout, extra=None):
    environment = {
        **os.environ, **WORKSPACE_ENVIRONMENT, "RS_GOPLS_LOG_FILE": str(log),
        "RS_GOPLS_LOG_REDACT": "1", **(extra or {}),
    }
    client = LspClient([binary], root, timeout, environment=environment)
    try:
        client.initialize(watch_files=True)
        yield client
        client.shutdown()
    finally:
        client.close()


def installed(log, include_tests, timeout):
    return wait_event(log, "workspace_install", lambda row: row["include_tests"] == include_tests, timeout)


def definition(client, origin, target):
    params = reference_params(origin)
    del params["context"]
    expect_location(client.request("textDocument/definition", **params), target)


def exact(response, expected):
    if "error" in response:
        raise RuntimeError(f"reference request failed: {response['error']}")

    def key(path, span):
        return path, *(span[edge][coordinate] for edge in ("start", "end") for coordinate in ("line", "character"))

    actual = Counter(key(path_from_uri(row["uri"]), row["range"]) for row in response["result"] or ())
    want = Counter(key(target.path, target.selection()) for target in expected)
    if want != actual:
        raise RuntimeError(f"wrong locations: missing {want - actual}, extra {actual - want}")
    return actual


def measure(client, origin, declaration, uses, include_declaration=False):
    started = time.perf_counter_ns()
    request = client.start_request(REFERENCE, **reference_params(origin, include_declaration))
    response = client.wait_response(request)
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    exact(response, (declaration, *uses) if include_declaration else uses)
    other = client.start_request(REFERENCE, **reference_params(origin, not include_declaration))
    exact(client.wait_response(other), uses if include_declaration else (declaration, *uses))
    return request, elapsed_ms


def cold_fixture(root, count, uses, large):
    config = f"module {MODULE}\n\ngo 1.22\n"
    factory = root / "factory"
    if not large:
        # Outside the project search scope, these files enter through type resolution.
        factory = root.with_name(root.name + "-factory")
        config += f"\nrequire {MODULE}/factory v0.0.0\nreplace {MODULE}/factory => ../{factory.name}\n"
        write(factory / "go.mod", f"module {MODULE}/factory\n\ngo 1.22\n")
    write(root / "go.mod", config)
    target = write(root / "api" / "api.go", (
        "package api\ntype Token struct{}\nfunc (Token) Run() {}\nconst warmup = 0\n"
    ))
    barrier_target = write(root / "ready" / "ready.go", "package ready\nconst GraphReady = true\n")
    barrier_origin = write(root / "readycheck" / "check.go", (
        f'package readycheck\nimport "{MODULE}/ready"\nvar _ = ready.GraphReady\n'
    ))
    caller = []
    statements = "_ = n + 1; " * 60
    for index in range(count):
        size = (4300 if index == 0 else 360) if large else 12
        text = f'package factory\nimport "{MODULE}/api"\n'
        text += "".join(f"func f{index}_{n}(n int) {{ {statements}}}\n" for n in range(size))
        if index >= count - uses:
            text += f"func Make{index}() api.Token {{ return api.Token{{}} }}\n"
            use = write(root / "caller" / f"use{index:03}.go", (
                f'package caller\nimport dep "{MODULE}/factory"\n'
                f'func Call{index}() {{ _ = "😀"; dep.Make{index}().Run() }}\n'
            ))
            caller.append(site(use, "().[[Run]]()"))
        write(factory / f"dep{index:03}.go", text)
    return (site(target, "(Token) [[Run]]()"), caller,
            site(barrier_origin, "ready.[[GraphReady]]"), site(barrier_target, "const [[GraphReady]] ="))


def test_fixture(root):
    write(root / "go.mod", f"module {MODULE}\n\ngo 1.22\n")
    api = write(root / "api" / "api.go", "package api\ntype Token struct{}\nfunc (Token) Run() {}\n")
    text = (
        f'package client\nimport "{MODULE}/api"\n'
        'func call() { _ = "😀"; api.Token{}.Run() }\n'
    )
    caller = write(root / "client" / "call.go", text)
    internal = write(root / "client" / "call_test.go", text.replace("func call()", "func localTest()"))
    external = write(root / "client" / "ext_test.go", text.replace("package client", "package client_test"))
    return (
        site(api, "(Token) [[Run]]()"), site(caller, "{}.[[Run]]()"),
        [site(path, "{}.[[Run]]()") for path in (caller, internal, external)],
    )


def trace(log, request, elapsed_ms):
    result = next(row for row in events(log, "rpc_done") if row["req_id"] == request)
    counters = [row for row in events(log, "references_trace") if row["req_id"] == request][-1]
    return {
        "wall_ms": round(elapsed_ms, 3), "rpc_ms": result["total_ms"],
        "wait_reason": result["wait_reason"],
        **{key: counters[key] for key in ("disk_reads", "dependency_ast_parses", "ast_parses", "parse_ms")},
    }


def cold_trial(binary, role, root, log, fixture, timeout, include_declaration):
    anchor, uses, barrier, ready = fixture
    with session(binary, root, log, timeout) as client:
        client.open_document(anchor.path, anchor.text)
        definition(client, barrier, ready)
        if role == "candidate":
            installed(log, True, timeout)
        else:
            # Load the test graph without resolving a member or reading its dependencies.
            check = site(anchor.path, "const [[warmup]] =")
            exact(client.wait_response(client.start_request(REFERENCE, **reference_params(check, True))), (check,))
        request, elapsed = measure(client, anchor, anchor, uses, include_declaration)
    return trace(log, request, elapsed)


def warm_trial(binary, role, root, log, fixture, timeout, include_declaration):
    declaration, origin, uses = fixture
    with session(binary, root, log, timeout) as client:
        client.open_document(origin.path, origin.text)
        definition(client, origin, declaration)
        if role == "candidate":
            installed(log, True, timeout)
        request, elapsed = measure(client, origin, declaration, uses, include_declaration)
    result = trace(log, request, elapsed)
    if role == "candidate" and result["wait_reason"] is not None:
        raise RuntimeError(f"a warmed test view unexpectedly blocked the request: {result}")
    return result


def adoption_trial(binary, root, log, fixture, gate, timeout):
    declaration, origin, uses = fixture
    with session(binary, root, log, timeout, gate.environment) as client:
        definition(client, origin, declaration)
        with gate.pause():
            gate.wait_for_snapshot()
            request = client.start_request(REFERENCE, **reference_params(origin, True))
            wait_event(log, "workspace_load_promoted", lambda row: row["include_tests"], timeout)
            if len(gate.calls()) != 2:
                raise RuntimeError(f"expected one production load and one test load, got {gate.calls()}")
        exact(client.wait_response(request), (declaration, *uses))
        exact(client.wait_response(client.start_request(REFERENCE, **reference_params(origin))), uses)
    if len(gate.calls()) != 2:
        raise RuntimeError(f"test view was loaded twice: {gate.calls()}")
    reason = trace(log, request, 0)["wait_reason"]
    if reason != "missing_test_view":
        raise RuntimeError(f"request did not wait for the paused test view: {reason}")
    return {"go_list_calls": len(gate.calls()), "wait_reason": reason}


def supersession_trial(binary, root, log, fixture, gate, timeout):
    declaration, origin, uses = fixture
    tagged = write(root / "client" / "tagged.go", (
        "//go:build phase3\n\n" + origin.text.replace("func call()", "func tagged()")
    ))
    tagged_use = site(tagged, "{}.[[Run]]()")
    build_flags = "-tags=phase3"
    with session(binary, root, log, timeout, gate.environment) as client:
        definition(client, origin, declaration)
        production = installed(log, False, timeout)["target_generation"]
        with gate.pause():
            gate.wait_for_snapshot()
            client.notify("workspace/didChangeConfiguration", settings={"env": {"GOFLAGS": build_flags}})
            # The cancelled load must finish even though the relay has not released its snapshot.
            wait_event(log, "workspace_load_done", lambda row: (
                row["target_generation"] == production and row["include_tests"] and row["cancelled"]
            ), timeout)
        measure(client, origin, declaration, [*uses, tagged_use])
    test_installs = [row for row in events(log, "workspace_install") if row["include_tests"]]
    if len(test_installs) != 1 or test_installs[0]["target_generation"] <= production:
        raise RuntimeError(f"expected just the new test view after cancellation, got {test_installs}")
    with session(binary, root, log.with_name("force-build-flags.jsonl"), timeout,
                 {"GOFLAGS": build_flags}) as fresh:
        measure(fresh, origin, declaration, [*uses, tagged_use])
    return {"cancelled_loads": 1, "stale_installs": 0, "build_flags_force_load_equal": True}


def watch_trial(binary, root, log, fixture, timeout, scratch):
    declaration, origin, uses = fixture
    selected = root / "client" / "watched.go"
    alternate = selected.with_name("renamed.go")
    dirty_starts = []
    with session(binary, root, log, timeout) as client:
        client.open_document(origin.path, origin.text)
        definition(client, origin, declaration)
        generation = installed(log, True, timeout)["target_generation"]
        for group in range(15):
            previous = selected if selected.exists() else alternate
            upcoming = alternate if previous == selected else selected
            changes = []
            if previous.exists():
                previous.unlink()
                changes.append({"uri": previous.as_uri(), "type": 3})
            # After a burst of create/change notifications, only the final build selection matters.
            prefix = "// +build excluded\n\n" if group % 3 == 1 else "// +build excluded\n"
            text = prefix + origin.text.replace("func call()", f"func watched{group}()")
            write(upcoming, text)
            changed = {"uri": upcoming.as_uri(), "type": 2}
            messages = [
                {"method": WATCHER, "params": {"changes": [*changes, {"uri": upcoming.as_uri(), "type": 1}]}},
                {"method": "textDocument/didChange", "params": {
                    "textDocument": {"uri": origin.path.as_uri(), "version": group + 2},
                    "contentChanges": [{"text": f"// edit {group}\n" + origin.text}],
                }},
                *({"method": WATCHER, "params": {"changes": [changed]}} for _ in range(5)),
            ]
            client.send_batch(*messages)
            overlay = site(origin.path, "{}.[[Run]]()", f"// edit {group}\n" + origin.text)
            expected = [declaration, overlay, *uses[1:]]
            if group % 3 != 1:
                expected.append(site(upcoming, "{}.[[Run]]()"))
            request = client.start_request(REFERENCE, **reference_params(overlay, True))
            actual = exact(client.wait_response(request), expected)
            starts = [row for row in events(log, "workspace_load_start") if row["target_generation"] > generation]
            if len(starts) != 1:
                raise RuntimeError(f"watch burst {group + 1}: expected one new load, got {starts}")
            generation = starts[0]["target_generation"]
            dirty_starts.append(generation)
        # A separate process loads the stabilized paths from scratch and receives the same overlay.
        with session(binary, root, scratch / "force-load.jsonl", timeout) as fresh:
            fresh.open_document(origin.path, overlay.text)
            forced = fresh.start_request(REFERENCE, **reference_params(overlay, True))
            if exact(fresh.wait_response(forced), expected) != actual:
                raise RuntimeError("stabilized graph differs from full load")
    loads = [row for row in events(log, "workspace_load_done") if row["target_generation"] in dirty_starts]
    installs = [row for row in events(log, "workspace_install") if row["target_generation"] in dirty_starts]
    if len(loads) != 15 or len(installs) != 15 or any(row["stale"] for row in loads):
        raise RuntimeError(f"expected 15 successful current installations, got {loads}, {installs}")
    return {"windows": 15, "max_new_loads_per_window": 1, "stale_installs": 0, "force_load_equal": True}


def distribution(samples):
    values = sorted(row["wall_ms"] for row in samples)
    p95 = statistics.quantiles(values, n=100, method="inclusive")[94] if len(values) > 1 else values[0]
    return {"n": len(values), "p50_ms": statistics.median(values), "p95_ms": p95, "max_ms": max(values)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server", type=executable)
    parser.add_argument("--prior", type=executable)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.samples < 1 or args.timeout <= 0:
        parser.error("samples and timeout must be positive")
    servers = [("candidate", args.server)]
    if args.prior:
        servers.append(("prior", args.prior))
    report = {"sha256": {role: hashlib.sha256(Path(binary).read_bytes()).hexdigest() for role, binary in servers}}
    with tempfile.TemporaryDirectory(prefix="rs-gopls-phase3-") as temporary:
        root = Path(temporary)
        for lane, count, uses in (("cold-26", 26, 17), ("cold-168", 168, 1), ("test-view", 0, 0)):
            project = root / lane
            fixture = cold_fixture(project, count, uses, count == 26) if count else test_fixture(project)
            trial = cold_trial if count else warm_trial
            samples = {role: [] for role, _ in servers}
            for sample in range(args.samples):
                for role, binary in servers if sample % 2 == 0 else reversed(servers):
                    result = trial(binary, role, project, root / f"{lane}-{role}-{sample}.jsonl",
                                   fixture, args.timeout, sample % 2 != 0)
                    samples[role].append(result)
                    print(f"{lane} {role} {sample + 1}: {result}", flush=True)
            summary = {role: distribution(rows) for role, rows in samples.items()}
            if args.prior:
                summary["p50_speedup"] = summary["prior"]["p50_ms"] / summary["candidate"]["p50_ms"]
            report[lane] = {"summary": summary, "samples": samples}

        project = root / "test-view"
        gate = PausedGoList(root / "control", args.timeout)
        report["in_flight_adoption"] = adoption_trial(args.server, project, root / "adoption.jsonl",
                                                      fixture, gate, args.timeout)
        gate = PausedGoList(root / "supersession", args.timeout)
        report["prewarm_supersession"] = supersession_trial(args.server, project, root / "supersession.jsonl",
                                                           fixture, gate, args.timeout)
        report["watcher"] = watch_trial(args.server, project, root / "watcher.jsonl", fixture, args.timeout, root)
    result = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(result + "\n", encoding="utf-8")
    print(result)


if __name__ == "__main__":
    main()

"""Exercise diagnostic and navigation JSONL using a built stdio server."""

from collections import Counter
from contextlib import contextmanager
import json
import os
from pathlib import Path
from queue import Queue
import shutil
import sys
import tempfile
import time
import unittest

from lsp_harness import LspClient, PausedGoList, executable, expect_error, notify_changes, session, site, write
from workspace_checks._assertions import check_references, expect_location, reference_params
from workspace_checks.diagnostics import capability_session, expect_publications, full_report, pull
from workspace_checks.gopath_fixture import dual_gopath, isolated_go_environment


_BINARY = os.environ.get("RS_GOPLS_TEST_TELEMETRY_BINARY")
_LOG_FILE = "RS_GOPLS_LOG_FILE"
_REDACT = "RS_GOPLS_LOG_REDACT"
_SOURCE_CANARY = "SOURCE_CANARY_58cb5c7320"
_ID_CANARY = "RPC_ID_CANARY_a918ba0754"
_DIAGNOSTIC_NAME = "MissingIdentifier_ef1713a99e"
_DIAGNOSTIC_FILE = "diagnostic-fixture-e1ae423.go"
_REFRESH_ERROR = "REFRESH_ACK_ERROR_e11d24cecc"
_TEMP_PREFIX = "rs-gopls-telemetry-workspace-"
_TIMEOUT = 30
_REFRESH = "workspace/diagnostic/refresh"
_HASH = r"^h:[0-9a-f]{16}$"
_LANES = {"foreground", "reserve", "background", "large"}
_CODES = {
    "syntax.error", "package.load", "import.unused", "import.invalid", "import.self",
    "import.cycle", "import.duplicate_alias", "name.unresolved", "name.unexported",
    "type.unresolved", "member.unresolved", "name.redeclared", "var.unused",
    "shortvar.no_new_vars", "const.reassign", "call.non_function", "call.arity",
    "return.arity", "type.mismatch", "type.interface_not_implemented",
    "type.condition_not_bool", "type.nil_without_type", "literal.unknown_field",
    "switch.duplicate_case", "expr.blank_as_value",
}


def _invalid_constant(value):
    raise ValueError(f"invalid JSON constant {value}")


class _RefreshClient(LspClient):
    """Hand refresh requests to the test so it can vary ACK timing and outcome."""

    def __init__(self, *args, **kwargs):
        self.refreshes = Queue()
        super().__init__(*args, **kwargs)

    def reply(self, message):
        if message["method"] == _REFRESH:
            self.refreshes.put(message)
        else:
            super().reply(message)

    def next_refresh(self):
        return self.refreshes.get(timeout=self.timeout)["id"]


class DiagnosticTelemetryTests(unittest.TestCase):
    def setUp(self):
        if not _BINARY:
            self.skipTest("pass a release binary or set RS_GOPLS_TEST_TELEMETRY_BINARY")
        if not shutil.which("go"):
            self.skipTest("Go is required for the stdio telemetry fixture")
        self.server = executable(_BINARY)
        temporary = tempfile.TemporaryDirectory(prefix=_TEMP_PREFIX)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.fixture = dual_gopath(self.root / "project")
        self.environment = {
            key: value for key, value in isolated_go_environment().items()
            if key.upper() not in {_LOG_FILE, _REDACT}
        }
        self.environment.update(GOPATH=self.fixture.gopath, GO111MODULE="auto")

    def _session(self, *, environment=None, options=None):
        return session(
            self.server, self.fixture.root, _TIMEOUT, watch_files=True,
            environment={**self.environment, **(environment or {})},
            initialization_options={"env": options or {}},
        )

    @contextmanager
    def _diagnostic_session(self, project, capabilities, *, log=None, redact=False, environment=None):
        env = {**self.environment, "GO111MODULE": "on", "GOWORK": "off", **(environment or {})}
        if log is not None:
            env[_LOG_FILE] = str(log)
        if redact:
            env[_REDACT] = "1"
        client = _RefreshClient([self.server], project, _TIMEOUT, environment=env)
        try:
            result = client.request("initialize", rootUri=project.as_uri(),
                                    workspaceFolders=client.folders, capabilities=capabilities)
            client.notify("initialized")
            yield client, result["capabilities"]
            client.shutdown()
        finally:
            client.close()

    def _diagnostic_fixture(self):
        project = self.root / "module"
        write(project / "go.mod", "module example.test/telemetry\n\ngo 1.22\n")
        clean = "package sample\nvar Value = 1\n"
        path = write(project / _DIAGNOSTIC_FILE, clean)
        write(project / "dep" / "dep.go", "package dep\nconst Value = 42\n")
        # Syntax is locally verifiable before the graph arrives. The full report
        # also needs resolution, including a cold dependency distinct from the target.
        text = (
            'package sample\nimport "example.test/telemetry/dep"\n'
            f"// {_SOURCE_CANARY}\n"
            "func broken() { _ = (1 + ) }\n"
            f"func sample() int {{ _ = dep.Value; return {_DIAGNOSTIC_NAME} }}\n"
        )
        return project, path, clean, text

    def _wait_log(self, log, event, predicate, *, after=0):
        deadline = time.monotonic() + _TIMEOUT
        while True:
            rows = self._read_log(log, complete=False) if log.exists() else []
            for index, row in enumerate(rows[after:], after):
                if row["event"] == event and predicate(row):
                    return index + 1, row
            self.assertLess(time.monotonic(), deadline, f"missing {event} in {log.name}")
            time.sleep(0.005)

    def _navigate(self, client):
        origin = self.fixture.reference
        target = self.fixture.target
        text = origin.text + f"\n// {_SOURCE_CANARY}\n"
        client.open_document(origin.path, text)
        client.change_document(origin.path, 2, text + "\n")
        params = {"textDocument": {"uri": origin.path.as_uri()}, "position": origin.cursor()}
        result = client.request("textDocument/definition", **params)
        expect_location(result, target)
        requests = {client.next_id: ("textDocument/definition", 1, "no_graph")}
        for wait_reason in ("missing_test_view", None):
            check_references(client, origin, (origin, target), include_declaration=True)
            requests[client.next_id] = ("textDocument/references", 2, wait_reason)
        client.notify("textDocument/didSave", textDocument=params["textDocument"], text=text)
        client.close_document(origin.path)
        return requests

    @staticmethod
    def _events(rows, name):
        return [row for row in rows if row["event"] == name]

    def _read_log(self, log, *, complete=True):
        raw = log.read_text(encoding="utf-8")
        if complete:
            self.assertTrue(raw.endswith("\n"), "log did not finish with a complete JSONL line")
        else:
            raw = raw.rpartition("\n")[0]
        rows = [json.loads(line, parse_constant=_invalid_constant) for line in raw.splitlines()]
        if complete:
            self.assertTrue(rows, "log is empty")
        self.assertFalse(_SOURCE_CANARY in raw, "log includes source text")
        self.assertFalse(_ID_CANARY in raw, "log includes a raw string request ID")
        self.assertFalse(_TEMP_PREFIX in raw.casefold(), "log includes an absolute fixture path")
        self.assertNotIn(_DIAGNOSTIC_NAME, raw, "log includes a source identifier")
        self.assertNotIn(_REFRESH_ERROR, raw, "log includes refresh ACK error text")
        self.assertNotIn("rs-gopls/diagnostic-refresh/", raw, "log includes a raw refresh request ID")
        self.assertNotIn("file://", raw.casefold(), "log includes a file URI")
        if complete:
            paths = {path.name for path in self.root.rglob("*.go")}
            paths.update((_DIAGNOSTIC_FILE, "go.mod", "go.work", "go.sum"))
            for name in sorted(paths):
                self.assertNotIn(name.casefold(), raw.casefold(), "log includes a fixture basename")
        last_timestamp = 0
        for row in rows:
            self.assertIsInstance(row, dict)
            self.assertIsInstance(row["event"], str)
            self.assertGreaterEqual(row["ts_ms"], last_timestamp)
            last_timestamp = row["ts_ms"]
        self._check_log_privacy(rows)
        return rows

    def _check_log_privacy(self, rows, *, messages=()):
        def check_fields(fields, event):
            if isinstance(fields, dict):
                for name, value in fields.items():
                    if (name in {"document", "path", "directory", "slowest_file"} and value is not None
                            and (event, name) != ("document_highlight", "path")):
                        self.assertRegex(value, _HASH)
                    check_fields(value, event)
            elif isinstance(fields, list):
                for value in fields:
                    check_fields(value, event)

        for row in rows:
            with self.subTest(event=row["event"]):
                check_fields(row, row["event"])
                if messages:
                    raw = json.dumps(row, ensure_ascii=False)
                    for message in messages:
                        encoded = json.dumps(message, ensure_ascii=False)[1:-1]
                        self.assertNotIn(encoded, raw, "log includes a wire diagnostic message")

    def _check_codes(self, row, expected=None):
        counts = row["diagnostic_codes"]
        self.assertIsInstance(counts, dict)
        self.assertLessEqual(counts.keys(), _CODES)
        for count in counts.values():
            self.assertIs(type(count), int)
            self.assertGreaterEqual(count, 0)
        if expected is not None:
            self.assertEqual({code: count for code, count in counts.items() if count}, Counter(expected))

    def _check_job_link(self, row, start):
        for field in ("job", "run", "document", "doc_version", "stage", "lane"):
            self.assertEqual(row[field], start[field], f"{row['event']} has a different {field}")

    def _check_worker_rows(self, rows):
        starts = {}
        for row in self._events(rows, "diagnostic_worker_start"):
            self.assertNotIn(row["job"], starts)
            starts[row["job"]] = row
            self.assertRegex(row["document"], _HASH)
            self.assertIn(row["stage"], ("stage1", "stage2"))
            self.assertIn(row["context_completeness"], ("partial", "full"))
            self.assertIn(row["lane"], _LANES)
            for field in ("queue_wait_ms", "merge_wait_ms", "runnable_queue_wait_ms", "context_wait_ms"):
                self.assertGreaterEqual(row[field], 0)
            self.assertAlmostEqual(row["queue_wait_ms"], row["worker_started_ms"] - row["enqueued_ms"], delta=0.02)
            self.assertAlmostEqual(row["queue_wait_ms"], row["merge_wait_ms"] + row["context_wait_ms"]
                                   + row["runnable_queue_wait_ms"], delta=0.02)
        self.assertTrue(starts)
        finished = {}
        for row in self._events(rows, "diagnostic_worker_done"):
            job = row["job"]
            self.assertNotIn(job, finished)
            finished[job] = row
            start = starts[job]
            self._check_job_link(row, start)
            self.assertGreaterEqual(row["worker_exited_ms"], start["worker_started_ms"])
            self.assertGreaterEqual(row["analysis_ms"], 0)
            for field in ("queue_wait_ms", "merge_wait_ms"):
                self.assertEqual(row[field], start[field])
            for field in ("cold_dep_reads", "cold_dep_parses"):
                self.assertIs(type(row[field]), int)
                self.assertGreaterEqual(row[field], 0)
            self._check_codes(row)
            if row["cancelled"]:
                requested, exited = row["cancel_requested_ms"], row["worker_exited_ms"]
                self.assertGreaterEqual(exited, requested)
                self.assertGreaterEqual(row["cancel_lag_ms"], 0)
                self.assertAlmostEqual(row["cancel_lag_ms"], exited - requested, delta=0.02)
            else:
                self.assertIsNone(row["cancel_requested_ms"])
                self.assertIsNone(row["cancel_lag_ms"])
        self.assertEqual(finished.keys(), starts.keys())
        stages = self._events(rows, "diagnostic_stages")
        self.assertTrue(stages)
        for row in stages:
            done = finished[row["job"]]
            self._check_job_link(row, done)
            for field in ("context_completeness", "diagnostic_codes", "cold_dep_reads", "cold_dep_parses"):
                self.assertEqual(row[field], done[field])
            for field in ("syntax_ms", "declarations_ms", "imports_ms", "unused_ms", "unresolved_ms",
                          "types_ms", "package_errors_ms", "inspected_nodes"):
                self.assertGreaterEqual(row[field], 0)
        return starts

    def _check_pull_delivery(self, rows, starts, req_id, *, version, stage, context, delivery, kind, codes):
        response, = [row for row in self._events(rows, "rpc_done") if row["req_id"] == req_id]
        report, = [row for row in self._events(rows, "diagnostic_delivery")
                   if row.get("req_id") == req_id and row["delivery"] == delivery]
        self.assertEqual(response["method"], "textDocument/diagnostic")
        self.assertEqual(response["outcome"], "ok")
        self.assertEqual((response["doc_version"], response["stage"], response["context_completeness"]),
                         (version, stage, context))
        self.assertEqual((response["delivery"], response["report_kind"]), (delivery, kind))
        for field in ("job", "run", "document", "doc_version", "stage", "context_completeness", "lane",
                      "result_id", "report_kind", "delivery", "diagnostic_codes"):
            self.assertEqual(response[field], report[field], f"Pull response has a different {field}")
        self.assertRegex(response["result_id"], r"^(h:)?[0-9a-f]{16}$")
        self._check_job_link(response, starts[response["job"]])
        self._check_codes(response, codes)
        return response

    def _check_session(self, rows, pid, source):
        start, = self._events(rows, "session_start")
        config, = self._events(rows, "session_config")
        self.assertEqual(start["pid"], pid)
        self.assertTrue(start["version"])
        self.assertGreater(start["started_at_unix_ms"], 0)
        self.assertEqual(start["source"], source)
        self.assertIs(start["redact"], True)
        self.assertEqual(config["source"], source)
        self.assertEqual(config["workspace_folders"], 1)
        self.assertEqual(config["go111module"], "auto")
        self.assertEqual(config["gopath_segments"], 2)
        for event in ("workspace_load_start", "workspace_load_done", "workspace_install_start",
                      "workspace_install", "workspace_replay"):
            self.assertTrue(self._events(rows, event), f"missing {event}")
        for row in self._events(rows, "workspace_install"):
            self.assertGreaterEqual(row["install_ms"], row["metadata_ms"])
        commands = self._events(rows, "workspace_load_cmd")
        for command in ("go env", "go list"):
            starts = {row["pid"] for row in commands if row["phase"] == "cmd_start" and row["command"] == command}
            completed = [row for row in commands if row["phase"] == "cmd_done"
                         and row["command"] == command and row["exit_code"] == 0]
            self.assertTrue(completed, f"missing completed {command}")
            for row in completed:
                self.assertGreater(row["pid"], 0)
                self.assertGreaterEqual(row["cmd_ms"], 0)
                self.assertIn(row["pid"], starts)
                if command == "go list":
                    self.assertGreaterEqual(row["package_count"], 3)
                    self.assertGreaterEqual(row["project_file_count"], 3)
                    self.assertEqual(row["error_count"], 0)

    def _check_requests(self, rows, requests):
        origin = self.fixture.reference
        position = (origin.cursor()["line"], origin.cursor()["character"])
        receipts = self._events(rows, "rpc_recv")
        done = self._events(rows, "rpc_done")
        documents = set()
        for req_id, (method, count, wait_reason) in requests.items():
            received, = [row for row in receipts if row["req_id"] == req_id]
            response, = [row for row in done if row["req_id"] == req_id]
            self.assertEqual(received["method"], method)
            self.assertEqual(response["method"], method)
            self.assertEqual(response["outcome"], "ok")
            self.assertEqual(response["result_count"], count)
            self.assertGreaterEqual(response["ts_ms"], received["ts_ms"])
            self.assertEqual(response["dispatch_queue_ms"], received["dispatch_queue_ms"])
            self.assertGreaterEqual(received["dispatch_queue_ms"], 0)
            self.assertLessEqual(received["dispatch_queue_ms"], response["total_ms"])
            for timer in ("queue_wait_ms", "exec_ms", "serialize_ms", "write_ms", "total_ms"):
                self.assertGreaterEqual(response[timer], 0)
            self.assertIs(response["waited_workspace"], wait_reason is not None)
            self.assertEqual(response["wait_reason"], wait_reason)
            self.assertEqual(response["replay_count"], int(wait_reason is not None))
            for row in (received, response):
                documents.add(row["document"])
                self.assertEqual((row["line"], row["char"]), position)

            definition = method == "textDocument/definition"
            event = "definition_trace" if definition else "references_trace"
            count_field = "found_count" if definition else "matches_count"
            traces = [row for row in self._events(rows, event) if row["req_id"] == req_id]
            self.assertTrue(traces, f"missing {event} for request {req_id}")
            trace = traces[-1]
            documents.add(trace["document"])
            self.assertEqual((trace["line"], trace["char"]), position)
            self.assertEqual(trace[count_field], count)
            self.assertGreaterEqual(trace["exec_ms"], 0)
            if definition:
                self.assertEqual(trace["hit_path"], "cross_package")
            else:
                self.assertFalse(trace["cancelled"])
                self.assertGreaterEqual(trace["wall_ms"], trace["exec_ms"])
                self.assertGreaterEqual(trace["processed_files"], 2)
                self.assertGreaterEqual(trace["candidate_files"], trace["processed_files"])

        initialize, = [row for row in done if row["method"] == "initialize"]
        shutdown, = [row for row in done if row["method"] == "shutdown"]
        self.assertEqual((initialize["outcome"], initialize["result_count"]), ("ok", 1))
        self.assertEqual((shutdown["outcome"], shutdown["result_count"]), ("null", 0))
        document, = documents
        return document

    def test_process_environment_and_path_redaction(self):
        log = self.root / "diagnostics" / "process.jsonl"
        ignored = self.root / "unused-options.jsonl"
        write(log, '{"event": "previous_session", "ts_ms": 0}\n')
        with self._session(environment={_LOG_FILE: str(log)}, options={_LOG_FILE: str(ignored)}) as client:
            client.change_configuration(None)
            requests = self._navigate(client)
            unsupported = client.start_request(_SOURCE_CANARY)
            expect_error(client.wait_response(unsupported), -32601)

        self.assertEqual(client.process.returncode, 0)
        self.assertFalse(ignored.exists())
        rows = self._read_log(log)
        self.assertEqual(rows[0]["event"], "previous_session")
        self._check_session(rows, client.process.pid, "process_env")
        document = self._check_requests(rows, requests)
        self.assertRegex(document, _HASH)
        unknown, = [row for row in self._events(rows, "rpc_done") if row["req_id"] == unsupported]
        self.assertEqual((unknown["method"], unknown["outcome"]), ("unknown", "error_-32601"))
        methods = {row["method"] for row in self._events(rows, "notification")}
        self.assertTrue({
            "textDocument/didOpen", "textDocument/didChange", "textDocument/didSave",
            "textDocument/didClose", "workspace/didChangeConfiguration",
        }.issubset(methods))

    def test_initialization_options_and_session_keyed_redaction(self):
        documents = []
        for run in range(2):
            log = self.root / "diagnostics" / f"redacted-{run}.jsonl"
            with self._session(options={_LOG_FILE: str(log), _REDACT: "0"}) as client:
                requests = self._navigate(client)

            self.assertEqual(client.process.returncode, 0)
            rows = self._read_log(log)
            self._check_session(rows, client.process.pid, "initialization_options")
            document = self._check_requests(rows, requests)
            self.assertRegex(document, r"^h:[0-9a-f]{16}$")
            documents.append(document)
        self.assertNotEqual(*documents)

    def test_same_basename_across_workspace_roots(self):
        roots = [self.root / name for name in ("privacy-root-left", "privacy-root-right")]
        source = f"package main\nfunc main() {{}}\n// {_SOURCE_CANARY}\n"
        paths = []
        for root in roots:
            write(root / "go.mod", f"module example.test/{root.name}\n\ngo 1.22\n")
            paths.append(write(root / "main.go", source))
        log = self.root / "diagnostics" / "multi-root.jsonl"
        env = {**self.environment, "GO111MODULE": "on", "GOWORK": "off", _LOG_FILE: str(log)}
        capabilities = {"workspace": {"workspaceFolders": True}, "textDocument": {"diagnostic": {}}}
        with capability_session(self.server, roots[0], _TIMEOUT, capabilities, folders=roots,
                                environment=env) as (client, _):
            ids = []
            for path in paths:
                client.open_document(path, source)
                full_report(pull(client, path), [])
                ids.append(client.next_id)
                client.close_document(path)

        rows = self._read_log(log)
        opened = [row["document"] for row in self._events(rows, "notification")
                  if row["method"] == "textDocument/didOpen"]
        self.assertEqual(len(opened), 2)
        self.assertNotEqual(*opened)
        for req_id, document in zip(ids, opened, strict=True):
            for event in ("rpc_recv", "rpc_done", "diagnostic_delivery"):
                row, = [row for row in self._events(rows, event) if row["req_id"] == req_id]
                self.assertEqual(row["document"], document)
        for root in roots:
            self.assertNotIn(root.name, log.read_text(encoding="utf-8"))

    def test_disabled_does_not_create_log(self):
        late_log = self.root / "late.jsonl"
        project, path, _, text = self._diagnostic_fixture()
        fixture_files = {path for path in self.root.rglob("*") if path.is_file()}
        with self._session() as client:
            client.change_configuration({"env": {_LOG_FILE: str(late_log)}})
            self._navigate(client)
        with self._diagnostic_session(project, {"textDocument": {"diagnostic": {}}}) as (client, _):
            client.open_document(path, text)
            full_report(pull(client, path), ["name.unresolved", "syntax.error"])

        self.assertEqual(client.process.returncode, 0)
        self.assertFalse(late_log.exists())
        self.assertEqual(fixture_files, {path for path in self.root.rglob("*") if path.is_file()})

    def test_pull_stage_upgrade_cache_and_refresh_lifecycle(self):
        project, path, _, text = self._diagnostic_fixture()
        log = self.root / "diagnostics" / "pull.jsonl"
        gate = PausedGoList(self.root / "gate", _TIMEOUT)
        capabilities = {
            "workspace": {"diagnostics": {"refreshSupport": True}},
            "textDocument": {"diagnostic": {}},
        }
        early_codes = ["syntax.error"]
        terminal_codes = ["name.unresolved", "syntax.error"]
        with self._diagnostic_session(project, capabilities, log=log, redact=True,
                                      environment=gate.environment) as (client, caps):
            self.assertIn("diagnosticProvider", caps)
            with gate.pause():
                client.open_document(path, text)
                early = pull(client, path)
                early_request = client.next_id
                early_result = full_report(early, early_codes)
                gate.wait_for_snapshot()

            _, upgrade = self._wait_log(log, "diagnostic_refresh_merge", lambda row: row.get("stage") == "stage2")
            first_refresh = client.next_refresh()
            while int(first_refresh.rsplit("/", 1)[1]) < upgrade["refresh_id"]:
                client.send(id=first_refresh, result=None)
                first_refresh = client.next_refresh()
            # Pull succeeds while the refresh ACK is still outstanding.
            full = pull(client, path, early_result)
            full_request = client.next_id
            full_result = full_report(full, terminal_codes)
            self.assertNotEqual(full_result, early_result)
            unresolved, = [item for item in full["items"] if item["code"] == "name.unresolved"]
            self.assertEqual(unresolved["range"], site(path, f"[[{_DIAGNOSTIC_NAME}]]", text).selection())
            self.assertEqual(pull(client, path, full_result), {"kind": "unchanged", "resultId": full_result})
            unchanged_request = client.next_id
            client.send(id=first_refresh, result=None)

            disk_path = project / "dep" / "dep.go"
            full_report(pull(client, disk_path), [])
            disk_request = client.next_id

            def workspace_refresh(index):
                new_file = write(project / f"note{index}.go", "package sample\n")
                notify_changes(client, (new_file, 1))
                return client.next_refresh()

            second_refresh = workspace_refresh(2)
            client.send(id=second_refresh, error={"code": -32603, "message": _REFRESH_ERROR})
            third_refresh = workspace_refresh(3)
            third_sequence = int(third_refresh.rsplit("/", 1)[1])
            self._wait_log(log, "diagnostic_refresh_timeout",
                           lambda row: row["refresh_id"] == third_sequence)
            client.send(id=third_refresh, result=None)  # A late ACK must not reclaim the cleared request.
            fourth_refresh = workspace_refresh(4)
            client.send(id=fourth_refresh, result=None)
            client.close_document(path)

        rows = self._read_log(log)
        self._check_log_privacy(rows, messages={item["message"] for item in full["items"]})
        starts = self._check_worker_rows(rows)
        preview = self._check_pull_delivery(rows, starts, early_request, version=1, stage="stage1",
                                            context="partial", delivery="delivered", kind="full", codes=early_codes)
        terminal = self._check_pull_delivery(rows, starts, full_request, version=1, stage="stage2", context="full",
                                             delivery="cache_hit", kind="full", codes=terminal_codes)
        unchanged = self._check_pull_delivery(rows, starts, unchanged_request, version=1, stage="stage2",
                                              context="full", delivery="cache_hit", kind="unchanged",
                                              codes=terminal_codes)
        disk = self._check_pull_delivery(rows, starts, disk_request, version=None, stage="stage2", context="full",
                                        delivery="delivered", kind="full", codes=[])
        self.assertNotEqual(disk["document"], terminal["document"])
        self.assertEqual(preview["document"], terminal["document"])
        self.assertEqual(preview["run"], terminal["run"])
        self.assertNotEqual(preview["job"], terminal["job"])
        self.assertNotEqual(preview["result_id"], terminal["result_id"])
        self.assertEqual(terminal["result_id"], unchanged["result_id"])
        self._check_job_link(upgrade, starts[terminal["job"]])
        self.assertEqual(upgrade["result_id"], terminal["result_id"])
        terminal_worker, = [row for row in self._events(rows, "diagnostic_worker_done")
                            if row["job"] == terminal["job"]]
        self._check_codes(terminal_worker, terminal_codes)
        self.assertGreater(terminal_worker["context_wait_ms"], 0)
        self.assertIn(terminal_worker["cold_dep_reads"], (0, 1))
        self.assertEqual(terminal_worker["cold_dep_parses"], 1)
        self.assertFalse(self._events(rows, "diagnostic_push_write"))

        sent = {row["refresh_id"]: row for row in self._events(rows, "diagnostic_refresh_send")}
        writes = {row["refresh_id"]: row for row in self._events(rows, "diagnostic_refresh_write")}
        acks = {row["refresh_id"]: row for row in self._events(rows, "diagnostic_refresh_ack")}
        for request, ok, code in ((first_refresh, True, None), (second_refresh, False, -32603),
                                  (third_refresh, None, None), (fourth_refresh, True, None)):
            sequence = int(request.rsplit("/", 1)[1])
            send = sent[sequence]
            write_row = writes[sequence]
            self.assertRegex(send["req_id"], _HASH)
            self.assertEqual(write_row["req_id"], send["req_id"])
            self.assertTrue(write_row["write_ok"])
            self.assertGreaterEqual(write_row["write_ms"], 0)
            self.assertLess(rows.index(send), rows.index(write_row))
            if ok is None:
                self.assertNotIn(sequence, acks)
                timeout, = [row for row in self._events(rows, "diagnostic_refresh_timeout")
                            if row["refresh_id"] == sequence]
                self.assertEqual(timeout["req_id"], send["req_id"])
                self.assertGreater(timeout["timeout_ms"], 0)
                self.assertGreaterEqual(timeout["elapsed_ms"], timeout["timeout_ms"])
            else:
                ack = acks[sequence]
                self.assertEqual((ack["req_id"], ack["ack_ok"], ack["code"]), (send["req_id"], ok, code))
                self.assertLess(rows.index(send), rows.index(ack))
                self.assertGreaterEqual(ack["ts_ms"], send["ts_ms"])
                self.assertGreaterEqual(ack["ack_ms"], 0)
        self.assertEqual(upgrade["source"], "document")
        self.assertIs(type(upgrade["coalesced"]), bool)
        self.assertEqual(upgrade["req_id"], sent[upgrade["refresh_id"]]["req_id"])
        self.assertTrue(any(mark["job"] == terminal["job"]
                            and mark["result_id"] == terminal["result_id"]
                            for mark in sent[upgrade["refresh_id"]]["documents"]))
        refresh_error, = self._events(rows, "diagnostic_refresh_error")
        self.assertEqual(refresh_error["code"], -32603)
        self.assertEqual(refresh_error["req_id"], acks[refresh_error["refresh_id"]]["req_id"])

    def test_push_writes_with_and_without_version_support(self):
        project, path, clean, text = self._diagnostic_fixture()
        gate = PausedGoList(self.root / "gate", _TIMEOUT)
        early_codes = ["syntax.error"]
        terminal_codes = ["name.unresolved", "syntax.error"]
        for versioned in (True, False):
            with self.subTest(version_support=versioned):
                log = self.root / "diagnostics" / f"push-{versioned}.jsonl"
                publish = {"versionSupport": True} if versioned else {}
                capabilities = {"textDocument": {"publishDiagnostics": publish}}
                # The server's existing wire behavior includes the sync version for both
                # capability variants. The capability governs staged full-context delivery.
                with self._diagnostic_session(project, capabilities, log=log,
                                              environment=gate.environment) as (client, _):
                    with gate.pause():
                        client.open_document(path, text)
                        expect_publications(client, {path: (1, early_codes)})
                        gate.wait_for_snapshot()
                    expect_publications(client, {path: (1, terminal_codes)})
                    client.change_document(path, 2, clean)
                    expect_publications(client, {path: (2, [])})
                    self._wait_log(log, "diagnostic_worker_done",
                                   lambda row: row["doc_version"] == 2 and row["stage"] == "stage2")
                    client.close_document(path)
                    expect_publications(client, {path: (None, [])})

                rows = self._read_log(log)
                self._check_log_privacy(rows, messages=("Expected an identifier or expression",))
                starts = self._check_worker_rows(rows)
                writes = self._events(rows, "diagnostic_push_write")
                self.assertEqual([row["push_version"] for row in writes], [1, 1, 2, None])
                self.assertEqual([row["doc_version"] for row in writes], [1, 1, 2, 2])
                self.assertEqual([(row["stage"], row["context_completeness"]) for row in writes[:2]],
                                 [("stage1", "partial"), ("stage2", "full")])
                self.assertEqual(writes[0]["run"], writes[1]["run"])
                self.assertEqual([row["stage"] for row in starts.values() if row["doc_version"] == 2],
                                 ["stage1", "stage2"] if versioned else ["stage2"])
                self.assertFalse(self._events(rows, "diagnostic_refresh_send"))
                for row, codes in zip(writes[:3], (early_codes, terminal_codes, []), strict=True):
                    self.assertEqual((row["delivery"], row["report_kind"]), ("delivered", "full"))
                    self.assertTrue(row["write_ok"])
                    self.assertGreaterEqual(row["write_ms"], 0)
                    self._check_job_link(row, starts[row["job"]])
                    self._check_codes(row, codes)
                    delivered, = [report for report in self._events(rows, "diagnostic_delivery")
                                  if report["delivery"] == "delivered" and report["job"] == row["job"]]
                    self.assertEqual((row["result_id"], row["diagnostic_codes"]),
                                     (delivered["result_id"], delivered["diagnostic_codes"]))
                clear = writes[-1]
                self.assertEqual(clear["report_kind"], "clear")
                self.assertIsNone(clear["job"])
                self.assertIsNone(clear["result_id"])
                self._check_codes(clear, [])

    def test_running_diagnostic_cancellation_and_supersession(self):
        project, path, clean, _ = self._diagnostic_fixture()
        log = self.root / "diagnostics" / "cancel.jsonl"
        slow = "package sample\nfunc slow() {\n" + "\t_ = 1 + 2 + 3 + 4 + 5\n" * 70_000 + "}\n"
        params = {"textDocument": {"uri": path.as_uri()}}
        capabilities = {"textDocument": {"diagnostic": {}}}
        with self._diagnostic_session(project, capabilities, log=log, redact=True) as (client, _):
            client.open_document(path, slow)
            cancel_request = client.start_request("textDocument/diagnostic", **params)
            _, first = self._wait_log(log, "diagnostic_worker_start", lambda row: row["doc_version"] == 1)
            client.notify("$/cancelRequest", id=cancel_request)
            expect_error(client.wait_response(cancel_request), -32800)
            after, _ = self._wait_log(log, "diagnostic_worker_done", lambda row: row["job"] == first["job"])

            stale_request = client.start_request("textDocument/diagnostic", **params)
            _, second = self._wait_log(log, "diagnostic_worker_start", lambda row: row["doc_version"] == 1,
                                       after=after)
            client.change_document(path, 2, clean)
            expect_error(client.wait_response(stale_request), -32801)
            full_report(pull(client, path), [])
            current_request = client.next_id
            self._wait_log(log, "diagnostic_worker_done", lambda row: row["job"] == second["job"])

        rows = self._read_log(log)
        starts = self._check_worker_rows(rows)
        self._check_pull_delivery(rows, starts, current_request, version=2, stage="stage2", context="full",
                                  delivery="delivered", kind="full", codes=[])
        done = {row["job"]: row for row in self._events(rows, "diagnostic_worker_done")}
        for worker, request, status, outcome in ((first, cancel_request, "discarded", "cancelled_-32800"),
                                                  (second, stale_request, "superseded", "content_modified_-32801")):
            job = worker["job"]
            self.assertEqual(worker["lane"], "large")
            self.assertTrue(done[job]["cancelled"])
            self.assertGreaterEqual(done[job]["cancel_requested_ms"], worker["worker_started_ms"])
            matching = [row for row in self._events(rows, "diagnostic_delivery")
                        if row["job"] == job and row["delivery"] == status]
            self.assertTrue(matching, f"missing {status} for job {job}")
            for row in matching:
                self._check_job_link(row, worker)
            reply, = [row for row in self._events(rows, "rpc_done") if row["req_id"] == request]
            self.assertEqual(reply["outcome"], outcome)
            self.assertEqual(reply["doc_version"], 1)
            self.assertEqual(reply["job"], job)

    def test_go_list_start_flush_and_cancel_waiting_request(self):
        gate = PausedGoList(self.root / "gate", _TIMEOUT)
        log = self.root / "diagnostics" / "paused.jsonl"
        origin = self.fixture.reference
        with self._session(environment={**gate.environment, _LOG_FILE: str(log)}) as client:
            check_references(client, origin, (origin,))
            with gate.pause():
                new_file = write(self.fixture.glog.with_name("extra.go"), "package glog\nfunc Additional() {}\n")
                notify_changes(client, (new_file, 1))
                client.send(id=_ID_CANARY, method="textDocument/references", params=reference_params(origin))
                gate.wait_for_snapshot()
                # A start event must be readable while go list is still blocked inside the relay.
                deadline = time.monotonic() + 5
                while True:
                    rows = self._read_log(log, complete=False) if log.exists() else []
                    dirty_index = next((i for i, row in enumerate(rows) if row["event"] == "workspace_dirty"), len(rows))
                    if any(row["phase"] == "cmd_start" and row["command"] == "go list"
                           for row in self._events(rows[dirty_index + 1:], "workspace_load_cmd")):
                        break
                    self.assertLess(time.monotonic(), deadline, "paused go list start was not flushed")
                    time.sleep(0.01)

                client.notify("$/cancelRequest", id=_ID_CANARY)
                expect_error(client.wait_response(_ID_CANARY), -32800)

            check_references(client, origin, (origin,))

        rows = self._read_log(log)
        cancelled, = [row for row in self._events(rows, "rpc_done") if row["outcome"] == "cancelled_-32800"]
        req_id = cancelled["req_id"]
        self.assertRegex(req_id, r"^h:[0-9a-f]{16}$")
        self.assertTrue(cancelled["waited_workspace"])
        self.assertEqual(cancelled["wait_reason"], "dirty_graph")
        self.assertTrue(any(row["req_id"] == req_id for row in self._events(rows, "rpc_cancel")))
        trace, = [row for row in self._events(rows, "references_trace") if row["req_id"] == req_id]
        self.assertTrue(trace["cancelled"])
        dirty, = self._events(rows, "workspace_dirty")
        self.assertEqual(dirty["trigger"], "watched_files")
        self.assertEqual(dirty["reason"], "created_or_deleted")
        self.assertEqual(dirty["changed_file_count"], 1)
        self.assertEqual(dirty["new_generation"], dirty["old_generation"] + 1)
        self.assertRegex(dirty["document"], _HASH)
        methods = {row["method"] for row in self._events(rows, "notification")}
        self.assertTrue({"workspace/didChangeWatchedFiles", "$/cancelRequest"}.issubset(methods))


if __name__ == "__main__":
    if len(sys.argv) > 1 and not sys.argv[1].startswith("-"):
        _BINARY = sys.argv.pop(1)
    unittest.main()

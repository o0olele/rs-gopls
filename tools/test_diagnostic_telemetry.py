"""Exercise diagnostic JSONL using a built server and a synthetic GOPATH workspace."""

import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest

from lsp_harness import PausedGoList, executable, expect_error, notify_changes, session, write
from workspace_checks._assertions import check_references, expect_location, reference_params
from workspace_checks.gopath_fixture import dual_gopath, isolated_go_environment


_BINARY = os.environ.get("RS_GOPLS_TEST_TELEMETRY_BINARY")
_LOG_FILE = "RS_GOPLS_LOG_FILE"
_REDACT = "RS_GOPLS_LOG_REDACT"
_SOURCE_CANARY = "SOURCE_CANARY_58cb5c7320"
_ID_CANARY = "RPC_ID_CANARY_a918ba0754"
_TEMP_PREFIX = "rs-gopls-telemetry-workspace-"
_TIMEOUT = 30


def _invalid_constant(value):
    raise ValueError(f"invalid JSON constant {value}")


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
        last_timestamp = 0
        for row in rows:
            self.assertIsInstance(row, dict)
            self.assertIsInstance(row["event"], str)
            self.assertGreaterEqual(row["ts_ms"], last_timestamp)
            last_timestamp = row["ts_ms"]
        return rows

    def _check_session(self, rows, pid, source):
        start, = self._events(rows, "session_start")
        config, = self._events(rows, "session_config")
        self.assertEqual(start["pid"], pid)
        self.assertTrue(start["version"])
        self.assertGreater(start["started_at_unix_ms"], 0)
        self.assertEqual(start["source"], source)
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

    def test_process_environment_and_relative_paths(self):
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
        self.assertEqual(document, self.fixture.caller.relative_to(self.fixture.root).as_posix())
        unknown, = [row for row in self._events(rows, "rpc_done") if row["req_id"] == unsupported]
        self.assertEqual((unknown["method"], unknown["outcome"]), ("unknown", "error_-32601"))
        methods = {row["method"] for row in self._events(rows, "notification")}
        self.assertTrue({
            "textDocument/didOpen", "textDocument/didChange", "textDocument/didSave",
            "textDocument/didClose", "workspace/didChangeConfiguration",
        }.issubset(methods))

    def test_initialization_options_and_stable_redaction(self):
        documents = []
        for run in range(2):
            log = self.root / "diagnostics" / f"redacted-{run}.jsonl"
            with self._session(options={_LOG_FILE: str(log), _REDACT: "1"}) as client:
                requests = self._navigate(client)

            self.assertEqual(client.process.returncode, 0)
            rows = self._read_log(log)
            self._check_session(rows, client.process.pid, "initialization_options")
            document = self._check_requests(rows, requests)
            self.assertRegex(document, r"^h:[0-9a-f]{16}$")
            self.assertFalse(self.fixture.caller.name in log.read_text(encoding="utf-8"),
                             "redacted log includes a document basename")
            documents.append(document)
        self.assertEqual(*documents)

    def test_disabled_does_not_create_log(self):
        late_log = self.root / "late.jsonl"
        fixture_files = {path for path in self.root.rglob("*") if path.is_file()}
        with self._session() as client:
            client.change_configuration({"env": {_LOG_FILE: str(late_log)}})
            self._navigate(client)

        self.assertEqual(client.process.returncode, 0)
        self.assertFalse(late_log.exists())
        self.assertEqual(fixture_files, {path for path in self.root.rglob("*") if path.is_file()})

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
        methods = {row["method"] for row in self._events(rows, "notification")}
        self.assertTrue({"workspace/didChangeWatchedFiles", "$/cancelRequest"}.issubset(methods))


if __name__ == "__main__":
    if len(sys.argv) > 1 and not sys.argv[1].startswith("-"):
        _BINARY = sys.argv.pop(1)
    unittest.main()

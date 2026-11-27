"""Exercise the shared harness with an independent stdio peer."""

from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock

from lsp_harness import (
    GO_WATCH_PATTERNS, LspClient, PausedGoList, WATCHED_FILES_METHOD, acknowledge_file_watches,
    await_watcher_registration, collect_responses, expect_error, next_message, session, wait_for_response,
)
from workspace_checks.symbols import check_no_extra_symbol_responses


DIAGNOSTICS = {"method": "textDocument/publishDiagnostics", "params": {"uri": "file:///main.go", "diagnostics": []}}
LOG = {"method": "window/logMessage", "params": {"type": 3, "message": "UTF-8 🙂"}}


def read_message(stream):
    length_header = stream.readline()
    if not length_header:
        return None
    prefix = b"Content-Length: "
    assert length_header.startswith(prefix) and stream.readline() == b"\r\n", length_header
    length = int(length_header[len(prefix):])
    body = stream.read(length)
    assert len(body) == length, body
    return json.loads(body.decode("utf-8"))


def send_messages(*messages):
    output = sys.stdout.buffer
    for message in messages:
        payload = json.dumps({"jsonrpc": "2.0", **message}, ensure_ascii=False).encode("utf-8")
        output.write(b"Content-Length: %d\r\n\r\n" % len(payload))
        output.write(payload)
    output.flush()


def run_peer():
    transcript = []
    paired = []
    server_replies = []
    reply_count = None
    while (message := read_message(sys.stdin.buffer)) is not None:
        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params", {})
        if method == "transcript":
            send_messages({"id": request_id, "result": transcript})
            continue
        transcript.append(message)
        if method is None:
            server_replies.append(message)
            if len(server_replies) == reply_count:
                send_messages({"method": "test/serverReplies", "params": server_replies})
        elif method == "echo":
            send_messages({"id": request_id, "result": params})
        elif method == "initialize":
            send_messages({"id": request_id, "result": {"capabilities": {"definitionProvider": True}}})
        elif method == "pair":
            paired.append({"id": request_id, "result": params})
            if len(paired) == 2:
                send_messages(DIAGNOSTICS, paired[1], LOG, paired[0])
                paired.clear()
        elif method == "error":
            send_messages({"id": request_id, "error": {"code": -32602, "message": "bad parameters"}})
        elif method == "script":
            reply_count = params.get("replyCount")
            send_messages(*params.get("messages", ()))
            if "badFrame" in params:
                sys.stdout.buffer.write(params["badFrame"].encode("ascii"))
                sys.stdout.buffer.flush()
            if params.get("stop"):
                return
        elif method == "shutdown":
            send_messages({"id": request_id, "result": None})
        elif method == "exit":
            return


def watch_registration(request_id, watchers, method=WATCHED_FILES_METHOD):
    return {
        "id": request_id,
        "method": "client/registerCapability",
        "params": {
            "registrations": [{"id": "watch", "method": method, "registerOptions": {"watchers": watchers}}],
        },
    }


class LspHarnessTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="rs-gopls-harness-test-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.client = LspClient([sys.executable, "-B", str(Path(__file__).resolve()), "--stdio-peer"], self.root, 5)
        self.addCleanup(self.client.close)

    def test_wait_by_id_preserves_reversed_replies_and_notifications(self):
        client = self.client
        first = client.start_request("pair", name="first")
        second = str(first)
        client.send(id=second, method="pair", params={"name": "second"})
        self.assertEqual(client.wait_response(first), {"jsonrpc": "2.0", "id": first, "result": {"name": "first"}})
        self.assertEqual(client.wait_response(second), {"jsonrpc": "2.0", "id": second, "result": {"name": "second"}})
        self.assertEqual(client.get_notification(), {"jsonrpc": "2.0", **DIAGNOSTICS})
        self.assertEqual(client.get_notification(), {"jsonrpc": "2.0", **LOG})
        with self.assertRaises(TimeoutError):
            client.get_notification(timeout=0)

    def test_batch_conflicts_are_atomic_and_auto_ids_skip_explicit_ids(self):
        client = self.client
        client.send(id=1, method="hold")
        with self.assertRaisesRegex(RuntimeError, "already in flight"):
            client.send_batch({"id": 2, "method": "echo"}, {"id": 1, "method": "echo"})
        with self.assertRaisesRegex(RuntimeError, "already in flight"):
            client.send_batch({"id": "same", "method": "echo"}, {"id": "same", "method": "echo"})
        self.assertEqual(client.start_request("echo", text="🙂"), 2)
        self.assertEqual(client.wait_response(2)["result"], {"text": "🙂"})
        with self.assertRaisesRegex(RuntimeError, "already been used"):
            client.send(id=2, method="echo")
        transcript = client.request("transcript")
        self.assertEqual([(row["id"], row["method"]) for row in transcript], [(1, "hold"), (2, "echo")])

    def test_concurrent_synchronous_requests_keep_frames_intact(self):
        with ThreadPoolExecutor(max_workers=4) as pool:
            tasks = [
                (index, pool.submit(self.client.request, "echo", index=index, text="𐐀" * 4096))
                for index in range(8)
            ]
            for index, task in tasks:
                self.assertEqual(task.result(timeout=5), {"index": index, "text": "𐐀" * 4096})

    def test_idle_server_requests_are_answered_once_and_recorded(self):
        client = self.client
        requests = [
            {"id": 7, "method": "workspace/workspaceFolders"},
            {"id": "7", "method": "workspace/configuration", "params": {"items": [{}, {}]}},
            {"id": "progress", "method": "window/workDoneProgress/create", "params": {"token": "build"}},
            watch_registration("watch", [{"globPattern": pattern} for pattern in sorted(GO_WATCH_PATTERNS)]),
            {"id": "unregister", "method": "client/unregisterCapability", "params": {}},
            {"id": "refresh", "method": "workspace/diagnostic/refresh"},
            {"id": "unsupported", "method": "test/unknown"},
        ]
        client.notify("script", messages=requests, replyCount=len(requests))
        # The peer reports replies without any client request following the script notification.
        notice = client.get_notification()
        self.assertEqual(notice["method"], "test/serverReplies")
        replies = notice["params"]
        self.assertEqual([row["id"] for row in replies], [row["id"] for row in requests])
        self.assertEqual([row["result"] for row in replies[:-1]], [client.folders, [None, None], None, None, None, None])
        expect_error(replies[-1], -32601)

        self.assertEqual(client.wait_for_registration(WATCHED_FILES_METHOD), {"jsonrpc": "2.0", **requests[3]})
        acknowledge_file_watches(client)
        deadline = time.monotonic() + 5
        for expected in requests:
            received = next_message(client, deadline)
            self.assertEqual(received, {"jsonrpc": "2.0", **expected})
            client.reply(received)
        transcript = client.request("transcript")
        self.assertEqual([row for row in transcript if "method" not in row], replies)

    def test_bidirectional_requests_with_the_same_id(self):
        client = self.client
        request_id = client.start_request("hold")
        client.notify("script", messages=[
            {"id": request_id, "method": "workspace/workspaceFolders"},
            {"id": request_id, "result": "client response"},
        ], replyCount=1)
        self.assertEqual(client.wait_response(request_id)["result"], "client response")
        self.assertEqual(client.get_notification()["params"][0]["result"], client.folders)

    def test_watcher_handshake_accepts_relative_patterns_for_benchmark(self):
        client = self.client
        client.notify("script", messages=[
            watch_registration("unrelated", [], "other/method"),
            watch_registration("empty", []),
            watch_registration("relative", [{"globPattern": {"baseUri": self.root.as_uri(), "pattern": "**/*"}}]),
        ])
        self.assertEqual(await_watcher_registration(client)["id"], "relative")
        with self.assertRaisesRegex(RuntimeError, "missing Go file watcher registration"):
            acknowledge_file_watches(client)

    def test_initialize_and_shutdown_keep_existing_protocol(self):
        client = self.client
        self.assertEqual(client.initialize(watch_files=True, hierarchical_document_symbols=False), {"definitionProvider": True})
        initialize, initialized = client.request("transcript")
        self.assertEqual(initialize["method"], "initialize")
        self.assertEqual(initialize["params"]["rootUri"], self.root.as_uri())
        self.assertEqual(initialize["params"]["initializationOptions"], {"diagnosticsTrigger": "Save"})
        self.assertEqual(initialize["params"]["capabilities"], {
            "general": {"positionEncodings": ["utf-16"]},
            "workspace": {"workspaceFolders": True, "didChangeWatchedFiles": {"dynamicRegistration": True}},
            "textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": False}},
        })
        self.assertEqual(initialized["method"], "initialized")
        client.shutdown()
        self.assertEqual(client.process.returncode, 0)

    def test_session_forwards_initialization_options_and_explicit_environment(self):
        options = {"rs-gopls": {"env": {"GOPATH": "gopath", "GOWORK": ""}}}
        environment = {"GOENV": "off"}
        with mock.patch("lsp_harness.LspClient", return_value=self.client) as client_type:
            with session("server", self.root, 5, initialization_options=options, environment=environment) as client:
                initialize, initialized = client.request("transcript")
                self.assertEqual(initialize["params"]["initializationOptions"], options)
                self.assertEqual(initialized["method"], "initialized")
        client_type.assert_called_once_with(["server"], self.root, 5, environment=environment)

    def test_initialize_accepts_explicit_null_options(self):
        self.client.initialize(initialization_options=None)
        initialize, _ = self.client.request("transcript")
        self.assertIsNone(initialize["params"]["initializationOptions"])

    def test_change_configuration_sends_settings_as_a_notification(self):
        settings = [{"env": {"GOPATH": "gopath"}}, {}, None, {"env": {}}]
        for value in settings:
            self.client.change_configuration(value)
        self.assertEqual(self.client.request("transcript"), [
            {"jsonrpc": "2.0", "method": "workspace/didChangeConfiguration", "params": {"settings": value}}
            for value in settings
        ])

    def test_protocol_errors_stay_raw_for_wait_response(self):
        client = self.client
        request_id = client.start_request("error")
        expect_error(client.wait_response(request_id), -32602)
        with self.assertRaisesRegex(RuntimeError, "error failed: .*bad parameters"):
            client.request("error")

    def test_rename_capabilities_can_be_negotiated_independently(self):
        self.client.initialize(document_changes=True, prepare_rename=False)
        initialize, _ = self.client.request("transcript")
        capabilities = initialize["params"]["capabilities"]
        self.assertEqual(capabilities["workspace"]["workspaceEdit"], {"documentChanges": True})
        self.assertEqual(capabilities["textDocument"]["rename"], {"prepareSupport": False})

    def test_timeout_uses_snapshot_and_late_replies_do_not_match_retries(self):
        client = self.client
        client.timeout = 0.02
        start_request = client.start_request

        def change_timeout(method, **params):
            request_id = start_request(method, **params)
            client.timeout = 5
            return request_id

        with mock.patch.object(client, "start_request", side_effect=change_timeout):
            with self.assertRaisesRegex(RuntimeError, "hold timed out after 0.02s"):
                client.request("hold")

        with self.assertRaisesRegex(RuntimeError, "already been used"):
            client.send(id=1, method="hold")
        retry = client.start_request("hold")
        self.assertEqual(retry, 2)
        late = {"id": 1, "result": "late"}
        unknown = {"id": 999, "result": "unknown"}
        duplicate = {"id": retry, "result": "duplicate"}
        client.notify("script", messages=[late, unknown, {"id": retry, "result": "retry"}, duplicate])
        self.assertEqual(client.wait_response(retry)["result"], "retry")
        self.assertEqual([client.get_next_response() for _ in range(3)], [
            {"jsonrpc": "2.0", **row} for row in (late, unknown, duplicate)
        ])

    def test_response_order_checks_still_detect_early_and_duplicate_replies(self):
        client = self.client
        client.send_batch({"id": 1, "method": "pair"}, {"id": "1", "method": "pair"})
        with self.assertRaisesRegex(RuntimeError, "expected response 1 first, received '1'"):
            wait_for_response(client, 1)
        self.assertEqual(wait_for_response(client, 1)["id"], 1)
        client.send_batch({"id": 2, "method": "pair"}, {"id": "2", "method": "pair"})
        self.assertEqual(list(collect_responses(client, 2, "2")), ["2", 2])
        check_no_extra_symbol_responses(client)
        client.notify("script", messages=[{"id": 2, "result": None}])
        with self.assertRaisesRegex(RuntimeError, "unexpected response after symbol requests completed"):
            check_no_extra_symbol_responses(client)
        self.assertEqual(client.get_notification()["method"], DIAGNOSTICS["method"])

    def test_eof_wakes_waiters_and_preserves_received_frames(self):
        client = self.client
        ready, first, second = (client.start_request("hold") for _ in range(3))
        with ThreadPoolExecutor(max_workers=3) as pool:
            waits = [
                pool.submit(client.wait_response, first),
                pool.submit(client.wait_response, second),
                pool.submit(client.wait_for_registration, WATCHED_FILES_METHOD),
            ]
            client.notify("script", messages=[{"id": ready, "result": "kept"}, LOG], stop=True)
            for wait in waits:
                with self.assertRaisesRegex(RuntimeError, "server closed stdout"):
                    wait.result(timeout=2)
        self.assertEqual(client.wait_response(ready)["result"], "kept")
        self.assertEqual(client.get_notification()["method"], LOG["method"])
        with self.assertRaisesRegex(RuntimeError, "server closed stdout"):
            client.get_notification()

    def test_bad_frame_wakes_waiters_and_preserves_received_frames(self):
        client = self.client
        ready, pending = (client.start_request("hold") for _ in range(2))
        client.notify(
            "script",
            messages=[{"id": ready, "result": "kept"}, DIAGNOSTICS],
            badFrame="Content-Length: -1\r\n\r\n",
        )
        with self.assertRaisesRegex(RuntimeError, "invalid LSP Content-Length header"):
            client.wait_response(pending)
        self.assertEqual(client.wait_response(ready)["result"], "kept")
        self.assertEqual(client.get_notification()["method"], DIAGNOSTICS["method"])

    def test_close_wakes_waiters_and_preserves_received_frames(self):
        client = self.client
        ready, pending = (client.start_request("hold") for _ in range(2))
        client.notify("script", messages=[{"id": ready, "result": "kept"}, LOG])
        client.request("echo")
        with ThreadPoolExecutor(max_workers=2) as pool:
            wait = pool.submit(client.wait_response, pending)
            registration = pool.submit(client.wait_for_registration, WATCHED_FILES_METHOD)
            client.close()
            for task in (wait, registration):
                with self.assertRaisesRegex(RuntimeError, "LSP client closed"):
                    task.result(timeout=2)
        self.assertEqual(client.wait_response(ready)["result"], "kept")
        self.assertEqual(client.get_notification()["method"], LOG["method"])
        with self.assertRaisesRegex(RuntimeError, "LSP client closed"):
            client.get_notification()

    def test_write_failure_wakes_response_and_notification_waiters(self):
        client = self.client
        pending = client.start_request("hold")
        with ThreadPoolExecutor(max_workers=2) as pool:
            response = pool.submit(client.wait_response, pending)
            notification = pool.submit(client.get_notification)
            with mock.patch.object(client.process, "stdin") as pipe:
                pipe.write.side_effect = BrokenPipeError("broken test pipe")
                with self.assertRaisesRegex(RuntimeError, "broken test pipe"):
                    client.notify("ignored")
            for task in (response, notification):
                with self.assertRaisesRegex(RuntimeError, "broken test pipe"):
                    task.result(timeout=2)


class PausedGoListTests(unittest.TestCase):
    def test_gate_releases_if_case_fails_before_sending_and_can_be_rearmed(self):
        with tempfile.TemporaryDirectory() as directory:
            control = Path(directory).resolve()
            with mock.patch("lsp_harness.go_list_relay", return_value={}):
                gate = PausedGoList(control, 0.02)
            with self.assertRaisesRegex(RuntimeError, "failed to send request"):
                with gate.pause():
                    raise RuntimeError("failed to send request")
            self.assertTrue((control / "release").is_file())
            with self.assertRaisesRegex(RuntimeError, "did not capture a pending snapshot"):
                with gate.pause():
                    self.assertFalse((control / "release").exists())
                    gate.wait_for_snapshot()
            self.assertTrue((control / "release").is_file())


if __name__ == "__main__":
    if sys.argv[1:] == ["--stdio-peer"]:
        run_peer()
    else:
        unittest.main()

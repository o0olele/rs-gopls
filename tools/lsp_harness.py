"""Shared stdio LSP and Go workspace fixtures."""

import argparse
from contextlib import contextmanager, suppress
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit
from urllib.request import url2pathname


WORKSPACE_ENVIRONMENT = {"GO111MODULE": "on", "GOWORK": "off", "GOPROXY": "off"}
GO_WATCH_PATTERNS = {"**/*.go", "**/go.mod", "**/go.sum", "**/go.work", "**/go.work.sum"}
WATCHED_FILES_METHOD = "workspace/didChangeWatchedFiles"
_DEFAULT_INITIALIZATION_OPTIONS = object()


def position_at(text, offset):
    return {
        "line": text.count("\n", 0, offset),
        "character": len(text[text.rfind("\n", 0, offset) + 1 : offset].encode("utf-16-le")) // 2,
    }


def path_from_uri(uri):
    components = urlsplit(uri)
    if components.scheme != "file":
        raise RuntimeError(f"expected a file URI, received {uri!r}")
    path = ("//" + components.netloc if components.netloc and components.netloc != "localhost" else "") + components.path
    return Path(url2pathname(path)).resolve()


def executable(value):
    resolved = shutil.which(value)
    if not resolved:
        raise argparse.ArgumentTypeError(f"executable not found: {value}")
    return str(Path(resolved).resolve())


@dataclass(frozen=True)
class Site:
    path: Path
    text: str
    start: int
    end: int

    def cursor(self):
        return position_at(self.text, min(self.start + 1, self.end - 1))

    def selection(self):
        return {
            "start": position_at(self.text, self.start),
            "end": position_at(self.text, self.end),
        }


def site(path, anchor, text=None):
    if text is None:
        text = path.read_text(encoding="utf-8")
    before, selected = anchor.split("[[", 1)
    token, after = selected.split("]]", 1)
    try:
        start = text.index(before + token + after) + len(before)
    except ValueError as exc:
        raise RuntimeError(f"could not find {anchor!r} in {path}") from exc
    return Site(path, text, start, start + len(token))


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")
    return path


def _frames(messages):
    frames = bytearray()
    for message in messages:
        body = json.dumps({"jsonrpc": "2.0", **message}, ensure_ascii=False).encode("utf-8")
        frames.extend(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
        frames.extend(body)
    return frames


def _read_frame(output):
    size = None
    while (line := output.readline()).strip():
        key, value = line.split(b":", 1)
        if key.strip().lower() == b"content-length":
            if size is not None or not value.strip().isdigit():
                raise ValueError(f"invalid LSP Content-Length header: {line!r}")
            size = int(value)
    if not line:
        raise EOFError("server closed stdout")
    if size is None:
        raise ValueError("missing LSP Content-Length header")
    body = output.read(size)
    if len(body) != size:
        raise EOFError("server truncated an LSP message")
    message = json.loads(body.decode("utf-8"))
    if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
        raise ValueError(f"invalid JSON-RPC message: {message!r}")
    if "method" in message:
        if not isinstance(message["method"], str):
            raise ValueError(f"invalid JSON-RPC method: {message!r}")
    elif "id" not in message or ("result" in message) == ("error" in message):
        raise ValueError(f"invalid JSON-RPC response: {message!r}")
    return message


@dataclass
class _PendingRequest:
    method: str
    response: int | None = None


class _AnsweredRequest(dict):
    """A request already answered by the reader before it was made available."""


class LspClient:
    def __init__(self, command, root, timeout, *, environment=None):
        self.timeout = timeout
        self.next_id = 0
        self.folders = [{"uri": root.as_uri(), "name": root.name}]
        self._condition = threading.Condition()
        self._write_lock = threading.Lock()
        self._close_lock = threading.Lock()
        self._failure = None
        self._closed = False
        # Reusing an ID would let a late reply for an earlier request satisfy its retry.
        self._used_ids = set()
        self._pending = {}
        # Arrival order serves the strict assertions and the legacy mixed-message helper.
        self._sequence = 0
        self._responses = {}
        self._notifications = {}
        self._answered_requests = {}
        self._registrations = []
        self.stderr = tempfile.TemporaryFile(mode="w+b")
        try:
            self.process = subprocess.Popen(
                command,
                cwd=root,
                env=environment if environment is not None else {**os.environ, "GOWORK": "off", "GOPROXY": "off"},
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self.stderr,
            )
        except OSError:
            self.stderr.close()
            raise
        self.reader = threading.Thread(target=self.read_messages, daemon=True)
        self.reader.start()

    def read_messages(self):
        try:
            with self.process.stdout as output:
                while True:
                    self._dispatch(_read_frame(output))
        except Exception as exc:
            self._stop(exc)

    def _dispatch(self, message):
        if "method" in message and "id" in message:
            self.reply(message)
            message = _AnsweredRequest(message)
        with self._condition:
            sequence = self._sequence
            self._sequence += 1
            if isinstance(message, _AnsweredRequest):
                self._answered_requests[sequence] = message
                if message["method"] == "client/registerCapability":
                    self._registrations.append(message)
            elif "method" in message:
                self._notifications[sequence] = message
            else:
                self._responses[sequence] = message
                request_id = message["id"]
                pending = self._pending.get(request_id) if type(request_id) in (int, str) else None
                if pending is not None and pending.response is None:
                    pending.response = sequence
            self._condition.notify_all()

    def _stop(self, cause):
        with self._condition:
            if self._failure is None:
                self._failure = cause
            self._condition.notify_all()

    def _check_transport(self):
        if self._failure is not None:
            raise RuntimeError(f"LSP transport: {self._failure}") from self._failure

    def _wait(self, deadline, description):
        self._check_transport()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"timed out waiting for {description}")
        self._condition.wait(min(remaining, threading.TIMEOUT_MAX))

    def _write(self, frames):
        with self._write_lock:
            with self._condition:
                self._check_transport()
            try:
                self.process.stdin.write(frames)
                self.process.stdin.flush()
            except (OSError, ValueError) as exc:
                self._stop(exc)
                raise RuntimeError(f"LSP transport: {exc}") from exc

    def send(self, **message):
        self.send_batch(message)

    def send_batch(self, *messages):
        frames = _frames(messages)
        with self._condition:
            self._check_transport()
            requests = {}
            for message in messages:
                if "method" not in message or "id" not in message:
                    continue
                request_id = message["id"]
                if type(request_id) not in (int, str):
                    raise ValueError(f"request ID must be an integer or string, got {request_id!r}")
                if request_id in requests or request_id in self._pending:
                    raise RuntimeError(f"LSP request ID {request_id!r} is already in flight")
                if request_id in self._used_ids:
                    raise RuntimeError(f"LSP request ID {request_id!r} has already been used")
                requests[request_id] = _PendingRequest(message["method"])
            self._pending.update(requests)
            self._used_ids.update(requests)
        self._write(frames)

    def start_request(self, method, **params):
        with self._condition:
            self._check_transport()
            request_id = self.next_id + 1
            while request_id in self._used_ids:
                request_id += 1
            frames = _frames(({"id": request_id, "method": method, "params": params},))
            self.next_id = request_id
            self._used_ids.add(request_id)
            self._pending[request_id] = _PendingRequest(method)
        self._write(frames)
        return request_id

    def _take_response(self, sequence):
        message = self._responses.pop(sequence)
        request_id = message["id"]
        pending = self._pending.get(request_id) if type(request_id) in (int, str) else None
        if pending is not None and pending.response == sequence:
            del self._pending[request_id]
            self._condition.notify_all()
        return message

    def wait_response(self, request_id, timeout=None):
        """Consume the first response to this request, independently of other IDs."""
        if type(request_id) not in (int, str):
            raise ValueError(f"request ID must be an integer or string, got {request_id!r}")
        timeout = self.timeout if timeout is None else timeout
        deadline = time.monotonic() + timeout
        with self._condition:
            while (pending := self._pending.get(request_id)) is not None:
                if pending.response is not None:
                    return self._take_response(pending.response)
                try:
                    self._wait(deadline, f"response {request_id!r}")
                except TimeoutError as exc:
                    # Retire the ID even if its response is still on the wire.
                    del self._pending[request_id]
                    self._condition.notify_all()
                    raise RuntimeError(f"{pending.method} timed out after {timeout:g}s") from exc
            raise RuntimeError(f"no pending LSP request with ID {request_id!r}")

    def get_next_response(self, timeout=None):
        """Consume the next response in arrival order, including unmatched responses."""
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        with self._condition:
            while not self._responses:
                self._wait(deadline, "an LSP response")
            return self._take_response(next(iter(self._responses)))

    def get_notification(self, timeout=None):
        """Consume the next server notification in arrival order."""
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        with self._condition:
            while not self._notifications:
                self._wait(deadline, "an LSP notification")
            return self._notifications.pop(next(iter(self._notifications)))

    def _get_message(self, deadline):
        with self._condition:
            queues = (self._responses, self._notifications, self._answered_requests)
            while not any(queues):
                self._wait(deadline, "an LSP message")
            sequence, queue = min((next(iter(queue)), queue) for queue in queues if queue)
            if queue is self._responses:
                return self._take_response(sequence)
            return queue.pop(sequence)

    def wait_for_registration(self, method, timeout=None, *, predicate=None):
        """Return an answered request containing a registration for the given method."""
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        with self._condition:
            inspected = 0
            while True:
                for message in self._registrations[inspected:]:
                    for registration in message.get("params", {}).get("registrations", ()):
                        if registration.get("method") == method and (predicate is None or predicate(registration)):
                            return message
                inspected = len(self._registrations)
                self._wait(deadline, f"a registration for {method}")

    def notify(self, method, **params):
        self.send(method=method, params=params)

    def change_configuration(self, settings):
        self.notify("workspace/didChangeConfiguration", settings=settings)

    def reply(self, message):
        if isinstance(message, _AnsweredRequest):
            return
        request_id = message["id"]
        method = message["method"]
        if method == "workspace/workspaceFolders":
            self.send(id=request_id, result=self.folders)
        elif method == "workspace/configuration":
            self.send(id=request_id, result=[None] * len(message.get("params", {}).get("items", ())))
        elif method in {
            "window/workDoneProgress/create", "client/registerCapability", "client/unregisterCapability",
            "workspace/diagnostic/refresh",
        }:
            self.send(id=request_id, result=None)
        else:
            self.send(id=request_id, error={"code": -32601, "message": f"Unsupported client method: {method}"})

    def request(self, method, **params):
        timeout = self.timeout
        response = self.wait_response(self.start_request(method, **params), timeout=timeout)
        if "error" in response:
            raise RuntimeError(f"{method} failed: {response['error']}")
        return response["result"]

    def initialize(self, *, watch_files=False, hierarchical_document_symbols=None, document_changes=None,
                   prepare_rename=None, initialization_options=_DEFAULT_INITIALIZATION_OPTIONS):
        workspace = {"workspaceFolders": True}
        if watch_files:
            workspace["didChangeWatchedFiles"] = {"dynamicRegistration": True}
        if document_changes is not None:
            workspace["workspaceEdit"] = {"documentChanges": document_changes}
        capabilities = {
            "general": {"positionEncodings": ["utf-16"]},
            "workspace": workspace,
        }
        if hierarchical_document_symbols is not None:
            capabilities["textDocument"] = {
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": hierarchical_document_symbols},
            }
        if prepare_rename is not None:
            capabilities.setdefault("textDocument", {})["rename"] = {"prepareSupport": prepare_rename}
        if initialization_options is _DEFAULT_INITIALIZATION_OPTIONS:
            initialization_options = {"diagnosticsTrigger": "Save"}
        init = self.request(
            "initialize",
            processId=os.getpid(),
            rootUri=self.folders[0]["uri"],
            workspaceFolders=self.folders,
            initializationOptions=initialization_options,
            capabilities=capabilities,
        )
        capabilities = init["capabilities"]
        if capabilities.get("definitionProvider") in (None, False):
            raise RuntimeError("server did not advertise definitionProvider")
        if capabilities.get("positionEncoding", "utf-16").lower() != "utf-16":
            raise RuntimeError("server did not negotiate UTF-16 positions")
        self.notify("initialized")
        return capabilities

    def open_document(self, path, text):
        self.notify(
            "textDocument/didOpen",
            textDocument={"uri": path.as_uri(), "languageId": "go", "version": 1, "text": text},
        )

    def change_document(self, path, version, text):
        self.notify(
            "textDocument/didChange",
            textDocument={"uri": path.as_uri(), "version": version},
            contentChanges=[{"text": text}],
        )

    def change_document_range(self, path, version, start, end, text):
        self.notify(
            "textDocument/didChange",
            textDocument={"uri": path.as_uri(), "version": version},
            contentChanges=[{"range": {"start": start, "end": end}, "text": text}],
        )

    def close_document(self, path):
        self.notify("textDocument/didClose", textDocument={"uri": path.as_uri()})

    def shutdown(self):
        self.request("shutdown")
        self.notify("exit")
        self.process.wait(timeout=min(self.timeout, 5))

    def stderr_tail(self):
        size = self.stderr.seek(0, os.SEEK_END)
        self.stderr.seek(max(0, size - 4096))
        return self.stderr.read().decode("utf-8", errors="replace").strip()

    def close(self):
        with self._close_lock:
            if self._closed:
                return
            self._stop(RuntimeError("LSP client closed"))
            try:
                if self.process.poll() is None:
                    self.process.kill()
                    self.process.wait(timeout=5)
            finally:
                with self._write_lock, suppress(OSError):
                    self.process.stdin.close()
                self.reader.join(timeout=2)
                self.stderr.close()
                self._closed = True


def notify_changes(client, *events):
    client.notify(
        WATCHED_FILES_METHOD,
        changes=[{"uri": path.as_uri(), "type": event} for path, event in events],
    )


def await_watcher_registration(client):
    return client.wait_for_registration(
        WATCHED_FILES_METHOD,
        predicate=lambda registration: bool(registration.get("registerOptions", {}).get("watchers")),
    )


def acknowledge_file_watches(client):
    message = client.wait_for_registration(WATCHED_FILES_METHOD)
    patterns = {
        pattern
        for registration in message["params"]["registrations"]
        if registration["method"] == WATCHED_FILES_METHOD
        for watcher in registration.get("registerOptions", {}).get("watchers", ())
        if isinstance(pattern := watcher.get("globPattern"), str)
    }
    if missing := GO_WATCH_PATTERNS - patterns:
        raise RuntimeError(f"missing Go file watcher registration: {missing}")


def initialize_session(client, *, watch_files=False, hierarchical=None, require_go_watches=True,
                       document_changes=None, prepare_rename=None,
                       initialization_options=_DEFAULT_INITIALIZATION_OPTIONS):
    capabilities = client.initialize(
        watch_files=watch_files, hierarchical_document_symbols=hierarchical,
        document_changes=document_changes, prepare_rename=prepare_rename,
        initialization_options=initialization_options,
    )
    if watch_files:
        if require_go_watches:
            acknowledge_file_watches(client)
        else:
            await_watcher_registration(client)
    return capabilities


@contextmanager
def session(executable_path, root, timeout, *, watch_files=False, hierarchical=None,
            document_changes=None, prepare_rename=None,
            initialization_options=_DEFAULT_INITIALIZATION_OPTIONS, environment=None, **env):
    if environment is None:
        environment = {**os.environ, **WORKSPACE_ENVIRONMENT}
    client = LspClient([executable_path], root, timeout, environment={**environment, **env})
    try:
        initialize_session(
            client, watch_files=watch_files, hierarchical=hierarchical,
            document_changes=document_changes, prepare_rename=prepare_rename,
            initialization_options=initialization_options,
        )
        yield client
        client.shutdown()
    except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
        log = client.stderr_tail()
        raise RuntimeError(f"{root}: {exc}" + (f"\n{log}" if log else "")) from exc
    finally:
        client.close()


def next_message(client, deadline):
    """Read the next LSP message for legacy callers of the workspace checker."""
    try:
        return client._get_message(deadline)
    except TimeoutError as exc:
        raise RuntimeError(str(exc)) from exc


def response_detail(response):
    error = response.get("error")
    if error is not None:
        return f"error {error!r}"
    result = response.get("result")
    return f"success with {len(result)} items" if isinstance(result, list) else f"success with {result!r}"


def expect_error(response, code):
    if response.get("error", {}).get("code") != code:
        raise RuntimeError(
            f"expected LSP error {code} for {response.get('id')!r}, received {response_detail(response)}"
        )


def wait_for_response(client, request_id, timeout=None):
    """Assert that the next response is for the given request."""
    try:
        message = client.get_next_response(timeout)
    except (TimeoutError, RuntimeError) as exc:
        raise RuntimeError(f"waiting for {request_id}: {exc}") from exc
    if type(message["id"]) is not type(request_id) or message["id"] != request_id:
        raise RuntimeError(
            f"expected response {request_id!r} first, received {message['id']!r} "
            f"with {response_detail(message)}"
        )
    return message


def collect_responses(client, *request_ids):
    """Collect responses to the given requests in arrival order."""
    pending = {(type(request_id), request_id) for request_id in request_ids}
    responses = {}
    deadline = time.monotonic() + client.timeout
    while pending:
        try:
            message = client.get_next_response(max(0, deadline - time.monotonic()))
        except TimeoutError as exc:
            raise RuntimeError(str(exc)) from exc
        request_id = message["id"]
        key = (type(request_id), request_id)
        if not isinstance(request_id, (int, str)) or key not in pending:
            raise RuntimeError(f"unexpected or duplicate response {message!r}")
        pending.remove(key)
        responses[request_id] = message
    return responses


GO_LIST_RELAY = r"""package main

import (
    "bytes"
    "fmt"
    "os"
    "os/exec"
    "path/filepath"
    "time"
)

func main() {
    cmd := exec.Command(os.Getenv("RS_GOPLS_TEST_REAL_GO"), os.Args[1:]...)
    cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
    if len(os.Args) < 2 || os.Args[1] != "list" {
        os.Exit(run(cmd))
    }

    control := os.Getenv("RS_GOPLS_TEST_GO_GATE")
    log, err := os.OpenFile(filepath.Join(control, "calls"), os.O_WRONLY|os.O_CREATE|os.O_APPEND, 0600)
    if err != nil {
        fail(err)
    }
    if _, err := fmt.Fprintln(log, "list"); err != nil {
        fail(err)
    }
    log.Close()
    if os.Remove(filepath.Join(control, "fail")) == nil {
        fail(fmt.Errorf("injected go list failure"))
    }
    if os.Remove(filepath.Join(control, "arm")) != nil {
        os.Exit(run(cmd))
    }
    os.Exit(runPaused(cmd, control))
}

func runPaused(cmd *exec.Cmd, control string) int {
    // Capture metadata before signaling that caller and build config may change.
    defer os.WriteFile(filepath.Join(control, "done"), nil, 0600)
    var stdout, stderr bytes.Buffer
    cmd.Stdout, cmd.Stderr = &stdout, &stderr
    code := run(cmd)
    if err := os.WriteFile(filepath.Join(control, "ready"), nil, 0600); err != nil {
        fmt.Fprintln(os.Stderr, err)
        return 1
    }

    deadline := time.Now().Add(30 * time.Second)
    for {
        if _, err := os.Stat(filepath.Join(control, "release")); err == nil {
            break
        }
        if time.Now().After(deadline) {
            fmt.Fprintln(os.Stderr, "timed out waiting for the Go list test gate")
            return 1
        }
        time.Sleep(10 * time.Millisecond)
    }
    os.Stdout.Write(stdout.Bytes())
    os.Stderr.Write(stderr.Bytes())
    return code
}

func run(cmd *exec.Cmd) int {
    if err := cmd.Run(); err != nil {
        if exit, ok := err.(*exec.ExitError); ok {
            return exit.ExitCode()
        }
        fmt.Fprintln(cmd.Stderr, err)
        return 1
    }
    return 0
}

func fail(err error) {
    fmt.Fprintln(os.Stderr, err)
    os.Exit(1)
}
"""


def go_list_relay(root, timeout):
    real_go = shutil.which("go")
    if not real_go:
        raise RuntimeError("Go executable not found for the workspace loader relay")
    root = root.resolve()
    relay_file = write(root / "relay.go", GO_LIST_RELAY)
    relay_path = root / ("go.exe" if os.name == "nt" else "go")
    subprocess.run(
        [real_go, "build", "-o", str(relay_path), str(relay_file)],
        cwd=root, env={**os.environ, **WORKSPACE_ENVIRONMENT, "GO111MODULE": "off"},
        check=True, timeout=timeout,
    )
    return {
        "PATH": str(root) + os.pathsep + os.environ["PATH"],
        "RS_GOPLS_TEST_REAL_GO": str(Path(real_go).resolve()),
        "RS_GOPLS_TEST_GO_GATE": str(root),
    }


def _wait_for_file(path, timeout, failure):
    deadline = time.monotonic() + timeout
    while not path.is_file():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(failure)
        time.sleep(min(0.01, remaining))


def wait_for_snapshot(control, timeout):
    _wait_for_file(control / "ready", timeout, "go list relay did not capture a pending snapshot")


class PausedGoList:
    def __init__(self, control, timeout):
        self.control = control.resolve()
        self.timeout = timeout
        self.environment = go_list_relay(self.control, timeout)
        self._armed = False

    def arm(self):
        arm = self.control / "arm"
        if self._armed:
            if not (self.control / "release").is_file():
                raise RuntimeError("go list relay is already armed")
            try:
                arm.unlink()
            except FileNotFoundError:
                _wait_for_file(self.control / "done", self.timeout, "previous go list relay did not finish")
            for marker in ("ready", "release", "done"):
                (self.control / marker).unlink(missing_ok=True)
        write(arm, "")
        self._armed = True

    def wait_for_snapshot(self, timeout=None):
        wait_for_snapshot(self.control, self.timeout if timeout is None else timeout)

    def release(self):
        write(self.control / "release", "")

    @contextmanager
    def pause(self):
        self.arm()
        try:
            yield self
        finally:
            self.release()

    def fail_next(self):
        write(self.control / "fail", "")

    def calls(self):
        try:
            return (self.control / "calls").read_text(encoding="utf-8").splitlines()
        except FileNotFoundError:
            return []

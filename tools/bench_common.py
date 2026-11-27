"""Shared fixtures, LSP validation, and timing for the GoLand comparisons."""

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
import time

from check_goland_definitions import fixture_path
from compare_workspace_definitions import MEMORY_INTERVAL_SECONDS, WorkingSetSampler
from lsp_harness import LspClient, WORKSPACE_ENVIRONMENT, executable, path_from_uri


TOOLS = Path(__file__).resolve().parent
START = {"line": 0, "character": 0}
AFTER_PREFIX = {"line": 1, "character": 0}
EDIT_PREFIX = "// rs-gopls benchmark overlay\n"
WARMUPS = 5
ITERATIONS = 30
ERRORS = (KeyError, OSError, RuntimeError, TypeError, ValueError, subprocess.SubprocessError)


def percentile(values, p):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(len(ordered) * p) - 1))
    return ordered[index]


def summarize(values):
    return {
        "count": len(values),
        "p50_ms": percentile(values, 0.50),
        "p95_ms": percentile(values, 0.95),
        "min_ms": min(values, default=None),
        "max_ms": max(values, default=None),
    }


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as binary:
        for chunk in iter(lambda: binary.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest().upper()


def describe_binary(path):
    return {"name": Path(path).name, "sha256": sha256_file(path)}


def resolve_goland_exe(cli_arg):
    value = cli_arg or os.environ.get("GOLAND_EXE")
    if not value:
        raise ValueError("GoLand capture requires --goland or GOLAND_EXE")
    try:
        return Path(executable(os.fspath(value)))
    except argparse.ArgumentTypeError as exc:
        raise ValueError("GoLand executable was not found. Check --goland or GOLAND_EXE.") from exc


def go_output(*args, cwd=None, environment=None, timeout=60):
    return subprocess.check_output(
        ["go", *args], cwd=cwd, env=environment, encoding="utf-8", timeout=timeout,
    ).strip()


def describe_go_env(cwd=None, environment=None, timeout=60):
    options = {"cwd": cwd, "environment": environment, "timeout": timeout}
    return {
        "version": go_output("version", **options),
        **json.loads(go_output("env", "-json", "GOVERSION", "GOOS", "GOARCH", "CGO_ENABLED", **options)),
    }


@contextmanager
def sample_working_set(pid):
    sampler = WorkingSetSampler(pid)
    try:
        yield sampler
    finally:
        sampler.finish()


def working_set(sampler):
    return {
        "scope": "parent and descendants of the measured process",
        "sample_interval_ms": MEMORY_INTERVAL_SECONDS * 1000 if sys.platform == "win32" else None,
        **sampler.finish(),
    }


def normalize_path(path):
    if not isinstance(path, str):
        raise ValueError("expected a relative fixture path string")
    relative = PurePosixPath(path.replace("\\", "/"))
    if not relative.parts or relative.is_absolute() or any(
        part == ".." or ":" in part for part in relative.parts
    ):
        raise ValueError("expected a path relative to the fixture workspace")
    return relative.as_posix()


def relative_uri(uri, root):
    path = path_from_uri(uri)
    if not path.is_relative_to(root):
        raise ValueError("LSP returned a URI outside the fixture workspace")
    return path.relative_to(root).as_posix()


def normalize_capture(data):
    if not isinstance(data, dict) or data.get("schemaVersion") != 1 or data.get("root") != ".":
        raise ValueError("expected a version 1 GoLand capture with a workspace-relative root")

    def normalize(value):
        if isinstance(value, list):
            return [normalize(item) for item in value]
        if isinstance(value, dict):
            return {
                key: normalize_path(item) if key == "path" else normalize(item)
                for key, item in value.items()
            }
        return value

    return normalize(data)


def point(position):
    line, character = position["line"], position["character"]
    if any(type(value) is not int or value < 0 for value in (line, character)):
        raise ValueError(f"invalid UTF-16 position {position!r}")
    return line, character


def text_offset(text, position):
    line, character = point(position)
    start = 0
    lines = re.finditer(r"\r\n|\r|\n", text)
    for _ in range(line):
        ending = next(lines, None)
        if ending is None:
            raise ValueError(f"line {line} is outside the source")
        start = ending.end()
    ending = next(lines, None)
    end = ending.start() if ending else len(text)
    for offset in range(start, end):
        if character == 0:
            return offset
        character -= 2 if ord(text[offset]) > 0xFFFF else 1
        if character < 0:
            break
    if character == 0:
        return end
    raise ValueError(f"invalid UTF-16 position {position!r}")


def range_offsets(text, span):
    start, end = (text_offset(text, span[key]) for key in ("start", "end"))
    if start > end:
        raise ValueError(f"reversed range {span!r}")
    return start, end


def shift_range(span, lines):
    return {key: {**span[key], "line": span[key]["line"] + lines} for key in ("start", "end")}


def apply_text_edits(text, edits):
    """Apply non-overlapping edits against the original UTF-16 document."""
    spans = [(range_offsets(text, edit["range"]), edit["newText"]) for edit in edits]
    # The LSP uses array order for insertions at the same position.
    spans.sort(key=lambda item: item[0][0])
    cursor, parts = 0, []
    for (start, end), replacement in spans:
        if start < cursor:
            raise ValueError("overlapping text edits")
        parts.extend((text[cursor:start], replacement))
        cursor = end
    return "".join([*parts, text[cursor:]])


def line_ending(text):
    return "\r\n" if text.count("\r\n") > text.count("\n") / 2 else "\n"


def overlay_prefix(text):
    return EDIT_PREFIX.replace("\n", line_ending(text))


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def write_json(path, data):
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def capture_goland(launcher, track, workspace, spec, timeout):
    source = (TOOLS / f"capture_goland_{track}.groovy").read_text(encoding="utf-8")
    with TemporaryDirectory(prefix=f"rs-gopls-{track}-") as directory:
        stage = Path(directory).resolve()
        write_json(stage / f"{track}-queries.json", {**spec, "root": "."})
        imports, declaration, body = source.partition("def directory = ")
        root_reference = "directory.resolve(spec.root as String)"
        benchmark_flag = "System.getenv('RS_GOPLS_BENCHMARK') == '1'"
        if not declaration or root_reference not in body or benchmark_flag not in body:
            raise ValueError("GoLand capture script has incompatible staging declarations")
        project = f"Path.of(java.net.URI.create({json.dumps(workspace.as_uri())}))"
        body = body.split("\n", 1)[1].replace(root_reference, project).replace(benchmark_flag, "true")
        # A forwarded ideScript command inherits the running IDE's environment.
        script = stage / f"capture_goland_{track}.groovy"
        script.write_text(
            imports + f"def directory = Path.of(java.net.URI.create({json.dumps(stage.as_uri())}))\n"
            "Files.writeString(directory.resolve('pid'), Long.toString(ProcessHandle.current().pid()))\n"
            "try {\n" + body + "\n} catch (Throwable err) {\n"
            "    Files.writeString(directory.resolve('failure'), err.message ?: err.class.name)\n"
            "    throw err\n}\n", encoding="utf-8",
        )
        startup = None
        if sys.platform == "win32":
            startup = subprocess.STARTUPINFO()
            startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
            startup.wShowWindow = subprocess.SW_HIDE
        process = subprocess.Popen([str(launcher), "ideScript", str(script)], cwd=workspace, startupinfo=startup)
        output, failure, pid = (stage / name for name in (f"goland-{track}.json", "failure", "pid"))
        deadline = time.monotonic() + timeout
        sampler = None
        try:
            while True:
                if sampler is None and pid.is_file():
                    value = pid.read_text(encoding="utf-8").strip()
                    if value:
                        sampler = WorkingSetSampler(int(value))
                if output.is_file():
                    break
                if failure.is_file():
                    raise RuntimeError(f"GoLand capture: {failure.read_text(encoding='utf-8')}")
                status = process.poll()
                if status not in (0, None):
                    raise RuntimeError(f"GoLand ideScript exited with status {status}")
                if time.monotonic() >= deadline:
                    raise RuntimeError("GoLand capture timed out. Open the fixture workspace and configure its Go SDK.")
                time.sleep(0.05)
            data = normalize_capture(read_json(output))
            if sampler is not None:
                data["workingSet"] = working_set(sampler)
            return data
        finally:
            if sampler is not None:
                sampler.finish()


def index_cases(rows):
    cases = {}
    for row in rows:
        case_id = row["id"]
        if not isinstance(case_id, str) or not case_id or case_id in cases:
            raise ValueError(f"invalid or duplicate fixture case ID {case_id!r}")
        cases[case_id] = {**row, "path": normalize_path(row["path"])}
    return cases


def workspace_hash(root):
    paths = [path for path in root.rglob("*") if path.is_file() and
             (path.suffix == ".go" or path.name in {"go.mod", "go.sum", "go.work", "go.work.sum"})]
    digest = hashlib.sha256()
    for path in sorted(paths):
        relative = path.relative_to(root).as_posix()
        digest.update(f"{relative}\0{sha256_file(path)}\n".encode("utf-8"))
    return digest.hexdigest().upper()


@dataclass(frozen=True)
class Checked:
    result: object
    differences: tuple = ()


class Fixture:
    def __init__(self, args):
        self.root = args.workspace.resolve(strict=True)
        if not self.root.is_dir():
            raise ValueError("--workspace must be a directory")
        self.timeout = args.timeout
        self.environment = {**os.environ, **WORKSPACE_ENVIRONMENT}
        spec_name = f"{self.track}-queries.json"
        fixture = args.fixture or self.root / f"goland-{self.track}.json"
        data = read_json(fixture) if args.fixture or fixture.is_file() else None
        baseline = normalize_capture(data) if data is not None and "schemaVersion" in data else None
        spec_path = self.root / spec_name
        self.spec = (data if data is not None and baseline is None else
                     read_json(spec_path if spec_path.is_file() else TOOLS / "fixtures" / self.track / spec_name))
        self.cases = index_cases([
            *({"request": self.default_method, **row} for row in self.spec["cases"]),
            *({"request": "quickfix", **row} for row in self.spec.get("quickFixes", ())),
        ])
        if not self.cases:
            raise ValueError("fixture has no cases")
        self.paths = {}
        self.sources = {
            relative: self.path(relative).read_bytes().decode("utf-8")
            for relative in dict.fromkeys(case["path"] for case in self.cases.values())
        }
        self.revision = workspace_hash(self.root)
        self.baseline = self.capture_rows(baseline, complete=True) if baseline else {}
        self.timed_capture = None
        self.timed_rows = {}
        if getattr(args, "capture_goland", False):
            launcher = resolve_goland_exe(args.goland)
            capture_spec = {**self.spec, "iterations": args.iterations}
            self.timed_capture = capture_goland(launcher, self.track, self.root, capture_spec, self.timeout)
            if workspace_hash(self.root) != self.revision:
                raise ValueError("workspace files changed during GoLand capture")
            self.timed_capture.update(workspaceSha256=self.revision, binary=describe_binary(launcher))
            if args.goland_bench:
                write_json(args.goland_bench, self.timed_capture)
        elif getattr(args, "goland_bench", None):
            self.timed_capture = normalize_capture(read_json(args.goland_bench))
        elif hasattr(args, "iterations"):
            self.timed_capture = baseline
        if self.timed_capture is not None:
            self.timed_rows = self.capture_rows(self.timed_capture)
            for case_id, row in self.timed_rows.items():
                if case_id in self.baseline and any(
                    row.get(key) != self.baseline[case_id].get(key) for key in self.capture_fields
                ):
                    raise ValueError(f"GoLand fixture and timed results disagree for {case_id}")
                self.baseline.setdefault(case_id, row)

    def capture_rows(self, capture, *, complete=False):
        if capture["build"] != self.spec["build"]:
            raise ValueError("GoLand build differs from the query fixture")
        if capture.get("workspaceSha256", self.revision) != self.revision:
            raise ValueError("GoLand capture was recorded against different workspace contents")
        rows = index_cases([*capture["cases"], *capture.get("quickFixes", ())])
        if complete:
            missing = {case_id for case_id, case in self.cases.items() if case["request"] != "quickfix"} - rows.keys()
            if missing:
                raise ValueError(f"GoLand fixture is missing cases {sorted(missing)}")
        for case_id, row in rows.items():
            if case_id not in self.cases:
                raise ValueError(f"GoLand capture includes an unknown case {case_id}")
            case = self.cases[case_id]
            keys = ("path", "request", "line", "character", "token", "expectedCategories", "diagnosticCode", "importPath")
            if any(key in row and row[key] != case.get(key) for key in keys):
                raise ValueError(f"GoLand query metadata differs for {case_id}")
        return rows

    def path(self, relative):
        if relative not in self.paths:
            self.paths[relative] = fixture_path(relative, self.root)
        return self.paths[relative]

    @contextmanager
    def session(self, server):
        client = LspClient([server], self.root, self.timeout, environment=self.environment)
        try:
            capabilities = client.request(
                "initialize", processId=os.getpid(), rootUri=self.root.as_uri(), workspaceFolders=client.folders,
                initializationOptions={"diagnosticsTrigger": "Save"},
                capabilities={
                    "general": {"positionEncodings": ["utf-16"]},
                    "workspace": {"workspaceFolders": True},
                    "textDocument": {
                        "diagnostic": {"dynamicRegistration": False, "relatedDocumentSupport": False},
                        "codeAction": {"codeActionLiteralSupport": {
                            "codeActionKind": {"valueSet": ["quickfix", "source.organizeImports"]},
                        }},
                    },
                },
            )["capabilities"]
            if capabilities.get("positionEncoding", "utf-16").lower() != "utf-16":
                raise ValueError("server did not negotiate UTF-16 positions")
            for provider in self.providers:
                if capabilities.get(provider) in (None, False):
                    raise ValueError(f"server did not advertise {provider}")
            client.notify("initialized")
            for relative, text in self.sources.items():
                client.open_document(self.path(relative), text)
            yield client
            client.shutdown()
        finally:
            client.close()


def arguments(description, track, *, benchmark=False):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("server", type=executable)
    parser.add_argument("--workspace", type=Path, default=TOOLS / "fixtures" / track)
    parser.add_argument("--fixture", type=Path, help="GoLand capture or query manifest")
    parser.add_argument("--timeout", type=float, default=60.0, metavar="SEC")
    parser.add_argument("--json", type=Path, metavar="OUT")
    if benchmark:
        parser.add_argument("--goland-bench", type=Path, metavar="JSON", help="GoLand timings, or output capture path")
        parser.add_argument("--capture-goland", action="store_true", help="capture from the configured workspace open in GoLand")
        parser.add_argument("--goland", help="GoLand executable; defaults to GOLAND_EXE when capturing")
        parser.add_argument("--iterations", type=int, default=ITERATIONS, metavar="N")
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be positive and finite")
    if benchmark and args.iterations < 1:
        parser.error("--iterations must be positive")
    return args


def check_main(fixture_type, description, *, extra_checks=None):
    args = arguments(description, fixture_type.track)
    report = {"track": fixture_type.track, "cases": {}, "checks": {}}
    try:
        fixture = fixture_type(args)
        if fixture.baseline:
            report["goland_build"] = fixture.spec["build"]
        with fixture.session(args.server) as client:
            client.request("workspace/symbol", query="rsGoplsBenchmarkIndexReady")
            for case_id, case in fixture.cases.items():
                text = fixture.sources[case["path"]]
                try:
                    raw = fixture.request(client, case, text, 0)
                    observed = fixture.validate(case, raw, text, 0)
                except ERRORS as exc:
                    report["cases"][case_id] = {"path": case["path"], "error": str(exc)}
                    print(f"FAIL {case_id}: {exc}", file=sys.stderr)
                else:
                    report["cases"][case_id] = {"path": case["path"], "result": observed.result,
                                                 "differences": observed.differences}
                    print(f"{'DIFF' if observed.differences else 'PASS'} {case_id}")
            if extra_checks is not None:
                report["checks"] = extra_checks(fixture, client)
    except ERRORS as exc:
        report["error"] = str(exc)
        print(f"FAIL {exc}", file=sys.stderr)
    passed = sum("error" not in row for row in report["cases"].values())
    report["passed"] = "error" not in report and passed == len(report["cases"])
    print(f"{passed}/{len(report['cases'])} cases passed")
    return output_report(args, report)


def measure(fixture, client, case, text, shift, expected=None):
    started = time.perf_counter_ns()
    raw = fixture.request(client, case, text, shift)
    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    checked = fixture.validate(case, raw, text, shift)
    if expected is not None and checked.result != expected.result:
        raise ValueError("validated result changed between requests")
    return elapsed, checked


def benchmark(fixture, args):
    results = {}
    versions = dict.fromkeys(fixture.sources, 1)
    with fixture.session(args.server) as client, sample_working_set(client.process.pid) as sampler:
        client.request("workspace/symbol", query="rsGoplsBenchmarkIndexReady")
        first = {}
        for case_id, case in fixture.cases.items():
            row = results[case_id] = {"request": case["request"], "path": case["path"]}
            try:
                elapsed, checked = measure(fixture, client, case, fixture.sources[case["path"]], 0)
                first[case_id] = checked
                row.update(differences=checked.differences, timings_ms={"first": [elapsed]})
            except ERRORS as exc:
                row["error"] = str(exc)

        for case_id, expected in first.items():
            case, row = fixture.cases[case_id], results[case_id]
            relative = case["path"]
            path, text = fixture.path(relative), fixture.sources[relative]
            prefix = overlay_prefix(text)
            try:
                for _ in range(WARMUPS):
                    measure(fixture, client, case, text, 0, expected)
                warm = [measure(fixture, client, case, text, 0, expected)[0] for _ in range(args.iterations)]
                edited = []
                for _ in range(args.iterations):
                    versions[relative] += 1
                    client.change_document_range(path, versions[relative], START, START, prefix)
                    try:
                        elapsed, _ = measure(fixture, client, case, prefix + text, 1, expected)
                        edited.append(elapsed)
                    finally:
                        versions[relative] += 1
                        client.change_document_range(path, versions[relative], START, AFTER_PREFIX, "")
                    measure(fixture, client, case, text, 0, expected)
                row["timings_ms"].update(warm=warm, after_edit=edited)
            except ERRORS as exc:
                row["error"] = str(exc)
        return results, working_set(sampler)


def goland_timings(capture, row):
    edited = []
    if capture.get("editPrefix") == EDIT_PREFIX:
        edited = row.get("afterEditTimingsMs", [])
        if "afterEditFirstMs" in row and not edited:
            edited = [row["afterEditFirstMs"]]
    phases = {"first": [row["firstMs"]] if "firstMs" in row else [],
              "warm": row.get("timingsMs", []), "after_edit": edited}
    if any(type(ms) not in (int, float) or not math.isfinite(ms) or ms < 0
           for values in phases.values() for ms in values):
        raise ValueError(f"invalid GoLand timing for {row['id']}")
    return phases


def benchmark_summary(fixture, results, go):
    excluded, lanes = {}, {}
    capture = fixture.timed_capture
    for case_id, row in results.items():
        if "error" in row:
            continue
        timing = fixture.timed_rows.get(case_id)
        reference = None
        if capture is not None:
            if timing is None:
                excluded[case_id] = {"reason": "no GoLand timing row"}
            elif timing.get("sdkVersion") not in (go["GOVERSION"], go["GOVERSION"].removeprefix("go")):
                excluded[case_id] = {"reason": "Go SDK version differs", "sdkVersion": timing.get("sdkVersion")}
            elif row["differences"]:
                excluded[case_id] = {"reason": "GoLand result differs", "differences": row["differences"]}
            else:
                reference = goland_timings(capture, timing)
                row["goland_timings_ms"] = reference
        for phase, samples in row["timings_ms"].items():
            lane = lanes.setdefault(row["request"], {}).setdefault(
                phase, {"rs_gopls": [], "common": {"case_ids": [], "rs_gopls": [], "goland": []}},
            )
            lane["rs_gopls"].extend(samples)
            if reference is not None and reference[phase]:
                lane["common"]["case_ids"].append(case_id)
                lane["common"]["rs_gopls"].extend(samples)
                lane["common"]["goland"].extend(reference[phase])
    for phases in lanes.values():
        for lane in phases.values():
            lane["rs_gopls"] = summarize(lane["rs_gopls"])
            if capture is None:
                del lane["common"]
            else:
                for key in ("rs_gopls", "goland"):
                    lane["common"][key] = summarize(lane["common"][key])
    return lanes, excluded


def compare_main(fixture_type, description):
    args = arguments(description, fixture_type.track, benchmark=True)
    report = {"track": fixture_type.track, "passed": False}
    try:
        fixture = fixture_type(args)
        go = describe_go_env(fixture.root, fixture.environment, args.timeout)
        results, memory = benchmark(fixture, args)
        if workspace_hash(fixture.root) != fixture.revision:
            raise ValueError("workspace files changed during the benchmark")
        summary, excluded = benchmark_summary(fixture, results, go)
        report.update(
            server=describe_binary(args.server), workspace_sha256=fixture.revision, go=go,
            cases=results, latency=summary, excluded_from_goland=excluded,
            working_set={"rs_gopls": memory}, warmup_runs=WARMUPS, sample_runs=args.iterations,
            open_files=list(fixture.sources), edit_prefix=EDIT_PREFIX,
            conditions={
                "first": "first request per case after a nonempty workspace/symbol readiness probe",
                "warm": "same request after five validated warmups with server caches enabled",
                "after_edit": "request after inserting one comment in the open buffer, followed by a verified revert",
                "rs_gopls": "stdio request through JSON response, including prerequisite requests for compound queries",
                "goland": fixture.goland_timing,
                "go_workspace_environment": WORKSPACE_ENVIRONMENT,
            },
        )
        if fixture.timed_capture is not None:
            capture = fixture.timed_capture
            report["goland"] = {
                "build": capture["build"], "scope": capture.get("scope"), "binary": capture.get("binary"),
                "workspace_sha256": capture.get("workspaceSha256"),
            }
            report["working_set"]["goland"] = capture.get("workingSet")
        for case_id, row in results.items():
            if "error" in row:
                print(f"FAIL {case_id}: {row['error']}", file=sys.stderr)
        report["passed"] = all("error" not in row for row in results.values())
    except ERRORS as exc:
        report["error"] = str(exc)
        print(f"FAIL {exc}", file=sys.stderr)
    return output_report(args, report, print_json=True)


def output_report(args, report, *, print_json=False):
    if args.json is not None:
        try:
            write_json(args.json, report)
        except ERRORS as exc:
            print(f"FAIL writing report: {exc}", file=sys.stderr)
            return 1
    elif print_json:
        print(json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False))
    return int(not report["passed"])

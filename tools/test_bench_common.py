"""Check the shared benchmark helpers without launching either server."""

import hashlib
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from bench_common import (
    EDIT_PREFIX, Fixture, apply_text_edits, describe_binary, goland_timings, normalize_capture,
    normalize_path, percentile, relative_uri, resolve_goland_exe, summarize, write_json,
)


def edit(start, end, new_text):
    return {
        "range": {
            "start": {"line": start[0], "character": start[1]},
            "end": {"line": end[0], "character": end[1]},
        },
        "newText": new_text,
    }


class BenchCommonTests(unittest.TestCase):
    def test_nearest_rank_statistics(self):
        samples = list(range(20, 0, -1))
        self.assertEqual(percentile(samples, 0.50), 10)
        self.assertEqual(percentile(samples, 0.95), 19)
        self.assertEqual(summarize(samples), {"count": 20, "p50_ms": 10, "p95_ms": 19, "min_ms": 1, "max_ms": 20})
        self.assertEqual(summarize([4])["p95_ms"], 4)
        self.assertEqual(summarize([]), {"count": 0, "p50_ms": None, "p95_ms": None, "min_ms": None, "max_ms": None})

    def test_edits_preserve_crlf_and_use_original_utf16_coordinates(self):
        source = "a🙂b\r\n世界\r\n"
        edits = [edit((1, 1), (2, 0), "界!\r\n"), edit((0, 1), (0, 3), "🌊"), edit((0, 3), (0, 4), "B")]
        self.assertEqual(apply_text_edits(source, edits), "a🌊B\r\n世界!\r\n")

    def test_edits_preserve_insertion_order(self):
        edits = [edit((0, 1), (0, 1), "X"), edit((0, 1), (0, 1), "Y"), edit((0, 1), (0, 2), "Z")]
        self.assertEqual(apply_text_edits("abc", edits), "aXYZc")

    def test_edits_reject_invalid_positions_and_overlaps(self):
        source = "a🙂b\r\n"
        for start, end in [((0, 2), (0, 3)), ((0, 4), (0, 5)), ((2, 0), (2, 0)), ((1, 0), (0, 4))]:
            with self.subTest(start=start, end=end), self.assertRaises(ValueError):
                apply_text_edits(source, [edit(start, end, "")])
        with self.assertRaisesRegex(ValueError, "overlapping"):
            apply_text_edits(source, [edit((0, 0), (0, 3), ""), edit((0, 1), (0, 4), "")])

    def test_capture_normalizes_relative_paths_at_every_level(self):
        data = {"schemaVersion": 1, "root": ".", "cases": [{"path": r"dir\source.go", "target": {"path": "./dir/file.go"}}]}
        self.assertEqual(normalize_capture(data)["cases"], [{"path": "dir/source.go", "target": {"path": "dir/file.go"}}])
        for path in ("../source.go", r"dir\..\source.go", r"\\server\share\file.go", "/absolute.go", "Z:/absolute.go"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                normalize_path(path)
        with self.assertRaises(ValueError):
            normalize_capture({**data, "root": "../fixture"})

    def test_file_uri_and_binary_description_omit_machine_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            workspace = root / "fixture"
            workspace.mkdir()
            binary = workspace / "sample file"
            binary.write_bytes(b"sample")
            self.assertEqual(relative_uri(binary.as_uri(), workspace), binary.name)
            self.assertEqual(describe_binary(binary), {"name": binary.name, "sha256": hashlib.sha256(b"sample").hexdigest().upper()})
            with self.assertRaises(ValueError):
                relative_uri((root / binary.name).as_uri(), workspace)

    def test_goland_resolver_uses_only_cli_or_environment(self):
        python = Path(sys.executable).resolve()
        with patch.dict(os.environ, {"GOLAND_EXE": str(python)}):
            self.assertEqual(resolve_goland_exe(None), python)
        with patch.dict(os.environ, {"GOLAND_EXE": "missing-goland"}):
            self.assertEqual(resolve_goland_exe(python), python)
        with patch.dict(os.environ, {"GOLAND_EXE": ""}):
            with self.assertRaisesRegex(ValueError, "--goland or GOLAND_EXE"):
                resolve_goland_exe(None)

    def test_live_capture_uses_requested_iteration_count(self):
        class SampleFixture(Fixture):
            track = "sample"
            default_method = "sample/request"
            capture_fields = ()

        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory).resolve()
            (workspace / "sample.go").write_text("package sample\n", encoding="utf-8")
            row = {"id": "sample", "path": "sample.go"}
            spec = {"build": "sample-build", "cases": [row]}
            write_json(workspace / "sample-queries.json", spec)
            args = SimpleNamespace(workspace=workspace, fixture=None, timeout=60, capture_goland=True,
                                   goland=sys.executable, goland_bench=None, iterations=3)
            capture = {"build": spec["build"], "cases": [row]}
            with patch("bench_common.capture_goland", return_value=capture) as run_capture:
                fixture = SampleFixture(args)
            run_capture.assert_called_once_with(Path(sys.executable).resolve(), "sample", workspace,
                                               {**spec, "iterations": 3}, 60)
            self.assertEqual(fixture.spec, spec)

    def test_goland_timings_replay_all_verified_edit_samples(self):
        row = {"id": "sample", "firstMs": 3, "timingsMs": [1, 2], "afterEditTimingsMs": [4, 5]}
        self.assertEqual(goland_timings({"editPrefix": EDIT_PREFIX}, row),
                         {"first": [3], "warm": [1, 2], "after_edit": [4, 5]})
        self.assertEqual(goland_timings({}, row)["after_edit"], [])


if __name__ == "__main__":
    unittest.main()

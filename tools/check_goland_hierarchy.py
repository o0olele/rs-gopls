#!/usr/bin/env python3
"""Check call and type hierarchy targets and UTF-16 call sites over stdio LSP."""

from bench_common import Checked, Fixture, check_main, point, range_offsets, relative_uri, shift_range
from check_goland_definitions import query_site


PREPARE_CALL = "textDocument/prepareCallHierarchy"
PREPARE_TYPE = "textDocument/prepareTypeHierarchy"
INCOMING = "callHierarchy/incomingCalls"
OUTGOING = "callHierarchy/outgoingCalls"
SUPERTYPES = "typeHierarchy/supertypes"
SUBTYPES = "typeHierarchy/subtypes"
WORKER = "worker/worker.go"
DECLARATIONS = {
    "Dispatch": ("hierarchy.go", 4, 5),
    "Recurse": ("hierarchy.go", 9, 5),
    "TestLeaf": ("hierarchy_test.go", 16, 5),
    "Read": (WORKER, 3, 1),
    "Close": (WORKER, 8, 1),
    "Leaf": (WORKER, 41, 5),
    "Recursive": (WORKER, 45, 5),
    "Run": (WORKER, 52, 23),
    "Base.Read": (WORKER, 24, 17),
    "Reader": (WORKER, 2, 5),
    "ReadCloser": (WORKER, 6, 5),
    "Service": (WORKER, 11, 5),
    "Resetter": (WORKER, 16, 5),
    "Base": (WORKER, 20, 5),
    "Derived": (WORKER, 28, 5),
    "Alias": (WORKER, 36, 5),
}
PREPARED_TARGETS = {
    "prepare-function": "Dispatch",
    "prepare-cross-package-call": "Leaf",
    "prepare-interface-call": "Close",
    "prepare-method-call": "Run",
    "leaf-callers": "Leaf",
    "recursive-callers": "Recursive",
    "interface-method-callers": "Read",
    "dispatch-callees": "Dispatch",
    "method-callees": "Run",
    "recursive-callees": "Recursive",
    "leaf-callees": "Leaf",
    "prepare-interface": "Service",
    "prepare-struct": "Derived",
    "prepare-alias-use": "Alias",
    "reader-subtypes": "Reader",
    "read-closer-subtypes": "ReadCloser",
    "base-subtypes": "Base",
    "service-supertypes": "Service",
    "read-closer-supertypes": "ReadCloser",
    "derived-supertypes": "Derived",
}
# Each site gives the LSP identifier range, followed by GoLand's columns when they differ.
# GoLand filters the recursive self-edge from both trees.
RECURSIVE_SITE = ((49, 22, 31),)
CALLS = {
    "leaf-callers": {
        "Dispatch": ((6, 48, 52, 41, 52), (6, 73, 77, 66, 77)),
        "TestLeaf": ((17, 24, 28, 17, 28),),
        "Recursive": ((47, 9, 13), (49, 8, 12)),
        "Run": ((53, 8, 12),),
    },
    "recursive-callers": {"Recurse": ((10, 15, 24, 8, 24),), "Recursive": RECURSIVE_SITE},
    "interface-method-callers": {"Dispatch": ((6, 16, 20, 8, 20),)},
    "dispatch-callees": {
        "Read": ((6, 16, 20, 8, 22),),
        "Close": ((5, 9, 14, 1, 16),),
        "Leaf": ((6, 48, 52, 41, 63), (6, 73, 77, 66, 80)),
        "Run": ((6, 33, 36, 25, 38),),
    },
    "method-callees": {"Base.Read": ((53, 21, 25, 13, 27),), "Leaf": ((53, 8, 12, 8, 28),)},
    "recursive-callees": {"Leaf": ((47, 9, 13, 9, 20), (49, 8, 12, 8, 19)), "Recursive": RECURSIVE_SITE},
    "leaf-callees": {},
}
TYPES = {
    "reader-subtypes": ("ReadCloser", "Service", "Base", "Derived", "Alias"),
    "read-closer-subtypes": ("Service", "Derived", "Alias"),
    "base-subtypes": ("Derived",),
    "service-supertypes": ("Reader", "ReadCloser", "Resetter"),
    "read-closer-supertypes": ("Reader",),
    "derived-supertypes": ("Reader", "ReadCloser", "Service", "Resetter", "Base"),
}
GOLAND_TYPES = {
    "base-subtypes": (),
    "derived-supertypes": ("Reader", "ReadCloser", "Service", "Resetter"),
}
IDENTIFIER_RANGES = "LSP uses identifier ranges for fromRanges"
QUALIFIED_REFERENCES = f"{IDENTIFIER_RANGES}. GoLand highlights qualified references."
CALL_EXPRESSIONS = f"{IDENTIFIER_RANGES}. GoLand highlights full call expressions."
RECURSIVE_EDGE = "LSP keeps the recursive self-edge. GoLand filters that edge."
INTENTIONAL_DIFFERENCES = {
    "leaf-callers": QUALIFIED_REFERENCES,
    "recursive-callers": f"{QUALIFIED_REFERENCES} {RECURSIVE_EDGE}",
    "interface-method-callers": QUALIFIED_REFERENCES,
    "dispatch-callees": CALL_EXPRESSIONS,
    "method-callees": CALL_EXPRESSIONS,
    "recursive-callees": f"{CALL_EXPRESSIONS} {RECURSIVE_EDGE}",
    "base-subtypes": "LSP lists structs that directly embed the queried type. GoLand returns an empty tree.",
    "derived-supertypes": "LSP lists the directly embedded struct as a parent. GoLand lists satisfied interfaces.",
}


def span(line, start, end):
    return {"start": {"line": line, "character": start}, "end": {"line": line, "character": end}}


def declaration(name):
    path, line, start = DECLARATIONS[name]
    return {"path": path, "selectionRange": span(line, start, start + len(name.rsplit(".", 1)[-1]))}


def range_key(value):
    return point(value["start"]), point(value["end"])


def sorted_items(items, edge):
    def key(row):
        item = row[edge] if edge else row
        return item["path"], range_key(item["selectionRange"])

    keys = [key(item) for item in items]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate hierarchy target")
    return sorted(items, key=key)


def expected_results(case, edge, *, goland=False):
    case_id = case["id"]
    root = declaration(PREPARED_TARGETS[case_id])
    if edge:
        results = []
        for name, sites in CALLS[case_id].items():
            if goland and sites == RECURSIVE_SITE:
                continue
            ranges = []
            for line, start, end, *goland_columns in sites:
                if goland and goland_columns:
                    start, end = goland_columns
                ranges.append(span(line, start, end))
            results.append({edge: declaration(name), "fromRanges": ranges})
    elif case["request"] in (PREPARE_CALL, PREPARE_TYPE):
        results = [root]
    else:
        names = GOLAND_TYPES[case_id] if goland and case_id in GOLAND_TYPES else TYPES[case_id]
        results = [declaration(name) for name in names]
    return {"target": root, "results": sorted_items(results, edge)}


class Hierarchy(Fixture):
    track = "hierarchy"
    default_method = None
    providers = ("callHierarchyProvider", "typeHierarchyProvider")
    capture_fields = ("target", "results")
    goland_timing = "smart-mode target resolution, tree expansion, and range extraction without UI rendering"

    def __init__(self, args):
        super().__init__(args)
        self.documents = dict(self.sources)
        for case in self.cases.values():
            if case["request"] not in (PREPARE_CALL, PREPARE_TYPE, INCOMING, OUTGOING, SUPERTYPES, SUBTYPES):
                raise ValueError(f"invalid hierarchy request for {case['id']}")
            relative = case["path"]
            query_site(self.path(relative), self.sources[relative], case, case["token"])

    def capture_rows(self, capture, *, complete=False):
        if capture.get("scope") != "All":
            raise ValueError("GoLand hierarchy capture must use All scope")
        return super().capture_rows(capture, complete=complete)

    def source(self, relative):
        if relative not in self.documents:
            self.documents[relative] = self.path(relative).read_bytes().decode("utf-8")
        return self.documents[relative]

    def request(self, client, case, text, shift):
        method = case["request"]
        prepare = PREPARE_CALL if method in (PREPARE_CALL, INCOMING, OUTGOING) else PREPARE_TYPE
        prepared = client.request(
            prepare, textDocument={"uri": self.path(case["path"]).as_uri()},
            position={"line": case["line"] + shift, "character": case["character"]},
        )
        if not isinstance(prepared, list) or len(prepared) != 1:
            raise ValueError("expected one prepared hierarchy target")
        target = prepared[0]
        results = prepared if method == prepare else client.request(method, item=target)
        return target, results

    def normalize(self, target, results, edge, *, edited_path=None, edited_text=None, shift=0):
        if not isinstance(results, list):
            raise ValueError("expected a hierarchy result array")

        def checked_range(relative, value):
            if relative == edited_path:
                range_offsets(edited_text, value)
                value = shift_range(value, -shift)
            range_offsets(self.source(relative), value)
            return value

        def item(value):
            relative = value["path"] if edited_path is None else relative_uri(value["uri"], self.root)
            selection = checked_range(relative, value["selectionRange"])
            start, end = range_key(selection)
            outer_start, outer_end = range_key(checked_range(relative, value["range"]))
            if not outer_start <= start < end <= outer_end:
                raise ValueError("hierarchy item does not enclose its identifier")
            return {"path": relative, "selectionRange": selection}

        root = item(target)
        rows = []
        for result in results:
            if edge:
                child = item(result[edge])
                source = child if edge == "from" else root
                ranges = [checked_range(source["path"], value) for value in result["fromRanges"]]
                keys = [range_key(value) for value in ranges]
                if not keys or len(keys) != len(set(keys)):
                    raise ValueError("hierarchy edge must have distinct call sites")
                rows.append({edge: child, "fromRanges": sorted(ranges, key=range_key)})
            else:
                rows.append(item(result))
        return {"target": root, "results": sorted_items(rows, edge)}

    def validate(self, case, raw, text, shift):
        method, case_id = case["request"], case["id"]
        edge = {INCOMING: "from", OUTGOING: "to"}.get(method)
        actual = self.normalize(*raw, edge, edited_path=case["path"], edited_text=text, shift=shift)
        reference = self.baseline.get(case_id)
        captured = self.normalize(reference["target"], reference["results"], edge) if reference else None
        if case_id in PREPARED_TARGETS:
            expected = expected_results(case, edge)
            if captured is not None and captured != expected_results(case, edge, goland=True):
                raise ValueError(f"unexpected GoLand hierarchy result {captured!r}")
        elif captured is not None:
            expected = captured
        else:
            raise ValueError("hierarchy query requires a GoLand result fixture")
        if actual != expected:
            raise ValueError(f"hierarchy result expected {expected!r}, received {actual!r}")
        if captured is None or captured == actual:
            return Checked(actual)
        reason = INTENTIONAL_DIFFERENCES.get(case_id)
        if reason is None:
            raise ValueError("hierarchy difference requires a recorded reason")
        return Checked(actual, ({"reason": reason, "goland": captured, "rs_gopls": actual},))


def main():
    return check_main(Hierarchy, __doc__)


if __name__ == "__main__":
    raise SystemExit(main())

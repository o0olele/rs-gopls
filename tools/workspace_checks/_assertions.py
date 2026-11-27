"""Shared assertions for Go workspace checks."""

from collections import Counter
from pathlib import Path

from check_definitions import check_location
from check_goland_definitions import normalize_locations
from check_goland_symbols import contains, lsp_span
from lsp_harness import path_from_uri


def expect_workspace_symbols(result, *targets, kind=13, container="symbols"):
    if not isinstance(result, list):
        raise RuntimeError(f"expected a workspace symbol list, got {result!r}")
    expected = Counter(
        (target.path, lsp_span(target.selection()), target.text[target.start:target.end], kind, container)
        for target in targets
    )
    actual = Counter(
        (path_from_uri(row["location"]["uri"]), lsp_span(row["location"]["range"]),
         row["name"], row["kind"], row["containerName"])
        for row in result
    )
    if actual != expected:
        missing = list((expected - actual).elements())
        extra = list((actual - expected).elements())
        raise RuntimeError(
            f"workspace symbols: {len(missing)} missing {missing[:3]!r}, {len(extra)} extra {extra[:3]!r}",
        )


def expect_outline(result, *members, parent=None):
    if not isinstance(result, list) or len(result) != len(members):
        raise RuntimeError(f"outline: expected {len(members)} symbols, got {result!r}")
    for row, (kind, target, children) in zip(result, members):
        expected = (target.text[target.start:target.end], kind, target.selection())
        if (row["name"], row["kind"], row["selectionRange"]) != expected:
            raise RuntimeError(f"outline: expected {expected!r}, got {row!r}")
        span, selection = lsp_span(row["range"]), lsp_span(row["selectionRange"])
        if span == selection or not contains(span, selection) or (parent and not contains(parent, span)):
            raise RuntimeError(f"outline: invalid declaration range {span} for {row!r}")
        expect_outline(row.get("children", []), *children, parent=span)


def no_definition(client, reference):
    response = client.request(
        "textDocument/definition",
        textDocument={"uri": reference.path.as_uri()},
        position=reference.cursor(),
    )
    if response:
        raise RuntimeError(f"expected no definition for the excluded symbol, received {response!r}")


def send_definition(client, request_id, reference):
    client.send(
        id=request_id, method="textDocument/definition",
        params={"textDocument": {"uri": reference.path.as_uri()}, "position": reference.cursor()},
    )


def expect_target(response, target):
    if "error" in response:
        raise RuntimeError(f"expected definition in {target.path}, received {response['error']}")
    expect_location(response["result"], target)


def check_type_location(client, reference, target):
    result = client.request(
        "textDocument/typeDefinition",
        textDocument={"uri": reference.path.as_uri()},
        position=reference.cursor(),
    )
    expect_location(result, target)


def expect_location(result, target):
    locations = normalize_locations(result)
    if (
        len(locations) != 1
        or Path(locations[0]["path"]) != target.path
        or locations[0]["range"] != target.selection()
    ):
        raise RuntimeError(f"expected {target.path} {target.selection()}, received {locations!r}")


def reference_params(query, include_declaration=False):
    return {
        "textDocument": {"uri": query.path.as_uri()},
        "position": query.cursor(),
        "context": {"includeDeclaration": include_declaration},
    }


def check_references(client, query, expected, *, include_declaration=False):
    def location_key(path, span):
        start, end = span["start"], span["end"]
        return path, start["line"], start["character"], end["line"], end["character"]

    result = client.request("textDocument/references", **reference_params(query, include_declaration))
    if result is not None and not isinstance(result, list):
        raise RuntimeError(f"expected an array of references, received {result!r}")
    actual = sorted(
        location_key(Path(location["path"]), location["range"])
        for location in normalize_locations(result)
    )
    targets = sorted(location_key(target.path, target.selection()) for target in expected)
    if actual != targets:
        raise RuntimeError(
            f"references from {query.path} at {query.cursor()} "
            f"(includeDeclaration={include_declaration}): expected {targets!r}, got {actual!r}"
        )

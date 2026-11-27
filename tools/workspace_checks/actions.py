"""Apply formatting and code-action edits to unsaved buffers over stdio."""

from check_goland_definitions import utf16_offset
from lsp_harness import PausedGoList, path_from_uri, position_at, session, write

from .diagnostics import PULL, capability_session, full_report, pull


def apply_edits(text, edits):
    replacements = []
    for edit in edits:
        span = edit["range"]
        start = utf16_offset(text, **span["start"])
        end = utf16_offset(text, **span["end"])
        replacements.append((start, end, edit["newText"]))
    limit = len(text)
    for start, end, replacement in sorted(replacements, reverse=True):
        if not start <= end <= limit:
            raise RuntimeError(f"overlapping or reversed edits {edits!r}")
        text = text[:start] + replacement + text[end:]
        limit = start
    return text


def check_document_formatting(executable, root, timeout):
    write(root / "go.mod", "module example.test/format\n\ngo 1.22\n")
    disk = "package sample\n"
    path = write(root / "format.go", disk)
    before = 'package sample\nfunc label()string{return "😀"+"ok"}\n'
    clean = 'package sample\n\nfunc label() string { return "😀" + "ok" }\n'
    broken = "package sample\nfunc broken( {\n"
    with session(executable, root, timeout) as client:
        client.open_document(path, before)
        for version, (text, expected) in enumerate([
                (before, clean), (clean, clean), (before.replace("\n", "\r\n"), clean.replace("\n", "\r\n")),
                (broken, broken)], 1):
            if version > 1:
                client.change_document(path, version, text)
            edits = client.request("textDocument/formatting", textDocument={"uri": path.as_uri()},
                                   options={"tabSize": 4, "insertSpaces": False})
            if (text == expected and edits != []) or apply_edits(text, edits) != expected:
                raise RuntimeError(f"unexpected formatting edits for version {version}: {edits!r}")
        if path.read_text(encoding="utf-8") != disk:
            raise RuntimeError("formatting overwrote the disk document")


def actions(client, path, text, kind):
    span = {"start": position_at(text, 0), "end": position_at(text, len(text))}
    result = client.request("textDocument/codeAction", textDocument={"uri": path.as_uri()},
                            range=span, context={"diagnostics": [], "only": [kind]})
    if any(action.get("kind") != kind for action in result):
        raise RuntimeError(f"unexpected action kind for {kind}: {result!r}")
    return result


def applied(action, path, text):
    changes = action["edit"]["changes"]
    if len(changes) != 1 or path_from_uri(next(iter(changes))) != path:
        raise RuntimeError(f"unexpected WorkspaceEdit {changes!r}")
    return apply_edits(text, next(iter(changes.values())))


def check_code_actions(executable, root, timeout):
    project = root / "project"
    write(project / "go.mod", "module example.test/actions\n\ngo 1.22\n")
    write(project / "helper" / "helper.go", "package helper\nfunc Value(x int) int { return x }\n")
    text = ('package sample\n\nimport "strings"\n\nfunc compute(x int) int {\n'
            '\tunused := 42\n\tx := x + 1\n\treturn helper.Value(x)\n}\n')
    path = write(project / "fix.go", text)
    imports = write(project / "imports.go", 'package sample\nimport _ "fmt"\n')
    preamble = 'package sample\n\n/*\n#include <stdlib.h>\n*/\nimport "C"\n\n'
    messy = (preamble + 'import (\n\t// Keep this with strings.\n\t"strings" // trailing\n\t"fmt"\n)\n'
             + '\nvar _ = strings.TrimSpace\n')
    fixes = [
        ("var.unused", "Replace unused variable 'unused' with '_'", "unused := 42", "_ = 42"),
        ("shortvar.no_new_vars", "Replace ':=' with '='", "x := x + 1", "x = x + 1"),
        ("import.unused", 'Remove unused import "strings"', 'import "strings"\n\n', ""),
        ("name.unresolved", 'Import "example.test/actions/helper"', 'package sample\n\n',
         'package sample\n\nimport "example.test/actions/helper"\n\n'),
    ]
    with capability_session(executable, project, timeout, PULL) as (client, caps):
        if (caps.get("documentFormattingProvider") is not True or
                set(caps["codeActionProvider"]["codeActionKinds"]) != {"quickfix", "source.organizeImports"}):
            raise RuntimeError(f"missing formatting/code action capabilities {caps!r}")
        client.open_document(path, text)
        for index, (code, title, old, new) in enumerate(fixes):
            candidates = actions(client, path, text, "quickfix")
            expected_codes = [entry[0] for entry in fixes[index:]]
            if sorted(action["diagnostics"][0]["code"] for action in candidates) != sorted(expected_codes):
                raise RuntimeError(f"expected quickfixes for {expected_codes!r}, received {candidates!r}")
            action = next((a for a in candidates if a["title"] == title and a["diagnostics"][0]["code"] == code), None)
            if action is None:
                raise RuntimeError(f"missing {title} in {candidates!r}")
            expected = text.replace(old, new, 1)
            text = applied(action, path, text)
            if text != expected:
                raise RuntimeError(f"incorrect edits for {title}: {action!r}")
            client.change_document(path, index + 2, text)
            full_report(pull(client, path), expected_codes[1:])
        if actions(client, path, text, "quickfix"):
            raise RuntimeError("a clean document still offered a quickfix")
        client.open_document(imports, messy)
        organize = actions(client, imports, messy, "source.organizeImports")
        if len(organize) != 1:
            raise RuntimeError(f"expected one organizeImports action, received {organize!r}")
        organized = applied(organize[0], imports, messy)
        if organized != messy.replace('\t"fmt"\n', ''):
            raise RuntimeError(f"organizeImports lost an import comment or cgo preamble: {organize!r}")
        client.change_document(imports, 2, organized)
        full_report(pull(client, imports), [])
        if actions(client, imports, organized, "source.organizeImports"):
            raise RuntimeError("organizeImports was not idempotent")

    gate = PausedGoList(root / "control", timeout)
    with capability_session(executable, project, timeout, PULL, **gate.environment) as (client, _):
        gate.fail_next()
        unsorted = 'package sample\nimport (\n\t"strings"\n\t"fmt"\n)\n'
        client.open_document(imports, unsorted)
        organize = actions(client, imports, unsorted, "source.organizeImports")
        expected = unsorted.replace('\t"strings"\n\t"fmt"', '\t"fmt"\n\t"strings"')
        if len(organize) != 1 or applied(organize[0], imports, unsorted) != expected:
            raise RuntimeError(f"local organizeImports failed after a package load failure: {organize!r}")


CASES = [
    ("document-formatting", check_document_formatting),
    ("code-actions", check_code_actions),
]

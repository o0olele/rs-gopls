"""Verify LSP environment configuration against the real Go package loader."""

from dataclasses import replace
import shutil
import subprocess

from lsp_harness import session, site, write

from ._assertions import check_location, no_definition
from .gopath_fixture import dual_gopath, isolated_go_environment


def _check_env_locations(executable_path, fixture, timeout, environment):
    configured = {"GOPATH": fixture.gopath, "GO111MODULE": True}
    conflicting = {"GOPATH": str(fixture.server)}
    options = {
        "env": {"env": configured, "rs-gopls": {"env": conflicting}},
        "rs-gopls.env": {"env": [], "rs-gopls": {"env": configured}, "gopls": {"env": conflicting}},
        "gopls.env": {"rs-gopls": {"env": False}, "gopls": {"env": configured},
                      "settings": {"env": conflicting}},
        "settings.env": {"env": None, "settings": {"env": configured}},
    }
    reference, target = fixture.reference, fixture.target
    for key, settings in options.items():
        try:
            with session(executable_path, fixture.server, timeout, environment=environment,
                         initialization_options=settings) as client:
                client.open_document(reference.path, reference.text)
                check_location(client, reference, target)
                client.change_configuration({"env": conflicting})
                no_definition(client, reference)
                client.change_configuration(settings)
                check_location(client, reference, target)
        except RuntimeError as exc:
            raise RuntimeError(f"{key} configuration: {exc}") from exc


def _check_startup_environment(executable_path, fixture, timeout, environment):
    reference, target = fixture.reference, fixture.target
    process_env = {**environment, "GOPATH": fixture.gopath, "GO111MODULE": "off"}
    options = {"env": {"GOPATH": str(fixture.server)}}
    with session(executable_path, fixture.server, timeout, environment=process_env,
                 initialization_options=options) as client:
        client.open_document(reference.path, reference.text)
        no_definition(client, reference)
        # Zed binary.env values are inherited by the language server process.
        client.change_configuration({"env": {}})
        check_location(client, reference, target)
        client.change_configuration({"env": {"GO111MODULE": "on"}})
        no_definition(client, reference)
        client.change_configuration({"env": {"GO111MODULE": "off"}})
        check_location(client, reference, target)


def _check_module_mode(executable_path, fixture, timeout, environment):
    real_go = shutil.which("go", path=environment.get("PATH"))
    if real_go is None:
        raise RuntimeError("Go executable not found for the persisted module mode check")
    goenv = {**environment, "GOENV": str(fixture.root / "go-env")}
    result = subprocess.run(
        [real_go, "env", "-w", "GO111MODULE=on"], cwd=fixture.server,
        env=goenv, capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode != 0:
        raise RuntimeError(f"could not persist GO111MODULE=on: {result.stderr.strip()}")

    reference, target = fixture.reference, fixture.target
    options = {"env": {"GOPATH": fixture.gopath}}
    for label, process_env in (("inherited", {**environment, "GO111MODULE": "on"}), ("persisted", goenv)):
        try:
            with session(executable_path, fixture.server, timeout, environment=process_env,
                         initialization_options=options) as client:
                client.open_document(reference.path, reference.text)
                no_definition(client, reference)
                client.change_configuration({"env": {"GOPATH": fixture.gopath, "GO111MODULE": "off"}})
                check_location(client, reference, target)
        except RuntimeError as exc:
            raise RuntimeError(f"{label} GO111MODULE: {exc}") from exc


def _check_unmatched_directory(executable_path, fixture, timeout, environment):
    outside = fixture.server.with_name(f"{fixture.server.name}-outside")
    reference = fixture.reference
    path = write(outside / fixture.caller.name, reference.text)
    reference = replace(reference, path=path)
    with session(executable_path, outside, timeout, environment=environment,
                 initialization_options={"env": {"GOPATH": fixture.gopath}}) as client:
        client.open_document(path, reference.text)
        no_definition(client, reference)
        # A forced GOPATH load can resolve the same source outside the configured roots.
        client.change_configuration({"env": {"GOPATH": fixture.gopath, "GO111MODULE": "off"}})
        check_location(client, reference, fixture.target)


def _check_empty_gowork(executable_path, root, timeout, environment):
    write(root / "go.work", "go 1.22\n\nuse (\n\t./app\n\t./lib\n)\n")
    write(root / "app" / "go.mod", "module example.test/app\n\ngo 1.22\n")
    write(root / "lib" / "go.mod", "module example.test/lib\n\ngo 1.22\n")
    caller = write(
        root / "app" / "caller.go",
        'package app\nimport "example.test/lib"\nfunc call() { lib.Run() }\n',
    )
    library = write(root / "lib" / "lib.go", "package lib\nfunc Run() {}\n")
    reference = site(caller, "lib.[[Run]]()")
    target = site(library, "func [[Run]]()")
    with session(executable_path, root, timeout, environment={**environment, "GOWORK": "off"},
                 initialization_options={"env": {"GOWORK": ""}}) as client:
        client.open_document(caller, reference.text)
        check_location(client, reference, target)
        client.change_configuration({"env": {}})
        no_definition(client, reference)
        client.change_configuration({"env": {"GOWORK": ""}})
        check_location(client, reference, target)


def check_gopath_configuration(executable_path, root, timeout):
    environment = isolated_go_environment()
    fixture = dual_gopath(root / "legacy")
    _check_env_locations(executable_path, fixture, timeout, environment)
    _check_startup_environment(executable_path, fixture, timeout, environment)
    _check_module_mode(executable_path, fixture, timeout, environment)
    _check_unmatched_directory(executable_path, fixture, timeout, environment)
    _check_empty_gowork(executable_path, root / "workspace", timeout, environment)

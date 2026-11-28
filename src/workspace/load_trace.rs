use std::cell::Cell;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::Instant;

use serde_json::{Value, json};

use super::{Workspace, WorkspaceLoad};
use crate::telemetry;

#[derive(Clone, Copy)]
struct LoadContext {
    trigger: &'static str,
    load: WorkspaceLoad,
    in_flight_generation: Option<u64>,
    build_group_count: Option<usize>,
    group_id: Option<usize>,
}

thread_local! {
    static LOAD_CONTEXT: Cell<Option<LoadContext>> = const { Cell::new(None) };
}

pub(crate) struct LoadTrace(Option<LoadContext>);

impl LoadTrace {
    pub(crate) fn enter(
        trigger: &'static str,
        load: WorkspaceLoad,
        in_flight_generation: Option<u64>,
    ) -> Option<Self> {
        telemetry::enabled().then(|| {
            Self(LOAD_CONTEXT.replace(Some(LoadContext {
                trigger,
                load,
                in_flight_generation,
                build_group_count: None,
                group_id: None,
            })))
        })
    }

    pub(super) fn group(group_id: usize) -> Option<Self> {
        if !telemetry::enabled() {
            return None;
        }
        let previous = LOAD_CONTEXT.get()?;
        LOAD_CONTEXT.set(Some(LoadContext {
            group_id: Some(group_id),
            ..previous
        }));
        Some(Self(Some(previous)))
    }
}

impl Drop for LoadTrace {
    fn drop(&mut self) {
        LOAD_CONTEXT.set(self.0);
    }
}

fn emit(event: &'static str, mut fields: Value, fallback: &'static str) {
    if let Some(context) = LOAD_CONTEXT.get() {
        fields["trigger"] = json!(context.trigger);
        fields["target_generation"] = json!(context.load.generation);
        fields["in_flight_generation"] = json!(context.in_flight_generation);
        fields["include_tests"] = json!(context.load.include_tests);
        fields["build_group_count"] = json!(context.build_group_count);
        if let Some(group_id) = context.group_id {
            fields["group_id"] = json!(group_id);
        }
    } else {
        fields["trigger"] = json!(fallback);
    }
    telemetry::emit(event, fields);
}

pub(super) fn groups_discovered(build_group_count: usize) {
    if telemetry::enabled() {
        if let Some(context) = LOAD_CONTEXT.get() {
            LOAD_CONTEXT.set(Some(LoadContext {
                build_group_count: Some(build_group_count),
                ..context
            }));
        }
        emit(
            "workspace_load_groups",
            json!({ "build_group_count": build_group_count }),
            "workspace_load",
        );
    }
}

pub(super) fn graph_built(workspace: &Workspace) {
    if telemetry::enabled() {
        // Report unique source files with each file's selected project package.
        // This walk stays on the loader thread.
        let project_file_count = workspace
            .file_packages
            .iter()
            .filter(|(path, id)| {
                let package = &workspace.packages[*id];
                !package.external
                    && path.parent() == Some(package.directory.as_path())
                    && workspace.is_project_package(package)
                    && super::is_symbol_source(path)
            })
            .count();
        emit(
            "workspace_load_graph",
            json!({
                "build_group_count": workspace.group_scopes.len(),
                "package_count": workspace.packages.len(),
                "project_file_count": project_file_count,
                "error_count": workspace.package_errors.len(),
            }),
            "workspace_load",
        );
    }
}

pub(crate) struct CommandTrace {
    command: &'static str,
    directory: String,
    started: Instant,
    pid: Option<u32>,
    exit: Option<(ExitStatus, f64)>,
    counts: Option<(usize, usize, usize)>,
    cancelled: bool,
}

impl CommandTrace {
    pub(crate) fn new(command: &'static str, directory: &Path) -> Option<Self> {
        let started = telemetry::now()?;
        Some(Self {
            command,
            directory: telemetry::path(directory),
            started,
            pid: None,
            exit: None,
            counts: None,
            cancelled: false,
        })
    }

    pub(crate) fn output(command: &mut Command, trace: Option<&mut Self>) -> io::Result<Output> {
        let Some(trace) = trace else {
            return command.output();
        };
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        trace.spawned(child.id());
        let output = child.wait_with_output()?;
        trace.exited(output.status);
        Ok(output)
    }

    pub(crate) fn spawned(&mut self, pid: u32) {
        self.pid = Some(pid);
        emit(
            "workspace_load_cmd",
            json!({
                "phase": "cmd_start",
                "command": self.command,
                "pid": pid,
                "directory": self.directory,
            }),
            "standalone_go_command",
        );
    }

    pub(crate) fn exited(&mut self, status: ExitStatus) {
        self.exit = Some((status, telemetry::elapsed(self.started)));
    }

    pub(super) fn cancelled(&mut self) {
        self.cancelled = true;
    }

    pub(crate) fn parsed(&mut self, packages: usize, project_files: usize, errors: usize) {
        self.counts = Some((packages, project_files, errors));
    }
}

impl Drop for CommandTrace {
    fn drop(&mut self) {
        let code = self.exit.and_then(|(status, _)| status.code());
        let cmd_ms = self
            .exit
            .map_or_else(|| telemetry::elapsed(self.started), |(_, ms)| ms);
        let (packages, project_files, errors) = self.counts.unwrap_or_default();
        let failed = !self.cancelled
            && (self.counts.is_none() || !self.exit.is_some_and(|(status, _)| status.success()));
        emit(
            "workspace_load_cmd",
            json!({
                "phase": "cmd_done",
                "command": self.command,
                "pid": self.pid,
                "directory": self.directory,
                "cmd_ms": cmd_ms,
                "exit_code": code,
                "cancelled": self.cancelled,
                "package_count": packages,
                "project_file_count": project_files,
                "error_count": errors + usize::from(failed),
            }),
            "standalone_go_command",
        );
    }
}

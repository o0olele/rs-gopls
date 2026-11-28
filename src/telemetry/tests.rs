use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use lsp_server::{Connection, Message, Notification, Request};

use super::*;
use crate::protocol::path_uri;
use crate::test_support::TestProject;

const MODE: &str = "RS_GOPLS_TELEMETRY_TEST_MODE";
const ROOT: &str = "RS_GOPLS_TELEMETRY_TEST_ROOT";
const SOURCE_CANARY: &str = "telemetry_source_canary_35ca5d9d";
const SOURCE: &str =
    "package sample\nimport \"example.test/telemetry/dep\"\nfunc use() { dep.Target() }\n";

#[test]
fn configured_sessions_emit_private_jsonl() {
    let project = TestProject::new();
    project.write("go.mod", "module example.test/telemetry\n\ngo 1.22\n");
    project.write("caller.go", SOURCE);
    project.write("dep/target.go", "package dep\nfunc Target() {}\n");
    for (mode, source) in [
        ("disabled", None),
        ("process", Some("process_env")),
        ("options", Some("initialization_options")),
    ] {
        let log = project.root.join(mode).join("diagnostics.jsonl");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "telemetry::tests::session_worker", "--nocapture"])
            .env(MODE, mode)
            .env(ROOT, &project.root)
            .env("GO111MODULE", "on")
            .env("GOWORK", "off")
            .env("GOENV", "off")
            .env_remove(FILE_ENV)
            .env_remove(REDACT_ENV);
        if mode == "process" {
            command.env(FILE_ENV, &log);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{mode} worker failed\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let Some(source) = source else {
            assert!(!log.exists());
            continue;
        };
        let raw = fs::read_to_string(&log).unwrap();
        assert!(raw.ends_with('\n'));
        assert!(!raw.contains(SOURCE_CANARY));
        assert!(!raw.contains("Undefined Name"));
        assert!(!raw.contains(project.root.file_name().unwrap().to_str().unwrap()));
        let rows: Vec<Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["event"], "session_start");
        assert_eq!(rows[0]["source"], source);
        assert_eq!(rows[0]["redact"], true);
        assert!(
            rows.windows(2).all(
                |pair| pair[0]["ts_ms"].as_f64().unwrap() <= pair[1]["ts_ms"].as_f64().unwrap()
            )
        );
        for event in [
            "session_config",
            "workspace_load_start",
            "workspace_load_cmd",
            "workspace_load_done",
            "workspace_install_start",
            "workspace_install",
            "workspace_dirty",
            "definition_trace",
            "references_trace",
            "rpc_recv",
            "rpc_done",
        ] {
            assert!(
                rows.iter().any(|row| row["event"] == event),
                "{mode} missing {event}"
            );
        }
        let definition = rows
            .iter()
            .find(|row| row["event"] == "definition_trace")
            .unwrap();
        assert_eq!(definition["found_count"], 1);
        assert_eq!(definition["hit_path"], "cross_package");
        let document = definition["document"].as_str().unwrap();
        assert!(document.starts_with("h:"));
        assert_eq!(document.len(), 18);
        assert!(!raw.contains("caller.go"));
        assert!(!raw.contains("target.go"));
        assert!(!raw.contains("go.mod"));
        let diagnostic = |id| {
            rows.iter()
                .find(|row| row["event"] == "rpc_done" && row["req_id"] == id)
                .unwrap()
        };
        let full = diagnostic(10);
        let unchanged = diagnostic(11);
        assert_eq!(full["doc_version"], 2);
        assert_eq!(full["stage"], "stage2");
        assert_eq!(full["context_completeness"], "full");
        assert_eq!(full["delivery"], "delivered");
        assert_eq!(full["report_kind"], "full");
        assert_eq!(full["diagnostic_codes"], json!({"name.unresolved": 1}));
        assert_eq!(unchanged["delivery"], "cache_hit");
        assert_eq!(unchanged["report_kind"], "unchanged");
        assert_eq!(full["document"], document);
        for field in ["job", "run", "document", "doc_version", "result_id", "lane"] {
            assert!(!full[field].is_null(), "missing {field}");
            assert_eq!(full[field], unchanged[field], "changed {field}");
        }
        assert!(
            rows.iter()
                .any(|row| row["event"] == "diagnostic_worker_done"
                    && row["job"] == full["job"]
                    && row["doc_version"] == full["doc_version"])
        );
    }
}

#[test]
fn session_worker() {
    let Some(mode) = std::env::var_os(MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let log = root.join(&mode).join("diagnostics.jsonl");
    let _session = Session::start();
    assert_eq!(enabled(), mode == "process");
    let (server, client) = Connection::memory();
    let worker = thread::spawn(move || crate::serve(&server));
    let options = if mode == "options" {
        json!({"env": {FILE_ENV: log, REDACT_ENV: "1"}})
    } else {
        Value::Null
    };
    let initialize = json!({
        "capabilities": {"textDocument": {"diagnostic": {}}},
        "rootUri": path_uri(&root).unwrap(),
        "initializationOptions": options,
    });
    assert!(request(&client, 1, "initialize", initialize).is_object());
    let uri = path_uri(&root.join("caller.go")).unwrap();
    let source = format!("{SOURCE}// {SOURCE_CANARY}\n");
    notify(
        &client,
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri, "languageId": "go", "version": 1, "text": source}}),
    );
    let column = SOURCE.lines().nth(2).unwrap().find("Target").unwrap();
    let params = json!({
        "textDocument": {"uri": uri},
        "position": {"line": 2, "character": column},
        "context": {"includeDeclaration": true},
    });
    let definition = request(&client, 2, "textDocument/definition", params.clone());
    assert!(
        definition["uri"]
            .as_str()
            .unwrap()
            .ends_with("/dep/target.go")
    );
    for id in [3, 4] {
        let references = request(&client, id, "textDocument/references", params.clone());
        assert_eq!(references.as_array().unwrap().len(), 2);
    }
    let manifest_uri = path_uri(&root.join("go.mod")).unwrap();
    notify(
        &client,
        "textDocument/didSave",
        json!({"textDocument": {"uri": manifest_uri}}),
    );
    assert!(request(&client, 5, "textDocument/definition", params).is_object());
    notify(
        &client,
        "textDocument/didChange",
        json!({"textDocument": {"uri": uri, "version": 2}, "contentChanges": [{
            "text": format!("package sample\nfunc use() {{ {SOURCE_CANARY}() }}\n")
        }]}),
    );
    let full = request(
        &client,
        10,
        "textDocument/diagnostic",
        json!({"textDocument": {"uri": uri}}),
    );
    assert_eq!(full["kind"], "full");
    assert_eq!(full["items"][0]["code"], "name.unresolved");
    let unchanged = request(
        &client,
        11,
        "textDocument/diagnostic",
        json!({
            "textDocument": {"uri": uri}, "previousResultId": full["resultId"]
        }),
    );
    assert_eq!(unchanged["kind"], "unchanged");
    if mode == "disabled" {
        notify(
            &client,
            "workspace/didChangeConfiguration",
            json!({"settings": {"env": {FILE_ENV: log}}}),
        );
    }
    assert!(request(&client, 6, "shutdown", Value::Null).is_null());
    notify(&client, "exit", Value::Null);
    worker.join().unwrap().unwrap();
}

fn request(client: &Connection, id: i32, method: &str, params: Value) -> Value {
    let request: Message = Request::new(id.into(), method.into(), params).into();
    crate::rpc_telemetry::received(&request);
    client.sender.send(request).unwrap();
    let response = client
        .receiver
        .recv_timeout(Duration::from_secs(20))
        .unwrap();
    crate::rpc_telemetry::write_message(&response, &mut io::sink()).unwrap();
    let Message::Response(response) = response else {
        panic!("expected response");
    };
    assert_eq!(response.id, id.into());
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

fn notify(client: &Connection, method: &str, params: Value) {
    client
        .sender
        .send(Notification::new(method.into(), params).into())
        .unwrap();
}

#[test]
fn full_queue_reports_drops_on_shutdown() {
    let project = TestProject::new();
    let log = project.root.join("diagnostics.jsonl");
    let (events, receiver) = bounded(1);
    let (stop, stopped) = bounded(1);
    let logger = Logger {
        events,
        stop,
        writer: Mutex::new(None),
        dropped: Arc::new(AtomicU64::new(0)),
        origin: Instant::now(),
        source: "test",
    };
    logger.send("kept", json!({}));
    logger.send("dropped", json!({}));
    assert_eq!(logger.dropped.load(Relaxed), 1);
    logger.stop.try_send(()).unwrap();
    write_log(&log, receiver, stopped, &logger.dropped, logger.origin).unwrap();
    let raw = fs::read_to_string(log).unwrap();
    let rows: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["event"], "kept");
    assert_eq!(rows[1]["event"], "log_dropped");
    assert_eq!(rows[1]["dropped"], 1);
}

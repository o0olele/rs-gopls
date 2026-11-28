use std::fs;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lsp_server::{Message, Notification, Request, RequestId};
use serde_json::{Value, json};

struct Client {
    child: Child,
    input: ChildStdin,
    responses: Receiver<Message>,
}

impl Client {
    fn start(root: &std::path::Path, log: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rs-gopls"))
            .current_dir(root)
            .env_remove("RS_GOPLS_LOG_FILE")
            .env("RS_GOPLS_COMPLETION_PROFILE", log)
            .env("GO111MODULE", "on")
            .env("GOENV", "off")
            .env("GOWORK", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            while let Some(message) = Message::read(&mut output).unwrap() {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            responses,
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        Message::from(Notification::new(method.into(), params))
            .write(&mut self.input)
            .unwrap();
    }

    fn request(&mut self, id: impl Into<RequestId>, method: &str, params: Value) -> Value {
        let id = id.into();
        Message::from(Request::new(id.clone(), method.into(), params))
            .write(&mut self.input)
            .unwrap();
        loop {
            match self
                .responses
                .recv_timeout(Duration::from_secs(20))
                .unwrap()
            {
                Message::Response(response) if response.id == id => {
                    assert!(response.error.is_none(), "{method}: {:?}", response.error);
                    return response.result.unwrap_or(Value::Null);
                }
                Message::Request(request) => {
                    Message::from(lsp_server::Response::new_ok(request.id, Value::Null))
                        .write(&mut self.input)
                        .unwrap();
                }
                _ => {}
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn stdio_redacts_completion_and_measures_ignored_and_applied_changes() {
    const NAME: &str = "private-stdio-document.go";
    const REQUEST: &str = "private-completion-id";
    const CANARY: &str = "private_stdio_source";
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let project = Project(std::env::temp_dir().join(format!("rs-gopls-stdio-{nonce}")));
    fs::create_dir(&project.0).unwrap();
    fs::write(
        project.0.join("go.mod"),
        "module example.test/stdio\n\ngo 1.22\n",
    )
    .unwrap();
    let content = format!(
        "package sample\n// Candidate is documented.\nvar Candidate = 1\nfunc use() {{ _ = Candidate }}\n// {CANARY}\n"
    );
    let path = project.0.join(NAME);
    fs::write(&path, &content).unwrap();
    let log = project.0.join("logs").join("telemetry.jsonl");
    let root = url::Url::from_directory_path(&project.0)
        .unwrap()
        .to_string();
    let uri = url::Url::from_file_path(path).unwrap().to_string();
    let mut client = Client::start(&project.0, &log);
    client.request(
        1,
        "initialize",
        json!({
            "rootUri": root,
            "workspaceFolders": [{"uri": root, "name": "telemetry"}],
            "capabilities": {"workspace": {"symbol": {}}, "textDocument": {
                "completion": {"completionItem": {"resolveSupport": {"properties": ["detail", "documentation"]}}},
                "diagnostic": {},
            }},
        }),
    );
    client.notify("initialized", json!({}));
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {"uri": uri, "languageId": "go", "version": 2, "text": content},
        }),
    );
    for (version, text) in [(1, "package sample"), (3, content.as_str())] {
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": text}],
            }),
        );
    }
    assert_eq!(
        client.request(5, "workspace/symbol", json!({"query": "NoSuchGraphProbe"})),
        json!([])
    );
    let completion = client.request(
        REQUEST.to_owned(),
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 3, "character": 22},
        }),
    );
    let candidate = completion
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["label"] == "Candidate")
        .unwrap();
    assert!(candidate["detail"].is_null());
    assert!(candidate["documentation"].is_null());
    let resolved = client.request(3, "completionItem/resolve", candidate.clone());
    assert!(resolved["detail"].as_str().unwrap().contains("Candidate"));
    assert_eq!(
        resolved["documentation"]["value"],
        "Candidate is documented."
    );
    client.request(2, "shutdown", Value::Null);
    client.notify("exit", Value::Null);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = client.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "server did not exit");
        thread::sleep(Duration::from_millis(5));
    }
    let raw = fs::read_to_string(log).unwrap();
    for secret in [
        NAME,
        REQUEST,
        CANARY,
        project.0.file_name().unwrap().to_str().unwrap(),
    ] {
        assert!(!raw.contains(secret), "telemetry leaked {secret}");
    }
    let rows: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["source"], "completion_profile_env");
    assert_eq!(rows[0]["redact"], true);
    let changes: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "notification" && row["method"] == "textDocument/didChange")
        .collect();
    assert_eq!(changes.len(), 2);
    for (row, version) in changes.into_iter().zip([1, 3]) {
        assert_eq!(row["doc_version"], version);
        assert!(row["dispatch_queue_ms"].as_f64().unwrap() >= 0.0);
    }
    let rpc = rows
        .iter()
        .find(|row| row["event"] == "rpc_done" && row["method"] == "textDocument/completion")
        .unwrap();
    let id = rpc["req_id"].as_str().unwrap();
    assert_eq!(id.len(), 18);
    assert!(id.starts_with("h:"));
    let worker = rows
        .iter()
        .find(|row| {
            row["event"] == "completion_worker" && row["req_id"] == id && row["outcome"] == "ready"
        })
        .unwrap();
    for field in [
        "job_seq",
        "pending_ms",
        "worker_queue_ms",
        "worker_ms",
        "result_queue_ms",
    ] {
        assert!(
            worker[field].is_number(),
            "completion worker missing {field}"
        );
    }
    let profile = rows
        .iter()
        .find(|row| row["event"] == "completion_profile" && row["req_id"] == id)
        .unwrap();
    for phase in [
        "expected_type",
        "same_package",
        "imports",
        "rank",
        "render",
        "encode",
    ] {
        assert!(
            worker["phases_ms"][phase].is_number(),
            "worker missing {phase}"
        );
        assert!(
            profile["phases_ms"][phase].is_number(),
            "profile missing {phase}"
        );
    }
    let resolve = rows
        .iter()
        .find(|row| row["event"] == "rpc_done" && row["req_id"] == 3)
        .unwrap();
    assert_eq!(resolve["method"], "completionItem/resolve");
    assert!(resolve["phases_ms"]["rev_check"].is_number());
    assert!(resolve["phases_ms"]["detail_doc"].is_number());
}

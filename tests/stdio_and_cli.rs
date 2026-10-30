use industrious::{sessions::SessionStore, storage};
use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};

// MCP refuses a browser consult before creating a session when the host has no Chrome. Admission
// only checks for an executable file, so any one stands in; no test here launches it.
const CHROME_STAND_IN: &str = env!("CARGO_BIN_EXE_industrious");

struct McpChild {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
fn public_session(store: &SessionStore) -> Value {
    let entries = store
        .list(false)
        .unwrap()
        .into_iter()
        .filter(|entry| entry["options"]["providerAttempt"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "{entries:?}");
    store.require(entries[0]["id"].as_str().unwrap()).unwrap()
}
fn provider_child(store: &SessionStore, parent: &Value) -> Value {
    let link = &parent["browser"]["consultation"]["active"];
    assert_eq!(link["version"], 1);
    assert_eq!(link["parentId"], parent["id"]);
    assert_ne!(link["childId"], parent["id"]);
    let child = store.require(link["childId"].as_str().unwrap()).unwrap();
    assert_eq!(child["browser"]["attempt"], *link);
    assert_eq!(
        child["options"]["providerAttempt"],
        json!({"version":1,"role":"browser-provider-attempt"})
    );
    child
}
#[tokio::test]
async fn persistent_project_uploads_are_refused_before_reading_files_or_browser_configuration() {
    let home = tempfile::tempdir().unwrap();
    let server = industrious::mcp::IndustriousMcp {
        store: SessionStore::new(home.path().join("sessions")),
        cwd: home.path().to_owned(),
        env: Default::default(),
    };
    for confirm in [false, true] {
        let result = server
            .invoke(
                "project_sources",
                json!({
                    "operation":"add",
                    "chatgptUrl":"https://chatgpt.com/g/g-p-public/project",
                    "files":["PUBLIC-missing-file.rs"],
                    "confirmMutation":confirm,
                    "dryRun":false
                }),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Temporary Chat policy"),
            "Persistent upload must stop before resolving the nonexistent file"
        );
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }
}
#[tokio::test]
async fn restart_cli_creates_a_fresh_api_run_and_preserves_its_active_source_and_saved_cwd() {
    use tokio::io::AsyncReadExt;
    for wait in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let saved = home.path().join("saved");
        fs::create_dir(&saved).unwrap();
        fs::write(saved.join("PUBLIC-source.rs"), "PUBLIC saved source bytes").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(8), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = vec![];
            let mut buffer = [0; 4096];
            let body = loop {
                let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 1024 * 1024);
                if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|s| s.trim().parse::<usize>().ok())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length])
                            .unwrap();
                    }
                }
            };
            let response=[
                json!({"type":"response.created","response":{"id":"resp_PUBLIC_restart","status":"in_progress","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[]}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_PUBLIC_restart","content":[]}}),
                json!({"type":"response.content_part.added","output_index":0,"item_id":"msg_PUBLIC_restart","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
                json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_PUBLIC_restart","content_index":0,"delta":"PUBLIC fresh restart answer"}),
                json!({"type":"response.completed","response":{"id":"resp_PUBLIC_restart","status":"completed","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[{"type":"message","id":"msg_PUBLIC_restart","content":[{"type":"output_text","text":"PUBLIC fresh restart answer","annotations":[]}]}]}})
            ].into_iter().map(|event|format!("data: {event}\r\n\r\n")).collect::<String>();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
            body
        });
        let store = SessionStore::new(home.path().join("oracle"));
        let original=store.create(json!({"prompt":"PUBLIC restart the saved request","model":"gpt-6-astra","mode":"api","reasoningMode":"pro","file":["PUBLIC-source.rs"],"provider":"azure","azure":{"endpoint":format!("http://{address}"),"deployment":"PUBLIC-restart-deployment"},"background":false,"previousResponseId":"resp_PUBLIC_previous","followupSessionId":"PUBLIC-previous","followupModel":"gpt-6-astra","waitPreference":!wait}),&saved,Some("one")).unwrap();
        let source_path = store.directory("one").unwrap().join("meta.json");
        let before = fs::read(&source_path).unwrap();
        assert_eq!(original["status"], "pending");
        storage::atomic_json(&store.home().join("config.json"),&json!({"promptSuffix":"PUBLIC must not be added","model":"gpt-5","heartbeatSeconds":1})).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
        command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            command.env("SystemRoot", root);
        }
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            command
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("ORACLE_HOME_DIR", store.home())
                .env("OPENAI_API_KEY", "PUBLIC-synthetic-key")
                .current_dir(home.path())
                .args(["restart", "one", if wait { "--wait" } else { "--no-wait" }])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if wait {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("PUBLIC fresh restart answer")
            );
        } else {
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains(
                        "Session: one-2\nMode: api background\nModels: 1\nDetach: yes, polling\nReattach: industrious session one-2\nPro runs can take up to 60 minutes (usually 10-15). Add --wait to stay attached.\n"
                    )
            );
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let restarted = loop {
            let restarted = store.require("one-2").unwrap();
            if industrious::sessions::terminal(restarted["status"].as_str().unwrap_or("")) {
                break restarted;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(restarted["status"], "completed");
        assert_eq!(fs::read(&source_path).unwrap(), before);
        assert_eq!(store.list(false).unwrap().len(), 2);
        assert_eq!(
            restarted["options"]["prompt"],
            original["options"]["prompt"]
        );
        assert_eq!(restarted["cwd"], json!(saved));
        assert_eq!(restarted["options"]["waitPreference"], wait);
        assert_eq!(restarted["options"]["followupSessionId"], "PUBLIC-previous");
        assert_eq!(restarted["response"]["id"], "resp_PUBLIC_restart");
        assert!(
            store
                .log("one-2")
                .unwrap()
                .contains("PUBLIC fresh restart answer")
        );
        let body = server.await.unwrap();
        assert_eq!(body["previous_response_id"], "resp_PUBLIC_previous");
        assert_eq!(body["reasoning"]["mode"], "pro");
        assert!(body.to_string().contains("PUBLIC saved source bytes"));
        assert!(!body.to_string().contains("PUBLIC must not be added"));
        if !wait {
            let worker: industrious::process::ProcessIdentity =
                serde_json::from_value(restarted["lifecycle"]["processIdentity"].clone()).unwrap();
            while worker.liveness() != industrious::process::Liveness::Dead {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}
#[tokio::test]
async fn native_reply_cli_validates_request_before_endpoint_io_and_requires_both_paths() {
    for case in [
        "missing-output",
        "same-path",
        "invalid-json",
        "missing-endpoint",
        "missing-profile",
    ] {
        let home = tempfile::tempdir().unwrap();
        let request = home.path().join("PUBLIC-request.json");
        let answer = home.path().join("PUBLIC-answer.md");
        let raw = if case == "invalid-json" {
            "PUBLIC invalid JSON".to_owned()
        } else {
            json!({"chatId":"PUBLIC-conversation","label":"PUBLIC-label-1234"}).to_string()
        };
        fs::write(&request, &raw).unwrap();
        let profile = home.path().join("PUBLIC-profile");
        if case != "missing-profile" {
            fs::create_dir(&profile).unwrap();
        }
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
        command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            command.env("SystemRoot", root);
        }
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path().join("sessions"))
            .env("ORACLE_BROWSER_PROFILE_DIR", &profile)
            .current_dir(home.path())
            .args(["recover-native", "--request"])
            .arg(&request)
            .kill_on_drop(true);
        if case != "missing-output" {
            command.arg("--write-output").arg(if case == "same-path" {
                &request
            } else {
                &answer
            });
        }
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        let expected = match case {
            "missing-output" => "requires --request <path> and --write-output <path>",
            "same-path" => "Request and answer paths must differ.",
            "invalid-json" => "Native reply request could not be read as JSON.",
            "missing-endpoint" | "missing-profile" => {
                "Open Industrious's manual-login Chrome before recovering a native reply."
            }
            _ => unreachable!(),
        };
        assert!(error.contains(expected), "{case}: {error}");
        assert!(!answer.exists());
        assert_eq!(fs::read_to_string(&request).unwrap(), raw);
        assert!(
            SessionStore::new(home.path().join("sessions"))
                .list(true)
                .unwrap()
                .is_empty()
        );
    }
}
#[tokio::test]
async fn attach_running_profile_conflict_refuses_before_session_or_discovery_io() {
    for configured in [false, true] {
        let home = tempfile::tempdir().unwrap();
        if configured {
            storage::atomic_json(
                &home.path().join("config.json"),
                &json!({"browser":{"attachRunning":true}}),
            )
            .unwrap();
        }
        let borrowed = home.path().join("PUBLIC borrowed profile");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
        command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
        if !configured {
            command.arg("--browser-attach-running");
        }
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            command.env("SystemRoot", root);
        }
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            command
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("ORACLE_HOME_DIR", home.path())
                .current_dir(home.path())
                .args([
                    "--engine",
                    "browser",
                    "--prompt",
                    "PUBLIC attach conflict",
                    "--wait",
                    "--remote-chrome",
                    &address.to_string(),
                    "--browser-manual-login-profile-dir",
                ])
                .arg(&borrowed)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("--browser-attach-running cannot be combined")
        );
        assert!(
            SessionStore::new(home.path())
                .list(true)
                .unwrap()
                .is_empty()
        );
        assert!(!borrowed.exists());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn root_cli_no_detach_preserves_wait_defaults_without_dispatching_background_requests() {
    for (engine, no_wait) in [
        ("browser", false),
        ("browser", true),
        ("api", false),
        ("api", true),
    ] {
        let home = tempfile::tempdir().unwrap();
        storage::atomic_json(
            &home.path().join("config.json"),
            &json!({"browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("PUBLIC-profile"),"cookieSync":false}}),
        ).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
        command
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .env("ORACLE_NO_DETACH", "1")
            .env("OPENAI_API_KEY", "PUBLIC synthetic fixture key")
            .current_dir(home.path())
            .args([
                "--engine",
                engine,
                "-p",
                "PUBLIC check wait policy before any dispatch",
            ])
            .kill_on_drop(true);
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            command.env("SystemRoot", root);
        }
        if no_wait {
            command.arg("--no-wait");
        }
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let inline = engine == "browser" && !no_wait;
        // An inline run failure is reported once, as the session log's ERROR line on stdout.
        let error = String::from_utf8_lossy(if inline {
            &output.stdout
        } else {
            &output.stderr
        });
        assert!(
            if inline {
                error.contains("\nERROR: ") && error.contains("not initialized")
            } else {
                error.contains("Unable to start in background; use --wait to run inline.")
            },
            "{engine} no_wait={no_wait}: {error}"
        );
        let store = SessionStore::new(home.path());
        let sessions = store.list(false).unwrap();
        assert_eq!(sessions.len(), if inline { 2 } else { 1 });
        let metadata = public_session(&store);
        assert_eq!(metadata["options"]["waitPreference"], inline);
        assert!(metadata.get("dispatch").is_none());
        if inline {
            assert_eq!(metadata["status"], "error");
            assert_eq!(metadata["lifecycle"]["detached"], false);
            assert_eq!(provider_child(&store, &metadata)["status"], "error");
        } else {
            assert_eq!(metadata["status"], "pending");
            assert!(metadata.get("lifecycle").is_none());
        }
    }
}

#[tokio::test]
async fn local_browser_cli_and_mcp_admit_foreground_and_detached_rust_workers() {
    for detached in [false, true] {
        for mcp in [false, true] {
            let home = tempfile::tempdir().unwrap();
            storage::atomic_json(
                &home.path().join("config.json"),
                &json!({"engine":"browser","browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("profile"),"cookieSync":false}}),
            ).unwrap();
            let store = SessionStore::new(home.path());
            let mut server = if mcp {
                Some(McpChild::start(home.path()).await)
            } else {
                None
            };
            if let Some(server) = &mut server {
                server.initialise().await;
                let reply = server.call(2, "consult", json!({"prompt":"PUBLIC native admission with an uninitialised isolated profile","engine":"browser","waitForCompletion":!detached})).await;
                assert_eq!(
                    reply["result"]["structuredContent"]["detached"] == true,
                    detached,
                    "{reply}"
                );
                assert_eq!(reply["result"]["isError"] == true, !detached, "{reply}");
            } else {
                let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
                command
                    .env_clear()
                    .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
                    .env("HOME", home.path())
                    .env("USERPROFILE", home.path())
                    .env("ORACLE_HOME_DIR", home.path())
                    .current_dir(home.path())
                    .args([
                        "--engine",
                        "browser",
                        "-p",
                        "PUBLIC native admission with an uninitialised isolated profile",
                    ])
                    .args([
                        "--browser-headless",
                        "--browser-hide-window",
                        "--browser-manual-login",
                        "--browser-manual-login-profile-dir",
                    ])
                    .arg(home.path().join("PUBLIC-chosen-profile"))
                    .kill_on_drop(true);
                if !detached {
                    command.env("ORACLE_NO_DETACH", "1").arg("--wait");
                } else {
                    command.arg("--no-wait");
                }
                let output = tokio::time::timeout(Duration::from_secs(10), command.output())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    output.status.success(),
                    detached,
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if detached {
                    assert!(
                        String::from_utf8_lossy(&output.stdout).contains(
                            "Mode: browser background\nModels: 1\nDetach: yes, polling\n"
                        )
                    );
                } else {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    assert!(
                        stdout.contains("\nERROR: ") && stdout.contains("not initialized"),
                        "{stdout}"
                    );
                    assert!(!String::from_utf8_lossy(&output.stderr).contains('\u{2716}'));
                }
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let metadata = loop {
                let metadata = public_session(&store);
                if metadata["status"] == "error" {
                    break metadata;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "worker did not reach terminal preflight"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert_eq!(metadata["models"][0]["status"], "error");
            assert_eq!(metadata["lifecycle"]["engine"], "browser");
            assert_eq!(metadata["lifecycle"]["detached"], detached);
            if !mcp {
                assert_eq!(metadata["browser"]["config"]["headless"], true);
                assert_eq!(metadata["browser"]["config"]["hideWindow"], true);
                assert_eq!(
                    metadata["browser"]["config"]["manualLoginProfileDir"],
                    home.path()
                        .join("PUBLIC-chosen-profile")
                        .to_string_lossy()
                        .as_ref()
                );
            }
            assert_eq!(metadata["recoveryRequired"], false);
            let child = provider_child(&store, &metadata);
            assert_eq!(store.list(false).unwrap().len(), 2);
            assert_eq!(child["status"], "error");
            assert_eq!(
                child["browser"]["config"]["manualLoginProfileDir"],
                metadata["browser"]["config"]["manualLoginProfileDir"]
            );
            assert!(child.get("dispatch").is_none());
            assert!(child["browser"]["runtime"]["chromeTargetId"].is_null());
            assert!(metadata.get("dispatch").is_none());
            assert!(
                metadata["errorMessage"]
                    .as_str()
                    .unwrap()
                    .contains("not initialized")
            );
            if detached {
                let identity: industrious::process::ProcessIdentity =
                    serde_json::from_value(metadata["lifecycle"]["processIdentity"].clone())
                        .unwrap();
                while identity.liveness() != industrious::process::Liveness::Dead {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "owned worker remains alive"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            if let Some(server) = server {
                server.shutdown().await;
            }
        }
    }
}
impl McpChild {
    async fn start(home: &std::path::Path) -> Self {
        Self::spawn(home, env!("CARGO_BIN_EXE_industrious-mcp"), &[]).await
    }
    async fn spawn(home: &std::path::Path, program: &str, args: &[&str]) -> Self {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("ORACLE_HOME_DIR", home)
            .env("CHROME_PATH", CHROME_STAND_IN)
            .current_dir(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }
    async fn send(&mut self, message: Value) {
        let mut raw = serde_json::to_vec(&message).unwrap();
        raw.push(b'\n');
        self.input.as_mut().unwrap().write_all(&raw).await.unwrap();
        self.input.as_mut().unwrap().flush().await.unwrap();
    }
    async fn receive(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.output.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(n, 0, "MCP child exited without reply");
            let value: Value = serde_json::from_str(&line)
                .expect("stdout must contain only complete JSON-RPC lines");
            assert_eq!(value["jsonrpc"], "2.0");
            if value["id"] == id {
                return value;
            }
        }
    }
    async fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})).await;
        self.receive(id).await
    }
    async fn initialise(&mut self) {
        self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"synthetic-controller","version":"0.0.0"}}})).await;
        let response = self.receive(1).await;
        assert_eq!(
            response["result"],
            serde_json::from_str::<Value>(include_str!("../fixtures/mcp-initialize.json")).unwrap()
        );
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
    }
    async fn shutdown(mut self) {
        self.input.take();
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("MCP child must exit on stdin EOF")
            .unwrap();
        assert!(status.success(), "MCP child shutdown failed: {status}");
        let mut line = String::new();
        while self.output.read_line(&mut line).await.unwrap() > 0 {
            let frame: Value =
                serde_json::from_str(&line).expect("trailing stdout frame must be JSON");
            assert_eq!(frame["jsonrpc"], "2.0");
            line.clear();
        }
    }
}
// bin/oracle-cli.ts main: `industrious oracle-mcp` serves MCP; the word is never a prompt.
#[tokio::test]
async fn oracle_mcp_alias_serves_mcp_and_creates_no_session() {
    let home = tempfile::tempdir().unwrap();
    let mut server = McpChild::spawn(
        home.path(),
        env!("CARGO_BIN_EXE_industrious"),
        &["oracle-mcp"],
    )
    .await;
    server.initialise().await;
    server.shutdown().await;
    assert!(!home.path().join("sessions").exists());
}
#[tokio::test]
async fn actual_stdio_discovery_sessions_wait_and_resources_match_reference_shapes() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC captured session","model":"gpt-5.1"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    store
        .update(id, |_| {
            Ok(Some(
                json!({"status":"completed","historicalExtension":{"retain":true}}),
            ))
        })
        .unwrap();
    store
        .append_log(id, Some("gpt-5.1"), "PUBLIC historical answer\n")
        .unwrap();
    let mut server = McpChild::start(home.path()).await;
    server.initialise().await;
    for (request_id, method, fixture) in [
        (2, "tools/list", include_str!("../fixtures/mcp-tools.json")),
        (
            3,
            "resources/templates/list",
            include_str!("../fixtures/mcp-resources.json"),
        ),
    ] {
        server
            .send(json!({"jsonrpc":"2.0","id":request_id,"method":method}))
            .await;
        assert_eq!(
            server.receive(request_id).await["result"],
            serde_json::from_str::<Value>(fixture).unwrap()
        );
    }
    let result = server
        .call(
            4,
            "sessions",
            json!({"id":id,"detail":true,"extraFieldIsStripped":true}),
        )
        .await;
    assert_eq!(
        result["result"]["structuredContent"]["session"]["metadata"]["historicalExtension"]["retain"],
        true
    );
    assert!(
        result["result"]["structuredContent"]["session"]["log"]
            .as_str()
            .unwrap()
            .contains("PUBLIC historical answer")
    );
    let result = server.call(5, "wait", json!({"id":id,"timeoutMs":0})).await;
    assert_eq!(
        result["result"]["structuredContent"]["waitStatus"],
        "terminal"
    );
    assert_eq!(result["result"]["structuredContent"]["cancelled"], false);
    for (n, kind) in [(6, "metadata"), (7, "request"), (8, "log")] {
        server.send(json!({"jsonrpc":"2.0","id":n,"method":"resources/read","params":{"uri":format!("oracle-session://{id}/{kind}")}})).await;
        let result = server.receive(n).await;
        assert!(result["result"]["contents"][0]["text"].is_string());
    }
    let invalid = server.call(9, "sessions", json!({"id":"../outside"})).await;
    assert_eq!(invalid["result"]["isError"], true);
    let invalid = server
        .call(10, "wait", json!({"id":id,"unknown":true}))
        .await;
    assert_eq!(invalid["result"]["isError"], true);
    server.shutdown().await;
}
#[tokio::test]
async fn actual_stdio_dry_runs_never_create_sessions_or_touch_browser_state() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("public.rs"), "fn public() {}\n").unwrap();
    let mut server = McpChild::start(home.path()).await;
    server.initialise().await;
    let result=server.call(2,"consult",json!({"prompt":"PUBLIC browser preview","preset":"chatgpt-pro-heavy","dryRun":true,"files":["public.rs"]})).await;
    assert_eq!(result["result"]["structuredContent"]["status"], "dry-run");
    assert_eq!(
        result["result"]["structuredContent"]["resolved"]["model"],
        "gpt-6-pro"
    );
    assert_eq!(
        result["result"]["structuredContent"]["resolved"]["browser"]["thinkingTime"],
        "pro"
    );
    let result = server
        .call(
            3,
            "chatgpt_image",
            json!({"prompt":"PUBLIC illustration","dryRun":true,"aspectRatio":"16:9"}),
        )
        .await;
    let path = result["result"]["structuredContent"]["requestedOutputPath"]
        .as_str()
        .unwrap();
    assert!(path.starts_with(home.path().join("generated").to_str().unwrap()));
    // Reference dry runs print the plan, not the composed prompt.
    let output = result["result"]["structuredContent"]["output"]
        .as_str()
        .unwrap();
    let name = std::path::Path::new(path)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let image = output
        .lines()
        .find_map(|line| line.strip_prefix("  image output: "))
        .unwrap();
    assert!(
        image.ends_with(name) && !output.contains("16:9"),
        "{output}"
    );
    let result=server.call(4,"project_sources",json!({"operation":"add","chatgptUrl":"https://chatgpt.com/g/g-p-public/project","files":["public.rs"],"dryRun":true,"unknownIsStripped":true})).await;
    assert_eq!(
        result["result"]["structuredContent"]["plannedUploads"][0]["batch"],
        1
    );
    for (id, name, input) in [
        (5, "consult", json!({"prompt":"PUBLIC","unknown":true})),
        (
            6,
            "chatgpt_image",
            json!({"prompt":"PUBLIC","unknown":true}),
        ),
        (
            7,
            "consult",
            json!({"prompt":"PUBLIC","model":"gpt-5.5-pro","dryRun":true}),
        ),
        (8, "consult", json!({"prompt":"PUBLIC","engine":"browser"})),
    ] {
        assert_eq!(
            server.call(id, name, input).await["result"]["isError"],
            true
        );
    }
    assert!(!home.path().join("sessions").exists());
    assert!(!home.path().join("browser-profile").exists());
    server.shutdown().await;
}
#[tokio::test]
async fn actual_stdio_wait_cancellation_and_eof_preserve_the_underlying_record() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC pending record","model":"gpt-6-astra"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let path = store.directory(id).unwrap().join("meta.json");
    let before = fs::read(&path).unwrap();
    let mut server = McpChild::start(home.path()).await;
    server.initialise().await;
    server.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait","arguments":{"id":id}}})).await;
    server.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"synthetic cancellation"}})).await;
    server
        .send(json!({"jsonrpc":"2.0","id":3,"method":"ping"}))
        .await;
    assert!(server.receive(3).await.get("result").is_some());
    assert_eq!(fs::read(&path).unwrap(), before);
    server.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"wait","arguments":{"id":id}}})).await;
    server.shutdown().await;
    assert_eq!(fs::read(path).unwrap(), before);
}
fn cli(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_industrious"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ORACLE_HOME_DIR", home)
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn browser_health_uses_explicit_safe_inputs_and_reports_failed_launch_without_selection() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("PUBLIC health profile");
    let inherited = home.path().join("PUBLIC ignored inherited profile");
    let missing = home.path().join("PUBLIC missing Chrome");
    fs::create_dir(home.path().join(".oracle")).unwrap();
    fs::write(home.path().join(".oracle/config.json"),r#"{"browser":{"manualLoginProfileDir":"PUBLIC ignored project profile","remoteChrome":{"host":"example.invalid","port":1},"chatgptUrl":"https://chatgpt.com/c/PUBLIC-ignored","cookieSync":true}}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_industrious"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .env("ORACLE_BROWSER_PROFILE_DIR", &inherited)
        .current_dir(home.path())
        .args(["browser-health", "--json", "--browser-chrome-path"])
        .arg(&missing)
        .arg("--browser-manual-login-profile-dir")
        .arg(&profile)
        .args(["--browser-timeout", "1s"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "controls_broken");
    assert_eq!(result["cleanupComplete"], true);
    assert_eq!(result["observations"]["browserConnected"], false);
    assert!(profile.is_dir());
    assert!(!inherited.exists());
    assert!(!home.path().join("sessions").exists());
    let invalid = cli(
        home.path(),
        &["browser-health", "--json", "--browser-timeout", "301s"],
    );
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("between 1ms and 5m"));
}
#[test]
fn actual_cli_help_version_alias_render_and_stdin_interfaces() {
    let home = tempfile::tempdir().unwrap();
    let help = cli(home.path(), &["--help"]);
    assert!(help.status.success());
    assert_eq!(
        String::from_utf8(help.stdout).unwrap(),
        include_str!("../fixtures/cli-help.txt")
    );
    assert!(help.stderr.is_empty());
    let debug = cli(home.path(), &["--debug-help"]);
    assert!(debug.status.success());
    assert_eq!(
        String::from_utf8(debug.stdout).unwrap(),
        include_str!("../fixtures/cli-debug-help.txt")
    );
    assert!(cli(home.path(), &["--version"]).status.success());
    fs::write(home.path().join("public.rs"), "pub fn public() {}\n").unwrap();
    let rendered = cli(
        home.path(),
        &[
            "--message",
            "PUBLIC review",
            "--include",
            "public.rs",
            "--render",
        ],
    );
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    assert!(rendered.contains("### File: public.rs"));
    assert!(rendered.contains("1 | pub fn public() {}"));
    assert!(!home.path().join("sessions").exists());
    let mut child = Command::new(env!("CARGO_BIN_EXE_industrious"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .current_dir(home.path())
        .args(["-p", "-", "--render"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"PUBLIC piped prompt\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("PUBLIC piped prompt")
    );
    assert_eq!(
        cli(home.path(), &["--unknown-option"]).status.code(),
        Some(1)
    );
    assert_eq!(
        cli(home.path(), &["--engine", "api"]).status.code(),
        Some(1)
    );
}
#[test]
fn actual_cli_session_path_keeps_boolean_and_file_alias_scopes_separate() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC path contract","model":"gpt-5.1"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let output = cli(home.path(), &["session", id, "--path"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Session dir:")
    );
    store
        .update(id, |_| Ok(Some(json!({"status":"completed"}))))
        .unwrap();
    let output = cli(home.path(), &["status", id]);
    assert!(output.status.success());
    assert_eq!(
        cli(home.path(), &["session", "../outside"]).status.code(),
        Some(1)
    );
    let before = fs::read(store.directory(id).unwrap().join("meta.json")).unwrap();
    let output = cli(home.path(), &["session", "--clear"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
        before
    );
    // Read-only historical access permits retired model records.
    storage::atomic_json(
        &store.directory(id).unwrap().join("models/gpt-5.1.json"),
        &json!({"model":"gpt-5.1","status":"completed"}),
    )
    .unwrap();
    assert!(
        cli(home.path(), &["session", id, "--model", "gpt-5.1"])
            .status
            .success()
    );
}
#[tokio::test]
async fn actual_cli_session_streams_live_logs_and_finishes_on_the_saved_terminal_commit() {
    use tokio::io::AsyncReadExt;
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let owner = industrious::process::ProcessIdentity::current().unwrap();
    let meta = store
        .create(
            json!({"prompt":"PUBLIC hidden prompt","model":"PUBLIC-model"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    store.update(id, |_| Ok(Some(json!({"status":"running","lifecycle":{"processIdentity":owner,"workerPid":owner.pid,"workerProcessStartedAtMs":owner.started_at_ms}})))).unwrap();
    store
        .append_log(id, Some("PUBLIC-model"), "PUBLIC before attachment\n")
        .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .current_dir(home.path())
        .args(["session", id, "--hide-prompt"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut bytes = vec![];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !String::from_utf8_lossy(&bytes).contains("PUBLIC before attachment") {
            let mut chunk = [0; 1024];
            let n = output.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0, "Attachment exited before streaming its log");
            bytes.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .unwrap();
    assert!(child.try_wait().unwrap().is_none());
    store
        .append_log(id, Some("PUBLIC-model"), "\nPUBLIC completed café 🦀\n")
        .unwrap();
    store
        .update(id, |_| {
            Ok(Some(
                json!({"status":"error","errorMessage":"PUBLIC saved failure"}),
            ))
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), output.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(0)
    );
    let output = String::from_utf8(bytes).unwrap();
    for text in [
        "PUBLIC before attachment",
        "PUBLIC completed café 🦀",
        "PUBLIC saved failure",
    ] {
        assert_eq!(output.matches(text).count(), 1, "{output}");
    }
    assert!(!output.contains("PUBLIC hidden prompt"));
    assert_eq!(owner.liveness(), industrious::process::Liveness::Alive);
}
#[tokio::test]
async fn actual_cli_session_settles_a_dead_legacy_api_worker_without_resending() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC legacy worker","model":"PUBLIC-model"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let saved=store.update(id,|_|Ok(Some(json!({"status":"running","lifecycle":{"detached":true,"workerPid":u32::MAX},"dispatch":{"state":"issued","PUBLIC":"retain"},"response":{"id":"PUBLIC-remote-response","PUBLIC":"retain"}})))).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .current_dir(home.path())
            .args(["--session", id])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata = store.require(id).unwrap();
    assert_eq!(metadata["status"], "error");
    assert_eq!(metadata["models"][0]["status"], "error");
    assert_eq!(metadata["dispatch"], saved["dispatch"]);
    assert_eq!(metadata["response"]["id"], "PUBLIC-remote-response");
    assert_eq!(metadata["response"]["PUBLIC"], "retain");
    assert_eq!(
        metadata["response"]["incompleteReason"],
        "incomplete-capture"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn actual_cli_session_sigint_stops_only_the_observer() {
    use tokio::io::AsyncReadExt;
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let owner = industrious::process::ProcessIdentity::current().unwrap();
    let meta = store
        .create(
            json!({"prompt":"PUBLIC observer cancellation","model":"PUBLIC-model"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    store.update(id, |_| Ok(Some(json!({"status":"running","lifecycle":{"processIdentity":owner,"workerPid":owner.pid,"workerProcessStartedAtMs":owner.started_at_ms}})))).unwrap();
    store
        .append_log(id, Some("PUBLIC-model"), "PUBLIC observer ready\n")
        .unwrap();
    let before = fs::read(store.directory(id).unwrap().join("meta.json")).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .current_dir(home.path())
        .args(["status", id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = vec![];
        while !String::from_utf8_lossy(&bytes).contains("PUBLIC observer ready") {
            let mut chunk = [0; 1024];
            let n = stdout.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .unwrap();
    // The unreaped child reserves the exact observer PID until signal delivery.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) },
        0
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(130)
    );
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).await.unwrap();
    assert!(rest.ends_with("\nCancelled.\n"), "{rest:?}");
    assert_eq!(
        fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
        before
    );
    assert_eq!(owner.liveness(), industrious::process::Liveness::Alive);
}
#[test]
fn actual_cli_model_log_selection_validates_names_and_limits_fallback() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    for multi in [false, true] {
        let mut options = json!({"prompt":"PUBLIC model log selection","model":"Recorded/Model"});
        if multi {
            options["models"] = json!(["Recorded/Model", "Other/Model"]);
        }
        let meta = store.create(options, home.path(), None).unwrap();
        let id = meta["id"].as_str().unwrap();
        store
            .update(id, |_| Ok(Some(json!({"status":"completed"}))))
            .unwrap();
        store
            .append_log(id, None, "PUBLIC root-only fallback\n")
            .unwrap();
        let output = cli(home.path(), &["session", id, "--model", " recorded/MODEL "]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).contains("PUBLIC root-only fallback"),
            !multi
        );
        store
            .append_log(id, Some("Recorded/Model"), "PUBLIC selected answer\n")
            .unwrap();
        if multi {
            store
                .append_log(id, Some("Other/Model"), "PUBLIC unrelated model answer\n")
                .unwrap();
        }
        let output = cli(home.path(), &["session", id, "--model", " RECORDED/model "]);
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("=== Recorded/Model ===\nPUBLIC selected answer"));
        assert!(!text.contains("PUBLIC unrelated model answer"));
        assert!(!text.contains("PUBLIC root-only fallback"));
        let output = cli(home.path(), &["session", id, "--model", "Unknown/Model"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("not found in session"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PUBLIC model log selection"));
    }
}
#[test]
fn actual_cli_file_aliases_preserve_nested_braces_across_repeated_flags() {
    let home = tempfile::tempdir().unwrap();
    for directory in ["a", "b", "c"] {
        fs::create_dir(home.path().join(directory)).unwrap();
    }
    for file in ["a/x.ts", "b/y.ts", "c/z.ts"] {
        fs::write(home.path().join(file), format!("PUBLIC {file}\n")).unwrap();
    }
    for alias in ["--file", "--include", "--files", "--path", "--paths"] {
        let output = cli(
            home.path(),
            &[
                "--prompt",
                "PUBLIC brace parity",
                "--render",
                alias,
                "{a,{b,c}}/*.ts",
                alias,
                "a/x.ts",
            ],
        );
        assert!(
            output.status.success(),
            "{alias}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        for file in ["a/x.ts", "b/y.ts", "c/z.ts"] {
            assert_eq!(
                text.matches(&format!("### File: {file}")).count(),
                1,
                "{alias}: {text}"
            );
        }
    }
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn legacy_gemini_options_are_accepted_without_creating_a_session() {
    let home = tempfile::tempdir().unwrap();
    for engine in ["api", "browser"] {
        let output = cli(
            home.path(),
            &[
                "--prompt",
                "PUBLIC legacy Gemini options",
                "--engine",
                engine,
                "--youtube",
                "https://example.invalid/PUBLIC",
                "--edit-image",
                "PUBLIC.png",
                "--aspect",
                "16:9",
                "--gemini-show-thoughts",
                "--no-gemini-fallback",
                "--dry-run",
            ],
        );
        assert!(
            output.status.success(),
            "{engine}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn requested_unimplemented_flags_and_invalid_modes_fail_before_creating_a_session() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec![
            "--prompt",
            "PUBLIC complete consultation",
            "--render",
            "--verbose-render",
        ],
        vec![
            "--prompt",
            "PUBLIC complete consultation",
            "--reasoning-mode",
            "mystery",
        ],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(!home.path().join("sessions").exists());
    }
    assert!(
        cli(
            home.path(),
            &[
                "--prompt",
                "PUBLIC complete consultation",
                "--models",
                "Latest",
                "--dry-run",
                "json"
            ]
        )
        .status
        .success()
    );
    assert_eq!(
        cli(
            home.path(),
            &[
                "--prompt",
                "PUBLIC complete consultation",
                "--engine",
                "browser",
                "--models",
                "Latest",
                "--dry-run"
            ]
        )
        .status
        .code(),
        Some(1)
    );
}

#[test]
fn browser_fallback_accepts_only_none_or_grok_before_creating_a_session() {
    let home = tempfile::tempdir().unwrap();
    for (value, accepted) in [
        ("grok", true),
        ("none", true),
        ("Grok", false),
        ("xai", false),
    ] {
        let output = cli(
            home.path(),
            &[
                "--prompt",
                "PUBLIC provider consent",
                "--engine",
                "browser",
                "--model",
                "gpt-6-pro",
                "--browser-fallback",
                value,
                "--dry-run",
            ],
        );
        assert_eq!(
            output.status.success(),
            accepted,
            "{value}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!home.path().join("sessions").exists());
    }
}

#[test]
fn required_file_export_flags_keep_browser_and_answer_path_preflight_contracts() {
    let home = tempfile::tempdir().unwrap();
    for (args, message) in [
        (
            vec![
                "--prompt",
                "PUBLIC file export",
                "--engine",
                "api",
                "--write-artifacts",
                "--dry-run",
            ],
            "--write-artifacts requires --engine browser.",
        ),
        (
            vec![
                "--prompt",
                "PUBLIC file export",
                "--engine",
                "browser",
                "--write-artifacts",
                "--dry-run",
            ],
            "--write-artifacts requires --write-output <path>.",
        ),
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
    let output = cli(
        home.path(),
        &[
            "--prompt",
            "PUBLIC file export",
            "--engine",
            "browser",
            "--write-artifacts",
            "--write-output",
            "PUBLIC-answer.md",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join("sessions").exists());
    assert!(!home.path().join("PUBLIC-answer.md").exists());
}
#[test]
fn root_launch_flags_validate_before_creating_browser_or_session_state() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(
        home.path(),
        &[
            "-p",
            "PUBLIC launch flags",
            "--engine",
            "browser",
            "--browser-port",
            "9333",
            "--browser-url",
            "chatgpt.com/g/g-p-public/project",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for flags in [
        ["--browser-debug-port", "65536"],
        ["--chatgpt-url", "https://example.invalid/"],
    ] {
        let output = cli(
            home.path(),
            &[
                "-p",
                "PUBLIC launch flags",
                "--engine",
                "browser",
                flags[0],
                flags[1],
                "--dry-run",
            ],
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("is gated"));
    }
    assert!(!home.path().join("sessions").exists());
    assert!(!home.path().join("browser").exists());
}
#[test]
fn project_sources_legacy_url_preview_keeps_its_separate_navigation_contract() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("PUBLIC.rs"), b"PUBLIC source fixture").unwrap();
    let output = cli(
        home.path(),
        &[
            "project-sources",
            "add",
            "--chatgpt-url",
            "https://chat.openai.com/g/g-p-public/project",
            "--file",
            "PUBLIC.rs",
            "--json",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        preview["projectUrl"],
        "https://chat.openai.com/g/g-p-public/project?tab=sources"
    );
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn project_sources_cli_accepts_scoped_runtime_flags_and_prefers_the_child_profile() {
    let home = tempfile::tempdir().unwrap();
    storage::atomic_json(&home.path().join("config.json"),&json!({"browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("PUBLIC-ignored-profile"),"cookieSync":false}})).unwrap();
    for (index, flags) in [
        vec![
            "--browser-manual-login-profile-dir",
            "PUBLIC-root-profile",
            "--browser-chrome-path",
            "PUBLIC-missing-chrome",
            "project-sources",
        ],
        vec![
            "project-sources",
            "--browser-manual-login-profile-dir",
            "PUBLIC-parent-profile",
            "--browser-manual-login",
            "--browser-chrome-path",
            "PUBLIC-missing-chrome",
        ],
        vec![
            "--browser-manual-login-profile-dir",
            "PUBLIC-overridden-profile",
            "project-sources",
            "--browser-manual-login-profile-dir",
            "PUBLIC-preferred-profile",
        ],
        vec![
            "--browser-manual-login-profile-dir",
            "PUBLIC-child-overridden-root",
            "project-sources",
            "--browser-manual-login-profile-dir",
            "PUBLIC-child-overridden-parent",
            "--chatgpt-url",
            "https://chatgpt.com/g/g-p-public-parent/project",
            "list",
            "--browser-manual-login-profile-dir",
            "PUBLIC-child-profile",
            "--browser-manual-login",
            "--browser-chrome-path",
            "PUBLIC-missing-chrome",
            "--json",
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = flags;
        if index != 3 {
            args.push("list");
        }
        args.extend(["--chatgpt-url", "https://chatgpt.com/g/g-p-public/project"]);
        let result = cli(home.path(), &args);
        assert_eq!(
            result.status.code(),
            Some(1),
            "case{index}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("not initialized"), "case{index}: {error}");
        assert!(
            home.path()
                .join(
                    [
                        "PUBLIC-root-profile",
                        "PUBLIC-parent-profile",
                        "PUBLIC-preferred-profile",
                        "PUBLIC-child-profile"
                    ][index]
                )
                .is_dir()
        );
    }
    assert!(!home.path().join("PUBLIC-overridden-profile").exists());
    assert!(!home.path().join("PUBLIC-child-overridden-root").exists());
    assert!(!home.path().join("PUBLIC-child-overridden-parent").exists());
    assert!(!home.path().join("PUBLIC-ignored-profile").exists());
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn project_sources_cli_preview_keeps_parent_url_and_json_options() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("public.rs"), b"PUBLIC").unwrap();
    let output = cli(
        home.path(),
        &[
            "project-sources",
            "add",
            "--chatgpt-url",
            "https://chatgpt.com/g/g-p-public/project",
            "--json",
            "--file",
            "public.rs",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "dry-run");
    assert_eq!(value["plannedUploads"][0]["name"], "public.rs");
    assert!(value["projectUrl"].as_str().unwrap().contains("g-p-public"));
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn live_tail_of_a_completed_session_prints_the_saved_answer_without_a_browser() {
    // The owner retires its tab as soon as it captures the answer, so a tail that outlives the run
    // must read the saved answer instead of inspecting a closing target.
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC source","model":"gpt-6-pro","engine":"browser"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap().to_owned();
    let receipt = industrious::artifacts::capture(
        &store,
        &id,
        "public-answer.md",
        b"PUBLIC completed answer",
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    store
        .update(&id, |_| {
            Ok(Some(
                json!({"status":"completed","mode":"browser","browser":{"capturedAnswer":receipt}}),
            ))
        })
        .unwrap();
    let before = store.require(&id).unwrap();
    let output = cli(
        home.path(),
        &["session", &id, "--live", "--write-output", "-"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("already completed"), "{stdout}");
    assert_eq!(
        stdout.matches("PUBLIC completed answer").count(),
        1,
        "{stdout}"
    );
    assert_eq!(store.require(&id).unwrap(), before);
    // Pending recovery or target disposition still needs the live path to finalise it.
    for pending in ["recoveryRequired", "targetDispositionPending"] {
        let set = |value: bool| {
            store
                .update(&id, |current| {
                    let mut patch = json!({});
                    if pending == "recoveryRequired" {
                        patch["recoveryRequired"] = json!(value);
                    } else {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["targetDispositionPending"] = json!(value);
                        patch["browser"] = browser;
                    }
                    Ok(Some(patch))
                })
                .unwrap();
        };
        set(true);
        assert!(!store.require(&id).unwrap()["browser"]["capturedAnswer"].is_null());
        let output = cli(
            home.path(),
            &[
                "session",
                &id,
                "--live",
                "--no-recover",
                "--write-output",
                "-",
            ],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("already completed"), "{pending}: {stdout}");
        assert!(
            !stdout.contains("PUBLIC completed answer"),
            "{pending}: {stdout}"
        );
        set(false);
    }
}

#[test]
fn inspection_cli_rejects_empty_references_before_reading_browser_or_publishing() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC inspection boundary","model":"gpt-6-pro"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let before = store.require(id).unwrap();
    for mode in ["--harvest", "--live"] {
        for reference in ["", " ", "\u{2003}"] {
            let output = cli(
                home.path(),
                &[
                    "session",
                    id,
                    mode,
                    "--browser-tab",
                    reference,
                    "--no-recover",
                    "--write-output",
                    "PUBLIC-refused.md",
                ],
            );
            assert_eq!(output.status.code(), Some(1));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("reference must not be empty")
            );
            assert!(output.stdout.is_empty());
            assert_eq!(store.require(id).unwrap(), before);
            assert!(!home.path().join("PUBLIC-refused.md").exists());
        }
    }
}

#[tokio::test]
async fn inspection_cli_settles_absent_completed_target_before_live_or_harvest() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    for mode in ["--harvest", "--live"] {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("AppData/Local")).unwrap();
        let store = SessionStore::new(home.path());
        let meta = store
            .create(
                json!({"prompt":"PUBLIC source","model":"gpt-6-pro","engine":"browser"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        let receipt = industrious::artifacts::capture(
            &store,
            &id,
            "public-answer.md",
            b"PUBLIC completed answer",
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        store
            .update_model(&id, "gpt-6-pro", &json!({"status":"completed"}))
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-original");
        let target = json!({"host":"127.0.0.1","port":port,"targetId":"PUBLIC-owned","browserWSEndpoint":endpoint,"claimId":uuid::Uuid::new_v4().to_string(),"conversationId":"public-conversation"});
        let completed = industrious::sessions::timestamp();
        let attempt = json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"binding":{"target":target,"capture":receipt,"completedAt":completed}});
        store.update(&id,|_| Ok(Some(json!({"status":"completed","mode":"browser","completedAt":completed,"recoveryRequired":true,
            "browser":{"config":{"keepBrowser":false},"capturedAnswer":receipt,"runtime":{
                "chromeHost":"127.0.0.1","chromePort":port,"chromeBrowserWSEndpoint":endpoint,"chromeTargetId":"PUBLIC-owned",
                "conversationId":"public-conversation","tabUrl":"https://chatgpt.com/c/public-conversation",
                "controllerPid":u32::MAX,"controllerProcessStartedAtMs":1.0,"ownedRecoveryTarget":target,
                "creatorlessRetirement":attempt,"targetDispositionPending":true}}})))).unwrap();
        let before = store.require(&id).unwrap();
        let server = tokio::spawn(async move {
            let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
            let Ok(Ok((socket, _))) = accepted else {
                return 0;
            };
            let mut peer = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut seen = 0;
            while let Some(Ok(Message::Text(frame))) = peer.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(request["method"], "Target.getTargets");
                seen += 1;
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":{"targetInfos":[]}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
            seen
        });
        let directory = home.path().to_owned();
        let session = id.clone();
        let output = tokio::task::spawn_blocking(move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_industrious"));
            command
                .env_clear()
                .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
                .env("HOME", &directory)
                .env("USERPROFILE", &directory)
                .env("ORACLE_HOME_DIR", &directory)
                .env("LOCALAPPDATA", directory.join("AppData/Local"))
                .current_dir(&directory)
                .args(["session", &session, mode, "--write-output", "-"]);
            if let Ok(root) = std::env::var("SystemRoot") {
                command.env("SystemRoot", root);
            }
            command.output().unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(server.await.unwrap(), 1);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("its original browser target is absent"),
            "{stdout}"
        );
        assert_eq!(
            stdout.matches("PUBLIC completed answer").count(),
            1,
            "{stdout}"
        );
        let saved = store.require(&id).unwrap();
        assert_eq!(saved["status"], "completed");
        assert_eq!(
            saved["browser"]["capturedAnswer"],
            before["browser"]["capturedAnswer"]
        );
        assert_eq!(saved["recoveryRequired"], false);
        assert_eq!(
            saved["browser"]["runtime"]["targetDispositionPending"],
            false
        );
    }
}

#[test]
fn runtime_browser_flags_reach_the_saved_worker_before_isolated_profile_preflight() {
    let home = tempfile::tempdir().unwrap();
    storage::atomic_json(&home.path().join("config.json"),&json!({"engine":"browser","browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("PUBLIC empty profile"),"cookieSync":false,"chromePath":"PUBLIC configured.exe","timeoutMs":17,"inputTimeoutMs":18,"maxConcurrentTabs":2}})).unwrap();
    let output = cli(
        home.path(),
        &[
            "--engine",
            "browser",
            "--prompt",
            "PUBLIC runtime bindings",
            "--wait",
            "--browser-chrome-path",
            "PUBLIC CLI.exe",
            "--browser-timeout",
            "2s",
            "--browser-input-timeout",
            "500ms",
            "--browser-max-concurrent-tabs",
            "+5tabs",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("ChatGPT browser manual-login profile is not initialized")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Reattach via: industrious session "));
    let store = SessionStore::new(home.path());
    let sessions = store.list(true).unwrap();
    assert_eq!(sessions.len(), 2);
    let saved = public_session(&store);
    assert_eq!(saved["lifecycle"]["detached"], true);
    assert_eq!(saved["status"], "error");
    let browser = &saved["browser"]["config"];
    assert_eq!(browser["chromePath"], "PUBLIC CLI.exe");
    assert_eq!(browser["timeoutMs"], 2000);
    assert_eq!(browser["inputTimeoutMs"], 500);
    assert_eq!(browser["maxConcurrentTabs"], 5);
    assert_eq!(saved["options"]["browserConfig"], *browser);
    assert!(saved["browser"]["runtime"].get("chromeTargetId").is_none());
    let child = provider_child(&store, &saved);
    assert_eq!(
        child["browser"]["config"]["chromePath"],
        browser["chromePath"]
    );
    assert!(child["browser"]["runtime"]["chromeTargetId"].is_null());
}

#[tokio::test]
async fn runtime_remote_chrome_flag_probes_its_explicit_endpoint_and_clears_stale_socket() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    for attach in [false, true] {
        let home = tempfile::tempdir().unwrap();
        storage::atomic_json(&home.path().join("config.json"),&json!({"engine":"browser","browser":{"manualLogin":true,"cookieSync":false,"manualLoginProfileDir":home.path().join("PUBLIC isolated remote profile"),"remoteChromeBrowserWSEndpoint":"ws://127.0.0.1:9/devtools/browser/PUBLIC-stale"}})).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..if attach { 2 } else { 1 } {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = [0u8; 4096];
                let count = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    String::from_utf8_lossy(&bytes[..count])
                        .starts_with("GET /json/version HTTP/1.1\r\n")
                );
                socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
        });
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
        command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
        if attach {
            command.arg("--browser-attach-running");
        }
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            command.env("SystemRoot", root);
        }
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            command
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("ORACLE_HOME_DIR", home.path())
                .current_dir(home.path())
                .args([
                    "--engine",
                    "browser",
                    "--prompt",
                    "PUBLIC explicit endpoint",
                    "--wait",
                    "--remote-chrome",
                    &address.to_string(),
                    "--browser-input-timeout",
                    "1s",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        let store = SessionStore::new(home.path());
        let sessions = store.list(true).unwrap();
        let saved = public_session(&store);
        assert_eq!(
            saved["browser"]["config"]["remoteChrome"]["port"],
            address.port()
        );
        let received = server.await;
        assert!(
            received.is_ok(),
            "Fixture endpoint was not probed; CLI stderr: {}; stdout: {}; saved browser: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
            json!(
                sessions
                    .iter()
                    .map(|session| &session["browser"])
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(sessions.len(), 2);
        let config = &saved["browser"]["config"];
        assert_eq!(
            config["remoteChrome"],
            json!({"host":"127.0.0.1","port":address.port()})
        );
        assert!(config["remoteChromeBrowserWSEndpoint"].is_null());
        if attach {
            assert_eq!(config["attachRunning"], true);
        }
        assert_eq!(saved["status"], "error");
        let child = provider_child(&store, &saved);
        assert_eq!(
            child["browser"]["config"]["remoteChrome"],
            config["remoteChrome"]
        );
        assert!(child["browser"]["runtime"]["chromeTargetId"].is_null());
        assert!(saved["browser"]["runtime"].get("chromeTargetId").is_none());
    }
}

#[cfg(windows)]
#[test]
fn windows_entries_reserve_the_linux_main_thread_stack() {
    // PE32+ optional header: SizeOfStackReserve is the u64 at offset 72.
    for binary in [
        env!("CARGO_BIN_EXE_industrious"),
        env!("CARGO_BIN_EXE_industrious-mcp"),
    ] {
        let bytes = fs::read(binary).unwrap();
        let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "{binary}");
        let optional = pe + 24;
        assert_eq!(
            &bytes[optional..optional + 2],
            &0x20b_u16.to_le_bytes(),
            "{binary}"
        );
        let reserve = u64::from_le_bytes(bytes[optional + 72..optional + 80].try_into().unwrap());
        assert!(reserve >= 8 << 20, "{binary} reserves {reserve} bytes");
    }
}

#[tokio::test]
async fn background_cli_releases_its_callers_pipes_while_its_worker_runs() {
    use tokio::io::AsyncReadExt;
    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut request = vec![];
        let mut buffer = [0; 4096];
        loop {
            let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                    .unwrap_or(0usize);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        // The provider holds its answer until the caller has seen the CLI's output end.
        tokio::time::timeout(Duration::from_secs(20), released)
            .await
            .unwrap()
            .unwrap();
        let response = [
            json!({"type":"response.created","response":{"id":"resp_PUBLIC_held","status":"in_progress","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_PUBLIC_held","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"item_id":"msg_PUBLIC_held","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_PUBLIC_held","content_index":0,"delta":"PUBLIC held answer"}),
            json!({"type":"response.completed","response":{"id":"resp_PUBLIC_held","status":"completed","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[{"type":"message","id":"msg_PUBLIC_held","content":[{"type":"output_text","text":"PUBLIC held answer","annotations":[]}]}]}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let store = SessionStore::new(home.path().join("oracle"));
    store.create(json!({"prompt":"PUBLIC held background request","model":"gpt-6-astra","mode":"api","reasoningMode":"pro","provider":"azure","azure":{"endpoint":format!("http://{address}"),"deployment":"PUBLIC-held-deployment"},"background":false,"waitPreference":true}),home.path(),Some("held")).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
    command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        command.env("SystemRoot", root);
    }
    // output() reads both pipes to their end, so it returns only when no worker holds them.
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", store.home())
            .env("OPENAI_API_KEY", "PUBLIC-synthetic-key")
            .current_dir(home.path())
            .args(["restart", "held", "--no-wait"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("A background worker kept its caller's pipes open")
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "Session: held-2\nMode: api background\nModels: 1\nDetach: yes, polling\nReattach: industrious session held-2\n"
    ));
    assert!(!industrious::sessions::terminal(
        store.require("held-2").unwrap()["status"]
            .as_str()
            .unwrap_or("")
    ));
    release.send(()).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let restarted = store.require("held-2").unwrap();
        if industrious::sessions::terminal(restarted["status"].as_str().unwrap_or("")) {
            assert_eq!(
                restarted["status"], "completed",
                "{}",
                restarted["errorMessage"]
            );
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.await.unwrap();
}

#[test]
fn azure_api_version_is_accepted_and_route_plans_merge_user_config_by_field() {
    let home = tempfile::tempdir().unwrap();
    storage::atomic_json(
        &home.path().join("config.json"),
        &json!({"azure":{"endpoint":"https://public-azure.invalid","deployment":"PUBLIC-config-deployment"}}),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_industrious"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .env("AZURE_OPENAI_API_KEY", "PUBLIC-SYNTHETIC-KEY")
            .current_dir(home.path())
            .args(args)
            .output()
            .unwrap()
    };
    for args in [
        vec![
            "--prompt",
            "PUBLIC Azure version",
            "--azure-api-version",
            "2025-04-01-preview",
            "--route",
        ],
        vec![
            "doctor",
            "--providers",
            "--json",
            "--azure-api-version",
            "2025-04-01-preview",
        ],
    ] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args[0] == "doctor" {
            let plans: Value = serde_json::from_slice(&output.stdout).unwrap();
            let plan = &plans["providers"][0];
            assert_eq!(plan["providerLabel"], "Azure OpenAI", "{args:?}");
            assert_eq!(plan["keySource"], "AZURE_OPENAI_API_KEY", "{args:?}");
        } else {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.starts_with("Route plan\n\ngpt-6-astra: ok\n"),
                "{text}"
            );
            assert!(text.contains("\n  provider: Azure OpenAI\n"), "{text}");
            assert!(text.contains("\n  key: AZURE_OPENAI_API_KEY="), "{text}");
        }
        assert!(
            !output
                .stdout
                .windows(20)
                .any(|w| w == b"PUBLIC-SYNTHETIC-KEY")
        );
    }
    let output = run(&[
        "--prompt",
        "PUBLIC Azure version",
        "--no-azure",
        "--azure-api-version",
        "2025-04-01-preview",
        "--route",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be combined with Azure options")
    );
    // A config-only Azure endpoint selects the API unless a remote browser host keeps the browser.
    for (extra, preview) in [
        (vec![], "would call gpt-6-astra"),
        (
            vec![
                "--remote-host",
                "127.0.0.1:9",
                "--remote-token",
                "PUBLIC-token",
            ],
            "browser mode",
        ),
    ] {
        let mut args = vec!["--prompt", "PUBLIC Azure engine", "--dry-run"];
        args.extend(extra);
        let output = run(&args);
        assert!(output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(preview),
            "{args:?}"
        );
    }
    assert!(!home.path().join("sessions").exists());
}

/// One inline Azure API run against a streaming synthetic provider; returns stdout.
async fn foreground_api_run(extra: &[&str], usage: Value) -> String {
    api_run(extra, usage, false).await
}
/// The same run, inline or detached. Detached, it waits for the worker to finish and exit and
/// returns the session log instead of stdout.
async fn api_run(extra: &[&str], usage: Value, detached: bool) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("PUBLIC-source.rs"), "fn public() {}\n").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut request = vec![];
        let mut buffer = [0; 4096];
        loop {
            let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                    .unwrap_or(0usize);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        // Silence before the first delta lets the one-second heartbeat report the connection.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let response = [
            json!({"type":"response.created","response":{"id":"resp_PUBLIC_beat","status":"in_progress","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_PUBLIC_beat","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"item_id":"msg_PUBLIC_beat","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_PUBLIC_beat","content_index":0,"delta":"PUBLIC beat answer"}),
            json!({"type":"response.completed","response":{"id":"resp_PUBLIC_beat","status":"completed","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[{"type":"message","id":"msg_PUBLIC_beat","content":[{"type":"output_text","text":"PUBLIC beat answer","annotations":[]}]}],"usage":usage}}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
    command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        command.env("SystemRoot", root);
    }
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .env("AZURE_OPENAI_API_KEY", "PUBLIC-synthetic-key")
            .current_dir(home.path())
            .args([
                "--prompt",
                "PUBLIC heartbeat consultation",
                "--file",
                "PUBLIC-source.rs",
                "--files-report",
                "--heartbeat",
                "1",
                "--no-background",
                if detached { "--no-wait" } else { "--wait" },
                "--azure-endpoint",
                &format!("http://{address}"),
                "--azure-deployment",
                "PUBLIC-deployment",
            ])
            .args(extra)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stdout.contains("PUBLIC-synthetic-key"));
    if !detached {
        server.await.unwrap();
        // Reference combinedLog: the inline run's session log holds what stdout showed.
        let store = SessionStore::new(home.path().to_path_buf());
        let id = store.list(false).unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // Reference runInteractiveSession prints and logs the lifecycle block first.
        assert!(
            stdout.starts_with(&format!("Session: {id}\nMode: api foreground\nModels: 1\nDetach: no\nReattach: industrious session {id}\n")),
            "{stdout}"
        );
        assert_eq!(
            store.log(&id).unwrap(),
            format!("=== gpt-6-astra ===\n{}", stdout.trim_end()),
            "{stdout}"
        );
        return stdout;
    }
    assert!(
        stdout.contains("Mode: api background\nModels: 1\nDetach: yes, polling\n"),
        "{stdout}"
    );
    let store = SessionStore::new(home.path().to_path_buf());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let (id, finished) = loop {
        let sessions = store.list(false).unwrap();
        if let Some(meta) = sessions.first()
            && industrious::sessions::terminal(meta["status"].as_str().unwrap_or(""))
        {
            break (meta["id"].as_str().unwrap().to_owned(), meta.clone());
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        finished["status"],
        "completed",
        "{}",
        store.log(&id).unwrap()
    );
    let worker: industrious::process::ProcessIdentity =
        serde_json::from_value(finished["lifecycle"]["processIdentity"].clone()).unwrap();
    while worker.liveness() != industrious::process::Liveness::Dead {
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker did not exit"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.await.unwrap();
    let log = store.log(&id).unwrap();
    assert!(!log.contains("PUBLIC-synthetic-key"), "{log}");
    log
}
#[tokio::test]
async fn detached_api_run_logs_the_reference_header_answer_heading_and_finish_lines() {
    // Reference executeSession gives runOracle the session log writer, so the worker's log
    // reads like a foreground run's stdout.
    let log = api_run(&[], json!({}), true).await;
    let at = |text: &str| {
        log.find(text)
            .unwrap_or_else(|| panic!("{text:?} missing from {log}"))
    };
    let calling = at("Calling gpt-6-astra (API: PUBLIC-deployment) \u{2014} ");
    let provider = at("Provider: Azure OpenAI | endpoint: ");
    let report = at("File Token Usage");
    let beat = at("API connection active \u{2014} 1s elapsed. Timeout in ~");
    let answer = at("\n\nAnswer:\nPUBLIC beat answer");
    assert!(
        calling < provider && provider < report && report < beat && beat < answer,
        "{log}"
    );
    assert_eq!(log.matches("Answer:").count(), 1, "{log}");
    assert!(!log.contains("[verbose]"), "{log}");
    let tail = finish_lines(&log);
    assert_eq!(tail.len(), 3, "{log}");
    assert_eq!(tail[0], "");
    let summary: Vec<_> = tail[1].split(" \u{b7} ").collect();
    assert_eq!(summary[2], "gpt-6-astra[pro/xhigh]", "{log}");
    assert!(summary[4].starts_with("sid="), "{log}");
    assert_eq!(tail[2], "files=1");
}
#[tokio::test]
async fn detached_verbose_api_run_logs_the_status_line_before_the_finish_lines() {
    let log = api_run(
        &["--verbose"],
        json!({"input_tokens":1234,"output_tokens":56,"total_tokens":1290,"output_tokens_details":{"reasoning_tokens":7}}),
        true,
    )
    .await;
    assert!(
        log.contains("\n[verbose] Dispatching request to API...\n\n"),
        "{log}"
    );
    let tail = finish_lines(&log);
    assert_eq!(tail.len(), 4, "{log}");
    assert_eq!(tail[..2], ["[verbose] Response status: completed", ""]);
    assert!(tail[3].starts_with("est\u{2192}actual="), "{log}");
}
fn finish_lines(stdout: &str) -> Vec<&str> {
    let tail = stdout
        .rsplit_once("PUBLIC beat answer\n")
        .unwrap_or_else(|| panic!("answer missing from {stdout}"))
        .1;
    tail.lines().collect()
}
#[tokio::test]
async fn foreground_api_run_prints_the_reference_header_heartbeat_and_file_report() {
    let stdout = foreground_api_run(&[], json!({})).await;
    let address = stdout
        .split("endpoint: ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .to_owned();
    let at = |text: &str| {
        stdout
            .find(text)
            .unwrap_or_else(|| panic!("{text:?} missing from {stdout}"))
    };
    let calling = at("Calling gpt-6-astra (API: PUBLIC-deployment) \u{2014} ");
    let provider = at(&format!(
        "Provider: Azure OpenAI | endpoint: {address} | deployment: PUBLIC-deployment | key: AZURE_OPENAI_API_KEY"
    ));
    let report = at("File Token Usage");
    let beat = at("API connection active \u{2014} 1s elapsed. Timeout in ~");
    let answer = at("PUBLIC beat answer");
    assert!(
        calling < provider && provider < report && report < beat && beat < answer,
        "{stdout}"
    );
    assert!(
        !stdout[answer..].contains("API connection active"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n\nAnswer:\nPUBLIC beat answer\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("[verbose]"), "{stdout}");
    // Unreported usage counts carry the reference "*" and the run has one file.
    let tail = finish_lines(&stdout);
    assert_eq!(tail.len(), 3, "{stdout}");
    assert_eq!(tail[0], "");
    let summary: Vec<_> = tail[1].split(" \u{b7} ").collect();
    assert!(summary[0].ends_with('s'), "{stdout}");
    assert_eq!(summary[2], "gpt-6-astra[pro/xhigh]");
    assert!(
        summary[3].starts_with('\u{2191}') && summary[3].contains("\u{2193}0* \u{21bb}0* \u{394}"),
        "{stdout}"
    );
    assert!(summary[4].starts_with("sid="), "{stdout}");
    assert_eq!(tail[2], "files=1");
}
#[tokio::test]
async fn verbose_api_run_prints_the_reference_verbose_lines_and_finish_details() {
    let stdout = foreground_api_run(
        &["--verbose"],
        json!({"input_tokens":1234,"output_tokens":56,"total_tokens":1290,"output_tokens_details":{"reasoning_tokens":7}}),
    )
    .await;
    let at = |text: &str| {
        stdout
            .find(text)
            .unwrap_or_else(|| panic!("{text:?} missing from {stdout}"))
    };
    let provider = at("Provider: Azure OpenAI | endpoint: ");
    // Reference maskApiKey: four leading and trailing characters only.
    let key = at(
        "\nUsing AZURE_OPENAI_API_KEY=PUBL****-key for model gpt-6-astra (API: PUBLIC-deployment)\n",
    );
    let estimated = at("\n[verbose] Estimated tokens (request body): ");
    let dispatch = at("\n[verbose] Dispatching request to API...\n\n");
    let answer = at("\n\nAnswer:\nPUBLIC beat answer\n");
    assert!(
        provider < key && key < estimated && estimated < dispatch && dispatch < answer,
        "{stdout}"
    );
    let tail = finish_lines(&stdout);
    assert_eq!(tail.len(), 4, "{stdout}");
    assert_eq!(tail[..2], ["[verbose] Response status: completed", ""]);
    let summary: Vec<_> = tail[2].split(" \u{b7} ").collect();
    // Synthetic usage at the reference rates: 1234 * $10/M + 56 * $50/M.
    assert_eq!(
        summary[1..],
        [
            "$0.0151",
            "gpt-6-astra[pro/xhigh]",
            "\u{2191}1.23k \u{2193}56 \u{21bb}7 \u{394}1.29k",
            summary[4],
        ],
        "{stdout}"
    );
    let estimate = stdout[estimated..]
        .split_once(": ")
        .unwrap()
        .1
        .lines()
        .next()
        .unwrap()
        .replace(',', "")
        .parse::<i64>()
        .unwrap();
    let delta = 1234 - estimate;
    let grouped = |value: i64| match value.abs() {
        a if a >= 1000 => format!("{},{:03}", value / 1000, a % 1000),
        _ => value.to_string(),
    };
    assert_eq!(
        tail[3],
        format!(
            "est\u{2192}actual={}\u{2192}1,234{} | files=1",
            grouped(estimate),
            match delta {
                0 => String::new(),
                d if d > 0 => format!(" (+{})", grouped(d)),
                d => format!(" ({})", grouped(d)),
            }
        ),
        "{stdout}"
    );
}
#[test]
fn verbose_is_accepted_for_browser_runs() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(
        home.path(),
        &[
            "--prompt",
            "PUBLIC verbose browser",
            "--engine",
            "browser",
            "--verbose",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("verbose logging is gated"));
}
#[test]
fn partial_modes_are_validated_and_saved_for_single_model_runs() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["--allow-partial"],
        vec!["--partial", "ok"],
        vec!["--partial", "fail"],
    ] {
        let mut full = vec![
            "--prompt",
            "PUBLIC partial mode",
            "--engine",
            "api",
            "--dry-run",
        ];
        full.extend(args.iter().copied());
        assert!(cli(home.path(), &full).status.success(), "{args:?}");
    }
    let output = cli(
        home.path(),
        &[
            "--prompt",
            "PUBLIC partial mode",
            "--partial",
            "maybe",
            "--dry-run",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Allowed choices are fail, ok."));
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn browser_choice_options_refuse_unknown_values_at_parse_time() {
    let home = tempfile::tempdir().unwrap();
    for (flag, spelling, allowed) in [
        (
            "--browser-archive",
            "--browser-archive <mode>",
            "auto, always, never",
        ),
        (
            "--browser-research",
            "--browser-research <mode>",
            "off, search, deep",
        ),
        (
            "--browser-model-strategy",
            "--browser-model-strategy <mode>",
            "select, current, ignore",
        ),
        (
            "--browser-attachments",
            "--browser-attachments <mode>",
            "auto, never, always",
        ),
        (
            "--browser-bundle-format",
            "--browser-bundle-format <format>",
            "auto, text, zip",
        ),
        ("--mode", "--mode <mode>", "api, browser"),
    ] {
        let output = cli(
            home.path(),
            &[
                "--prompt",
                "PUBLIC choice validation",
                flag,
                "bogus",
                "--dry-run",
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{flag}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!(
                "error: option '{spelling}' argument 'bogus' is invalid. Allowed choices are {allowed}.\n(use --help for usage)\n"
            ),
            "{flag}"
        );
    }
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn browser_tab_requires_the_browser_engine_and_previews_the_reused_tab() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(
        home.path(),
        &[
            "--prompt",
            "PUBLIC borrowed tab check",
            "--engine",
            "api",
            "--browser-tab",
            "current",
            "--dry-run",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--browser-tab requires --engine browser.")
    );
    let output = cli(
        home.path(),
        &[
            "--prompt",
            "PUBLIC borrowed tab check",
            "--engine",
            "browser",
            "--browser-attach-running",
            "--browser-tab",
            "PUBLIC-target",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "Industrious reuses the matching ChatGPT tab (PUBLIC-target) and leaves the existing browser process alone."
    ));
    assert!(!home.path().join("sessions").exists());
}
#[tokio::test]
async fn api_write_output_reports_the_saved_answer_inline_and_detached() {
    let extra = ["--write-output", "PUBLIC-answer.md"];
    let stdout = foreground_api_run(&extra, json!({})).await;
    let last = stdout.lines().last().unwrap_or("");
    assert!(
        last.starts_with("Saved assistant output to ") && last.ends_with("PUBLIC-answer.md"),
        "{stdout}"
    );
    let log = api_run(&extra, json!({}), true).await;
    let last = log.trim_end().lines().last().unwrap_or("");
    assert!(
        last.starts_with("Saved assistant output to ") && last.ends_with("PUBLIC-answer.md"),
        "{log}"
    );
}
#[tokio::test]
async fn failed_api_runs_log_the_reference_error_line_once() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for detached in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut buffer = [0; 4096];
            let _ = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer)).await;
            let body =
                r#"{"error":{"message":"PUBLIC rejected request","type":"invalid_request_error"}}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
        command.env_clear().env("ORACLE_DISABLE_NOTIFICATIONS", "1");
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            command.env("SystemRoot", root);
        }
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            command
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("ORACLE_HOME_DIR", home.path())
                .env("AZURE_OPENAI_API_KEY", "PUBLIC-synthetic-key")
                .current_dir(home.path())
                .args([
                    "--prompt",
                    "PUBLIC failing consultation",
                    "--no-background",
                    if detached { "--no-wait" } else { "--wait" },
                    "--azure-endpoint",
                    &format!("http://{address}"),
                    "--azure-deployment",
                    "PUBLIC-deployment",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let store = SessionStore::new(home.path().to_path_buf());
        let id = store.list(false).unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let log = if detached {
            assert_eq!(output.status.code(), Some(0), "{stdout}{stderr}");
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let log = store.log(&id).unwrap();
                if log.contains("ERROR: ") || std::time::Instant::now() > deadline {
                    break log;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else {
            assert_eq!(output.status.code(), Some(1), "{stdout}{stderr}");
            assert!(!stderr.contains('\u{2716}'), "{stderr}");
            store.log(&id).unwrap()
        };
        server.await.unwrap();
        let line = log
            .lines()
            .find(|line| line.starts_with("ERROR: "))
            .unwrap_or_else(|| panic!("{log}"));
        assert!(line.contains("PUBLIC rejected request"), "{log}");
        assert_eq!(log.matches("ERROR: ").count(), 1, "{log}");
        assert!(!log.contains("may still be alive"), "{log}");
        if !detached {
            assert_eq!(stdout.lines().last(), Some(line), "{stdout}");
        }
    }
}

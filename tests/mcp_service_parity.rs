//! MCP service parity with the reference server: synthetic sessions, local stubs and no real
//! credentials or provider requests.
use alcuin::{
    config::Environment,
    execution,
    mcp::{AlcuinMcp, add_summaries, log_tail},
    sessions::SessionStore,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn server(home: &Path, mut env: Environment) -> AlcuinMcp {
    // The user home bounds the project-config walk, so no real configuration is read.
    for key in ["HOME", "USERPROFILE"] {
        env.entry(key.into())
            .or_insert_with(|| home.to_string_lossy().into_owned());
    }
    AlcuinMcp {
        store: SessionStore::new(home),
        cwd: home.into(),
        env,
    }
}
fn text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}
async fn call(server: &AlcuinMcp, name: &str, input: Value) -> anyhow::Result<String> {
    server
        .invoke(name, input, &CancellationToken::new())
        .await
        .map(|result| text(&result))
}
/// An environment in which no Chrome installation can be found.
fn chromeless(home: &Path) -> Environment {
    let empty = home.join("PUBLIC-no-programs");
    std::fs::create_dir_all(&empty).unwrap();
    let path = empty.to_string_lossy().into_owned();
    Environment::from([
        ("ProgramFiles".into(), path.clone()),
        ("ProgramFiles(x86)".into(), path.clone()),
        ("LOCALAPPDATA".into(), path.clone()),
        ("HOME".into(), path.clone()),
        ("PATH".into(), path),
    ])
}
/// Serves canned HTTP replies, one per connection, and records each request.
fn stub(replies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let record = seen.clone();
    std::thread::spawn(move || {
        for (status, body) in replies {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut request = vec![];
            let mut buffer = [0; 4096];
            loop {
                let count = socket.read(&mut buffer).unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            record
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into_owned());
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 {status} PUBLIC\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (address, seen)
}
fn azure(endpoint: &str) -> Environment {
    Environment::from([
        ("AZURE_OPENAI_ENDPOINT".into(), endpoint.into()),
        ("AZURE_OPENAI_DEPLOYMENT".into(), "PUBLIC-deployment".into()),
        ("AZURE_OPENAI_API_KEY".into(), "PUBLIC-SYNTHETIC-KEY".into()),
        ("ORACLE_DISABLE_NOTIFICATIONS".into(), "1".into()),
    ])
}
const PROMPT: &str = "PUBLIC synthetic consultation prompt for parity";

#[test]
fn startup_failure_uses_the_reference_prefix_and_exit_code() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_alcuin-mcp"))
        .env_clear()
        .current_dir(home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("oracle-mcp exited with an error: "),
        "{stderr}"
    );
    assert_eq!(output.status.code(), Some(1));
}

#[tokio::test]
async fn dry_runs_print_the_reference_summary_resolved_lines_and_guidance() {
    let home = tempfile::tempdir().unwrap();
    let config = json!({"browser":{"manualLogin":true,"manualLoginProfileDir":"PUBLIC-profile","chatgptUrl":"https://chatgpt.com/g/g-p-PUBLIC/project"}});
    let input = json!({"prompt":PROMPT,"engine":"browser","browserFollowUps":["PUBLIC follow-up"]});
    let request = execution::resolve(&input, &config, &Environment::new(), home.path()).unwrap();
    let resolved = execution::dry_run_resolved(&request);
    let lines = execution::dry_run_lines(&resolved);
    for expected in [
        "  browser manual login: yes",
        "  browser profile: PUBLIC-profile",
        "  ChatGPT URL: https://chatgpt.com/g/g-p-PUBLIC/project",
        "  browser attachments: auto",
        "  browser bundle files: no",
        "  follow-ups: 1",
        "  guidance: Manual-login browser mode uses Alcuin's private Chrome profile at PUBLIC-profile, separate from your normal Chrome profile.",
        "  guidance: First-time setup: run alcuin --engine browser --browser-manual-login --browser-keep-browser --browser-manual-login-profile-dir \"PUBLIC-profile\" -p \"HI\", sign into ChatGPT in that window, then retry the consult.",
        "  guidance: This ChatGPT project URL is persistent. Project Sources should be mutated only by the project_sources tool with confirmMutation:true.",
        "  guidance: This is a multi-turn browser consult; all follow-ups stay in one ChatGPT conversation.",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "{expected}\n{lines:#?}"
        );
    }
    let server = server(home.path(), Environment::new());
    let browser = call(
        &server,
        "consult",
        json!({"prompt":PROMPT,"engine":"browser","dryRun":true}),
    )
    .await
    .unwrap();
    assert!(
        browser.starts_with("[dry-run] Alcuin (")
            && browser.contains(") would launch browser mode ("),
        "{browser}"
    );
    assert!(browser.contains("\n[dry-run] No files attached.\n[dry-run] MCP resolved request:\n"));
    assert!(
        !browser.contains("[preview]") && !browser.contains(PROMPT),
        "{browser}"
    );
    let api = call(
        &server,
        "consult",
        json!({"prompt":PROMPT,"engine":"api","dryRun":true}),
    )
    .await
    .unwrap();
    assert!(
        api.starts_with("[dry-run] Alcuin (") && api.contains(" would call gpt-6-astra with ~"),
        "{api}"
    );
    assert!(api.contains("[dry-run] No files matched the provided --file patterns.\n[dry-run] MCP resolved request:\n"));
}

#[tokio::test]
async fn image_default_path_validation_and_engine_texts_match_the_reference() {
    let home = tempfile::tempdir().unwrap();
    let server = server(home.path(), Environment::new());
    let image = server
        .invoke(
            "chatgpt_image",
            json!({"prompt":PROMPT,"dryRun":true}),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let name = Path::new(image["requestedOutputPath"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let middle = name
        .strip_prefix("chatgpt-image-")
        .and_then(|rest| rest.strip_suffix(".png"))
        .unwrap();
    let (time, unique) = middle.split_once('-').unwrap();
    assert!(
        !time.is_empty()
            && time
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
            && unique.len() == 8
            && unique.bytes().all(|b| b.is_ascii_hexdigit()),
        "{name}"
    );
    for (tool, input, expected) in [
        ("consult", json!({"prompt":""}), "Prompt is required."),
        ("chatgpt_image", json!({"prompt":""}), "Prompt is required."),
        ("wait", json!({"id":""}), "Session id is required."),
        (
            "consult",
            json!({"prompt":PROMPT,"PUBLIC":true}),
            "Unrecognized key(s) in object: 'PUBLIC'",
        ),
        (
            "consult",
            json!({"prompt":PROMPT,"engine":"api","browserKeepBrowser":false,"generateImage":"generated/PUBLIC.png"}),
            "MCP image output requires engine:\"browser\".",
        ),
        (
            "consult",
            json!({"prompt":PROMPT,"engine":"api","browserKeepBrowser":false,"dryRun":true}),
            "API execution cannot honour requested browserKeepBrowser; use engine browser.",
        ),
    ] {
        assert_eq!(
            call(&server, tool, input).await.unwrap_err().to_string(),
            expected
        );
    }
}

#[tokio::test]
async fn browser_consult_without_chrome_fails_before_creating_a_session() {
    let home = tempfile::tempdir().unwrap();
    let environment = chromeless(home.path());
    // Linux discovery also checks fixed system paths (as chrome-launcher does), which no test
    // environment can hide, so a host with Chrome installed cannot exercise this refusal.
    if let Some(chrome) = alcuin::browser_detect::chrome(&environment) {
        eprintln!("skipped: Chrome is installed at {}", chrome.display());
        return;
    }
    let server = server(home.path(), environment);
    let error = call(
        &server,
        "consult",
        json!({"prompt":PROMPT,"engine":"browser"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Browser engine unavailable: no Chrome installation found and CHROME_PATH is unset."
    );
    assert!(server.store.list(true).unwrap().is_empty());
}

#[test]
fn external_output_keeps_the_lexical_path() {
    let home = tempfile::tempdir().unwrap();
    let requested = home.path().join("PUBLIC-a").join("..").join("PUBLIC.png");
    assert_eq!(
        alcuin::config::mcp_output(home.path(), &requested, "outputPath", true).unwrap(),
        home.path().join("PUBLIC.png")
    );
}

#[test]
fn warning_filenames_and_log_tails_follow_the_reference_units() {
    let mut out = json!({});
    add_summaries(
        &mut out,
        &json!({"browser":{"warnings":[{"code":"artifact-staging-cleanup-failed","severity":"warning","details":{"artifactKind":"file","origin":"local","filename":"C:\\PUBLIC\\My Report.PDF.crdownload"}},{"code":"artifact-staging-cleanup-failed","severity":"warning","details":{"artifactKind":"file","origin":"local","filename":""}}]}}),
    );
    assert_eq!(out["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(out["warnings"][0]["details"]["filename"], "my-report.pdf");
    let astral = "🦀".repeat(4000);
    let tail = log_tail(&astral, 4000);
    assert_eq!(
        (tail.chars().count(), tail.encode_utf16().count()),
        (2000, 4000)
    );
    assert_eq!(log_tail("PUBLIC", 4000), "PUBLIC");
}

#[tokio::test]
async fn inline_api_consults_log_the_reference_run_and_trace() {
    let home = tempfile::tempdir().unwrap();
    let completed = json!({"id":"resp_PUBLIC","status":"completed","output_text":["PUBLIC answer"],"reasoning":{"mode":"pro"},"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}).to_string();
    let (endpoint, seen) = stub(vec![(200, completed)]);
    let mut env = azure(&endpoint);
    env.insert("ORACLE_PERF_TRACE".into(), "PUBLIC-trace.json".into());
    let traced = server(home.path(), env);
    let reply = call(&traced, "consult", json!({"prompt":PROMPT,"engine":"api"}))
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
    let id = reply
        .strip_prefix("Session ")
        .and_then(|rest| rest.split_once(' '))
        .unwrap()
        .0
        .to_owned();
    assert!(
        reply.contains(&format!("Session {id} (completed)\n")),
        "{reply}"
    );
    assert!(
        reply.contains("Calling gpt-6-astra (API: PUBLIC-deployment)"),
        "{reply}"
    );
    assert!(reply.contains("\nAnswer:\nPUBLIC answer\n"), "{reply}");
    assert!(reply.contains("gpt-6-astra"), "{reply}");
    let traces = std::fs::read_dir(home.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("PUBLIC-trace.") && name.contains(&id))
        .collect::<Vec<_>>();
    assert_eq!(traces.len(), 1, "{traces:?}");

    let (endpoint, _) = stub(vec![(
        400,
        json!({"error":{"message":"PUBLIC rejected"}}).to_string(),
    )]);
    let server = server(home.path(), azure(&endpoint));
    let failed = server
        .invoke(
            "consult",
            json!({"prompt":format!("{PROMPT} again"),"engine":"api"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(failed.is_error, Some(true));
    let message = text(&failed);
    assert!(
        message.starts_with("Session ") && message.contains(" failed: "),
        "{message}"
    );
    let id = message["Session ".len()..].split_once(' ').unwrap().0;
    let log = server.store.log(id).unwrap();
    assert!(log.contains("Run failed: "), "{log}");
}

#[tokio::test]
async fn detached_launch_failure_names_the_session() {
    let home = tempfile::tempdir().unwrap();
    let mut server = server(home.path(), azure("http://127.0.0.1:9"));
    // A working directory that does not exist makes the worker spawn fail.
    server.cwd = home.path().join("PUBLIC-missing");
    let error = call(
        &server,
        "consult",
        json!({"prompt":PROMPT,"engine":"api","waitForCompletion":false}),
    )
    .await
    .unwrap_err()
    .to_string();
    let session = server.store.list(true).unwrap();
    let id = session[0]["id"].as_str().unwrap();
    assert!(
        error.starts_with(&format!("Unable to start detached session {id}: ")),
        "{error}"
    );
    assert_eq!(
        server.store.require(id).unwrap()["options"]["waitPreference"],
        false
    );
}

struct Child {
    child: std::process::Child,
    output: BufReader<std::process::ChildStdout>,
    frames: Vec<Value>,
}
impl Child {
    fn send(&mut self, message: Value) {
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
    }
    fn receive(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            assert_ne!(
                self.output.read_line(&mut line).unwrap(),
                0,
                "MCP child exited"
            );
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == id {
                return frame;
            }
            self.frames.push(frame);
        }
    }
}

#[test]
fn stdio_serves_the_package_version_log_notifications_and_detached_hint() {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_alcuin-mcp"));
    command
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("ORACLE_HOME_DIR", home.path())
        .env("USERPROFILE", home.path())
        .env("HOME", home.path());
    for (key, value) in azure("http://127.0.0.1:9") {
        command.env(key, value);
    }
    let mut child = command
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let output = BufReader::new(child.stdout.take().unwrap());
    let mut child = Child {
        child,
        output,
        frames: vec![],
    };
    child.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"PUBLIC","version":"0.0.0"}}}));
    assert_eq!(
        child.receive(1)["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    child.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    child.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"consult","arguments":{"prompt":PROMPT,"engine":"api","dryRun":true}}}));
    let dry = child.receive(2);
    let logged = child
        .frames
        .iter()
        .filter(|frame| frame["method"] == "notifications/message")
        .map(|frame| frame["params"]["data"]["text"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        logged.join("\n"),
        dry["result"]["structuredContent"]["output"]
            .as_str()
            .unwrap()
    );
    child.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"consult","arguments":{"prompt":PROMPT,"engine":"api","waitForCompletion":false}}}));
    let detached = child.receive(3);
    let id = detached["result"]["structuredContent"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        detached["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with(&format!(
                "; detached)\nUse the wait tool with id=\"{id}\" to wait without polling."
            )),
        "{detached}"
    );
    // The worker fails against the closed port; wait for it so the directory can be removed.
    child.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"wait","arguments":{"id":id,"timeoutMs":60000}}}));
    let _ = child.receive(4);
    drop(child.child.stdin.take());
    let _ = child.child.wait();
    std::thread::sleep(Duration::from_millis(200));
}

#[tokio::test]
async fn missing_api_keys_use_the_reference_text() {
    let home = tempfile::tempdir().unwrap();
    let hint = " If you have a ChatGPT Pro subscription, retry with --engine browser (or MCP engine:\"browser\" / preset:\"chatgpt-pro-heavy\"); browser mode uses your signed-in ChatGPT session instead of an API key.";
    for (env, source) in [
        (Environment::new(), "OPENAI_API_KEY"),
        (
            Environment::from([
                (
                    "AZURE_OPENAI_ENDPOINT".into(),
                    "https://PUBLIC.invalid".into(),
                ),
                ("AZURE_OPENAI_DEPLOYMENT".into(), "PUBLIC".into()),
            ]),
            "AZURE_OPENAI_API_KEY (or OPENAI_API_KEY)",
        ),
    ] {
        let server = server(home.path(), env);
        assert_eq!(
            call(&server, "consult", json!({"prompt":PROMPT,"engine":"api"}))
                .await
                .unwrap_err()
                .to_string(),
            format!("Missing {source}. Set it via the environment.{hint}")
        );
    }
}

#[tokio::test]
async fn file_inputs_outside_the_working_directory_are_refused_unless_opted_in() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("PUBLIC-project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/PUBLIC.rs"), "fn main() {}\n").unwrap();
    let outside = home.path().join("PUBLIC-outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("credentials"), "PUBLIC synthetic secret\n").unwrap();
    let mut escapes = vec![
        outside.join("credentials").to_string_lossy().into_owned(),
        "../PUBLIC-outside/credentials".to_owned(),
        "src/../../PUBLIC-outside/credentials".to_owned(),
        "../PUBLIC-outside/*".to_owned(),
    ];
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(
            outside.join("credentials"),
            project.join("PUBLIC-file-link"),
        )
        .unwrap();
        symlink(&outside, project.join("PUBLIC-folder-link")).unwrap();
        escapes.extend(
            [
                "PUBLIC-file-link",
                "PUBLIC-folder-link/credentials",
                "PUBLIC-folder-link/*",
            ]
            .map(str::to_owned),
        );
    }
    #[cfg(windows)]
    {
        // A junction needs no symlink privilege.
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(project.join("PUBLIC-folder-link"))
            .arg(&outside)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        // A file symlink needs Developer Mode or elevation.
        if std::os::windows::fs::symlink_file(
            outside.join("credentials"),
            project.join("PUBLIC-file-link"),
        )
        .is_ok()
        {
            escapes.push("PUBLIC-file-link".to_owned());
        }
        escapes
            .extend(["PUBLIC-folder-link/credentials", "PUBLIC-folder-link/*"].map(str::to_owned));
    }
    let mut confined = server(home.path(), Environment::new());
    confined.cwd = project.clone();
    for escape in &escapes {
        for (tool, input) in [
            (
                "consult",
                json!({"prompt":PROMPT,"engine":"api","dryRun":true,"files":["src/PUBLIC.rs", escape]}),
            ),
            (
                "project_sources",
                json!({"operation":"add","dryRun":true,"chatgptUrl":"https://chatgpt.com/g/g-p-public/project","files":[escape]}),
            ),
        ] {
            let error = call(&confined, tool, input).await.unwrap_err().to_string();
            assert!(
                error.starts_with(&format!(
                    "MCP file \"{escape}\" is outside the working directory"
                )) && error.contains("ORACLE_MCP_ALLOW_EXTERNAL_INPUT=1"),
                "{tool} {escape}: {error}"
            );
        }
    }
    let inside = call(
        &confined,
        "consult",
        json!({"prompt":PROMPT,"engine":"api","dryRun":true,"files":["src/*.rs", "!../PUBLIC-outside/*"]}),
    )
    .await
    .unwrap();
    assert!(inside.contains("src/PUBLIC.rs"), "{inside}");
    let mut opted_in = server(
        home.path(),
        Environment::from([("ORACLE_MCP_ALLOW_EXTERNAL_INPUT".into(), "1".into())]),
    );
    opted_in.cwd = project;
    let external = call(
        &opted_in,
        "consult",
        json!({"prompt":PROMPT,"engine":"api","dryRun":true,"files":["../PUBLIC-outside/credentials"]}),
    )
    .await
    .unwrap();
    assert!(
        external.contains("PUBLIC-outside/credentials"),
        "{external}"
    );
}

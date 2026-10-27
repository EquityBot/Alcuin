use alcuin::{
    config::Environment,
    followup::{self, Followup},
    sessions::SessionStore,
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
};

async fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("ORACLE_HOME_DIR", home)
            .env("ORACLE_NO_DETACH", "1")
            .current_dir(home)
            .args(args)
            .arg("--wait")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("owned CLI child timed out")
    .unwrap()
}

#[tokio::test]
async fn actual_cli_api_followups_send_one_previous_response_and_preserve_parent_bytes() {
    for (case, expected) in [
        ("root", "resp_root"),
        ("selected", "resp_two"),
        ("sole", "resp_one"),
        ("log", "resp_latest"),
        ("direct", "resp_direct"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let parent = store
            .create(
                json!({"prompt":"PUBLIC parent","model":"gpt-6-astra"}),
                home.path(),
                Some("public-parent"),
            )
            .unwrap();
        let id = parent["id"].as_str().unwrap();
        let patch = match case {
            "root" => {
                json!({"status":"completed","response":{"responseId":"resp_root","id":"resp_ignored"},"models":[{"model":"old-two","response":{"id":"resp_ignored2"}}]})
            }
            "selected" => {
                json!({"status":"completed","models":[{"model":"old-one","response":{"responseId":"resp_one"}},{"model":"old-two","response":{"id":"resp_two"}}]})
            }
            "sole" => {
                json!({"status":"completed","models":[{"model":"old-one","response":{"responseId":"resp_one"}}]})
            }
            _ => json!({"status":"completed"}),
        };
        store.update(id, |_| Ok(Some(patch))).unwrap();
        if case == "selected" {
            store
                .update_model(
                    id,
                    "old-one",
                    &json!({"response":{"responseId":"resp_one"}}),
                )
                .unwrap();
            store
                .update_model(id, "old-two", &json!({"response":{"id":"resp_two"}}))
                .unwrap();
        } else if case == "sole" {
            store
                .update_model(
                    id,
                    "gpt-6-astra",
                    &json!({"response":{"responseId":"resp_one"}}),
                )
                .unwrap();
        }
        if case == "log" {
            store
                .append_log(id, None, "PUBLIC resp_old then resp_latest!")
                .unwrap();
        }
        let before = std::fs::read(store.directory(id).unwrap().join("meta.json")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = vec![];
            let mut buffer = [0; 4096];
            loop {
                let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
                assert!(bytes.len() < 64 * 1024);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body = [
                json!({"type":"response.created","response":{"id":"resp_child","status":"in_progress","model":"gpt-6-astra","output":[]}}),
                json!({"type":"response.completed","response":{"id":"resp_child","status":"completed","model":"gpt-6-astra","output":[{"type":"message","id":"msg_child","role":"assistant","content":[{"type":"output_text","text":"PUBLIC follow-up answer"}]}]}})
            ].into_iter().map(|event|format!("data: {event}\n\n")).collect::<String>();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "a second create request was sent"
            );
            bytes
        });
        let reference = if case == "direct" { "resp_direct" } else { id };
        let mut command = Command::new(env!("CARGO_BIN_EXE_alcuin"));
        command
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .env("AZURE_OPENAI_API_KEY", "PUBLIC-SYNTHETIC-KEY")
            .env("ORACLE_NO_DETACH", "1")
            .current_dir(home.path())
            .arg("--wait")
            .args([
                "-p",
                "PUBLIC child",
                "--engine",
                "api",
                "--model",
                "gpt-6-astra",
                "--followup",
                reference,
                "--no-background",
                "--azure-endpoint",
                &endpoint,
                "--azure-deployment",
                "gpt-6-astra",
            ])
            .kill_on_drop(true);
        if matches!(case, "root" | "selected") {
            command.args(["--followup-model", "old-two"]);
        }
        if cfg!(windows) {
            // Windows networking needs the OS directory even in an isolated child.
            command.env(
                "SystemRoot",
                std::env::var_os("SystemRoot").expect("Windows OS directory"),
            );
        }
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("owned API CLI child timed out")
            .unwrap();
        let bytes = server.await.unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(bytes.starts_with(b"POST /openai/v1/responses "));
        let end = bytes
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes[end + 4..]).unwrap();
        assert_eq!(body["previous_response_id"], expected);
        assert_eq!(body["model"], "gpt-6-astra");
        let child = store
            .list(false)
            .unwrap()
            .into_iter()
            .find(|session| session["id"] != id)
            .unwrap();
        assert_eq!(child["status"], "completed");
        assert_eq!(child["options"]["model"], "gpt-6-astra");
        assert_eq!(child["options"]["previousResponseId"], expected);
        assert_eq!(
            child["options"]["followupSessionId"],
            if case == "direct" {
                Value::Null
            } else {
                json!(id)
            }
        );
        assert_eq!(
            std::fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
            before
        );
        assert_eq!(store.list(false).unwrap().len(), 2);
    }
}

#[tokio::test]
async fn actual_cli_followup_errors_and_browser_previews_do_not_mutate_parent() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    store
        .create(
            json!({"prompt":"PUBLIC parent","model":"gpt-6-astra"}),
            home.path(),
            Some("public-parent"),
        )
        .unwrap();
    let parent = browser_parent();
    store
        .update("public-parent", |_| {
            Ok(Some(
                json!({"mode":"browser","options":parent["options"],"browser":parent["browser"]}),
            ))
        })
        .unwrap();
    let before =
        std::fs::read(store.directory("public-parent").unwrap().join("meta.json")).unwrap();
    for extra in [
        vec!["--models", "gpt-6-astra,gpt-6-sol"],
        vec!["--reasoning-mode", "pro"],
        vec!["--reasoning-effort", "high"],
    ] {
        let mut args = vec![
            "-p",
            "PUBLIC child",
            "--followup",
            "public-parent",
            "--dry-run",
            "summary",
        ];
        args.extend(extra);
        let output = cli(home.path(), &args).await;
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("--followup cannot be combined with --models.")
                || error.contains("requires --engine api."),
            "{error}"
        );
    }
    for timeout in [None, Some("1m30s"), Some("bad"), Some("0")] {
        let mut args = vec![
            "-p",
            "PUBLIC child",
            "--followup",
            "public-parent",
            "--dry-run",
            "json",
        ];
        if let Some(timeout) = timeout {
            args.extend(["--browser-attachment-timeout", timeout]);
        }
        let output = cli(home.path(), &args).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let start = text.find("{\n").expect("JSON browser preview");
        let value: Value = serde_json::from_str(&text[start..]).unwrap();
        assert_eq!(value["engine"], "browser");
        assert_eq!(value["model"], "gpt-6-pro");
        assert!(text.contains("target=GPT-6; requested=gpt-6-pro"));
        assert!(!text.contains("old-owned-tab"));
        assert_eq!(
            text.contains("using fallback 45000ms"),
            timeout == Some("bad")
        );
    }
    for reference in ["resp_direct", "missing-reference"] {
        let output = cli(
            home.path(),
            &[
                "-p",
                "PUBLIC child",
                "--engine",
                "api",
                "--followup",
                reference,
                "--base-url",
                "https://public.invalid",
                "--render",
            ],
        )
        .await;
        assert!(!output.status.success());
    }
    let output = cli(
        home.path(),
        &["-p", "PUBLIC child", "--followup", "public-parent"],
    )
    .await;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Temporary Chat cannot be reopened by URL"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(store.directory("public-parent").unwrap().join("meta.json")).unwrap(),
        before
    );
    let sessions = store.list(false).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "public-parent");
}

#[test]
fn maintained_response_keys_root_priority_selection_and_log_fallback() {
    let root = json!({"id":"public-parent","response":{"responseId":"resp_root","id":"resp_other"},"models":[{"model":"old-one","response":{"responseId":"resp_run"}}]});
    assert_eq!(
        followup::metadata_response(&root, Some("not-present"))
            .unwrap()
            .as_deref(),
        Some("resp_root")
    );
    for response in [
        json!({"responseId":"resp_legacy"}),
        json!({"id":"resp_legacy"}),
    ] {
        assert_eq!(
            followup::metadata_response(&json!({"response":response}), None)
                .unwrap()
                .as_deref(),
            Some("resp_legacy")
        );
        assert_eq!(
            followup::metadata_response(
                &json!({"models":[{"model":"old-one","response":response}]}),
                None
            )
            .unwrap()
            .as_deref(),
            Some("resp_legacy")
        );
    }
    assert!(
        followup::metadata_response(
            &json!({"response":{"responseId":"","id":"resp_other"}}),
            None
        )
        .unwrap()
        .is_none()
    );
    let multiple = json!({"id":"public-parent","models":[{"model":"old-one","response":{"responseId":"resp_one"}},{"model":"old-two","response":{"id":"resp_two"}}]});
    assert_eq!(
        followup::metadata_response(&multiple, Some("old-two"))
            .unwrap()
            .as_deref(),
        Some("resp_two")
    );
    assert_eq!(
        followup::metadata_response(&multiple, None)
            .unwrap_err()
            .to_string(),
        "Session public-parent has multiple model runs. Re-run with --followup-model. Available: old-one, old-two"
    );
    assert_eq!(
        followup::metadata_response(&multiple, Some("missing"))
            .unwrap_err()
            .to_string(),
        "Session public-parent has no model named missing. Available: old-one, old-two"
    );
    assert_eq!(
        followup::last_log_response("PUBLIC resp_First, invalid resp_😀 then resp_LAST99_more")
            .as_deref(),
        Some("resp_LAST99")
    );
    assert!(followup::last_log_response("PUBLIC no response identity resp_").is_none());
}
#[test]
fn actual_stored_references_trim_preserve_lineage_and_use_only_the_last_log_id() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    let parent = store
        .create(
            json!({"prompt":"PUBLIC previous request","model":"gpt-6-astra"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = parent["id"].as_str().unwrap();
    store
        .append_log(id, None, "PUBLIC old resp_first followed by resp_latest9")
        .unwrap();
    match followup::resolve(&store, &format!(" \u{feff}{id}\t"), None).unwrap() {
        Followup::Api {
            response_id,
            session_id,
        } => {
            assert_eq!(response_id, "resp_latest9");
            assert_eq!(session_id.as_deref(), Some(id));
        }
        _ => panic!("API parent was reclassified"),
    }
    match followup::resolve(&store, " \tresp_direct \u{00a0}", Some("old-model")).unwrap() {
        Followup::Api {
            response_id,
            session_id,
        } => {
            assert_eq!(response_id, "resp_direct");
            assert!(session_id.is_none());
        }
        _ => panic!("Direct response was reclassified"),
    }
    assert_eq!(store.list(false).unwrap().len(), 1);
    assert_eq!(
        followup::resolve(&store, " \t", None)
            .err()
            .unwrap()
            .to_string(),
        "--followup requires a session id or response id."
    );
}
fn browser_parent() -> Value {
    json!({"id":"public-parent","mode":"browser","options":{"model":"gpt-6-pro"},"browser":{"config":{"url":"https://chatgpt.com/","desiredModel":"GPT-6","thinkingTime":"pro","requireVerifiedSelection":true,"modelStrategy":"select","attachmentTimeoutMs":65432,"manualLogin":true,"browserTabRef":"old-owned-tab","researchMode":"on","archiveConversations":"always"},"runtime":{"tabUrl":"https://chatgpt.com/c/public-runtime","conversationId":"public-runtime"}}})
}
#[test]
fn browser_resume_prefers_eligible_harvest_then_runtime_archive_and_saved_base() {
    let mut parent = browser_parent();
    parent["browser"]["harvest"] =
        json!({"url":"https://chatgpt.com/c/public-harvest","integrity":{"status":"matched"}});
    assert_eq!(
        followup::browser_resume(&parent).as_deref(),
        Some("https://chatgpt.com/c/public-harvest")
    );
    for integrity in [
        json!({"status":"mismatch"}),
        json!({"status":"matched","explicitTarget":true}),
    ] {
        parent["browser"]["harvest"]["integrity"] = integrity;
        assert_eq!(
            followup::browser_resume(&parent).as_deref(),
            Some("https://chatgpt.com/c/public-runtime")
        );
    }
    parent["browser"]["runtime"]["tabUrl"] = json!("https://chatgpt.com/");
    parent["browser"]["archive"] =
        json!({"conversationUrl":"https://chat.openai.com/c/public-archive"});
    assert_eq!(
        followup::browser_resume(&parent).as_deref(),
        Some("https://chat.openai.com/c/public-archive")
    );
    parent["browser"]["archive"] = Value::Null;
    parent["browser"]["config"]["url"] = json!("https://chatgpt.com/g/g-p-public/project");
    assert_eq!(
        followup::browser_resume(&parent).as_deref(),
        Some("https://chatgpt.com/g/g-p-public/project/c/public-runtime")
    );
    for url in [
        "http://chatgpt.com/c/public",
        "https://external.invalid/c/public",
        "https://chatgpt.com:444/c/public",
        "https://chatgpt.com/",
        "https://chatgpt.com/g/g-p-public/project",
        "https://chatgpt.com/c/WEB:public",
    ] {
        parent["browser"]["config"]["url"] = json!("https://external.invalid");
        parent["browser"]["runtime"]["tabUrl"] = json!(url);
        assert!(followup::browser_resume(&parent).is_none(), "{url}");
    }
}
#[test]
fn browser_followups_preserve_saved_selection_and_reset_only_the_lifecycle_choices() {
    let mut parent = browser_parent();
    let config = followup::browser_parent("public-parent", &parent)
        .unwrap()
        .unwrap();
    assert_eq!(config["desiredModel"], "GPT-6");
    assert_eq!(config["thinkingTime"], "pro");
    assert_eq!(config["attachmentTimeoutMs"], 65432);
    assert_eq!(config["browserTabRef"], Value::Null);
    assert_eq!(config["researchMode"], "off");
    assert_eq!(config["archiveConversations"], "never");
    assert_eq!(config["automaticModelPriority"], false);
    assert_eq!(
        parent["browser"]["config"]["browserTabRef"],
        "old-owned-tab"
    );
    parent["browser"]["config"]["desiredModel"] = Value::Null;
    parent["browser"]["config"]["thinkingTime"] = Value::Null;
    parent["browser"]["modelSelection"] = json!({"verified":true,"requestedModel":"GPT-6"});
    parent["browser"]["thinkingSelection"] = json!({"verified":true,"requestedLevel":"extra high"});
    let recovered = followup::browser_parent("public-parent", &parent)
        .unwrap()
        .unwrap();
    assert_eq!(recovered["desiredModel"], "GPT-6");
    assert_eq!(recovered["thinkingTime"], "extra-high");
    parent["browser"]["thinkingSelection"]["verified"] = json!(false);
    assert!(
        followup::browser_parent("public-parent", &parent)
            .unwrap_err()
            .to_string()
            .contains("Refusing to replace the saved selection")
    );
    for strategy in ["current", "ignore"] {
        parent["browser"]["config"]["modelStrategy"] = json!(strategy);
        let config = followup::browser_parent("public-parent", &parent)
            .unwrap()
            .unwrap();
        assert!(config["desiredModel"].is_null() && config["thinkingTime"].is_null());
    }
    let options_only = json!({"id":"public-options","options":{"mode":"browser","model":"gpt-6-astra","browserConfig":{"url":"https://chatgpt.com/","desiredModel":"GPT-6","thinkingTime":"pro"}},"browser":{"runtime":{"conversationId":"public-options"}}});
    assert_eq!(
        followup::browser_parent("public-options", &options_only)
            .unwrap()
            .unwrap()["resumeConversationUrl"],
        "https://chatgpt.com/c/public-options"
    );
}

#[tokio::test]
async fn url_followups_refuse_before_profile_access_or_child_creation() {
    for saved_capacity in [None, Some(4)] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        store
            .create(
                json!({"prompt":"PUBLIC saved browser parent","model":"gpt-6-pro"}),
                home.path(),
                Some("public-parent"),
            )
            .unwrap();
        let mut parent = browser_parent();
        parent["browser"]["config"]["manualLoginProfileDir"] =
            json!(home.path().join("PUBLIC saved profile"));
        parent["browser"]["config"]["chromePath"] = json!("PUBLIC saved Chrome.exe");
        parent["browser"]["config"]["cookieSync"] = json!(false);
        parent["browser"]["config"]["debugPort"] = json!(9332);
        parent["browser"]["config"]["chatgptUrl"] = json!("https://chatgpt.com/c/public-saved");
        if let Some(capacity) = saved_capacity {
            parent["browser"]["config"]["maxConcurrentTabs"] = json!(capacity);
        }
        store.update("public-parent",|_|Ok(Some(json!({"mode":"browser","options":parent["options"],"browser":parent["browser"]})))).unwrap();
        let before =
            std::fs::read(store.directory("public-parent").unwrap().join("meta.json")).unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(env!("CARGO_BIN_EXE_alcuin"))
                .env_clear()
                .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("ORACLE_HOME_DIR", home.path())
                .env("ORACLE_BROWSER_MAX_CONCURRENT_TABS", "8")
                .env("ORACLE_NO_DETACH", "1")
                .current_dir(home.path())
                .args([
                    "-p",
                    "PUBLIC child runtime settings",
                    "--followup",
                    "public-parent",
                    "--wait",
                    "--remote-chrome",
                    "PUBLIC invalid endpoint",
                    "--browser-max-concurrent-tabs",
                    "PUBLIC invalid capacity",
                    "--browser-timeout",
                    "PUBLIC invalid duration",
                    "--browser-port",
                    "PUBLIC invalid port",
                    "--chatgpt-url",
                    "https://example.invalid/",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{error}");
        assert!(
            error.contains("Temporary Chat cannot be reopened by URL"),
            "{error}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("invalid --browser-timeout"));
        let entries = store.list(true).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "public-parent");
        assert_eq!(
            std::fs::read(store.directory("public-parent").unwrap().join("meta.json")).unwrap(),
            before
        );
    }
}
#[test]
fn api_eligibility_and_explicit_attachment_durations_match_the_source_contract() {
    let mut env = Environment::new();
    followup::require_api("api", "gpt-6-astra", &json!({}), &env).unwrap();
    assert!(followup::require_api("browser", "gpt-6-astra", &json!({}), &env).is_err());
    for model in ["gemini-pro", "claude-opus"] {
        assert!(followup::require_api("api", model, &json!({}), &env).is_err());
    }
    for url in [
        "https://api.openai.com/v1",
        "https://api.anthropic.com",
        "https://generativelanguage.googleapis.com",
        "https://api.x.ai",
    ] {
        assert!(
            followup::require_api("api", "gpt-6-astra", &json!({"baseUrl":url}), &env).is_err()
        );
    }
    env.insert("OPENAI_BASE_URL".into(), "https://api.anthropic.com".into());
    assert!(followup::require_api("api", "gpt-6-astra", &json!({}), &env).is_err());
    followup::require_api(
        "api",
        "gpt-6-astra",
        &json!({"azure":{"endpoint":"https://public-azure.invalid"}}),
        &env,
    )
    .unwrap();
    for (raw, expected) in [
        ("0", 0.0),
        ("0ms", 0.0),
        ("200", 200.0),
        ("2 s", 2000.0),
        ("1m30s", 90000.0),
        ("1 H 2M 3s", 3_723_000.0),
    ] {
        assert_eq!(followup::attachment_timeout(raw), Some(expected), "{raw}");
    }
    for raw in ["", "auto", "0m0s", "-1s", "1.5s", "5days", "1s trailing"] {
        assert!(followup::attachment_timeout(raw).is_none(), "{raw}");
    }
}

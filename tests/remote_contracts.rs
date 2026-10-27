use alcuin::{
    config::Environment,
    mcp::AlcuinMcp,
    remote_host::{HostOptions, RemoteHost, RunDriver},
    sessions::SessionStore,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

async fn host(home: &std::path::Path, calls: Arc<AtomicUsize>) -> RemoteHost {
    let driver: RunDriver = Arc::new(move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { anyhow::bail!("PUBLIC remote fixture producer failed") })
    });
    RemoteHost::start(
        SessionStore::new(home.join("producer")),
        HostOptions {
            address: "127.0.0.1:0".parse().unwrap(),
            token: "PUBLIC ephemeral test token".into(),
            max_concurrent_runs: Some(1),
            max_queued_runs: Some(0),
            max_browser_tabs: 1,
        },
        driver,
    )
    .await
    .unwrap()
}
fn environment(home: &std::path::Path) -> Environment {
    let environment = Environment::from_iter([
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("USERPROFILE".into(), home.to_string_lossy().into_owned()),
        (
            "ORACLE_HOME_DIR".into(),
            home.join("receiver").to_string_lossy().into_owned(),
        ),
    ]);
    #[cfg(windows)]
    let environment = {
        let mut environment = environment;
        environment.insert("SystemRoot".into(), std::env::var("SystemRoot").unwrap());
        environment
    };
    environment
}
#[tokio::test]
async fn compiled_cli_remote_runs_wait_and_dispatch_once_without_persisting_tokens() {
    for preference in [Some("--wait"), Some("--no-wait"), None] {
        let home = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let host = host(home.path(), calls.clone()).await;
        let env = environment(home.path());
        let store = SessionStore::new(home.path().join("receiver"));
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"));
        command
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(&env)
            .current_dir(home.path())
            .kill_on_drop(true)
            .args([
                "--engine",
                "browser",
                "--prompt",
                "PUBLIC compiled remote receiver fixture",
                "--remote-host",
                &host.address.to_string(),
                "--remote-token",
                "PUBLIC ephemeral test token",
            ]);
        if let Some(preference) = preference {
            command.arg(preference);
        }
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let metadata = loop {
            let sessions = store.list(false).unwrap();
            if let Some(meta) = sessions.into_iter().find(|meta| meta["status"] == "error") {
                break meta;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Remote receiver never finalised"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "preference={preference:?}; receiver error={}; stderr={}",
            metadata["errorMessage"],
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(metadata["browser"]["runtime"]["remoteRunId"].is_string());
        assert!(
            metadata["errorMessage"]
                .as_str()
                .unwrap()
                .contains("PUBLIC remote fixture producer failed")
        );
        let id = metadata["id"].as_str().unwrap();
        for name in ["meta.json", "browser-request.json"] {
            assert!(
                !std::fs::read_to_string(store.directory(id).unwrap().join(name))
                    .unwrap()
                    .contains("PUBLIC ephemeral test token")
            );
        }
        assert_eq!(metadata["lifecycle"]["detached"], false);
        assert_eq!(metadata["options"]["waitPreference"], true);
        if preference == Some("--no-wait") {
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("Remote browser runs require --wait; ignoring --no-wait.")
            );
        }
        host.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn remote_cli_api_host_checks_include_configuration_and_ignore_blank_values() {
    for source in ["cli", "config", "env", "blank", "blank-cli-config"] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("receiver"));
        std::fs::create_dir_all(store.home()).unwrap();
        let mut env = environment(home.path());
        if source == "env" {
            env.insert("ORACLE_REMOTE_HOST".into(), "PUBLIC.invalid:1".into());
        }
        if source == "blank" {
            env.insert("ORACLE_REMOTE_HOST".into(), " \t".into());
        }
        if matches!(source, "config" | "blank-cli-config") {
            alcuin::storage::atomic_json(
                &store.home().join("config.json"),
                &json!({"browser":{"remoteHost":" PUBLIC.invalid:1 "}}),
            )
            .unwrap();
        }
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"));
        command
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(&env)
            .current_dir(home.path())
            .kill_on_drop(true)
            .args([
                "--engine",
                "api",
                "--prompt",
                "PUBLIC remote API admission",
                "--wait",
            ]);
        if source == "cli" {
            command.args(["--remote-host", "PUBLIC.invalid:1"]);
        }
        if matches!(source, "blank" | "blank-cli-config") {
            command.args(["--remote-host", "  "]);
        }
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if source == "blank" {
                "Missing OPENAI_API_KEY"
            } else {
                "--remote-host requires --engine browser."
            }),
            "{source}: {error}"
        );
        assert!(store.list(false).unwrap().is_empty());
    }
}

#[tokio::test]
async fn mcp_remote_token_and_no_detach_guards_precede_session_creation_and_dry_run_bypasses_them()
{
    for (engine, wait, disabled, expected) in [
        ("browser", true, false, "remote token is missing"),
        (
            "browser",
            false,
            true,
            "Detached MCP consults are not supported",
        ),
        (
            "api",
            false,
            true,
            "Detached MCP consults are disabled by ORACLE_NO_DETACH=1",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("receiver"));
        let mut env = environment(home.path());
        env.insert("ORACLE_REMOTE_HOST".into(), "PUBLIC.invalid:1".into());
        if disabled {
            env.insert("ORACLE_NO_DETACH".into(), "1".into());
        }
        let server = AlcuinMcp {
            store: store.clone(),
            cwd: home.path().into(),
            env,
        };
        let mut input = json!({"prompt":"PUBLIC guarded remote admission","engine":engine,"waitForCompletion":wait});
        let error = server
            .invoke("consult", input.clone(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        assert!(store.list(false).unwrap().is_empty());
        input["dryRun"] = json!(true);
        let result = server
            .invoke("consult", input, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.as_ref().unwrap()["status"],
            "dry-run"
        );
        assert!(store.list(false).unwrap().is_empty());
    }
}

#[tokio::test]
async fn mcp_api_ignores_remote_browser_hosts_and_uses_the_local_pro_provider() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for source in ["config", "env", "blank"] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("receiver"));
        std::fs::create_dir_all(store.home()).unwrap();
        if source == "config" {
            alcuin::storage::atomic_json(
                &store.home().join("config.json"),
                &json!({"browser":{"remoteHost":"PUBLIC.invalid:1"}}),
            )
            .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = vec![];
            let mut buffer = [0; 4096];
            let body = loop {
                let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(count > 0 && bytes.len() + count < 1024 * 1024);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let length = String::from_utf8_lossy(&bytes[..end])
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(
                            &bytes[end + 4..end + 4 + length],
                        )
                        .unwrap();
                    }
                }
            };
            let response=json!({"id":"resp_PUBLIC_remote_settings_ignored","status":"completed","model":"gpt-6-astra","reasoning":{"mode":"pro"},"output":[{"type":"message","content":[{"type":"output_text","text":"PUBLIC local MCP API answer","annotations":[]}]}]}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            body
        });
        let mut env = environment(home.path());
        if source != "config" {
            env.insert(
                "ORACLE_REMOTE_HOST".into(),
                if source == "blank" {
                    " \t"
                } else {
                    "PUBLIC.invalid:1"
                }
                .into(),
            );
        }
        env.insert("AZURE_OPENAI_ENDPOINT".into(), format!("http://{address}"));
        env.insert("AZURE_OPENAI_DEPLOYMENT".into(), "PUBLIC-deployment".into());
        env.insert(
            "AZURE_OPENAI_API_KEY".into(),
            "PUBLIC synthetic API credential".into(),
        );
        let server = AlcuinMcp {
            store: store.clone(),
            cwd: home.path().into(),
            env,
        };
        let result=server.invoke("consult",json!({"prompt":"PUBLIC local API despite remote browser settings","engine":"api","waitForCompletion":true}),&CancellationToken::new()).await.unwrap();
        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["status"], "completed");
        assert!(
            structured["output"]
                .as_str()
                .unwrap()
                .contains("PUBLIC local MCP API answer")
        );
        assert_eq!(peer.await.unwrap()["reasoning"]["mode"], "pro");
        assert_eq!(store.list(false).unwrap().len(), 1);
    }
}

#[tokio::test]
async fn mcp_remote_foreground_retains_failure_and_detach_is_rejected_before_session_creation() {
    let home = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let host = host(home.path(), calls.clone()).await;
    let mut env = environment(home.path());
    env.insert("ORACLE_REMOTE_HOST".into(), host.address.to_string());
    env.insert(
        "ORACLE_REMOTE_TOKEN".into(),
        "PUBLIC ephemeral test token".into(),
    );
    let store = SessionStore::new(home.path().join("receiver"));
    let server = AlcuinMcp {
        store: store.clone(),
        cwd: home.path().into(),
        env,
    };
    let mut input = json!({"prompt":"PUBLIC MCP remote receiver fixture","engine":"browser","waitForCompletion":false});
    let cancel = CancellationToken::new();
    let error = server
        .invoke("consult", input.clone(), &cancel)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Detached MCP consults are not supported")
    );
    assert!(store.list(false).unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    input["waitForCompletion"] = json!(true);
    let result = server.invoke("consult", input, &cancel).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    // Reference consult: a failed run returns only "Session <id> failed: <message>".
    let text = result.content[0].as_text().unwrap().text.clone();
    let id = store.list(false).unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.starts_with(&format!("Session {id} failed: ")),
        "{text}"
    );
    assert_eq!(store.require(&id).unwrap()["status"], "error");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn compiled_recovery_requires_explicit_host_and_never_dispatches_an_unknown_run() {
    let home = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let host = host(home.path(), calls.clone()).await;
    let mut env = environment(home.path());
    env.insert("ORACLE_REMOTE_HOST".into(), host.address.to_string());
    let store = SessionStore::new(home.path().join("receiver"));
    for explicit_host in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"));
        command
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .envs(&env)
            .current_dir(home.path())
            .kill_on_drop(true)
            .args(["recover-remote", "12345678-1234-1234-1234-123456789abc"]);
        if explicit_host {
            command.args([
                "--host",
                &host.address.to_string(),
                "--token",
                "PUBLIC ephemeral test token",
            ]);
        }
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if explicit_host {
                "receiver session"
            } else {
                "requires --host"
            }),
            "{error}"
        );
        assert_eq!(store.list(false).unwrap().len(), usize::from(explicit_host));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn compiled_serve_advertises_authenticated_capabilities_without_opening_browser_or_session() {
    use tokio::io::AsyncBufReadExt;
    let home = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .envs(environment(home.path()))
        .current_dir(home.path())
        .kill_on_drop(true)
        .args([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--token",
            "PUBLIC ephemeral test token",
        ])
        .args([
            "--remote-chrome",
            "127.0.0.1:1",
            "--browser-attach-running",
            "--browser-approval-wait",
            "5s",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let identity = alcuin::process::ProcessIdentity::read(child.id().unwrap())
        .unwrap()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        next().await,
        "Using the host browser attachment; skipping local Chrome login/bootstrap."
    );
    let line = next().await;
    let address = line.strip_prefix("Listening at ").unwrap();
    let store = SessionStore::new(home.path().join("receiver"));
    let client = alcuin::remote_client::RemoteClient::new(
        address,
        Some("PUBLIC ephemeral test token".into()),
        store.clone(),
    )
    .unwrap();
    let health = client.health(&CancellationToken::new()).await.unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["capabilities"]["runCancellation"], true);
    assert_eq!(health["capabilities"]["durableResultRecovery"], true);
    let wrong = alcuin::remote_client::RemoteClient::new(
        address,
        Some("PUBLIC wrong token".into()),
        store.clone(),
    )
    .unwrap()
    .health(&CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(wrong["ok"], false);
    assert_eq!(wrong["statusCode"], 401);
    assert!(store.list(false).unwrap().is_empty());
    assert!(!home.path().join(".oracle/browser-profile").exists());
    #[cfg(unix)]
    {
        assert_eq!(identity.liveness(), alcuin::process::Liveness::Alive);
        // SAFETY: this unreaped child and its verified native identity belong to the fixture.
        assert_eq!(unsafe { libc::kill(identity.pid as i32, libc::SIGTERM) }, 0);
    }
    #[cfg(not(unix))]
    child.start_kill().unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    #[cfg(unix)]
    assert!(stopped.success());
    #[cfg(not(unix))]
    let _ = stopped;
    assert_ne!(identity.liveness(), alcuin::process::Liveness::Alive);
}

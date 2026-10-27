use industrious::{
    config::Environment,
    remote_host::{HostOptions, RemoteHost, RunDriver},
    sessions::SessionStore,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

fn environment(home: &Path) -> Environment {
    let mut env = Environment::from_iter([
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("USERPROFILE".into(), home.to_string_lossy().into_owned()),
        (
            "ORACLE_HOME_DIR".into(),
            home.join("oracle").to_string_lossy().into_owned(),
        ),
    ]);
    if let Ok(value) = std::env::var("SystemRoot") {
        env.insert("SystemRoot".into(), value);
    }
    env
}
async fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
    command
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .envs(environment(home))
        .current_dir(home)
        .kill_on_drop(true)
        .args(args);
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn compiled_client_checks_auth_before_writing_and_honours_negated_flags() {
    let home = tempfile::tempdir().unwrap();
    let oracle = home.path().join("oracle");
    std::fs::create_dir(&oracle).unwrap();
    let path = oracle.join("config.json");
    let initial = b"{ // PUBLIC fixture\n extra:42, browser:{keepBrowser:true},\n}";
    std::fs::write(&path, initial).unwrap();
    let driver: RunDriver =
        Arc::new(|_| Box::pin(async { anyhow::bail!("PUBLIC health must not dispatch") }));
    let host = RemoteHost::start(
        SessionStore::new(home.path().join("producer")),
        HostOptions {
            address: "127.0.0.1:0".parse().unwrap(),
            token: "PUBLIC-good".into(),
            max_concurrent_runs: None,
            max_queued_runs: None,
            max_browser_tabs: 1,
        },
        driver,
    )
    .await
    .unwrap();
    let bad = format!("oracle+tcp://{}?token=PUBLIC-wrong", host.address);
    let output = cli(home.path(), &["bridge", "client", "--connect", &bad]).await;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    let output = cli(
        home.path(),
        &[
            "bridge",
            "client",
            "--connect",
            &bad,
            "--no-test",
            "--no-write-config",
        ],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    let good = format!("oracle+tcp://{}?token=PUBLIC-good", host.address);
    let output = cli(home.path(), &["bridge", "client", "--connect", &good]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Remote service OK") && !stdout.contains("PUBLIC-good"));
    let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["extra"], 42);
    assert_eq!(saved["browser"]["keepBrowser"], true);
    assert_eq!(saved["browser"]["remoteToken"], "PUBLIC-good");
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn compiled_snippets_are_parseable_and_doctor_redacts_tokens_and_reports_capabilities() {
    let home = tempfile::tempdir().unwrap();
    let oracle = home.path().join("oracle");
    std::fs::create_dir(&oracle).unwrap();
    let driver: RunDriver =
        Arc::new(|_| Box::pin(async { anyhow::bail!("PUBLIC health must not dispatch") }));
    let host = RemoteHost::start(
        SessionStore::new(home.path().join("producer")),
        HostOptions {
            address: "127.0.0.1:0".parse().unwrap(),
            token: "PUBLIC-doctor-token".into(),
            max_concurrent_runs: None,
            max_queued_runs: None,
            max_browser_tabs: 1,
        },
        driver,
    )
    .await
    .unwrap();
    std::fs::write(oracle.join("config.json"),serde_json::to_vec(&json!({"browser":{"remoteHost":host.address.to_string(),"remoteToken":"PUBLIC-doctor-token"}})).unwrap()).unwrap();
    let output = cli(home.path(), &["bridge", "claude-config"]).await;
    assert!(output.status.success());
    let snippet: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        snippet["mcpServers"]["industrious"]["env"]["ORACLE_REMOTE_TOKEN"],
        "<YOUR_TOKEN>"
    );
    let output = cli(
        home.path(),
        &[
            "bridge",
            "codex-config",
            "--local-browser",
            "--oracle-home-dir",
            "PUBLIC-state",
            "--browser-profile-dir",
            "PUBLIC-profile",
        ],
    )
    .await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("PUBLIC-state")
            && text.contains("PUBLIC-profile")
            && !text.contains("ORACLE_REMOTE_TOKEN")
    );
    let output = cli(home.path(), &["bridge", "doctor"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("TCP connect: ok")
            && text.contains("Artifact transfer: bridge v1")
            && !text.contains("PUBLIC-doctor-token")
    );
    host.shutdown().await.unwrap();
    let output = cli(home.path(), &["bridge", "doctor"]).await;
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Problems:") && !text.contains("PUBLIC-doctor-token"));
    assert!(!oracle.join("sessions").exists());
}

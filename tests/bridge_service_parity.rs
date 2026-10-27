//! Bridge command texts that match the reference bridge CLI; local files and processes only.
use std::{path::Path, time::Duration};

fn command(home: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_industrious"));
    command
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ORACLE_HOME_DIR", home.join("oracle"))
        .envs(std::env::var("SystemRoot").map(|root| ("SystemRoot", root)))
        .current_dir(home)
        .args(args);
    command
}
fn run(home: &Path, args: &[&str]) -> (bool, String, String) {
    let output = command(home, args).output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}
fn refusal(home: &Path, args: &[&str]) -> String {
    let (success, _, stderr) = run(home, args);
    assert!(!success, "{args:?}");
    stderr
}

#[test]
fn connection_artifacts_and_host_ports_name_the_offending_input() {
    let home = tempfile::tempdir().unwrap();
    let absolute = |name: &str| home.path().join(name).display().to_string();
    let client = |name: &str| refusal(home.path(), &["bridge", "client", "--connect", name]);
    assert!(client("missing.json").contains(&format!(
        "Connection artifact not found at {}",
        absolute("missing.json")
    )));
    std::fs::create_dir(home.path().join("folder.json")).unwrap();
    assert!(client("folder.json").contains(&format!(
        "--connect points to {}, but it is not a file.",
        absolute("folder.json")
    )));
    for (name, body, expected) in [
        ("null.json", "null", "expected an object."),
        ("array.json", "[]", "remoteHost is missing."),
        (
            "host.json",
            r#"{"remoteHost":" ","remoteToken":"PUBLIC"}"#,
            "remoteHost is missing.",
        ),
        (
            "token.json",
            r#"{"remoteHost":"127.0.0.1:9473"}"#,
            "remoteToken is missing.",
        ),
    ] {
        std::fs::write(home.path().join(name), body).unwrap();
        let stderr = client(name);
        assert!(
            stderr.contains(&format!(
                "Invalid connection artifact at {}: {expected}",
                absolute(name)
            )),
            "{stderr}"
        );
    }
    std::fs::write(home.path().join("broken.json"), "{").unwrap();
    assert!(client("broken.json").contains(&format!(
        "Failed to parse connection artifact JSON at {}: ",
        absolute("broken.json")
    )));
    for (host, expected) in [
        (
            "PUBLIC-host",
            "Invalid host:port format: PUBLIC-host. Expected host:port (IPv6 must use [host]:port notation).",
        ),
        (
            " ::1:9473 ",
            "Invalid host:port format: ::1:9473. Wrap IPv6 addresses in brackets, e.g. \"[2001:db8::1]:9473\".",
        ),
        (
            " :9473",
            "Invalid host:port format: :9473. Host portion is missing.",
        ),
        (
            "PUBLIC-host: abc ",
            "Invalid port: \"abc\". Expected a number between 1 and 65535.",
        ),
    ] {
        let body = serde_json::json!({"remoteHost":host,"remoteToken":"PUBLIC"}).to_string();
        std::fs::write(home.path().join("port.json"), body).unwrap();
        let stderr = client("port.json");
        assert!(stderr.contains(expected), "{host:?}: {stderr}");
    }
}

#[test]
fn client_configuration_failures_name_the_trimmed_path() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("good.json"),
        r#"{"remoteHost":"127.0.0.1:9473","remoteToken":"PUBLIC"}"#,
    )
    .unwrap();
    std::fs::write(home.path().join("PUBLIC-config.json5"), "{").unwrap();
    let stderr = refusal(
        home.path(),
        &[
            "bridge",
            "client",
            "--connect",
            "good.json",
            "--no-test",
            "--config",
            "  PUBLIC-config.json5  ",
        ],
    );
    assert!(
        stderr.contains("Failed to read PUBLIC-config.json5: "),
        "{stderr}"
    );
}

#[test]
fn mcp_snippets_suggest_print_token_only_when_the_token_is_withheld() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("oracle")).unwrap();
    std::fs::write(
        home.path().join("oracle/config.json"),
        r#"{"browser":{"remoteHost":"127.0.0.1:9473","remoteToken":"PUBLIC-token"}}"#,
    )
    .unwrap();
    let tip = "\nTip: rerun with --print-token to include ORACLE_REMOTE_TOKEN in the snippet.\n";
    for kind in ["claude-config", "codex-config"] {
        for (flags, shown) in [
            (vec![], true),
            (vec!["--print-token"], false),
            (vec!["--local-browser"], false),
        ] {
            let mut args = vec!["bridge", kind];
            args.extend(flags.iter().copied());
            let (success, stdout, stderr) = run(home.path(), &args);
            assert!(success, "{stderr}");
            assert!(!stdout.contains("Tip:"));
            assert_eq!(stderr.ends_with(tip), shown, "{args:?}: {stderr:?}");
        }
    }
}

#[test]
fn ssh_remote_ports_report_the_parsed_value() {
    let home = tempfile::tempdir().unwrap();
    for (port, expected) in [
        (
            "70000",
            "Invalid --ssh-remote-port: 70000. Expected 1-65535.",
        ),
        ("0", "Invalid --ssh-remote-port: 0. Expected 1-65535."),
        (" -5x", "Invalid --ssh-remote-port: -5. Expected 1-65535."),
        ("PUBLIC", "Value must be an integer."),
    ] {
        let stderr = refusal(
            home.path(),
            &[
                "bridge",
                "host",
                "--foreground",
                "--bind",
                "127.0.0.1:1",
                "--ssh",
                "PUBLIC-user@PUBLIC-host",
                "--ssh-remote-port",
                port,
            ],
        );
        assert!(stderr.contains(expected), "{port:?}: {stderr}");
    }
}

/// A copied industrious stands in for ssh, so no network connection is attempted.
#[tokio::test]
async fn foreground_tunnels_announce_their_ports() {
    use tokio::io::AsyncBufReadExt;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("oracle")).unwrap();
    std::fs::write(
        home.path().join("oracle/config.json"),
        r#"{"browser":{"remoteChrome":{"host":"127.0.0.1","port":1}}}"#,
    )
    .unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_industrious"),
        bin.join(if cfg!(windows) { "ssh.exe" } else { "ssh" }),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = tokio::process::Command::from(command(
        home.path(),
        &[
            "bridge",
            "host",
            "--foreground",
            "--bind",
            &bind,
            "--ssh",
            "PUBLIC-user@PUBLIC-host",
            "--ssh-remote-port",
            "2222",
        ],
    ));
    let mut child = child
        .env("PATH", &bin)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let expected =
        format!("Reverse SSH tunnel active (remote 127.0.0.1:2222 -> local 127.0.0.1:{port})");
    let mut seen = vec![];
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if line == expected {
                return true;
            }
            seen.push(line);
        }
        false
    })
    .await;
    child.start_kill().unwrap();
    assert_eq!(found, Ok(true), "{seen:?}");
}

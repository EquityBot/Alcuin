//! `alcuin serve` startup parity with the reference serveRemote: a host browser attachment,
//! no Chrome launch and no provider traffic.
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const TOKEN: &str = "PUBLIC-serve-token";
fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
enum Token {
    Flag,
    File(&'static str),
    Generated,
}
/// Starts serve and returns its startup lines up to the listening line, and /health.
async fn serve(config: Option<&str>, env: &[(&str, &str)]) -> (Vec<String>, Value) {
    serve_with(config, env, Token::Flag).await
}
/// Starts serve with the given token source and returns its startup lines through the
/// access token line, and /health fetched with the token that source supplies.
async fn serve_with(
    config: Option<&str>,
    env: &[(&str, &str)],
    token: Token,
) -> (Vec<String>, Value) {
    let home = tempfile::tempdir().unwrap();
    if let Some(config) = config {
        std::fs::write(home.path().join("config.json"), config).unwrap();
    }
    let token_args: Vec<&str> = match token {
        Token::Flag => vec!["--token", TOKEN],
        Token::File(content) => {
            std::fs::write(home.path().join("token.txt"), content).unwrap();
            vec!["--token-file", "token.txt"]
        }
        Token::Generated => vec![],
    };
    let port = unused_port().to_string();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .envs(env.iter().copied())
        .envs(std::env::var("SystemRoot").map(|root| ("SystemRoot", root)))
        .current_dir(home.path())
        .kill_on_drop(true)
        .args(["serve", "--browser-attach-running", "--host", "127.0.0.1"])
        .args(["--port", &port, "--max-concurrent-runs", "3"])
        .args(token_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut lines = vec![];
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(line) = reader.next_line().await.unwrap() {
            let done = line.starts_with("Access token:");
            lines.push(line);
            if done {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("serve did not start: {lines:?}"));
    assert!(
        lines.iter().any(|line| line.starts_with("Listening at"))
            && lines
                .last()
                .is_some_and(|line| line.starts_with("Access token:")),
        "{lines:?} {:?}",
        child.try_wait()
    );
    let bearer = match token {
        Token::Generated => lines.last().unwrap()["Access token: ".len()..].to_owned(),
        _ => TOKEN.to_owned(),
    };
    let health = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{port}/health"))
        .bearer_auth(bearer)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    child.start_kill().unwrap();
    (lines, health)
}

#[tokio::test]
async fn attached_hosts_skip_bootstrap_and_admit_within_the_environment_tab_cap() {
    let (lines, health) = serve(None, &[("ORACLE_BROWSER_MAX_CONCURRENT_TABS", " 1 ")]).await;
    assert_eq!(
        lines[..2],
        [
            "Using the host browser attachment; skipping local Chrome login/bootstrap.",
            "[serve] Admitting 1 concurrent run(s): the host tab cap is lower than the requested 3.",
        ],
        "{lines:?}"
    );
    assert_eq!(health["maxConcurrentRuns"], 1);
}

#[tokio::test]
async fn configured_tab_caps_normalise_and_outrank_the_environment() {
    for (config, expected) in [
        (r#"{"browser":{"maxConcurrentTabs":"2.9"}}"#, 2),
        (r#"{"browser":{"maxConcurrentTabs":2.5}}"#, 2),
        (r#"{"browser":{"maxConcurrentTabs":0}}"#, 3),
    ] {
        let (lines, health) =
            serve(Some(config), &[("ORACLE_BROWSER_MAX_CONCURRENT_TABS", "1")]).await;
        assert_eq!(health["maxConcurrentRuns"], expected, "{config} {lines:?}");
    }
    // An invalid environment cap falls back to the default of 3.
    let (lines, health) = serve(None, &[("ORACLE_BROWSER_MAX_CONCURRENT_TABS", "1.5")]).await;
    assert_eq!(health["maxConcurrentRuns"], 3);
    assert!(!lines.iter().any(|line| line.contains("Admitting")));
}

#[test]
fn manual_login_hosts_print_the_profile_guidance_before_the_launch() {
    let home = tempfile::tempdir().unwrap();
    let hidden = home.path().join("PUBLIC-no-chrome");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"));
    command
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .env("CHROME_PATH", &hidden)
        .current_dir(home.path())
        .args([
            "serve",
            "--manual-login",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
        ]);
    // No Chrome is discoverable, so the launch fails after the guidance.
    for name in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA", "PATH"] {
        command.env(name, &hidden);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let profile = home.path().join(".oracle").join("browser-profile");
    assert_eq!(
        stdout.lines().take(2).collect::<Vec<_>>(),
        [
            "No ChatGPT cookies detected on this host.".to_owned(),
            format!(
                "Cookie extraction is unavailable on this platform. Using manual-login Chrome profile at {}. Remote runs will reuse this profile; sign in once when the browser opens.",
                profile.display()
            ),
        ],
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
}

#[tokio::test]
async fn only_a_generated_token_is_printed() {
    for (token, printed) in [
        (Token::Flag, false),
        (Token::File("PUBLIC-serve-token\r\n"), false),
        (Token::File("PUBLIC-serve-token"), false),
        (Token::Generated, true),
    ] {
        let (lines, health) = serve_with(None, &[], token).await;
        assert_eq!(health["ok"], true, "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains(TOKEN)), "{lines:?}");
        assert_eq!(
            lines.last().unwrap() == "Access token: supplied (not printed)",
            !printed,
            "{lines:?}"
        );
    }
}

#[test]
fn unusable_token_sources_are_refused_before_startup() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("empty.txt"), "\n").unwrap();
    std::fs::write(home.path().join("lines.txt"), "PUBLIC\nsecond\n").unwrap();
    for (args, message) in [
        (
            vec!["--token", TOKEN, "--token-file", "empty.txt"],
            "not both",
        ),
        (vec!["--token-file", "empty.txt"], "is empty"),
        (vec!["--token-file", "lines.txt"], "one line"),
        (
            vec!["--token-file", "missing.txt"],
            "Cannot read token file",
        ),
        (vec!["--token", ""], "cannot be empty"),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ORACLE_HOME_DIR", home.path())
            .envs(std::env::var("SystemRoot").map(|root| ("SystemRoot", root)))
            .current_dir(home.path())
            .args([
                "serve",
                "--browser-attach-running",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
            ])
            .args(&args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}");
        assert!(stderr.contains(message), "{args:?} {stderr}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("Listening at"),
            "{args:?}"
        );
    }
}

//! A fabricated credential stops before dispatch on every route, and a refusal repeats none of it.
//! Values are assembled at run time, so this file holds no contiguous credential.
use alcuin::{
    browser_inputs::StagedUploads, browser_prompt::BrowserPrompt, config::Environment, execution,
    mcp::AlcuinMcp, prompts, sessions::SessionStore,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "PUBLIC review of this project";
const SECRET: &str = "secret.txt:2: GitHub token";

fn token() -> String {
    ["gh", "p_"].concat() + &"PbL7q".repeat(8)
}
fn secret_file(root: &Path, name: &str) {
    fs::write(root.join(name), format!("PUBLIC line\nkey = {}\n", token())).unwrap();
}
fn cli(root: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("ORACLE_HOME_DIR", root)
        .current_dir(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
/// No 8 consecutive bytes of the value appear, which also rules out its head and tail.
fn assert_no_part_of_the_value(text: &str) {
    let secret = token();
    for part in secret.as_bytes().windows(8) {
        let part = std::str::from_utf8(part).unwrap();
        assert!(!text.contains(part), "{part} in {text}");
    }
}
fn assert_refused(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout} {stderr}");
    assert!(stderr.contains(expected), "{stdout} {stderr}");
    assert!(stderr.contains("Nothing was sent"), "{stderr}");
    assert_no_part_of_the_value(&stderr);
    assert_no_part_of_the_value(&stdout);
}
fn server(home: &Path) -> AlcuinMcp {
    let path = home.to_string_lossy().into_owned();
    AlcuinMcp {
        store: SessionStore::new(home),
        cwd: home.into(),
        env: Environment::from([("HOME".into(), path.clone()), ("USERPROFILE".into(), path)]),
    }
}
async fn call(server: &AlcuinMcp, name: &str, input: Value) -> anyhow::Result<()> {
    server
        .invoke(name, input, &CancellationToken::new())
        .await
        .map(|_| ())
}

#[test]
fn the_cli_refuses_a_credential_in_a_file_for_api_and_browser_runs() {
    let home = tempfile::tempdir().unwrap();
    secret_file(home.path(), "secret.txt");
    for route in [
        vec!["--render"],
        vec!["--engine", "browser", "--render"],
        vec!["--engine", "browser", "--dry-run", "summary"],
        vec!["--engine", "api", "--dry-run", "summary"],
    ] {
        let mut args = vec!["-p", PROMPT, "--file", "secret.txt"];
        args.extend(route);
        assert_refused(&cli(home.path(), &args, ""), SECRET);
    }
}
#[test]
fn browser_bundles_and_raw_uploads_are_scanned_before_any_session_exists() {
    let home = tempfile::tempdir().unwrap();
    secret_file(home.path(), "secret.txt");
    secret_file(home.path(), "secret.png");
    for (name, extra) in [
        ("secret.txt", vec!["--browser-bundle-files"]),
        ("secret.png", vec![]),
    ] {
        let mut args = vec!["-p", PROMPT, "--engine", "browser", "--dry-run", "summary"];
        args.extend(["--file", name]);
        args.extend(extra);
        assert_refused(
            &cli(home.path(), &args, ""),
            &format!("{name}:2: GitHub token"),
        );
    }
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn a_match_found_while_expanding_a_folder_refuses_the_whole_run() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("project")).unwrap();
    secret_file(&home.path().join("project"), "secret.txt");
    fs::write(home.path().join("project/public.txt"), "PUBLIC").unwrap();
    let output = cli(
        home.path(),
        &["-p", PROMPT, "--file", "project", "--render"],
        "",
    );
    assert_refused(&output, "project/secret.txt:2: GitHub token");
}
#[test]
fn prompt_text_is_scanned_however_it_arrives_and_has_no_override() {
    let home = tempfile::tempdir().unwrap();
    let inline = format!("{PROMPT}: {}", token());
    let refused = "prompt:1: GitHub token";
    let cases = [
        (vec!["-p", inline.as_str(), "--render"], "", refused),
        (vec!["-p", "-", "--render"], inline.as_str(), refused),
        (
            vec![
                "-p",
                PROMPT,
                "--engine",
                "browser",
                "--dry-run",
                "summary",
                "--browser-follow-up",
                inline.as_str(),
            ],
            "",
            "follow-up:1: GitHub token",
        ),
    ];
    for (args, stdin, expected) in cases {
        assert_refused(&cli(home.path(), &args, stdin), expected);
    }
    fs::write(home.path().join("public.txt"), "PUBLIC").unwrap();
    let args = [
        "-p",
        inline.as_str(),
        "--file",
        "public.txt",
        "--allow-secret-file",
        "public.txt",
        "--render",
    ];
    assert_refused(&cli(home.path(), &args, ""), refused);
}
#[test]
fn a_saved_system_prompt_is_scanned() {
    let home = tempfile::tempdir().unwrap();
    let error = execution::resolve(
        &json!({"prompt": PROMPT, "system": token()}),
        &json!({}),
        &Environment::new(),
        home.path(),
    )
    .expect_err("refused");
    assert!(error.to_string().contains("system prompt:1: GitHub token"));
    assert_no_part_of_the_value(&format!("{error:#}"));
}
#[tokio::test]
async fn mcp_refuses_files_and_prompt_text_and_offers_no_override() {
    let home = tempfile::tempdir().unwrap();
    secret_file(home.path(), "secret.txt");
    let mcp = server(home.path());
    let inline = format!("{PROMPT}: {}", token());
    for (tool, input, expected) in [
        (
            "consult",
            json!({"prompt":PROMPT,"files":["secret.txt"],"engine":"browser","dryRun":true}),
            SECRET,
        ),
        (
            "consult",
            json!({"prompt":inline,"engine":"browser","dryRun":true}),
            "prompt:1: GitHub token",
        ),
        (
            "consult",
            json!({"prompt":PROMPT,"browserFollowUps":[inline],"engine":"browser","dryRun":true}),
            "follow-up:1: GitHub token",
        ),
        (
            "project_sources",
            json!({"operation":"add","files":["secret.txt"],"dryRun":true,"chatgptUrl":"https://chatgpt.com/g/g-p-public/project"}),
            SECRET,
        ),
    ] {
        let error = call(&mcp, tool, input).await.expect_err("refused");
        let text = format!("{error:#}");
        assert!(text.contains(expected), "{text}");
        assert_no_part_of_the_value(&text);
    }
    let error = call(
        &mcp,
        "consult",
        json!({"prompt":PROMPT,"files":["secret.txt"],"allowSecretFiles":["secret.txt"],"dryRun":true}),
    )
    .await
    .expect_err("undeclared key refused");
    assert!(error.to_string().contains("allowSecretFiles"), "{error:#}");
    let direct = execution::resolve_mcp(
        &json!({"prompt":PROMPT,"files":["secret.txt"],"allowSecretFiles":["secret.txt"]}),
        &json!({}),
        &Environment::new(),
        home.path(),
    )
    .expect_err("the MCP route ignores an override");
    assert!(direct.to_string().contains(SECRET), "{direct:#}");
}
#[test]
fn an_override_names_real_files_this_run_sends_and_only_those_pass() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();
    fs::create_dir(root.join("sub")).unwrap();
    secret_file(root, "secret.txt");
    secret_file(root, "other.txt");
    fs::write(root.join("public.txt"), "PUBLIC").unwrap();
    let run = |files: &[&str], allowed: &[&str]| {
        let mut args = vec!["-p", PROMPT, "--render"];
        args.extend(files.iter().flat_map(|file| ["--file", *file]));
        args.extend(
            allowed
                .iter()
                .flat_map(|file| ["--allow-secret-file", *file]),
        );
        cli(root, &args, "")
    };
    for spelling in ["secret.txt", "./sub/../secret.txt"] {
        let passed = run(&["secret.txt"], &[spelling]);
        let stderr = String::from_utf8_lossy(&passed.stderr);
        assert!(passed.status.success(), "{stderr}");
        assert!(stderr.contains(SECRET), "{stderr}");
        assert_no_part_of_the_value(&stderr);
    }
    let unnamed = run(&["secret.txt", "other.txt"], &["secret.txt"]);
    assert_refused(&unnamed, "other.txt:2: GitHub token");
    assert!(!String::from_utf8_lossy(&unnamed.stderr).contains(SECRET));
    for (allowed, expected) in [
        ("*.txt", "Cannot resolve --allow-secret-file"),
        ("public.txt", "does not send"),
        ("sub", "does not send"),
    ] {
        let output = run(&["secret.txt"], &[allowed]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{allowed}: {stderr}");
        assert!(stderr.contains(expected), "{allowed}: {stderr}");
        assert_no_part_of_the_value(&stderr);
    }
    // One run only: nothing a restart could reuse reaches the saved options.
    let request = execution::resolve(
        &json!({"prompt":PROMPT,"files":["secret.txt"],"allowSecretFiles":["secret.txt"]}),
        &json!({}),
        &Environment::new(),
        root,
    )
    .unwrap();
    assert!(request.options.get("allowSecretFiles").is_none());
}
#[test]
fn a_selection_over_the_scan_budget_is_refused_unread() {
    let home = tempfile::tempdir().unwrap();
    let mut inputs = vec![];
    for name in ["one.zip", "two.zip"] {
        let file = fs::File::create(home.path().join(name)).unwrap();
        file.set_len(600 * 1024 * 1024).unwrap();
        inputs.push(name.to_owned());
    }
    let error = prompts::resolve_files(&inputs, home.path(), 0, false).expect_err("refused");
    assert!(
        error.to_string().contains("scanned for credentials"),
        "{error:#}"
    );
}
#[cfg(unix)]
#[test]
fn a_socket_in_a_selected_folder_is_refused_not_skipped() {
    let home = tempfile::tempdir().unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(home.path().join("socket")).unwrap();
    fs::write(home.path().join("public.txt"), "PUBLIC").unwrap();
    let error = prompts::resolve_files(&[".".into()], home.path(), 0, false).expect_err("refused");
    assert!(
        error.to_string().contains("Not a regular file"),
        "{error:#}"
    );
}
/// A file swapped for one of the same size after the scan. `assemble` and `capture` read it again.
fn swapped(root: &Path, name: &str) -> Vec<prompts::PromptFile> {
    let public = "x".repeat(token().len());
    fs::write(root.join(name), &public).unwrap();
    let files = prompts::resolve_files(&[name.into()], root, 0, false).unwrap();
    fs::write(root.join(name), token()).unwrap();
    files
}
#[test]
fn a_file_changed_between_scan_and_zip_bundle_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let files = swapped(home.path(), "public.txt");
    let parent = tempfile::tempdir().unwrap();
    let error = BrowserPrompt::assemble(
        &files,
        PROMPT,
        None,
        &json!({"browserBundleFiles": true, "browserBundleFormat": "zip"}),
        parent.path(),
    )
    .err()
    .expect("refused");
    assert!(
        error
            .to_string()
            .contains("changed after its credential scan"),
        "{error:#}"
    );
}
#[test]
fn a_file_changed_between_scan_and_staging_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let files = swapped(home.path(), "public.png");
    let parent = tempfile::tempdir().unwrap();
    let plan = BrowserPrompt::assemble(&files, PROMPT, None, &json!({}), parent.path()).unwrap();
    let store = SessionStore::new(home.path().join("store"));
    let meta = store
        .create(json!({"prompt":PROMPT,"mode":"browser"}), home.path(), None)
        .unwrap();
    let error = StagedUploads::capture(&store, meta["id"].as_str().unwrap(), &plan.attachments)
        .expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("changed after its credential scan"),
        "{error:#}"
    );
}

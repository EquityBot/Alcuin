use alcuin::sessions::SessionStore;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, SystemTime},
};

const ENGINE: &str =
    "fn run() {\n    let status = \"BLOCKED\";\n    // A blank ReviewGroup is refused\n}\n";

fn ground(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ORACLE_HOME_DIR", home)
        .current_dir(home)
        .arg("ground")
        .args(args)
        .output()
        .unwrap()
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}
/// A store whose `work` folder holds `engine.rs`.
fn fixture() -> (tempfile::TempDir, SessionStore, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("engine.rs"), ENGINE).unwrap();
    let store = SessionStore::new(home.path());
    (home, store, work)
}
/// A completed API session that sent `engine.rs` from `work` and saved `answer`.
fn answered(store: &SessionStore, work: &Path, answer: &str) -> String {
    let meta = store
        .create(
            json!({"prompt":"Review the engine for silent refusals.","file":["engine.rs"],"model":"gpt-6-pro","mode":"api"}),
            work,
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap().to_owned();
    let receipt = alcuin::artifacts::capture(
        store,
        &id,
        "answer.md",
        answer.as_bytes(),
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    store
        .update(&id, |_| {
            Ok(Some(json!({"status":"completed","capturedAnswer":receipt})))
        })
        .unwrap();
    id
}

#[test]
fn a_planted_false_citation_exits_1_and_the_corrected_answer_exits_0() {
    let (home, store, work) = fixture();
    let planted = answered(
        &store,
        &work,
        "engine.rs:30 says 'A blank ReviewGroup is refused'.\n",
    );
    let output = ground(home.path(), &[&planted]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("Out of range:\n  answer line 1: engine.rs:30 (file has 4 lines)"),
        "{stdout}"
    );

    // The answer may quote the session's prompt as well as the files it sent.
    let corrected = answered(
        &store,
        &work,
        "engine.rs:3 says 'A blank ReviewGroup is refused', as the brief's 'silent refusals' asks.\n",
    );
    let output = ground(home.path(), &[&corrected, "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report,
        json!({
            "references":[{"answerLine":1,"reference":"engine.rs:3","status":"grounded","fileLines":4}],
            "quotes":[
                {"answerLine":1,"quote":"A blank ReviewGroup is refused","found":true},
                {"answerLine":1,"quote":"silent refusals","found":true}
            ],
            "changedFiles":[],
            "skippedFiles":[]
        })
    );
    let output = ground(home.path(), &[&corrected]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Grounding: 1 reference (1 grounded, 0 out of range, 0 not in the files sent); 2 quotes (2 found, 0 not found).\n"
    );
}

#[test]
fn a_file_changed_after_the_session_is_named_and_checked_as_it_is_now() {
    let (home, store, work) = fixture();
    let id = answered(&store, &work, "engine.rs:5 is new.\n");
    let engine = work.join("engine.rs");
    fs::write(&engine, format!("{ENGINE}// added later\n")).unwrap();
    fs::File::options()
        .write(true)
        .open(&engine)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    let output = ground(home.path(), &[&id, "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["references"][0]["status"], "grounded");
    assert_eq!(report["changedFiles"], json!(["engine.rs"]));
    let output = ground(home.path(), &[&id]);
    assert!(
        text(&output.stdout).contains(
            "Changed since the session was created (checked against their current text):\n  engine.rs"
        ),
        "{}",
        text(&output.stdout)
    );
}

#[test]
fn a_missing_session_or_unreadable_input_fails_with_exit_2() {
    let (home, store, work) = fixture();
    let output = ground(home.path(), &["no-such-session"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        text(&output.stderr).contains("Session \"no-such-session\" not found."),
        "{}",
        text(&output.stderr)
    );

    let unanswered = store
        .create(
            json!({"prompt":"PUBLIC pending","file":["engine.rs"],"mode":"api"}),
            &work,
            None,
        )
        .unwrap();
    let output = ground(home.path(), &[unanswered["id"].as_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("has no saved answer to check"));

    let id = answered(&store, &work, "nothing cited\n");
    fs::remove_file(work.join("engine.rs")).unwrap();
    let output = ground(home.path(), &[&id]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("Missing file or directory: engine.rs"),
        "{}",
        text(&output.stderr)
    );
}

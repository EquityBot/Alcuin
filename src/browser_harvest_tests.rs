use super::*;
use std::cell::Cell;

fn setup() -> (tempfile::TempDir, SessionStore, String) {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC historical harvest","model":"gpt-6-astra"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap().to_owned();
    (home, store, id)
}
fn observed() -> Value {
    json!({"targetId":"PUBLIC-target","url":"https://chatgpt.com/c/public-a","lastAssistantText":"abc","state":"completed","stopExists":false,"sendExists":true,"assistantCount":1})
}
fn saved(store: &SessionStore, id: &str, browser: Value, artifacts: Value) {
    store
        .update(id, |_| {
            Ok(Some(json!({"browser":browser,"artifacts":artifacts})))
        })
        .unwrap();
}
#[test]
fn matched_harvest_retains_unknown_fields_and_never_commits_completion_or_artifacts() {
    let (_home, store, id) = setup();
    saved(
        &store,
        &id,
        json!({"runtime":{"conversationId":"public-a","tabUrl":"https://chatgpt.com/c/public-a"},"harvest":{"extension":17},"warnings":[{"code":"retain"},{"code":WARNING}]}),
        json!([{"kind":"file","sourceUrl":"https://example.com/public-file","path":"PUBLIC-file"}]),
    );
    let before = store.require(&id).unwrap();
    let integrity = persist(&store, &id, &observed(), false).unwrap();
    assert_eq!(integrity.status, "matched");
    let after = store.require(&id).unwrap();
    assert_eq!(after["status"], before["status"]);
    assert_eq!(after["artifacts"], before["artifacts"]);
    assert_eq!(after["browser"]["runtime"], before["browser"]["runtime"]);
    assert_eq!(after["browser"]["harvest"]["extension"], 17);
    assert_eq!(
        after["browser"]["harvest"]["assistantHash"],
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(after["browser"]["warnings"], json!([{"code":"retain"}]));
}
#[test]
fn mismatches_are_durable_before_implicit_refusal_and_explicit_targets_can_continue() {
    let (_home, store, id) = setup();
    saved(
        &store,
        &id,
        json!({"runtime":{"conversationId":"public-b"},"harvest":{"conversationId":"public-old"}}),
        json!([]),
    );
    assert!(persist(&store, &id, &observed(), false).is_err());
    let after = store.require(&id).unwrap();
    assert_eq!(
        after["browser"]["harvest"]["integrity"]["status"],
        "mismatch"
    );
    assert_eq!(
        after["browser"]["harvest"]["integrity"]["previousHarvestConversationId"],
        "public-old"
    );
    assert_eq!(after["browser"]["warnings"][0]["code"], WARNING);
    let explicit = persist(&store, &id, &observed(), true).unwrap();
    assert_eq!(explicit.status, "mismatch");
    assert!(explicit.explicit_target);
}
#[test]
fn transcript_headers_are_bounded_and_all_saved_identities_must_agree() {
    let (_home, store, id) = setup();
    let dir = store.directory(&id).unwrap().join("artifacts");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("transcript.md"), "# Oracle Browser Transcript\r\n\r\nConversation: https://chatgpt.com/c/public-a\r\n\r\nPUBLIC answer").unwrap();
    assert_eq!(
        persist(&store, &id, &observed(), false).unwrap().status,
        "matched"
    );
    fs::write(
        dir.join("transcript.md"),
        "# Oracle Browser Transcript\n\nConversation: https://chatgpt.com/c/public-b\n",
    )
    .unwrap();
    assert!(persist(&store, &id, &observed(), false).is_err());
    fs::write(
        dir.join("transcript.md"),
        format!(
            "# Oracle Browser Transcript\n\nConversation: {}\n",
            "x".repeat(2048)
        ),
    )
    .unwrap();
    let integrity = persist(&store, &id, &observed(), false).unwrap();
    assert_eq!(integrity.status, "unverified");
    assert!(
        integrity
            .unverified_sources
            .contains(&"transcript:transcript.md".into())
    );
    assert_eq!(
        store.require(&id).unwrap()["browser"]["warnings"][0]["code"],
        WARNING
    );
}
#[test]
fn previous_harvest_is_not_captured_provenance_and_missing_defaults_are_harmless() {
    let (_home, store, id) = setup();
    saved(
        &store,
        &id,
        json!({"harvest":{"conversationId":"public-b"}}),
        json!([]),
    );
    let result = persist(&store, &id, &observed(), false).unwrap();
    assert_eq!(result.status, "unverified");
    assert!(result.captured.is_empty());
    assert!(result.unverified_sources.is_empty());
    saved(
        &store,
        &id,
        json!({"runtime":{"conversationId":"public-a"}}),
        json!([{"kind":"transcript","path":"artifacts/missing.md"},{"kind":"transcript","path":"../outside.md"}]),
    );
    let dir = store.directory(&id).unwrap().join("artifacts");
    fs::create_dir(&dir).unwrap();
    let result = persist(&store, &id, &observed(), false).unwrap();
    assert_eq!(result.status, "unverified");
    assert!(
        result
            .unverified_sources
            .contains(&"external-transcript-path".into())
    );
    assert!(
        result
            .unverified_sources
            .contains(&"transcript:missing.md".into())
    );
    assert!(
        !result
            .unverified_sources
            .contains(&"transcript:transcript.md".into())
    );
}
#[test]
fn relative_and_unnormalised_homes_accept_recorded_direct_child_transcripts() {
    let root = tempfile::Builder::new()
        .prefix("PUBLIC-harvest-")
        .tempdir_in(".")
        .unwrap();
    for home in [
        root.path().join("relative"),
        root.path().join("existing").join("..").join("normalised"),
    ] {
        fs::create_dir_all(root.path().join("existing")).unwrap();
        let store = SessionStore::new(&home);
        let meta = store
            .create(
                json!({"prompt":"PUBLIC path harvest","model":"gpt-6-astra"}),
                &home,
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap();
        let dir = store.directory(id).unwrap().join("artifacts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("transcript.md"),
            "# Oracle Browser Transcript\n\nConversation: https://chatgpt.com/c/public-a\n",
        )
        .unwrap();
        saved(
            &store,
            id,
            json!({}),
            json!([{"kind":"transcript","path":"artifacts/transcript.md"}]),
        );
        assert_eq!(
            persist(&store, id, &observed(), false).unwrap().status,
            "matched"
        );
    }
}
#[test]
fn metadata_churn_retries_without_discarding_concurrent_fields_then_records_unverified() {
    let (_home, store, id) = setup();
    let attempts = Cell::new(0);
    let harvest = observed();
    let result = persist_with(&store, &id, &harvest, false, |meta, directory| {
        attempts.set(attempts.get()+1);
        let result = inspect(meta, &harvest, false, directory);
        store.update(&id, |current| Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"archive":{"conversationUrl":format!("https://chatgpt.com/c/public-{}",attempts.get())},"concurrent":attempts.get()}))})))).unwrap();
        Ok(result)
    }).unwrap();
    assert_eq!(attempts.get(), 3);
    assert_eq!(result.status, "unverified");
    assert!(
        result
            .unverified_sources
            .contains(&"transcript-inspection-changed".into())
    );
    assert_eq!(store.require(&id).unwrap()["browser"]["concurrent"], 3);
}
#[cfg(unix)]
#[test]
fn home_symlink_followed_by_parent_uses_the_actual_session_and_not_lexical_neighbours() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(b.join("child")).unwrap();
    symlink(b.join("child"), a.join("link")).unwrap();
    let home = a.join("link").join("..").join("home");
    let store = SessionStore::new(&home);
    let meta = store
        .create(
            json!({"prompt":"PUBLIC symlink home","model":"gpt-6-astra"}),
            &home,
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let actual = store.directory(id).unwrap().join("artifacts");
    fs::create_dir_all(&actual).unwrap();
    fs::write(
        actual.join("transcript.md"),
        "# Oracle Browser Transcript\n\nConversation: https://chatgpt.com/c/public-a\n",
    )
    .unwrap();
    let neighbour = a.join("home").join("sessions").join(id).join("artifacts");
    fs::create_dir_all(&neighbour).unwrap();
    fs::write(
        neighbour.join("transcript.md"),
        "# Oracle Browser Transcript\n\nConversation: https://chatgpt.com/c/public-other\n",
    )
    .unwrap();
    saved(
        &store,
        id,
        json!({}),
        json!([{"kind":"transcript","path":"artifacts/transcript.md"},{"kind":"transcript","path":actual.join("transcript.md")} ]),
    );
    assert_eq!(
        persist(&store, id, &observed(), false).unwrap().status,
        "matched"
    );
}
#[cfg(unix)]
#[test]
fn linked_transcripts_and_fifo_entries_are_unverified_without_following_or_blocking() {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    let (_home, store, id) = setup();
    let dir = store.directory(&id).unwrap().join("artifacts");
    fs::create_dir(&dir).unwrap();
    let outside = store.home().join("PUBLIC-outside.md");
    fs::write(
        &outside,
        "# Oracle Browser Transcript\n\nConversation: https://chatgpt.com/c/public-a\n",
    )
    .unwrap();
    symlink(&outside, dir.join("transcript.md")).unwrap();
    let result = persist(&store, &id, &observed(), false).unwrap();
    assert!(result.captured.is_empty());
    assert_eq!(result.unverified_sources, ["transcript:transcript.md"]);
    fs::remove_file(dir.join("transcript.md")).unwrap();
    let fifo = CString::new(dir.join("transcript.md").as_os_str().as_bytes()).unwrap();
    // SAFETY: the fixture owns this fresh path and the NUL-terminated bytes live through the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(
        persist(&store, &id, &observed(), false).unwrap().status,
        "unverified"
    );
    fs::remove_file(dir.join("transcript.md")).unwrap();
    fs::remove_dir(&dir).unwrap();
    symlink(store.home(), &dir).unwrap();
    assert_eq!(
        persist(&store, &id, &observed(), false)
            .unwrap()
            .unverified_sources,
        ["artifact-directory"]
    );
}

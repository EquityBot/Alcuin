use alcuin::{
    artifacts,
    process::{Liveness, ProcessIdentity},
    sessions::{self, SessionStore, merge_artifacts, merge_browser},
    storage::{self, DirectoryLock},
};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[test]
fn session_ids_match_immutable_fixture() {
    let fixture: Vec<Value> =
        serde_json::from_str(include_str!("../fixtures/session-ids.json")).unwrap();
    for case in fixture {
        assert_eq!(
            sessions::valid_id(case["input"].as_str().unwrap_or("")),
            case["accepted"].as_bool().unwrap(),
            "{case}"
        );
    }
    assert_eq!(
        sessions::create_id("Hello, WORLD??? -- Example", None).unwrap(),
        "hello-world-example"
    );
    assert_eq!(
        sessions::create_id("abcdefghijklm nopqrstuvwxyz shorty", None).unwrap(),
        "abcdefghij-nopqrstuvw-shorty"
    );
    assert!(sessions::create_id("ignored", Some("only two")).is_err());
    assert_eq!(sessions::safe_model_slug("openai/a:b\\x"), "openai__a_b__x");
}
fn session(store: &SessionStore) -> Value {
    store.create(json!({"prompt":"PUBLIC session contract","model":"gpt-6-astra","legacyExtension":{"retain":true}}),store.home(),None).unwrap()
}
#[test]
fn guarded_log_checks_current_model_before_writing_and_releases_a_rejected_transaction() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let model = meta["model"].as_str().unwrap();
    store
        .update_model(
            id,
            model,
            &json!({"status":"partial","marker":"PUBLIC winning model outcome"}),
        )
        .unwrap();
    let before = store.log(id).unwrap();
    assert!(
        store
            .append_log_guarded(id, Some(model), "PUBLIC rejected answer", |current, run| {
                assert_eq!(current["id"], id);
                assert_eq!(run.unwrap()["marker"], "PUBLIC winning model outcome");
                anyhow::bail!("PUBLIC terminal model refuses log")
            })
            .is_err()
    );
    assert_eq!(store.log(id).unwrap(), before);
    store
        .append_log_guarded(id, Some(model), "PUBLIC permitted log", |_, _| Ok(()))
        .unwrap();
    assert!(store.log(id).unwrap().contains("PUBLIC permitted log"));
}
#[test]
fn malformed_modern_bytes_refuse_writes_and_preserve_legacy_and_unknown_fields() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let dir = store.directory(id).unwrap();
    storage::atomic_json(&dir.join("session.json"), &meta).unwrap();
    for raw in [
        "{broken",
        "null",
        "{\"id\":\"other-session\",\"status\":\"running\"}",
    ] {
        fs::write(dir.join("meta.json"), raw).unwrap();
        assert!(
            store
                .update(id, |_| Ok(Some(json!({"status":"error"}))))
                .is_err()
        );
        assert_eq!(fs::read_to_string(dir.join("meta.json")).unwrap(), raw);
    }
    fs::remove_file(dir.join("meta.json")).unwrap();
    store
        .update(id, |_| Ok(Some(json!({"unknownHistorical":{"retain":17}}))))
        .unwrap();
    store
        .update(id, |_| Ok(Some(json!({"status":"completed"}))))
        .unwrap();
    assert_eq!(
        store.require(id).unwrap()["unknownHistorical"]["retain"],
        17
    );
    assert_eq!(
        store.request(id).unwrap().unwrap()["legacyExtension"]["retain"],
        true
    );
    let before = fs::read(dir.join("meta.json")).unwrap();
    store.require(id).unwrap();
    store.list(true).unwrap();
    assert_eq!(fs::read(dir.join("meta.json")).unwrap(), before);
}
#[test]
fn mismatched_ids_and_case_distinct_directories_do_not_redirect_reads() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    store.ensure_storage().unwrap();
    for id in ["Case-Session", "case-session"] {
        let dir = store.directory(id).unwrap();
        storage::private_dir(&dir).unwrap();
        storage::atomic_json(&dir.join("meta.json"),&json!({"id":"case-session","status":"completed","createdAt":"2026-10-13T00:00:00Z","options":{}})).unwrap();
    }
    #[cfg(target_os = "linux")]
    assert!(store.read("Case-Session").unwrap().is_none());
    #[cfg(target_os = "linux")]
    assert_eq!(store.list(true).unwrap().len(), 1);
    for id in ["../outside", "/tmp/session", "a/b", ".x", "abc.", ""] {
        assert!(store.read(id).is_err());
        assert!(store.update(id, |_| Ok(None)).is_err());
    }
}
#[test]
fn concurrent_allocation_and_current_record_updates_do_not_lose_fields() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(SessionStore::new(home.path()));
    let threads = (0..4)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || session(&store)["id"].as_str().unwrap().to_owned())
        })
        .collect::<Vec<_>>();
    let mut ids = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    let id = ids[0].clone();
    let threads = (0..4)
        .map(|i| {
            let store = store.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                for _ in 0..5 {
                    store
                        .update(&id, |current| {
                            let mut counters =
                                current["counters"].as_object().cloned().unwrap_or_default();
                            let key = i.to_string();
                            let n = counters.get(&key).and_then(Value::as_u64).unwrap_or(0);
                            counters.insert(key, json!(n + 1));
                            Ok(Some(json!({"counters":counters})))
                        })
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    let record = store.require(&id).unwrap();
    for i in 0..4 {
        assert_eq!(record["counters"][i.to_string()], 5);
    }
}
#[test]
fn stale_producer_patches_keep_newer_browser_and_artifact_evidence() {
    let basis = json!({"config":{"timeoutMs":100},"runtime":{"chromePort":9222,"submittedPromptHash":"old"},"warnings":[{"code":"first","message":"old"}]});
    let current = json!({"config":{"timeoutMs":200},"runtime":{"chromePort":9222,"submittedPromptHash":"old"},"harvest":{"conversationId":"capture"},"warnings":[{"code":"first","message":"new"},{"code":"second","message":"keep"}]});
    let mut next = basis.clone();
    next["runtime"]["submittedPromptHash"] = Value::Null;
    next["runtime"]["chromeTargetId"] = json!("new-target");
    let merged = merge_browser(&current, &next, &basis);
    assert_eq!(merged["config"]["timeoutMs"], 200);
    assert_eq!(merged["warnings"], current["warnings"]);
    assert_eq!(merged["runtime"]["submittedPromptHash"], Value::Null);
    assert_eq!(merged["harvest"], current["harvest"]);
    let old =
        json!([{"kind":"file","path":"fixture.zip","label":"old","transfer":{"status":"ready"}}]);
    let new = json!([{"kind":"file","path":"fixture.zip","label":"new","transfer":{"status":"completed","bytes":17}}]);
    let merged = merge_artifacts(&new, &old, &old);
    assert_eq!(merged, new);
    let changed =
        json!([{"kind":"file","path":"fixture.zip","validation":{"type":"generic","ok":true}}]);
    assert_eq!(
        merge_artifacts(&new, &changed, &Value::Null)[0]["validation"],
        changed[0]["validation"]
    );
}
#[tokio::test]
async fn cancelled_or_timed_out_wait_does_not_cancel_or_rewrite_the_session() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let before = fs::read(store.directory(id).unwrap().join("meta.json")).unwrap();
    assert!(
        store
            .wait(id, Some(Duration::ZERO), &CancellationToken::new())
            .await
            .unwrap()
            .1
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(store.wait(id, None, &cancel).await.is_err());
    assert_eq!(
        fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
        before
    );
}
#[test]
fn lock_owner_replacement_is_preserved_on_release() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("example.lock");
    let lock = DirectoryLock::acquire(&path, Duration::from_secs(1)).unwrap();
    let mut owner: Value =
        serde_json::from_slice(&fs::read(path.join("owner.json")).unwrap()).unwrap();
    owner["id"] = json!("replacement-owner");
    fs::rename(
        path.join("owner.json"),
        home.path().join("original-owner.json"),
    )
    .unwrap();
    storage::atomic_json(&path.join("owner.json"), &owner).unwrap();
    assert!(lock.release().is_err());
    assert!(path.exists());
    assert!(DirectoryLock::acquire(&path, Duration::from_millis(25)).is_err());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(path.join("owner.json")).unwrap()).unwrap()["id"],
        "replacement-owner"
    );
}
#[test]
fn dead_lock_owner_is_recovered_but_unreadable_ownership_is_preserved() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("example.lock");
    storage::private_dir(&path).unwrap();
    let identity = ProcessIdentity {
        pid: u32::MAX,
        started_at_ms: 1.0,
        native_start: 1,
    };
    storage::atomic_json(&path.join("owner.json"),&json!({"id":"dead","pid":identity.pid,"createdAt":"2025-01-01T00:00:00Z","processStartedAtMs":1.0,"processIdentity":identity})).unwrap();
    DirectoryLock::acquire(&path, Duration::from_secs(1))
        .unwrap()
        .release()
        .unwrap();
    assert!(!path.exists());
    storage::private_dir(&path).unwrap();
    fs::write(path.join("owner.json"), "{broken").unwrap();
    assert!(DirectoryLock::acquire(&path, Duration::from_millis(25)).is_err());
    assert_eq!(
        fs::read_to_string(path.join("owner.json")).unwrap(),
        "{broken"
    );
}
#[test]
fn native_pid_reuse_requires_the_exact_process_start() {
    let identity = ProcessIdentity::current().unwrap();
    assert_eq!(identity.liveness(), Liveness::Alive);
    let mut reused = identity.clone();
    reused.native_start += 1;
    assert_eq!(reused.liveness(), Liveness::Dead);
}
#[test]
fn verified_capture_and_no_replace_publication_preserve_exact_bytes() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let cancel = CancellationToken::new();
    let bytes = "  PUBLIC café 🐕\r\nanswer\n\n ".as_bytes();
    let receipt =
        artifacts::capture(&store, id, "captured-answer-test.md", bytes, &cancel).unwrap();
    assert_eq!(artifacts::verify_capture(&store, &receipt).unwrap(), bytes);
    let output = home.path().join("output.md");
    let published = artifacts::publish(bytes, &output, false, &cancel).unwrap();
    assert_eq!(fs::read(&published.path).unwrap(), bytes);
    assert!(artifacts::publish(b"replacement", &output, false, &cancel).is_err());
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert_eq!(
        artifacts::publish(b"next", &output, true, &cancel)
            .unwrap()
            .path,
        home.path().join("output-2.md")
    );
    fs::write(
        store.directory(id).unwrap().join(&receipt.file),
        b"tampered",
    )
    .unwrap();
    assert!(artifacts::verify_capture(&store, &receipt).is_err());
    cancel.cancel();
    assert!(
        artifacts::publish(
            b"cancelled",
            &home.path().join("cancelled.md"),
            false,
            &cancel
        )
        .is_err()
    );
    assert!(!home.path().join("cancelled.md").exists());
}

#[cfg(not(windows))]
#[test]
fn replacement_directory_with_reused_owner_is_preserved_on_release() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("example.lock");
    let moved = home.path().join("original.lock");
    let lock = DirectoryLock::acquire(&path, Duration::from_secs(1)).unwrap();
    let raw = fs::read(path.join("owner.json")).unwrap();
    fs::rename(&path, &moved).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("owner.json"), &raw).unwrap();
    assert!(lock.release().is_err());
    assert_eq!(fs::read(path.join("owner.json")).unwrap(), raw);
    assert_eq!(fs::read(moved.join("owner.json")).unwrap(), raw);
}
#[cfg(windows)]
#[test]
fn windows_lock_child_witness_blocks_replacement_and_release_removes_only_owned_contents() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("example.lock");
    let lock = DirectoryLock::acquire(&path, Duration::from_secs(1)).unwrap();
    let raw = fs::read(path.join("owner.json")).unwrap();
    let moved = home.path().join("replacement.lock");
    assert_eq!(
        fs::rename(&path, &moved).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(fs::read(path.join("owner.json")).unwrap(), raw);
    lock.release().unwrap();
    assert!(!path.exists());
    assert!(!moved.exists());
}
#[test]
fn unknown_lock_contents_survive_release_and_dead_owner_recovery() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("example.lock");
    let lock = DirectoryLock::acquire(&path, Duration::from_secs(1)).unwrap();
    fs::write(path.join("unrecognised"), b"PUBLIC preserve").unwrap();
    assert!(lock.release().is_err());
    assert_eq!(
        fs::read(path.join("unrecognised")).unwrap(),
        b"PUBLIC preserve"
    );
    let identity = ProcessIdentity {
        pid: u32::MAX,
        started_at_ms: 1.0,
        native_start: 1,
    };
    storage::atomic_json(
        &path.join("owner.json"),
        &json!({"id":"dead","pid":identity.pid,"processIdentity":identity}),
    )
    .unwrap();
    let before = fs::read(path.join("owner.json")).unwrap();
    assert!(DirectoryLock::acquire(&path, Duration::from_millis(25)).is_err());
    assert_eq!(fs::read(path.join("owner.json")).unwrap(), before);
    assert_eq!(
        fs::read(path.join("unrecognised")).unwrap(),
        b"PUBLIC preserve"
    );
}
#[test]
fn malformed_unknown_and_symlinked_lock_owners_cannot_authorise_recovery() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("example.lock");
    fs::create_dir(&path).unwrap();
    for owner in [
        json!({"id":"unknown","pid":0}),
        json!({"id":"unknown","pid":std::process::id(),"processIdentity":{"pid":std::process::id(),"nativeStart":0,"startedAtMs":1.0}}),
        json!({"id":"unknown","pid":std::process::id()}),
    ] {
        storage::atomic_json(&path.join("owner.json"), &owner).unwrap();
        let before = fs::read(path.join("owner.json")).unwrap();
        assert!(DirectoryLock::acquire(&path, Duration::from_millis(25)).is_err());
        assert_eq!(fs::read(path.join("owner.json")).unwrap(), before);
    }
    #[cfg(unix)]
    {
        fs::remove_file(path.join("owner.json")).unwrap();
        fs::write(home.path().join("foreign.json"), b"{broken").unwrap();
        std::os::unix::fs::symlink(home.path().join("foreign.json"), path.join("owner.json"))
            .unwrap();
        assert!(DirectoryLock::acquire(&path, Duration::from_millis(25)).is_err());
        assert!(
            fs::symlink_metadata(path.join("owner.json"))
                .unwrap()
                .is_symlink()
        );
    }
}
#[test]
fn historical_absolute_log_reads_do_not_grant_log_write_authority() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let historical = home.path().join("historical-public.log");
    fs::write(&historical, b"PUBLIC old answer\n").unwrap();
    store
        .update_model(
            id,
            "gpt-6-astra",
            &json!({"log":{"path":historical},"status":"completed"}),
        )
        .unwrap();
    assert!(store.log(id).unwrap().contains("PUBLIC old answer"));
    assert!(
        store
            .log_for_model(id, " GPT-6-ASTRA ")
            .unwrap()
            .contains("PUBLIC old answer")
    );
    store
        .append_log(id, Some("gpt-6-astra"), "PUBLIC new write\n")
        .unwrap();
    assert_eq!(fs::read(historical).unwrap(), b"PUBLIC old answer\n");
    assert!(
        fs::read_to_string(store.directory(id).unwrap().join("models/gpt-6-astra.log"))
            .unwrap()
            .contains("PUBLIC new write")
    );
}

#[test]
fn historical_metadata_only_model_log_selection_uses_the_stored_model_name() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let id = "public-legacy-model-log";
    let dir = store.directory(id).unwrap();
    fs::create_dir_all(dir.join("models")).unwrap();
    fs::write(
        dir.join("meta.json"),
        serde_json::to_vec(&json!({"id":id,"status":"completed","model":"Legacy/Model"})).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("output.log"), b"PUBLIC legacy root fallback\n").unwrap();
    let path = dir.join("models/Legacy__Model.log");
    fs::write(&path, "\u{feff} \n").unwrap();
    assert_eq!(
        store.log_for_model(id, " legacy/MODEL ").unwrap(),
        "PUBLIC legacy root fallback\n"
    );
    fs::write(&path, b"PUBLIC legacy selected answer\n").unwrap();
    assert_eq!(
        store.log_for_model(id, "LEGACY/model").unwrap(),
        "PUBLIC legacy selected answer\n"
    );
    assert!(store.log_for_model(id, "unknown").is_err());
}

#[test]
fn session_directory_replacement_during_transaction_cannot_redirect_a_metadata_write() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let path = store.directory(id).unwrap();
    let moved = store.root().join("original-session");
    let result = store.update(id, |_| {
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("meta.json"), b"PUBLIC foreign record").unwrap();
        Ok(Some(json!({"status":"completed"})))
    });
    assert!(result.is_err());
    assert_eq!(
        fs::read(path.join("meta.json")).unwrap(),
        b"PUBLIC foreign record"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(moved.join("meta.json")).unwrap()).unwrap(),
        meta
    );
}
#[cfg(unix)]
#[test]
fn historical_symlink_logs_remain_readable_but_cannot_receive_new_writes() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = session(&store);
    let id = meta["id"].as_str().unwrap();
    let path = store.directory(id).unwrap().join("models/gpt-6-astra.log");
    let external = home.path().join("historical-public.log");
    fs::write(&external, b"PUBLIC historical bytes").unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&external, &path).unwrap();
    assert!(store.log(id).unwrap().contains("PUBLIC historical bytes"));
    assert!(
        store
            .append_log(id, Some("gpt-6-astra"), "PUBLIC new write")
            .is_err()
    );
    assert_eq!(fs::read(&external).unwrap(), b"PUBLIC historical bytes");
}

#[test]
#[ignore = "Subprocess fixture, invoked only by native_lock_survives_owner_crash_and_is_recovered"]
fn synthetic_lock_owner_helper() {
    let Some(directory) = std::env::var_os("ORACLE_SYNTHETIC_LOCK_OWNER") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let _lock =
        DirectoryLock::acquire(&directory.join("process.lock"), Duration::from_secs(1)).unwrap();
    storage::atomic_json(
        &directory.join("ready.json"),
        &json!(ProcessIdentity::current().unwrap()),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[test]
fn native_lock_survives_owner_crash_and_is_recovered() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("process.lock");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "synthetic_lock_owner_helper",
            "--nocapture",
        ])
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_SYNTHETIC_LOCK_OWNER", home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !home.path().join("ready.json").exists() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Synthetic native lock owner did not acknowledge start within 5 seconds");
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "Synthetic native lock owner exited before acknowledgement"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let identity: ProcessIdentity =
        serde_json::from_slice(&fs::read(home.path().join("ready.json")).unwrap()).unwrap();
    assert_eq!(identity.pid, child.id());
    assert_eq!(identity.liveness(), Liveness::Alive);
    let before = fs::read(path.join("owner.json")).unwrap();
    assert!(DirectoryLock::acquire(&path, Duration::from_millis(25)).is_err());
    assert_eq!(fs::read(path.join("owner.json")).unwrap(), before);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(identity.liveness(), Liveness::Dead);
    assert_eq!(fs::read(path.join("owner.json")).unwrap(), before);
    DirectoryLock::acquire(&path, Duration::from_secs(1))
        .unwrap()
        .release()
        .unwrap();
    assert!(!path.exists());
}
fn running(store: &SessionStore, slug: &str, age: chrono::Duration, extra: Value) -> String {
    let created = store
        .create(
            json!({"prompt":"PUBLIC zombie contract","model":"gpt-6-astra"}),
            store.home(),
            Some(slug),
        )
        .unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    let started = (chrono::Utc::now() - age).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    store
        .update(&id, |current| {
            let mut patch = json!({"status":"running","startedAt":started,"mode":"api"});
            sessions::apply_patch(&mut patch, &extra)?;
            let _ = current;
            Ok(Some(patch))
        })
        .unwrap();
    id
}
#[test]
fn zombie_rule_applies_only_when_the_worker_cannot_be_verified() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let hours = chrono::Duration::hours;
    let current = ProcessIdentity::current().unwrap();
    let zombie = |id: &str| store.reconcile_unverified_zombie(id).unwrap();
    // Unverifiable workers past the default hour are marked; dispatch evidence keeps recovery.
    let plain = running(&store, "plain", hours(2), json!({}));
    let marked = zombie(&plain);
    assert_eq!(marked["status"], "error");
    assert_eq!(
        marked["errorMessage"],
        "Session marked as zombie (> 1h 0m stale)"
    );
    assert_eq!(marked["recoveryRequired"], false);
    let dispatched = running(
        &store,
        "dispatched",
        hours(2),
        json!({"dispatch":{"state":"started"},"lifecycle":{"processIdentity":null,"workerPid":u32::MAX}}),
    );
    let marked = zombie(&dispatched);
    assert_eq!(marked["status"], "error");
    assert_eq!(marked["recoveryRequired"], true);
    assert_eq!(marked["dispatch"]["state"], "started");
    // A verified live worker, a live bare PID, youth and recovery-bearing browser state are kept.
    for (slug, age, extra) in [
        (
            "verified",
            hours(3),
            json!({"lifecycle":{"processIdentity":current}}),
        ),
        (
            "bare-pid",
            hours(3),
            json!({"lifecycle":{"workerPid":current.pid}}),
        ),
        ("young", chrono::Duration::minutes(10), json!({})),
        (
            "long-timeout",
            chrono::Duration::minutes(90),
            json!({"options":{"timeoutSeconds":7200}}),
        ),
        (
            "browser-target",
            hours(3),
            json!({"mode":"browser","browser":{"runtime":{"chromeTargetId":"PUBLIC-target"}}}),
        ),
    ] {
        let id = running(&store, slug, age, extra);
        assert_eq!(zombie(&id)["status"], "running", "{slug}");
    }
    let browser = running(&store, "browser-empty", hours(3), json!({"mode":"browser"}));
    assert_eq!(zombie(&browser)["status"], "error");
    // An explicit zombie timeout wins, including a shorter one than the default.
    let explicit = running(
        &store,
        "explicit",
        chrono::Duration::seconds(3),
        json!({"options":{"zombieTimeoutMs":1000}}),
    );
    assert_eq!(
        zombie(&explicit)["errorMessage"],
        "Session marked as zombie (> 1s stale)"
    );
    // Last-activity mode anchors on the newest session or model log.
    let active = running(
        &store,
        "active",
        hours(3),
        json!({"options":{"zombieUseLastActivity":true}}),
    );
    store
        .append_log(&active, None, "PUBLIC recent output\n")
        .unwrap();
    assert_eq!(zombie(&active)["status"], "running");
    let idle = running(
        &store,
        "idle",
        hours(3),
        json!({"options":{"zombieUseLastActivity":true}}),
    );
    // Creation writes the session and model logs; age them so no activity follows the start.
    let dir = store.directory(&idle).unwrap();
    let models = fs::read_dir(dir.join("models"))
        .into_iter()
        .flatten()
        .flatten();
    for path in std::iter::once(dir.join("output.log")).chain(models.map(|entry| entry.path())) {
        if let Ok(file) = fs::File::options().write(true).open(&path) {
            file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3 * 3600))
                .unwrap();
        }
    }
    assert_eq!(zombie(&idle)["status"], "error");
}
#[test]
fn status_listing_marks_unverifiable_zombies_and_leaves_live_workers() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let stale = running(&store, "stale", chrono::Duration::hours(2), json!({}));
    let live = running(
        &store,
        "live",
        chrono::Duration::hours(2),
        json!({"lifecycle":{"processIdentity":ProcessIdentity::current().unwrap()}}),
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .current_dir(home.path())
        .args(["status", "--hours", "24"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let row = |id: &str| {
        stdout
            .lines()
            .find(|line| line.ends_with(id))
            .unwrap()
            .to_owned()
    };
    assert!(row(&stale).starts_with("error"), "{stdout}");
    assert!(row(&live).starts_with("running"), "{stdout}");
    assert_eq!(store.require(&stale).unwrap()["status"], "error");
    assert_eq!(store.require(&live).unwrap()["status"], "running");
}
#[tokio::test]
async fn internal_provider_attempts_are_addressable_but_do_not_clog_public_session_lists() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let parent = store
        .create(
            json!({"prompt":"PUBLIC consultation","model":"gpt-6-pro","engine":"browser"}),
            home.path(),
            Some("public-parent"),
        )
        .unwrap();
    let child=store.create(json!({"prompt":"PUBLIC provider attempt","model":"gpt-6-pro","engine":"browser","providerAttempt":{"version":1,"role":"browser-provider-attempt"}}),home.path(),Some("public-child")).unwrap();
    let parent_id = parent["id"].as_str().unwrap();
    let child_id = child["id"].as_str().unwrap();
    assert_eq!(store.list(true).unwrap().len(), 2);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .current_dir(home.path())
        .args(["status", "--all"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(parent_id), "{stdout}");
    assert!(!stdout.contains(child_id), "{stdout}");
    let server = alcuin::mcp::AlcuinMcp {
        store: store.clone(),
        cwd: home.path().to_owned(),
        env: Default::default(),
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let listed = server
        .invoke("sessions", json!({"includeAll":true}), &cancel)
        .await
        .unwrap();
    let listed = listed.structured_content.unwrap();
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["entries"][0]["id"], parent_id);
    let direct = server
        .invoke("sessions", json!({"id":child_id}), &cancel)
        .await
        .unwrap();
    assert_eq!(
        direct.structured_content.unwrap()["entries"][0]["id"],
        child_id
    );
    assert_eq!(store.list(false).unwrap().len(), 2);
}

//! Session record parity with the reference sessionManager and sessionMetadataPatches.
use alcuin::sessions::{self, SessionStore};
use serde_json::{Value, json};

fn store() -> (tempfile::TempDir, SessionStore) {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    (home, store)
}

#[test]
fn new_sessions_keep_the_given_models_and_a_utf16_preview() {
    let (home, store) = store();
    let prompt = format!("{}\u{1F980}PUBLIC", "x".repeat(159));
    let meta = store
        .create(json!({"prompt":prompt}), home.path(), None)
        .unwrap();
    assert_eq!(meta["models"], json!([]));
    assert!(meta.get("model").is_none());
    assert_eq!(meta["options"]["models"], json!([]));
    // The crab needs two UTF-16 units, so the 160-unit cut drops it.
    assert_eq!(meta["promptPreview"], "x".repeat(159));
    let id = meta["id"].as_str().unwrap();
    assert!(
        store
            .directory(id)
            .unwrap()
            .join("models/gpt-6-astra.json")
            .is_file()
    );
    let meta = store
        .create(
            json!({"prompt":"PUBLIC named","model":"gpt-6-pro"}),
            home.path(),
            None,
        )
        .unwrap();
    assert_eq!(meta["model"], "gpt-6-pro");
    assert_eq!(
        meta["models"],
        json!([{"model":"gpt-6-pro","status":"pending"}])
    );
}

#[test]
fn only_a_missing_record_may_lack_a_status() {
    let (home, store) = store();
    let meta = store
        .create(json!({"prompt":"PUBLIC stored"}), home.path(), None)
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let path = store.directory(id).unwrap().join("meta.json");
    std::fs::write(&path, json!({"id":id}).to_string()).unwrap();
    assert_eq!(
        store
            .update(id, |_| Ok(Some(json!({"status":"running"}))))
            .unwrap_err()
            .to_string(),
        format!("Invalid stored metadata for session {id}.")
    );
    std::fs::remove_file(&path).unwrap();
    let updated = store
        .update(id, |current| {
            assert_eq!(*current, json!({"id":id}));
            Ok(Some(json!({"status":"running"})))
        })
        .unwrap();
    assert_eq!(updated["status"], "running");
}

#[test]
fn merges_key_warnings_and_artifacts_and_skip_non_object_sections() {
    let warning = |code: &str, text: &str| json!({"code":code,"message":text});
    let current = json!({"config":{"PUBLIC":true},"warnings":[warning("a","old"),warning("b","kept"),warning("a","newer")]});
    let merged = sessions::merge_browser(
        &current,
        &json!({"config":null,"runtime":null,"warnings":[warning("c","added")]}),
        &Value::Null,
    );
    assert_eq!(merged["config"], json!({"PUBLIC":true}));
    assert!(merged.get("runtime").is_none());
    assert_eq!(
        merged["warnings"],
        json!([
            warning("a", "newer"),
            warning("b", "kept"),
            warning("c", "added")
        ])
    );
    let emptied = sessions::merge_browser(
        &json!({"warnings":[]}),
        &json!({"warnings":[]}),
        &Value::Null,
    );
    assert!(emptied.get("warnings").is_none());
    let artifact = |path: &str, bytes: u64| json!({"kind":"file","path":path,"bytes":bytes});
    assert_eq!(
        sessions::merge_artifacts(
            &json!([artifact("a", 1), artifact("b", 2), artifact("a", 3)]),
            &json!([]),
            &Value::Null
        ),
        json!([artifact("a", 3), artifact("b", 2)])
    );
}

/// Windows refuses to replace a file held open without delete sharing.
#[cfg(windows)]
#[test]
fn only_model_writes_explain_an_uncommitted_session_record() {
    use std::os::windows::fs::OpenOptionsExt;
    let (home, store) = store();
    let meta = store
        .create(json!({"prompt":"PUBLIC commit"}), home.path(), None)
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ only
        .open(store.directory(id).unwrap().join("meta.json"))
        .unwrap();
    for (model_patch, explained) in [(Some(json!({"status":"running"})), true), (None, false)] {
        let error = store
            .update_session_and_model(id, "gpt-6-astra", |_, _| {
                Ok(Some((json!({"status":"running"}), model_patch.clone())))
            })
            .unwrap_err()
            .to_string();
        assert_eq!(
            error
                == format!(
                    "Model metadata for gpt-6-astra was written, but session {id} metadata was not committed."
                ),
            explained,
            "{error}"
        );
    }
    drop(held);
}

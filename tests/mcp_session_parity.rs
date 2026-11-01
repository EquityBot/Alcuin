use alcuin::{
    config::Environment,
    mcp::{AlcuinMcp, add_summaries},
    process::{Liveness, ProcessIdentity},
    sessions::SessionStore,
};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

fn server(home: &Path) -> AlcuinMcp {
    AlcuinMcp {
        store: SessionStore::new(home),
        cwd: home.into(),
        env: Environment::new(),
    }
}
fn output_schema(name: &str) -> Value {
    let tool = AlcuinMcp::tools()
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap();
    Value::Object((*tool.output_schema.unwrap()).clone())
}
fn input_schema(name: &str) -> Value {
    let tool = AlcuinMcp::tools()
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap();
    Value::Object((*tool.input_schema).clone())
}
/// The subset of JSON Schema the fixture uses, with `additionalProperties:false` enforced at
/// every level, as strict MCP clients do for structured results.
fn conforms(value: &Value, schema: &Value, path: &str) {
    if let Some(expected) = schema.get("const") {
        assert_eq!(value, expected, "{path}");
    }
    if let Some(allowed) = schema["enum"].as_array() {
        assert!(allowed.contains(value), "{path}: {value} not in enum");
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    assert!(
        types.is_empty()
            || types.iter().any(|kind| match *kind {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.is_i64() || value.is_u64(),
                "boolean" => value.is_boolean(),
                "null" => value.is_null(),
                _ => true,
            }),
        "{path}: {value} is not {types:?}"
    );
    if let (Some(object), Some(properties)) = (value.as_object(), schema["properties"].as_object())
    {
        for key in schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            assert!(object.contains_key(key), "{path}.{key}: required");
        }
        for (key, child) in object {
            match properties.get(key) {
                Some(child_schema) => conforms(child, child_schema, &format!("{path}.{key}")),
                None => assert_ne!(
                    schema["additionalProperties"], false,
                    "{path}: unexpected property {key}"
                ),
            }
        }
    }
    if let (Some(items), Some(schema)) = (value.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            conforms(item, schema, &format!("{path}[{index}]"));
        }
    }
}
fn structured(result: rmcp::model::CallToolResult) -> Value {
    result.structured_content.unwrap()
}
fn dead_identity() -> ProcessIdentity {
    let mut dead = ProcessIdentity::current().unwrap();
    dead.pid = u32::MAX;
    dead.started_at_ms = 1.0;
    assert_eq!(dead.liveness(), Liveness::Dead);
    dead
}
fn running_with(store: &SessionStore, home: &Path, identity: &ProcessIdentity) -> String {
    let meta = store
        .create(json!({"prompt":"PUBLIC dead worker parity"}), home, None)
        .unwrap();
    let id = meta["id"].as_str().unwrap().to_owned();
    store
        .update(&id, |_| {
            Ok(Some(json!({
                "status":"running",
                "lifecycle":{"detached":true,"processIdentity":identity,"workerPid":identity.pid,"workerProcessStartedAtMs":identity.started_at_ms},
                "dispatch":{"state":"uncertain","PUBLIC":"preserve"},
            })))
        })
        .unwrap();
    id
}

#[tokio::test]
async fn cleanup_summaries_conform_to_the_consult_image_and_wait_output_schemas() {
    let home = tempfile::tempdir().unwrap();
    let server = server(home.path());
    let meta = server
        .store
        .create(
            json!({"prompt":"PUBLIC cleanup summary"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    let meta = server
        .store
        .update(id, |_| {
            Ok(Some(
                json!({"status":"completed","cleanupWarning":"PUBLIC retirement pending"}),
            ))
        })
        .unwrap();
    let wait = structured(
        server
            .invoke("wait", json!({"id":id}), &CancellationToken::new())
            .await
            .unwrap(),
    );
    assert_eq!(
        wait["browserCleanup"],
        json!({"state":"pending","recoveryRequired":true})
    );
    conforms(&wait, &output_schema("wait"), "wait");
    let mut out = json!({"sessionId":id,"status":"completed","output":""});
    add_summaries(&mut out, &meta);
    assert!(out.get("browserCleanup").is_some());
    conforms(&out, &output_schema("consult"), "consult");
}

#[tokio::test]
async fn consult_dry_runs_keep_the_fallback_consent() {
    let home = tempfile::tempdir().unwrap();
    let server = server(home.path());
    let cancel = CancellationToken::new();
    let consult = structured(
        server
            .invoke(
                "consult",
                json!({"prompt":"PUBLIC consult","engine":"browser","browserFallback":"grok","dryRun":true}),
                &cancel,
            )
            .await
            .unwrap(),
    );
    assert_eq!(consult["resolved"]["browser"]["fallbackProvider"], "grok");
    conforms(&consult, &output_schema("consult"), "consult");
}

#[tokio::test]
async fn wait_and_listings_report_a_verified_dead_worker_without_writing() {
    let home = tempfile::tempdir().unwrap();
    let server = server(home.path());
    let id = running_with(&server.store, home.path(), &dead_identity());
    let path = server.store.directory(&id).unwrap().join("meta.json");
    let before = fs::read(&path).unwrap();
    let (meta, timed_out) = tokio::time::timeout(
        Duration::from_secs(10),
        server.store.wait(&id, None, &CancellationToken::new()),
    )
    .await
    .expect("wait must return once the worker is verified dead")
    .unwrap();
    assert!(!timed_out);
    assert_eq!(meta["status"], "error");
    assert_eq!(
        meta["errorMessage"],
        "Detached worker exited before the session reached a terminal state."
    );
    assert_eq!(meta["recoveryRequired"], true);
    assert_eq!(meta["dispatch"]["PUBLIC"], "preserve");
    let cancel = CancellationToken::new();
    let wait = structured(
        server
            .invoke("wait", json!({"id":id}), &cancel)
            .await
            .unwrap(),
    );
    assert_eq!(
        (&wait["status"], &wait["waitStatus"]),
        (&json!("error"), &json!("terminal"))
    );
    conforms(&wait, &output_schema("wait"), "wait");
    let listed = structured(server.invoke("sessions", json!({}), &cancel).await.unwrap());
    assert_eq!(listed["entries"][0]["status"], "error");
    let single = structured(
        server
            .invoke("sessions", json!({"id":id}), &cancel)
            .await
            .unwrap(),
    );
    assert_eq!(single["entries"][0]["status"], "error");
    assert_eq!(fs::read(&path).unwrap(), before);

    // A live worker with the same evidence is still running.
    let live = running_with(
        &server.store,
        home.path(),
        &ProcessIdentity::current().unwrap(),
    );
    let (meta, timed_out) = server
        .store
        .wait(&live, Some(Duration::ZERO), &cancel)
        .await
        .unwrap();
    assert!(timed_out);
    assert_eq!(meta["status"], "running");
    let listed = structured(
        server
            .invoke("sessions", json!({"id":live}), &cancel)
            .await
            .unwrap(),
    );
    assert_eq!(listed["entries"][0]["status"], "running");
}

#[test]
fn cli_status_reports_a_verified_dead_worker_as_failed() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let id = running_with(&store, home.path(), &dead_identity());
    let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .current_dir(home.path())
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let line = stdout.lines().find(|line| line.ends_with(&id)).unwrap();
    assert!(line.starts_with("error "), "{stdout}");
    assert_eq!(
        fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
        before
    );
}

#[test]
fn consult_attachment_description_matches_upload_only_browser_delivery() {
    assert_eq!(
        input_schema("consult")["properties"]["browserAttachments"]["description"],
        "Browser-only: how to deliver `files`. \"auto\" (default) and \"always\" use real ChatGPT uploads. The \"never\" mode rejects resolved files because inline source cannot be verified."
    );
}

#[cfg(unix)]
#[test]
fn existing_session_trees_become_owner_only_without_following_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let home = tempfile::tempdir().unwrap();
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
    let set = |path: &Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap()
    };
    let outside = home.path().join("PUBLIC outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("inner.txt"), "PUBLIC").unwrap();
    fs::write(home.path().join("outside.txt"), "PUBLIC").unwrap();
    set(&outside, 0o755);
    set(&outside.join("inner.txt"), 0o644);
    set(&home.path().join("outside.txt"), 0o644);
    let root = home.path().join("sessions");
    let session = root.join("legacy-session");
    fs::create_dir_all(session.join("models")).unwrap();
    fs::write(session.join("meta.json"), "{}").unwrap();
    fs::write(session.join("models/PUBLIC.json"), "{}").unwrap();
    symlink(home.path().join("outside.txt"), session.join("file-link")).unwrap();
    symlink(&outside, session.join("dir-link")).unwrap();
    for (path, loose) in [
        (&root, 0o755),
        (&session, 0o755),
        (&session.join("models"), 0o775),
        (&session.join("meta.json"), 0o644),
        (&session.join("models/PUBLIC.json"), 0o664),
    ] {
        set(path, loose);
    }
    SessionStore::new(home.path()).ensure_storage().unwrap();
    for (path, expected) in [
        (root.clone(), 0o700),
        (session.clone(), 0o700),
        (session.join("models"), 0o700),
        (session.join("meta.json"), 0o600),
        (session.join("models/PUBLIC.json"), 0o600),
        (outside.clone(), 0o755),
        (outside.join("inner.txt"), 0o644),
        (home.path().join("outside.txt"), 0o644),
    ] {
        assert_eq!(mode(&path), expected, "{}", path.display());
    }
    assert!(
        fs::symlink_metadata(session.join("file-link"))
            .unwrap()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(session.join("dir-link"))
            .unwrap()
            .is_symlink()
    );
}

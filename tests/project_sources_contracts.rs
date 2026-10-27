use alcuin::{config::Environment, mcp::AlcuinMcp, sessions::SessionStore, storage};
use serde_json::json;
use std::fs;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn source_mutation_and_local_browser_admission_fail_before_browser_or_session_creation() {
    for (input, config, expected) in [
        (
            json!({"operation":"add","files":["PUBLIC-does-not-exist"]}),
            json!({}),
            "Temporary Chat policy forbids persistent Project Sources uploads",
        ),
        (
            json!({"operation":"add","files":[],"dryRun":true}),
            json!({}),
            "requires at least one file",
        ),
        (
            json!({"operation":"list"}),
            json!({"browser":{"remoteChrome":{"host":"127.0.0.1","port":1}}}),
            "local browser automation only",
        ),
        (
            json!({"operation":"list","dryRun":true}),
            json!({"browser":{"remoteHost":"PUBLIC.invalid:1"}}),
            "must run on the signed-in browser host",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        storage::atomic_json(&home.path().join("config.json"), &config).unwrap();
        let server = AlcuinMcp {
            store: SessionStore::new(home.path()),
            cwd: home.path().into(),
            env: Environment::new(),
        };
        let mut input = input;
        input["chatgptUrl"] = json!("https://chatgpt.com/g/g-p-public/project");
        let error = server
            .invoke("project_sources", input, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert!(!home.path().join("sessions").exists());
        assert!(!home.path().join("browser-profile").exists());
    }
}

#[tokio::test]
async fn source_previews_validate_binary_files_and_limits_without_browser_io() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("PUBLIC.bin"), [0, 255, 1, 2]).unwrap();
    storage::atomic_json(
        &home.path().join("config.json"),
        &json!({"browser":{"remoteHost":" "},"maxFileSizeBytes":10}),
    )
    .unwrap();
    let mut server = AlcuinMcp {
        store: SessionStore::new(home.path()),
        cwd: home.path().into(),
        env: Environment::from([
            ("ORACLE_REMOTE_HOST".into(), " ".into()),
            ("ORACLE_MAX_FILE_SIZE_BYTES".into(), "4".into()),
        ]),
    };
    let input = json!({"operation":"add","chatgptUrl":"https://chatgpt.com/g/g-p-public/project","files":["PUBLIC.bin"],"dryRun":true});
    let result = server
        .invoke("project_sources", input.clone(), &CancellationToken::new())
        .await
        .unwrap();
    let result = result.structured_content.unwrap();
    assert_eq!(result["status"], "dry-run");
    assert_eq!(result["plannedUploads"][0]["sizeBytes"], 4);
    assert_eq!(result["plannedUploads"][0]["name"], "PUBLIC.bin");
    server
        .env
        .insert("ORACLE_MAX_FILE_SIZE_BYTES".into(), "3".into());
    assert!(
        format!(
            "{:#}",
            server
                .invoke("project_sources", input, &CancellationToken::new())
                .await
                .unwrap_err()
        )
        .contains("exceed")
    );
    assert!(!home.path().join("sessions").exists());
}

use super::*;
use crate::cli_execution::{detach, inline_after_launch_error, wait_preference};
use crate::process::ProcessIdentity;
use crate::providers::{ApiTransport, Route};
use std::fs;
use std::time::Duration;

fn browser() -> Value {
    json!({"desiredModel":"Latest","thinkingTime":"pro","modelStrategy":"select","chatgptUrl":"https://chatgpt.com/","cookieSync":false,"keepBrowser":true})
}
fn metadata(mode: &str) -> Value {
    json!({"id":"PUBLIC-short","mode":mode,"options":{"prompt":"PUBLIC saved request for restarting","model":if mode=="browser"{"gpt-6-pro"}else{"gpt-6-astra"},"models":[if mode=="browser"{"gpt-6-pro"}else{"gpt-6-astra"}],"reasoningMode":"pro","file":[],"followupSessionId":"PUBLIC-original-followup","followupModel":"gpt-6-astra","previousResponseId":"resp_PUBLIC_original","heartbeatIntervalMs":2345,"modelOverrides":{"gpt-6-astra":{"inputLimit":12345}}},"browser":{"config":browser(),"runtime":{"conversationId":"PUBLIC-do-not-adopt","chromeTargetId":"PUBLIC-borrowed"}},"dispatch":{"reservationId":"PUBLIC-old"},"lifecycle":{"workerPid":123}})
}
#[test]
fn restart_strips_legacy_inline_cookies_after_saved_configuration_is_selected() {
    let home = tempfile::tempdir().unwrap();
    let mut meta = metadata("browser");
    meta["browser"]["config"]["inlineCookies"] =
        json!([{"name":"PUBLIC-cookie","value":"PUBLIC-secret-sentinel"}]);
    meta["browser"]["config"]["inlineCookiesSource"] = json!("PUBLIC-private-path");
    let (request, cwd) = saved_request(&meta, home.path(), &Environment::new()).unwrap();
    assert!(request.cookie_bootstrap.is_some());
    for saved in [&request.options, &request.browser, &request.browser_preview] {
        assert!(!saved.to_string().contains("PUBLIC-secret-sentinel"));
        assert!(!saved.to_string().contains("PUBLIC-private-path"));
    }
    let restarted = crate::browser_runner::prepare_restart(
        &SessionStore::new(home.path()),
        &request,
        &cwd,
        "PUBLIC-original",
    )
    .unwrap();
    assert!(!restarted.to_string().contains("PUBLIC-secret-sentinel"));
    assert_eq!(
        restarted["browser"]["config"]["cookieBootstrapRequired"]["kind"],
        "explicit-cookies"
    );
}
#[test]
fn saved_intent_preserves_source_files_and_followup_fields_without_current_defaults_or_old_authority()
 {
    for mode in ["api", "browser"] {
        let home = tempfile::tempdir().unwrap();
        let saved = home.path().join("saved");
        let current = home.path().join("current");
        fs::create_dir(&saved).unwrap();
        fs::create_dir(&current).unwrap();
        fs::write(saved.join("PUBLIC-file.rs"), "PUBLIC saved file").unwrap();
        let mut meta = metadata(mode);
        meta["cwd"] = json!(saved);
        meta["options"]["file"] = json!(["PUBLIC-file.rs"]);
        meta["options"]["maxFileSizeBytes"] = json!(1000);
        meta["options"]["preview"] = json!(true);
        meta["options"]["force"] = json!(true);
        meta["options"]["sessionId"] = json!("PUBLIC-old-id");
        meta["options"]["browserConfig"] = json!({"desiredModel":"PUBLIC wrong fallback"});
        meta["browser"]["config"]["remoteHost"] = json!("old.invalid:1");
        let env = Environment::from([
            ("ORACLE_MAX_FILE_SIZE_BYTES".into(), "1".into()),
            (
                "ORACLE_BROWSER_PROFILE_DIR".into(),
                "PUBLIC-current-profile".into(),
            ),
        ]);
        let (request, cwd) = saved_request(&meta, &current, &env).unwrap();
        assert_eq!(cwd, saved);
        assert_eq!(request.options["prompt"], meta["options"]["prompt"]);
        assert_eq!(request.files.len(), 1);
        assert!(request.user_prompt.contains("PUBLIC saved file"));
        assert_eq!(request.options["maxFileSizeBytes"], 1000);
        for field in [
            "previousResponseId",
            "followupSessionId",
            "followupModel",
            "heartbeatIntervalMs",
            "modelOverrides",
        ] {
            assert_eq!(request.options[field], meta["options"][field], "{field}");
        }
        for field in [
            "sessionId",
            "force",
            "preview",
            "dispatch",
            "lifecycle",
            "runtime",
        ] {
            assert!(request.options.get(field).is_none(), "{field}");
        }
        if mode == "browser" {
            assert_eq!(request.browser["desiredModel"], "Latest");
            assert_eq!(request.browser["keepBrowser"], true);
            assert!(request.browser.get("remoteHost").is_none());
            assert!(
                request
                    .options
                    .get("browserResumeConversationUrl")
                    .is_none()
            );
        }
    }
}
#[test]
fn restart_revalidates_model_and_reasoning_and_requires_saved_prompt_and_browser_config() {
    let home = tempfile::tempdir().unwrap();
    for (field, value) in [
        ("prompt", json!("")),
        ("model", json!("gpt-5")),
        ("reasoningMode", json!("standard")),
    ] {
        let mut meta = metadata("api");
        meta["options"][field] = value;
        assert!(
            saved_request(&meta, home.path(), &Environment::new()).is_err(),
            "{field}"
        );
    }
    let mut meta = metadata("browser");
    meta["browser"].as_object_mut().unwrap().remove("config");
    assert!(
        saved_request(&meta, home.path(), &Environment::new())
            .unwrap_err()
            .to_string()
            .contains("missing browser config")
    );
}
#[test]
fn saved_model_aliases_resolve_to_one_consistent_new_dispatch_and_model_run() {
    for mode in ["api", "browser"] {
        for alias in ["Latest", "gpt-6"] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path());
            let mut meta = metadata(mode);
            meta["options"]["model"] = json!(alias);
            meta["options"]["models"] = json!([alias]);
            let (request, cwd) = saved_request(&meta, home.path(), &Environment::new()).unwrap();
            let restarted = if mode == "browser" {
                crate::browser_runner::prepare_restart(&store, &request, &cwd, "PUBLIC-alias")
                    .unwrap()
            } else {
                execution::prepare_restart(&store, &request, &cwd, route(), "PUBLIC-alias").unwrap()
            };
            assert_eq!(restarted["model"], "gpt-6-astra");
            assert_eq!(restarted["options"]["model"], "gpt-6-astra");
            assert_eq!(restarted["models"].as_array().unwrap().len(), 1);
            assert_eq!(restarted["models"][0]["model"], "gpt-6-astra");
        }
    }
}
#[test]
fn explicit_wait_then_stored_boolean_then_engine_pro_default_and_no_detach_are_distinct() {
    for mode in ["api", "browser"] {
        for stored in [None, Some(false), Some(true)] {
            for explicit in [None, Some(false), Some(true)] {
                let mut meta = metadata(mode);
                if let Some(stored) = stored {
                    meta["options"]["waitPreference"] = json!(stored);
                }
                assert_eq!(
                    wait_preference(explicit, &meta),
                    explicit.or(stored).unwrap_or(mode == "browser")
                );
            }
        }
    }
    let mut meta = metadata("api");
    meta["options"]
        .as_object_mut()
        .unwrap()
        .remove("reasoningMode");
    assert!(wait_preference(None, &meta));
    let home = tempfile::tempdir().unwrap();
    let (request, _) =
        saved_request(&metadata("browser"), home.path(), &Environment::new()).unwrap();
    assert!(detach(&request, true, &Environment::new()));
    assert!(!detach(
        &request,
        true,
        &Environment::from([("ORACLE_NO_DETACH".into(), "1".into())])
    ));
}
fn route() -> Route {
    Route {
        model: "gpt-6-astra".into(),
        provider_label: "synthetic".into(),
        base_url: "https://public.invalid/v1".into(),
        key_source: "SYNTHETIC_KEY".into(),
        azure_deployment_name: None,
        transport: ApiTransport::Responses,
    }
}
#[test]
fn inline_launch_fallback_requires_wait_and_no_native_handoff_or_dispatch() {
    let untouched = json!({"status":"pending"});
    assert!(inline_after_launch_error(&untouched, true));
    assert!(!inline_after_launch_error(&untouched, false));
    for changed in [
        json!({"status":"running"}),
        json!({"status":"pending","lifecycle":null}),
        json!({"status":"pending","lifecycle":{"startAcknowledgement":"uncertain"}}),
        json!({"status":"pending","dispatch":null}),
        json!({"status":"pending","dispatch":{"state":"uncertain"}}),
    ] {
        assert!(!inline_after_launch_error(&changed, true));
    }
}
#[test]
fn restart_fresh_admission_suffixes_the_exact_source_and_preserves_an_active_original() {
    for mode in ["api", "browser"] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta = metadata(mode);
        let mut options = meta["options"].clone();
        if mode == "browser" {
            options["browserConfig"] = browser();
        }
        options["mode"] = json!(mode);
        let original = store.create(options, home.path(), Some("one")).unwrap();
        let source = store.stored_for_update("one").unwrap();
        let (request, cwd) = saved_request(&original, home.path(), &Environment::new()).unwrap();
        let restarted = if mode == "browser" {
            assert!(crate::browser_runner::prepare(&store, &request, &cwd).is_err());
            crate::browser_runner::prepare_restart(&store, &request, &cwd, "one").unwrap()
        } else {
            assert!(execution::prepare(&store, &request, &cwd, route()).is_err());
            execution::prepare_restart(&store, &request, &cwd, route(), "one").unwrap()
        };
        assert_eq!(restarted["id"], "one-2");
        assert_eq!(restarted["status"], "pending");
        for field in ["dispatch", "lifecycle", "response", "completedAt"] {
            assert!(restarted.get(field).is_none(), "{field}");
        }
        assert_eq!(store.stored_for_update("one").unwrap(), source);
        assert_eq!(
            restarted["options"]["followupSessionId"],
            "PUBLIC-original-followup"
        );
        if mode == "api" {
            let committed = execution::committed_request(&store, "one-2").unwrap();
            assert_eq!(committed.session_id, "one-2");
            assert_eq!(
                committed.body["previous_response_id"],
                "resp_PUBLIC_original"
            );
        }
    }
}
#[tokio::test]
async fn launched_attachment_propagates_failure_and_cancels_only_its_exact_new_worker() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let initial = store
        .create(
            json!({"prompt":"PUBLIC launch cancellation","model":"gpt-6-astra","mode":"api"}),
            home.path(),
            Some("PUBLIC-launch"),
        )
        .unwrap();
    let id = initial["id"].as_str().unwrap();
    let worker = ProcessIdentity::current().unwrap();
    execution::start_owner(&store, id, &worker, true).unwrap();
    let mut wrong = worker.clone();
    wrong.started_at_ms += 1.0;
    assert!(store.cancel_launched_worker(id, &wrong).is_err());
    assert!(
        !store
            .directory(id)
            .unwrap()
            .join(format!(".cancel-requested-{}", worker.pid))
            .exists()
    );
    let watcher_store = store.clone();
    let watched = id.to_owned();
    let pid = worker.pid;
    let updater = tokio::spawn(async move {
        let path = watcher_store
            .directory(&watched)
            .unwrap()
            .join(format!(".cancel-requested-{pid}"));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        watcher_store
            .update(&watched, |_| Ok(Some(json!({"status":"cancelled"}))))
            .unwrap();
    });
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut output = vec![];
    assert_eq!(
        crate::session_attach::attach_launched(
            &store,
            id,
            &worker,
            &Environment::new(),
            &cancel,
            &mut output
        )
        .await
        .unwrap(),
        130
    );
    updater.await.unwrap();
    assert!(
        !store
            .directory(id)
            .unwrap()
            .join(format!(".cancel-requested-{}", worker.pid))
            .exists()
    );
    assert!(!String::from_utf8(output).unwrap().contains("Session:"));
    store
        .update(id, |_| {
            Ok(Some(
                json!({"status":"error","errorMessage":"PUBLIC failed worker"}),
            ))
        })
        .unwrap();
    assert_eq!(
        crate::session_attach::attach_launched(
            &store,
            id,
            &worker,
            &Environment::new(),
            &CancellationToken::new(),
            &mut vec![]
        )
        .await
        .unwrap(),
        1
    );
    assert!(store.cancel_launched_worker(id, &worker).unwrap().is_none());
}
#[test]
#[ignore = "Subprocess fixture, invoked by cancellation_after_exact_worker_death_settles_and_retires_its_marker"]
fn launched_worker_liveness_helper() {
    if std::env::var("PUBLIC_RESTART_HELPER").as_deref() == Ok("1") {
        std::thread::sleep(Duration::from_secs(20));
    }
}
#[tokio::test]
async fn cancellation_after_exact_worker_death_settles_and_retires_its_marker() {
    use crate::process::Liveness;
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    store.create(json!({"prompt":"PUBLIC dead launched cancellation","model":"gpt-6-astra","mode":"api"}),home.path(),Some("PUBLIC-dead-launch")).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart::tests::launched_worker_liveness_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("PUBLIC_RESTART_HELPER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let worker = ProcessIdentity::read(child.id().unwrap()).unwrap().unwrap();
    execution::start_owner(&store, "PUBLIC-dead-launch", &worker, true).unwrap();
    let marker = store
        .directory("PUBLIC-dead-launch")
        .unwrap()
        .join(format!(".cancel-requested-{}", worker.pid));
    let observed = marker.clone();
    let killer = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !observed.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    });
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        crate::session_attach::attach_launched(
            &store,
            "PUBLIC-dead-launch",
            &worker,
            &Environment::new(),
            &cancel,
            &mut vec![],
        ),
    )
    .await
    .unwrap()
    .unwrap();
    killer.await.unwrap();
    assert_eq!(result, 130);
    assert_eq!(worker.liveness(), Liveness::Dead);
    assert_eq!(
        store.require("PUBLIC-dead-launch").unwrap()["status"],
        "error"
    );
    assert!(!marker.exists());
}
#[test]
fn restart_keeps_the_saved_notification_choice() {
    let home = tempfile::tempdir().unwrap();
    let mut meta = metadata("api");
    let (request, _) = saved_request(&meta, home.path(), &Environment::new()).unwrap();
    assert!(request.options.get("notifications").is_none());
    meta["notifications"] = json!({"enabled":false,"sound":true});
    let (request, _) = saved_request(&meta, home.path(), &Environment::new()).unwrap();
    assert_eq!(
        request.options["notifications"],
        json!({"enabled":false,"sound":true})
    );
}
#[test]
fn restart_routes_saved_azure_options_without_current_azure_environment() {
    let home = tempfile::tempdir().unwrap();
    let env = Environment::from([
        (
            "AZURE_OPENAI_ENDPOINT".into(),
            "https://current-azure.invalid".into(),
        ),
        (
            "AZURE_OPENAI_DEPLOYMENT".into(),
            "PUBLIC-current-deployment".into(),
        ),
        ("AZURE_OPENAI_API_KEY".into(), "PUBLIC-azure-key".into()),
        ("OPENAI_API_KEY".into(), "PUBLIC-openai-key".into()),
    ]);
    let route = |meta: &Value| {
        let (request, _) = saved_request(meta, home.path(), &env).unwrap();
        crate::providers::resolve_route(
            request.options["model"].as_str().unwrap(),
            &request.options,
            &env,
        )
        .unwrap()
        .0
    };
    let saved = route(&metadata("api"));
    assert_eq!(saved.provider_label, "OpenAI");
    assert_eq!(saved.base_url, "https://api.openai.com/v1");
    let mut meta = metadata("api");
    meta["options"]["azure"] =
        json!({"endpoint":"https://saved-azure.invalid","deployment":"PUBLIC-saved-deployment"});
    let saved = route(&meta);
    assert_eq!(saved.provider_label, "Azure OpenAI");
    assert_eq!(saved.base_url, "https://saved-azure.invalid/openai/v1");
    assert_eq!(
        saved.azure_deployment_name.as_deref(),
        Some("PUBLIC-saved-deployment")
    );
}

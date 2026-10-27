use alcuin::{
    browser_runner, config::Environment, execution, process::ProcessIdentity,
    sessions::SessionStore,
};
use serde_json::json;
use std::{fs, time::Duration};
#[test]
fn deep_research_refuses_before_session_or_browser_side_effects() {
    for (mode, followups, refused) in [
        ("deep", json!(["PUBLIC additional request"]), true),
        ("deep", json!(["  ", "\n"]), true),
        ("deep", json!([]), true),
        ("search", json!([]), false),
    ] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let request=execution::resolve(&json!({"engine":"browser","prompt":"PUBLIC original research request","browserFollowUps":followups}),
            &json!({"browser":{"researchMode":mode}}),&Environment::new(),home.path()).unwrap();
        let result = browser_runner::prepare(&store, &request, home.path());
        if refused {
            let error = result.unwrap_err().to_string();
            assert!(
                error.starts_with(
                    "Deep Research is unavailable: ChatGPT offers it only in ordinary chats"
                ) && error.contains("Nothing was sent."),
                "{error}"
            );
            assert!(
                !store.home().exists(),
                "Refusal must precede session and browser allocation"
            );
        } else {
            assert!(result.is_ok());
        }
    }
}
#[test]
fn image_generation_refuses_before_session_or_browser_side_effects() {
    for (key, refused) in [
        (Some("generateImage"), true),
        (Some("outputPath"), true),
        (None, false),
    ] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let mut input = json!({"engine":"browser","prompt":"PUBLIC image request"});
        if let Some(key) = key {
            input[key] = json!(home.path().join("PUBLIC-image.png"));
        }
        let request =
            execution::resolve(&input, &json!({}), &Environment::new(), home.path()).unwrap();
        let result = browser_runner::prepare(&store, &request, home.path());
        if refused {
            let error = result.unwrap_err().to_string();
            assert!(
                error.starts_with("Image generation is unavailable: ChatGPT does not generate images in Temporary Chat")
                    && error.contains("Nothing was sent."),
                "{key:?}: {error}"
            );
            assert!(
                !store.home().exists(),
                "Refusal must precede session and browser allocation"
            );
        } else {
            assert!(result.is_ok());
        }
    }
}
#[test]
fn prepared_browser_bundle_is_durable_after_source_removal() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    fs::write(home.path().join("one.rs"), b"PUBLIC one").unwrap();
    fs::write(home.path().join("two.rs"), b"PUBLIC two").unwrap();
    let request=execution::resolve(&json!({"engine":"browser","prompt":"PUBLIC bundle session source","files":["one.rs","two.rs"],"browserAttachments":"always"}),&json!({}),&Environment::new(),home.path()).unwrap();
    let meta = browser_runner::prepare(&store, &request, home.path()).unwrap();
    let id = meta["id"].as_str().unwrap();
    let saved: browser_runner::BrowserRequest = serde_json::from_slice(
        &fs::read(store.directory(id).unwrap().join("browser-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.upload_paths.len(), 1);
    assert!(
        saved.upload_paths[0].starts_with(fs::canonicalize(store.directory(id).unwrap()).unwrap())
    );
    assert_eq!(
        saved.extra["bundled"]["bundlePath"],
        saved.upload_paths[0].to_string_lossy().as_ref()
    );
    fs::remove_file(home.path().join("one.rs")).unwrap();
    fs::remove_file(home.path().join("two.rs")).unwrap();
    let staged = saved.staged_uploads.unwrap();
    staged.verify(&store, id).unwrap();
    assert!(staged.open(0).unwrap().generated_bundle);
    assert!(
        fs::read_to_string(&saved.upload_paths[0])
            .unwrap()
            .contains("### File: two.rs")
    );
    staged.cleanup(&store, id).unwrap();
}
#[test]
fn browser_text_and_raw_files_both_stage_as_uploads() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    fs::write(home.path().join("public.rs"), b"PUBLIC text").unwrap();
    fs::write(home.path().join("public.png"), b"PUBLIC synthetic image").unwrap();
    let request=execution::resolve(&json!({"engine":"browser","prompt":"PUBLIC mixed source request","files":["public.rs","public.png"]}),&json!({}),&Environment::new(),home.path()).unwrap();
    let meta = browser_runner::prepare(&store, &request, home.path()).unwrap();
    let id = meta["id"].as_str().unwrap();
    let saved: browser_runner::BrowserRequest = serde_json::from_slice(
        &fs::read(store.directory(id).unwrap().join("browser-request.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.prompt, "PUBLIC mixed source request");
    let names: Vec<_> = saved
        .upload_paths
        .iter()
        .map(|path| path.file_name().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["public.rs", "public.png"]);
    fs::remove_file(home.path().join("public.rs")).unwrap();
    fs::remove_file(home.path().join("public.png")).unwrap();
    saved
        .staged_uploads
        .as_ref()
        .unwrap()
        .verify(&store, id)
        .unwrap();
}
#[tokio::test]
async fn compiled_browser_worker_acknowledges_start_in_isolated_home_then_refuses_unqualified_preflight()
 {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    // Both home variables and the working directory are synthetic. launch_detached clears the
    // inherited environment and uses the committed session cwd before spawning the Rust binary.
    let mut environment = Environment::new();
    environment.insert("HOME".into(), home.path().to_string_lossy().into_owned());
    environment.insert(
        "USERPROFILE".into(),
        home.path().to_string_lossy().into_owned(),
    );
    let request = execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC synthetic browser worker without a provider"}),
        &json!({}),
        &environment,
        home.path(),
    )
    .unwrap();
    let metadata = browser_runner::prepare(&store, &request, home.path()).unwrap();
    let id = metadata["id"].as_str().unwrap();
    let pid = execution::launch_detached(
        &store,
        id,
        std::path::Path::new(env!("CARGO_BIN_EXE_alcuin")),
        &environment,
    )
    .await
    .unwrap();
    // The spawned worker refuses in about 2 s unloaded; 30 s covers a busy CI runner.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let metadata = loop {
        let metadata = store.require(id).unwrap();
        if metadata["status"] == "error" {
            break metadata;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let ack: serde_json::Value = serde_json::from_slice(
        &fs::read(store.directory(id).unwrap().join("worker-started.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ack["pid"], pid);
    assert_eq!(ack["sessionId"], id);
    assert!(ack["processIdentity"]["nativeStart"].is_string());
    assert_eq!(metadata["lifecycle"]["engine"], "browser");
    assert_eq!(metadata["lifecycle"]["detached"], true);
    assert!(metadata.get("dispatch").is_none());
    assert_eq!(metadata["recoveryRequired"], false);
    assert_eq!(metadata["models"][0]["status"], "error");
    assert!(metadata["models"][0]["completedAt"].is_string());
    assert_eq!(
        metadata["models"][0]["errorMessage"],
        metadata["errorMessage"]
    );
    while ProcessIdentity::read(pid).unwrap().is_some() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(home.path().join(".oracle").exists());
}

#[tokio::test]
async fn native_preflight_cancellation_updates_model_and_session_without_dispatch() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    let mut environment = Environment::new();
    for key in ["HOME", "USERPROFILE"] {
        environment.insert(key.into(), home.path().to_string_lossy().into_owned());
    }
    let request = execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC cancelled native browser request"}),
        &json!({"browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("profile")}}),
        &environment,
        home.path(),
    ).unwrap();
    let metadata = browser_runner::prepare(&store, &request, home.path()).unwrap();
    let id = metadata["id"].as_str().unwrap();
    let identity = ProcessIdentity::current().unwrap();
    execution::start_owner(&store, id, &identity, false).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    assert!(
        browser_runner::run_native(&store, id, &environment, &identity, &cancel)
            .await
            .is_err()
    );
    let metadata = store.require(id).unwrap();
    assert_eq!(metadata["status"], "cancelled");
    assert_eq!(metadata["models"][0]["status"], "cancelled");
    assert!(metadata.get("dispatch").is_none());
    assert_eq!(metadata["recoveryRequired"], false);
    // Reference runBrowserSessionExecution logs the launch header before any browser work.
    let log = store.log(id).unwrap();
    assert!(
        log.contains("Launching browser mode (target=GPT-6; requested=gpt-6-pro) with ~")
            && log.ends_with(" tokens.\nThis run can take up to an hour (usually ~10 minutes).\n[browser] Browser control: launch visible Chrome; may focus/control the browser UI.\n[browser] Browser guidance: Manual-login mode may show the persistent Alcuin Chrome profile for sign-in or automation.\n[browser] Browser guidance: Use --browser-attach-running or --remote-chrome to reduce desktop disruption."),
        "{log:?}"
    );
}
#[test]
fn borrowed_tabs_verify_an_existing_extra_high_without_the_fresh_pro_first_rule() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    let mut environment = Environment::new();
    for key in ["HOME", "USERPROFILE"] {
        environment.insert(key.into(), home.path().to_string_lossy().into_owned());
    }
    let input = json!({"engine":"browser","prompt":"PUBLIC borrowed extra high","browserThinkingTime":"extra-high"});
    let fresh = execution::resolve(
        &input,
        &json!({"browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("profile")}}),
        &environment,
        home.path(),
    )
    .unwrap();
    assert_eq!(
        browser_runner::prepare(&store, &fresh, home.path())
            .unwrap_err()
            .to_string(),
        "a fresh Latest request must try Pro before Extra High"
    );
    let borrowed = execution::resolve(
        &input,
        &json!({"browser":{"manualLogin":true,"manualLoginProfileDir":home.path().join("profile"),"browserTabRef":"current"}}),
        &environment,
        home.path(),
    )
    .unwrap();
    let metadata = browser_runner::prepare(&store, &borrowed, home.path()).unwrap();
    assert_eq!(metadata["browser"]["config"]["thinkingTime"], "extra-high");
}
#[tokio::test]
async fn ordinary_cookie_sync_logs_the_reference_warning_before_reading_chrome() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join(".oracle"));
    let mut environment = Environment::new();
    for key in ["HOME", "USERPROFILE"] {
        environment.insert(key.into(), home.path().to_string_lossy().into_owned());
    }
    let request = execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC ordinary cookie warning request","browserManualLogin":false,"browserCookieSync":true,"browserCookiePath":home.path().join("PUBLIC-missing/Cookies")}),
        &json!({}),
        &environment,
        home.path(),
    )
    .unwrap();
    let metadata = browser_runner::prepare(&store, &request, home.path()).unwrap();
    let id = metadata["id"].as_str().unwrap();
    let identity = ProcessIdentity::current().unwrap();
    execution::start_owner(&store, id, &identity, false).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    assert!(
        browser_runner::run_native(&store, id, &environment, &identity, &cancel)
            .await
            .is_err()
    );
    let log = store.log(id).unwrap();
    assert!(
        log.contains("Warning: copying cookies from a live Chrome profile can invalidate that browser's ChatGPT session when tokens rotate. Prefer --browser-manual-login or inline cookies when possible."),
        "{log}"
    );
    assert!(store.require(id).unwrap().get("dispatch").is_none());
}
#[test]
fn ordinary_cookie_sync_options_select_a_source_without_reaching_saved_values() {
    let home = tempfile::tempdir().unwrap();
    let env = Environment::from([(
        "ORACLE_BROWSER_COOKIE_NAMES".to_owned(),
        " PUBLIC-env-name ,".to_owned(),
    )]);
    let request = execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC ordinary sync request","browserManualLogin":false,"browserCookieSync":true,"browserCookiePath":"PUBLIC-source/Default","browserCookieNames":"PUBLIC-a, PUBLIC-b,","browserCookieWait":"2s","browserChromeProfile":"Profile 1","browserAllowCookieErrors":true}),
        &json!({}),
        &env,
        home.path(),
    )
    .unwrap();
    let browser = &request.options["browserConfig"];
    assert_eq!(browser["manualLogin"], false);
    assert_eq!(browser["cookieSync"], true);
    assert_eq!(browser["chromeCookiePath"], "PUBLIC-source/Default");
    assert_eq!(browser["cookieNames"], json!(["PUBLIC-a", "PUBLIC-b"]));
    assert_eq!(browser["cookieSyncWaitMs"], 2000);
    assert_eq!(browser["chromeProfile"], "Profile 1");
    assert!(browser.get("inlineCookies").is_none() && browser["cookieBootstrapRequired"].is_null());
    let request = execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC ordinary sync request","browserManualLogin":false,"browserCookieSync":true}),
        &json!({}),
        &env,
        home.path(),
    )
    .unwrap();
    assert_eq!(
        request.options["browserConfig"]["cookieNames"],
        json!(["PUBLIC-env-name"])
    );
    // Disabling sync wins over configured sync; custom profiles still need a source route.
    let config = json!({"browser":{"cookieSync":true,"manualLogin":false}});
    let request = execution::resolve(
        &json!({"engine":"browser","prompt":"PUBLIC ordinary sync request","browserNoCookieSync":true}),
        &config,
        &Environment::new(),
        home.path(),
    )
    .unwrap();
    assert_eq!(request.options["browserConfig"]["cookieSync"], false);
    assert!(
        execution::resolve(
            &json!({"engine":"browser","prompt":"PUBLIC ordinary sync request","browserManualLogin":false,"browserChromeProfile":"Profile 1"}),
            &json!({}),
            &Environment::new(),
            home.path(),
        )
        .is_err()
    );
    assert!(fs::read_dir(home.path()).unwrap().next().is_none());
}

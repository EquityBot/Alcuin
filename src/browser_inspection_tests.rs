use super::*;
use crate::browser::prompt_fingerprint;

fn metadata() -> Value {
    json!({"browser":{"runtime":{"submittedPromptHash":prompt_fingerprint("PUBLIC source","PUBLIC-user"),"submittedPromptHashFormat":"original-source-v1"}}})
}
fn tab() -> Value {
    json!({"targetId":"PUBLIC-target","conversationId":"public-conversation","authenticated":true,"lastUserMessageId":"PUBLIC-user","lastUserOriginalSourceMessageId":"PUBLIC-user","lastUserOriginalSourceText":"PUBLIC source","lastAssistantMessageId":"PUBLIC-answer","lastAssistantText":"PUBLIC answer","assistantFollowsLatestUser":true,"stopExists":false,"completionVisible":true,"completionProbe":{"source":"mutation-observer","stopSeen":true,"stopGone":true},"snapshotCaptures":{"conversationId":"public-conversation","lastUserMessageId":"PUBLIC-user","lastAssistantMessageId":"PUBLIC-answer"}})
}
#[test]
fn live_requires_the_original_source_even_when_an_assistant_is_paired() {
    let now = Instant::now();
    let mut progress =
        LiveProgress::new(now, Duration::from_secs(2), false, Duration::from_secs(60));
    let mut wrong = tab();
    wrong["lastUserOriginalSourceText"] = json!("PUBLIC changed source");
    assert!(
        progress
            .observe(&metadata(), &mut wrong, false, now, Duration::from_secs(60))
            .is_err()
    );
    assert!(progress.hash.is_none());
    assert!(
        progress
            .observe(&metadata(), &mut wrong, true, now, Duration::from_secs(60))
            .unwrap()
            .unwrap()
            .changed
    );
}
#[test]
fn live_freshness_requires_pairing_and_allows_an_active_reply_to_continue() {
    let now = Instant::now();
    let later = now + Duration::from_secs(3);
    let mut progress =
        LiveProgress::new(now, Duration::from_secs(2), false, Duration::from_secs(60));
    let mut unpaired = tab();
    unpaired["assistantFollowsLatestUser"] = json!(false);
    assert!(
        progress
            .observe(
                &metadata(),
                &mut unpaired,
                false,
                now,
                Duration::from_secs(60)
            )
            .unwrap()
            .is_none()
    );
    unpaired["stopExists"] = json!(true);
    assert!(
        progress
            .observe(
                &metadata(),
                &mut unpaired,
                false,
                later,
                Duration::from_secs(60)
            )
            .unwrap()
            .is_none()
    );
    unpaired["stopExists"] = json!(false);
    assert!(
        progress
            .observe(
                &metadata(),
                &mut unpaired,
                false,
                later,
                Duration::from_secs(60)
            )
            .is_err()
    );
    assert!(progress.hash.is_none());
}
#[test]
fn recovered_content_must_hydrate_before_stall_and_cannot_publish_an_empty_turn() {
    let now = Instant::now();
    let stall = Duration::from_secs(60);
    let mut progress = LiveProgress::new(now, Duration::from_secs(2), true, stall);
    let mut empty = tab();
    empty["lastAssistantText"] = json!("");
    assert!(
        progress
            .observe(&metadata(), &mut empty, true, now, stall)
            .unwrap()
            .is_none()
    );
    assert!(
        progress
            .observe(&metadata(), &mut empty, true, now + stall, stall)
            .is_err()
    );
    assert!(progress.hash.is_none());
    assert!(
        progress
            .observe(
                &metadata(),
                &mut tab(),
                false,
                now + Duration::from_secs(1),
                stall
            )
            .unwrap()
            .unwrap()
            .changed
    );
    assert!(progress.recovered_deadline.is_none());
}
#[test]
fn live_tracks_changes_and_distinguishes_running_stalled_and_detached() {
    let now = Instant::now();
    let stall = Duration::from_secs(60);
    let mut progress = LiveProgress::new(now, Duration::from_secs(2), false, stall);
    let mut active = tab();
    active["stopExists"] = json!(true);
    let first = progress
        .observe(&metadata(), &mut active, false, now, stall)
        .unwrap()
        .unwrap();
    assert!(first.changed && !first.terminal);
    assert_eq!(active["state"], "running");
    let unchanged = progress
        .observe(&metadata(), &mut active, false, now + stall, stall)
        .unwrap()
        .unwrap();
    assert!(!unchanged.changed && unchanged.terminal);
    assert_eq!(active["state"], "stalled");
    active["lastAssistantText"] = json!("PUBLIC more answer");
    assert!(
        progress
            .observe(&metadata(), &mut active, false, now + stall, stall)
            .unwrap()
            .unwrap()
            .changed
    );
    assert_eq!(active["state"], "running");
    active["authenticated"] = json!(false);
    assert!(
        progress
            .observe(&metadata(), &mut active, false, now + stall, stall)
            .unwrap()
            .unwrap()
            .terminal
    );
    assert_eq!(active["state"], "detached");
}
#[test]
fn live_completion_requires_stable_positive_evidence_for_the_same_turn() {
    let now = Instant::now();
    let stall = Duration::from_secs(60);
    let mut progress = LiveProgress::new(now, Duration::from_secs(2), false, stall);
    let mut complete = tab();
    complete["strongThinkingActive"] = json!(false);
    for ms in [0, 400, 800] {
        assert!(
            !progress
                .observe(
                    &metadata(),
                    &mut complete,
                    false,
                    now + Duration::from_millis(ms),
                    stall
                )
                .unwrap()
                .unwrap()
                .terminal
        );
    }
    assert!(
        progress
            .observe(
                &metadata(),
                &mut complete,
                false,
                now + Duration::from_millis(1200),
                stall
            )
            .unwrap()
            .unwrap()
            .terminal
    );
    assert_eq!(complete["state"], "completed");
    complete["lastAssistantMessageId"] = json!("PUBLIC-other-answer");
    assert!(
        !progress
            .observe(
                &metadata(),
                &mut complete,
                false,
                now + Duration::from_secs(2),
                stall
            )
            .unwrap()
            .unwrap()
            .terminal
    );
    assert_eq!(complete["completionConfirmed"], false);
}
#[test]
fn only_missing_tabs_and_connection_failures_allow_reopening() {
    assert!(recoverable(&anyhow::anyhow!(
        "CDP endpoint is unavailable."
    )));
    assert!(recoverable(&anyhow::anyhow!(
        "No ChatGPT tab matched the saved conversation identity"
    )));
    assert!(recoverable(
        &std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into()
    ));
    for message in [
        "Historical copy preparation was refused",
        "Capture source mismatch",
        "Browser inspection cancelled",
        "Browser inspection or recovery exceeded its timeout.",
        "Ambiguous ChatGPT tab reference",
    ] {
        assert!(!recoverable(&anyhow::anyhow!(message)));
    }
}
#[test]
fn an_expired_inspection_deadline_uses_the_reference_timeout_text() {
    assert_eq!(
        remaining(Instant::now()).unwrap_err().to_string(),
        "Browser inspection or recovery exceeded its timeout."
    );
}
#[test]
fn recovered_live_waits_for_missing_source_but_refuses_a_hydrated_mismatch() {
    let now = Instant::now();
    let stall = Duration::from_secs(60);
    let mut progress = LiveProgress::new(now, Duration::from_secs(2), true, stall);
    let mut loading = tab();
    loading["lastUserOriginalSourceMessageId"] = Value::Null;
    loading["lastUserOriginalSourceText"] = Value::Null;
    loading["assistantFollowsLatestUser"] = json!(false);
    assert!(
        progress
            .observe(
                &metadata(),
                &mut loading,
                false,
                now + Duration::from_secs(3),
                stall
            )
            .unwrap()
            .is_none()
    );
    assert!(progress.hash.is_none());
    let mut wrong = tab();
    wrong["lastUserOriginalSourceText"] = json!("PUBLIC wrong hydrated source");
    assert!(
        progress
            .observe(
                &metadata(),
                &mut wrong,
                false,
                now + Duration::from_secs(4),
                stall
            )
            .is_err()
    );
    assert!(progress.hash.is_none());
    assert!(
        progress
            .observe(
                &metadata(),
                &mut tab(),
                false,
                now + Duration::from_secs(5),
                stall
            )
            .unwrap()
            .unwrap()
            .changed
    );
    let mut expired = LiveProgress::new(now, Duration::from_secs(2), true, stall);
    assert!(
        expired
            .observe(&metadata(), &mut loading, false, now + stall, stall)
            .is_err()
    );
    assert!(expired.hash.is_none());
}
#[tokio::test]
async fn null_submission_refuses_before_endpoint_lookup_or_reopening() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC inspection","model":"gpt-6-pro"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    store.update(id,|_|Ok(Some(json!({"browser":{"config":{"manualLogin":true,"manualProfile":"PUBLIC-invalid-unused"},"runtime":{"submittedPromptHash":null,"chromeHost":"PUBLIC-invalid-unused","chromePort":65535}}})))).unwrap();
    let before = store.stored_for_update(id).unwrap();
    let error = harvest(
        &store,
        id,
        &InspectionOptions::default(),
        &Environment::new(),
        &CancellationToken::new(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("no confirmed submitted user turn")
    );
    assert_eq!(store.stored_for_update(id).unwrap(), before);
    assert!(!store.home().join("observation-admission").exists());
}

fn provider_metadata(port: u16) -> Value {
    let socket = format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC");
    json!({"options":{"providerAttempt":{"version":1,"role":"browser-provider-attempt"}},
        "browser":{"config":{"remoteChrome":{"host":"PUBLIC-ignored","port":1}},
        "harvest":{"url":"https://chatgpt.com/c/PUBLIC-misleading"},
        "runtime":{"chromeHost":"127.0.0.1","chromePort":port,
            "browserWSEndpoint":socket,"chromeBrowserWSEndpoint":socket,
            "chromeTargetId":"PUBLIC-original","tabUrl":"https://chatgpt.com/c/PUBLIC-misleading",
            "ownedRecoveryTarget":{"host":"127.0.0.1","port":port,"browserWSEndpoint":socket,"targetId":"PUBLIC-original"}}}})
}

#[test]
fn provider_inspection_uses_only_its_pinned_socket_and_exact_target() {
    let meta = provider_metadata(9222);
    let (endpoint, target) = crate::browser_attempt::pinned_target(&meta, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        endpoint.browser_ws_endpoint,
        "ws://127.0.0.1:9222/devtools/browser/PUBLIC"
    );
    assert_eq!(target, "PUBLIC-original");
    assert!(crate::browser_attempt::pinned_target(&meta, Some(&target)).is_ok());
    for reference in [
        "PUBLIC-other",
        "current",
        "PUBLIC-misleading",
        "https://chatgpt.com/c/PUBLIC-misleading",
    ] {
        assert!(crate::browser_attempt::pinned_target(&meta, Some(reference)).is_err());
    }
    for path in [
        "/browser/runtime/browserWSEndpoint",
        "/browser/runtime/chromeBrowserWSEndpoint",
        "/browser/runtime/chromeTargetId",
        "/browser/runtime/ownedRecoveryTarget/targetId",
        "/browser/runtime/ownedRecoveryTarget/browserWSEndpoint",
        "/browser/runtime/ownedRecoveryTarget/port",
    ] {
        let mut changed = meta.clone();
        *changed.pointer_mut(path).unwrap() = Value::Null;
        assert!(
            crate::browser_attempt::pinned_target(&changed, None).is_err(),
            "{path}"
        );
    }
}

#[test]
fn public_inspection_resolves_its_original_child_and_refuses_changed_ready_seals() {
    let (_root, manifest) = crate::browser_runner::tests::dead_provider_attempt();
    let store =
        SessionStore::new(std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"));
    let parent = manifest["parentId"].as_str().unwrap();
    let child = manifest["childId"].as_str().unwrap();
    let public = store.require(parent).unwrap();
    assert!(public["browser"]["runtime"]["chromeTargetId"].is_null());
    assert_eq!(source_metadata(&store, parent).unwrap()["id"], child);
    assert_eq!(source_metadata(&store, child).unwrap()["id"], child);
    store
        .update(child, |saved| {
            let mut browser = saved["browser"].clone();
            browser["attemptReady"]["childRequestSha256"] = json!("PUBLIC-changed");
            Ok(Some(json!({"browser":browser})))
        })
        .unwrap();
    assert!(source_metadata(&store, parent).is_err());
    assert!(source_metadata(&store, child).is_err());
    assert!(!store.home().join("observation-admission").exists());
}

/// Budget for each provider inspection step against the in-process fake browser. The test
/// asserts target identity, not speed, so it must outlast parallel-suite I/O stalls.
const PROVIDER_FIXTURE_BUDGET: Duration = Duration::from_secs(30);

#[tokio::test]
async fn provider_inspection_never_substitutes_another_tab_with_the_same_conversation() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    for present in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let meta = provider_metadata(listener.local_addr().unwrap().port());
        let peer = tokio::spawn(async move {
            let mut trace = vec![];
            for observation in 0..if present { 3 } else { 1 } {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(message))) = socket.next().await {
                    let request: Value = serde_json::from_str(&message).unwrap();
                    trace.push(request["method"].clone());
                    assert_eq!(request["method"], "Target.getTargets");
                    let mut rows = vec![
                        json!({"type":"page","targetId":"PUBLIC-other","url":"https://chatgpt.com/c/PUBLIC-same"}),
                    ];
                    if present && observation == 0 {
                        rows.push(json!({"type":"page","targetId":"PUBLIC-original","url":"https://chatgpt.com/c/PUBLIC-same"}));
                    } else if observation > 0 {
                        // Strict legacy lookup accepts this conversation ID, but provider
                        // inspection must still reject its different target ID.
                        rows[0]["url"] = json!("https://chatgpt.com/c/PUBLIC-original");
                    }
                    socket
                        .send(Message::Text(
                            json!({"id":request["id"],"result":{"targetInfos":rows}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            trace
        });
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path());
        let result = InspectionTarget::open(
            &store,
            "PUBLIC-unused",
            &meta,
            &InspectionOptions::default(),
            &Environment::new(),
            Instant::now() + PROVIDER_FIXTURE_BUDGET,
            &CancellationToken::new(),
        )
        .await;
        if present {
            let target = result.unwrap();
            assert_eq!(target.reference, "PUBLIC-original");
            assert!(target.pinned);
            assert!(target.recovered.is_none());
            let error = inspect(
                &target,
                true,
                PROVIDER_FIXTURE_BUDGET,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Original provider inspection target changed")
            );
            let saved = store
                .create(
                    json!({"model":"gpt-6-pro","prompt":"PUBLIC inspection","mode":"browser"}),
                    root.path(),
                    None,
                )
                .unwrap();
            let id = saved["id"].as_str().unwrap();
            store.update(id, |_| Ok(Some(meta.clone()))).unwrap();
            let error = browser_sessions::harvest_target_within(
                &store,
                id,
                &target.endpoint,
                &target.reference,
                true,
                &CancellationToken::new(),
                PROVIDER_FIXTURE_BUDGET,
            )
            .await
            .err()
            .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("Original provider harvest target changed")
            );
        } else {
            assert!(result.is_err());
        }
        assert_eq!(
            tokio::time::timeout(PROVIDER_FIXTURE_BUDGET, peer)
                .await
                .unwrap()
                .unwrap(),
            vec![json!("Target.getTargets"); if present { 3 } else { 1 }]
        );
        assert!(!store.home().join("observation-admission").exists());
    }
}
#[tokio::test]
async fn legacy_harvest_warns_before_inspecting_and_reports_progress_through_the_caller() {
    static WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let meta = store
        .create(
            json!({"prompt":"PUBLIC legacy inspection","model":"gpt-6-pro"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = meta["id"].as_str().unwrap();
    store.update(id,|_|Ok(Some(json!({"mode":"browser","browser":{"config":{"inputTimeoutMs":1000},"runtime":{"chromeHost":"127.0.0.1","chromePort":9}}})))).unwrap();
    let options = InspectionOptions {
        recover: false,
        warning: Some(|line| WARNINGS.lock().unwrap().push(line.to_owned())),
        ..Default::default()
    };
    let _ = harvest(
        &store,
        id,
        &options,
        &Environment::new(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        *WARNINGS.lock().unwrap(),
        [
            "Legacy browser session: submitted-turn identity is unavailable; verifying latest user/assistant pairing and positive completion."
        ]
    );
}

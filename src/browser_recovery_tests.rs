use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[tokio::test]
async fn reopening_preserves_original_session_and_cleans_early_failures_with_one_deadline() {
    // These protocol peers have no real Chrome process. Give their startup admission
    // its own namespace so unrelated tests cannot consume the staged CDP deadline.
    let startup = tempfile::tempdir().unwrap();
    crate::browser_chrome::TEST_STARTUP_LOCK
        .scope(
            startup.path().join("startup.lock"),
            verify_reopening_deadlines(),
        )
        .await;
}
async fn verify_reopening_deadlines() {
    const URL: &str = "https://chatgpt.com/c/public-reopened";
    for case in [
        "open",
        "close",
        "navigation-pending",
        "navigation-uncommitted",
        "page-error",
        "runtime-error",
        "navigation-error",
        "shared-deadline",
    ] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let metadata = store
            .create(
                json!({"prompt":"PUBLIC reopen","model":"gpt-6-pro"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = metadata["id"].as_str().unwrap();
        store.update(id,|_|Ok(Some(json!({"mode":"browser","browser":{"config":{"manualLogin":false,"inputTimeoutMs":if matches!(case,"shared-deadline"|"navigation-uncommitted"){1000}else{2000}},"runtime":{"tabUrl":URL,"chromeTargetId":"PUBLIC-ORIGINAL","submittedPromptHash":null,"ownedRecoveryTarget":{"PUBLIC":"preserve"}}}})))).unwrap();
        let before = store.stored_for_update(id).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = AttachmentEndpoint {
            host: "127.0.0.1".into(),
            port,
            browser_ws_endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-reopen"),
        };
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut delayed = tokio::task::JoinSet::new();
            let mut created = 0;
            let mut navigated = false;
            let mut closed = false;
            let mut pending_checks = if case == "navigation-pending" { 2 } else { 0 };
            loop {
                let frame = tokio::select! {
                    frame = socket.next() => frame,
                    response = delayed.join_next(), if !delayed.is_empty() => {
                        if socket.send(Message::Text(response.unwrap().unwrap())).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let Some(Ok(Message::Text(frame))) = frame else {
                    break;
                };
                let request: Value = serde_json::from_str(&frame).unwrap();
                let method = request["method"].as_str().unwrap();
                let failure = matches!(
                    (case, method),
                    ("page-error", "Page.enable")
                        | ("runtime-error", "Runtime.enable")
                        | ("navigation-error", "Page.navigate")
                );
                let result = match method {
                    "Target.createTarget" => {
                        created += 1;
                        assert_eq!(created, 1);
                        assert_eq!(request["params"]["url"], "about:blank");
                        json!({"targetId":"REOPEN-PUBLIC"})
                    }
                    "Target.attachToTarget" => json!({"sessionId":"REOPEN-SESSION"}),
                    "Page.enable" | "Runtime.enable" => json!({}),
                    "Page.navigate" => {
                        assert_eq!(request["params"]["url"], URL);
                        navigated = !failure;
                        json!({})
                    }
                    "Target.getTargetInfo" => {
                        let pending = pending_checks > 0;
                        let empty = pending_checks == 2;
                        if pending {
                            pending_checks -= 1;
                        }
                        json!({"targetInfo":{"targetId":"REOPEN-PUBLIC","type":"page","url":if case=="navigation-uncommitted"{"about:blank"}else if empty{""}else if navigated&&!pending{URL}else{"about:blank"}}})
                    }
                    "Runtime.evaluate" => {
                        assert!(
                            request["params"]["expression"]
                                .as_str()
                                .unwrap()
                                .contains("alcuin-dom:observation-handover")
                        );
                        json!({"result":{"value":{"url":if navigated&&case!="navigation-uncommitted"{URL}else{"about:blank"},"unclaimed":true}}})
                    }
                    "Target.getTargets" => {
                        json!({"targetInfos":if closed{vec![json!({"targetId":"FOREIGN-PUBLIC","type":"page"})]}else{vec![json!({"targetId":"REOPEN-PUBLIC","type":"page"}),json!({"targetId":"FOREIGN-PUBLIC","type":"page"})]}})
                    }
                    "Target.closeTarget" => {
                        assert_eq!(request["params"]["targetId"], "REOPEN-PUBLIC");
                        assert!(!closed);
                        closed = true;
                        json!({"success":true})
                    }
                    other => panic!("Unexpected recovery command {other}"),
                };
                let response = if failure {
                    json!({"id":request["id"],"error":{"code":-32000,"message":"PUBLIC early browser failure"}})
                } else {
                    json!({"id":request["id"],"result":result})
                };
                if case == "shared-deadline"
                    && matches!(method, "Page.enable" | "Runtime.enable" | "Page.navigate")
                {
                    // A delayed page acknowledgement must not block independent browser
                    // Target commands used to clean up the timed-out observation.
                    delayed.spawn(async move {
                        tokio::time::sleep(Duration::from_millis(650)).await;
                        response.to_string().into()
                    });
                    continue;
                }
                if socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            delayed.abort_all();
            while delayed.join_next().await.is_some() {}
            assert_eq!(created, 1);
            assert_eq!(
                navigated,
                matches!(
                    case,
                    "open" | "close" | "navigation-pending" | "navigation-uncommitted"
                ),
                "{case}"
            );
            closed
        });
        let started = Instant::now();
        let mut cleanup_deadline = false;
        let reopened = reopen(
            &store,
            id,
            Some(&endpoint),
            &Environment::new(),
            &CancellationToken::new(),
        )
        .await;
        if matches!(case, "open" | "close" | "navigation-pending") {
            let resource = reopened.unwrap_or_else(|error| panic!("{case}: {error:#}"));
            assert_eq!(resource.reference, "REOPEN-PUBLIC");
            resource
                .finish(case == "close")
                .await
                .unwrap_or_else(|error| panic!("{case}: {error:#}"));
        } else {
            let error = reopened
                .err()
                .expect("Early failure/deadline unexpectedly reopened the page");
            assert!(
                error.to_string().contains("cleanup") || error.to_string().contains("timed out"),
                "{case}: {error:#}"
            );
            let detail = format!("{error:#}");
            if !detail.contains("Fresh observation cleanup confirmed") {
                // Native authority reads can exceed the bounded cleanup budget under
                // concurrent filesystem load. Only that exact, fail-closed outcome is valid.
                let method = match case {
                    "page-error" => "Page.enable",
                    "runtime-error" => "Runtime.enable",
                    "navigation-error" => "Page.navigate",
                    _ => panic!("{case}: {detail}"),
                };
                assert!(
                    detail.contains(&format!("CDP command {method} was refused by the endpoint")),
                    "{case}: {detail}"
                );
                assert!(detail.contains("Observation cleanup unconfirmed; exact admission retained: Browser profile transaction deadline reached"), "{case}: {detail}");
                cleanup_deadline = true;
            }
        }
        if case == "shared-deadline" {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "Reopening restarted its timeout across stages"
            );
        }
        let closed = tokio::time::timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
        if !cleanup_deadline {
            assert_eq!(
                closed,
                !matches!(case, "open" | "navigation-pending"),
                "{case}"
            );
        }
        assert_eq!(store.stored_for_update(id).unwrap(), before);
        let admission = store
            .home()
            .join("observation-admission")
            .join(sha256(endpoint.browser_ws_endpoint.as_bytes()));
        let registry = ProfileRegistry::open(&admission).unwrap();
        let records = registry.records().unwrap();
        let leases = records["leases"].as_array().unwrap();
        if cleanup_deadline {
            assert_eq!(leases.len(), 1, "{case}: exact admission missing");
            let lease = &leases[0];
            assert_eq!(lease["sessionId"], id);
            assert_eq!(
                lease["processIdentity"],
                json!(crate::process::ProcessIdentity::current().unwrap())
            );
            assert_eq!(lease["chromeHost"], endpoint.host);
            assert_eq!(lease["chromePort"], endpoint.port);
            assert_eq!(lease["browserWSEndpoint"], endpoint.browser_ws_endpoint);
            assert_eq!(lease["chromeTargetId"], "REOPEN-PUBLIC");
            assert_eq!(lease["observationIntent"]["version"], 1);
            assert_eq!(lease["observationIntent"]["url"], URL);
            uuid::Uuid::parse_str(lease["observationIntent"]["id"].as_str().unwrap()).unwrap();
            assert_eq!(lease["targetDispositionPending"], true);
            assert!(lease["targetDispositionAllocationId"].is_null());
            assert_ne!(lease["promptSubmitted"], true);
            assert_ne!(lease["capacityReleased"], true);
        } else {
            assert!(leases.is_empty(), "{case}: admission leaked");
        }
    }
}

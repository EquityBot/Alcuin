use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};
async fn page_case(case: &'static str) -> Value {
    let (client, server) = tokio::io::duplex(512 * 1024);
    let page = Cdp::from_socket(
        WebSocketStream::from_raw_socket(client, Role::Client, None).await,
        Duration::from_secs(1),
    );
    let mut socket = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let peer = tokio::spawn(async move {
        let mut model_reads = 0;
        let mut effort_reads = 0;
        while let Some(Ok(Message::Text(frame))) = socket.next().await {
            let request: Value = serde_json::from_str(&frame).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "Page.enable" | "Runtime.enable" => json!({}),
                "Page.navigate" => {
                    assert_eq!(
                        request["params"]["url"],
                        "https://chatgpt.com/?temporary-chat=true"
                    );
                    json!({"frameId":"PUBLIC-frame"})
                }
                "Runtime.evaluate" if request["params"]["expression"] == "location.href" => {
                    json!({"result":{"value":"https://chatgpt.com/?temporary-chat=true"}})
                }
                "Runtime.evaluate" => {
                    let expression = request["params"]["expression"].as_str().unwrap();
                    let args: Value = serde_json::from_str(
                        expression
                            .rsplit_once(")(...")
                            .unwrap()
                            .1
                            .strip_suffix(')')
                            .unwrap(),
                    )
                    .unwrap();
                    let cfg = &args[0];
                    let value = if expression.contains("alcuin-dom:health-login") {
                        assert_eq!(cfg["timeoutMs"], 2000);
                        json!({"pageUrl":if case=="foreign-origin"{"https://example.invalid/"}else{"https://chatgpt.com/"},"domLoginCta":case=="login","cfBlocked":case=="challenge","sessionResolved":case!="unresolved","sessionAuthenticated":case!="login"})
                    } else if expression.contains("alcuin-dom:navigation") {
                        json!({"currentUrl":"https://chatgpt.com/?temporary-chat=true","workSelected":false,"modeUnverified":false})
                    } else if expression.contains("alcuin-dom:health-guard") {
                        json!({"throttled":case=="throttle","modelsLoadFailed":case=="models-broken"})
                    } else if expression.contains("alcuin-dom:submission") {
                        assert!(matches!(
                            cfg["action"].as_str(),
                            Some("acquire" | "assert" | "releasing" | "clean" | "release")
                        ));
                        json!(true)
                    } else if expression.contains("alcuin-dom:inspection-prepare") {
                        json!({"token":cfg["token"],"parentReservationId":cfg["owner"]["reservationId"],"rendererId":"PUBLIC-renderer","acknowledged":true})
                    } else if expression.contains("alcuin-dom:inspection-finish") {
                        json!({"token":cfg["token"],"parentReservationId":cfg["owner"]["reservationId"],"rendererId":cfg["rendererId"],"acknowledged":true,"cleanupComplete":true})
                    } else if expression.contains("alcuin-dom:model") {
                        assert_eq!(cfg["strategy"], "current");
                        assert_eq!(cfg["inspectCurrent"], "evidence");
                        model_reads += 1;
                        json!({"status":"already-selected","label":match case {"wrong-model"=>"GPT-5.6 Sol","pro-model"|"extra-high-pro-model"=>"Pro",_=>"GPT-6"},"checkedCount":1,"ownerKey":if case=="changed-model"&&model_reads>1{"PUBLIC-changed"}else{"PUBLIC-model"}})
                    } else if expression.contains("alcuin-dom:thinking") {
                        assert_eq!(cfg["inspectionOnly"], true);
                        assert_eq!(cfg["inspectionEvidence"], true);
                        effort_reads += 1;
                        json!({"status":if case=="unavailable"||case=="missing-capability"{"capability_unavailable"}else{"ready"},"selectedLevel":if case=="changed-effort"&&effort_reads>1||case=="extra-high-pro-model"{"extra-high"}else{"pro"},"ownerKey":"PUBLIC-effort","basis":if case=="unavailable"||case=="missing-capability"{"disabled-tier-row"}else{"slider-label-and-index"},"capabilityKey":if case=="unavailable"{json!("PUBLIC-disabled")}else{Value::Null}})
                    } else {
                        panic!("Unexpected health mutation")
                    };
                    json!({"result":{"value":value}})
                }
                other => panic!("Health issued unqualified operation {other}"),
            };
            if socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut result = json!({"status":"controls_broken","observations":{"authenticated":false,"modelVerified":false,"effortVerified":false}});
    inspect_page(
        &page,
        &mut result,
        if case == "extra-high-pro-model" {
            "extra-high"
        } else {
            "pro"
        },
        Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    page.disconnect_and_wait(Duration::from_secs(1))
        .await
        .unwrap();
    peer.await.unwrap();
    result
}
#[tokio::test]
async fn health_observes_without_selection_and_rechecks_both_controls_and_access() {
    // GPT-6's top Power step checks the Pro radio; that is ready only for level pro.
    for case in ["ready", "pro-model"] {
        let result = page_case(case).await;
        assert_eq!(result["status"], "ready", "{case}");
        assert_eq!(
            result["observations"],
            json!({"authenticated":true,"modelVerified":true,"effortVerified":true})
        );
    }
    let result = page_case("unavailable").await;
    assert_eq!(result["status"], "capability_unavailable");
    assert_eq!(result["observations"]["modelVerified"], true);
    assert_eq!(result["observations"]["effortVerified"], false);
    for case in [
        "changed-model",
        "changed-effort",
        "wrong-model",
        "extra-high-pro-model",
        "missing-capability",
    ] {
        assert_eq!(
            page_case(case).await["status"],
            "selection_unknown",
            "{case}"
        );
    }
    for (case, status, reason) in [
        ("login", "sign_in_required", None),
        ("unresolved", "controls_broken", None),
        ("foreign-origin", "controls_broken", None),
        ("challenge", "controls_broken", Some("challenge_detected")),
        ("throttle", "throttled", None),
        ("models-broken", "controls_broken", None),
    ] {
        let result = page_case(case).await;
        assert_eq!(result["status"], status, "{case}");
        assert_eq!(result["reason"].as_str(), reason, "{case}");
        assert_ne!(result["observations"]["modelVerified"], true);
    }
}
#[test]
fn evidence_normalisation_and_custom_limits_fail_closed() {
    for value in [
        json!({"status":"ready","selectedLevel":"pro","basis":"checked-tier-row"}),
        // Only the direct Power slider proves Pro.
        json!({"status":"ready","selectedLevel":"pro","ownerKey":"owner","basis":"checked-tier-row"}),
        json!({"status":"ready","selectedLevel":"pro","ownerKey":"owner","basis":"text"}),
        json!({"status":"capability_unavailable","ownerKey":"owner","basis":"disabled-tier-row"}),
    ] {
        assert_eq!(effort(&value)["status"], "selection_unknown");
    }
    assert_eq!(
        effort(
            &json!({"status":"ready","selectedLevel":"extra-high","ownerKey":"owner","basis":"checked-tier-row"})
        )["status"],
        "ready"
    );
    for (value, expected) in [
        (json!(null), 3),
        (json!(0), 3),
        (json!("2tabs"), 2),
        (json!(" +4 "), 4),
        (json!(1.5), 1),
        (json!(-1), 3),
        (json!(true), 1),
    ] {
        assert_eq!(tab_limit(&value), expected);
    }
    assert_eq!(
        format(&json!({"status":"ready","cleanupComplete":true})),
        "Browser health: ready. Advisory snapshot; cleanup confirmed."
    );
}
#[tokio::test]
async fn health_rejects_invalid_deadlines_before_profile_or_browser_changes() {
    let home = tempfile::tempdir().unwrap();
    let env = Environment::from([
        ("HOME".into(), home.path().to_string_lossy().into_owned()),
        (
            "USERPROFILE".into(),
            home.path().to_string_lossy().into_owned(),
        ),
    ]);
    for duration in [Duration::ZERO, Duration::from_secs(301)] {
        assert!(
            inspect(
                &json!({}),
                &env,
                home.path(),
                duration,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

/// Polls the persisted admission queue without taking its lock until a health waiter is
/// registered, `stop` is cancelled or 30 s pass. Returns whether the waiter was seen.
async fn health_waiter_registered(path: std::path::PathBuf, stop: CancellationToken) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !stop.is_cancelled() && std::time::Instant::now() < deadline {
        // A read can race an atomic replacement; the next poll sees the published file.
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(waiters) = serde_json::from_slice::<Value>(&bytes)
            && waiters["waiters"].as_array().is_some_and(|rows| {
                rows.iter().any(|row| {
                    row["sessionId"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("health-"))
                })
            })
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}
#[tokio::test]
async fn admission_deadline_and_external_cancellation_retire_the_exact_fifo_waiter() {
    let home = tempfile::tempdir().unwrap();
    let env = Environment::from([
        ("HOME".into(), home.path().to_string_lossy().into_owned()),
        (
            "USERPROFILE".into(),
            home.path().to_string_lossy().into_owned(),
        ),
    ]);
    let profile = ProfileRegistry::open(home.path()).unwrap();
    let blocker = profile
        .acquire_with_limit(
            "PUBLIC-blocker",
            1,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let input = json!({"manualLoginProfileDir":home.path(),"maxConcurrentTabs":1});
    let queue = home.path().join("oracle-tab-waiters.json");
    // The deadline must outlast waiter registration under parallel load, so the retirement
    // under test runs rather than a deadline that expired before the waiter was queued.
    let stop = CancellationToken::new();
    let observer = tokio::spawn(health_waiter_registered(queue.clone(), stop.clone()));
    let result = inspect(
        &input,
        &env,
        home.path(),
        Duration::from_secs(3),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    stop.cancel();
    assert!(
        observer.await.unwrap(),
        "The health waiter was never registered"
    );
    assert_eq!(result["status"], "controls_broken");
    assert_eq!(result["reason"], "admission_deadline_exceeded");
    assert_eq!(result["cleanupComplete"], false);
    let waiters: Value = serde_json::from_slice(&std::fs::read(&queue).unwrap()).unwrap();
    assert!(waiters["waiters"].as_array().unwrap().is_empty());
    // Cancel only once the waiter is queued, so cancellation is what retires it.
    let token = CancellationToken::new();
    let cancel = token.clone();
    let cancellation = tokio::spawn(async move {
        let registered = health_waiter_registered(queue, CancellationToken::new()).await;
        cancel.cancel();
        registered
    });
    assert!(
        inspect(&input, &env, home.path(), Duration::from_secs(30), &token)
            .await
            .is_err()
    );
    assert!(
        cancellation.await.unwrap(),
        "The cancelled health waiter was never registered"
    );
    let waiters: Value = serde_json::from_slice(
        &std::fs::read(home.path().join("oracle-tab-waiters.json")).unwrap(),
    )
    .unwrap();
    assert!(waiters["waiters"].as_array().unwrap().is_empty());
    blocker.release().unwrap();
    profile
        .acquire_with_limit(
            "PUBLIC-next",
            1,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .release()
        .unwrap();
}
#[tokio::test]
async fn launch_identity_refusal_reports_browser_identity_unverified() {
    let home = tempfile::tempdir().unwrap();
    let env = Environment::from([
        ("HOME".into(), home.path().to_string_lossy().into_owned()),
        (
            "USERPROFILE".into(),
            home.path().to_string_lossy().into_owned(),
        ),
    ]);
    let executable = std::env::current_exe().unwrap();
    let other = home.path().join("PUBLIC-other-executable");
    std::fs::write(&other, "").unwrap();
    let identity = |path: &Path, directory| {
        crate::native_fs::NativeEntry::open(path, directory)
            .unwrap()
            .identity
    };
    // The recorded Chrome executable no longer matches its identity: the profile is not provably ours.
    crate::storage::atomic_json(
        &home.path().join("oracle-chrome-native.json"),
        &json!({"version":1,"processIdentity":crate::process::ProcessIdentity::current().unwrap(),"profileIdentity":identity(home.path(),true),"executable":executable,"executableIdentity":identity(&other,false),"headless":false}),
    )
    .unwrap();
    let result = inspect(
        &json!({"manualLoginProfileDir":home.path(),"chromePath":executable}),
        &env,
        home.path(),
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "controls_broken");
    assert_eq!(result["reason"], "browser_identity_unverified");
    assert!(home.path().join("oracle-chrome-native.json").exists());
}

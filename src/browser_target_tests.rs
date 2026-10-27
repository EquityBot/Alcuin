use super::*;
use crate::browser_profile::ProfileRegistry;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

#[tokio::test]
async fn a_lost_close_ack_still_confirms_absence_without_closing_other_tabs() {
    for observed in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = Arc::new(
            registry
                .acquire(
                    "PUBLIC-lost-close",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap(),
        );
        lease
            .update(&json!({"chromeTargetId":"OWNED-PUBLIC","targetDispositionPending":true}))
            .unwrap();
        let (client, server) = tokio::io::duplex(16 * 1024);
        let client = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let peer = tokio::spawn(async move {
            let mut closes = 0;
            let mut checks = 0;
            while let Some(Ok(Message::Text(frame))) = server.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "Target.closeTarget" => {
                        assert_eq!(request["params"]["targetId"], "OWNED-PUBLIC");
                        closes += 1;
                        continue; // The close succeeded, but its acknowledgement was lost.
                    }
                    "Target.getTargets" => {
                        assert_eq!(closes, 1);
                        checks += 1;
                        json!({"targetInfos":[{"targetId":"USER-PUBLIC","type":"page"}]})
                    }
                    other => panic!("Unexpected close operation {other}"),
                };
                if server
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
            (closes, checks)
        });
        let mut target = OwnedTarget {
            browser: client.clone(),
            page: client.clone(),
            id: "OWNED-PUBLIC".into(),
            lease: lease.clone(),
            retired: false,
            creation: Some(TargetCreation::allocated("OWNED-PUBLIC")),
        };
        let result = if observed {
            target.close_observed(Duration::from_secs(2)).await
        } else {
            target
                .close_until(std::time::Instant::now() + Duration::from_secs(2))
                .await
        };
        assert!(result.is_ok(), "observed={observed}: {result:?}");
        target.release_retired_lease().unwrap();
        assert!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        client
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        let (closes, checks) = peer.await.unwrap();
        assert_eq!(closes, 1);
        assert!(checks > 0);
    }
}
/// Budget for fixture admissions, commands and cleanup that these authority checks do not time.
/// One native publication has been measured stalling for 1.4 s under the parallel suite.
const FIXTURE_BUDGET: Duration = Duration::from_secs(10);
#[tokio::test]
async fn unused_blank_retirement_rechecks_authority_at_the_close_boundary() {
    for case in [
        "blank",
        "last-page",
        "navigated",
        "claimed",
        "recovered",
        "observation",
        "prompt-submitted",
        "allocation-replaced",
        "target-replaced",
        "owner-replaced",
        "late-navigation",
        "late-claim",
        "late-admission",
        "survivor-claim",
        "unconfirmed",
    ] {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = Arc::new(
            registry
                .acquire("PUBLIC-unused", FIXTURE_BUDGET, &CancellationToken::new())
                .await
                .unwrap(),
        );
        lease
            .update(&json!({"chromeTargetId":"UNUSED-PUBLIC","targetDispositionPending":true}))
            .unwrap();
        let foreign = registry
            .acquire("PUBLIC-foreign", FIXTURE_BUDGET, &CancellationToken::new())
            .await
            .unwrap();
        let foreign_before = registry.records().unwrap()["leases"][1].clone();
        let (client, server) = tokio::io::duplex(32 * 1024);
        let client = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            FIXTURE_BUDGET,
        );
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let observed_registry = registry.clone();
        let peer = tokio::spawn(async move {
            let mut listed = false;
            let mut survivor = false;
            let mut closed = false;
            while let Some(Ok(Message::Text(frame))) = server.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "Target.getTargetInfo" => {
                        json!({"targetInfo":{"targetId":"UNUSED-PUBLIC","type":"page","url":if case=="navigated" || case=="late-navigation" && listed {"https://chatgpt.com/"}else{"about:blank"}}})
                    }
                    "Runtime.evaluate" => {
                        let expression = request["params"]["expression"].as_str().unwrap();
                        assert!(expression.contains("industrious-dom:unused-blank"));
                        assert!(
                            expression.contains("sessionStorage.getItem('oracle:target-claim')")
                        );
                        json!({"result":{"value":!(case=="claimed" || case=="late-claim" && listed || case=="survivor-claim" && survivor)}})
                    }
                    "Target.getTargets" => {
                        listed = true;
                        if case == "late-admission" && !closed {
                            let mut rows = observed_registry.records().unwrap();
                            rows["leases"][0]["promptSubmitted"] = json!(true);
                            crate::storage::atomic_json(
                                &observed_registry.path().join("oracle-tab-leases.json"),
                                &rows,
                            )
                            .unwrap();
                        }
                        let mut rows = vec![];
                        if !closed || case == "unconfirmed" {
                            rows.push(json!({"targetId":"UNUSED-PUBLIC","type":"page"}));
                        }
                        if !matches!(case, "last-page" | "survivor-claim") {
                            rows.push(json!({"targetId":"FOREIGN-PUBLIC","type":"page"}));
                        }
                        if survivor {
                            rows.push(json!({"targetId":"SURVIVOR-PUBLIC","type":"page"}));
                        }
                        json!({"targetInfos":rows})
                    }
                    "Target.createTarget" => {
                        assert!(matches!(case, "last-page" | "survivor-claim"));
                        assert_eq!(request["params"]["url"], "about:blank");
                        survivor = true;
                        json!({"targetId":"SURVIVOR-PUBLIC"})
                    }
                    "Target.closeTarget" => {
                        assert!(matches!(case, "blank" | "last-page" | "unconfirmed"));
                        assert_eq!(request["params"]["targetId"], "UNUSED-PUBLIC");
                        assert!(!closed);
                        closed = true;
                        json!({"success":true})
                    }
                    other => panic!("Unexpected unused cleanup: {other}"),
                };
                if server
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
            assert_eq!(
                closed,
                matches!(case, "blank" | "last-page" | "unconfirmed")
            );
            assert_eq!(survivor, matches!(case, "last-page" | "survivor-claim"));
        });
        let mut target = OwnedTarget {
            browser: client.clone(),
            page: client.clone(),
            id: "UNUSED-PUBLIC".into(),
            lease: lease.clone(),
            retired: false,
            creation: Some(TargetCreation::allocated("UNUSED-PUBLIC")),
        };
        match case {
            "recovered" => target.creation = None,
            "observation" => {
                target.creation = Some(TargetCreation::observation(
                    "UNUSED-PUBLIC",
                    &uuid::Uuid::new_v4().to_string(),
                    "https://chatgpt.com/c/public-observation",
                ))
            }
            "prompt-submitted" => lease.update(&json!({"promptSubmitted":true})).unwrap(),
            "allocation-replaced" => lease
                .update(&json!({"targetDispositionAllocationId":uuid::Uuid::new_v4().to_string()}))
                .unwrap(),
            "target-replaced" => lease
                .update(&json!({"chromeTargetId":"FOREIGN-PUBLIC"}))
                .unwrap(),
            "owner-replaced" => {
                let mut rows = registry.records().unwrap();
                rows["leases"][0]["pid"] = json!(999999);
                rows["leases"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("processIdentity");
                crate::storage::atomic_json(&registry.path().join("oracle-tab-leases.json"), &rows)
                    .unwrap();
            }
            _ => {}
        }
        let before = registry.records().unwrap();
        // This matrix checks changing authority, rather than disk performance.
        let result = target.retire_unclaimed_blank(FIXTURE_BUDGET).await;
        let success = matches!(case, "blank" | "last-page");
        assert_eq!(result.is_ok(), success, "{case}: {result:?}");
        assert_eq!(target.retired, success);
        let after = registry.records().unwrap_or_else(|error| {
            panic!("{case}: registry unavailable after {result:?}: {error:#}")
        });
        if success {
            assert_eq!(after["leases"], json!([foreign_before]));
        } else if case == "late-admission" {
            let mut expected = before;
            expected["leases"][0]["promptSubmitted"] = json!(true);
            assert_eq!(after, expected);
        } else {
            assert_eq!(after, before);
        }
        foreign.release().unwrap();
        client.disconnect_and_wait(FIXTURE_BUDGET).await.unwrap();
        peer.await.unwrap();
    }
}
#[tokio::test]
async fn open_observation_handover_requires_positive_purpose_and_preserves_other_owners() {
    const URL: &str = "https://chatgpt.com/c/public-observation";
    for close in [false, true] {
        for case in [
            "visible",
            "generic",
            "recovered",
            "claimed",
            "url-changed",
            "intent-replaced",
            "journal-bound",
            "owner-replaced",
            "cancelled",
            "receipt-conflict",
            "receipt-retry",
            "contended",
            "cancelled-contended",
            "blank",
            "prompt-submitted",
            "target-replaced",
            "raced-journal",
        ] {
            if close
                && matches!(
                    case,
                    "receipt-conflict" | "receipt-retry" | "cancelled" | "cancelled-contended"
                )
                || !close && matches!(case, "blank" | "raced-journal")
            {
                continue;
            }
            let home = tempfile::tempdir().unwrap();
            let registry = ProfileRegistry::open(home.path()).unwrap();
            let cancel = CancellationToken::new();
            let lease = Arc::new(
                registry
                    .acquire("PUBLIC-observation", FIXTURE_BUDGET, &cancel)
                    .await
                    .unwrap(),
            );
            lease
                .update(&json!({"chromeHost":"127.0.0.1","chromePort":9222}))
                .unwrap();
            let (client, server) = tokio::io::duplex(16 * 1024);
            let client = Cdp::from_socket(
                WebSocketStream::from_raw_socket(client, Role::Client, None).await,
                FIXTURE_BUDGET,
            );
            let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
            let observed_registry = registry.clone();
            let peer = tokio::spawn(async move {
                let mut created = 0;
                let mut closed = false;
                while let Some(Ok(Message::Text(frame))) = server.next().await {
                    let request: Value = serde_json::from_str(&frame).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "Target.createTarget" => {
                            created += 1;
                            assert_eq!(created, 1);
                            assert_eq!(request["params"]["url"], "about:blank");
                            let rows = observed_registry.records().unwrap();
                            let record = &rows["leases"][0];
                            assert_eq!(record["targetDispositionPending"], true);
                            assert_eq!(
                                record["observationIntent"]["version"] == 1,
                                case != "generic"
                            );
                            json!({"targetId":"OBSERVE-PUBLIC"})
                        }
                        "Target.attachToTarget" => json!({"sessionId":"OBSERVATION-PUBLIC"}),
                        "Target.getTargetInfo" => {
                            json!({"targetInfo":{"targetId":"OBSERVE-PUBLIC","type":"page","url":if case=="url-changed"{"https://chatgpt.com/c/public-replaced"}else if case=="blank"{"about:blank"}else{URL}}})
                        }
                        "Runtime.evaluate" => {
                            assert!(
                                request["params"]["expression"]
                                    .as_str()
                                    .unwrap()
                                    .contains("industrious-dom:observation-handover")
                            );
                            json!({"result":{"value":{"url":if case=="blank"{"about:blank"}else{URL},"unclaimed":case!="claimed"}}})
                        }
                        "Target.getTargets" => {
                            if case == "raced-journal" {
                                let mut rows = observed_registry.records().unwrap();
                                rows["leases"][0]["targetDispositionAllocationId"] =
                                    json!("11111111-1111-4111-8111-111111111111");
                                crate::storage::atomic_json(
                                    &observed_registry.path().join("oracle-tab-leases.json"),
                                    &rows,
                                )
                                .unwrap();
                            }
                            json!({"targetInfos":if closed{vec![json!({"targetId":"FOREIGN-PUBLIC","type":"page"})]}else{vec![json!({"targetId":"OBSERVE-PUBLIC","type":"page"}),json!({"targetId":"FOREIGN-PUBLIC","type":"page"})]}})
                        }
                        "Target.closeTarget" => {
                            assert!(close && matches!(case, "visible" | "blank"));
                            assert_eq!(request["params"]["targetId"], "OBSERVE-PUBLIC");
                            assert!(!closed);
                            closed = true;
                            json!({"success":true})
                        }
                        other => panic!("Unexpected observation command: {other}"),
                    };
                    if server
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
                assert_eq!(created, 1);
                assert_eq!(closed, close && matches!(case, "visible" | "blank"));
            });
            let acquisition = TargetAcquisition::default();
            let mut target = if case == "generic" {
                acquisition
                    .acquire(client.clone(), lease.clone(), &cancel, FIXTURE_BUDGET)
                    .await
                    .unwrap()
            } else {
                acquisition
                    .acquire_observation(
                        client.clone(),
                        lease.clone(),
                        URL,
                        &cancel,
                        FIXTURE_BUDGET,
                    )
                    .await
                    .unwrap()
            };
            let foreign = registry
                .acquire("PUBLIC-foreign", FIXTURE_BUDGET, &cancel)
                .await
                .unwrap();
            let foreign_before = registry.records().unwrap()["leases"][1].clone();
            assert!(lease.update(&json!({"observationIntent":null})).is_err());
            let receipt = target.creation().unwrap().allocation_id().to_owned();
            let receipt = home
                .path()
                .join(format!("oracle-observation-handover-{receipt}.json"));
            match case {
                "recovered" => target.creation = None,
                "journal-bound" => lease
                    .update(
                        &json!({"targetDispositionAllocationId":uuid::Uuid::new_v4().to_string()}),
                    )
                    .unwrap(),
                "cancelled" | "cancelled-contended" => cancel.cancel(),
                "prompt-submitted" => lease.update(&json!({"promptSubmitted":true})).unwrap(),
                "target-replaced" => lease
                    .update(&json!({"chromeTargetId":"FOREIGN-PUBLIC"}))
                    .unwrap(),
                "intent-replaced" | "owner-replaced" => {
                    let mut rows = registry.records().unwrap();
                    let record = &mut rows["leases"][0];
                    if case == "intent-replaced" {
                        record["observationIntent"]["id"] = json!(uuid::Uuid::new_v4().to_string());
                    } else {
                        record["pid"] = json!(999999);
                        record.as_object_mut().unwrap().remove("processIdentity");
                    }
                    crate::storage::atomic_json(&home.path().join("oracle-tab-leases.json"), &rows)
                        .unwrap();
                }
                "receipt-conflict" => {
                    crate::storage::atomic_json(&receipt, &json!({"PUBLIC":"preserve"})).unwrap()
                }
                "receipt-retry" => {
                    let mut proof = registry.records().unwrap()["leases"][0].clone();
                    proof.as_object_mut().unwrap().remove("updatedAt");
                    let authority = json!({"version":1,"kind":"open-observation-handover","creation":target.creation(),"intent":proof["observationIntent"],"lease":proof,"admissionRelease":"authorised"});
                    crate::storage::atomic_json(&receipt, &authority).unwrap();
                    lease.update(&json!({})).unwrap();
                }
                _ => {}
            }
            let mut before = registry.records().unwrap();
            let held = matches!(case, "contended" | "cancelled-contended").then(|| {
                crate::storage::DirectoryLock::acquire(
                    &home.path().join("oracle-tab-leases.lock"),
                    FIXTURE_BUDGET,
                )
                .unwrap()
            });
            let started = std::time::Instant::now();
            let limit = if held.is_some() {
                Duration::from_millis(50)
            } else {
                FIXTURE_BUDGET
            };
            let result = if close {
                target
                    .close_observation(URL, limit)
                    .await
                    .map(|_| Value::Null)
            } else {
                target.handover_open_observation(URL, limit, &cancel).await
            };
            if held.is_some() {
                assert!(
                    started.elapsed() < Duration::from_millis(350),
                    "Handover exceeded its initial lease deadline: {case}"
                );
            }
            drop(held);
            let success = matches!(case, "visible" | "receipt-retry" | "blank");
            assert_eq!(result.is_ok(), success, "close={close} {case}: {result:?}");
            let after = registry.records().unwrap();
            if success {
                assert_eq!(after["leases"], json!([foreign_before]));
                if close {
                    assert!(!receipt.exists());
                    assert!(target.retired);
                } else {
                    let saved: Value =
                        serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
                    assert_eq!(saved, result.unwrap());
                    assert_eq!(saved["creation"]["kind"], "historical-observation");
                    assert!(saved.get("absence").is_none());
                }
                assert!(lease.require().is_err());
                let store = crate::sessions::SessionStore::new(home.path());
                let journal = crate::browser_disposition::TargetJournal::open(&store).unwrap();
                assert!(journal.record_creation(target.creation().unwrap(),&json!({"targetId":"OBSERVE-PUBLIC","claimId":uuid::Uuid::new_v4().to_string(),"host":"127.0.0.1","port":9222}),&crate::process::ProcessIdentity::current().unwrap(),Some("PUBLIC-observation"),None).is_err());
            } else {
                if case == "raced-journal" {
                    before["leases"][0]["targetDispositionAllocationId"] =
                        json!("11111111-1111-4111-8111-111111111111");
                }
                assert_eq!(after, before, "{case}");
                if case == "receipt-conflict" {
                    assert_eq!(
                        std::fs::read(&receipt).unwrap(),
                        serde_json::to_vec_pretty(&json!({"PUBLIC":"preserve"})).unwrap()
                    );
                }
            }
            foreign.release().unwrap();
            client.disconnect_and_wait(FIXTURE_BUDGET).await.unwrap();
            peer.await.unwrap();
        }
    }
}
#[tokio::test]
async fn closing_the_last_owned_health_page_requires_a_distinct_survivor_before_close() {
    for (invalid, contended) in [(false, false), (true, false), (false, true)] {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = Arc::new(
            registry
                .acquire(
                    "PUBLIC-health",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap(),
        );
        lease
            .update(&json!({"chromeTargetId":"HEALTH-PUBLIC","targetDispositionPending":true}))
            .unwrap();
        let (client, server) = tokio::io::duplex(16 * 1024);
        let client = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let lock_path = home.path().join("oracle-tab-leases.lock");
        let peer = tokio::spawn(async move {
            let mut held_lock = None;
            let mut survivor = false;
            let mut closed = false;
            while let Some(Ok(Message::Text(frame))) = server.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "Target.getTargets" => {
                        if closed && contended && held_lock.is_none() {
                            held_lock = Some(
                                crate::storage::DirectoryLock::acquire(
                                    &lock_path,
                                    Duration::from_secs(1),
                                )
                                .unwrap(),
                            );
                        }
                        json!({"targetInfos":if closed{vec![json!({"targetId":"SURVIVOR-PUBLIC","type":"page"})]}else{vec![json!({"targetId":"HEALTH-PUBLIC","type":"page"})]}})
                    }
                    "Target.createTarget" => {
                        assert_eq!(request["params"]["url"], "about:blank");
                        survivor = true;
                        json!({"targetId":if invalid{"HEALTH-PUBLIC"}else{"SURVIVOR-PUBLIC"}})
                    }
                    "Target.closeTarget" => {
                        assert!(survivor && !invalid);
                        assert_eq!(request["params"]["targetId"], "HEALTH-PUBLIC");
                        closed = true;
                        json!({"success":true})
                    }
                    other => panic!("Unexpected cleanup {other}"),
                };
                if server
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
            assert!(survivor);
            assert_eq!(closed, !invalid);
            if let Some(lock) = held_lock {
                lock.release().unwrap();
            }
        });
        let mut target = OwnedTarget {
            browser: client.clone(),
            page: client.clone(),
            id: "HEALTH-PUBLIC".into(),
            lease: lease.clone(),
            retired: false,
            creation: Some(TargetCreation::allocated("HEALTH-PUBLIC")),
        };
        let started = std::time::Instant::now();
        let cleanup = target
            .close_preserving_browser(Duration::from_secs(1))
            .await;
        assert_eq!(
            cleanup.is_ok(),
            !invalid && !contended,
            "invalid={invalid}, contended={contended}, cleanup={cleanup:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1600),
            "Cleanup restarted its native mutex budget"
        );
        client
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
        assert_eq!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty(),
            !invalid && !contended
        );
    }
}
#[tokio::test]
async fn cancellation_retains_allocation_until_exact_late_blank_target_absence_is_confirmed() {
    for observation in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::open(home.path()).unwrap();
        let lease = Arc::new(
            registry
                .acquire(
                    "late-public",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap(),
        );
        lease
            .update(&json!({"chromeHost":"127.0.0.1","chromePort":9222}))
            .unwrap();
        let (client, server) = tokio::io::duplex(16 * 1024);
        let client = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let (issued, seen) = oneshot::channel();
        let (resume, continued) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let first = server.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Target.createTarget");
            issued.send(()).unwrap();
            continued.await.unwrap();
            server
                .send(Message::Text(
                    json!({"id":request["id"],"result":{"targetId":"LATE-PUBLIC"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let mut closed = false;
            while let Some(Ok(Message::Text(frame))) = server.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "Target.attachToTarget" => json!({"sessionId":"LATE-SESSION"}),
                    "Target.getTargetInfo" => {
                        json!({"targetInfo":{"targetId":"LATE-PUBLIC","type":"page","url":"about:blank"}})
                    }
                    "Target.closeTarget" => {
                        assert_eq!(request["params"]["targetId"], "LATE-PUBLIC");
                        assert!(!closed);
                        closed = true;
                        json!({"success":true})
                    }
                    "Target.getTargets" => {
                        json!({"targetInfos":[{"targetId":"FOREIGN-PUBLIC","type":"page"}]})
                    }
                    "Runtime.evaluate" => {
                        json!({"result":{"value":{"url":"about:blank","unclaimed":true}}})
                    }
                    other => panic!("Foreign or unaccounted operation {other}"),
                };
                server
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let acquisition = Arc::new(TargetAcquisition::default());
        let cancel = CancellationToken::new();
        let copy = acquisition.clone();
        let token = cancel.clone();
        let connection = client.clone();
        let allocation_lease = lease.clone();
        let allocating = tokio::spawn(async move {
            if observation {
                copy.acquire_observation(
                    connection,
                    allocation_lease,
                    "https://chatgpt.com/c/public-late",
                    &token,
                    Duration::from_secs(1),
                )
                .await
            } else {
                copy.acquire(connection, allocation_lease, &token, Duration::from_secs(1))
                    .await
            }
        });
        seen.await.unwrap();
        cancel.cancel();
        assert!(allocating.await.unwrap().is_err());
        assert!(lease.release().is_err());
        assert_eq!(
            registry.records().unwrap()["leases"][0]["targetDispositionPending"],
            true
        );
        resume.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while acquisition.state().await != AllocationState::Absent {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            registry.records().unwrap()["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        client
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
    }
}

use super::*;

#[test]
fn disconnect_failure_preserves_the_first_retirement_or_settlement_cause() {
    for first in [
        None,
        Some("PUBLIC retirement refused"),
        Some("PUBLIC settlement refused"),
    ] {
        let session = json!({"status":"completed","PUBLIC":"answer survives"});
        let disposition = Some(json!({"decision":"retained","absence":"unknown"}));
        let mut completion = HistoricalCompletion {
            session: session.clone(),
            disposition: disposition.clone(),
            cleanup_error: first.map(str::to_owned),
        };
        completion.add_cleanup_error(anyhow::anyhow!("PUBLIC transport teardown failed"));
        assert_eq!(completion.session, session);
        assert_eq!(completion.disposition, disposition);
        assert_eq!(
            completion.cleanup_error,
            Some(match first {
                Some(first) =>
                    format!("{first}; disconnect also failed: PUBLIC transport teardown failed"),
                None => "PUBLIC transport teardown failed".into(),
            })
        );
    }
}
use crate::{browser_chrome::AttachmentEndpoint, browser_harvest::HarvestIntegrity, storage};

#[tokio::test]
async fn creatorless_restart_settles_only_saved_absence_with_inventory_traffic() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    for case in [
        "absent",
        "present",
        "bad-version",
        "bad-id",
        "target",
        "claim",
        "endpoint",
        "capture",
        "completion",
        "output",
        "live-owner",
        "live-lease",
        "cancel",
        "kept",
        "no-attempt",
        "invalid-inventory",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (home, store, initial, mut snapshot, _) = fixture();
        let id = initial["id"].as_str().unwrap().to_owned();
        without_creator(&store, &id, false);
        let endpoint = format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-original");
        let output = home.path().join("PUBLIC-output.md");
        let saved = store.update(&id, |current| {
            let mut browser = current["browser"].clone();
            browser["config"]["keepBrowser"] = json!(false);
            browser["runtime"]["chromePort"] = json!(port);
            browser["runtime"]["chromeBrowserWSEndpoint"] = json!(endpoint);
            browser["runtime"]["ownedRecoveryTarget"]["port"] = json!(port);
            browser["runtime"]["ownedRecoveryTarget"]["browserWSEndpoint"] = json!(endpoint);
            Ok(Some(json!({"browser":browser,"options":sessions::merge_object(&current["options"],&json!({"writeOutputPath":output}))})))
        }).unwrap();
        snapshot.endpoint.port = port;
        snapshot.endpoint.browser_ws_endpoint = endpoint;
        snapshot.tab["port"] = json!(port);
        let owned = eligible(&saved, &snapshot).unwrap().unwrap();
        let completed = commit_historical(
            &store,
            &id,
            &saved,
            &snapshot,
            &owned,
            None,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let attempt = json!({"version":1,"id":Uuid::new_v4().to_string(),"binding":{"target":owned,"capture":completed["browser"]["capturedAnswer"],"completedAt":completed["completedAt"]}});
        store
            .update(&id, |current| {
                let mut browser = current["browser"].clone();
                browser["runtime"]["creatorlessRetirement"] = attempt;
                browser["runtime"]["targetDispositionPending"] = json!(true);
                match case {
                    "bad-version" => {
                        browser["runtime"]["creatorlessRetirement"]["version"] = json!(2)
                    }
                    "bad-id" => {
                        browser["runtime"]["creatorlessRetirement"]["id"] = json!("PUBLIC-invalid")
                    }
                    "target" => {
                        browser["runtime"]["ownedRecoveryTarget"]["targetId"] =
                            json!("PUBLIC-other")
                    }
                    "claim" => {
                        browser["runtime"]["ownedRecoveryTarget"]["claimId"] = json!("PUBLIC-other")
                    }
                    "endpoint" => {
                        browser["runtime"]["ownedRecoveryTarget"]["browserWSEndpoint"] = Value::Null
                    }
                    "capture" => browser["capturedAnswer"]["sha256"] = json!("PUBLIC-changed"),
                    "completion" => {
                        browser["runtime"]["creatorlessRetirement"]["binding"]["completedAt"] =
                            json!("PUBLIC-changed")
                    }
                    "live-owner" => {
                        let controller = ProcessIdentity::current().unwrap();
                        browser["runtime"]["controllerPid"] = json!(controller.pid);
                        browser["runtime"]["controllerProcessStartedAtMs"] =
                            json!(controller.started_at_ms);
                    }
                    "kept" => browser["config"]["keepBrowser"] = json!(true),
                    "no-attempt" => browser["runtime"]
                        .as_object_mut()
                        .unwrap()
                        .remove("creatorlessRetirement")
                        .map(|_| ())
                        .unwrap(),
                    _ => {}
                }
                Ok(Some(json!({"browser":browser,"recoveryRequired":true})))
            })
            .unwrap();
        if case == "output" {
            std::fs::write(&output, "PUBLIC damaged").unwrap();
        }
        let live_lease = if case == "live-lease" {
            let path = home.path().join("PUBLIC-live-profile");
            let profile = crate::browser_profile::ProfileRegistry::open(&path).unwrap();
            let lease = profile
                .acquire(
                    "PUBLIC-other-session",
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            lease.update(&json!({"chromeHost":"127.0.0.1","chromePort":port,"chromeTargetId":"PUBLIC-owned"})).unwrap();
            store
                .update(&id, |current| {
                    let mut browser = current["browser"].clone();
                    browser["runtime"]["userDataDir"] = json!(path);
                    Ok(Some(json!({"browser":browser})))
                })
                .unwrap();
            Some(lease)
        } else {
            None
        };
        let before = store.stored_for_update(&id).unwrap();
        let models = store.model_runs(&id).unwrap();
        let answer = std::fs::read(
            store.directory(&id).unwrap().join(
                completed["browser"]["capturedAnswer"]["file"]
                    .as_str()
                    .unwrap(),
            ),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        if case == "cancel" {
            cancel.cancel();
        }
        let server = tokio::spawn(async move {
            let accepted = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
            let Ok(Ok((socket, _))) = accepted else {
                return vec![];
            };
            #[expect(
                clippy::result_large_err,
                reason = "Tungstenite requires its unboxed ErrorResponse in the handshake callback"
            )]
            let verify_endpoint =
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.uri().path(), "/devtools/browser/PUBLIC-original");
                    Ok(response)
                };
            let mut peer = tokio_tungstenite::accept_hdr_async(socket, verify_endpoint)
                .await
                .unwrap();
            let mut seen = vec![];
            while let Some(Ok(Message::Text(frame))) = peer.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(request["method"], "Target.getTargets");
                seen.push(request.clone());
                let rows = if case == "present" {
                    json!([{"targetId":"PUBLIC-owned","type":"page"}])
                } else if case == "invalid-inventory" {
                    json!([{}])
                } else {
                    json!([])
                };
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":{"targetInfos":rows}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
            seen
        });
        let reopened = SessionStore::new(store.home());
        let result = crate::browser_chrome::TEST_STARTUP_LOCK
            .scope(
                home.path().join("PUBLIC-startup.lock"),
                resume_creatorless_absence(&reopened, &id, &cancel),
            )
            .await;
        if matches!(case, "absent" | "kept" | "present" | "invalid-inventory") {
            let seen = server.await.unwrap();
            assert_eq!(seen.len(), 1, "{case}");
        } else {
            server.abort();
            let _ = server.await;
        }
        // A completed answer retires its tab even in a kept browser, so a kept restart settles.
        if matches!(case, "absent" | "kept") {
            let completion = result.unwrap().unwrap();
            assert!(completion.cleanup_error.is_none());
            assert_eq!(completion.session["recoveryRequired"], false);
            assert_eq!(
                completion.session["browser"]["runtime"]["targetDispositionPending"],
                false
            );
            assert_eq!(
                completion.session["browser"]["runtime"]["creatorlessRetirement"],
                before["browser"]["runtime"]["creatorlessRetirement"]
            );
            assert_eq!(
                completion.session["browser"]["capturedAnswer"],
                completed["browser"]["capturedAnswer"]
            );
        } else {
            if matches!(case, "present" | "no-attempt") {
                assert!(result.unwrap().is_none(), "{case}");
            } else {
                assert!(result.is_err(), "{case}");
            }
            assert_eq!(store.stored_for_update(&id).unwrap(), before, "{case}");
        }
        assert_eq!(store.model_runs(&id).unwrap(), models, "{case}");
        if let Some(lease) = live_lease {
            lease.release().unwrap();
        }
        assert_eq!(
            std::fs::read(
                store.directory(&id).unwrap().join(
                    completed["browser"]["capturedAnswer"]["file"]
                        .as_str()
                        .unwrap()
                )
            )
            .unwrap(),
            answer,
            "{case}"
        );
    }
}

#[test]
fn creatorless_settlement_interruption_keeps_the_attempt_and_retries_without_reclosing() {
    let (_home, store, initial, snapshot, _) = fixture();
    let id = initial["id"].as_str().unwrap();
    let saved = without_creator(&store, id, false);
    let target = eligible(&saved, &snapshot).unwrap().unwrap();
    let completed = commit_historical(
        &store,
        id,
        &saved,
        &snapshot,
        &target,
        None,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    let attempt = json!({"version":1,"id":Uuid::new_v4().to_string(),"binding":{"target":target,"capture":completed["browser"]["capturedAnswer"],"completedAt":completed["completedAt"]}});
    let saved = store
        .update(id, |current| {
            let mut browser = current["browser"].clone();
            browser["runtime"]["creatorlessRetirement"] = attempt.clone();
            browser["runtime"]["targetDispositionPending"] = json!(true);
            Ok(Some(json!({"browser":browser,"recoveryRequired":true})))
        })
        .unwrap();
    let root = store.root();
    let lock_path = root
        .parent()
        .unwrap()
        .join(format!(
            "{}-write-locks",
            root.file_name().unwrap().to_str().unwrap()
        ))
        .join(format!("{id}.lock"));
    let lock = storage::DirectoryLock::acquire(&lock_path, Duration::from_secs(1)).unwrap();
    let disposition =
        json!({"decision":"retired","absence":"confirmed-absent","retirement":attempt});
    assert!(
        settle_creatorless_retirement_until(
            &store,
            id,
            &target,
            &disposition,
            std::time::Instant::now() + Duration::from_millis(80)
        )
        .is_err()
    );
    assert_eq!(store.stored_for_update(id).unwrap(), saved);
    lock.release().unwrap();
    settle_creatorless_retirement(&SessionStore::new(store.home()), id, &target, &disposition)
        .unwrap();
    let retried = store.stored_for_update(id).unwrap();
    assert_eq!(retried["recoveryRequired"], false);
    assert_eq!(
        retried["browser"]["capturedAnswer"],
        saved["browser"]["capturedAnswer"]
    );
    assert_eq!(
        retried["browser"]["runtime"]["creatorlessRetirement"],
        attempt
    );
}

#[test]
fn successful_native_recovery_preserves_its_answer_when_disconnect_fails() {
    let saved = json!({"status":"completed","browser":{"capturedAnswer":{"sha256":"PUBLIC capture identity"}}});
    let completed = recovered_completion(
        Ok(saved.clone()),
        Err(anyhow::anyhow!("PUBLIC disconnect timeout")),
    )
    .unwrap();
    assert_eq!(completed.session, saved);
    assert_eq!(
        completed.cleanup_error.as_deref(),
        Some("PUBLIC disconnect timeout")
    );
    assert!(completed.disposition.is_none());
    let error = recovered_completion(
        Err(anyhow::anyhow!("PUBLIC recovery failure")),
        Err(anyhow::anyhow!("PUBLIC disconnect failure")),
    )
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "PUBLIC recovery failure");
}

fn without_creator(store: &SessionStore, id: &str, no_claim: bool) -> Value {
    store
        .update(id, |current| {
            let mut browser = current["browser"].clone();
            let target = browser["runtime"]["ownedRecoveryTarget"]
                .as_object_mut()
                .unwrap();
            target.remove("creator");
            target.remove("conversationId");
            if no_claim {
                target.remove("claimId");
            }
            Ok(Some(json!({"browser":browser})))
        })
        .unwrap()
}
#[test]
fn creatorless_optional_conversation_fields_preserve_identity_and_reject_conflicts() {
    let (_home, store, saved, snapshot, _) = fixture();
    let id = saved["id"].as_str().unwrap();
    let mut saved = without_creator(&store, id, false);
    saved["browser"]["runtime"]
        .as_object_mut()
        .unwrap()
        .remove("tabUrl");
    saved["browser"]["runtime"]["ownedRecoveryTarget"]["claimId"] = json!("PUBLIC:claim_with.dots");
    assert!(eligible(&saved, &snapshot).unwrap().is_some());
    let mut native = saved.clone();
    native["lifecycle"]["processIdentity"] = json!(ProcessIdentity::current().unwrap());
    assert!(eligible(&native, &snapshot).unwrap().is_none());
    for case in ["url", "conversation", "owned-conversation"] {
        let mut changed = saved.clone();
        match case {
            "url" => {
                changed["browser"]["runtime"]["tabUrl"] =
                    json!("https://chatgpt.com/c/public-foreign")
            }
            "conversation" => {
                changed["browser"]["runtime"]["conversationId"] = json!("public-foreign")
            }
            _ => {
                changed["browser"]["runtime"]["ownedRecoveryTarget"]["conversationId"] =
                    json!("public-foreign")
            }
        }
        assert!(eligible(&changed, &snapshot).unwrap().is_none(), "{case}");
    }
}
#[tokio::test]
async fn creatorless_completion_saves_outputs_without_inventing_a_journal_or_claim() {
    for no_claim in [false, true] {
        let (home, store, saved, snapshot, creator) = fixture();
        let id = saved["id"].as_str().unwrap();
        let saved = without_creator(&store, id, no_claim);
        let output = home.path().join("PUBLIC creatorless.md");
        let saved=store.update(id,|_|Ok(Some(json!({"options":sessions::merge_object(&saved["options"],&json!({"writeOutputPath":output}))})))).unwrap();
        let journal = TargetJournal::open(&store).unwrap();
        let record = journal.read(&creator["allocationId"]).unwrap();
        let completed = complete_owned(&store, id, &snapshot, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.session["status"], "completed");
        assert_eq!(
            completed.session["browser"]["runtime"],
            saved["browser"]["runtime"]
        );
        assert!(completed.disposition.is_none());
        assert_eq!(completed.cleanup_error.is_none(), no_claim);
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "GPT-6 Pro said:\n\n**PUBLIC answer**\n"
        );
        assert_eq!(journal.read(&creator["allocationId"]).unwrap(), record);
        let root = std::fs::read(store.directory(id).unwrap().join("meta.json")).unwrap();
        let log = store.log(id).unwrap();
        complete_owned(&store, id, &snapshot, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
            root
        );
        assert_eq!(store.log(id).unwrap(), log);
        std::fs::write(&output, "PUBLIC changed").unwrap();
        assert!(
            complete_owned(&store, id, &snapshot, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
            root
        );
    }
}
#[test]
fn creatorless_completion_refuses_malformed_authority_and_failed_outputs() {
    for case in [
        "malformed-creator",
        "malformed-claim",
        "output",
        "cancel",
        "new-generation",
        "winning-model",
    ] {
        let (_home, store, saved, snapshot, _) = fixture();
        let id = saved["id"].as_str().unwrap();
        let saved = without_creator(&store, id, false);
        if case.starts_with("malformed") {
            let mut invalid = saved.clone();
            invalid["browser"]["runtime"]["ownedRecoveryTarget"][if case == "malformed-creator" {
                "creator"
            } else {
                "claimId"
            }] = json!(false);
            assert!(eligible(&invalid, &snapshot).unwrap().is_none());
            continue;
        }
        let saved = if case == "output" {
            store.update(id,|_|Ok(Some(json!({"options":sessions::merge_object(&saved["options"],&json!({"writeOutputPath":store.directory(id).unwrap().join("meta.json")}))})))).unwrap()
        } else {
            saved
        };
        let owned = eligible(&saved, &snapshot).unwrap().unwrap();
        let cancel = CancellationToken::new();
        match case {
            "cancel" => cancel.cancel(),
            "new-generation" => {
                store
                    .update(id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["ownedRecoveryTarget"]["claimId"] =
                            json!("PUBLIC-new-generation");
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
            }
            "winning-model" => {
                store
                    .update_model(id, "gpt-6-pro", &json!({"status":"completed"}))
                    .unwrap();
            }
            _ => {}
        }
        let before = store.stored_for_update(id).unwrap();
        assert!(
            commit_historical(&store, id, &saved, &snapshot, &owned, None, &cancel).is_err(),
            "{case}"
        );
        assert_eq!(store.stored_for_update(id).unwrap(), before, "{case}");
    }
}
#[tokio::test]
async fn creatorless_retirement_closes_only_after_fresh_authority_and_exact_marker() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };
    for case in [
        "present",
        "absent",
        "last-page",
        "wrong-claim",
        "changed-marker",
        "new-generation",
        "uncertain-close",
        "retry",
        "live-owner",
        "live-lease",
        "kept",
    ] {
        let (home, store, saved, snapshot, _) = fixture();
        let id = saved["id"].as_str().unwrap().to_owned();
        without_creator(&store, &id, false);
        let saved = store
            .update(&id, |current| {
                let mut browser = current["browser"].clone();
                browser["config"]["keepBrowser"] = json!(case == "kept");
                Ok(Some(json!({"browser":browser})))
            })
            .unwrap();
        let owned = eligible(&saved, &snapshot).unwrap().unwrap();
        let completed = commit_historical(
            &store,
            &id,
            &saved,
            &snapshot,
            &owned,
            None,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let mut active_lease = None;
        match case {
            "live-owner" => {
                store
                    .update(&id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["controllerPid"] = json!(std::process::id());
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
            }
            "live-lease" => {
                let profile = ProfileRegistry::open(&home.path().join("PUBLIC-profile")).unwrap();
                let lease = profile
                    .acquire(&id, Duration::from_secs(1), &CancellationToken::new())
                    .await
                    .unwrap();
                lease.update(&json!({"chromeHost":owned.host,"chromePort":owned.port,"chromeTargetId":owned.target_id})).unwrap();
                store
                    .update(&id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["profileDir"] = json!(profile.path());
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
                active_lease = Some(lease);
            }
            _ => {}
        }
        let saved_for_retirement = if case == "live-lease" {
            store.stored_for_update(&id).unwrap()
        } else {
            completed
        };
        let (client, server) = tokio::io::duplex(128 * 1024);
        let cdp = crate::cdp::Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let peer_store = store.clone();
        let peer_id = id.clone();
        let task = tokio::spawn(async move {
            let mut seen = vec![];
            let mut closed = false;
            let mut close_count = 0;
            while let Some(Ok(Message::Text(frame))) = peer.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let method = request["method"].as_str().unwrap();
                seen.push(request.clone());
                let result = match method {
                    "Target.getTargets" => {
                        if closed && case == "uncertain-close" {
                            peer.send(Message::Text(json!({"id":request["id"],"error":{"message":"PUBLIC absence lost"}}).to_string().into())).await.unwrap();
                            continue;
                        }
                        let mut rows = vec![];
                        if case != "last-page" {
                            rows.push(json!({"targetId":"PUBLIC-borrowed","type":"page"}));
                        }
                        if !closed && case != "absent" {
                            rows.push(json!({"targetId":"PUBLIC-owned","type":"page"}));
                        }
                        json!({"targetInfos":rows})
                    }
                    "Target.attachToTarget" => json!({"sessionId":"PUBLIC-cdp-session"}),
                    "Target.createTarget" => json!({"targetId":"PUBLIC-replacement"}),
                    "Target.closeTarget" => {
                        assert_eq!(request["params"]["targetId"], "PUBLIC-owned");
                        close_count += 1;
                        closed = case != "retry" || close_count > 1;
                        json!({"success":true})
                    }
                    "Runtime.evaluate" => {
                        let expression = request["params"]["expression"].as_str().unwrap();
                        if expression.contains("industrious-dom:claimed-mark") {
                            let saved = peer_store.stored_for_update(&peer_id).unwrap();
                            let marker = saved["browser"]["runtime"]["creatorlessRetirement"]["id"]
                                .as_str()
                                .unwrap();
                            assert!(
                                expression.contains(marker),
                                "Marker must be durable before renderer mutation"
                            );
                        }
                        if expression.contains("industrious-dom:claimed-mark")
                            && case == "new-generation"
                        {
                            peer_store
                                .update(&peer_id, |current| {
                                    let mut browser = current["browser"].clone();
                                    browser["runtime"]["ownedRecoveryTarget"]["claimId"] =
                                        json!("PUBLIC-replaced");
                                    Ok(Some(json!({"browser":browser})))
                                })
                                .unwrap();
                        }
                        json!({"result":{"value":!(case=="wrong-claim"&&expression.contains("industrious-dom:claimed-mark")||case=="changed-marker"&&expression.contains("industrious-dom:claimed-inspect"))}})
                    }
                    _ => panic!("Unexpected {method}"),
                };
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
            seen
        });
        let mut result = crate::browser_retirement::retire_claimed_completed(
            &store,
            &id,
            &saved_for_retirement,
            &owned,
            &cdp,
        )
        .await;
        if case == "retry" {
            assert!(result.is_err(), "First close must remain unconfirmed");
            let saved = store.stored_for_update(&id).unwrap();
            let marker = saved["browser"]["runtime"]["creatorlessRetirement"].clone();
            result = crate::browser_retirement::retire_claimed_completed(
                &store, &id, &saved, &owned, &cdp,
            )
            .await;
            assert_eq!(
                store.stored_for_update(&id).unwrap()["browser"]["runtime"]["creatorlessRetirement"],
                marker
            );
        }
        if let Ok(disposed) = &result {
            let before = store.stored_for_update(&id).unwrap();
            if case == "present" {
                let mut changed = before.clone();
                changed["browser"]["capturedAnswer"]["sha256"] = json!("PUBLIC changed capture");
                store.update(&id, |_| Ok(Some(changed))).unwrap();
                let newer = store.stored_for_update(&id).unwrap();
                assert!(settle_creatorless_retirement(&store, &id, &owned, disposed).is_err());
                assert_eq!(store.stored_for_update(&id).unwrap(), newer);
                store.update(&id, |_| Ok(Some(before.clone()))).unwrap();
            }
            settle_creatorless_retirement(&store, &id, &owned, disposed).unwrap();
            let settled = store.stored_for_update(&id).unwrap();
            assert_eq!(
                settled["browser"]["runtime"]["targetDispositionPending"],
                false
            );
            assert_eq!(settled["recoveryRequired"], false);
            assert_eq!(
                settled["browser"]["capturedAnswer"],
                before["browser"]["capturedAnswer"]
            );
        }
        cdp.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        let seen = task.await.unwrap();
        assert_eq!(
            result.is_ok(),
            matches!(case, "present" | "absent" | "last-page" | "kept" | "retry"),
            "{case}: {result:?}"
        );
        assert_eq!(
            seen.iter()
                .any(|request| request["method"] == "Target.closeTarget"),
            matches!(
                case,
                "present" | "last-page" | "uncertain-close" | "kept" | "retry"
            ),
            "{case}: {result:?}"
        );
        assert_eq!(
            seen.iter().any(|request| request["params"]["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("claimed-rollback"))),
            matches!(case, "wrong-claim" | "changed-marker" | "new-generation"),
            "{case}"
        );
        if case == "live-lease" {
            assert!(active_lease.is_some());
        }
        assert_eq!(store.require(&id).unwrap()["status"], "completed");
    }
}

fn fixture() -> (
    tempfile::TempDir,
    SessionStore,
    Value,
    HarvestSnapshot,
    Value,
) {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join("PUBLIC-store"));
    let metadata = store
        .create(
            json!({"prompt":"PUBLIC source","model":"gpt-6-pro"}),
            home.path(),
            None,
        )
        .unwrap();
    let id = metadata["id"].as_str().unwrap();
    let endpoint = AttachmentEndpoint {
        host: "127.0.0.1".into(),
        port: 1,
        browser_ws_endpoint: "ws://127.0.0.1:1/devtools/browser/PUBLIC-historical".into(),
    };
    let owned = json!({"host":endpoint.host,"port":endpoint.port,"browserWSEndpoint":endpoint.browser_ws_endpoint,"targetId":"PUBLIC-owned","claimId":Uuid::new_v4().to_string(),"conversationId":"public-conversation"});
    let creator = json!({"version":1,"kind":"created","allocationId":Uuid::new_v4().to_string(),"target":owned,"sessionId":id,"controllerPid":u32::MAX,"controllerStartedAtMs":1.0,"createdAt":sessions::timestamp()});
    let journal = TargetJournal::open(&store).unwrap();
    storage::atomic_json(&store.home().join("target-dispositions").join(format!("{}.json",creator["allocationId"].as_str().unwrap())),&json!({"version":1,"creator":creator,"disposition":{"version":1,"allocationId":creator["allocationId"],"target":creator["target"],"decision":"retained","absence":"unknown","reason":"commit-missing","checkedAt":sessions::timestamp()}})).unwrap();
    assert_eq!(
        journal.read(&creator["allocationId"]).unwrap()["creator"],
        creator
    );
    let mut recorded = owned;
    recorded["creator"] = creator.clone();
    let saved=store.update(id,|_|Ok(Some(json!({"mode":"browser","browser":{"config":{"keepBrowser":true},"runtime":{"chromeHost":endpoint.host,"chromePort":endpoint.port,"chromeBrowserWSEndpoint":endpoint.browser_ws_endpoint,"chromeTargetId":"PUBLIC-owned","controllerPid":u32::MAX,"controllerProcessStartedAtMs":1.0,"tabUrl":"https://chatgpt.com/c/public-conversation","conversationId":"public-conversation","submittedPromptHash":browser::prompt_fingerprint("PUBLIC source","PUBLIC-user"),"ownedRecoveryTarget":recorded}}})))).unwrap();
    let tab = json!({"host":endpoint.host,"port":endpoint.port,"targetId":"PUBLIC-owned","url":"https://chatgpt.com/c/public-conversation","conversationId":"public-conversation","state":"completed","authenticated":true,"completionConfirmed":true,"stopExists":false,"assistantFollowsLatestUser":true,"lastUserMessageId":"PUBLIC-user","lastUserTextRaw":"PUBLIC source","lastUserContentText":"PUBLIC source","lastAssistantMessageId":"PUBLIC-assistant","lastAssistantText":"PUBLIC answer","lastAssistantMarkdown":"**PUBLIC answer**"});
    let integrity = HarvestIntegrity {
        status: "matched".into(),
        observed_conversation_id: Some("public-conversation".into()),
        captured: vec![],
        unverified_sources: vec![],
        explicit_target: false,
        previous_harvest_conversation_id: None,
    };
    (
        home,
        store,
        saved,
        HarvestSnapshot {
            endpoint,
            tab,
            integrity,
        },
        creator,
    )
}
#[tokio::test]
async fn recovered_observation_publishes_full_outputs_without_original_target_authority() {
    let (home, store, saved, mut snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let output = home.path().join("PUBLIC recovered observation.md");
    let saved=store.update(id,|current|Ok(Some(json!({"status":"running","options":sessions::merge_object(&current["options"],&json!({"writeOutputPath":output})),"dispatch":{"state":"uncertain","PUBLIC":"preserve"},"response":{"id":"PUBLIC-existing-response","PUBLIC":"preserve"}})))).unwrap();
    store.update_model(id,"gpt-6-pro",&json!({"response":{"id":"PUBLIC-model-response","status":"incomplete","incompleteReason":"incomplete-capture"}})).unwrap();
    snapshot.tab["targetId"] = json!("PUBLIC-observation");
    snapshot.endpoint.port = 2;
    snapshot.endpoint.browser_ws_endpoint =
        "ws://127.0.0.1:2/devtools/browser/PUBLIC-observer".into();
    snapshot.tab["port"] = json!(2);
    let original_record = TargetJournal::open(&store)
        .unwrap()
        .read(&creator["allocationId"])
        .unwrap();
    store.update(id,|current|Ok(Some(json!({"browser":sessions::merge_object(&current["browser"],&json!({"harvest":{"PUBLIC":"concurrent observation"}}))})))).unwrap();
    let completed = complete_recovered(&store, id, &saved, &snapshot, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert!(completed.disposition.is_none() && completed.cleanup_error.is_none());
    assert_eq!(completed.session["status"], "completed");
    assert_eq!(completed.session["models"][0]["status"], "completed");
    assert_eq!(
        completed.session["browser"]["runtime"],
        saved["browser"]["runtime"]
    );
    assert_eq!(completed.session["lifecycle"], saved["lifecycle"]);
    assert_eq!(completed.session["dispatch"], saved["dispatch"]);
    assert_eq!(
        completed.session["response"]["id"],
        "PUBLIC-existing-response"
    );
    let runs = store.model_runs(id).unwrap();
    let model = &runs[0];
    assert_eq!(model["response"]["id"], "PUBLIC-model-response");
    assert_eq!(model["response"]["status"], "completed");
    assert!(model["response"]["incompleteReason"].is_null());
    assert_eq!(
        completed.session["browser"]["harvest"]["PUBLIC"],
        "concurrent observation"
    );
    let capture: artifacts::CaptureReceipt =
        serde_json::from_value(completed.session["browser"]["capturedAnswer"].clone()).unwrap();
    assert_eq!(
        artifacts::verify_capture(&store, &capture).unwrap(),
        b"**PUBLIC answer**"
    );
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "GPT-6 Pro said:\n\n**PUBLIC answer**\n"
    );
    assert_eq!(
        TargetJournal::open(&store)
            .unwrap()
            .read(&creator["allocationId"])
            .unwrap(),
        original_record
    );
}
#[tokio::test]
async fn recovered_observation_refuses_changed_authority_source_outputs_and_terminal_winners() {
    for case in [
        "runtime",
        "controller",
        "outputs",
        "model",
        "cancel",
        "native-null",
        "submission-null",
        "native-journal",
        "explicit",
        "source",
        "conversation",
        "generating",
        "unmatched",
    ] {
        let (_home, store, saved, mut snapshot, creator) = fixture();
        let id = saved["id"].as_str().unwrap();
        let saved = store
            .update(id, |_| Ok(Some(json!({"status":"running"}))))
            .unwrap();
        snapshot.tab["targetId"] = json!("PUBLIC-observation");
        let token = CancellationToken::new();
        match case {
            "runtime" => {
                store
                    .update(id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["chromeTargetId"] = json!("PUBLIC-new");
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
            }
            "controller" => {
                store
                    .update(id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["controllerPid"] = json!(std::process::id());
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
            }
            "outputs" => {
                store.update(id,|current|Ok(Some(json!({"options":sessions::merge_object(&current["options"],&json!({"writeOutputPath":"PUBLIC-new.md"}))})))).unwrap();
            }
            "model" => {
                store
                    .update_model(
                        id,
                        "gpt-6-pro",
                        &json!({"status":"completed","PUBLIC":"winner"}),
                    )
                    .unwrap();
            }
            "cancel" => token.cancel(),
            "native-null" => {
                store
                    .update(id, |_| {
                        Ok(Some(json!({"lifecycle":{"processIdentity":null}})))
                    })
                    .unwrap();
            }
            "submission-null" => {
                store
                    .update(id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["submissionOwner"] = Value::Null;
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
            }
            "native-journal" => {
                let journal = TargetJournal::open(&store).unwrap();
                let mut record = journal.read(&creator["allocationId"]).unwrap();
                record["nativeOwnerIdentity"] = json!(ProcessIdentity::current().unwrap());
                storage::atomic_json(
                    &store.home().join("target-dispositions").join(format!(
                        "{}.json",
                        creator["allocationId"].as_str().unwrap()
                    )),
                    &record,
                )
                .unwrap();
            }
            "explicit" => snapshot.integrity.explicit_target = true,
            "source" => {
                snapshot.tab["lastUserTextRaw"] = json!("PUBLIC different source");
                snapshot.tab["lastUserContentText"] = json!("PUBLIC different source");
            }
            "conversation" => snapshot.tab["url"] = json!("https://chatgpt.com/c/public-different"),
            "generating" => snapshot.tab["stopExists"] = json!(true),
            _ => snapshot.integrity.status = "mismatch".into(),
        }
        let before = store.stored_for_update(id).unwrap();
        let log = store.log(id).unwrap();
        let result = complete_recovered(&store, id, &saved, &snapshot, &token).await;
        assert!(result.is_err() || result.unwrap().is_none(), "{case}");
        assert_eq!(store.stored_for_update(id).unwrap(), before, "{case}");
        assert_eq!(store.log(id).unwrap(), log, "{case}");
    }
}
#[test]
fn legacy_completion_saves_full_outputs_before_model_session_and_finalisation() {
    let (home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let output = home.path().join("PUBLIC answer.md");
    let saved=store.update(id,|_|Ok(Some(json!({"options":sessions::merge_object(&saved["options"],&json!({"writeOutputPath":output}))})))).unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    let completed = commit_legacy(
        &store,
        id,
        &saved,
        &snapshot,
        &owned,
        &creator,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(store.model_runs(id).unwrap()[0]["status"], "completed");
    let capture: artifacts::CaptureReceipt =
        serde_json::from_value(completed["browser"]["capturedAnswer"].clone()).unwrap();
    assert_eq!(
        artifacts::verify_capture(&store, &capture).unwrap(),
        b"**PUBLIC answer**"
    );
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "GPT-6 Pro said:\n\n**PUBLIC answer**\n"
    );
    assert!(store.log(id).unwrap().contains("**PUBLIC answer**"));
    assert_eq!(completed["browser"]["runtime"], saved["browser"]["runtime"]);
    let journal = TargetJournal::open(&store).unwrap();
    let record = journal.read(&creator["allocationId"]).unwrap();
    journal.verify(&record).unwrap();
    assert!(record.get("nativeOwnerIdentity").is_none());
    assert_eq!(record["commit"]["kind"], "session");
    assert_eq!(
        record["commit"]["assistantContent"]["representation"],
        "markdown"
    );
    assert!(record["finalisedAt"].as_str().is_some());
    assert_eq!(record["disposition"]["absence"], "unknown");
}
#[test]
fn failed_required_output_keeps_the_original_session_and_never_finalises() {
    let (_home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let destination = store.directory(id).unwrap().join("meta.json");
    let saved=store.update(id,|_|Ok(Some(json!({"options":sessions::merge_object(&saved["options"],&json!({"writeOutputPath":destination}))})))).unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    let error = commit_legacy(
        &store,
        id,
        &saved,
        &snapshot,
        &owned,
        &creator,
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("cannot replace session storage"),
        "{error:#}"
    );
    assert_eq!(store.stored_for_update(id).unwrap(), saved);
    let record = TargetJournal::open(&store)
        .unwrap()
        .read(&creator["allocationId"])
        .unwrap();
    assert!(record.get("commit").is_none());
    assert!(record.get("finalisedAt").is_none());
    assert!(
        store
            .model_runs(id)
            .unwrap()
            .iter()
            .all(|run| run["status"] != "completed")
    );
}
#[test]
fn changed_owner_outputs_winning_model_and_cancellation_prevent_completion() {
    for case in ["owner", "outputs", "model", "cancel"] {
        let (_home, store, saved, snapshot, creator) = fixture();
        let id = saved["id"].as_str().unwrap();
        let owned = eligible(&saved, &snapshot).unwrap().unwrap();
        let token = CancellationToken::new();
        match case {
            "owner" => {
                store
                    .update(id, |current| {
                        let mut browser = current["browser"].clone();
                        browser["runtime"]["controllerPid"] = json!(std::process::id());
                        Ok(Some(json!({"browser":browser})))
                    })
                    .unwrap();
            }
            "outputs" => {
                store.update(id,|current|Ok(Some(json!({"options":sessions::merge_object(&current["options"],&json!({"writeOutputPath":"PUBLIC-changed.md"}))})))).unwrap();
            }
            "model" => {
                store
                    .update_model(
                        id,
                        "gpt-6-pro",
                        &json!({"status":"completed","usage":{"PUBLIC":"winner"}}),
                    )
                    .unwrap();
            }
            _ => token.cancel(),
        }
        let before = store.stored_for_update(id).unwrap();
        assert!(
            commit_legacy(&store, id, &saved, &snapshot, &owned, &creator, &token).is_err(),
            "{case}"
        );
        assert_eq!(store.stored_for_update(id).unwrap(), before, "{case}");
        assert!(
            TargetJournal::open(&store)
                .unwrap()
                .read(&creator["allocationId"])
                .unwrap()
                .get("commit")
                .is_none()
        );
        if case == "model" {
            assert_eq!(
                store.model_runs(id).unwrap()[0]["usage"]["PUBLIC"],
                "winner"
            );
        }
    }
}
#[test]
fn exact_saved_socket_target_turn_and_positive_completion_are_required() {
    for case in [
        "explicit",
        "mismatch",
        "source",
        "missing-hash",
        "new-target",
        "new-socket",
        "new-conversation",
        "streaming",
        "live-owner",
    ] {
        let (_home, _store, mut saved, mut snapshot, _creator) = fixture();
        match case {
            "explicit" => snapshot.integrity.explicit_target = true,
            "mismatch" => snapshot.integrity.status = "mismatch".into(),
            "source" => snapshot.tab["lastUserTextRaw"] = json!("PUBLIC changed"),
            "missing-hash" => {
                saved["browser"]["runtime"]
                    .as_object_mut()
                    .unwrap()
                    .remove("submittedPromptHash");
            }
            "new-target" => snapshot.tab["targetId"] = json!("PUBLIC-observation"),
            "new-socket" => {
                snapshot.endpoint.browser_ws_endpoint =
                    "ws://127.0.0.1:1/devtools/browser/PUBLIC-restarted".into()
            }
            "new-conversation" => snapshot.tab["url"] = json!("https://chatgpt.com/c/public-other"),
            "streaming" => snapshot.tab["stopExists"] = json!(true),
            _ => {
                let current = ProcessIdentity::current().unwrap();
                saved["browser"]["runtime"]["controllerPid"] = json!(current.pid);
                saved["browser"]["runtime"]["controllerProcessStartedAtMs"] =
                    json!(current.started_at_ms);
            }
        }
        if case == "source" {
            snapshot.tab["lastUserContentText"] = json!("PUBLIC changed");
        }
        assert!(eligible(&saved, &snapshot).unwrap().is_none(), "{case}");
    }
}
#[test]
fn legacy_controller_liveness_requires_valid_positive_absence_evidence() {
    assert!(!controllers_dead(&json!({})));
    assert!(!controllers_dead(&json!({"lifecycle":{"workerPid":null}})));
    assert!(!controllers_dead(
        &json!({"lifecycle":{"workerPid":u32::MAX,"workerProcessStartedAtMs":null}})
    ));
    assert!(controllers_dead(
        &json!({"lifecycle":{"workerPid":u32::MAX}})
    ));
    let current = ProcessIdentity::current().unwrap();
    assert!(!controllers_dead(
        &json!({"lifecycle":{"workerPid":current.pid,"workerProcessStartedAtMs":current.started_at_ms}})
    ));
    assert!(controllers_dead(
        &json!({"lifecycle":{"workerPid":current.pid,"workerProcessStartedAtMs":current.started_at_ms-1.0}})
    ));
}
#[test]
fn historical_completion_cannot_reinterpret_a_native_journal() {
    let (_home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let journal = TargetJournal::open(&store).unwrap();
    let mut record = journal.read(&creator["allocationId"]).unwrap();
    record["nativeOwnerIdentity"] = json!(ProcessIdentity::current().unwrap());
    storage::atomic_json(
        &store.home().join("target-dispositions").join(format!(
            "{}.json",
            creator["allocationId"].as_str().unwrap()
        )),
        &record,
    )
    .unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    assert!(
        commit_legacy(
            &store,
            id,
            &saved,
            &snapshot,
            &owned,
            &creator,
            &CancellationToken::new()
        )
        .is_err()
    );
    assert_eq!(store.stored_for_update(id).unwrap(), saved);
    assert!(
        journal
            .read(&creator["allocationId"])
            .unwrap()
            .get("commit")
            .is_none()
    );
}
#[tokio::test]
async fn historical_review_cross_store_live_lease_prevents_completion() {
    let (home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let profile = ProfileRegistry::open(&home.path().join("PUBLIC-shared-profile")).unwrap();
    let other_store = SessionStore::new(home.path().join("PUBLIC-other-store"));
    let other = other_store
        .create(
            json!({"prompt":"PUBLIC other controller","model":"gpt-6-pro"}),
            home.path(),
            Some("PUBLIC-other"),
        )
        .unwrap();
    let lease = profile
        .acquire(
            other["id"].as_str().unwrap(),
            Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    lease.update(&json!({"chromeHost":snapshot.endpoint.host,"chromePort":snapshot.endpoint.port,"chromeTargetId":snapshot.tab["targetId"],"targetDispositionPending":true})).unwrap();
    let saved = store
        .update(id, |current| {
            let mut browser = current["browser"].clone();
            browser["runtime"]["userDataDir"] = json!(profile.path());
            Ok(Some(json!({"browser":browser})))
        })
        .unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    let leases = profile.records().unwrap();
    assert!(
        commit_legacy(
            &store,
            id,
            &saved,
            &snapshot,
            &owned,
            &creator,
            &CancellationToken::new()
        )
        .is_err()
    );
    assert_eq!(store.stored_for_update(id).unwrap(), saved);
    assert_eq!(profile.records().unwrap(), leases);
    assert!(
        TargetJournal::open(&store)
            .unwrap()
            .read(&creator["allocationId"])
            .unwrap()
            .get("commit")
            .is_none()
    );
}
#[test]
fn historical_review_relative_artifacts_use_the_saved_working_directory() {
    let (home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let source = home.path().join("downloads/PUBLIC-report.txt");
    std::fs::create_dir(source.parent().unwrap()).unwrap();
    let bytes = b"PUBLIC relative artifact";
    std::fs::write(&source, bytes).unwrap();
    let destination = home.path().join("exports/PUBLIC answer.md");
    let saved=store.update(id,|current|Ok(Some(json!({"options":sessions::merge_object(&current["options"],&json!({"writeOutputPath":destination,"writeArtifacts":true})),"artifacts":[{"kind":"file","path":"downloads/PUBLIC-report.txt","filename":"PUBLIC-report.txt","sizeBytes":bytes.len(),"sha256":artifacts::sha256(bytes)}]})))).unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    let completed = commit_legacy(
        &store,
        id,
        &saved,
        &snapshot,
        &owned,
        &creator,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(
        std::fs::read(home.path().join("exports/public-report.txt")).unwrap(),
        bytes
    );
    let journal = TargetJournal::open(&store).unwrap();
    let record = journal.read(&creator["allocationId"]).unwrap();
    journal.verify(&record).unwrap();
    assert!(
        record["commit"]["requiredOutputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|receipt| receipt["file"]
                .as_str()
                .is_some_and(|file| std::path::Path::new(file).canonicalize().ok()
                    == source.canonicalize().ok()))
    );
}

#[test]
fn completed_root_retry_finalises_without_rewriting_saved_outputs() {
    let (home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let output = home.path().join("PUBLIC retry.md");
    let saved=store.update(id,|_|Ok(Some(json!({"options":sessions::merge_object(&saved["options"],&json!({"writeOutputPath":output}))})))).unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    let completed = commit_legacy(
        &store,
        id,
        &saved,
        &snapshot,
        &owned,
        &creator,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    let journal = TargetJournal::open(&store).unwrap();
    let mut record = journal.read(&creator["allocationId"]).unwrap();
    record.as_object_mut().unwrap().remove("commit");
    record.as_object_mut().unwrap().remove("finalisedAt");
    storage::atomic_json(
        &store.home().join("target-dispositions").join(format!(
            "{}.json",
            creator["allocationId"].as_str().unwrap()
        )),
        &record,
    )
    .unwrap();
    let log = store.log(id).unwrap();
    let model = store.model_runs(id).unwrap();
    let output_time = std::fs::metadata(&output).unwrap().modified().unwrap();
    assert!(eligible(&completed, &snapshot).unwrap().is_some());
    let retried = commit_legacy(
        &store,
        id,
        &completed,
        &snapshot,
        &owned,
        &creator,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(retried, completed);
    assert_eq!(store.stored_for_update(id).unwrap(), completed);
    assert_eq!(store.model_runs(id).unwrap(), model);
    assert_eq!(store.log(id).unwrap(), log);
    assert_eq!(
        std::fs::metadata(output).unwrap().modified().unwrap(),
        output_time
    );
    let record = journal.read(&creator["allocationId"]).unwrap();
    journal.verify(&record).unwrap();
    assert!(record["finalisedAt"].is_string());
}

#[test]
fn completion_retry_requires_the_durable_root_and_unchanged_capture() {
    for case in [
        "root-pending",
        "corrupt-capture",
        "changed-answer",
        "partial",
        "cancelled",
    ] {
        let (_home, store, saved, mut snapshot, creator) = fixture();
        let id = saved["id"].as_str().unwrap();
        let owned = eligible(&saved, &snapshot).unwrap().unwrap();
        let completed = commit_legacy(
            &store,
            id,
            &saved,
            &snapshot,
            &owned,
            &creator,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let journal = TargetJournal::open(&store).unwrap();
        let record = journal.read(&creator["allocationId"]).unwrap();
        match case {
            "root-pending" => {
                storage::atomic_json(&store.directory(id).unwrap().join("meta.json"), &saved)
                    .unwrap();
            }
            "corrupt-capture" => {
                std::fs::write(
                    store.directory(id).unwrap().join(
                        completed["browser"]["capturedAnswer"]["file"]
                            .as_str()
                            .unwrap(),
                    ),
                    "PUBLIC corrupted",
                )
                .unwrap();
            }
            "changed-answer" => {
                snapshot.tab["lastAssistantMarkdown"] = json!("PUBLIC different answer")
            }
            _ => {
                store
                    .update(id, |_| Ok(Some(json!({"status":case}))))
                    .unwrap();
            }
        }
        let before = store.stored_for_update(id).unwrap();
        assert!(
            commit_legacy(
                &store,
                id,
                &completed,
                &snapshot,
                &owned,
                &creator,
                &CancellationToken::new()
            )
            .is_err(),
            "{case}"
        );
        assert_eq!(store.stored_for_update(id).unwrap(), before, "{case}");
        assert_eq!(
            journal.read(&creator["allocationId"]).unwrap(),
            record,
            "{case}"
        );
    }
}

#[test]
fn recycled_controller_pid_does_not_block_the_original_dead_owner() {
    let (_home, store, saved, snapshot, mut creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let current = ProcessIdentity::current().unwrap();
    creator["controllerPid"] = json!(current.pid);
    creator["controllerStartedAtMs"] = json!(current.started_at_ms - 1.0);
    let journal = TargetJournal::open(&store).unwrap();
    let mut record = journal.read(&creator["allocationId"]).unwrap();
    record["creator"] = creator.clone();
    storage::atomic_json(
        &store.home().join("target-dispositions").join(format!(
            "{}.json",
            creator["allocationId"].as_str().unwrap()
        )),
        &record,
    )
    .unwrap();
    let saved = store
        .update(id, |current| {
            let mut browser = current["browser"].clone();
            browser["runtime"]["controllerPid"] = creator["controllerPid"].clone();
            browser["runtime"]["controllerProcessStartedAtMs"] =
                creator["controllerStartedAtMs"].clone();
            browser["runtime"]["ownedRecoveryTarget"]["creator"] = creator.clone();
            Ok(Some(json!({"browser":browser})))
        })
        .unwrap();
    let owned = eligible(&saved, &snapshot).unwrap().unwrap();
    assert!(
        commit_legacy(
            &store,
            id,
            &saved,
            &snapshot,
            &owned,
            &creator,
            &CancellationToken::new()
        )
        .unwrap()
        .is_some()
    );
}
/// A legacy creator completion retries the idle launcher hinted by its lease profile,
/// even when its own browser cleanup could not connect. Malformed saved authority
/// proves the retry ran; it refuses before any browser command.
#[tokio::test]
async fn legacy_creator_completion_retries_the_hinted_idle_launcher() {
    let (home, store, saved, snapshot, creator) = fixture();
    let id = saved["id"].as_str().unwrap();
    let profile = home.path().join("PUBLIC-browser-profile");
    std::fs::create_dir(&profile).unwrap();
    let mut leased = creator.clone();
    leased["lease"] = json!({"profileDir":profile});
    let record = store.home().join("target-dispositions").join(format!(
        "{}.json",
        creator["allocationId"].as_str().unwrap()
    ));
    let mut journal: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    journal["creator"] = leased.clone();
    storage::atomic_json(&record, &journal).unwrap();
    store
        .update(id, |current| {
            let mut browser = current["browser"].clone();
            browser["runtime"]["ownedRecoveryTarget"]["creator"] = leased.clone();
            Ok(Some(json!({"browser":browser})))
        })
        .unwrap();
    let owner = store
        .create(json!({"prompt":"PUBLIC idle launcher"}), home.path(), None)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    store
        .update(&owner, |_| {
            Ok(Some(json!({"status":"completed","recoveryRequired":false,
            "browser":{"runtime":{"persistentBrowserOwner":{"version":1,"kind":"PUBLIC-malformed"},"targetDispositionPending":false}}})))
        })
        .unwrap();
    crate::browser_profile::ProfileRegistry::open_existing(&profile)
        .unwrap()
        .set_idle_owner(&owner)
        .unwrap();
    let completed = complete_owned(&store, id, &snapshot, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.session["status"], "completed");
    let runtime = &store.stored_for_update(&owner).unwrap()["browser"]["runtime"];
    let disposal = &runtime["persistentBrowserDisposal"];
    assert_eq!(disposal["disposition"], "retained", "{runtime}");
}

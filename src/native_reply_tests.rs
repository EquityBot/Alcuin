use super::*;
use std::fs;

fn request() -> Request {
    Request::parse(&json!({"chatId":"PUBLIC-conversation","label":"PUBLIC-label-1234"})).unwrap()
}
fn completed() -> Value {
    json!({"url":"https://chatgpt.com/c/PUBLIC-conversation","conversationId":"PUBLIC-conversation",
        "authenticated":true,"stopExists":false,"state":"completed","completionConfirmed":true,
        "assistantCount":1,"assistantFollowsLatestUser":true,"lastUserMessageId":"PUBLIC-user",
        "lastUserText":"ORACLE_REQUEST_LABEL: PUBLIC-label-1234 PUBLIC question",
        "lastUserTextRaw":"ORACLE_REQUEST_LABEL: PUBLIC-label-1234\r\nPUBLIC question",
        "lastAssistantMessageId":"PUBLIC-assistant","lastAssistantTurnId":"PUBLIC-turn",
        "lastAssistantText":"PUBLIC full answer 😀","lastAssistantMarkdown":"**PUBLIC full answer 😀**"})
}

#[test]
fn request_validation_preserves_exact_turn_ids_and_rejects_null_or_oversized_inputs() {
    for invalid in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"chatId":"a/b","label":"PUBLIC-label-1234"}),
        json!({"chatId":"a","label":"short"}),
        json!({"chatId":"a","label":"-PUBLIC-label-1234"}),
        json!({"chatId":"a","label":"PUBLIC-label-1234","turnId":null}),
        json!({"chatId":"a","label":"PUBLIC-label-1234","turnId":"\u{feff}\t"}),
        json!({"chatId":"a","label":"PUBLIC-label-1234","turnId":"😀".repeat(51)}),
        json!({"chatId":"a".repeat(101),"label":"PUBLIC-label-1234"}),
        json!({"chatId":"a","label":"a".repeat(129)}),
    ] {
        assert!(Request::parse(&invalid).is_err(), "{invalid}");
    }
    let raw = json!({"chatId":"a","label":"PUBLIC-label-1234","turnId":" 😀 ","extra":true});
    assert_eq!(
        Request::parse(&raw).unwrap().turn_id.as_deref(),
        Some(" 😀 ")
    );
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("PUBLIC-request.json");
    fs::write(&file, format!("\u{feff}{raw}")).unwrap();
    assert_eq!(read_request(&file).unwrap(), Request::parse(&raw).unwrap());
    fs::write(&file, vec![b' '; LIMIT as usize + 1]).unwrap();
    assert_eq!(
        read_request(&file).unwrap_err().to_string(),
        "Native reply request exceeds 32 KiB."
    );
    fs::write(&file, b"PUBLIC invalid JSON").unwrap();
    assert_eq!(
        read_request(&file).unwrap_err().to_string(),
        "Native reply request could not be read as JSON."
    );
    assert_eq!(
        read_request(home.path()).unwrap_err().to_string(),
        "Native reply request could not be read."
    );
    #[cfg(unix)]
    {
        let link = home.path().join("PUBLIC-link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_request(&link).is_err());
    }
}

#[test]
fn labelled_reply_requires_exact_origin_pairing_completion_and_one_structured_marker() {
    let request = request();
    let tab = completed();
    assert_eq!(
        validate_reply(&request, &tab).unwrap(),
        "**PUBLIC full answer 😀**"
    );
    for (field, value) in [
        (
            "url",
            json!("https://chatgpt.com.example.com/c/PUBLIC-conversation"),
        ),
        (
            "url",
            json!("https://name@chatgpt.com/c/PUBLIC-conversation"),
        ),
        (
            "url",
            json!("https://chat.openai.com/c/PUBLIC-conversation"),
        ),
        (
            "url",
            json!("https://chatgpt.com:444/c/PUBLIC-conversation"),
        ),
        ("url", json!("https://chatgpt.com/c/PUBLIC-other")),
        ("conversationId", json!("PUBLIC-other")),
        ("authenticated", json!(false)),
        ("stopExists", json!(true)),
        ("state", json!("running")),
        ("completionConfirmed", json!(false)),
        ("assistantCount", json!(0)),
        ("assistantFollowsLatestUser", json!(false)),
        (
            "lastUserTextRaw",
            json!("Quote: ORACLE_REQUEST_LABEL: PUBLIC-label-1234"),
        ),
        (
            "lastUserTextRaw",
            json!("ORACLE_REQUEST_LABEL: PUBLIC-label-1234-suffix\nPUBLIC question"),
        ),
        (
            "lastUserTextRaw",
            json!("ORACLE_REQUEST_LABEL: PUBLIC-label-1234\nORACLE_REQUEST_LABEL: duplicate"),
        ),
        (
            "lastUserTextRaw",
            json!(" ORACLE_REQUEST_LABEL: PUBLIC-label-1234"),
        ),
        ("lastAssistantText", json!("Pro thinking Answer now")),
        (
            "lastAssistantText",
            json!("ChatGPT said: File upload request Pro thinking Answer now"),
        ),
        ("lastAssistantText", json!("Thought for 3 seconds Edit")),
    ] {
        let mut bad = tab.clone();
        bad[field] = value;
        assert!(
            validate_reply(&request, &bad).is_err(),
            "{field}: {}",
            bad[field]
        );
    }
    let mut old = request;
    old.turn_id = Some("PUBLIC-user".into());
    let mut quoted = tab.clone();
    quoted["lastUserTextRaw"] = json!("PUBLIC exact turn with quoted PUBLIC-label-1234");
    assert!(validate_reply(&old, &quoted).is_ok());
    quoted["lastUserMessageId"] = json!("PUBLIC-other-user");
    assert!(validate_reply(&old, &quoted).is_err());
    assert!(!answer_placeholder(
        "PUBLIC answer now is explained in this answer"
    ));
}

#[test]
fn publication_is_exclusive_private_exact_and_preserves_a_cancelled_output() {
    let home = tempfile::tempdir().unwrap();
    let output = home.path().join("PUBLIC-answer.md");
    let token = CancellationToken::new();
    let text = "PUBLIC exact 😀\r\n\n**answer**\n";
    let receipt = output_receipt(text, &output, &token).unwrap();
    assert_eq!(fs::read(&output).unwrap(), text.as_bytes());
    assert_eq!(receipt["bytes"], text.len());
    assert_eq!(receipt["sha256"], artifacts::sha256(text.as_bytes()));
    let linked = NativeEntry::open(&output, false).unwrap();
    let (dev, ino) = NativeIdentity::legacy_stat(&linked.handle).unwrap();
    assert_eq!(receipt["dev"], dev);
    assert_eq!(receipt["ino"], ino);
    assert_eq!(
        output_receipt("replacement", &output, &token)
            .unwrap_err()
            .to_string(),
        "The answer output already exists; no answer was written."
    );
    token.cancel();
    assert!(output_receipt("replacement", &output, &token).is_err());
    assert_eq!(fs::read_to_string(&output).unwrap(), text);
    let missing = home.path().join("PUBLIC-missing");
    assert!(
        output_receipt(
            "PUBLIC answer",
            &missing.join("answer"),
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(!missing.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let symlink = home.path().join("PUBLIC-answer-link");
        std::os::unix::fs::symlink(&output, &symlink).unwrap();
        assert!(output_receipt("replacement", &symlink, &CancellationToken::new()).is_err());
        assert_eq!(fs::read_to_string(&output).unwrap(), text);
    }
}

#[tokio::test]
async fn standalone_admission_is_not_a_consultation_and_requires_its_exact_bound_receipt() {
    use crate::browser_disposition::TargetCreation;
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join("sessions"));
    let profile = ProfileRegistry::open(&home.path().join("admission")).unwrap();
    let lease = profile
        .acquire(
            "native-reply-PUBLIC",
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let creation = TargetCreation::allocated("PUBLIC-target");
    lease.update(&json!({"chromeHost":"127.0.0.1","chromePort":9222,"chromeTargetId":"PUBLIC-target","targetDispositionPending":true,"targetDispositionAllocationId":creation.allocation_id()})).unwrap();
    let journal = TargetJournal::open(&store).unwrap();
    let creator = journal.record_creation(&creation, &json!({"host":"127.0.0.1","port":9222,"targetId":"PUBLIC-target","claimId":uuid::Uuid::new_v4().to_string()}), &ProcessIdentity::current().unwrap(), None, Some(json!({"profileDir":profile.path(),"id":lease.id(),"admissionId":"native-reply-PUBLIC"}))).unwrap();
    assert!(store.list(true).unwrap().is_empty());
    crate::browser_retirement::require_controllers(
        &store,
        &creator,
        std::time::Instant::now() + Duration::from_secs(1),
        true,
    )
    .unwrap();
    let disposed = journal
        .record_disposition(&creator, "confirmed-absent", "PUBLIC exact absence")
        .unwrap();
    let mut changed = creator.clone();
    changed["lease"]["admissionId"] = json!("native-reply-other");
    assert!(
        profile
            .release_disposed(
                &journal,
                &changed,
                &disposed,
                std::time::Instant::now() + Duration::from_secs(1),
                true
            )
            .is_err()
    );
    assert_eq!(
        profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        profile
            .release_disposed(
                &journal,
                &creator,
                &disposed,
                std::time::Instant::now() + Duration::from_secs(1),
                true
            )
            .unwrap()
    );
    assert!(
        profile.records().unwrap()["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(store.list(true).unwrap().is_empty());
}

#[derive(Clone, Copy)]
enum CaptureFault {
    None,
    Pending,
    Hydrating,
    LostAfterObservation,
    Duplicate,
    WrongLabel,
    ClaimDenied,
}
struct BrowserFixture {
    endpoint: AttachmentEndpoint,
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl BrowserFixture {
    async fn start(created: bool, fault: CaptureFault) -> Self {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        let token = stop.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            let observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! { biased;
                    _ = token.cancelled() => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let requests = seen.clone();
                        let stop = token.clone();
                        let observations = observations.clone();
                        clients.spawn(async move {
                            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                            loop {
                                let frame = tokio::select! { biased; _ = stop.cancelled() => break, frame = socket.next() => frame };
                                let Some(Ok(Message::Text(frame))) = frame else { break; };
                                let request: Value = serde_json::from_str(&frame).unwrap();
                                requests.lock().unwrap().push(request.clone());
                                let method = request["method"].as_str().unwrap();
                                let result = match method {
                                    "Target.getTargets" => {
                                        let mut rows = vec![json!({"type":"page","targetId":"PUBLIC-other","url":"https://chatgpt.com/c/PUBLIC-other"})];
                                        if !created {
                                            rows.push(json!({"type":"page","targetId":"PUBLIC-target","url":"https://chatgpt.com/c/PUBLIC-conversation"}));
                                            if matches!(fault, CaptureFault::Duplicate) { rows.push(json!({"type":"page","targetId":"PUBLIC-duplicate","url":"https://chatgpt.com/c/PUBLIC-conversation"})); }
                                        }
                                        json!({"targetInfos":rows})
                                    },
                                    "Target.createTarget" => json!({"targetId":"PUBLIC-created"}),
                                    "Target.attachToTarget" => json!({"sessionId":"PUBLIC-attachment"}),
                                    "Target.getTargetInfo" => json!({"targetInfo":{"type":"page","targetId":"PUBLIC-created","url":"about:blank"}}),
                                    "Runtime.enable" | "DOM.enable" | "Page.enable" | "Page.navigate" => json!({}),
                                    "Runtime.evaluate" => {
                                        let source = request["params"]["expression"].as_str().unwrap();
                                        let value = if source.contains("industrious-dom:historical-inspection") {
                                            let index = observations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            if matches!(fault, CaptureFault::Hydrating) && index == 0 { json!({"identityChanged":true}) }
                                            else if matches!(fault, CaptureFault::LostAfterObservation) && index >= 2 {
                                                socket.send(Message::Text(json!({"id":request["id"],"error":{"message":"PUBLIC target disappeared"}}).to_string().into())).await.unwrap();
                                                continue;
                                            } else {
                                                json!({"url":"https://chatgpt.com/c/PUBLIC-conversation","authenticated":true,"lastUserText":"ORACLE_REQUEST_LABEL: PUBLIC-label-1234 PUBLIC question",
                                                    "lastUserTextRaw":if matches!(fault, CaptureFault::WrongLabel) {"PUBLIC wrong label"} else {"ORACLE_REQUEST_LABEL: PUBLIC-label-1234\nPUBLIC question"},
                                                    "lastUserMessageId":"PUBLIC-user","lastUserTurnIndex":0,"lastAssistantTurnIndex":1,
                                                    "lastAssistantText":"PUBLIC full answer 😀","assistantCount":1,"assistantFollowsLatestUser":true,"stopExists":matches!(fault, CaptureFault::Pending),"strongThinkingActive":false,"completionVisible":true,
                                                    "assistantSnapshot":{"text":"PUBLIC full answer 😀","turnIndex":1,"messageId":"PUBLIC-assistant","turnId":"PUBLIC-turn"}})
                                            }
                                        } else if source.contains("industrious-dom:historical-copy-markdown") {
                                            json!({"prepared":true,"success":true,"markdown":"**PUBLIC full answer 😀**"})
                                        } else if source.contains("industrious-dom:native-reply-claim") { json!(!matches!(fault, CaptureFault::ClaimDenied)) }
                                        else { panic!("Unexpected renderer expression: {source}") };
                                        json!({"result":{"value":value}})
                                    },
                                    method => panic!("Unexpected native export command: {method}"),
                                };
                                socket.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).await.unwrap();
                            }
                        });
                    }
                }
            }
            while let Some(result) = clients.join_next().await {
                result.unwrap();
            }
        });
        Self {
            endpoint: AttachmentEndpoint {
                host: "127.0.0.1".into(),
                port,
                browser_ws_endpoint: format!(
                    "ws://127.0.0.1:{port}/devtools/browser/PUBLIC-native-export"
                ),
            },
            requests,
            stop,
            task,
        }
    }
    async fn finish(self) -> Vec<Value> {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap();
        self.requests.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn borrowing_never_claims_navigates_creates_or_retires_and_duplicate_or_wrong_labels_write_nothing()
 {
    for fault in [
        CaptureFault::None,
        CaptureFault::Duplicate,
        CaptureFault::WrongLabel,
    ] {
        let fixture = BrowserFixture::start(false, fault).await;
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("untouched-session-store"));
        let token = CancellationToken::new();
        let browser = fixture.endpoint.connect(COMMAND, &token).await.unwrap();
        let output = home.path().join("PUBLIC-answer.md");
        let result = recover_connected(
            &store,
            &fixture.endpoint,
            &browser,
            &request(),
            &output,
            &token,
        )
        .await;
        if matches!(fault, CaptureFault::None) {
            assert_eq!(
                result.unwrap(),
                "**PUBLIC full answer 😀**".encode_utf16().count()
            );
            assert_eq!(
                fs::read_to_string(&output).unwrap(),
                "**PUBLIC full answer 😀**"
            );
        } else {
            assert!(result.is_err());
            assert!(!output.exists());
        }
        assert!(!store.home().exists());
        browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        let requests = fixture.finish().await;
        assert!(requests.iter().all(|r| !matches!(
            r["method"].as_str(),
            Some("Target.createTarget" | "Page.navigate" | "Target.closeTarget")
        )));
        assert!(
            requests.iter().all(|r| !r["params"]["expression"]
                .as_str()
                .is_some_and(|s| s.contains("native-reply-claim")
                    || s.contains("industrious-dom:submission")))
        );
    }
}

#[tokio::test]
async fn initial_created_hydration_retries_but_target_loss_and_pending_deadlines_fail_closed() {
    let startup = tempfile::tempdir().unwrap();
    crate::browser_chrome::TEST_STARTUP_LOCK
        .scope(
            startup.path().join("startup.lock"),
            verify_initial_hydration_deadlines(),
        )
        .await;
}
async fn verify_initial_hydration_deadlines() {
    for fault in [
        CaptureFault::Hydrating,
        CaptureFault::LostAfterObservation,
        CaptureFault::Pending,
    ] {
        let fixture = BrowserFixture::start(false, fault).await;
        let token = CancellationToken::new();
        let target =
            json!({"targetId":"PUBLIC-target","url":"https://chatgpt.com/c/PUBLIC-conversation"});
        let result = harvest(
            &fixture.endpoint,
            &target,
            true,
            &token,
            if matches!(fault, CaptureFault::Pending) {
                Duration::from_millis(650)
            } else {
                Duration::from_secs(5)
            },
        )
        .await;
        match fault {
            CaptureFault::Hydrating => {
                assert_eq!(
                    validate_reply(&request(), &result.unwrap()).unwrap(),
                    "**PUBLIC full answer 😀**"
                );
            }
            CaptureFault::LostAfterObservation => assert_eq!(
                result.unwrap_err().to_string(),
                "Unable to read the saved conversation."
            ),
            CaptureFault::Pending => assert_eq!(result.unwrap_err().to_string(), TIMEOUT),
            _ => unreachable!(),
        }
        let requests = fixture.finish().await;
        assert!(requests.iter().all(|r| !matches!(
            r["method"].as_str(),
            Some("Target.createTarget" | "Page.navigate" | "Target.closeTarget")
        )));
    }
}

#[tokio::test]
async fn unverified_created_claim_retains_its_real_allocation_and_still_saves_full_answer() {
    let fixture = BrowserFixture::start(true, CaptureFault::ClaimDenied).await;
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join("sessions"));
    let token = CancellationToken::new();
    let browser = fixture.endpoint.connect(COMMAND, &token).await.unwrap();
    let output = home.path().join("PUBLIC-answer.md");
    recover_connected(
        &store,
        &fixture.endpoint,
        &browser,
        &request(),
        &output,
        &token,
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        "**PUBLIC full answer 😀**"
    );
    assert!(store.list(true).unwrap().is_empty());
    let files: Vec<_> = fs::read_dir(store.home().join("target-dispositions"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    assert_eq!(files.len(), 1);
    let record: Value = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(record["creator"]["target"]["targetId"], "PUBLIC-created");
    assert!(record["creator"].get("sessionId").is_none());
    assert_eq!(record["disposition"]["reason"], "creator-unverified");
    assert!(record.get("commit").is_none());
    browser
        .disconnect_and_wait(Duration::from_secs(1))
        .await
        .unwrap();
    let requests = fixture.finish().await;
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "Target.createTarget")
            .count(),
        1
    );
    assert!(requests.iter().all(|r| r["method"] != "Target.closeTarget"));
}

/// A successful native reply retries the idle launcher its Chrome profile hints at.
/// Malformed saved authority proves the retry ran; it refuses before any browser command.
#[tokio::test]
async fn successful_reply_retries_the_hinted_idle_launcher() {
    let fixture = BrowserFixture::start(false, CaptureFault::None).await;
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join("sessions"));
    let profile = home.path().join("PUBLIC-browser-profile");
    fs::create_dir(&profile).unwrap();
    fs::write(
        profile.join("DevToolsActivePort"),
        format!(
            "{}\n/devtools/browser/PUBLIC-native-export\n",
            fixture.endpoint.port
        ),
    )
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
    let request_path = home.path().join("PUBLIC-request.json");
    fs::write(
        &request_path,
        json!({"chatId":"PUBLIC-conversation","label":"PUBLIC-label-1234"}).to_string(),
    )
    .unwrap();
    let output = home.path().join("PUBLIC-answer.md");
    let mut environment = Environment::new();
    environment.insert(
        "ORACLE_BROWSER_PROFILE_DIR".into(),
        profile.to_string_lossy().into(),
    );
    recover(
        &store,
        &environment,
        home.path(),
        &request_path,
        &output,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        "**PUBLIC full answer 😀**"
    );
    let disposal = &store.stored_for_update(&owner).unwrap()["browser"]["runtime"]["persistentBrowserDisposal"];
    assert_eq!(disposal["disposition"], "retained", "{disposal}");
    // The reply's attachment marker was released, so it cannot block later cleanup.
    crate::browser_profile::ProfileRegistry::open_existing(&profile)
        .unwrap()
        .empty_disposal_lock(std::time::Instant::now() + Duration::from_secs(1))
        .unwrap()
        .release()
        .unwrap();
    let requests = fixture.finish().await;
    assert!(requests.iter().all(|r| r["method"] != "Target.closeTarget"));
}

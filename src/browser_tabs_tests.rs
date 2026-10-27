use super::*;

fn endpoint() -> AttachmentEndpoint {
    AttachmentEndpoint {
        host: "127.0.0.1".into(),
        port: 9222,
        browser_ws_endpoint: "ws://127.0.0.1:9222/devtools/browser/PUBLIC-fixture".into(),
    }
}
fn target() -> Value {
    json!({"targetId":"PUBLIC-target","url":"https://chatgpt.com/c/public-a","title":"PUBLIC historical tab","type":"page"})
}
fn info() -> Value {
    json!({"url":"https://chatgpt.com/c/public-a","authenticated":true,"lastAssistantText":"PUBLIC rendered answer","assistantCount":1,"assistantFollowsLatestUser":true,"lastAssistantTurnIndex":1,"lastUserTurnIndex":0,"lastUserText":"PUBLIC prompt","lastUserMessageId":"PUBLIC-user","lastUserOriginalSourceText":"PUBLIC exact source\n  indented","lastUserOriginalSourceMessageId":"PUBLIC-user","stopExists":false,"strongThinkingActive":false,"completionVisible":true,"assistantSnapshot":{"text":"PUBLIC exact\n\nanswer","turnIndex":1,"messageId":"PUBLIC-assistant","turnId":"PUBLIC-turn"}})
}
#[test]
fn snapshots_must_match_the_inspected_turn_before_ids_or_completion_are_used() {
    let mut raw = info();
    let matched = summary(&endpoint(), &target(), &raw).unwrap();
    assert_eq!(matched["lastAssistantText"], "PUBLIC exact\n\nanswer");
    assert_eq!(matched["lastAssistantMessageId"], "PUBLIC-assistant");
    assert_eq!(matched["completionVisible"], true);
    assert_eq!(matched["completionConfirmed"], false);
    raw["assistantSnapshot"]["turnIndex"] = json!(0);
    let mismatched = summary(&endpoint(), &target(), &raw).unwrap();
    assert_eq!(mismatched["lastAssistantText"], "PUBLIC rendered answer");
    assert!(mismatched.get("lastAssistantMessageId").is_none());
    assert_eq!(mismatched["completionVisible"], false);
    raw["lastAssistantTurnIndex"] = json!(-1);
    raw["assistantSnapshot"]["turnIndex"] = Value::Null;
    raw["assistantSnapshot"]["text"] = json!("  public   RENDERED answer ");
    assert_eq!(
        summary(&endpoint(), &target(), &raw).unwrap()["completionVisible"],
        true
    );
    raw["identityChanged"] = json!(true);
    assert!(summary(&endpoint(), &target(), &raw).is_err());
    raw["identityChanged"] = json!(false);
    raw["url"] = json!("https://chatgpt.com.example.com/c/public-a");
    assert!(summary(&endpoint(), &target(), &raw).is_err());
}
#[test]
fn legacy_missing_hash_null_hash_and_exact_original_source_have_distinct_rules() {
    let mut tab = summary(&endpoint(), &target(), &info()).unwrap();
    tab["completionConfirmed"] = json!(true);
    assert!(ready_for_session(&json!({}), &tab, false).unwrap());
    assert!(
        ready_for_session(
            &json!({"browser":{"runtime":{"submittedPromptHash":null}}}),
            &tab,
            false
        )
        .is_err()
    );
    assert!(
        ready_for_session(
            &json!({"browser":{"runtime":{"submittedPromptHash":null}}}),
            &tab,
            true
        )
        .unwrap()
    );
    let exact = prompt_fingerprint("PUBLIC exact source\n  indented", "PUBLIC-user");
    assert!(matches_submitted_prompt(
        &tab,
        Some(&json!(exact)),
        Some(&json!("original-source-v1"))
    ));
    assert!(!matches_submitted_prompt(
        &tab,
        Some(&json!(exact)),
        Some(&json!("unknown-format"))
    ));
    tab["lastUserOriginalSourceText"] = json!("PUBLIC exact source indented");
    assert!(!matches_submitted_prompt(
        &tab,
        Some(&json!(exact)),
        Some(&json!("original-source-v1"))
    ));
    tab["lastUserTextRaw"] = json!("PUBLIC prompt status");
    tab["lastUserContentText"] = json!("PUBLIC prompt");
    for source in ["PUBLIC prompt status", "PUBLIC prompt"] {
        assert!(matches_submitted_prompt(
            &tab,
            Some(&json!(prompt_fingerprint(source, "PUBLIC-user"))),
            None
        ));
    }
    tab["lastUserMessageId"] = json!(" ");
    assert!(!matches_submitted_prompt(
        &tab,
        Some(&json!(prompt_fingerprint("PUBLIC prompt", "PUBLIC-user"))),
        None
    ));
    tab["completionConfirmed"] = json!(false);
    assert!(!ready_for_session(&json!({}), &tab, false).unwrap());
    assert!(ready_for_session(&json!({}), &tab, true).unwrap());
    tab["assistantFollowsLatestUser"] = json!(false);
    assert!(!ready_for_session(&json!({}), &tab, true).unwrap());
}
#[test]
fn positive_completion_is_stable_and_bound_to_user_and_assistant_identity() {
    let mut tab = summary(&endpoint(), &target(), &info()).unwrap();
    let mut gate = HarvestCompletionGate::default();
    for now in [0, 400, 800] {
        gate.observe(&mut tab, now, 3, 1200);
        assert_eq!(tab["completionConfirmed"], false);
    }
    gate.observe(&mut tab, 1200, 3, 1200);
    assert_eq!(tab["state"], "completed");
    tab["lastUserOriginalSourceMessageId"] = json!("PUBLIC-other-user");
    gate.observe(&mut tab, 1600, 3, 1200);
    assert_eq!(tab["completionConfirmed"], false);
    tab["completionVisible"] = json!(false);
    gate.observe(&mut tab, 10_000, 3, 1200);
    assert_eq!(tab["completionConfirmed"], false);
    tab["completionVisible"] = json!(true);
    tab["strongThinkingActive"] = json!(true);
    gate.observe(&mut tab, 20_000, 3, 1200);
    assert_eq!(tab["completionConfirmed"], false);
    tab["authenticated"] = json!(false);
    gate.observe(&mut tab, 21_000, 3, 1200);
    assert_eq!(tab["state"], "detached");
}
#[test]
fn exact_references_precede_titles_and_ambiguous_titles_refuse_selection() {
    let first = json!({"targetId":"PUBLIC-a","url":"https://chatgpt.com/c/public-a","conversationId":"public-a","title":"PUBLIC identical title"});
    let second = json!({"targetId":"PUBLIC-b","url":"https://chatgpt.com/c/public-b","conversationId":"public-b","title":"PUBLIC identical title"});
    let tabs = [first.clone(), second.clone()];
    assert_eq!(resolve(&tabs, None).unwrap(), first);
    for reference in ["PUBLIC-b", "https://chatgpt.com/c/public-b", "public-b"] {
        assert_eq!(resolve(&tabs, Some(reference)).unwrap(), second);
    }
    assert!(resolve(&tabs, Some("identical")).is_err());
    assert!(resolve(&tabs, Some("missing")).is_err());
}
#[test]
fn copy_capture_identity_includes_original_source_and_paired_assistant_evidence() {
    let tab = summary(&endpoint(), &target(), &info()).unwrap();
    for key in [
        "url",
        "lastUserOriginalSourceText",
        "lastUserOriginalSourceMessageId",
        "lastUserMessageId",
        "assistantFollowsLatestUser",
        "lastAssistantMessageId",
        "lastAssistantText",
        "stopExists",
        "strongThinkingActive",
    ] {
        let mut changed = tab.clone();
        changed[key] = json!("PUBLIC changed");
        assert_ne!(
            capture_identity(&tab, true),
            capture_identity(&changed, true),
            "{key}"
        );
    }
    let mut changed = tab.clone();
    changed["lastAssistantText"] = json!("PUBLIC changed");
    assert_eq!(
        capture_identity(&tab, false),
        capture_identity(&changed, false)
    );
}
#[tokio::test]
async fn strict_missing_target_reads_metadata_and_never_attaches_to_other_tabs() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut count = 0;
        while let Some(Ok(Message::Text(raw))) = socket.next().await {
            let request: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(request["method"], "Target.getTargets");
            count += 1;
            socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":{"targetInfos":[target()]}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        count
    });
    let endpoint = AttachmentEndpoint {
        host: "127.0.0.1".into(),
        port,
        browser_ws_endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-fixture"),
    };
    for reference in [Some("missing-saved-conversation"), None, Some("current")] {
        let result = resolve_target(
            &endpoint,
            reference,
            true,
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("refusing unrelated")
        );
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap(),
        1
    );
}
#[test]
fn page_identity_checks_accept_project_conversation_routes() {
    // Reference 15c8b9a: a project conversation (/g/g-p-…/project/c/<id>) keeps its identity.
    for script in [
        INSPECT,
        COPY,
        include_str!("../browser-scripts/copy-markdown.js"),
    ] {
        assert!(script.contains(r"pathname.match(/(?:^|\/)c\/([a-zA-Z0-9-]+)/)"));
        assert!(!script.contains(r"pathname.match(/^\/c\/([a-zA-Z0-9-]+)/)"));
    }
}

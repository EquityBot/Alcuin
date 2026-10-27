use industrious::browser::{self, RecoveryTarget, TerminalGate, TerminalSample};
use serde_json::{Value, json};
fn evidence(format: Option<&str>) -> (Value, Value) {
    let source = "  PUBLIC complete source\r\n```rust\r\nfn public() {}\r\n```\r\n";
    let mut runtime = json!({"conversationId":"conversation-public","tabUrl":"https://chatgpt.com/c/conversation-public","submittedPromptHash":browser::prompt_fingerprint(source,"user-public")});
    if let Some(format) = format {
        runtime["submittedPromptHashFormat"] = json!(format);
    }
    let observed = json!({"currentUrl":"https://chatgpt.com/c/conversation-public","conversationId":"conversation-public","assistantFollowsLatestUser":true,"lastUserMessageId":"user-public","lastAssistantMessageId":"assistant-public","lastUserOriginalSourceMessageId":"user-public","lastUserOriginalSourceText":source,"lastUserTextRaw":source,"lastUserTurnIndex":3,"lastAssistantTurnIndex":4,"lastAssistantTurnId":"turn-public","completionConfirmed":true,"stopExists":false,"strongThinkingActive":false});
    (json!({"browser":{"runtime":runtime}}), observed)
}
#[test]
fn conversation_urls_require_provider_origin_and_complete_allowed_identifiers() {
    for raw in [
        "https://chatgpt.com/c/public-id?key=public",
        "https://chat.openai.com/c/public-id#public",
        "https://chatgpt.com/g/g-p-public/c/public-id",
        "https://chatgpt.com/g/g-p-public/project/c/public-id",
    ] {
        assert_eq!(
            browser::conversation_id(raw).as_deref(),
            Some("public-id"),
            "{raw}"
        );
    }
    for raw in [
        "https://foreign.example/c/public-id",
        "http://chatgpt.com/c/public-id",
        "https://chatgpt.com.foreign.example/c/public-id",
        "https://user@chatgpt.com/c/public-id",
        "https://chatgpt.com:444/c/public-id",
        "https://chatgpt.com/c/WEB:public",
        "https://chatgpt.com/c/WEB%3apublic",
        "https://chatgpt.com/c/public%2Fid",
        "https://chatgpt.com/c/public-id/extra",
        "https://foreign.example/?url=https://chatgpt.com/c/public-id",
        "/c/public-id",
        "https://chatgpt.com/foreign/c/public-id",
    ] {
        assert!(browser::conversation_id(raw).is_none(), "{raw}");
    }
    assert!(!browser::stable_conversation_id(&"x".repeat(257)));
}
#[test]
fn completed_load_error_labels_are_refused_without_rejecting_explanations() {
    let selection =
        serde_json::from_str::<Vec<Value>>(include_str!("../fixtures/browser-completion.json"))
            .unwrap()
            .into_iter()
            .find(|case| case["accepted"] == true)
            .unwrap()["input"]
            .clone();
    let (meta, mut observed) = evidence(Some("original-source-v1"));
    for text in [
        "This response couldn’t load",
        "THIS RESPONSE COULDN'T LOAD! Retry",
        "\u{feff}This response couldn’t load.\nRetry\u{a0}",
    ] {
        observed["text"] = json!(text);
        assert!(
            browser::accept_answer(&meta, &observed, &selection).is_err(),
            "{text}"
        );
    }
    for text in [
        "PUBLIC complete answer",
        "The label ‘This response couldn’t load’ means the response failed.",
        "This response couldn’t load. Here is an explanation.",
        "This response couldn't load..",
    ] {
        observed["text"] = json!(text);
        browser::accept_answer(&meta, &observed, &selection).unwrap();
    }
    // Reference chatgpt-response-unloaded: the label is refused even beside an owned image.
    observed["text"] = json!("This response couldn’t load");
    observed["images"] = json!([{"url":"PUBLIC owned image"}]);
    assert!(browser::accept_answer(&meta, &observed, &selection).is_err());
}
#[test]
fn original_source_v1_refuses_rendered_fallback_truncation_and_wrong_message_or_turn() {
    let (meta, observed) = evidence(Some("original-source-v1"));
    assert!(browser::require_original_source_pair(&meta, &observed).is_ok());
    for (field, replacement) in [
        (
            "currentUrl",
            json!("https://foreign.example/c/conversation-public"),
        ),
        ("conversationId", json!("foreign-conversation")),
        ("lastUserOriginalSourceMessageId", json!("foreign-user")),
        (
            "lastUserOriginalSourceText",
            json!("  PUBLIC complete source"),
        ),
        ("lastAssistantMessageId", json!("user-public")),
        ("lastAssistantTurnIndex", json!(3)),
        ("assistantFollowsLatestUser", json!(false)),
        ("lastAssistantTurnId", json!("")),
    ] {
        let mut changed = observed.clone();
        changed[field] = replacement;
        assert!(
            browser::require_original_source_pair(&meta, &changed).is_err(),
            "{field}"
        );
    }
    let mut changed = observed.clone();
    changed
        .as_object_mut()
        .unwrap()
        .remove("lastUserOriginalSourceText");
    assert!(browser::require_original_source_pair(&meta, &changed).is_err());
    let mut changed = meta.clone();
    changed["browser"]["runtime"]["submittedPromptHashFormat"] = json!("unknown-format");
    assert!(browser::require_original_source_pair(&changed, &observed).is_err());
}
#[test]
fn legacy_fingerprints_remain_readable_and_preserve_exact_json_array_format() {
    let (meta, observed) = evidence(None);
    assert!(browser::require_original_source_pair(&meta, &observed).is_ok());
    let mut source_only = observed.clone();
    source_only
        .as_object_mut()
        .unwrap()
        .remove("lastUserTextRaw");
    assert!(browser::require_original_source_pair(&meta, &source_only).is_err());
    assert_eq!(
        browser::prompt_fingerprint("one\r\ntwo", "public-message"),
        browser::prompt_fingerprint("one\ntwo", "public-message")
    );
    assert_ne!(
        browser::prompt_fingerprint("One\ntwo", "public-message"),
        browser::prompt_fingerprint("one\ntwo", "public-message")
    );
    assert_ne!(
        browser::prompt_fingerprint("  source", "public-message"),
        browser::prompt_fingerprint("source", "public-message")
    );
    assert_eq!(
        browser::prompt_fingerprint("one\ntwo", "public-message"),
        industrious::artifacts::sha256(br#"["public-message","one\ntwo"]"#)
    );
}
#[test]
fn a_real_assistant_turn_preserves_source_pairing_when_the_message_id_is_absent() {
    let (meta, mut observed) = evidence(Some("original-source-v1"));
    observed
        .as_object_mut()
        .unwrap()
        .remove("lastAssistantMessageId");
    browser::require_original_source_pair(&meta, &observed).unwrap();
    for (field, value) in [
        ("lastAssistantTurnId", json!("")),
        ("lastAssistantTurnIndex", json!(2)),
        ("lastAssistantMessageId", json!(123)),
        ("lastAssistantMessageId", json!("user-public")),
        (
            "lastUserOriginalSourceText",
            json!("PUBLIC different source"),
        ),
        ("assistantFollowsLatestUser", json!(false)),
    ] {
        let mut changed = observed.clone();
        changed[field] = value;
        assert!(
            browser::require_original_source_pair(&meta, &changed).is_err(),
            "{field}"
        );
    }
}
#[test]
fn maintained_shared_exchange_requires_positive_order_source_and_exact_exchange_identity() {
    let (meta, mut observed) = evidence(Some("original-source-v1"));
    observed["lastAssistantTurnIndex"] = observed["lastUserTurnIndex"].clone();
    assert!(browser::require_original_source_pair(&meta, &observed).is_err());
    observed["sameTurnExchange"] = json!(true);
    observed["lastUserTurnId"] = observed["lastAssistantTurnId"].clone();
    browser::require_original_source_pair(&meta, &observed).unwrap();
    for (field, value) in [
        ("assistantFollowsLatestUser", json!(false)),
        ("lastUserTurnId", json!("foreign-turn")),
        ("sameTurnExchange", json!(false)),
        ("lastUserOriginalSourceText", json!("wrong source")),
    ] {
        let mut changed = observed.clone();
        changed[field] = value;
        assert!(
            browser::require_original_source_pair(&meta, &changed).is_err(),
            "{field}"
        );
    }
}
#[test]
fn owned_recovery_claim_requires_exact_endpoint_target_and_provider_identity() {
    let (mut meta, _) = evidence(Some("original-source-v1"));
    let target = RecoveryTarget {
        host: "localhost".into(),
        port: 9222,
        target_id: "TARGET-PUBLIC".into(),
        browser_ws_endpoint: Some("ws://127.0.0.1:9222/devtools/browser/public".into()),
        claim_id: "claim-public".into(),
        conversation_id: "conversation-public".into(),
    };
    meta["browser"]["runtime"]["ownedRecoveryTarget"] = json!({"host":"127.0.0.1","port":9222,"targetId":target.target_id,"browserWSEndpoint":target.browser_ws_endpoint,"claimId":target.claim_id,"conversationId":target.conversation_id});
    assert!(browser::exact_owned_target(&meta, &target));
    for field in [
        "host",
        "port",
        "targetId",
        "browserWSEndpoint",
        "claimId",
        "conversationId",
    ] {
        let mut changed = meta.clone();
        changed["browser"]["runtime"]["ownedRecoveryTarget"][field] = json!("foreign");
        assert!(!browser::exact_owned_target(&changed, &target), "{field}");
    }
    meta["browser"]["runtime"]["tabUrl"] = json!("https://foreign.example/c/conversation-public");
    assert!(!browser::exact_owned_target(&meta, &target));
}
#[test]
fn terminal_acceptance_requires_positive_controls_and_resets_on_same_length_rewrites() {
    let mut gate = TerminalGate::default();
    let mut sample = TerminalSample {
        now_ms: 0,
        length: 32,
        content_key: "turn-one:PUBLIC preamble".into(),
        stop_visible: false,
        bar_visible: false,
        strong_thinking_active: false,
    };
    assert!(!gate.observe(&sample, 3, 1200));
    sample.now_ms = 100000;
    assert!(!gate.observe(&sample, 3, 1200));
    sample.bar_visible = true;
    for n in 1..=3 {
        sample.now_ms += 500;
        let accepted = gate.observe(&sample, 3, 1200);
        assert_eq!(accepted, n == 3);
    }
    sample.content_key = "turn-one:rewritten text".into();
    assert!(!gate.observe(&sample, 3, 1200));
    sample.now_ms += 5000;
    sample.strong_thinking_active = true;
    assert!(!gate.observe(&sample, 3, 1200));
    sample.strong_thinking_active = false;
    assert!(!gate.observe(&sample, 3, 1200));
}

#[test]
fn a_recovery_claim_cannot_mix_provider_origins_or_duplicate_route_segments() {
    let (meta, mut observed) = evidence(Some("original-source-v1"));
    observed["origin"] = json!("https://chat.openai.com");
    assert!(browser::require_original_source_pair(&meta, &observed).is_err());
    observed.as_object_mut().unwrap().remove("origin");
    observed["currentUrl"] = json!("https://chat.openai.com/c/conversation-public");
    assert!(browser::require_original_source_pair(&meta, &observed).is_err());
    assert!(browser::conversation_id("https://chatgpt.com/c//public-id").is_none());
}

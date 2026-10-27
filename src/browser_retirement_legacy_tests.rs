use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

#[tokio::test]
async fn legacy_observation_uses_the_committed_fingerprint_without_a_native_user_receipt() {
    for case in [
        "message",
        "turn",
        "original",
        "raw-fallback",
        "changed-source",
        "unsupported-format",
        "native-session",
        "active-thinking",
        "missing-completion",
    ] {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta = store
            .create(
                json!({"prompt":"PUBLIC legacy source","model":"gpt-6-pro"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap();
        if case == "native-session" {
            store.update(id,|_|Ok(Some(json!({"browser":{"runtime":{"userCommitReceipt":{"messageId":"PUBLIC-user"}}}})))).unwrap();
        }
        let mut record = json!({"creator":{"target":{"claimId":"PUBLIC-claim"}},"commit":{"kind":"session","sessionId":id,"conversationId":"public-conversation","assistantMessageId":if case=="turn"{"PUBLIC-turn"}else{"PUBLIC-assistant"},"submittedPromptHash":browser::prompt_fingerprint("PUBLIC legacy source","PUBLIC-user")}});
        if matches!(case, "original" | "changed-source") {
            record["commit"]["submittedPromptHashFormat"] = json!("original-source-v1");
        }
        if case == "unsupported-format" {
            record["commit"]["submittedPromptHashFormat"] = json!("PUBLIC-unknown-format");
        }
        if case == "native-session" {
            record["nativeOwnerIdentity"] = json!(ProcessIdentity::current().unwrap());
        }
        let (client, server) = tokio::io::duplex(128 * 1024);
        let cdp = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let worker = tokio::spawn(async move {
            let mut requests = vec![];
            while let Some(Ok(Message::Text(frame))) = peer.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                let mut observed = json!({"conversationId":"public-conversation","stopExists":false,"strongThinkingActive":false,"completionVisible":true,"assistantFollowsLatestUser":true,"lastAssistantMessageId":"PUBLIC-assistant","lastAssistantTurnId":"PUBLIC-turn","lastUserMessageId":"PUBLIC-user","lastUserTextRaw":"PUBLIC legacy source","lastUserText":"PUBLIC legacy source","lastUserOriginalSourceMessageId":"PUBLIC-user","lastUserOriginalSourceText":"PUBLIC legacy source","text":"PUBLIC answer"});
                if case == "active-thinking" {
                    observed["strongThinkingActive"] = json!(true);
                }
                if case == "missing-completion" {
                    observed["completionVisible"] = json!(false);
                }
                if case == "turn" {
                    observed
                        .as_object_mut()
                        .unwrap()
                        .remove("lastAssistantMessageId");
                }
                if case == "raw-fallback" {
                    observed.as_object_mut().unwrap().remove("lastUserTextRaw");
                }
                if case == "changed-source" {
                    observed["lastUserOriginalSourceText"] = json!("PUBLIC changed source");
                }
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":{"result":{"value":observed}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
                requests.push(request);
            }
            requests
        });
        let result = observe(
            &store,
            &record,
            &cdp,
            None,
            Instant::now() + Duration::from_secs(2),
        )
        .await;
        cdp.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        let requests = worker.await.unwrap();
        assert_eq!(
            result.is_ok(),
            !matches!(
                case,
                "changed-source" | "unsupported-format" | "active-thinking" | "missing-completion"
            ),
            "{case}: {result:?}"
        );
        if case == "unsupported-format" {
            assert!(requests.is_empty());
            continue;
        }
        let source = requests[0]["params"]["expression"].as_str().unwrap();
        assert!(
            source.contains(if case == "native-session" {
                "\"legacyDisposition\":false"
            } else {
                "\"legacyDisposition\":true"
            }),
            "Disposition adapter selected the wrong authority mode for {case}"
        );
        if case != "native-session" {
            assert!(
                source.contains("\"messageId\":null"),
                "Legacy observation invented a native user receipt"
            );
        }
    }
}

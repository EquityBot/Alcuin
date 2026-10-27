//! Synthetic HTTP boundaries, with no real credentials or provider requests.
use alcuin::providers::{
    ApiTransport, FailureReason, HttpProvider, ProviderError, ResponseObserver, Route, Secret,
    chat_request, chat_response, text_output,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Observed {
    snapshots: Mutex<Vec<Value>>,
    deltas: Mutex<String>,
}
impl ResponseObserver for Observed {
    fn response(&self, response: &Value) -> anyhow::Result<()> {
        self.snapshots.lock().unwrap().push(response.clone());
        Ok(())
    }
    fn delta(&self, text: &str) -> anyhow::Result<()> {
        self.deltas.lock().unwrap().push_str(text);
        Ok(())
    }
}
async fn peer(
    body: String,
    truncate: bool,
    delay: bool,
) -> (HttpProvider, Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(vec![]));
    let recording = captured.clone();
    let handle = tokio::spawn(async move {
        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = accepted.0;
        let mut request = vec![];
        let mut buffer = [0; 2048];
        loop {
            let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|length| length.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        *recording.lock().unwrap() = request;
        let length = body.len() + if truncate { 100 } else { 0 };
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nx-request-id: req_public\r\nConnection: close\r\n\r\n"
        );
        if socket.write_all(header.as_bytes()).await.is_ok() {
            for chunk in body.as_bytes().chunks(7) {
                if socket.write_all(chunk).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        if delay {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    let provider = HttpProvider::new(
        Route {
            model: "gpt-6-astra".into(),
            provider_label: "synthetic".into(),
            base_url: format!("http://{address}/v1"),
            key_source: "PUBLIC_SYNTHETIC".into(),
            azure_deployment_name: None,
            transport: ApiTransport::Responses,
        },
        Secret::new("PUBLIC-SYNTHETIC-KEY".into()),
        Duration::from_secs(5),
    )
    .unwrap();
    (provider, captured, handle)
}
fn stream() -> String {
    [json!({"type":"response.created","response":{"id":"resp_public","status":"in_progress","output":[]}}),
     json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_public","content":[]}}),
     json!({"type":"response.content_part.added","output_index":0,"item_id":"msg_public","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
     json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_public","content_index":0,"delta":"PUBLIC café 🦀"})]
    .into_iter().map(|event|format!("data: {event}\r\n\r\n")).collect()
}
#[tokio::test]
async fn native_foreground_sse_preserves_partial_identity_and_deltas_without_post_replay() {
    for truncate in [false, true] {
        let (provider, captured, handle) = peer(stream(), truncate, false).await;
        let observed = Observed::default();
        let result = provider
            .create_observed(
                &json!({"background":false,"input":[],"model":"gpt-6-astra"}),
                &CancellationToken::new(),
                &observed,
            )
            .await;
        handle.await.unwrap();
        assert_eq!(*observed.deltas.lock().unwrap(), "PUBLIC café 🦀");
        assert_eq!(
            observed.snapshots.lock().unwrap().last().unwrap()["id"],
            "resp_public"
        );
        assert_eq!(
            observed.snapshots.lock().unwrap().last().unwrap()["_request_id"],
            "req_public"
        );
        if truncate {
            assert_eq!(
                result
                    .unwrap_err()
                    .downcast_ref::<ProviderError>()
                    .unwrap()
                    .reason,
                FailureReason::ConnectionLost
            );
        } else {
            assert_eq!(result.unwrap()["status"], "in_progress");
        }
        let request = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(request.starts_with("POST /v1/responses "));
        assert!(request.contains("\"stream\":true"));
        assert_eq!(request.matches("POST ").count(), 1);
    }
}
#[tokio::test]
async fn cancellation_closes_the_stream_and_retains_observed_evidence() {
    let (provider, _, handle) = peer(stream(), true, true).await;
    let observed = Observed::default();
    let cancel = CancellationToken::new();
    let cancellation = cancel.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
    });
    let result = provider
        .create_observed(&json!({"background":false,"input":[]}), &cancel, &observed)
        .await;
    timer.await.unwrap();
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    assert_eq!(
        result
            .unwrap_err()
            .downcast_ref::<ProviderError>()
            .unwrap()
            .reason,
        FailureReason::ClientAbort
    );
    assert_eq!(*observed.deltas.lock().unwrap(), "PUBLIC café 🦀");
}
#[test]
fn gateway_maps_the_reference_contract_and_refuses_reasoning_mode() {
    let body = json!({"model":"openai/gpt-6-astra","instructions":"PUBLIC system","input":[{"role":"user","content":[{"type":"input_text","text":"PUBLIC one"},{"type":"input_text","text":"two"}]}],"reasoning":{"effort":"xhigh"},"max_output_tokens":100,"tools":[{"type":"web_search"}]});
    assert_eq!(
        chat_request(&body).unwrap(),
        json!({"model":"openai/gpt-6-astra","messages":[{"role":"system","content":"PUBLIC system"},{"role":"user","content":"PUBLIC one\n\ntwo"}],"reasoning_effort":"xhigh","max_tokens":100})
    );
    let mut forbidden = body;
    forbidden["reasoning"]["mode"] = json!("pro");
    assert!(chat_request(&forbidden).is_err());
    let response = chat_response(
        &json!({"id":"chat_public","choices":[{"message":{"content":"PUBLIC answer"}}],"usage":{"prompt_tokens":17,"completion_tokens":23,"total_tokens":40,"completion_tokens_details":{"reasoning_tokens":11}}}),
    );
    assert_eq!(response["usage"]["reasoning_tokens"], 11);
    assert_eq!(text_output(&response), "PUBLIC answer");
}
#[test]
fn empty_output_text_falls_back_and_content_arrays_precede_item_text() {
    assert_eq!(
        text_output(&json!({"output_text":"ignored string","output":[{"text":"PUBLIC fallback"}]})),
        "PUBLIC fallback"
    );
    assert_eq!(
        text_output(
            &json!({"output_text":[],"output":[{"text":"ignored","content":[{"type":"output_text","text":"PUBLIC one"},{"type":"text","text":""}]},{"text":"two"}]})
        ),
        "PUBLIC one\ntwo"
    );
    assert_eq!(
        text_output(&json!({"output_text":["PUBLIC direct"],"output":[{"text":"ignored"}]})),
        "PUBLIC direct"
    );
}
#[test]
fn known_model_overrides_preserve_source_defaults_validation_and_usage_cost() {
    use alcuin::providers::{build_request, model_settings, usage_summary};
    let options = json!({"modelOverrides":{"gpt-6-astra":{"apiModel":" gpt-6-astra ","reasoning":{"effort":"high"},"inputLimit":1000,"pricing":{"inputPerToken":0.00002,"outputPerToken":0.00006}}}});
    let settings = model_settings("gpt-6-astra", &options).unwrap();
    assert_eq!(settings.input_limit, 1000);
    assert_eq!(
        build_request("gpt-6-astra", &options, "PUBLIC prompt", "PUBLIC system").unwrap()["reasoning"]
            ["effort"],
        "high"
    );
    let usage = usage_summary(
        &json!({"usage":{"output_tokens":23,"output_tokens_details":{"reasoning_tokens":11}}}),
        17,
        settings.pricing,
    );
    assert_eq!(usage["inputTokens"], 17);
    assert_eq!(usage["totalTokens"], 40);
    assert_eq!(usage["reasoningTokens"], 11);
    assert_eq!(
        usage["cost"].as_f64().unwrap(),
        17.0 * 0.00002 + 23.0 * 0.00006
    );
    let cleared = json!({"modelOverrides":{"gpt-6-astra":{"reasoning":null,"pricing":null,"inputLimit":0.5}}});
    assert_eq!(
        model_settings("gpt-6-astra", &cleared).unwrap().input_limit,
        272000
    );
    assert!(
        build_request("gpt-6-astra", &cleared, "PUBLIC", "PUBLIC").unwrap()["reasoning"]
            .get("effort")
            .is_none()
    );
    assert!(usage_summary(&json!({}), 17, None).get("cost").is_none());
    let unsupported = json!({"modelOverrides":{"gpt-6-astra":{"reasoning":{"effort":"none"}}}});
    assert!(build_request("gpt-6-astra", &unsupported, "PUBLIC", "PUBLIC").is_err());
    let explicit = json!({"reasoningEffort":"high","modelOverrides":unsupported["modelOverrides"]});
    assert!(build_request("gpt-6-astra", &explicit, "PUBLIC", "PUBLIC").is_ok());
    assert!(
        model_settings(
            "gpt-6-astra",
            &json!({"modelOverrides":{"gpt-6-astra":{"apiModel":"gpt-5.5-pro"}}})
        )
        .is_err()
    );
}

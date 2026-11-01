//! API run parity with the reference runOracle and OpenAI client: synthetic providers and local
//! stubs only, with no real credentials or provider requests.
use alcuin::{
    config::Environment,
    execution,
    process::ProcessIdentity,
    providers::{
        self, ApiTransport, FailureReason, HttpProvider, ProviderError, ResponseObserver,
        ResponseProvider, Route, Secret,
    },
    sessions::SessionStore,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "PUBLIC synthetic consultation prompt for parity";
fn route(transport: ApiTransport, base: &str) -> Route {
    Route {
        model: "gpt-6-astra".into(),
        provider_label: "synthetic".into(),
        base_url: base.into(),
        key_source: "PUBLIC_SYNTHETIC".into(),
        azure_deployment_name: None,
        transport,
    }
}
fn resolve(input: Value, config: Value, env: &Environment) -> Result<execution::ResolvedRequest> {
    let home = tempfile::tempdir().unwrap();
    let mut input = input;
    input["engine"] = json!("api");
    execution::resolve(&input, &config, env, home.path())
}
fn prepare(input: Value, env: &Environment) -> Result<Value> {
    let home = tempfile::tempdir().unwrap();
    let mut input = input;
    input["engine"] = json!("api");
    let request = execution::resolve(&input, &json!({}), env, home.path())?;
    execution::prepare(
        &SessionStore::new(home.path()),
        &request,
        home.path(),
        route(ApiTransport::Responses, "https://synthetic.example/v1"),
    )
}

/// Creates with `created` (or a provider error) and answers reads from `reads` in order.
struct Scripted {
    route: Route,
    created: Mutex<Option<Result<Value>>>,
    reads: Mutex<Vec<Result<Value>>>,
}
impl ResponseProvider for Scripted {
    fn route(&self) -> &Route {
        &self.route
    }
    async fn create(&self, _: &Value, cancel: &CancellationToken) -> Result<Value> {
        let created = self.created.lock().unwrap().take();
        match created {
            Some(result) => result,
            None => {
                cancel.cancelled().await;
                anyhow::bail!("PUBLIC cancelled")
            }
        }
    }
    async fn retrieve(&self, _: &str, _: &CancellationToken) -> Result<Value> {
        let mut reads = self.reads.lock().unwrap();
        if reads.len() > 1 {
            reads.remove(0)
        } else {
            match &reads[0] {
                Ok(value) => Ok(value.clone()),
                Err(error) => Err(anyhow::anyhow!("{error}")),
            }
        }
    }
}
fn lost() -> Result<Value> {
    Err(ProviderError {
        reason: FailureReason::ConnectionLost,
        message: "PUBLIC raw transport text".into(),
        request_id: None,
        status: None,
    }
    .into())
}
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);
impl ResponseObserver for Recorder {
    fn response(&self, _: &Value) -> Result<()> {
        Ok(())
    }
    fn delta(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn log(&self, line: &str) -> Result<()> {
        self.0.lock().unwrap().push(line.to_owned());
        Ok(())
    }
}
async fn run(
    extra: Value,
    created: Option<Result<Value>>,
    reads: Vec<Result<Value>>,
) -> (Result<Value>, Vec<String>, Value) {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let mut input = json!({"prompt":PROMPT,"engine":"api"});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let request = execution::resolve(&input, &json!({}), &Environment::new(), home.path()).unwrap();
    let route = route(ApiTransport::Responses, "https://synthetic.example/v1");
    let session = execution::prepare(&store, &request, home.path(), route.clone()).unwrap();
    let id = session["id"].as_str().unwrap().to_owned();
    let identity = ProcessIdentity::current().unwrap();
    execution::start_owner(&store, &id, &identity, false).unwrap();
    let provider = Scripted {
        route,
        created: Mutex::new(created),
        reads: Mutex::new(reads),
    };
    let recorder = Recorder::default();
    let result = execution::execute_logged(
        &store,
        &id,
        &provider,
        &identity,
        &CancellationToken::new(),
        Some(&recorder),
    )
    .await;
    let lines = recorder.0.lock().unwrap().clone();
    (result, lines, store.require(&id).unwrap())
}
fn answer() -> Value {
    json!({"id":"resp_public","status":"completed","model":"gpt-6-astra","output_text":["PUBLIC answer"],"reasoning":{"mode":"pro"}})
}

#[tokio::test(start_paused = true)]
async fn background_retries_describe_the_failure_and_report_the_reconnect() {
    let (result, lines, _) = run(
        json!({}),
        Some(Ok(json!({"id":"resp_public","status":"queued"}))),
        vec![lost(), lost(), Ok(answer())],
    )
    .await;
    result.unwrap();
    let notices: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("Retrying") || line.starts_with("Reconnected"))
        .collect();
    assert_eq!(
        notices,
        [
            "Connection to OpenAI ended unexpectedly before the response completed. Retrying in 3s...",
            "Connection to OpenAI ended unexpectedly before the response completed. Retrying in 6s...",
            "Reconnected to API background response (status=completed). API is still working...",
        ],
        "{lines:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn failed_dispatch_logs_the_description_and_saves_the_transport_reason() {
    let unsupported = ProviderError {
        reason: FailureReason::UnsupportedEndpoint,
        message: "PUBLIC raw".into(),
        request_id: None,
        status: Some(404),
    };
    let (result, lines, meta) = run(json!({}), Some(Err(unsupported.into())), vec![]).await;
    assert!(result.is_err());
    assert!(lines.contains(&"The Responses API returned 404/405 \u{2014} your base URL/gateway probably lacks /v1/responses (check OPENAI_BASE_URL or switch to api.openai.com / browser engine).".into()), "{lines:?}");
    assert_eq!(meta["transport"], json!({"reason":"unsupported-endpoint"}));

    let (result, _, meta) = run(
        json!({}),
        Some(Ok(json!({"id":"resp_public","status":"in_progress"}))),
        vec![Ok(
            json!({"id":"resp_public","status":"failed","error":{"message":"PUBLIC failure"}}),
        )],
    )
    .await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Response did not complete: PUBLIC failure"
    );
    assert_eq!(meta["response"]["status"], "failed");
    assert_eq!(meta["response"]["responseId"], "resp_public");

    // The overall deadline: reference wording, logged as a client-side timeout.
    let (result, lines, meta) = run(json!({"timeoutSeconds":60}), None, vec![]).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Timed out waiting for API response after 1m 0s."
    );
    assert!(lines.contains(
        &"Client-side timeout: OpenAI streaming call exceeded the 1m 0s deadline.".into()
    ));
    assert_eq!(meta["transport"]["reason"], "client-timeout");
}

#[tokio::test(start_paused = true)]
async fn foreground_grace_poll_announces_itself_and_ends_the_run_after_three_minutes() {
    let pending = json!({"id":"resp_public","status":"in_progress"});
    let (result, lines, _) = run(
        json!({"background":false}),
        Some(Ok(pending.clone())),
        vec![Ok(pending)],
    )
    .await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Response did not complete: in_progress"
    );
    let at = |text: &str| lines.iter().position(|line| line == text);
    let announced = at("Response still in_progress; polling until completion...").unwrap();
    assert!(at("API ended the run early (status=in_progress).").unwrap() > announced);
    let (result, lines, _) = run(
        json!({"background":false}),
        Some(Ok(
            json!({"id":"resp_public","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}),
        )),
        vec![],
    )
    .await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Response did not complete: max_output_tokens"
    );
    assert!(lines.contains(
        &"API ended the run early (status=incomplete, reason=max_output_tokens).".into()
    ));
}

#[test]
fn prompt_budget_effort_and_gateway_guards_use_the_reference_texts() {
    let short = json!({"prompt":"ABCDE"});
    assert_eq!(
        prepare(short.clone(), &Environment::new())
            .unwrap_err()
            .to_string(),
        "Prompt is too short (<10 chars). This was likely accidental; please provide more detail."
    );
    for value in ["3", " 4 chars", "abc"] {
        let env = Environment::from([("ORACLE_MIN_PROMPT_CHARS".into(), value.into())]);
        prepare(short.clone(), &env).unwrap();
    }
    let env = Environment::from([("ORACLE_MIN_PROMPT_CHARS".into(), "6".into())]);
    assert!(
        prepare(short, &env)
            .unwrap_err()
            .to_string()
            .contains("(<6 chars)")
    );
    let error = prepare(json!({"prompt":PROMPT,"maxInput":1}), &Environment::new()).unwrap_err();
    assert!(error.is::<execution::InputTooLarge>());
    let text = error.to_string();
    assert!(
        text.starts_with("Input too large (") && text.ends_with(" tokens). Limit is 1 tokens."),
        "{text}"
    );
    assert_eq!(
        providers::build_request(
            "gpt-6-astra",
            &json!({"modelOverrides":{"gpt-6-astra":{"reasoning":{"effort":"none"}}}}),
            "PUBLIC",
            "PUBLIC"
        )
        .unwrap_err()
        .to_string(),
        "Reasoning effort \"none\" is not supported for gpt-6-astra. Expected low, medium, high, xhigh, or max."
    );
    let body = providers::build_request("gpt-6-astra", &json!({}), "PUBLIC", "PUBLIC").unwrap();
    assert_eq!(
        body["reasoning"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["effort", "mode"]
    );
    let home = tempfile::tempdir().unwrap();
    let request = resolve(json!({"prompt":PROMPT}), json!({}), &Environment::new()).unwrap();
    assert_eq!(
        execution::prepare(
            &SessionStore::new(home.path()),
            &request,
            home.path(),
            route(ApiTransport::ChatCompletions, "https://gateway.example/v1"),
        )
        .unwrap_err()
        .to_string(),
        "--reasoning-mode requires the OpenAI or Azure OpenAI Responses API; OpenRouter and custom --base-url routes use the Chat Completions adapter."
    );
}

#[test]
fn file_size_limits_follow_the_reference_order_and_labels() {
    let env =
        |value: &str| Environment::from([("ORACLE_MAX_FILE_SIZE_BYTES".into(), value.into())]);
    let input = json!({"prompt":PROMPT});
    assert_eq!(
        resolve(
            input.clone(),
            json!({"maxFileSizeBytes":-1}),
            &Environment::new()
        )
        .err()
        .unwrap()
        .to_string(),
        "config.maxFileSizeBytes must be a positive integer number of bytes."
    );
    assert_eq!(
        resolve(input.clone(), json!({}), &env("0"))
            .err()
            .unwrap()
            .to_string(),
        "ORACLE_MAX_FILE_SIZE_BYTES must be a positive integer number of bytes."
    );
    let limit = |request: execution::ResolvedRequest| request.options["maxFileSizeBytes"].clone();
    assert_eq!(
        limit(resolve(input.clone(), json!({}), &env("  2048 ")).unwrap()),
        2048
    );
    assert_eq!(
        limit(
            resolve(
                json!({"prompt":PROMPT,"maxFileSizeBytes":4096}),
                json!({}),
                &env("2048")
            )
            .unwrap()
        ),
        4096
    );
}

#[test]
fn route_logging_and_custom_base_classification_follow_the_reference() {
    assert_eq!(providers::route_target("  not a url  "), "not a url");
    assert_eq!(
        providers::route_target("https://exa mple.test/v1"),
        "exa mple.test/v1"
    );
    let long = format!("{}{}", "x".repeat(60), "PUBLIC-tail");
    assert_eq!(
        providers::route_target(&long),
        format!("{}\u{2026}LIC-tail", "x".repeat(32))
    );
    let (route, _) = providers::resolve_route(
        "openai/gpt-6-astra",
        &json!({"baseUrl":"not a url"}),
        &Environment::new(),
    )
    .unwrap();
    assert_eq!(route.base_url, "https://openrouter.ai/api/v1");
}

/// Serves `replies` in order, one per connection, and records each request line.
fn stub(replies: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let record = seen.clone();
    std::thread::spawn(move || {
        for (status, kind, body) in replies {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut request = vec![];
            let mut buffer = [0; 4096];
            while let Ok(count) = socket.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&request);
            record
                .lock()
                .unwrap()
                .push(text.lines().next().unwrap_or("").to_owned());
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 {status} PUBLIC\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = socket.write_all(body.as_bytes());
        }
    });
    (address, seen)
}
fn http(base: &str) -> HttpProvider {
    HttpProvider::new(
        route(ApiTransport::Responses, base),
        Secret::new("PUBLIC-SYNTHETIC-KEY".into()),
        Duration::from_secs(10),
    )
    .unwrap()
}
const JSON: &str = "application/json";

#[tokio::test]
async fn retrieve_retries_transient_statuses_and_create_never_replays() {
    let done = json!({"id":"resp_public","status":"completed"}).to_string();
    let (base, seen) = stub(vec![(503, JSON, String::new()), (200, JSON, done)]);
    let cancel = CancellationToken::new();
    assert_eq!(
        http(&base).retrieve("resp_public", &cancel).await.unwrap()["status"],
        "completed"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|line| line.starts_with("GET "))
    );

    let (base, seen) = stub(vec![(503, JSON, String::new()), (503, JSON, String::new())]);
    let error = http(&base)
        .create(&json!({"background":true,"input":[]}), &cancel)
        .await
        .unwrap_err();
    let failure = error.downcast_ref::<ProviderError>().unwrap();
    assert_eq!(
        (failure.reason, failure.message.as_str()),
        (FailureReason::ApiError, "503 status code (no body)")
    );
    assert_eq!(*seen.lock().unwrap(), ["POST /v1/responses HTTP/1.1"]);

    let (base, _) = stub(vec![(
        400,
        JSON,
        json!({"error":"PUBLIC oops"}).to_string(),
    )]);
    let error = http(&base)
        .create(&json!({"background":true,"input":[]}), &cancel)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "400 \"PUBLIC oops\"");
    let (base, _) = stub(vec![(200, JSON, "PUBLIC not json".into())]);
    let error = http(&base)
        .create(&json!({"background":true,"input":[]}), &cancel)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ProviderError>().unwrap().reason,
        FailureReason::Unknown
    );
}

#[tokio::test]
async fn streamed_answers_beyond_eight_mebibytes_complete() {
    let delta = "PUBLIC streamed text ".repeat(4096);
    let mut body = String::new();
    body.push_str(&format!("data: {}\n\n", json!({"type":"response.created","response":{"id":"resp_public","status":"in_progress","output":[]}})));
    body.push_str(&format!("data: {}\n\n", json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_public","content":[]}})));
    body.push_str(&format!("data: {}\n\n", json!({"type":"response.content_part.added","output_index":0,"item_id":"msg_public","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
    let event = format!(
        "data: {}\n\n",
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_public","content_index":0,"delta":delta})
    );
    while body.len() <= 9 * 1024 * 1024 {
        body.push_str(&event);
    }
    let (base, _) = stub(vec![(200, "text/event-stream", body)]);
    let create = |base: String| async move {
        http(&base)
            .create(
                &json!({"background":false,"input":[]}),
                &CancellationToken::new(),
            )
            .await
    };
    let response = create(base).await.unwrap();
    assert!(providers::text_output(&response).len() > 8 * 1024 * 1024);
    // One event stays within the non-streamed body limit.
    let oversized = format!(
        "data: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"x".repeat(8 * 1024 * 1024)})
    );
    let (base, _) = stub(vec![(200, "text/event-stream", oversized)]);
    assert_eq!(
        create(base).await.unwrap_err().to_string(),
        "Provider stream event exceeds the 8 MiB limit."
    );
}

#[test]
fn over_budget_cli_runs_print_the_file_report_before_the_refusal() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("PUBLIC-notes.txt"),
        "PUBLIC synthetic notes",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path())
        .env("OPENAI_API_KEY", "PUBLIC-SYNTHETIC-KEY")
        .current_dir(home.path())
        .args(["--engine", "api", "--prompt", PROMPT])
        .args(["--file", "PUBLIC-notes.txt", "--max-input", "1"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert!(stdout.contains("File Token Usage"), "{stdout}");
    assert!(stdout.contains("PUBLIC-notes.txt"), "{stdout}");
    assert!(stderr.contains("Input too large ("), "{stderr}");
}

use anyhow::{Result, bail};
use industrious::{
    artifacts,
    config::Environment,
    execution,
    process::ProcessIdentity,
    providers::{
        ApiTransport, FailureReason, ProviderError, ResponseObserver, ResponseProvider, Route,
    },
    sessions::SessionStore,
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
#[test]
fn configured_provider_native_capture_refuses_before_a_session_exists() {
    let home = tempfile::tempdir().unwrap();
    let env = Environment::new();
    let input = json!({"engine":"browser","prompt":"PUBLIC provider-native capture"});
    let error = execution::resolve(
        &input,
        &json!({"browser":{"captureProviderNative":true}}),
        &env,
        home.path(),
    )
    .err()
    .unwrap();
    assert_eq!(
        error.to_string(),
        "browser.captureProviderNative (provider-native capture): this build does not support it. Remove the setting from your configuration."
    );
    execution::resolve(
        &input,
        &json!({"browser":{"captureProviderNative":false}}),
        &env,
        home.path(),
    )
    .unwrap();
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn copied_profile_resolution_selects_one_account_without_home_cookie_fallback() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("cookies.json"),
        r#"[{"name":"PUBLIC-other-account","value":"PUBLIC-other-value"}]"#,
    )
    .unwrap();
    let env = Environment::from([(
        "ORACLE_HOME_DIR".into(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let input = json!({"engine":"browser","prompt":"PUBLIC copied consultation","copyProfile":"PUBLIC source","browserChromeProfile":"Profile 2"});
    let resolved = execution::resolve(&input, &json!({}), &env, home.path()).unwrap();
    assert_eq!(resolved.browser["manualLogin"], false);
    assert_eq!(resolved.browser["copyProfileSource"], "PUBLIC source");
    assert_eq!(resolved.browser["chromeProfile"], "Profile 2");
    assert_eq!(resolved.browser["cookieSync"], false);
    assert!(resolved.browser["cookieBootstrapRequired"].is_null());
    assert!(!resolved.options.to_string().contains("PUBLIC-other-value"));
    for (key, value) in [
        ("browserKeepBrowser", json!(true)),
        ("browserManualLogin", json!(true)),
        ("browserManualLoginProfileDir", json!("PUBLIC-persistent")),
        ("browserInlineCookies", json!("[]")),
    ] {
        let mut changed = input.clone();
        changed[key] = value;
        assert!(
            execution::resolve(&changed, &json!({}), &env, home.path()).is_err(),
            "{key}"
        );
    }
    assert!(
        execution::resolve(
            &input,
            &json!({"browser":{"remoteChrome":{"host":"127.0.0.1","port":1}}}),
            &env,
            home.path()
        )
        .is_err()
    );
    assert!(
        execution::resolve(
            &input,
            &json!({"browser":{"attachRunning":true}}),
            &env,
            home.path()
        )
        .is_err()
    );
    assert!(!home.path().join("sessions").exists());
}
#[test]
fn configured_chrome_selection_is_validated_after_configuration_merge() {
    let home = tempfile::tempdir().unwrap();
    let input = json!({"engine":"browser","prompt":"PUBLIC selection"});
    for profile in [json!("Profile 2"), json!(0), json!({}), json!([])] {
        assert!(
            execution::resolve(
                &input,
                &json!({"browser":{"chromeProfile":profile}}),
                &Environment::new(),
                home.path()
            )
            .is_err(),
            "{profile}"
        );
    }
    assert!(
        execution::resolve(
            &input,
            &json!({"browser":{"chromeProfile":"Default"}}),
            &Environment::new(),
            home.path()
        )
        .is_ok()
    );
}
struct FakeProvider {
    route: Route,
    sends: AtomicUsize,
    reads: AtomicUsize,
    response: Value,
    delay: bool,
    bodies: Mutex<Vec<Value>>,
    during_send: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl ResponseProvider for FakeProvider {
    fn route(&self) -> &Route {
        &self.route
    }
    async fn create(&self, body: &Value, cancel: &CancellationToken) -> Result<Value> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.bodies.lock().unwrap().push(body.clone());
        if let Some(callback) = self.during_send.lock().unwrap().take() {
            callback();
        }
        if self.delay {
            cancel.cancelled().await;
            bail!("Synthetic acknowledgement was lost after send; do not replay");
        }
        if self.response.is_null() {
            bail!("Synthetic endpoint disconnected after receiving the POST");
        }
        Ok(self.response.clone())
    }
    async fn retrieve(&self, id: &str, _: &CancellationToken) -> Result<Value> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(
            json!({"id":id,"status":"completed","model":"gpt-6-astra","output_text":["PUBLIC recovered response"],"reasoning":{"mode":"pro"}}),
        )
    }
}
fn fixture(
    response: Value,
    extra: Value,
) -> (
    tempfile::TempDir,
    SessionStore,
    String,
    ProcessIdentity,
    FakeProvider,
) {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path());
    let mut input = json!({"prompt":"PUBLIC complete provider consultation","engine":"api"});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    let request = execution::resolve(&input, &json!({}), &Environment::new(), home.path()).unwrap();
    let route = Route {
        model: "gpt-6-astra".into(),
        base_url: "https://synthetic.example/v1".into(),
        provider_label: "synthetic".into(),
        key_source: "SYNTHETIC_KEY".into(),
        azure_deployment_name: extra["azure"]["deployment"].as_str().map(str::to_owned),
        transport: ApiTransport::Responses,
    };
    let session = execution::prepare(&store, &request, home.path(), route.clone()).unwrap();
    let id = session["id"].as_str().unwrap().to_owned();
    let identity = ProcessIdentity::current().unwrap();
    execution::start_owner(&store, &id, &identity, false).unwrap();
    let provider = FakeProvider {
        route,
        sends: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        response,
        delay: false,
        bodies: Mutex::new(vec![]),
        during_send: Mutex::new(None),
    };
    (home, store, id, identity, provider)
}
fn completed() -> Value {
    json!({"id":"resp_public","status":"completed","model":"gpt-6-astra","output_text":["  PUBLIC complete answer\n\n "],"reasoning":{"mode":"pro"},"usage":{"input_tokens":17,"output_tokens":23,"total_tokens":40}})
}
struct RetryProvider {
    inner: FakeProvider,
    failures: usize,
    api_error: bool,
    reads_at: Mutex<Vec<tokio::time::Instant>>,
}
impl ResponseProvider for RetryProvider {
    fn route(&self) -> &Route {
        self.inner.route()
    }
    async fn create(&self, body: &Value, cancel: &CancellationToken) -> Result<Value> {
        self.inner.create(body, cancel).await
    }
    async fn retrieve(&self, id: &str, cancel: &CancellationToken) -> Result<Value> {
        self.reads_at
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        let attempt = self.reads_at.lock().unwrap().len();
        if attempt <= self.failures {
            return Err(ProviderError {
                reason: if self.api_error {
                    FailureReason::ApiError
                } else {
                    FailureReason::ConnectionLost
                },
                message: "PUBLIC retrieval failure".into(),
                request_id: Some("req_public".into()),
                status: None,
            }
            .into());
        }
        self.inner.retrieve(id, cancel).await
    }
}
#[tokio::test(start_paused = true)]
async fn background_retrieval_retries_only_connection_failures_with_capped_delays() {
    for api_error in [false, true] {
        let (_home, store, id, identity, provider) = fixture(
            json!({"id":"resp_public","status":"queued"}),
            json!({"timeoutSeconds":60}),
        );
        let provider = RetryProvider {
            inner: provider,
            failures: 4,
            api_error,
            reads_at: Mutex::new(vec![]),
        };
        let result =
            execution::execute(&store, &id, &provider, &identity, &CancellationToken::new()).await;
        assert_eq!(result.is_err(), api_error);
        assert_eq!(provider.inner.sends.load(Ordering::SeqCst), 1);
        let times = provider.reads_at.lock().unwrap();
        if api_error {
            assert_eq!(times.len(), 1);
        } else {
            assert_eq!(times.len(), 5);
            assert_eq!(
                times
                    .windows(2)
                    .map(|times| (times[1] - times[0]).as_secs())
                    .collect::<Vec<_>>(),
                vec![3, 6, 12, 15]
            );
            assert_eq!(store.require(&id).unwrap()["status"], "completed");
        }
    }
}
#[tokio::test(start_paused = true)]
async fn foreground_uses_two_second_grace_poll_and_allows_completed_answers_without_ids() {
    let (_home, store, id, identity, provider) = fixture(
        json!({"id":"resp_public","status":"in_progress"}),
        json!({"background":false}),
    );
    let start = tokio::time::Instant::now();
    execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
    for status in [Some("completed"), None] {
        let mut response = completed();
        response.as_object_mut().unwrap().remove("id");
        if let Some(status) = status {
            response["status"] = json!(status);
        } else {
            response.as_object_mut().unwrap().remove("status");
        }
        let (_home, store, id, identity, provider) = fixture(response, json!({"background":false}));
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
        assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
    }
}
struct InterruptedStream(FakeProvider);
impl ResponseProvider for InterruptedStream {
    fn route(&self) -> &Route {
        self.0.route()
    }
    async fn create(&self, _: &Value, _: &CancellationToken) -> Result<Value> {
        bail!("Observer is required")
    }
    async fn retrieve(&self, id: &str, cancel: &CancellationToken) -> Result<Value> {
        self.0.retrieve(id, cancel).await
    }
    async fn create_observed(
        &self,
        _: &Value,
        _: &CancellationToken,
        observer: &dyn ResponseObserver,
    ) -> Result<Value> {
        self.0.sends.fetch_add(1, Ordering::SeqCst);
        observer.response(&json!({"id":"resp_public","_request_id":"req_public","status":"in_progress","output_text":["PUBLIC partial"]}))?;
        observer.delta("PUBLIC partial")?;
        Err(ProviderError {
            reason: FailureReason::ConnectionLost,
            message: "PUBLIC stream interrupted".into(),
            request_id: Some("req_public".into()),
            status: None,
        }
        .into())
    }
}
struct LiveOutput<'a> {
    store: &'a SessionStore,
    id: &'a str,
    text: Mutex<String>,
}
impl ResponseObserver for LiveOutput<'_> {
    fn response(&self, response: &Value) -> Result<()> {
        assert_eq!(
            self.store.require(self.id)?["response"]["id"],
            response["id"]
        );
        Ok(())
    }
    fn delta(&self, text: &str) -> Result<()> {
        assert!(self.store.log(self.id)?.ends_with(text));
        self.text.lock().unwrap().push_str(text);
        Ok(())
    }
}
#[tokio::test]
async fn foreground_observer_receives_deltas_only_after_durable_evidence_is_saved() {
    let (_home, store, id, identity, provider) = fixture(completed(), json!({"background":false}));
    let provider = InterruptedStream(provider);
    let output = LiveOutput {
        store: &store,
        id: &id,
        text: Mutex::new(String::new()),
    };
    assert!(
        execution::execute_observed(
            &store,
            &id,
            &provider,
            &identity,
            &CancellationToken::new(),
            Some(&output)
        )
        .await
        .is_err()
    );
    assert_eq!(*output.text.lock().unwrap(), "PUBLIC partial");
    assert_eq!(provider.0.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn interrupted_stream_persists_acknowledgement_partial_log_and_request_id_without_replay() {
    let (_home, store, id, identity, provider) = fixture(completed(), json!({"background":false}));
    let provider = InterruptedStream(provider);
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    let metadata = store.require(&id).unwrap();
    assert_eq!(metadata["dispatch"]["responseId"], "resp_public");
    assert_eq!(metadata["response"]["requestId"], "req_public");
    assert_eq!(metadata["transportError"]["reason"], "connection-lost");
    assert_eq!(metadata["recoveryRequired"], true);
    assert!(store.log(&id).unwrap().contains("PUBLIC partial"));
    assert!(metadata.get("capturedAnswer").is_none());
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(provider.0.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn azure_deployment_routes_verify_the_intended_returned_model() {
    for (returned_model, accepted) in [
        ("gpt-6-astra", true),
        ("public-deployment", false),
        ("gpt-6-sol", false),
    ] {
        let mut response = completed();
        response["model"] = json!(returned_model);
        let (_home, store, id, identity, provider) = fixture(
            response,
            json!({"azure":{"deployment":"public-deployment"}}),
        );
        let result =
            execution::execute(&store, &id, &provider, &identity, &CancellationToken::new()).await;
        assert_eq!(result.is_ok(), accepted, "{returned_model}: {result:?}");
        assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
        assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
        assert_eq!(
            provider.bodies.lock().unwrap()[0]["model"],
            "public-deployment"
        );
        let metadata = store.require(&id).unwrap();
        assert_eq!(metadata["recoveryRequired"], !accepted);
        if accepted {
            assert_eq!(metadata["status"], "completed");
            assert!(metadata.get("capturedAnswer").is_some());
        } else {
            assert!(metadata.get("capturedAnswer").is_none());
        }
    }
}
#[tokio::test]
async fn positive_response_is_captured_and_published_once_with_exact_bytes() {
    let (home, store, id, identity, provider) = fixture(completed(), json!({}));
    let meta = execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(meta["status"], "completed");
    assert_eq!(meta["dispatch"]["state"], "completed");
    assert_eq!(meta["recoveryRequired"], false);
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    let receipt = serde_json::from_value(meta["capturedAnswer"].clone()).unwrap();
    assert_eq!(
        artifacts::verify_capture(&store, &receipt).unwrap(),
        b"  PUBLIC complete answer\n\n "
    );
    assert_eq!(meta["usage"]["totalTokens"], 40);
    let body = provider.bodies.lock().unwrap()[0].clone();
    assert_eq!(body["reasoning"]["mode"], "pro");
    assert!(
        body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("PUBLIC complete provider consultation")
    );
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    assert!(!home.path().join("output.md").exists());
}
#[tokio::test]
async fn lost_provider_acknowledgement_retains_reservation_and_forbids_duplicate_send() {
    let (_home, store, id, identity, provider) = fixture(Value::Null, json!({}));
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    let meta = store.require(&id).unwrap();
    assert_eq!(meta["status"], "error");
    assert_eq!(meta["dispatch"]["state"], "uncertain");
    assert_eq!(meta["recoveryRequired"], true);
    assert!(
        store
            .directory(&id)
            .unwrap()
            .join("dispatch-request.json")
            .exists()
    );
    assert!(execution::start_owner(&store, &id, &identity, false).is_err());
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn pre_dispatch_cancellation_releases_capacity_without_issuing_a_request() {
    let (_home, store, id, identity, provider) = fixture(completed(), json!({}));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        execution::execute(&store, &id, &provider, &identity, &cancel)
            .await
            .is_err()
    );
    assert_eq!(provider.sends.load(Ordering::SeqCst), 0);
    let meta = store.require(&id).unwrap();
    assert_eq!(meta["status"], "cancelled");
    assert_eq!(meta["recoveryRequired"], false);
    assert!(meta.get("dispatch").is_none());
}
#[tokio::test]
async fn cancellation_and_deadline_after_send_retain_uncertain_dispatch() {
    for cancel_after_send in [true, false] {
        let (_home, store, id, identity, mut provider) =
            fixture(completed(), json!({"timeoutSeconds":0.05}));
        provider.delay = true;
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if cancel_after_send {
                token.cancel();
            }
        });
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                execution::execute(&store, &id, &provider, &identity, &cancel)
            )
            .await
            .unwrap()
            .is_err()
        );
        trigger.await.unwrap();
        let meta = store.require(&id).unwrap();
        assert_eq!(
            meta["status"],
            if cancel_after_send {
                "cancelled"
            } else {
                "error"
            }
        );
        assert_eq!(meta["recoveryRequired"], true);
        assert_eq!(meta["dispatch"]["state"], "uncertain");
        assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn wrong_model_tier_missing_identity_and_nonterminal_responses_cannot_publish() {
    for (field, value) in [
        ("model", json!("retired-model")),
        ("reasoning", json!({"mode":"standard"})),
        ("id", json!("../foreign")),
        ("status", json!("incomplete")),
        ("output_text", json!("")),
    ] {
        let mut response = completed();
        response[field] = value;
        let (_home, store, id, identity, provider) = fixture(response.clone(), json!({}));
        assert!(
            execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
                .await
                .is_err(),
            "{field}"
        );
        let meta = store.require(&id).unwrap();
        assert_eq!(meta["recoveryRequired"], true);
        assert!(meta.get("capturedAnswer").is_none());
        assert_eq!(
            serde_json::from_slice::<Value>(
                &fs::read(store.directory(&id).unwrap().join("provider-response.json")).unwrap()
            )
            .unwrap(),
            response
        );
    }
}
#[tokio::test]
async fn background_completion_retrieves_the_same_response_without_resending() {
    let (_home, store, id, identity, provider) = fixture(
        json!({"id":"resp_public","status":"queued"}),
        json!({"timeoutSeconds":10}),
    );
    let meta = execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(meta["status"], "completed");
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
}
#[test]
fn dead_worker_capacity_release_preserves_original_dispatch_and_target_evidence() {
    let (_home, store, id, _identity, _provider) = fixture(completed(), json!({}));
    let dispatch = json!({"state":"uncertain","reservationId":"PUBLIC reservation"});
    let browser = json!({"runtime":{"ownedRecoveryTarget":{"targetId":"PUBLIC-TARGET","claimId":"PUBLIC-CLAIM"}}});
    store.update(&id,|_|Ok(Some(json!({"dispatch":dispatch,"browser":browser,"lifecycle":{"processIdentity":{"pid":u32::MAX,"nativeStart":1,"startedAtMs":1.0}}})))).unwrap();
    let meta = store.release_dead_worker_capacity(&id).unwrap();
    assert_eq!(meta["lifecycle"]["capacityReleased"], true);
    assert_eq!(meta["dispatch"], dispatch);
    assert_eq!(meta["browser"], browser);
    assert_eq!(meta["recoveryRequired"], true);
}
#[tokio::test]
async fn provider_route_change_is_refused_before_request_reservation() {
    let (_home, store, id, identity, mut provider) = fixture(completed(), json!({}));
    provider.route.key_source = "REPLACEMENT_SOURCE".into();
    let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(provider.sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
        before
    );
}

#[test]
fn worker_death_never_clears_a_preexisting_recovery_requirement() {
    let (_home, store, id, _identity, _provider) = fixture(completed(), json!({}));
    store.update(&id,|_|Ok(Some(json!({"recoveryRequired":true,"lifecycle":{"processIdentity":{"pid":u32::MAX,"nativeStart":1,"startedAtMs":1.0}}})))).unwrap();
    assert_eq!(
        store.release_dead_worker_capacity(&id).unwrap()["recoveryRequired"],
        true
    );
}
#[tokio::test]
async fn pid_reuse_cannot_impersonate_the_executing_worker() {
    let (_home, store, id, mut identity, provider) = fixture(completed(), json!({}));
    identity.native_start += 1;
    store
        .update(&id, |_| {
            Ok(Some(json!({"lifecycle":{"processIdentity":identity}})))
        })
        .unwrap();
    let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(provider.sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn a_replacement_session_with_reused_worker_identity_is_never_rewritten() {
    let (_home, store, id, identity, provider) = fixture(completed(), json!({}));
    let directory = store.directory(&id).unwrap();
    let original = store.root().join("original-session");
    let path = directory.clone();
    let moved = original.clone();
    let expected = std::sync::Arc::new(Mutex::new(Vec::new()));
    let saved = expected.clone();
    *provider.during_send.lock().unwrap() = Some(Box::new(move || {
        let mut metadata: Value =
            serde_json::from_slice(&fs::read(path.join("meta.json")).unwrap()).unwrap();
        metadata["replacementMarker"] = json!("PUBLIC preserve replacement");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        let bytes = serde_json::to_vec(&metadata).unwrap();
        fs::write(path.join("meta.json"), &bytes).unwrap();
        *saved.lock().unwrap() = bytes;
    }));
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(directory.join("meta.json")).unwrap(),
        *expected.lock().unwrap()
    );
    assert!(!directory.join("provider-response.json").exists());
    let retained: Value =
        serde_json::from_slice(&fs::read(original.join("meta.json")).unwrap()).unwrap();
    assert_eq!(retained["dispatch"]["state"], "started");
    assert_eq!(retained["recoveryRequired"], true);
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn a_new_controller_incarnation_in_the_same_process_refuses_late_completion() {
    let (_home, store, id, identity, provider) = fixture(completed(), json!({}));
    let changed = store.clone();
    let session_id = id.clone();
    *provider.during_send.lock().unwrap() = Some(Box::new(move || {
        changed.update(&session_id,|_|Ok(Some(json!({"startedAt":"2026-10-13T00:00:01Z","replacementMarker":"PUBLIC new incarnation"})))).unwrap();
    }));
    assert!(
        execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
            .await
            .is_err()
    );
    let metadata = store.require(&id).unwrap();
    assert_eq!(metadata["status"], "running");
    assert_eq!(metadata["replacementMarker"], "PUBLIC new incarnation");
    assert!(metadata.get("capturedAnswer").is_none());
    assert_eq!(metadata["recoveryRequired"], true);
}

#[test]
fn releasing_a_dead_completed_worker_does_not_reopen_a_completed_dispatch() {
    let (_home, store, id, _identity, _provider) = fixture(completed(), json!({}));
    store.update(&id,|_|Ok(Some(json!({"status":"completed","recoveryRequired":false,"dispatch":{"state":"completed"},"lifecycle":{"processIdentity":{"pid":u32::MAX,"nativeStart":1,"startedAtMs":1.0}}})))).unwrap();
    let meta = store.release_dead_worker_capacity(&id).unwrap();
    assert_eq!(meta["recoveryRequired"], false);
    assert_eq!(meta["status"], "completed");
    assert_eq!(meta["dispatch"]["state"], "completed");
}
#[derive(Default)]
struct Progress(Mutex<Vec<String>>);
impl ResponseObserver for Progress {
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
#[tokio::test(start_paused = true)]
async fn background_runs_log_reference_status_and_heartbeat_lines() {
    let (_home, store, id, identity, provider) = fixture(
        json!({"id":"resp_public","status":"queued"}),
        json!({"timeoutSeconds":10,"heartbeatSeconds":1}),
    );
    let progress = Progress::default();
    let meta = execution::execute_observed(
        &store,
        &id,
        &provider,
        &identity,
        &CancellationToken::new(),
        Some(&progress),
    )
    .await
    .unwrap();
    assert_eq!(meta["status"], "completed");
    let lines = progress.0.lock().unwrap().clone();
    assert_eq!(
        lines[..6],
        [
            "API scheduled background response resp_public (status=queued). Monitoring up to 0 minutes for completion...",
            "API background response status=queued. We'll keep retrying automatically.",
            "API background run still in progress \u{2014} 1s elapsed.",
            "API background run still in progress \u{2014} 2s elapsed.",
            "API background run still in progress \u{2014} 3s elapsed.",
            "API background run still in progress \u{2014} 4s elapsed.",
        ]
    );
    // The fifth tick races the five-second poll; nothing else may follow it.
    assert!(
        lines.len() == 6
            || lines[6..] == ["API background run still in progress \u{2014} 5s elapsed."]
    );
    assert!(!store.log(&id).unwrap().contains("API "));
}
struct SlowStream(FakeProvider);
impl ResponseProvider for SlowStream {
    fn route(&self) -> &Route {
        &self.0.route
    }
    async fn create(&self, _: &Value, _: &CancellationToken) -> Result<Value> {
        bail!("Observer is required")
    }
    async fn retrieve(&self, id: &str, cancel: &CancellationToken) -> Result<Value> {
        self.0.retrieve(id, cancel).await
    }
    async fn create_observed(
        &self,
        _: &Value,
        _: &CancellationToken,
        observer: &dyn ResponseObserver,
    ) -> Result<Value> {
        tokio::time::sleep(Duration::from_millis(2500)).await;
        observer.delta("  PUBLIC complete answer")?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        Ok(completed())
    }
}
#[tokio::test(start_paused = true)]
async fn streaming_heartbeat_stops_at_the_first_answer_delta_and_disabled_heartbeat_is_silent() {
    for (seconds, expected) in [
        (
            1,
            vec![
                "API connection active \u{2014} 1s elapsed. Timeout in ~9s if no response.",
                "API connection active \u{2014} 2s elapsed. Timeout in ~8s if no response.",
            ],
        ),
        (0, vec![]),
    ] {
        let (_home, store, id, identity, provider) = fixture(
            completed(),
            json!({"background":false,"timeoutSeconds":10,"heartbeatSeconds":seconds}),
        );
        let progress = Progress::default();
        execution::execute_observed(
            &store,
            &id,
            &SlowStream(provider),
            &identity,
            &CancellationToken::new(),
            Some(&progress),
        )
        .await
        .unwrap();
        assert_eq!(*progress.0.lock().unwrap(), expected);
        assert_eq!(
            store.require(&id).unwrap()["options"].get("heartbeatIntervalMs"),
            (seconds > 0).then(|| json!(1000)).as_ref()
        );
    }
}
#[test]
fn heartbeat_and_files_report_options_resolve_like_the_reference() {
    let home = tempfile::tempdir().unwrap();
    let resolve = |input: Value| {
        let mut input = input;
        input["prompt"] = json!("PUBLIC timeout defaults");
        input["engine"] = json!("api");
        execution::resolve(
            &input,
            &json!({"heartbeatSeconds":2.5,"filesReport":true}),
            &Environment::new(),
            home.path(),
        )
        .unwrap()
        .options
    };
    let options = resolve(json!({}));
    assert_eq!(options["heartbeatIntervalMs"], 2500);
    assert_eq!(options["filesReport"], true);
    let options = resolve(json!({"heartbeatSeconds":0,"filesReport":false}));
    assert!(options.get("heartbeatIntervalMs").is_none());
    assert_eq!(options["filesReport"], false);
}
#[test]
fn notification_choice_is_resolved_once_and_saved_beside_the_options() {
    let (_home, store, id, _identity, _provider) = fixture(completed(), json!({"notify":false}));
    let meta = store.require(&id).unwrap();
    assert_eq!(
        meta["notifications"],
        json!({"enabled":false,"sound":false})
    );
    assert!(meta["options"].get("notifications").is_none());
    let (_home, store, id, _identity, _provider) = fixture(completed(), json!({}));
    assert_eq!(
        store.require(&id).unwrap()["notifications"],
        json!({"enabled":true,"sound":false})
    );
}
#[tokio::test]
async fn logged_execution_writes_the_reference_heading_answer_and_finish_lines() {
    // The detached worker's run: a non-streamed answer gets the status line, the "Answer:"
    // heading, the answer and the finish lines in the session log before completion.
    let (_home, store, id, identity, provider) = fixture(completed(), json!({"verbose":true}));
    execution::execute_logged(
        &store,
        &id,
        &provider,
        &identity,
        &CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    let log = store.log(&id).unwrap();
    let status = log
        .find("[verbose] Response status: completed\n\nAnswer:\n")
        .unwrap_or_else(|| panic!("heading missing from {log}"));
    let answer = log.find("PUBLIC complete answer").unwrap();
    let finish = log.find(" \u{b7} sid=").unwrap();
    assert!(status < answer && answer < finish, "{log}");
    assert_eq!(log.matches("Answer:").count(), 1, "{log}");
    // The verbose detail line follows the summary.
    let detail = log[finish..].lines().nth(1).unwrap_or_default();
    assert!(
        detail.starts_with("est\u{2192}actual=") && detail.contains("\u{2192}17 ("),
        "{log}"
    );
    // The foreground run keeps its plain log: stdout carries those lines instead.
    let (_home, store, id, identity, provider) = fixture(completed(), json!({"verbose":true}));
    execution::execute(&store, &id, &provider, &identity, &CancellationToken::new())
        .await
        .unwrap();
    let log = store.log(&id).unwrap();
    assert!(
        log.contains("PUBLIC complete answer") && !log.contains("Answer:"),
        "{log}"
    );
}
#[derive(Default)]
struct Recorder {
    lines: Mutex<Vec<String>>,
}
impl ResponseObserver for Recorder {
    fn response(&self, _: &Value) -> Result<()> {
        Ok(())
    }
    fn delta(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn log(&self, line: &str) -> Result<()> {
        self.lines.lock().unwrap().push(line.to_owned());
        Ok(())
    }
}
#[tokio::test(start_paused = true)]
async fn logged_retry_notices_reach_the_caller_and_the_session_log_as_they_happen() {
    // Reference combinedLog: an inline run prints each retry notice and logs it.
    let (_home, store, id, identity, provider) = fixture(
        json!({"id":"resp_public","status":"queued"}),
        json!({"timeoutSeconds":60}),
    );
    let provider = RetryProvider {
        inner: provider,
        failures: 2,
        api_error: false,
        reads_at: Mutex::new(vec![]),
    };
    let recorder = Recorder::default();
    execution::execute_logged(
        &store,
        &id,
        &provider,
        &identity,
        &CancellationToken::new(),
        Some(&recorder),
    )
    .await
    .unwrap();
    let lines = recorder.lines.lock().unwrap().clone();
    let notices: Vec<_> = lines.iter().filter(|l| l.contains("Retrying in")).collect();
    assert_eq!(notices.len(), 2, "{lines:?}");
    assert!(notices[0].ends_with("Retrying in 3s...") && notices[1].ends_with("Retrying in 6s..."));
    let log = store.log(&id).unwrap();
    for notice in &notices {
        assert!(log.contains(&format!("{notice}\n")), "{log}");
    }
    assert!(
        log.find(notices[1].as_str()) < log.find("\nAnswer:\n"),
        "{log}"
    );
}

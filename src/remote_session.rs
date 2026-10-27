//! Durable local receiver sessions for the remote browser transport.
use crate::{
    artifacts,
    browser_inputs::StagedUploads,
    browser_runner,
    config::Environment,
    execution::{ExecutionWitness, ResolvedRequest},
    process::ProcessIdentity,
    remote_client::{Hooks, Persist, RemoteClient, Request},
    sessions::{self, SessionStore, merge_object, timestamp},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

pub struct Connection {
    pub host: String,
    pub token: Option<String>,
}
impl Connection {
    pub fn resolve(browser: &Value, env: &Environment) -> Option<Self> {
        let clean = |value: &str| {
            let value = crate::policy::trim(value);
            (!value.is_empty()).then(|| value.to_owned())
        };
        let host = browser["remoteHost"]
            .as_str()
            .and_then(clean)
            .or_else(|| env.get("ORACLE_REMOTE_HOST").and_then(|value| clean(value)))?;
        Some(Self {
            host,
            token: browser["remoteToken"].as_str().and_then(clean).or_else(|| {
                env.get("ORACLE_REMOTE_TOKEN")
                    .and_then(|value| clean(value))
            }),
        })
    }
    fn client(&self, store: &SessionStore) -> Result<RemoteClient> {
        RemoteClient::new(&self.host, self.token.clone(), store.clone())
    }
    pub(crate) fn worker_environment(&self, env: &Environment) -> Environment {
        let mut env = env.clone();
        env.insert("ORACLE_REMOTE_HOST".into(), self.host.clone());
        if let Some(token) = &self.token {
            env.insert("ORACLE_REMOTE_TOKEN".into(), token.clone());
        } else {
            env.remove("ORACLE_REMOTE_TOKEN");
        }
        env
    }
}
pub fn prepare(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    connection: &Connection,
) -> Result<Value> {
    prepare_with_base(store, request, cwd, connection, None)
}
pub(crate) fn prepare_restart(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    connection: &Connection,
    source: &str,
) -> Result<Value> {
    prepare_with_base(store, request, cwd, connection, Some(source))
}
fn prepare_with_base(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    connection: &Connection,
    source: Option<&str>,
) -> Result<Value> {
    anyhow::ensure!(
        request.browser["researchMode"] != "search",
        "Web Search is a local browser pilot; --remote-host does not negotiate this capability yet. Use local Chrome or --browser-attach-running."
    );
    connection.client(store)?;
    let mut request = request.clone();
    if let Some(options) = request.options.as_object_mut() {
        for key in ["remoteToken", "inlineCookies", "inlineCookiesSource"] {
            options.remove(key);
        }
    }
    for browser in [&mut request.browser, &mut request.options["browserConfig"]] {
        if let Some(browser) = browser.as_object_mut() {
            for key in ["remoteToken", "inlineCookies", "inlineCookiesSource"] {
                browser.remove(key);
            }
            browser.insert("remoteHost".into(), json!(connection.host));
        }
    }
    match source {
        Some(source) => browser_runner::prepare_restart(store, &request, cwd, source),
        None => browser_runner::prepare(store, &request, cwd),
    }
}
fn attachments(
    staged: Option<&StagedUploads>,
    store: &SessionStore,
    id: &str,
) -> Result<Vec<Value>> {
    let Some(staged) = staged else {
        return Ok(vec![]);
    };
    staged.verify(store, id)?;
    let result = (0..staged.files.len()).map(|index| {
        let mut file = staged.open(index)?;
        let bytes = file.contents()?;
        Ok(json!({"fileName":file.name,"displayPath":staged.files[index].display_path,"sizeBytes":bytes.len(),"contentBase64":STANDARD.encode(bytes)}))
    }).collect::<Result<Vec<_>>>()?;
    staged.verify(store, id)?;
    Ok(result)
}
fn payload(store: &SessionStore, id: &str) -> Result<Value> {
    let committed = browser_runner::request(store, id)?;
    let meta = store.require(id)?;
    let mut payload = json!({"prompt":committed.prompt,"attachments":attachments(committed.staged_uploads.as_ref(),store,id)?,
        "browserConfig":crate::remote_host::client_browser_config(&meta["options"]["browserConfig"]),
        "options":{"model":meta["model"],"sessionId":id,"cancelOnDisconnect":true,
        "imageOutputRequested":crate::browser_images::requested_destination(&meta["options"])?.is_some()}});
    for key in ["heartbeatIntervalMs", "verbose"] {
        if let Some(value) = meta["options"].get(key) {
            payload["options"][key] = value.clone();
        }
    }
    payload["options"]["followUpPrompts"] = json!(crate::browser_turns::prompts(&meta["options"])?);
    if let Some(fallback) = committed.fallback {
        let mut sources = vec![];
        for source in &fallback.text_sources {
            sources.extend(attachments(Some(source), store, id)?);
        }
        sources.extend(attachments(
            fallback
                .remote_source
                .as_ref()
                .or(fallback.raw_source.as_ref()),
            store,
            id,
        )?);
        payload["fallbackSubmission"] = json!({"prompt":fallback.composer_text,"attachments":sources,"bundle":fallback.pending_bundle});
    }
    Ok(payload)
}
fn materialise_fallback(store: &SessionStore, id: &str, payload: &mut Value) -> Result<()> {
    materialise_fallback_with(store, id, payload, || {})
}
fn materialise_fallback_with(
    store: &SessionStore,
    id: &str,
    payload: &mut Value,
    before_bundle: impl FnOnce(),
) -> Result<()> {
    let fallback = browser_runner::request(store, id)?
        .fallback
        .context("Missing committed fallback")?;
    let mut files = vec![];
    let mut originals = vec![];
    let sources = fallback.text_sources.iter().chain(
        fallback
            .remote_source
            .as_ref()
            .or(fallback.raw_source.as_ref()),
    );
    for staged in sources {
        staged.verify(store, id)?;
        let paths = staged.paths()?;
        for (index, saved) in staged.files.iter().enumerate() {
            let mut upload = staged.open(index)?;
            let bytes = upload.contents()?;
            files.push(crate::prompts::PromptFile {
                path: paths[index].clone(),
                display_path: saved.display_path.clone(),
                size_bytes: bytes.len() as u64,
                content: if crate::browser_prompt::raw_upload(&paths[index]) {
                    String::new()
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                },
            });
            originals.push(bytes);
        }
        staged.verify(store, id)?;
    }
    let pending = fallback
        .pending_bundle
        .context("Missing deferred bundle settings")?;
    let text_files = files
        .iter()
        .filter(|file| !crate::browser_prompt::raw_upload(&file.path))
        .cloned()
        .collect::<Vec<_>>();
    let expected_bundle = if pending.format == crate::browser_prompt::BundleFormat::Zip {
        let text = files
            .iter()
            .enumerate()
            .filter(|(_, file)| !crate::browser_prompt::raw_upload(&file.path));
        let raw = files.iter().enumerate().filter(|(_, file)| {
            pending.scope == crate::browser_prompt::BundleScope::All
                && crate::browser_prompt::raw_upload(&file.path)
        });
        let entries = text
            .chain(raw)
            .map(|(index, file)| crate::browser_bundle::ZipEntry {
                path: &file.display_path,
                bytes: &originals[index],
            })
            .collect::<Vec<_>>();
        let size = entries.iter().try_fold(0usize, |total, entry| {
            total
                .checked_add(entry.bytes.len())
                .context("Fallback ZIP size overflow")
        })?;
        ensure!(
            size <= 128 * 1024 * 1024,
            "Browser ZIP bundle inputs exceed the 134217728-byte in-memory limit."
        );
        crate::browser_bundle::stored_zip(&entries)?
    } else {
        let text = crate::prompts::file_sections(&text_files, false, true);
        if text.is_empty() {
            vec![]
        } else {
            format!("{text}\n").into_bytes()
        }
    };
    before_bundle();
    let mut plan = crate::browser_prompt::BrowserPrompt::remote_fallback(
        &files,
        &fallback.composer_text,
        fallback.pending_bundle,
        &store.directory(id)?,
    )?;
    let serialized = (|| -> Result<Value> {
        let mut attachments = vec![];
        for attachment in &plan.attachments {
            let mut file = crate::browser_dom::UploadFile::open(&attachment.path)?;
            let bytes = file.contents()?;
            let expected = if attachment.generated_bundle {
                expected_bundle.as_slice()
            } else {
                let index = files
                    .iter()
                    .position(|source| source.path == attachment.path)
                    .context("Materialised fallback has an unexpected source")?;
                originals[index].as_slice()
            };
            ensure!(
                bytes == expected,
                "Materialised fallback bytes differ from their committed source"
            );
            attachments.push(json!({"fileName":file.name,"displayPath":attachment.display_path,"sizeBytes":bytes.len(),"contentBase64":STANDARD.encode(bytes)}));
        }
        for staged in fallback.text_sources.iter().chain(
            fallback
                .remote_source
                .as_ref()
                .or(fallback.raw_source.as_ref()),
        ) {
            staged.verify(store, id)?;
        }
        Ok(json!({"prompt":plan.composer_text,"attachments":attachments}))
    })();
    let cleanup = plan.cleanup();
    payload["fallbackSubmission"] = match (serialized, cleanup) {
        (Ok(value), Ok(())) => value,
        (Err(error), Err(cleanup)) => {
            return Err(error.context(format!("Fallback cleanup also failed: {cleanup:#}")));
        }
        (Err(error), _) | (_, Err(error)) => return Err(error),
    };
    Ok(())
}
struct Receiver {
    store: SessionStore,
    id: String,
    model: String,
    witness: ExecutionWitness,
    recovery: bool,
    options_hash: String,
    remote_identity: Mutex<Value>,
    cancel: CancellationToken,
}
fn remote_identity(meta: &Value) -> Value {
    json!({"remoteHost":meta["browser"]["runtime"]["remoteHost"],"remoteRunId":meta["browser"]["runtime"]["remoteRunId"]})
}
impl Receiver {
    fn new(
        store: &SessionStore,
        id: &str,
        identity: &ProcessIdentity,
        recovery: bool,
        cancel: &CancellationToken,
    ) -> Result<Arc<Self>> {
        let meta = store.require(id)?;
        let witness = ExecutionWitness::capture(store, id, identity)?;
        witness.require(&meta)?;
        Ok(Arc::new(Self {
            store: store.clone(),
            id: id.into(),
            model: meta["model"]
                .as_str()
                .context("Missing receiver model")?
                .into(),
            witness,
            recovery,
            options_hash: artifacts::sha256(&serde_json::to_vec(&meta["options"])?),
            remote_identity: Mutex::new(remote_identity(&meta)),
            cancel: cancel.clone(),
        }))
    }
    fn require_bound(&self, meta: &Value, remote: &Value) -> Result<()> {
        self.witness.require(meta)?;
        ensure!(
            artifacts::sha256(&serde_json::to_vec(&meta["options"])?) == self.options_hash
                && remote_identity(meta) == *remote,
            "Remote receiver options or run identity changed; refusing late writes"
        );
        Ok(())
    }
    fn require(&self, meta: &Value) -> Result<()> {
        let remote = self
            .remote_identity
            .lock()
            .map_err(|_| anyhow::anyhow!("Remote receiver identity lock failed"))?;
        self.require_bound(meta, &remote)
    }
    fn update(&self, changes: impl FnOnce(&Value) -> Result<Value>) -> Result<Value> {
        let remote = self
            .remote_identity
            .lock()
            .map_err(|_| anyhow::anyhow!("Remote receiver identity lock failed"))?
            .clone();
        let updated=self.store
            .update_session_and_model(&self.id, &self.model, |meta, run| {
                self.require_bound(meta,&remote)?;
                ensure!(
                    meta["status"] == "running"
                        && run.is_some_and(|run| run["status"] == "running"),
                    "Remote receiver is no longer running; refusing late writes"
                );
                let patch=changes(meta)?;
                let model_patch=patch.get("status").map(|status| {
                    let mut run=json!({"status":status,"completedAt":patch["completedAt"],"effectiveModel":patch["browser"]["effectiveModel"],"response":{"status":status}});
                    for key in ["usage","elapsedMs"] {if let Some(value)=patch.get(key){run[key]=value.clone();}}
                    run
                });
                Ok(Some((patch, model_patch)))
            })?;
        *self
            .remote_identity
            .lock()
            .map_err(|_| anyhow::anyhow!("Remote receiver identity lock failed"))? =
            remote_identity(&updated);
        Ok(updated)
    }
    fn persist(self: &Arc<Self>, action: fn(&Self, Value) -> Result<()>) -> Persist {
        let receiver = self.clone();
        Arc::new(move |value| {
            let receiver = receiver.clone();
            Box::pin(async move { action(&receiver, value) })
        })
    }
    fn hooks(self: &Arc<Self>) -> Hooks {
        let logger = self.clone();
        Hooks {
            runtime_hint: Some(self.persist(Self::hint)),
            artifact_receipt: Some(self.persist(Self::receipt)),
            result_captured: Some(self.persist(Self::captured)),
            log: Some(Arc::new(move |text| {
                logger.store.append_log_guarded(
                    &logger.id,
                    Some(&logger.model),
                    &format!("{text}\n"),
                    |meta, _| logger.require(meta),
                )
            })),
        }
    }
    fn hint(&self, value: Value) -> Result<()> {
        crate::remote_result::session_id(
            value["remoteRunId"]
                .as_str()
                .context("Remote hint has no run identity")?,
        )?;
        let meta = self.store.require(&self.id)?;
        let expected = meta["options"]["browserConfig"]["remoteHost"]
            .as_str()
            .or(meta["browser"]["runtime"]["remoteHost"].as_str())
            .context("Remote receiver has no admitted host")?;
        ensure!(
            value["remoteHost"].as_str() == Some(crate::policy::trim(expected)),
            "Remote hint changed its admitted host"
        );
        ensure!(
            meta["browser"]["runtime"]["remoteRunId"].is_null()
                || meta["browser"]["runtime"]["remoteRunId"] == value["remoteRunId"],
            "Remote hint changed its admitted run"
        );
        self.update(|meta| Ok(json!({"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"], &value)}))})))?;
        Ok(())
    }
    fn receipt(&self, value: Value) -> Result<()> {
        self.update(|meta| Ok(json!({"artifacts":sessions::merge_artifacts(&meta["artifacts"], &json!([value]), &Value::Null)})))?;
        Ok(())
    }
    fn captured(&self, result: Value) -> Result<()> {
        ensure!(
            !self.cancel.is_cancelled(),
            "Remote result cancelled before capture admission"
        );
        self.update(|meta| {
            ensure!(meta["browser"]["remoteResult"].is_null(), "Remote receiver already captured a result; recover without replaying");
            // The transport awaits this complete transaction before acknowledging the host.
            let durable = CancellationToken::new();
            let saved = artifacts::capture(&self.store, &self.id, if self.recovery {"remote-recovered-result.json"}else{"remote-result.json"}, &serde_json::to_vec(&result)?, &durable)?;
            let answer = result["answerMarkdown"].as_str().filter(|text| !crate::policy::trim(text).is_empty()).or(result["answerText"].as_str()).unwrap_or("");
            let captured = if crate::policy::trim(answer).is_empty() { None } else { Some(artifacts::capture(&self.store, &self.id, "captured-answer.md", answer.as_bytes(), &durable)?) };
            let mut browser = merge_object(&meta["browser"], &json!({"remoteResult":saved,"capturedAnswer":captured}));
            for key in ["effectiveModel", "modelSelection", "thinkingSelection", "archive", "warnings"] {
                if let Some(value) = result.get(key) { browser[key] = value.clone(); }
            }
            let mut artifacts = sessions::merge_artifacts(&meta["artifacts"], &result["artifacts"], &Value::Null);
            if let Some(output) = meta["options"]["writeOutputPath"].as_str() {
                let destination = Path::new(meta["cwd"].as_str().context("Missing receiver working directory")?).join(output);
                let view = merge_object(meta, &json!({"browser":browser}));
                let receipt = crate::browser_output::save_answer(&self.store,&destination,answer.as_bytes(),&crate::browser_output::answer_heading(&view),&durable)?;
                artifacts = sessions::merge_artifacts(&artifacts,&json!([{"kind":"output","path":receipt["file"],"bytes":receipt["bytes"],"sha256":receipt["sha256"]}]),&Value::Null);
                browser["requiredTextOutput"] = receipt;
            }
            if meta["options"]["writeArtifacts"] == true {
                let files = result["savedFiles"].as_array().cloned().unwrap_or_default();
                let output = browser["requiredTextOutput"]["file"].as_str().map(Path::new);
                let exports = crate::browser_output::export_files(output,&files,&durable)?;
                artifacts = sessions::merge_artifacts(&artifacts,&json!(exports),&Value::Null);
            }
            let mut patch=json!({"browser":browser,"capturedAnswer":captured,"artifacts":artifacts});
            patch["status"]=json!("completed");patch["completedAt"]=json!(timestamp());patch["recoveryRequired"]=json!(false);
            patch["response"]=merge_object(&meta["response"],&json!({"status":"completed"}));
            patch["response"].as_object_mut().unwrap().remove("incompleteReason");
            for key in ["usage","elapsedMs"] {if let Some(value)=result.get(key){patch[key]=value.clone();}}
            Ok(patch)
        })?;
        let answer = result["answerMarkdown"]
            .as_str()
            .filter(|text| !crate::policy::trim(text).is_empty())
            .or(result["answerText"].as_str())
            .unwrap_or("");
        self.store.append_log_guarded(
            &self.id,
            Some(&self.model),
            &format!("{answer}\n"),
            |meta, _| self.require(meta),
        )?;
        Ok(())
    }
}
pub async fn execute(
    store: &SessionStore,
    id: &str,
    connection: &Connection,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(
        *identity == ProcessIdentity::current()?
            && identity.liveness() == crate::process::Liveness::Alive,
        "Remote dispatch requires the current live native worker identity"
    );
    let meta = store.require(id)?;
    ensure!(
        meta["options"]["browserConfig"]["remoteHost"]
            .as_str()
            .map(crate::policy::trim)
            == Some(crate::policy::trim(&connection.host)),
        "Remote connection differs from its committed receiver host"
    );
    let receiver = Receiver::new(store, id, identity, false, cancel)?;
    receiver.require(&meta)?;
    let client = connection.client(store)?;
    let mut payload = payload(store, id)?;
    receiver.update(|meta| {
        ensure!(meta["browser"]["remoteDispatch"].is_null(), "Remote dispatch was already attempted; recover its saved run without replaying");
        Ok(json!({"browser":merge_object(&meta["browser"], &json!({"remoteDispatch":{"attemptId":uuid::Uuid::new_v4(),"startedAt":timestamp()}}))}))
    })?;
    let result = async {
        if !payload["fallbackSubmission"]["bundle"].is_null() {
            let health = client.health(cancel).await?;
            if health["ok"] != true || health["capabilities"]["deferredFallbackBundling"] != true {
                materialise_fallback(store, id, &mut payload)?;
            }
        }
        let image_destination = crate::browser_images::requested_destination(&meta["options"])?
            .map(|path| Path::new(meta["cwd"].as_str().unwrap_or(".")).join(path));
        client
            .run(
                Request {
                    payload,
                    recover_run_id: None,
                    session_id: Some(id.into()),
                    image_destination,
                    cancel: cancel.clone(),
                    require_cancellation: true,
                    verbose: meta["options"]["verbose"] == true,
                },
                receiver.hooks(),
            )
            .await
    }
    .await;
    let status = if result.is_ok() {
        "completed"
    } else if cancel.is_cancelled() {
        "cancelled"
    } else {
        "error"
    };
    let committed = store.update_session_and_model(id, &receiver.model, |meta, run| {
        receiver.require(meta)?;
        if meta["status"]=="completed" && run.is_some_and(|run|run["status"]=="completed") {
            let patch=if let Err(error)=&result {json!({"recoveryRequired":true,"browser":merge_object(&meta["browser"],&json!({"remoteHandoffError":format!("{error:#}")}))})}else{json!({"recoveryRequired":false})};
            return Ok(Some((patch,None)));
        }
        ensure!(
            meta["status"] == "running" && run.is_some_and(|run| run["status"] == "running"),
            "Remote receiver changed before finalisation"
        );
        let mut patch = json!({"status":status,"completedAt":timestamp()});
        if let Err(error) = &result {
            patch["errorMessage"] = json!(format!("{error:#}"));
            patch["recoveryRequired"] = json!(true);
        }
        Ok(Some((patch.clone(), Some(patch))))
    });
    match (result, committed) {
        (Err(error), Err(persistence)) => Err(error.context(format!(
            "Receiver finalisation also failed: {persistence:#}"
        ))),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(_), Ok(metadata)) => {
            crate::notify::completed(&metadata, &store.log(id).unwrap_or_default()).await;
            Ok(metadata)
        }
    }
}

pub async fn recover(
    store: &SessionStore,
    cwd: &Path,
    connection: &Connection,
    run_id: &str,
    cancel: &CancellationToken,
) -> Result<(String, Value)> {
    crate::remote_result::session_id(run_id)?;
    ensure!(
        !cancel.is_cancelled(),
        "Remote recovery cancelled before receiver creation"
    );
    let client = connection.client(store)?;
    let identity = ProcessIdentity::current()?;
    let meta=store.create(json!({"prompt":"Recovered remote browser result","model":"remote-recovery","mode":"browser"}),cwd,None)?;
    let id = meta["id"]
        .as_str()
        .context("Missing recovery session")?
        .to_owned();
    store.update_session_and_model(&id,"remote-recovery",|meta,_| {
        ensure!(meta["status"]=="pending","Recovery receiver already started");
        Ok(Some((json!({"status":"running","startedAt":timestamp(),"browser":{"runtime":{"remoteRunId":run_id,"remoteHost":connection.host}},"lifecycle":{"engine":"browser","execution":"foreground","attached":true,"detached":false,"workerPid":identity.pid,"workerProcessStartedAtMs":identity.started_at_ms,"processIdentity":identity}}),Some(json!({"status":"running","startedAt":timestamp()})))))
    })?;
    let receiver = Receiver::new(store, &id, &identity, true, cancel)?;
    let result = client
        .run(
            Request {
                payload: Value::Null,
                recover_run_id: Some(run_id.into()),
                session_id: Some(id.clone()),
                image_destination: None,
                cancel: cancel.clone(),
                require_cancellation: false,
                verbose: false,
            },
            receiver.hooks(),
        )
        .await;
    if let Err(error) = result {
        let saved=store.update_session_and_model(&id,"remote-recovery",|meta,_| {
            receiver.require(meta)?;
            if meta["status"]=="completed" {
                return Ok(Some((json!({"recoveryRequired":true,"browser":merge_object(&meta["browser"],&json!({"remoteHandoffError":format!("{error:#}")}))}),None)));
            }
            let mut patch=json!({"recoveryRequired":true,"errorMessage":format!("{error:#}")});
            let model=if meta["status"]=="running" {patch["status"]=json!("error");patch["completedAt"]=json!(timestamp());Some(patch.clone())}else{None};
            Ok(Some((patch,model)))
        });
        return Err(match saved {
            Ok(_) => error,
            Err(persistence) => error.context(format!(
                "Recovery failure persistence also failed: {persistence:#}"
            )),
        }
        .context(format!("Remote recovery receiver session {id} is retained")));
    }
    Ok((id, result?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequential_prompts_survive_receiver_payload_and_producer_preparation() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("receiver"));
        let request=crate::execution::resolve(&json!({"prompt":"PUBLIC remote sequence request","engine":"browser","browserFollowUps":["  PUBLIC second turn "," ","PUBLIC third turn"]}),&json!({}),&Environment::new(),home.path()).unwrap();
        let meta = prepare(
            &store,
            &request,
            home.path(),
            &Connection {
                host: "127.0.0.1:1".into(),
                token: None,
            },
        )
        .unwrap();
        let outgoing = payload(&store, meta["id"].as_str().unwrap()).unwrap();
        assert_eq!(
            outgoing["options"]["followUpPrompts"],
            json!(["PUBLIC second turn", "PUBLIC third turn"])
        );
        let producer = SessionStore::new(home.path().join("producer"));
        let context = crate::remote_host::RunContext::fixture(
            "12345678-1234-1234-1234-123456789abc",
            outgoing,
            producer.clone(),
        );
        let driver = crate::remote_driver::BrowserDriver {
            environment: Environment::new(),
            cwd: home.path().into(),
            browser_config: json!({"manualLogin":true}),
        };
        let id = driver.prepare(&context).unwrap();
        let meta = producer.require(&id).unwrap();
        assert_eq!(
            meta["options"]["browserFollowUps"],
            json!(["PUBLIC second turn", "PUBLIC third turn"])
        );
        assert_eq!(
            browser_runner::request(&producer, &id)
                .unwrap()
                .options_sha256,
            artifacts::sha256(&serde_json::to_vec(&meta["options"]).unwrap())
        );
    }
    #[test]
    fn image_requests_are_refused_by_the_client_and_the_host() {
        let home = tempfile::tempdir().unwrap();
        let receiver = SessionStore::new(home.path().join("receiver"));
        let connection = Connection {
            host: "127.0.0.1:1".into(),
            token: None,
        };
        let image=crate::execution::resolve(&json!({"prompt":"PUBLIC remote image","engine":"browser","generateImage":"PUBLIC-receiver.png"}),&json!({}),&Environment::new(),home.path()).unwrap();
        let error = prepare(&receiver, &image, home.path(), &connection).unwrap_err();
        assert_eq!(
            error.to_string(),
            browser_runner::IMAGE_GENERATION_UNAVAILABLE
        );
        assert!(receiver.list(true).unwrap().is_empty());
        // An older or third-party client may still ask the host for an image.
        let request = crate::execution::resolve(
            &json!({"prompt":"PUBLIC remote image","engine":"browser"}),
            &json!({}),
            &Environment::new(),
            home.path(),
        )
        .unwrap();
        let meta = prepare(&receiver, &request, home.path(), &connection).unwrap();
        let mut outgoing = payload(&receiver, meta["id"].as_str().unwrap()).unwrap();
        outgoing["options"]["imageOutputRequested"] = json!(true);
        let producer = SessionStore::new(home.path().join("producer"));
        let context = crate::remote_host::RunContext::fixture(
            "12345678-1234-1234-1234-123456789abc",
            outgoing,
            producer.clone(),
        );
        let driver = crate::remote_driver::BrowserDriver {
            environment: Environment::new(),
            cwd: home.path().into(),
            browser_config: json!({"manualLogin":true}),
        };
        let error = driver.prepare(&context).unwrap_err();
        assert_eq!(
            error.to_string(),
            browser_runner::IMAGE_GENERATION_UNAVAILABLE
        );
        assert!(producer.list(true).unwrap().is_empty());
    }
    #[test]
    fn connection_resolution_trims_values_and_blanks_do_not_mask_config_or_environment() {
        let env = Environment::from_iter([
            ("ORACLE_REMOTE_HOST".into(), " 127.0.0.1:1234 \n".into()),
            (
                "ORACLE_REMOTE_TOKEN".into(),
                " PUBLIC environment token \t".into(),
            ),
        ]);
        let connection =
            Connection::resolve(&json!({"remoteHost":" \t","remoteToken":"\n"}), &env).unwrap();
        assert_eq!(connection.host, "127.0.0.1:1234");
        assert_eq!(
            connection.token.as_deref(),
            Some("PUBLIC environment token")
        );
        let connection = Connection::resolve(
            &json!({"remoteHost":" localhost:5678 ","remoteToken":" PUBLIC configured token "}),
            &env,
        )
        .unwrap();
        assert_eq!(connection.host, "localhost:5678");
        assert_eq!(connection.token.as_deref(), Some("PUBLIC configured token"));
        assert!(Connection::resolve(&json!({"remoteHost":" "}), &Environment::new()).is_none());
    }
    fn fixture(home: &Path) -> (SessionStore, String, Arc<Receiver>) {
        fixture_connection(
            home,
            &Connection {
                host: "127.0.0.1:1".into(),
                token: None,
            },
        )
    }
    fn fixture_connection(
        home: &Path,
        connection: &Connection,
    ) -> (SessionStore, String, Arc<Receiver>) {
        let store = SessionStore::new(home.join("oracle"));
        let mut request=crate::execution::resolve(&json!({"prompt":"PUBLIC receiver fixture prompt","engine":"browser","model":"gpt-6-pro"}),&json!({}),&Environment::new(),home).unwrap();
        request.browser["remoteToken"] = json!("PUBLIC ephemeral token");
        request.browser["inlineCookies"] = json!([{"name":"PUBLIC synthetic cookie"}]);
        request.options["browserConfig"] = request.browser.clone();
        let meta = prepare(&store, &request, home, connection).unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        let identity = ProcessIdentity::current().unwrap();
        crate::execution::start_owner(&store, &id, &identity, false).unwrap();
        let receiver =
            Receiver::new(&store, &id, &identity, false, &CancellationToken::new()).unwrap();
        (store, id, receiver)
    }
    #[test]
    fn receiver_capture_is_durable_before_ack_and_late_owner_writes_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let (store, id, receiver) = fixture(home.path());
        receiver.hint(json!({"remoteRunId":"12345678-1234-1234-1234-123456789abc","remoteHost":"127.0.0.1:1"})).unwrap();
        receiver
            .receipt(json!({"kind":"file","path":"PUBLIC verified file"}))
            .unwrap();
        receiver.captured(json!({"answerText":"PUBLIC answer","answerMarkdown":"PUBLIC **answer**","artifacts":[],"effectiveModel":"gpt-6-pro","usage":{"outputTokens":5},"elapsedMs":1234})).unwrap();
        let saved = store.require(&id).unwrap();
        assert_eq!(saved["status"], "completed");
        assert_eq!(saved["usage"]["outputTokens"], 5);
        assert_eq!(saved["elapsedMs"], 1234);
        let models = store.model_runs(&id).unwrap();
        assert_eq!(models[0]["status"], "completed");
        assert_eq!(models[0]["response"]["status"], "completed");
        assert_eq!(saved["artifacts"][0]["path"], "PUBLIC verified file");
        let answer = serde_json::from_value(saved["capturedAnswer"].clone()).unwrap();
        assert_eq!(
            artifacts::verify_capture(&store, &answer).unwrap(),
            b"PUBLIC **answer**"
        );
        let wire = serde_json::from_value(saved["browser"]["remoteResult"].clone()).unwrap();
        let wire: Value =
            serde_json::from_slice(&artifacts::verify_capture(&store, &wire).unwrap()).unwrap();
        assert_eq!(wire["answerText"], "PUBLIC answer");
        assert!(
            receiver
                .captured(json!({"answerText":"replacement"}))
                .is_err()
        );
        store
            .update(&id, |_| {
                Ok(Some(json!({"lifecycle":{"workerProcessStartedAtMs":0}})))
            })
            .unwrap();
        assert!(receiver.receipt(json!({"path":"late"})).is_err());
        assert_eq!(
            store.require(&id).unwrap()["artifacts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn preparation_excludes_ephemeral_credentials_and_changed_uploads() {
        let home = tempfile::tempdir().unwrap();
        let (store, id, _) = fixture(home.path());
        let saved = store.require(&id).unwrap();
        assert!(saved["options"]["browserConfig"]["remoteToken"].is_null());
        for file in [
            store.directory(&id).unwrap().join("meta.json"),
            store.directory(&id).unwrap().join("browser-request.json"),
        ] {
            let bytes = std::fs::read_to_string(file).unwrap();
            assert!(
                !bytes.contains("PUBLIC ephemeral token")
                    && !bytes.contains("PUBLIC synthetic cookie")
            );
        }
        let outgoing = payload(&store, &id).unwrap();
        assert!(outgoing["browserConfig"]["remoteHost"].is_null());
        assert!(outgoing["browserConfig"]["inlineCookies"].is_null());
        let staged = StagedUploads::capture_remote(
            &store,
            &id,
            &[json!({"fileName":"public.txt","contentBase64":"UEFCTElD"})],
        )
        .unwrap();
        assert_eq!(
            attachments(Some(&staged), &store, &id).unwrap()[0]["contentBase64"],
            "UEFCTElD"
        );
        std::fs::write(staged.paths().unwrap().first().unwrap(), b"changed").unwrap();
        assert!(attachments(Some(&staged), &store, &id).is_err());
    }
    #[test]
    fn changed_options_run_identity_and_cancelled_admission_cannot_capture() {
        for fault in ["options", "remote", "cancel"] {
            let home = tempfile::tempdir().unwrap();
            let (store, id, receiver) = fixture(home.path());
            receiver.hint(json!({"remoteRunId":"12345678-1234-1234-1234-123456789abc","remoteHost":"127.0.0.1:1"})).unwrap();
            match fault {
                "options" => {
                    store.update(&id,|meta|Ok(Some(json!({"options":merge_object(&meta["options"],&json!({"writeOutputPath":home.path().join("redirected.md")}))})))).unwrap();
                }
                "remote" => {
                    store.update(&id,|meta|Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"runtime":{"remoteRunId":"12345678-1234-1234-1234-123456789abd","remoteHost":"127.0.0.1:1"}}))})))).unwrap();
                }
                _ => receiver.cancel.cancel(),
            }
            assert!(
                receiver
                    .captured(json!({"answerText":"PUBLIC refused answer"}))
                    .is_err(),
                "{fault}"
            );
            assert!(
                !store
                    .directory(&id)
                    .unwrap()
                    .join("remote-result.json")
                    .exists()
            );
            assert!(!home.path().join("redirected.md").exists());
        }
    }
    #[test]
    fn old_hosts_receive_materialised_zip_bytes_and_temporary_bundle_is_removed() {
        use crate::browser_prompt::{BundleFormat, BundleScope, PendingBundle};
        use std::io::Read;
        let home = tempfile::tempdir().unwrap();
        let (store, id, _) = fixture(home.path());
        let source=StagedUploads::capture_remote(&store,&id,&[
            json!({"fileName":"a.rs","displayPath":"public/a.rs","contentBase64":STANDARD.encode(b"PUBLIC source a")}),
            json!({"fileName":"b.rs","displayPath":"public/b.rs","contentBase64":STANDARD.encode(b"PUBLIC source b")}),
        ]).unwrap();
        let mut committed = browser_runner::request(&store, &id).unwrap();
        committed.fallback = Some(browser_runner::BrowserFallback {
            composer_text: "PUBLIC original fallback prompt".into(),
            pending_bundle: Some(PendingBundle {
                format: BundleFormat::Zip,
                scope: BundleScope::All,
            }),
            text_sources: vec![source.clone()],
            raw_source: None,
            remote_source: None,
        });
        crate::storage::atomic_json(
            &store.directory(&id).unwrap().join("browser-request.json"),
            &committed,
        )
        .unwrap();
        let entries = || {
            let mut entries = std::fs::read_dir(store.directory(&id).unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            entries.sort();
            entries
        };
        let before = entries();
        let mut outgoing = payload(&store, &id).unwrap();
        materialise_fallback(&store, &id, &mut outgoing).unwrap();
        assert!(outgoing["fallbackSubmission"]["bundle"].is_null());
        let attachments = outgoing["fallbackSubmission"]["attachments"]
            .as_array()
            .unwrap();
        assert_eq!(attachments.len(), 1);
        assert!(
            attachments[0]["fileName"]
                .as_str()
                .unwrap()
                .ends_with(".zip")
        );
        let bytes = STANDARD
            .decode(attachments[0]["contentBase64"].as_str().unwrap())
            .unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut first = String::new();
        zip.by_name("public/a.rs")
            .unwrap()
            .read_to_string(&mut first)
            .unwrap();
        assert_eq!(first, "PUBLIC source a");
        let mut second = String::new();
        zip.by_name("public/b.rs")
            .unwrap()
            .read_to_string(&mut second)
            .unwrap();
        assert_eq!(second, "PUBLIC source b");
        assert_eq!(entries(), before);
        source.verify(&store, &id).unwrap();
        let target = source.paths().unwrap()[0].clone();
        let mut raced = payload(&store, &id).unwrap();
        let error = materialise_fallback_with(&store, &id, &mut raced, || {
            std::fs::write(&target, b"PUBLIC source X").unwrap()
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differ from their committed source"),
            "{error:#}"
        );
        assert_eq!(entries(), before);
        std::fs::write(target, b"PUBLIC source a").unwrap();
        source.verify(&store, &id).unwrap();
    }
    #[tokio::test]
    async fn failed_remote_dispatch_is_terminal_and_cannot_replay() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let home = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let driver: crate::remote_host::RunDriver = Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { anyhow::bail!("PUBLIC producer failure after admission") })
        });
        let host = crate::remote_host::RemoteHost::start(
            SessionStore::new(home.path().join("producer")),
            crate::remote_host::HostOptions {
                address: "127.0.0.1:0".parse().unwrap(),
                token: "PUBLIC fixture token".into(),
                max_concurrent_runs: Some(1),
                max_queued_runs: Some(0),
                max_browser_tabs: 1,
            },
            driver,
        )
        .await
        .unwrap();
        let connection = Connection {
            host: host.address.to_string(),
            token: Some("PUBLIC fixture token".into()),
        };
        let (store, id, _) = fixture_connection(home.path(), &connection);
        let identity = ProcessIdentity::current().unwrap();
        let cancel = CancellationToken::new();
        let original = store.require(&id).unwrap();
        let mut foreign = identity.clone();
        foreign.native_start += 1;
        foreign.started_at_ms += 1.0;
        store.update(&id,|meta|Ok(Some(json!({"lifecycle":merge_object(&meta["lifecycle"],&json!({"processIdentity":foreign,"workerProcessStartedAtMs":foreign.started_at_ms})),"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"],&json!({"controllerProcessStartedAtMs":foreign.started_at_ms}))}))})))).unwrap();
        let error = execute(&store, &id, &connection, &foreign, &cancel)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("current live native worker"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(store.require(&id).unwrap()["browser"]["remoteDispatch"].is_null());
        store
            .update(&id, |_| {
                Ok(Some(
                    json!({"lifecycle":original["lifecycle"],"browser":original["browser"]}),
                ))
            })
            .unwrap();
        assert!(
            execute(&store, &id, &connection, &identity, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("PUBLIC producer failure")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let saved = store.require(&id).unwrap();
        assert_eq!(saved["status"], "error");
        assert_eq!(saved["recoveryRequired"], true);
        assert!(saved["browser"]["runtime"]["remoteRunId"].is_string());
        assert!(
            execute(&store, &id, &connection, &identity, &cancel)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        host.shutdown().await.unwrap();
    }
}

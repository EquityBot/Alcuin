//! Native browser consultation sequence. Public CLI/MCP admission remains gated pending the ledger.
use crate::{
    artifacts::{self, CaptureReceipt, sha256},
    browser::{self, TerminalGate, TerminalSample},
    browser_chrome::AttachmentEndpoint,
    browser_commit::{self, CommitObservation, CommitReceipt, CommitSnapshot},
    browser_disposition::TargetJournal,
    browser_dom::{self, PreparedPrompt, UploadFile},
    browser_download::{self, DownloadOwner},
    browser_images::ImageDelivery,
    browser_inputs::StagedUploads,
    browser_profile::ProfileRegistry,
    browser_prompt::{BrowserPrompt, PendingBundle},
    browser_renderer::RendererReservation,
    browser_target::{OwnedTarget, TargetAcquisition},
    cdp::Cdp,
    execution::{ExecutionWitness, ResolvedRequest},
    policy,
    process::{Liveness, ProcessIdentity},
    prompts::PromptFile,
    sessions::{self, SessionStore, merge_object, timestamp},
    storage::{DirectoryLock, atomic_json, private_dir},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod grok;
#[cfg(test)]
pub(crate) use grok::tests::native_cases as native_grok_runner_cases;
mod read_recovery;
mod recovery_capture;
pub(crate) mod transcript;
pub(crate) use recovery_capture::recover_child_capture;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRequest {
    pub version: u8,
    pub session_id: String,
    pub reservation_id: String,
    pub prompt: String,
    pub prompt_sha256: String,
    pub options_sha256: String,
    pub upload_paths: Vec<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_uploads: Option<StagedUploads>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<BrowserFallback>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback_used: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserFallback {
    pub composer_text: String,
    pub pending_bundle: Option<PendingBundle>,
    pub text_sources: Vec<StagedUploads>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_source: Option<StagedUploads>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_source: Option<StagedUploads>,
}
impl BrowserRequest {
    /// Recapture every committed source into the child's own native directories.
    pub(crate) fn recapture_into(
        &self,
        store: &SessionStore,
        parent: &str,
        child: &str,
    ) -> Result<Self> {
        anyhow::ensure!(
            self.session_id == parent
                && parent != child
                && serde_json::to_value(request(store, parent)?)? == serde_json::to_value(self)?,
            "Provider child requires the unchanged committed parent request"
        );
        let recapture = |source: &StagedUploads| source.recapture_into(store, parent, child);
        let staged_uploads = self.staged_uploads.as_ref().map(recapture).transpose()?;
        let fallback = self
            .fallback
            .as_ref()
            .map(|fallback| -> Result<BrowserFallback> {
                Ok(BrowserFallback {
                    composer_text: fallback.composer_text.clone(),
                    pending_bundle: fallback.pending_bundle,
                    text_sources: fallback
                        .text_sources
                        .iter()
                        .map(recapture)
                        .collect::<Result<_>>()?,
                    raw_source: fallback.raw_source.as_ref().map(recapture).transpose()?,
                    remote_source: fallback.remote_source.as_ref().map(recapture).transpose()?,
                })
            })
            .transpose()?;
        let mut child_request = self.clone();
        child_request.session_id = child.into();
        child_request.reservation_id = Uuid::new_v4().to_string();
        child_request.options_sha256 =
            sha256(&serde_json::to_vec(&store.require(child)?["options"])?);
        child_request.upload_paths = staged_uploads
            .as_ref()
            .map(StagedUploads::paths)
            .transpose()?
            .unwrap_or_default();
        child_request.staged_uploads = staged_uploads;
        child_request.fallback = fallback;
        if child_request
            .extra
            .get("bundled")
            .is_some_and(Value::is_object)
        {
            let original = self
                .staged_uploads
                .as_ref()
                .context("Bundled request has no committed inputs")?;
            let old = original
                .files
                .iter()
                .filter(|file| file.generated_bundle)
                .collect::<Vec<_>>();
            let new = child_request
                .staged_uploads
                .as_ref()
                .context("Recaptured bundle is missing")?
                .files
                .iter()
                .filter(|file| file.generated_bundle)
                .collect::<Vec<_>>();
            anyhow::ensure!(
                old.len() == 1
                    && new.len() == 1
                    && self.extra["bundled"]["bundlePath"] == old[0].manifest["path"],
                "Committed bundle path does not identify its staged source"
            );
            child_request
                .extra
                .get_mut("bundled")
                .context("Bundle metadata missing")?["bundlePath"] =
                new[0].manifest["path"].clone();
        }
        child_request.extra.insert(
            "providerAttempt".into(),
            store.require(child)?["browser"]["attempt"].clone(),
        );
        anyhow::ensure!(
            serde_json::to_value(request(store, parent)?)? == serde_json::to_value(self)?,
            "Committed parent input changed during child recapture"
        );
        Ok(child_request)
    }
}
fn navigation_destination(config: &Value) -> Result<(String, Value)> {
    anyhow::ensure!(
        config["resumeConversationUrl"].is_null(),
        "Temporary Chat cannot be reopened by URL; exact live-target continuation is not yet qualified"
    );
    let url = crate::browser_privacy::landing_url(
        config["chatgptUrl"]
            .as_str()
            .or(config["url"].as_str())
            .unwrap_or("https://chatgpt.com/"),
    )?;
    let navigation = browser_dom::navigation(&url)?;
    Ok((url, navigation))
}
pub(crate) const DEEP_RESEARCH_UNAVAILABLE: &str = "Deep Research is unavailable: ChatGPT offers it only in ordinary chats, and Industrious runs every ChatGPT request in Temporary Chat so it stays out of your chat history. Nothing was sent. Use --browser-research off or search.";
pub(crate) const IMAGE_GENERATION_UNAVAILABLE: &str = "Image generation is unavailable: ChatGPT does not generate images in Temporary Chat, and Industrious runs every ChatGPT request in Temporary Chat so it stays out of your chat history. Nothing was sent.";
pub fn prepare(store: &SessionStore, request: &ResolvedRequest, cwd: &Path) -> Result<Value> {
    refuse_image_generation(request)?;
    prepare_with_base(store, request, cwd, None)
}
pub(crate) fn prepare_restart(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    source: &str,
) -> Result<Value> {
    refuse_image_generation(request)?;
    prepare_with_base(store, request, cwd, Some(source))
}
/// ChatGPT answers image requests in Temporary Chat with a refusal (checked 27 October 2026). The
/// image delivery path below stays for the fixtures, which prepare through `prepare_with_base`.
fn refuse_image_generation(request: &ResolvedRequest) -> Result<()> {
    anyhow::ensure!(
        crate::browser_images::requested_destination(&request.options)?.is_none(),
        "{IMAGE_GENERATION_UNAVAILABLE}"
    );
    Ok(())
}
fn prepare_with_base(
    store: &SessionStore,
    request: &ResolvedRequest,
    cwd: &Path,
    source: Option<&str>,
) -> Result<Value> {
    anyhow::ensure!(
        request.engine == "browser",
        "Browser runner requires a browser request"
    );
    navigation_destination(&request.browser)?;
    anyhow::ensure!(
        matches!(
            request.browser["desiredModel"].as_str(),
            Some("GPT-6" | "Latest")
        ),
        "Industrious only permits Latest (GPT-6) browser dispatch"
    );
    // Reference selectBrowserComposerModes: a follow-up or borrowed tab only verifies.
    let verify_only = !request.browser["resumeConversationUrl"].is_null()
        || request.browser["browserTabRef"]
            .as_str()
            .is_some_and(|reference| !policy::trim(reference).is_empty());
    if !verify_only {
        anyhow::ensure!(
            request.browser["thinkingTime"]
                .as_str()
                .is_none_or(|level| level == "pro"),
            "a fresh Latest request must try Pro before Extra High"
        );
    }
    if let Some(resume) = request.options.get("browserResumeConversationUrl") {
        anyhow::ensure!(
            *resume == request.browser["resumeConversationUrl"],
            "Browser follow-up navigation disagrees with its saved configuration"
        );
    }
    policy::assert_model_allowed(request.options["model"].as_str())?;
    policy::assert_model_allowed(request.browser["desiredModel"].as_str())?;
    anyhow::ensure!(
        request.browser["thinkingTime"]
            .as_str()
            .is_none_or(|s| matches!(s, "pro" | "extra-high")),
        "Browser admission requires Pro first with only a positively permitted Extra High fallback"
    );
    anyhow::ensure!(
        request.browser["modelStrategy"]
            .as_str()
            .is_none_or(|s| matches!(s, "select" | "current")),
        "Ignoring model verification cannot authorise browser dispatch"
    );
    crate::browser_images::requested_destination(&request.options)?;
    crate::browser_turns::prompts(&request.options)?;
    // ChatGPT offers Deep Research only in ordinary chats (checked 27 October 2026), and
    // every ChatGPT run here must be a verified Temporary Chat.
    anyhow::ensure!(
        request.browser["researchMode"] != "deep",
        "{DEEP_RESEARCH_UNAVAILABLE}"
    );
    anyhow::ensure!(
        request.browser["researchMode"]
            .as_str()
            .is_none_or(|s| matches!(s, "off" | "search" | "deep")),
        "Unsupported browser research mode"
    );
    let base = policy::trim(
        request.options["prompt"]
            .as_str()
            .context("Browser request lacks a prompt")?,
    );
    anyhow::ensure!(
        base.encode_utf16().count() >= 10,
        "Prompt is too short (<10 chars). This was likely accidental; please provide more detail."
    );
    let mut plan = BrowserPrompt::assemble(
        &request.files,
        base,
        request.options["system"].as_str(),
        &request.options,
        &std::env::temp_dir(),
    )?;
    let prompt = plan.composer_text.clone();
    private_dir(store.home())?;
    let lock = DirectoryLock::acquire(
        &store.home().join("dispatch-admission.lock"),
        Duration::from_secs(10),
    )?;
    if source.is_none() && request.options["force"] != true {
        for current in store.list(false)? {
            anyhow::ensure!(
                !matches!(current["status"].as_str(), Some("pending" | "running"))
                    || current["options"]["prompt"] != request.options["prompt"],
                "An identical prompt is already running; reattach to the retained session"
            );
        }
    }
    let meta = store.create(request.options.clone(), cwd, source)?;
    let id = meta["id"]
        .as_str()
        .context("Missing browser session identity")?;
    let staged = if plan.attachments.is_empty() {
        None
    } else {
        match StagedUploads::capture(store, id, &plan.attachments) {
            Ok(staged) => Some(staged),
            Err(error) => {
                store.update(id, |_|Ok(Some(json!({"status":"error","completedAt":timestamp(),"errorMessage":error.to_string(),"recoveryRequired":false}))))?;
                return Err(error);
            }
        }
    };
    let uploads = staged
        .as_ref()
        .map(StagedUploads::paths)
        .transpose()?
        .unwrap_or_default();
    if let Some(bundle) = &mut plan.bundled {
        bundle.bundle_path = staged
            .as_ref()
            .and_then(|staged| staged.files.iter().find(|file| file.generated_bundle))
            .and_then(|file| file.manifest["path"].as_str())
            .map(std::path::PathBuf::from)
            .context("Generated bundle lacks a durable staged path")?;
    }
    let committed = BrowserRequest {
        version: 1,
        session_id: id.into(),
        reservation_id: Uuid::new_v4().to_string(),
        prompt_sha256: sha256(prompt.as_bytes()),
        prompt,
        options_sha256: sha256(&serde_json::to_vec(&meta["options"])?),
        upload_paths: uploads,
        staged_uploads: staged,
        // Every file uploads, so local runs carry no deferred fallback.
        fallback: None,
        fallback_used: false,
        extra: serde_json::Map::from_iter([
            ("attachmentMode".into(), json!(plan.attachment_mode)),
            ("inlineFileCount".into(), json!(plan.inline_file_count)),
            (
                "attachmentDisplayPaths".into(),
                json!(
                    plan.attachments
                        .iter()
                        .map(|attachment| &attachment.display_path)
                        .collect::<Vec<_>>()
                ),
            ),
            ("bundled".into(), serde_json::to_value(&plan.bundled)?),
            (
                "estimatedInputTokens".into(),
                json!(plan.estimated_input_tokens),
            ),
        ]),
    };
    atomic_json(
        &store.directory(id)?.join("browser-request.json"),
        &committed,
    )?;
    plan.cleanup()?;
    lock.release()?;
    Ok(meta)
}
pub(crate) fn request(store: &SessionStore, id: &str) -> Result<BrowserRequest> {
    let committed: BrowserRequest = serde_json::from_slice(&fs::read(
        store.directory(id)?.join("browser-request.json"),
    )?)?;
    let meta = store.require(id)?;
    anyhow::ensure!(
        committed.version == 1
            && committed.session_id == id
            && committed.prompt_sha256 == sha256(committed.prompt.as_bytes())
            && committed.options_sha256 == sha256(&serde_json::to_vec(&meta["options"])?),
        "Browser committed source or options changed; refusing dispatch"
    );
    policy::assert_model_allowed(meta["options"]["model"].as_str())?;
    if let Some(staged) = &committed.staged_uploads {
        anyhow::ensure!(
            staged.paths()? == committed.upload_paths,
            "Browser upload paths differ from their saved receipts"
        );
    }
    Ok(committed)
}
pub fn start_owner(store: &SessionStore, id: &str, identity: &ProcessIdentity) -> Result<Value> {
    anyhow::ensure!(
        identity.liveness() == Liveness::Alive,
        "Browser worker native identity is unavailable"
    );
    let committed = request(store, id)?;
    if crate::browser_attempt::is_child(&store.require(id)?, &committed) {
        let link = crate::browser_attempt::require_child(store, id, &CancellationToken::new())?;
        anyhow::ensure!(
            store.require(&link.parent_id)?["lifecycle"]["processIdentity"] == json!(identity),
            "Provider child requires its original parent worker"
        );
    }
    let model = store.require(id)?["model"]
        .as_str()
        .context("Browser session has no model")?
        .to_owned();
    store.update_session_and_model(id,&model, |current,_| {
        crate::browser_attempt::require_if_child(store,id,&CancellationToken::new())?;
        anyhow::ensure!(current["mode"] == "browser" && current["status"] == "pending" && current.get("dispatch").is_none(), "Browser session already started; refusing replay");
        let browser = merge_object(&current["browser"], &json!({"runtime":merge_object(&current["browser"]["runtime"], &json!({"controllerPid":identity.pid,"controllerProcessStartedAtMs":identity.started_at_ms}))}));
        Ok(Some((json!({"status":"running","startedAt":timestamp(),"browser":browser,"lifecycle":{"engine":"browser","execution":"foreground","attached":true,"detached":false,"workerPid":identity.pid,"workerProcessStartedAtMs":identity.started_at_ms,"processIdentity":identity}}),Some(json!({"status":"running","startedAt":timestamp()})))))
    })
}
fn patch(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    changes: Value,
) -> Result<Value> {
    let model = store.require(id)?["model"]
        .as_str()
        .context("Browser model record is unavailable")?
        .to_owned();
    store.update_session_and_model(id, &model, |current, run| {
        witness.require(current)?;
        anyhow::ensure!(
            current["status"] == "running",
            "Browser session is terminal or changed owners; refusing late writes"
        );
        let model_patch = if sessions::terminal(changes["status"].as_str().unwrap_or("")) {
            anyhow::ensure!(
                run.is_none_or(|run| !sessions::terminal(run["status"].as_str().unwrap_or(""))),
                "Browser model is terminal; refusing late writes"
            );
            Some(terminal_model_patch(&changes))
        } else {
            None
        };
        Ok(Some((changes, model_patch)))
    })
}
fn terminal_model_patch(changes: &Value) -> Value {
    let mut model = json!({"status":changes["status"],"completedAt":changes["completedAt"]});
    if let Some(error) = changes.get("errorMessage") {
        model["errorMessage"] = error.clone();
    }
    model
}
fn fail_owned(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    decide: impl FnOnce(&Value) -> Value,
) -> Result<Value> {
    // Do not reacquire a write lock merely to discover that durable completion is terminal.
    // The transaction repeats these checks if another controller changes the record meanwhile.
    let saved = store.require(id)?;
    if witness.require(&saved).is_err()
        || sessions::terminal(saved["status"].as_str().unwrap_or(""))
    {
        return Ok(saved);
    }
    let model = saved["model"]
        .as_str()
        .context("Browser model record is unavailable")?
        .to_owned();
    store.update_session_and_model(id, &model, |meta, run| {
        if witness.require(meta).is_err()
            || sessions::terminal(meta["status"].as_str().unwrap_or(""))
        {
            return Ok(None);
        }
        let changes = decide(meta);
        let model_patch = run
            .is_none_or(|run| !sessions::terminal(run["status"].as_str().unwrap_or("")))
            .then(|| terminal_model_patch(&changes));
        Ok(Some((changes, model_patch)))
    })
}
/// Reference connection-lost error: a DevTools transport that closed under the run is classified
/// by the liveness probe and reported in the reference wording. The run is never replayed.
#[derive(Debug)]
struct ConnectionLost {
    cause: crate::browser::DisconnectCause,
    remote: bool,
}
impl std::fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.cause.user_message(self.remote))
    }
}
impl ConnectionLost {
    fn record(error: &anyhow::Error) -> Option<Value> {
        let lost = error.downcast_ref::<Self>()?;
        let recoverable = lost.cause == crate::browser::DisconnectCause::CdpClientDisconnect;
        Some(
            json!({"category":"browser-automation","message":lost.to_string(),"details":{"stage":"connection-lost","recoverableDisconnect":recoverable,"disconnectCause":lost.cause.as_str()}}),
        )
    }
}
async fn classify_connection_loss(
    store: &SessionStore,
    id: &str,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    cancel: &CancellationToken,
    result: Result<Value>,
) -> Result<Value> {
    let error = match result {
        Err(error) if target.browser().is_closed() && !cancel.is_cancelled() => error,
        other => return other,
    };
    let cause =
        crate::browser::probe_disconnect(&endpoint.host, endpoint.port, target.id(), cancel).await;
    if cancel.is_cancelled() {
        return Err(error);
    }
    let line = if cause == crate::browser::DisconnectCause::CdpClientDisconnect {
        "CDP client disconnected; Chrome/target still reachable. Leaving run recoverable for reattach.".to_owned()
    } else {
        format!(
            "Chrome connection lost ({}); attempting to abort run.",
            cause.as_str()
        )
    };
    if let Err(log) = automation_log(store, id, &line) {
        eprintln!("Browser log was not recorded: {log:#}");
    }
    let config = store
        .require(id)
        .map(|meta| meta["browser"]["config"].clone())
        .unwrap_or_default();
    let remote = config["remoteChrome"].is_object() || config["attachRunning"] == true;
    Err(error.context(ConnectionLost { cause, remote }))
}
fn runtime_patch(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    runtime: Value,
) -> Result<Value> {
    store.update(id, |current| {
        witness.require(current)?;
        anyhow::ensure!(current["status"] == "running", "Browser session changed owners");
        Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"runtime":merge_object(&current["browser"]["runtime"], &runtime)}))})))
    })
}
fn copied_cleanup_receipt(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    settled: &Value,
    removed: bool,
) -> Result<Value> {
    store.update_until(id, std::time::Instant::now() + Duration::from_secs(1), |current| {
        witness.require(current)?;
        anyhow::ensure!(current == settled, "Browser outcome changed during copied-profile cleanup");
        // Cleanup runs after completion too. It records only disposal, preserving the
        // exact observed outcome and refusing a different controller or later result.
        Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"runtime":merge_object(&current["browser"]["runtime"], &json!({"copiedProfileCleanup":{"version":1,"removed":removed},"recoveryUnavailable":if removed{"copied-profile-removed"}else{"copied-profile-cleanup-unconfirmed"}}))}))})))
    })
}
fn fresh_profile_receipt(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    settled: &Value,
    disposal: Value,
) -> Result<Value> {
    store.update_until(id, std::time::Instant::now() + Duration::from_secs(1), |current| {
        witness.require(current)?;
        anyhow::ensure!(current == settled, "Browser outcome changed during fresh-profile cleanup");
        // Records disposition only; the observed outcome and recovery anchors stay unchanged.
        Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"runtime":fresh_disposal_runtime(current,&disposal)}))})))
    })
}
fn fresh_disposal_runtime(current: &Value, disposal: &Value) -> Value {
    let mut patch = json!({"freshProfileDisposal":disposal});
    if current["browser"]["config"]["copyProfileSource"].is_string() {
        let removed = disposal["disposition"] == "removed";
        patch["copiedProfileCleanup"] = json!({"version":1,"removed":removed});
        patch["recoveryUnavailable"] = json!(if removed {
            "copied-profile-removed"
        } else {
            "copied-profile-cleanup-unconfirmed"
        });
    }
    merge_object(&current["browser"]["runtime"], &patch)
}
/// Settles fresh Chrome deferred by remote runs and records each disposition.
pub(crate) async fn settle_deferred_fresh(store: &SessionStore, session: Option<&str>) {
    // A named session is settled after a remote acknowledgement, which a host's 5 s
    // shutdown drain bounds, so it gets no exit grace. Service shutdown waits.
    let grace = session.is_none();
    let mut handled = std::collections::BTreeSet::new();
    for (id, mut connection) in crate::browser_chrome::take_deferred_fresh(session) {
        handled.insert(id.clone());
        if settle_completed_idle(store, &id, &mut connection, grace).await {
            let _ = connection
                .browser
                .disconnect_and_wait(Duration::from_secs(1))
                .await;
            continue;
        }
        if settle_completed_fresh(store, &id, &mut connection).await {
            let _ = connection
                .browser
                .disconnect_and_wait(Duration::from_secs(1))
                .await;
            continue;
        }
        let disposal = match connection.settle_fresh(Duration::from_secs(5)).await {
            Ok(()) => json!({"version":1,"disposition":"removed"}),
            Err(error) => json!({"version":1,"disposition":"retained","reason":error.to_string()}),
        };
        if let Err(error) = connection
            .browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await
        {
            eprintln!("Deferred Chrome connection cleanup remains pending: {error:#}");
        }
        // The session is already terminal; this records cleanup only.
        if let Err(error) = store.update_until(&id, std::time::Instant::now() + Duration::from_secs(1), |current| {
            Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"runtime":fresh_disposal_runtime(current,&disposal)}))})))
        }) {
            eprintln!("Fresh Chrome disposal receipt was not recorded: {error:#}");
        }
        settle_saved_fresh(store, &id).await;
    }
    if let Some(id) = session {
        // A restarted worker has no in-memory deferred connection. Its exact saved
        // launch capability must still be eligible after remote acknowledgement.
        if !handled.contains(id) {
            settle_saved_idle_now(store, id).await;
            settle_saved_fresh(store, id).await;
        }
        settle_hinted_idle(store, id, false).await;
    }
}
/// A borrowed run's retired answer tab may have been the last page holding its
/// launching session's idle browser open. Retry that launcher through the profile
/// hint; settle_saved_idle verifies every identity again before anything closes.
pub(crate) async fn settle_hinted_idle(store: &SessionStore, id: &str, grace: bool) {
    let Ok(saved) = store.stored_for_update(id) else {
        return;
    };
    let Some(path) =
        saved["browser"]["runtime"]["ownedRecoveryTarget"]["creator"]["lease"]["profileDir"]
            .as_str()
    else {
        return;
    };
    settle_profile_idle(store, Path::new(path), Some(id), grace).await;
}
/// Retries the idle launcher named by a profile's hint, other than `except`.
pub(crate) async fn settle_profile_idle(
    store: &SessionStore,
    profile: &Path,
    except: Option<&str>,
    grace: bool,
) {
    if let Some(owner) = ProfileRegistry::open_existing(profile)
        .ok()
        .and_then(|profile| profile.idle_owner())
        .filter(|owner| Some(owner.as_str()) != except)
    {
        if grace {
            settle_saved_idle(store, &owner).await;
        } else {
            settle_saved_idle_now(store, &owner).await;
        }
    }
}
/// Execution has released or handed off its admission; only this run's own marker
/// would otherwise refuse the idle retries below.
pub(crate) async fn settle_borrowed_run(
    store: &SessionStore,
    id: &str,
    connection: &mut crate::browser_chrome::ChromeConnection,
    borrowed_browser: bool,
    succeeded: bool,
) {
    connection.release_attachment();
    if borrowed_browser
        && succeeded
        && let Err(error) = crate::browser_retirement::reconcile_after_run(
            store,
            &connection.browser,
            &connection.endpoint,
        )
        .await
    {
        eprintln!("Completed browser disposal remains pending: {error:#}");
    }
    // This run borrowed a persistent browser and holds no admission now. Retry its
    // launching session's idle cleanup, which a lease or a missed attempt may have refused.
    // A failed run gets the retry too: any page, lease or marker it left still refuses
    // the launcher's cleanup, and a later run or recovery could settle it the same way.
    if connection.borrowed()
        && let Some(owner) = connection.idle_owner_hint().filter(|owner| owner != id)
    {
        // A failed run records its failure next; it gets one attempt without the grace.
        if succeeded {
            settle_saved_idle(store, &owner).await;
        } else {
            settle_saved_idle_now(store, &owner).await;
        }
    }
}
/// An idle persistent launch is a separate lifetime from disposable profile
/// deletion. Deferred provider attempts get here only after parent completion.
async fn settle_completed_idle(
    store: &SessionStore,
    id: &str,
    connection: &mut crate::browser_chrome::ChromeConnection,
    grace: bool,
) -> bool {
    if !connection.has_idle_startup() {
        return false;
    }
    if store
        .stored_for_update(id)
        .is_ok_and(|saved| saved["browser"]["runtime"]["persistentBrowserOwner"].is_object())
    {
        if let Err(error) = connection.retain_process() {
            eprintln!("Persistent child reaping handoff remains pending: {error:#}");
            return true;
        }
        if grace {
            settle_saved_idle(store, id).await;
        } else {
            settle_saved_idle_now(store, id).await;
        }
        return true;
    }
    let saved = store.stored_for_update(id);
    let outcome = async {
        let saved = saved
            .as_ref()
            .map_err(|error| anyhow::anyhow!("Idle browser session is unavailable: {error:#}"))?;
        anyhow::ensure!(
            saved["status"] == "completed"
                && saved["recoveryRequired"] == false
                && saved["browser"]["runtime"]["targetDispositionPending"] == false
                && saved["browser"]["runtime"]["targetRetiredAt"].is_string(),
            "Persistent browser still has an incomplete outcome or recovery work"
        );
        connection.settle_idle(Duration::from_secs(5)).await
    }
    .await;
    let disposal = match outcome {
        Ok(()) => {
            json!({"version":1,"disposition":"exited","profileRetained":true,"naturalExit":true})
        }
        Err(error) => {
            let _ = connection.retain_process();
            eprintln!("Persistent browser retained: {error:#}");
            json!({"version":1,"disposition":"retained","profileRetained":true,"reason":error.to_string()})
        }
    };
    if let Ok(saved)=saved
        && let Err(error)=store.update_until(id,std::time::Instant::now()+Duration::from_secs(1),|current| {
            anyhow::ensure!(*current==saved,"Persistent browser outcome changed during idle cleanup");
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"persistentBrowserDisposal":disposal}))}))})))
        }) {eprintln!("Persistent browser disposal receipt remains pending: {error:#}");}
    true
}
/// How long a closed idle Chrome may take to finish exiting before the one retry.
const IDLE_EXIT_GRACE: Duration = Duration::from_secs(10);
/// A failed close preserves its launch and descendant evidence for the next
/// acknowledgement or recovery cleanup. It cannot change the captured result.
pub(crate) async fn settle_saved_idle(store: &SessionStore, id: &str) {
    // Natural exit can outlast the first budget. Without a later run nothing else
    // would retry, so this process waits once and retries; that attempt repeats every
    // check. The retention receipt is already durable, so an interrupted wait leaves
    // later runs and recovery their ordinary retry.
    if let Some(pending) = settle_saved_idle_now(store, id).await {
        pending.wait(IDLE_EXIT_GRACE).await;
        settle_saved_idle_now(store, id).await;
    }
}
#[cfg(test)]
thread_local! {
    /// Runs after this thread's next Grok receipt persistence and before its renderer
    /// acknowledgement, as a worker crash.
    pub(crate) static TEST_BEFORE_GROK_ACKNOWLEDGE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}
#[cfg(test)]
thread_local! {
    /// Runs after this thread's next parent-attempt capture and before its clean release,
    /// as a worker crash.
    pub(crate) static TEST_BEFORE_ATTEMPT_RELEASE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}
#[cfg(test)]
thread_local! {
    /// Runs between this thread's next idle attempt and its receipt, as a concurrent retry.
    pub(crate) static TEST_BEFORE_IDLE_RECEIPT: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}
/// One attempt with no grace wait, for paths that must not delay a run or its
/// failure receipt. Returns the processes still exiting when closing had begun.
pub(crate) async fn settle_saved_idle_now(
    store: &SessionStore,
    id: &str,
) -> Option<crate::browser_chrome::ExitPending> {
    let Ok(saved) = store.stored_for_update(id) else {
        return None;
    };
    let owner = &saved["browser"]["runtime"]["persistentBrowserOwner"];
    if !owner.is_object()
        || saved["browser"]["runtime"]["persistentBrowserDisposal"]["disposition"]
            .as_str()
            .is_some_and(|disposition| disposition.starts_with("exited"))
        || saved["status"] != "completed"
        || saved["browser"]["runtime"]["targetDispositionPending"] != false
        || saved["recoveryRequired"] != false
    {
        return None;
    }
    let (outcome, pending) =
        match crate::browser_chrome::retire_saved_idle(store, id, Duration::from_secs(5)).await {
            Ok(Some(value)) => (Ok(value), None),
            Ok(None) => return None,
            Err(error) => {
                eprintln!("Persistent Chrome disposal remains pending: {error:#}");
                (
                    Err(format!("{error:#}")),
                    crate::browser_chrome::ExitPending::of(&error),
                )
            }
        };
    #[cfg(test)]
    if let Some(concurrent) = TEST_BEFORE_IDLE_RECEIPT.take() {
        concurrent();
    }
    if let Err(error)=store.update_until(id,std::time::Instant::now()+Duration::from_secs(1),|current|{
        anyhow::ensure!(current["status"]=="completed" && current["browser"]["runtime"]["persistentBrowserOwner"]==*owner,
            "Persistent disposal receipt owner changed");
        // A concurrent retry may already have proved exit; a losing attempt's refusal
        // must not replace that receipt with a retention claim.
        if outcome.is_err() && current["browser"]["runtime"]["persistentBrowserDisposal"]["disposition"]
            .as_str().is_some_and(|disposition| disposition.starts_with("exited")) {return Ok(None);}
        let disposal=match &outcome {Ok(value)=>value.clone(),Err(reason)=>merge_object(&current["browser"]["runtime"]["persistentBrowserDisposal"],
            &json!({"version":1,"disposition":"retained","profileRetained":true,"reason":reason}))};
        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"persistentBrowserDisposal":disposal}))}))})))
    }) {eprintln!("Persistent browser disposal receipt remains pending: {error:#}");}
    pending
}
/// Completed launches always use durable native exit evidence and the admission
/// marker. The earlier child-kill path cannot recover a partly deleted profile.
pub(crate) async fn settle_completed_fresh(
    store: &SessionStore,
    id: &str,
    connection: &mut crate::browser_chrome::ChromeConnection,
) -> bool {
    if !connection.is_fresh() {
        return false;
    }
    let saved = match store.stored_for_update(id).and_then(|saved| {
        anyhow::ensure!(
            saved["id"] == id && saved["status"].is_string(),
            "Fresh disposal session record is missing or malformed"
        );
        Ok(saved)
    }) {
        Ok(saved) => saved,
        Err(error) => {
            // Unreadable completion evidence cannot authorise the older deletion path.
            eprintln!("Fresh Chrome disposal awaits its session record: {error:#}");
            if let Err(error) = connection.retain_process() {
                eprintln!("Fresh Chrome retention remains pending: {error:#}");
            }
            return true;
        }
    };
    if saved["status"] != "completed" {
        return false;
    }
    if !saved["browser"]["runtime"]["freshProfileOwner"].is_object() {
        eprintln!("Fresh Chrome retained: completed session has no saved disposal capability");
        if let Err(error) = connection.retain_process() {
            eprintln!("Fresh Chrome retention remains pending: {error:#}");
        }
        return true;
    }
    if let Err(error) = connection.release_fresh_for_saved_disposal() {
        eprintln!("Fresh Chrome handoff remains pending: {error:#}");
        return true;
    }
    settle_saved_fresh(store, id).await;
    true
}
/// Disposal is a separate receipt: a failed retry never changes the captured answer.
pub(crate) async fn settle_saved_fresh(store: &SessionStore, id: &str) {
    let Ok(saved) = store.stored_for_update(id) else {
        return;
    };
    let owner = &saved["browser"]["runtime"]["freshProfileOwner"];
    if !owner.is_object()
        || saved["status"] != "completed"
        || saved["browser"]["runtime"]["targetDispositionPending"] != false
        || saved["recoveryRequired"] == true
    {
        return;
    }
    let outcome =
        match crate::browser_chrome::retire_saved_fresh(store, id, Duration::from_secs(5)).await {
            Ok(Some(outcome)) => Ok(outcome),
            Ok(None) => return,
            Err(error) => {
                eprintln!("Fresh Chrome disposal remains pending: {error:#}");
                Err(error.to_string())
            }
        };
    if let Err(error) = store.update_until(id, std::time::Instant::now() + Duration::from_secs(1), |current| {
        anyhow::ensure!(current["status"] == "completed"
            && current["browser"]["runtime"]["freshProfileOwner"] == *owner,
            "Fresh disposal receipt owner changed");
        // A close attempt may already have saved its native descendant identities.
        // Merge errors under this lock so a later retry keeps that exit evidence.
        let disposal = match &outcome {
            Ok(value) => value.clone(),
            Err(reason) => merge_object(&current["browser"]["runtime"]["freshProfileDisposal"],
                &json!({"version":1,"disposition":"retained","reason":reason})),
        };
        Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"runtime":fresh_disposal_runtime(current,&disposal)}))})))
    }) { eprintln!("Fresh Chrome disposal receipt remains pending: {error:#}"); }
}
fn recovery_identity(metadata: &Value) -> Value {
    let mut identity = json!({});
    for key in [
        "chromePid",
        "chromeHost",
        "chromePort",
        "chromeBrowserWSEndpoint",
        "browserWSEndpoint",
        "chromeTargetId",
        "conversationId",
        "submittedPromptHash",
        "submittedPromptHashFormat",
        "ownedRecoveryTarget",
        "borrowedTarget",
    ] {
        if let Some(value) = metadata["browser"]["runtime"].get(key) {
            identity[key] = value.clone();
        }
    }
    if metadata["browser"]["runtime"]["provider"] == "grok" {
        for key in [
            "provider",
            "grokCommitReceipt",
            "effectiveModel",
            "modelSelection",
            "thinkingSelection",
        ] {
            identity[key] = metadata["browser"]["runtime"][key].clone();
        }
    }
    identity
}
/// Saved authority over the target a run submits to. A created target carries its creator
/// record (absent on older creatorless records); a borrowed user tab (`--browser-tab`) carries
/// only a `borrowedTarget` record and never gains recovery, retirement or close authority.
#[derive(Clone, Copy)]
enum TargetAuthority<'a> {
    Created(Option<&'a Value>),
    Borrowed,
}
impl TargetAuthority<'_> {
    fn holds(self, runtime: &Value, target: &str) -> bool {
        match self {
            Self::Created(creator) => runtime["ownedRecoveryTarget"].get("creator") == creator,
            Self::Borrowed => {
                runtime
                    .get("ownedRecoveryTarget")
                    .is_none_or(Value::is_null)
                    && runtime["borrowedTarget"]["targetId"] == target
            }
        }
    }
    fn record(self) -> &'static str {
        match self {
            Self::Created(_) => "ownedRecoveryTarget",
            Self::Borrowed => "borrowedTarget",
        }
    }
}
struct OutcomeWitness {
    recovery: Value,
    options_sha256: String,
    dispatch: Value,
}
fn dispatch_identity(metadata: &Value) -> Value {
    let mut identity = json!({});
    for key in [
        "state",
        "reservationId",
        "submissionOwner",
        "promptSha256",
        "providerDispatchId",
        "startedAt",
        "turnIndex",
        "history",
        "turnCapture",
    ] {
        if let Some(value) = metadata["dispatch"].get(key) {
            identity[key] = value.clone();
        }
    }
    identity
}
impl OutcomeWitness {
    fn capture(
        metadata: &Value,
        target: &OwnedTarget,
        endpoint: &AttachmentEndpoint,
        receipt: &CommitReceipt,
        options_sha256: &str,
        authority: TargetAuthority<'_>,
    ) -> Result<Self> {
        let runtime = &metadata["browser"]["runtime"];
        anyhow::ensure!(
            authority.holds(runtime, target.id()),
            "Browser target creator changed before outcome observation"
        );
        let exact = match authority {
            TargetAuthority::Created(_) => browser::exact_owned_target(
                metadata,
                &browser::RecoveryTarget {
                    host: endpoint.host.clone(),
                    port: endpoint.port,
                    target_id: target.id().into(),
                    browser_ws_endpoint: Some(endpoint.browser_ws_endpoint.clone()),
                    claim_id: receipt.owner.claim_id.clone(),
                    conversation_id: receipt.conversation.conversation_id.clone(),
                },
            ),
            TargetAuthority::Borrowed => {
                let borrowed = &runtime["borrowedTarget"];
                borrowed["host"] == endpoint.host
                    && borrowed["port"] == endpoint.port
                    && borrowed["claimId"] == receipt.owner.claim_id
                    && borrowed["conversationId"] == receipt.conversation.conversation_id
            }
        };
        anyhow::ensure!(
            exact && runtime["chromeTargetId"] == target.id(),
            "Browser outcome has no exact saved recovery target"
        );
        Ok(Self {
            recovery: recovery_identity(metadata),
            options_sha256: options_sha256.into(),
            dispatch: dispatch_identity(metadata),
        })
    }
    fn require(
        &self,
        metadata: &Value,
        worker: &ExecutionWitness,
        model: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.require_identity(metadata, worker)?;
        let model = model.or_else(|| {
            metadata["models"]
                .as_array()
                .and_then(|runs| runs.iter().find(|run| run["model"] == metadata["model"]))
        });
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Browser completion was cancelled before commitment"
        );
        anyhow::ensure!(
            metadata["status"] == "running"
                && model.is_none_or(|run| !matches!(
                    run["status"].as_str(),
                    Some("completed" | "partial" | "cancelled")
                )),
            "Browser result arrived after terminal state or owner change"
        );
        Ok(())
    }
    fn require_identity(&self, metadata: &Value, worker: &ExecutionWitness) -> Result<()> {
        worker.require(metadata)?;
        anyhow::ensure!(
            recovery_identity(metadata) == self.recovery,
            "Browser recovery target changed; refusing late completion"
        );
        anyhow::ensure!(
            dispatch_identity(metadata) == self.dispatch,
            "Browser dispatch changed; refusing late completion"
        );
        anyhow::ensure!(
            sha256(&serde_json::to_vec(&metadata["options"])?) == self.options_sha256,
            "Browser committed options changed; refusing late completion"
        );
        Ok(())
    }
    fn download_settled(
        &self,
        store: &SessionStore,
        id: &str,
        worker: &ExecutionWitness,
    ) -> Result<Value> {
        store.update(id,|current| {
            self.require_identity(current,worker)?;
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"downloadContextSettled":true}))}))})))
        })
    }
    fn update(
        &self,
        store: &SessionStore,
        id: &str,
        worker: &ExecutionWitness,
        cancel: &CancellationToken,
        decide: impl FnOnce(&Value) -> Result<Option<Value>>,
    ) -> Result<Value> {
        let model = store.require(id)?["model"]
            .as_str()
            .context("Browser model record is unavailable")?
            .to_owned();
        store.update_session_and_model(id, &model, |meta, run| {
            self.require(meta, worker, run, cancel)?;
            Ok(decide(meta)?.map(|patch| (patch, None)))
        })
    }
    /// Appends a log line only while this outcome still owns the session.
    fn log(
        &self,
        store: &SessionStore,
        id: &str,
        worker: &ExecutionWitness,
        cancel: &CancellationToken,
        text: &str,
    ) -> Result<()> {
        let model = store.require(id)?["model"]
            .as_str()
            .context("Browser model record is unavailable")?
            .to_owned();
        store.append_log_guarded(id, Some(&model), text, |meta, run| {
            self.require(meta, worker, run, cancel)
        })
    }
    fn runtime(
        &self,
        store: &SessionStore,
        id: &str,
        worker: &ExecutionWitness,
        cancel: &CancellationToken,
        changes: Value,
    ) -> Result<Value> {
        self.update(store, id, worker, cancel, |current| {
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&changes)}))})))
        })
    }
}
/// Reference automation logger: a provider attempt writes to its consultation's user-visible
/// log, and only lines the reference prints for the run's verbosity are kept.
pub(crate) fn automation_log(store: &SessionStore, id: &str, line: &str) -> Result<()> {
    let meta = store.require(id)?;
    let meta = match meta["browser"]["attempt"]["parentId"].as_str() {
        Some(parent) => store.require(parent)?,
        None => meta,
    };
    if crate::browser_log::prints(&meta, line) {
        let id = meta["id"].as_str().context("Browser session ID missing")?;
        store.append_log(id, meta["model"].as_str(), &format!("{line}\n"))?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)] // The log block names the evidence the outcome will record.
fn complete(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &OutcomeWitness,
    changes: impl FnOnce(&Value) -> Result<Value>,
    evidence: &Value,
    text: &str,
    cancel: &CancellationToken,
) -> Result<Value> {
    let current = store.require(id)?;
    outcome.require(&current, witness, None, cancel)?;
    let model = current["model"]
        .as_str()
        .context("Browser model record is unavailable")?;
    let view = merge_object(
        &current,
        &json!({"browser":merge_object(&current["browser"],evidence)}),
    );
    let committed = request(store, id)?;
    let child = crate::browser_attempt::is_child(&current, &committed);
    if !child {
        save_transcript(store, id, text, cancel, |meta, run| {
            outcome.require(meta, witness, run, cancel)
        })?;
    }
    let mut warnings = current["browser"]["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let added = crate::browser_log::completion_warnings(&view, &committed);
    warnings.extend(added.iter().cloned());
    let mut block = crate::browser_log::completion(&view, &committed, &warnings, text);
    let (archive, line) = crate::browser_log::archive(&view);
    if !child {
        block.insert_str(0, &line);
    }
    store.append_log_guarded(id, Some(model), &block, |meta, run| {
        outcome.require(meta, witness, run, cancel)
    })?;
    store.update_session_and_model(id, model, |meta, run| {
        outcome.require(meta, witness, run, cancel)?;
        let mut response = merge_object(
            &run.map_or(Value::Null, |run| run["response"].clone()),
            &json!({"status":"completed"}),
        );
        response
            .as_object_mut()
            .context("Invalid browser response metadata")?
            .remove("incompleteReason");
        let mut changes = changes(meta)?;
        // A provider child's warnings and archive record belong to its parent's completion.
        if !child {
            let browser = if changes["browser"].is_object() {
                &changes["browser"]
            } else {
                &meta["browser"]
            };
            let mut browser = if added.is_empty() {
                browser.clone()
            } else {
                crate::browser_log::with_warnings(browser, &added)
            };
            browser["archive"] = archive;
            changes["browser"] = browser;
        }
        Ok(Some((
            changes,
            Some(json!({"status":"completed","completedAt":timestamp(),"response":response})),
        )))
    })
}
#[allow(clippy::too_many_arguments)] // Capture and its receipt share the exact outcome transaction.
fn capture_outcome(
    store: &SessionStore,
    id: &str,
    worker: &ExecutionWitness,
    outcome: &OutcomeWitness,
    text: &str,
    observed: Option<&Value>,
    cancel: &CancellationToken,
) -> Result<CaptureReceipt> {
    let mut receipt = None;
    outcome.update(store, id, worker, cancel, |current| {
        let captured = artifacts::capture(store,id,&format!("captured-answer-{}.md",Uuid::new_v4()),text.as_bytes(),cancel)?;
        let mut runtime=json!({"capturedAnswer":captured});
        if let Some(observed)=observed {
            runtime["acceptedObservation"]=observed.clone();
            runtime["answerSourceCapture"]=json!(captured);
        }
        receipt=Some(captured.clone());
        Ok(Some(json!({"capturedAnswer":captured,"browser":merge_object(&current["browser"],&json!({"capturedAnswer":captured,"runtime":merge_object(&current["browser"]["runtime"],&runtime)}))})))
    })?;
    receipt.context("Browser capture did not produce a receipt")
}
/// Reference saveOptionalArtifact around a direct session's transcript: a failed artifact is
/// logged, never fatal. Consultation parents save theirs at adoption.
fn save_transcript(
    store: &SessionStore,
    id: &str,
    text: &str,
    cancel: &CancellationToken,
    guard: impl Fn(&Value, Option<&Value>) -> Result<()>,
) -> Result<()> {
    let current = store.require(id)?;
    let model = current["model"]
        .as_str()
        .context("Browser model record is unavailable")?;
    let prompt = request(store, id)?.prompt;
    let mut failure = None;
    store.update_session_and_model(id, model, |current, run| {
        guard(current, run)?;
        if !transcript::wanted(current) {
            return Ok(None);
        }
        match transcript::save(store, id, current, &prompt, text, cancel) {
            Ok(saved) if saved.is_empty() => Ok(None),
            Ok(saved) => Ok(Some((json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!(saved),&Value::Null)}), None))),
            Err(error) if !cancel.is_cancelled() => {
                failure = Some(error);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    })?;
    if let Some(error) = failure {
        store.append_log(
            id,
            Some(model),
            &format!("[browser] Failed to save session artifact: {error:#}\n"),
        )?;
    }
    Ok(())
}
fn publish_answer(
    store: &SessionStore,
    id: &str,
    worker: &ExecutionWitness,
    outcome: &OutcomeWitness,
    receipt: &CaptureReceipt,
    cancel: &CancellationToken,
) -> Result<()> {
    let published=outcome.update(store,id,worker,cancel,|current| {
        let Some(output)=current["options"]["writeOutputPath"].as_str() else {return Ok(None)};
        let destination=Path::new(current["cwd"].as_str().context("Output has no saved working directory")?).join(output);
        let answer=artifacts::verify_capture(store,receipt)?;
        if current["browser"]["requiredTextOutput"].is_object() {
            let bytes=crate::browser_output::required_answer_bytes(&current["browser"]["requiredTextOutput"],current,&answer)?;
            crate::browser_output::reuse_answer(&current["browser"]["requiredTextOutput"],&destination,&bytes,cancel)?;
            return Ok(None);
        }
        let required=crate::browser_output::save_answer(store,&destination,&answer,&crate::browser_output::answer_heading(current),cancel)?;
        let additions=json!([{"kind":"output","path":required["file"],"bytes":required["bytes"],"sha256":required["sha256"]}]);
        Ok(Some(json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&additions,&Value::Null),"browser":merge_object(&current["browser"],&json!({"requiredTextOutput":required}))})))
    })?;
    if published["options"]["writeOutputPath"].is_string()
        && let Some(file) = published["browser"]["requiredTextOutput"]["file"].as_str()
    {
        // Reference sessionRunner logs the saved answer before copying file artifacts.
        outcome.log(
            store,
            id,
            worker,
            cancel,
            &format!("Saved assistant output to {file}\n"),
        )?;
    }
    let mut exported = vec![];
    outcome.update(store,id,worker,cancel,|current| {
        if current["options"]["writeArtifacts"]!=true {return Ok(None);}
        let saved=current["artifacts"].as_array().into_iter().flatten().filter(|artifact|artifact["kind"]=="file").cloned().collect::<Vec<_>>();
        let output=current["browser"]["requiredTextOutput"]["file"].as_str().map(Path::new);
        let exports=crate::browser_output::export_files(output,&saved,cancel)?;
        if exports.is_empty(){return Ok(None);}
        let patch=json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!(exports),&Value::Null)});
        exported=exports;
        Ok(Some(patch))
    })?;
    for artifact in exported {
        let line = format!(
            "[browser] Saved write-output file artifact to {} sha256={}\n",
            artifact["path"].as_str().unwrap_or_default(),
            artifact["sha256"].as_str().unwrap_or_default()
        );
        outcome.log(store, id, worker, cancel, &line)?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)] // Reuse binds the old capture and output to fresh renderer evidence.
fn reuse_published_capture(
    store: &SessionStore,
    id: &str,
    worker: &ExecutionWitness,
    outcome: &OutcomeWitness,
    saved: &Value,
    observed: &Value,
    cancel: &CancellationToken,
) -> Result<CaptureReceipt> {
    let accepted = &saved["browser"]["runtime"]["acceptedObservation"];
    let report = accepted["kind"] == "deep-research-report";
    if report {
        crate::browser_research::require_report(saved, accepted)?;
        crate::browser_research::require_report(saved, observed)?;
        anyhow::ensure!(
            accepted["report"] == observed["report"],
            "Published research report identity changed"
        );
    } else {
        browser::require_original_source_pair(saved, accepted)?;
        browser::require_original_source_pair(saved, observed)?;
    }
    let mut fields = vec!["lastUserMessageId", "html", "images", "files"];
    if !report {
        fields.extend(["lastAssistantMessageId", "lastAssistantTurnId"]);
    }
    for field in fields {
        anyhow::ensure!(
            !accepted[field].is_null() && accepted[field] == observed[field],
            "Published answer source, formatting or artifacts changed before recovery"
        );
    }
    anyhow::ensure!(
        accepted.get("providerText").unwrap_or(&accepted["text"])
            == observed.get("providerText").unwrap_or(&observed["text"]),
        "Published assistant content changed before recovery"
    );
    let capture: CaptureReceipt =
        serde_json::from_value(saved["browser"]["capturedAnswer"].clone())?;
    anyhow::ensure!(
        capture.session_id == id
            && saved["capturedAnswer"] == json!(capture)
            && saved["browser"]["runtime"]["capturedAnswer"] == json!(capture),
        "Saved recovery capture is not bound to its original session"
    );
    let bytes = artifacts::verify_capture(store, &capture)?;
    if saved["browser"]["requiredTextOutput"].is_object() {
        let destination = Path::new(
            saved["cwd"]
                .as_str()
                .context("Saved output working directory missing")?,
        )
        .join(
            saved["options"]["writeOutputPath"]
                .as_str()
                .context("Saved output requested path missing")?,
        );
        crate::browser_output::reuse_answer(
            &saved["browser"]["requiredTextOutput"],
            &destination,
            &crate::browser_output::required_answer_bytes(
                &saved["browser"]["requiredTextOutput"],
                saved,
                &bytes,
            )?,
            cancel,
        )?;
    } else {
        let source = captured_source_text(store, saved)?;
        let complete = image_answer_text(&source, &saved["browser"]["runtime"]["imageDelivery"])?;
        anyhow::ensure!(
            bytes == source.as_bytes() || bytes == complete.as_bytes(),
            "Saved recovery capture differs from its verified source and delivery summary"
        );
    }
    outcome.update(store,id,worker,cancel,|current| {
        anyhow::ensure!(current["browser"]["capturedAnswer"]==saved["browser"]["capturedAnswer"] && current["browser"]["requiredTextOutput"]==saved["browser"]["requiredTextOutput"] && current["browser"]["runtime"]["acceptedObservation"]==*accepted,"Published capture changed before recovery receipt update");
        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"acceptedObservation":observed}))}))})))
    })?;
    Ok(capture)
}

fn captured_source_text(store: &SessionStore, metadata: &Value) -> Result<String> {
    let source: CaptureReceipt =
        serde_json::from_value(metadata["browser"]["runtime"]["answerSourceCapture"].clone())
            .context("Saved answer has no original Markdown capture")?;
    anyhow::ensure!(
        metadata["id"] == source.session_id,
        "Saved source capture belongs to another session"
    );
    String::from_utf8(artifacts::verify_capture(store, &source)?)
        .context("Saved source capture is not UTF-8")
}

fn image_answer_text(source: &str, image: &Value) -> Result<String> {
    let count = image["imageCount"]
        .as_u64()
        .map(usize::try_from)
        .transpose()?
        .unwrap_or(0);
    if count == 0 {
        return Ok(source.to_owned());
    }
    let suffix = image["markdownSuffix"]
        .as_str()
        .context("Generated images have no delivery summary")?;
    Ok(format!(
        "{}{suffix}",
        if image["buttonDownloads"] == true {
            source.to_owned()
        } else {
            browser::normalise_image_answer(source, count)
        }
    ))
}
fn matches_saved_file(saved: &Value, candidate: &Value, target: &str, observed: &Value) -> bool {
    saved["kind"] == "file"
        && saved["sourceUrl"] == candidate["url"]
        && saved["origin"]["targetId"] == target
        && saved["origin"]["assistantMessageId"] == observed["lastAssistantMessageId"]
        && saved["origin"]["assistantTurnId"] == observed["lastAssistantTurnId"]
        && (candidate["url"] != "browser-download"
            || candidate["candidateId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
                && saved["candidateId"] == candidate["candidateId"]
                && saved["filename"] == candidate["filename"])
}
/// Reference browser logger: progress lines reach the consultation's session log. A provider
/// attempt writes to its parent, whose log the caller reads. Logging never changes a run.
pub(crate) fn progress(store: &SessionStore, id: &str, line: &str) {
    let written = (|| -> Result<()> {
        let mut meta = store.require(id)?;
        if let Some(parent) = meta["browser"]["attempt"]["parentId"].as_str() {
            meta = store.require(parent)?;
        }
        let session = meta["id"].as_str().context("Session has no identity")?;
        store.append_log(session, meta["model"].as_str(), &format!("{line}\n"))
    })();
    if let Err(error) = written {
        eprintln!("{line} (session log write failed: {error:#})");
    }
}
fn flush_notes(store: &SessionStore, id: &str, reservation: &mut RendererReservation) {
    for line in reservation.take_notes() {
        progress(store, id, &line);
    }
}
#[derive(Clone, Debug)]
pub struct RunnerLimits {
    pub command: Duration,
    /// Slot admission waits the run's `timeoutMs` (reference `acquireBrowserTabLease`).
    pub admission: Duration,
    /// Profile locks (bring-up, automation) wait `profileLockTimeoutMs`.
    pub profile_lock: Duration,
    pub commit: Duration,
    pub answer: Duration,
    pub poll: Duration,
    pub stable_ms: u64,
    pub confirm_cycles: u32,
    /// Reference BROWSER_BRINGUP_LOCK_TIMEOUT_MS: fixed, independent of profileLockTimeoutMs.
    pub bringup: Duration,
}
/// Reference readPositiveIntEnv: a finite positive number, floored; anything else is the default.
fn positive_env(environment: &crate::config::Environment, name: &str, fallback: u64) -> u64 {
    let raw = environment.get(name).map_or("", |s| policy::trim(s));
    match if raw.is_empty() {
        Ok(0.0)
    } else {
        raw.parse::<f64>()
    } {
        Ok(value) if value.is_finite() && value >= 1.0 => value.floor().min(u32::MAX as f64) as u64,
        Ok(value) if value.is_finite() && value > 0.0 => 0,
        _ => fallback,
    }
}
/// Terminal gate calibration (reference ORACLE_BAR_CONFIRM_CYCLES and ORACLE_TERMINAL_MIN_STABLE_MS).
pub(crate) fn gate_tunables(environment: &crate::config::Environment) -> (u32, u64) {
    let defaults = RunnerLimits::default();
    (
        positive_env(
            environment,
            "ORACLE_BAR_CONFIRM_CYCLES",
            u64::from(defaults.confirm_cycles),
        ) as u32,
        positive_env(
            environment,
            "ORACLE_TERMINAL_MIN_STABLE_MS",
            defaults.stable_ms,
        ),
    )
}
pub(crate) fn answer_timeout(config: &Value) -> Duration {
    Duration::from_millis(config["timeoutMs"].as_u64().unwrap_or_else(|| {
        if config["researchMode"] == "deep" {
            2_400_000
        } else {
            1_200_000
        }
    }))
}
pub(crate) type CaptureHook = Arc<dyn Fn(Value) -> Result<()> + Send + Sync>;
enum CaptureBoundary {
    Direct,
    Remote(CaptureHook),
    ParentAttempt,
}
impl CaptureBoundary {
    fn deferred(&self) -> bool {
        !matches!(self, Self::Direct)
    }
}
impl From<Option<CaptureHook>> for CaptureBoundary {
    fn from(value: Option<CaptureHook>) -> Self {
        value.map_or(Self::Direct, Self::Remote)
    }
}
/// Native worker entry. Unsupported profile/tool routes remain explicit preflight gates.
pub async fn run_native(
    store: &SessionStore,
    id: &str,
    environment: &crate::config::Environment,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
) -> Result<Value> {
    run_native_with_capture(store, id, environment, identity, cancel, None).await
}
pub(crate) async fn run_native_authenticated(
    store: &SessionStore,
    id: &str,
    environment: &crate::config::Environment,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
) -> Result<Value> {
    // Keep the native browser state machine out of its caller's Windows stack frame.
    Box::pin(run_native_consultation(
        store,
        id,
        environment,
        identity,
        cancel,
        None,
        bootstrap,
    ))
    .await
}
pub(crate) async fn run_native_with_capture(
    store: &SessionStore,
    id: &str,
    environment: &crate::config::Environment,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    capture: Option<CaptureHook>,
) -> Result<Value> {
    Box::pin(run_native_consultation(
        store,
        id,
        environment,
        identity,
        cancel,
        capture,
        None,
    ))
    .await
}
/// Own the public session separately from the provider's dispatch and recovery capability.
async fn run_native_consultation(
    store: &SessionStore,
    id: &str,
    environment: &crate::config::Environment,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    capture: Option<CaptureHook>,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
) -> Result<Value> {
    let initial = store.require(id)?;
    let committed = request(store, id)?;
    anyhow::ensure!(
        !crate::browser_attempt::is_child(&initial, &committed),
        "Provider children execute only through their original consultation"
    );
    // The remote client owns a captured run's header (reference runBrowserSessionExecution).
    if capture.is_none()
        && let Some(header) = crate::browser_log::header(&initial, &committed)
    {
        store.append_log(id, initial["model"].as_str(), &header)?;
    }
    // A user-selected existing target keeps its existing borrowed-target contract.
    if initial["browser"]["config"]["browserTabRef"]
        .as_str()
        .is_some_and(|reference| !policy::trim(reference).is_empty())
    {
        return Box::pin(run_native_prepared(
            store,
            id,
            environment,
            identity,
            cancel,
            capture.into(),
            bootstrap,
        ))
        .await;
    }
    let witness = ExecutionWitness::capture(store, id, identity)?;
    let result=async {
        let mut parent=crate::browser_attempt::Parent::begin(store,id,identity,cancel)?;
        let first=parent.reserve_first(cancel)?;
        let first_child=first.child_id.clone();
        start_owner(store,&first.child_id,identity)?;
        let attempt=match Box::pin(run_native_prepared(store,&first.child_id,environment,identity,cancel,CaptureBoundary::ParentAttempt,bootstrap)).await {
            Ok(_) => first,
            Err(error) => {
                let retained=format!("Provider attempt {} failed; its original target and recovery evidence are retained",first.child_id);
                // Remote runs never switch: their capture belongs to the remote client.
                let switched=if capture.is_none() {
                    parent.switch_to_grok(&first,&error,environment,cancel)
                        .with_context(||format!("{retained} ({error:#}); the Grok provider switch did not start"))?
                } else {
                    None
                };
                let Some(grok)=switched else {
                    return Err(error.context(retained));
                };
                eprintln!("[browser] ChatGPT attempt {} failed after bounded original-target recovery; consulting grok.com once in its separate Private profile. The ChatGPT records and target are retained.",first.child_id);
                let ran=match start_owner(store,&grok.child_id,identity) {
                    Ok(_) => Box::pin(run_native_prepared(store,&grok.child_id,environment,identity,cancel,CaptureBoundary::ParentAttempt,None)).await,
                    Err(failed) => Err(failed),
                };
                if let Err(failed)=ran {
                    // Only a Grok attempt that never sent hands the consultation back to ChatGPT.
                    return Err(match parent.revert_unsent_switch("grok-not-sent") {
                        Ok(()) => anyhow::anyhow!("ChatGPT failed and Grok did not submit (the prompt may remain in its composer): {failed:#}. Recover ChatGPT attempt {} ({error:#}) read-only through this consultation; Grok attempt {} is retained as evidence",first.child_id,grok.child_id),
                        Err(sent) => anyhow::anyhow!("ChatGPT failed and the Grok website fallback could not finish: {failed:#}. Grok attempt {} may have received the prompt ({sent:#}); it and ChatGPT attempt {} ({error:#}) are retained",grok.child_id,first.child_id),
                    });
                }
                grok
            }
        };
        parent.adopt(&attempt.child_id,cancel)?;
        if attempt.provider==crate::browser_attempt::Provider::Grok {
            let mode=store.require(&attempt.child_id)?["browser"]["runtime"]["modelSelection"]["selectedMode"].as_str().unwrap_or("unknown").to_owned();
            eprintln!("[browser] Answer adopted from Grok {mode} (verified Private Chat) after the provider switch; ChatGPT attempt {} keeps its retained target.",first_child);
        }
        if let Some(hook)=capture {
            parent.issue_capture(&hook,cancel)?;
            return store.require(id);
        }
        let completed=parent.complete(cancel)?;
        let text=String::from_utf8(captured(store,id)?)?;
        crate::notify::completed(&completed,&text).await;
        if let Err(error)=crate::browser_attempt::settle_selected_saved(store,id,cancel).await {
            // A durable parent answer survives an unconfirmed exact-child cleanup.
            eprintln!("Selected child cleanup remains pending: {error:#}");
            store.update(id,|current| {
                witness.require(current)?;
                anyhow::ensure!(current["status"]=="completed","Consultation changed before cleanup warning");
                Ok(Some(json!({"recoveryRequired":true,"cleanupWarning":format!("Selected child {} cleanup remains unconfirmed",attempt.child_id)})))
            })?;
        }
        store.require(id)
    }.await;
    if let Err(error) = &result {
        fail_owned(store, id, &witness, |current| {
            json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},
            "completedAt":timestamp(),"errorMessage":format!("{error:#}"),
            "recoveryRequired":current["recoveryRequired"]==true || crate::browser_attempt::needs_recovery(store,current)})
        })?;
    }
    result
}
async fn run_native_prepared(
    store: &SessionStore,
    id: &str,
    environment: &crate::config::Environment,
    identity: &ProcessIdentity,
    cancel: &CancellationToken,
    capture: CaptureBoundary,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
) -> Result<Value> {
    let witness = ExecutionWitness::capture(store, id, identity)?;
    // Browser phase timing (reference phaseTiming); a no-op unless --perf-trace is active.
    crate::perf::phase("setup", Some(0));
    let result = async {
        request(store, id)?;
        let meta = store.require(id)?;
        crate::browser_attempt::require_if_child(store,id,cancel)?;
        let config = &meta["browser"]["config"];
        let grok=meta["browser"]["attempt"]["provider"] == "grok";
        if grok {
            let link=crate::browser_attempt::require_child(store,id,cancel)?;
            anyhow::ensure!(link.provider == crate::browser_attempt::Provider::Grok && bootstrap.is_none(), "Grok cannot inherit ChatGPT cookie bootstrap");
            crate::browser_grok::require_text_request(&request(store,id)?,&meta["options"])?;
            crate::browser_grok::require_distinct(config,&store.require(&link.parent_id)?["browser"]["config"],environment,
                Path::new(meta["cwd"].as_str().context("Grok working directory is unavailable")?))?;
        }
        anyhow::ensure!(
            match bootstrap {
                Some(batch) => batch.requirement() == config["cookieBootstrapRequired"],
                None => config["cookieBootstrapRequired"].is_null(),
            },
            "Saved browser intent requires fresh cookie bootstrap input before dispatch"
        );
        if bootstrap.is_some() {
            anyhow::ensure!(
                config["attachRunning"] != true && !config["remoteChrome"].is_object(),
                "Explicit cookie bootstrap cannot seed a borrowed browser attachment"
            );
        }
        let copying = config["copyProfileSource"].is_string();
        anyhow::ensure!(
            bootstrap.is_some()
                || copying
                || config["manualLogin"] != true
                || config["cookieSync"] != true,
            "Ordinary Chrome cookie extraction never seeds the persistent manual-login profile; disable manual login or supply explicit cookies"
        );
        let cwd = Path::new(
            meta["cwd"]
                .as_str()
                .context("Browser worker working directory is unavailable")?,
        );
        // A fresh read for this run only; it never reaches saved intent or another process.
        let extracted = if bootstrap.is_none() && crate::browser_auth::ordinary_sync(config) {
            progress(store, id, crate::browser_auth::CHROME_COOKIE_SYNC_WARNING);
            Some(
                Box::pin(crate::browser_auth::read_ordinary_logged(
                    config,
                    environment,
                    cwd,
                    cancel,
                    &|line| progress(store, id, line),
                ))
                .await?,
            )
        } else {
            None
        };
        let bootstrap = bootstrap.or(extracted.as_ref());
        for key in ["cookieSync", "manualLoginCookieSync"] {
            anyhow::ensure!(
                config[key] != true || bootstrap.is_some() || copying,
                "Requested browser {key} lifecycle remains gated"
            );
        }
        anyhow::ensure!(
            config.get("inlineCookies").is_none_or(Value::is_null),
            "Requested browser inlineCookies route remains gated"
        );
        let (confirm_cycles, stable_ms) = gate_tunables(environment);
        let limits = RunnerLimits {
            confirm_cycles,
            stable_ms,
            ..RunnerLimits::from_config(config)
        };
        let configured = if grok {
            crate::browser_chrome::ChromeConnection::configured_grok(config,environment,cwd,limits.command,cancel).await?
        } else {
            crate::browser_chrome::ChromeConnection::configured(config,environment,cwd,limits.command,cancel).await?
        };
        let profile = configured.profile;
        let mut connection = configured.connection;
        anyhow::ensure!(
            bootstrap.is_none() || connection.owns_process(),
            "Explicit cookie bootstrap requires a newly owned Chrome process"
        );
        // Reference runBrowserMode hooks: a copied profile is never kept. Its remote-capture
        // exception belongs to serve, which keeps its own shutdown signals and no hooks.
        let termination = {
            let (store, session) = (store.clone(), id.to_owned());
            crate::browser_signal::register(crate::browser_signal::Hook::new(
                config["keepBrowser"] == true && !copying,
                config["manualLogin"] == true,
                copying,
                connection.signal_target(),
                Box::new(move |line| {
                    eprintln!("{line}");
                    progress(&store, &session, line);
                }),
            ))
        };
        for line in &configured.notes {
            progress(store, id, line);
        }
        if let Some(path) = configured.source_profile {
            runtime_patch(store, id, &witness, json!({"chromeProfileRoot":path}))?;
        }
        // Connections and persistent processes are separate lifetimes. No borrowed process or
        // unretired target is killed on worker exit, cancellation or result publication failure.
        let result = async {
        if capture.deferred() {
            connection.defer_copy_retirement(limits.command.min(Duration::from_secs(2))).await?;
        }
        if let Some(owner) = connection.fresh_disposal_owner(id, identity)? {
            runtime_patch(store, id, &witness, json!({"freshProfileOwner":owner}))?;
        }
        if let Some(owner) = connection.idle_disposal_owner(id, identity)? {
            runtime_patch(store, id, &witness, json!({"persistentBrowserOwner":owner}))?;
            connection.record_idle_owner(id);
        }
        execute_with_boundary(
            store,
            id,
            profile,
            connection.browser.clone(),
            connection.endpoint.clone(),
            identity,
            &limits,
            cancel,
            &capture,
            bootstrap,
            connection.take_bootstrap_lock(),
            connection.take_bootstrap_admission(),
        )
        .await
        }.await;
        if result.is_ok() {
            termination.complete();
        }
        if capture.deferred()
            && result.is_ok()
            && (connection.is_fresh() || connection.has_idle_startup())
            && config["keepBrowser"] != true
        {
            // A remote answer tab is retired only after client acknowledgement; dispose then.
            crate::browser_chrome::defer_fresh(id, connection);
            return result;
        }
        let fresh_settled = connection
            .is_fresh()
            .then(|| store.stored_for_update(id));
        let borrowed_browser = !connection.is_fresh();
        let durable_fresh = settle_completed_fresh(store, id, &mut connection).await;
        let fresh = if durable_fresh { Ok(()) } else { connection.settle_fresh(Duration::from_secs(5)).await };
        if let Some(settled) = fresh_settled.filter(|_| !durable_fresh) {
            let disposal = match &fresh {
                Ok(()) if config["keepBrowser"] == true => json!({"version":1,"disposition":"kept"}),
                Ok(()) => json!({"version":1,"disposition":"removed"}),
                Err(error) => {
                    json!({"version":1,"disposition":"retained","reason":error.to_string()})
                }
            };
            // A cleanup receipt never changes the run's own result.
            if let Err(error) = settled
                .and_then(|settled| fresh_profile_receipt(store, id, &witness, &settled, disposal))
            {
                eprintln!("Fresh Chrome disposal receipt was not recorded: {error:#}");
            }
        }
        if let Err(error) = &fresh {
            eprintln!("Fresh Chrome retained with its profile: {error:#}");
            settle_saved_fresh(store, id).await;
        }
        settle_borrowed_run(store, id, &mut connection, borrowed_browser, result.is_ok()).await;
        settle_completed_idle(store,id,&mut connection,true).await;
        // Reconciliation can update this run's retirement record. Capture that
        // outcome before disposal so its cleanup receipt is not already stale.
        let settled = connection.is_copied().then(|| store.stored_for_update(id));
        let disconnected = connection
            .browser
            .disconnect_and_wait(Duration::from_secs(1))
            .await;
        let disposed = connection.dispose_copy().await;
        if let Some(settled) = settled
            && let Err(error) = settled.and_then(|settled| {
                copied_cleanup_receipt(store, id, &witness, &settled, matches!(disposed, Ok(true)))
            })
        {
            eprintln!("Copied Chrome disposal receipt was not recorded: {error:#}");
        }
        // Connection/profile cleanup records its own failure; it never replaces the run's result.
        if let Err(error) = disconnected {
            eprintln!("Chrome connection cleanup remains pending: {error:#}");
        }
        if let Err(error) = disposed {
            eprintln!("Copied Chrome disposal remains pending: {error:#}");
        }
        result
    }
    .await;
    crate::perf::phase_end(u8::from(result.is_ok()));
    if let Err(error) = &result {
        fail_owned(
            store,
            id,
            &witness,
            |meta| json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"recoveryRequired":meta["recoveryRequired"]==true||meta.get("dispatch").is_some()||meta["browser"]["runtime"].get("chromeTargetId").is_some()}),
        )?;
    }
    result
}
impl RunnerLimits {
    fn from_config(config: &Value) -> Self {
        let command = Duration::from_millis(config["inputTimeoutMs"].as_u64().unwrap_or(60_000));
        Self {
            command,
            admission: answer_timeout(config),
            profile_lock: Duration::from_millis(
                config["profileLockTimeoutMs"].as_u64().unwrap_or(300_000),
            ),
            // Reference promptComposer: the receipt waits max(60 s, inputTimeoutMs).
            commit: command.max(Duration::from_secs(60)),
            answer: answer_timeout(config),
            ..Self::default()
        }
    }
}
impl Default for RunnerLimits {
    fn default() -> Self {
        Self {
            command: Duration::from_secs(60),
            admission: Duration::from_secs(1_200),
            profile_lock: Duration::from_secs(300),
            commit: Duration::from_secs(60),
            answer: Duration::from_secs(1_200),
            poll: Duration::from_millis(250),
            stable_ms: 1_200,
            confirm_cycles: 3,
            bringup: Duration::from_secs(600),
        }
    }
}
#[allow(clippy::too_many_arguments)] // These independent ownership witnesses bind one dispatch.
pub async fn execute(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    identity: &ProcessIdentity,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Value> {
    execute_with_capture(
        store, id, profile, browser, endpoint, identity, limits, cancel, None,
    )
    .await
}
#[allow(clippy::too_many_arguments)] // Remote capture adds a durable delivery boundary to the same witnesses.
pub(crate) async fn execute_with_capture(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    identity: &ProcessIdentity,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    capture: Option<&CaptureHook>,
) -> Result<Value> {
    execute_with_bootstrap(
        store, id, profile, browser, endpoint, identity, limits, cancel, capture, None, None, None,
    )
    .await
}
#[allow(clippy::too_many_arguments)] // Authentication remains runtime-only beside the existing ownership witnesses.
async fn execute_with_bootstrap(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    identity: &ProcessIdentity,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    capture: Option<&CaptureHook>,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
    bringup: Option<DirectoryLock>,
    startup: Option<DirectoryLock>,
) -> Result<Value> {
    execute_with_boundary(
        store,
        id,
        profile,
        browser,
        endpoint,
        identity,
        limits,
        cancel,
        &capture.cloned().into(),
        bootstrap,
        bringup,
        startup,
    )
    .await
}
#[allow(clippy::too_many_arguments)] // Capture policy is distinct from target and authentication ownership.
async fn execute_with_boundary(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    identity: &ProcessIdentity,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    capture: &CaptureBoundary,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
    bringup: Option<DirectoryLock>,
    startup: Option<DirectoryLock>,
) -> Result<Value> {
    anyhow::ensure!(
        *identity == ProcessIdentity::current()? && identity.liveness() == Liveness::Alive,
        "Browser execution requires the exact current native worker"
    );
    endpoint.validate()?;
    let committed = request(store, id)?;
    if let Some(staged) = &committed.staged_uploads {
        staged.verify(store, id)?;
    }
    let witness = ExecutionWitness::capture(store, id, identity)?;
    let current = store.require(id)?;
    witness.require(&current)?;
    crate::browser_attempt::require_if_child(store, id, cancel)?;
    anyhow::ensure!(
        !crate::browser_attempt::is_child(&current, &committed)
            || current["browser"]["config"]["browserTabRef"]
                .as_str()
                .is_none_or(|reference| policy::trim(reference).is_empty()),
        "Provider attempts require their own new target"
    );
    anyhow::ensure!(
        current["status"] == "running"
            && current.get("dispatch").is_none()
            && current["browser"]["runtime"]
                .get("ownedRecoveryTarget")
                .is_none(),
        "Browser dispatch or recovery already exists; refusing automatic replay"
    );
    for line in crate::browser_prompt::control_plan_lines(&current["browser"]["config"], "browser")
    {
        progress(store, id, &line);
    }
    if cancel.is_cancelled() {
        patch(
            store,
            id,
            &witness,
            json!({"status":"cancelled","completedAt":timestamp(),"recoveryRequired":false}),
        )?;
        bail!("Browser cancelled before allocation");
    }
    if startup.is_none() {
        crate::browser_retirement::reconcile(store, &browser, &endpoint).await?;
    }
    crate::perf::phase("queue", Some(0));
    // A locally launched Chrome's refused loopback port proves its records' targets are gone.
    let local_chrome = !current["browser"]["config"]["remoteChrome"].is_object()
        && current["browser"]["config"]["attachRunning"] != true;
    let admission = profile
        .acquire_admission(
            id,
            crate::browser_health::tab_limit(&current["browser"]["config"]["maxConcurrentTabs"]),
            limits.admission,
            cancel,
            local_chrome,
        )
        .await;
    let lease = match admission {
        Ok(lease) => Arc::new(lease),
        Err(error) => {
            fail_owned(store,id,&witness,|current|json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"errorPhase":"browser-admission","recoveryRequired":current["recoveryRequired"]==true||error.is::<crate::browser_profile::UnsettledAdmission>()}))
                .map_err(|persistence|anyhow::anyhow!("{error:#}; admission failure persistence failed: {persistence:#}"))?;
            return Err(error);
        }
    };
    let _heartbeat = lease.heartbeat();
    crate::perf::phase("setup", None);
    let setup: Result<DirectoryLock> = async {
        lease.update(&json!({"chromeHost":endpoint.host,"chromePort":endpoint.port}))?;
        let lock = match bringup {
            Some(lock) => Ok(lock),
            None => {
                profile_lock(
                    &profile.path().join("oracle-browser-bringup.lock"),
                    limits.bringup,
                    cancel,
                )
                .await
            }
        }?;
        crate::browser_attempt::require_if_child(store, id, cancel)?;
        Ok(lock)
    }
    .await;
    let bringup = match setup {
        Ok(lock) => lock,
        Err(error) => {
            let cleanup = lease
                .release_unissued(std::time::Instant::now() + Duration::from_secs(1))
                .await;
            fail_owned(store,id,&witness,|current| {
                let mut runtime=current["browser"]["runtime"].clone();
                if let Err(cleanup)=&cleanup {
                    runtime["admissionCleanupWarning"]=json!(cleanup.to_string());
                    runtime["tabLeaseId"]=json!(lease.id());
                    runtime["profileDir"]=json!(profile.path());
                }
                json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"errorPhase":"browser-pre-allocation","recoveryRequired":current["recoveryRequired"]==true||cleanup.is_err(),"browser":merge_object(&current["browser"],&json!({"runtime":runtime}))})
            }).map_err(|persistence|anyhow::anyhow!("{error:#}; pre-allocation failure persistence failed: {persistence:#}"))?;
            return Err(error);
        }
    };
    let borrowed = current["browser"]["config"]["browserTabRef"]
        .as_str()
        .map(policy::trim)
        .filter(|reference| !reference.is_empty())
        .map(str::to_owned);
    if let Some(reference) = borrowed {
        anyhow::ensure!(
            !capture.deferred(),
            "Remote --browser-tab runs remain gated in the Rust runner"
        );
        anyhow::ensure!(
            bootstrap.is_none(),
            "Explicit cookie bootstrap cannot seed a borrowed browser tab"
        );
        let attached = async {
            // An exact target ID, URL or conversation ID resolves on this run's own connection;
            // `current` and title matches need the inspected tab list (reference resolveChatGptTab).
            let listed = crate::browser_tabs::targets(&browser, limits.command, cancel).await?;
            let exact = listed.into_iter().find(|row| {
                row["targetId"] == reference.as_str()
                    || row["url"] == reference.as_str()
                    || row["url"]
                        .as_str()
                        .and_then(crate::browser::conversation_id)
                        .as_deref()
                        == Some(reference.as_str())
            });
            let tab = match exact {
                Some(tab) => tab,
                None => {
                    crate::browser_tabs::resolve_target(
                        &endpoint,
                        Some(&reference),
                        false,
                        limits.command,
                        cancel,
                    )
                    .await?
                }
            };
            let target_id = tab["targetId"]
                .as_str()
                .filter(|id| crate::browser_target::valid_id(id))
                .context("The selected ChatGPT tab has no valid target identity")?;
            OwnedTarget::recover(
                browser,
                lease.clone(),
                target_id.into(),
                limits.command,
                cancel,
            )
            .await
        }
        .await;
        let mut target = match attached {
            Ok(target) => target,
            Err(error) => {
                let released = lease.release();
                fail_owned(store,id,&witness,|current| json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"errorPhase":"browser-tab-selection","recoveryRequired":current["recoveryRequired"]==true||released.is_err()}))
                    .map_err(|persistence|anyhow::anyhow!("{error:#}; tab selection failure persistence failed: {persistence:#}"))?;
                return Err(error);
            }
        };
        let result = run_borrowed(
            store,
            id,
            &witness,
            &committed,
            &mut target,
            &endpoint,
            bringup,
            limits,
            cancel,
            startup,
        )
        .await;
        let result = classify_connection_loss(store, id, &target, &endpoint, cancel, result).await;
        if let Err(error) = &result {
            let saved = fail_owned(store, id, &witness, |current| {
                let dispatch = current
                    .get("dispatch")
                    .map(|v| merge_object(v, &json!({"state":"uncertain"})))
                    .unwrap_or_else(
                        || json!({"state":"not-started","reservationId":committed.reservation_id}),
                    );
                // A borrowed renderer may need manual cleanup even when no send started.
                let mut failed = json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"recoveryRequired":current["recoveryRequired"]==true||current.get("dispatch").is_some(),"dispatch":dispatch});
                if let Some(lost) = ConnectionLost::record(error) {
                    failed["error"] = lost;
                }
                failed
            });
            if let Err(persistence) = saved {
                return result.map_err(|original| {
                    anyhow::anyhow!("{original:#}; error state persistence failed: {persistence:#}")
                });
            }
        }
        return result;
    }
    let acquisition = TargetAcquisition::default();
    let mut target = match acquisition
        .acquire(browser, lease.clone(), cancel, limits.command)
        .await
    {
        Ok(t) => t,
        Err(error) => {
            patch(
                store,
                id,
                &witness,
                json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"recoveryRequired":true,"errorMessage":error.to_string(),"browser":merge_object(&current["browser"], &json!({"runtime":merge_object(&current["browser"]["runtime"], &json!({"tabLeaseId":lease.id(),"profileDir":profile.path(),"allocationUncertain":true,"chromeHost":endpoint.host,"chromePort":endpoint.port,"browserWSEndpoint":endpoint.browser_ws_endpoint}))}))}),
            )?;
            return Err(error);
        }
    };
    let result = run_owned(
        store,
        id,
        &witness,
        &committed,
        &mut target,
        &endpoint,
        bringup,
        limits,
        cancel,
        capture,
        bootstrap,
        startup,
    )
    .await;
    let result = classify_connection_loss(store, id, &target, &endpoint, cancel, result).await;
    if let Err(error) = &result {
        let saved = fail_owned(store, id, &witness, |current| {
            let dispatch = current
                .get("dispatch")
                .map(|v| merge_object(v, &json!({"state":"uncertain"})))
                .unwrap_or_else(
                    || json!({"state":"not-started","reservationId":committed.reservation_id}),
                );
            let retired = current["browser"]["runtime"]["targetDispositionPending"] == false
                && current["browser"]["runtime"]["chromeTargetId"] == target.id()
                && current["browser"]["runtime"]["targetRetiredAt"].is_string();
            let mut failed = json!({"status":if cancel.is_cancelled(){"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"recoveryRequired":!retired,"dispatch":dispatch});
            if let Some(lost) = ConnectionLost::record(error) {
                failed["error"] = lost;
            }
            failed
        });
        if let Err(persistence) = saved {
            return result.map_err(|original| {
                anyhow::anyhow!("{original:#}; error state persistence failed: {persistence:#}")
            });
        }
    }
    result
}
struct CompletedTurn {
    outcome: OutcomeWitness,
    dispatch: Value,
    receipt: Value,
    observed: Value,
}
#[allow(clippy::too_many_arguments)] // Durable progress is bound to the exact completed response and worker.
fn remember_turn(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &mut OutcomeWitness,
    receipt: &CommitReceipt,
    observed: &Value,
    markdown: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    if crate::browser_turns::prompts(&store.require(id)?["options"])?.is_empty() {
        return Ok(());
    }
    let mut dispatch = None;
    let mut before_turn = None;
    let after_turn = outcome.update(store,id,witness,cancel,|current| {
        let index=crate::browser_turns::index(current)?;
        let mut turns=crate::browser_turns::saved(current)?.to_vec();
        anyhow::ensure!(current["browser"]["runtime"]["userCommitReceipt"]==json!(receipt)&&observed["lastUserMessageId"]==receipt.message_id&&observed["completionConfirmed"]==true,"Sequential capture source changed");
        if let Some(row)=turns.get(index) {
            anyhow::ensure!(turns.len()==index+1&&row["assistantMessageId"]==observed["lastAssistantMessageId"]&&row["assistantTurnId"]==observed["lastAssistantTurnId"]&&row["report"]==observed["report"]&&row["sourceSha256"]==sha256(observed["text"].as_str().context("Completed turn has no source text")?.as_bytes())&&crate::browser_turns::verify_turn(store,current,row,index)?==markdown,"Saved sequential answer changed before recovery");
            return Ok(None);
        }
        anyhow::ensure!(turns.len()==index,"Sequential progress skipped an unobserved turn");
        let capture=artifacts::capture(store,id,&format!("captured-turn-{}.md",Uuid::new_v4()),markdown.as_bytes(),cancel)?;
        let next_dispatch=merge_object(&current["dispatch"],&json!({"turnCapture":capture}));
        turns.push(json!({"index":index,"capture":capture,"receipt":receipt,"assistantMessageId":observed["lastAssistantMessageId"],"assistantTurnId":observed["lastAssistantTurnId"],"report":observed["report"],"sourceSha256":sha256(observed["text"].as_str().context("Completed turn has no source text")?.as_bytes()),"images":observed["images"]}));
        dispatch=Some(next_dispatch.clone());
        before_turn=Some(current.clone());
        Ok(Some(json!({"dispatch":next_dispatch,"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"completedBrowserTurns":turns}))}))})))
    })?;
    if let Some(before) = before_turn {
        witness.seal_recovered_turn(&before, &after_turn, observed)?;
    }
    if let Some(dispatch) = dispatch {
        outcome.dispatch = dispatch_identity(&json!({"dispatch":dispatch}));
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)] // Bind each send to its request, exact allocation and preceding completed turn.
async fn send_owned_turn(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    authority: TargetAuthority<'_>,
    reservation: &mut RendererReservation,
    prepared: &mut PreparedPrompt,
    prompt: &str,
    selection: (&str, &str),
    previous: Option<&CompletedTurn>,
    recovery: &mut read_recovery::Budget,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<(CommitReceipt, OutcomeWitness)> {
    let result = Box::pin(send_owned_turn_inner(
        store,
        id,
        witness,
        committed,
        target,
        endpoint,
        authority,
        reservation,
        prepared,
        prompt,
        selection,
        previous,
        recovery,
        limits,
        cancel,
    ))
    .await;
    let retain = result.is_err()
        && reservation.state() != crate::browser_renderer::DispatchState::NotStarted;
    let cleanup = prepared
        .cleanup_witness(reservation, retain, Duration::from_secs(1))
        .await;
    if let Err(error) = cleanup {
        let _ = reservation
            .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await;
        return Err(error.context("Submission identity observation could not settle"));
    }
    result
}
#[allow(clippy::too_many_arguments)]
async fn send_owned_turn_inner(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    authority: TargetAuthority<'_>,
    reservation: &mut RendererReservation,
    prepared: &mut PreparedPrompt,
    prompt: &str,
    selection: (&str, &str),
    previous: Option<&CompletedTurn>,
    recovery: &mut read_recovery::Budget,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<(CommitReceipt, OutcomeWitness)> {
    let current = store.require(id)?;
    witness.require_worker(&current)?;
    if current["browser"]["config"]["researchMode"] == "search"
        && prepared
            .activate_search(reservation, limits.command, cancel)
            .await?
    {
        automation_log(
            store,
            id,
            "Web Search selected; inline search hint and staged prompt verified.",
        )?;
    }
    if current["browser"]["config"]["researchMode"] == "deep" {
        let already = prepared
            .activate_research(reservation, limits.command, cancel)
            .await?;
        automation_log(
            store,
            id,
            if already {
                "Deep Research mode already active"
            } else {
                "Deep Research mode activated"
            },
        )?;
        let baseline = crate::browser_research::scan_reports(
            reservation.page(),
            target.id(),
            None,
            &[],
            limits.command,
            cancel,
        )
        .await?;
        runtime_patch(
            store,
            id,
            witness,
            json!({"researchTargetBaseline":{"version":1,"pageTargetId":target.id(),"submissionOwner":reservation.owner(),"targetIds":baseline.target_keys}}),
        )?;
        prepared.check(reservation, limits.command, cancel).await?;
    }
    let mut snapshot = browser_commit::capture_snapshot(
        reservation.page(),
        reservation.owner(),
        prompt,
        limits.command,
        cancel,
    )
    .await?;
    snapshot.bind_submission(prepared.submission_intent())?;
    runtime_patch(store, id, witness, json!({"commitSnapshot":snapshot}))?;
    let verified = reservation
        .verify_pair(selection.0, selection.1, limits.command, cancel)
        .await;
    flush_notes(store, id, reservation);
    verified?;
    crate::perf::detail("send-readiness-click", Some(crate::perf::current_turn()));
    if browser_dom::chat_list_rate_limited(reservation, limits.command, cancel).await {
        progress(
            store,
            id,
            "[browser] ChatGPT is rate-limiting its conversation list (HTTP 429); attempting the composer send anyway.",
        );
    }
    prepared
        .arm_send(
            reservation,
            prepared.send_budget(&current["browser"]["config"]),
            cancel,
        )
        .await?;
    let prompt_sha256 = sha256(prompt.as_bytes());
    let mut dispatch_witness = Value::Null;
    store.update(id, |current| {
        witness.require(current)?;
        crate::browser_attempt::require_if_child(store, id, cancel)?;
        anyhow::ensure!(current["status"]=="running"&&!cancel.is_cancelled()&&current["browser"]["runtime"]["chromeTargetId"]==target.id()&&authority.holds(&current["browser"]["runtime"],target.id()),"Dispatch refused after cancellation or allocation change");
        let mut history = vec![];
        if let Some(previous) = previous {
            previous.outcome.require(current,witness,None,cancel)?;
            anyhow::ensure!(dispatch_identity(current)==previous.dispatch&&current["dispatch"]["state"]=="acknowledged"&&current["browser"]["runtime"]["userCommitReceipt"]==previous.receipt&&previous.observed["completionConfirmed"]==true&&previous.observed["lastUserMessageId"]==previous.receipt["messageId"],"A sequential send requires its exact preceding completed turn");
            if let Some(saved) = current["dispatch"].get("history") {
                history = saved.as_array().context("Sequential dispatch history changed")?.clone();
            }
            history.push(json!({"submissionOwner":current["dispatch"]["submissionOwner"],"promptSha256":current["dispatch"]["promptSha256"],"userMessageId":previous.receipt["messageId"],"assistantMessageId":previous.observed["lastAssistantMessageId"],"assistantTurnId":previous.observed["lastAssistantTurnId"],"report":previous.observed["report"],"answerSha256":sha256(previous.observed["text"].as_str().context("Completed turn has no answer")?.as_bytes()),"capture":previous.dispatch["turnCapture"]}));
        } else {
            anyhow::ensure!(current.get("dispatch").is_none(),"Dispatch refused after a prior send");
        }
        let mut dispatch=json!({"state":"started","reservationId":committed.reservation_id,"submissionOwner":reservation.owner(),"promptSha256":prompt_sha256,"startedAt":timestamp()});
        if previous.is_some() {dispatch["turnIndex"]=json!(history.len());dispatch["history"]=json!(history);}
        dispatch_witness=dispatch.clone();
        Ok(Some(json!({"dispatch":dispatch,"recoveryRequired":true})))
    })?;
    reservation.dispatch_started(limits.command, cancel).await?;
    crate::browser_attempt::require_if_child(store, id, cancel)?;
    let sent = prepared
        .send_once(reservation, limits.command, cancel)
        .await;
    let cleaned = prepared.cleanup(reservation, Duration::from_secs(1)).await;
    sent?;
    cleaned?;
    crate::perf::detail("send-acknowledgement", Some(crate::perf::current_turn()));
    let read_context = read_recovery::Context::capture(
        store,
        id,
        witness,
        committed,
        target,
        endpoint,
        authority,
        reservation,
    )?;
    let initial = wait_receipt(
        reservation,
        &snapshot,
        prompt,
        limits,
        cancel,
        |conversation| {
            let url = format!("https://chatgpt.com/c/{conversation}");
            runtime_patch(
                store,
                id,
                witness,
                json!({"tabUrl":url,"conversationId":conversation}),
            )?;
            target.lease().update(&json!({"tabUrl":url}))
        },
    )
    .await;
    let retained = &*reservation;
    let original_snapshot = &snapshot;
    let receipt = recovery
        .observe(
            initial,
            read_context,
            limits,
            cancel,
            |bounded| async move {
                wait_receipt(
                    retained,
                    original_snapshot,
                    prompt,
                    &bounded,
                    cancel,
                    |conversation| {
                        let url = format!("https://chatgpt.com/c/{conversation}");
                        runtime_patch(
                            store,
                            id,
                            witness,
                            json!({"tabUrl":url,"conversationId":conversation}),
                        )?;
                        target.lease().update(&json!({"tabUrl":url}))
                    },
                )
                .await
            },
        )
        .await?;
    snapshot.seal_witness(&receipt)?;
    store.update(id,|current| {
        witness.require(current)?;
        anyhow::ensure!(current["status"]=="running"&&current["dispatch"]["state"]=="started"&&current["dispatch"]["submissionOwner"]==json!(reservation.owner())&&current["dispatch"]["promptSha256"]==prompt_sha256&&current["browser"]["runtime"]["chromeTargetId"]==target.id()&&authority.holds(&current["browser"]["runtime"],target.id()),"Target or dispatch changed before source acknowledgement");
        if let Some(previous)=previous {anyhow::ensure!(current["browser"]["runtime"]["conversationId"]==receipt.conversation.conversation_id&&previous.receipt["conversation"]["conversationId"]==receipt.conversation.conversation_id,"Sequential send changed conversations");}
        let runtime=json!({"commitSnapshot":snapshot,"userCommitReceipt":receipt,"submittedPromptHash":receipt.message_fingerprint,"submittedPromptHashFormat":"original-source-v1","conversationId":receipt.conversation.conversation_id,"tabUrl":format!("https://chatgpt.com/c/{}",receipt.conversation.conversation_id),authority.record():merge_object(&current["browser"]["runtime"][authority.record()],&json!({"host":endpoint.host,"port":endpoint.port,"targetId":target.id(),"browserWSEndpoint":endpoint.browser_ws_endpoint,"claimId":reservation.owner().claim_id,"conversationId":receipt.conversation.conversation_id}))});
        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&runtime)}))})))
    })?;
    recovery
        .within(reservation.acknowledge(&receipt, recovery.limits(limits).command, cancel))
        .await?;
    target.lease().update(&json!({"promptSubmitted":true,"tabUrl":format!("https://chatgpt.com/c/{}",receipt.conversation.conversation_id)}))?;
    // A diagnostic line never fails an acknowledged send.
    let _ = automation_log(
        store,
        id,
        &format!(
            "[browser] conversation url (post-submit) = https://chatgpt.com/c/{}",
            receipt.conversation.conversation_id
        ),
    );
    let acknowledged = merge_object(&dispatch_witness, &json!({"state":"acknowledged"}));
    store.update(id, |current| {
        witness.require(current)?;
        anyhow::ensure!(
            current["status"] == "running"
                && current["dispatch"] == dispatch_witness
                && current["browser"]["runtime"]["userCommitReceipt"] == json!(receipt),
            "Dispatch changed before acknowledgement"
        );
        Ok(Some(json!({"dispatch":acknowledged})))
    })?;
    let metadata = store.require(id)?;
    anyhow::ensure!(
        metadata["dispatch"] == acknowledged,
        "Acknowledged dispatch changed before outcome observation"
    );
    let outcome = OutcomeWitness::capture(
        &metadata,
        target,
        endpoint,
        &receipt,
        &committed.options_sha256,
        authority,
    )?;
    Ok((receipt, outcome))
}
#[allow(clippy::too_many_arguments)] // Keep the durable request, session and target witnesses explicit.
async fn run_owned(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &mut OwnedTarget,
    endpoint: &AttachmentEndpoint,
    bringup: DirectoryLock,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    capture: &CaptureBoundary,
    bootstrap: Option<&crate::browser_auth::CookieBootstrap>,
    startup: Option<DirectoryLock>,
) -> Result<Value> {
    let mut committed = committed.clone();
    let deferred_reconciliation = startup.is_some();
    // Persist the exact allocation before navigating. A crash never loses its ownership anchor.
    let claim_id = Uuid::new_v4().to_string();
    runtime_patch(
        store,
        id,
        witness,
        json!({"tabLeaseId":target.lease().id(),"profileDir":target.lease().profile().path(),"chromeHost":endpoint.host,"chromePort":endpoint.port,"chromeTargetId":target.id(),"browserWSEndpoint":endpoint.browser_ws_endpoint,"chromeBrowserWSEndpoint":endpoint.browser_ws_endpoint,"targetClaimId":claim_id,"allocationUncertain":false,"targetDispositionPending":true,"creatorPersistenceIncomplete":true}),
    )?;
    let creation = target
        .creation()
        .context("New browser run has no actual target creation evidence")?;
    target
        .lease()
        .update(&json!({"targetDispositionAllocationId":creation.allocation_id()}))?;
    let journal = TargetJournal::open(store)?;
    let owned = json!({"host":endpoint.host,"port":endpoint.port,"targetId":target.id(),"browserWSEndpoint":endpoint.browser_ws_endpoint,"claimId":claim_id});
    let creator=journal.record_creation(creation,&owned,&ProcessIdentity::current()?,Some(id),Some(json!({"profileDir":crate::native_fs::canonical_wire_path(target.lease().profile().path())?,"id":target.lease().id()})))?;
    runtime_patch(
        store,
        id,
        witness,
        json!({"ownedRecoveryTarget":merge_object(&owned,&json!({"creator":creator})),"creatorPersistenceIncomplete":false}),
    )?;
    target.require(limits.command, cancel).await?;
    let config = store.require(id)?["browser"]["config"].clone();
    if config["attachRunning"] != true
        && !config["remoteChrome"].is_object()
        && let Err(error) = crate::browser_window::configure(
            target.page(),
            target.lease().profile(),
            &config,
            limits.command,
            cancel,
        )
        .await
    {
        eprintln!("Chrome window placement failed: {error}");
    }
    if let Err(error) = target
        .page()
        .call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled":true}),
            limits.command,
            cancel,
        )
        .await
    {
        eprintln!("Chrome focus emulation is unavailable: {error}");
    }
    let grok = store.require(id)?["browser"]["attempt"]["provider"] == "grok";
    let _dialogs = if grok {
        None
    } else {
        Some(dismiss_dialogs(store, id, target.page(), limits, cancel).await?)
    };
    let mut applied_cookies = 0;
    let (url, expected_navigation) = if grok {
        anyhow::ensure!(bootstrap.is_none(), "Grok cannot receive ChatGPT cookies");
        ("https://grok.com/".to_owned(), Value::Null)
    } else {
        navigation_destination(&config)?
    };
    if let Some(batch) = bootstrap {
        let applied = match batch
            .apply(target.page(), &url, limits.command, cancel)
            .await
        {
            Ok(applied) => applied,
            Err(error) => {
                if cancel.is_cancelled()
                    && !config["closeOwnedTabOnCancel"]
                        .as_bool()
                        .unwrap_or(config["keepBrowser"] != true)
                {
                    return Err(error);
                }
                let cleanup = async {
                    let current = store.require(id)?;
                    witness.require(&current)?;
                    anyhow::ensure!(
                        current["status"] == "running"
                            && current.get("dispatch").is_none()
                            && current["browser"]["runtime"]["ownedRecoveryTarget"]["creator"]
                                == creator
                            && journal.read(&creator["allocationId"])?["creator"] == creator,
                        "Pre-navigation creator or session changed"
                    );
                    runtime_patch(store, id, witness, json!({"preNavigationCleanupPending":true}))?;
                    journal.record_disposition(
                        &creator,
                        "unknown",
                        "settled-pre-dispatch-close-pending",
                    )?;
                    let retiring = &mut *target;
                    let expected_creator = &creator;
                    let disposed = journal.retire_settled_before_dispatch(&creator, |deadline| async move {
                        let current = store.require(id)?;
                        witness.require(&current)?;
                        anyhow::ensure!(current["status"] == "running" && current.get("dispatch").is_none()
                            && current["browser"]["runtime"]["ownedRecoveryTarget"]["creator"] == *expected_creator,
                            "Pre-navigation owner changed before retirement");
                        retiring.retire_unclaimed_blank(deadline.saturating_duration_since(std::time::Instant::now()).min(Duration::from_secs(1))).await?;
                        Ok(("confirmed-absent".into(),"unused-authentication-target".into()))
                    }).await?;
                    anyhow::ensure!(disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent",
                        "Pre-navigation target absence remains unconfirmed");
                    runtime_patch(
                        store,
                        id,
                        witness,
                        json!({"preNavigationCleanupPending":false,"targetDispositionPending":false,"targetRetiredAt":timestamp()}),
                    )?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(cleanup) = cleanup {
                    let _ = runtime_patch(
                        store,
                        id,
                        witness,
                        json!({"preNavigationCleanupWarning":cleanup.to_string()}),
                    );
                }
                return Err(error);
            }
        };
        runtime_patch(
            store,
            id,
            witness,
            json!({"cookieBootstrap":{"applied":applied,"completedAt":timestamp()}}),
        )?;
        progress(store, id, &batch.applied_line(applied, &config));
        applied_cookies = applied as u64;
    }
    if let Some(startup) = startup {
        startup.release()?;
    }
    let mut reservation = if grok {
        grok::navigate(target.page(), limits, cancel).await?;
        RendererReservation::acquire_initial_grok(
            target.page().clone(),
            claim_id,
            limits.command,
            cancel,
        )
        .await?
    } else {
        clear_stale_conversation_cookies(store, id, target, &config, limits, cancel).await?;
        navigate_exact(target.page(), &url, &expected_navigation, limits, cancel).await?;
        // A challenge keeps this tab for the check; the login check follows the claim so a
        // signed-out run retires its unused tab before dispatch.
        ensure_not_blocked(
            store,
            id,
            target.page(),
            &config,
            target.lease().profile().path(),
            limits,
            cancel,
        )
        .await?;
        let reservation =
            RendererReservation::acquire(target.page().clone(), claim_id, limits.command, cancel)
                .await?;
        // Reference captureRuntimeSnapshot once the composer page is ready.
        automation_log(store, id, &format!("[browser] url = {url}"))?;
        automation_log(
            store,
            id,
            &format!(
                "[reattach] chrome port={} host={} url={url} target={}",
                endpoint.port,
                endpoint.host,
                target.id()
            ),
        )?;
        reservation
    };
    let mut answer = String::new();
    let outcome = async {
        let selection = if config["resumeConversationUrl"].is_string() {
            Selection::Verify
        } else {
            Selection::Choose
        };
        if !grok {
            let hint = crate::browser_page::LoginHint {
                applied_cookies,
                remote: config["remoteChrome"].is_object(),
            };
            wait_for_login(
                store,
                id,
                target.page(),
                &config,
                hint,
                (&url, &expected_navigation),
                target.lease().profile().path(),
                limits,
                cancel,
            )
            .await?;
        }
        let delivered = if grok {
            Box::pin(grok::deliver(store,id,witness,&committed,target,endpoint,&creator,&mut reservation,bringup,deferred_reconciliation,limits,cancel)).await?
        } else {
            Box::pin(deliver(store, id, witness, &mut committed, target, endpoint, TargetAuthority::Created(Some(&creator)), selection, &config, &mut reservation, bringup, deferred_reconciliation, limits, cancel)).await?
        };
        let Delivered { model, thinking, receipt, text, outcome: outcome_witness } = delivered;
        let text = text.as_str();
        answer = text.to_owned();
        if capture.deferred() {
            match capture {
                CaptureBoundary::Remote(hook) => {
                    let mut captured = None;
                    outcome_witness.update(store,id,witness,cancel,|current| {
                        let run=current["browser"]["remoteDelivery"]["runId"].as_str().context("Remote producer has no delivery identity")?;
                        anyhow::ensure!(crate::remote_result::session_id(run)?==id
                            && current["browser"]["remoteDelivery"]["acknowledged"]!=true
                            && current["browser"]["remoteDelivery"]["captureIssued"]!=true,
                            "Remote producer delivery identity changed before capture");
                        captured=Some(crate::remote_protocol::captured(store,id)?);
                        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"remoteDelivery":merge_object(&current["browser"]["remoteDelivery"],&json!({"captureIssued":true}))}))})))
                    })?;
                    // A failed or interrupted callback is a durable barrier. It runs outside
                    // the child transaction so parent adoption cannot invert write locks.
                    hook(captured.context("Remote capture snapshot missing")?)?;
                }
                CaptureBoundary::ParentAttempt => {
                    crate::browser_attempt::require_child(store,id,cancel)?;
                    outcome_witness.require(&store.require(id)?,witness,None,cancel)?;
                    crate::remote_protocol::captured(store,id)?;
                }
                CaptureBoundary::Direct => unreachable!(),
            }
            #[cfg(test)]
            if let Some(crash) = TEST_BEFORE_ATTEMPT_RELEASE.take() {
                crash();
            }
            // ACK owns completion and exact retirement after the receiver has persisted output.
            reservation.release_clean(crate::browser_renderer::CONTROL_ACK_TIMEOUT).await?;
            return Ok(());
        }
        complete(store, id, witness,&outcome_witness, |current| {
            let changes=json!({"status":"completed","completedAt":timestamp(),"capturedAnswer":receipt,"browser":merge_object(&current["browser"], &json!({"modelSelection":model,"thinkingSelection":thinking})),"recoveryRequired":true,"dispatch":merge_object(&current["dispatch"], &json!({"state":"completed"}))});
            let proposed=merge_object(current,&changes);
            let commit=journal.session_commit(&creator,&proposed)?;
            journal.verify_proposed(&json!({"creator":creator,"commit":commit}),&proposed)?;
            Ok(changes)
        },&json!({"modelSelection":model,"thinkingSelection":thinking}),text,cancel)?;
        let completed=store.require(id)?;
        let commit=journal.commit(&creator,&journal.session_commit(&creator,&completed)?)?;
        journal.finalise(&commit)?;
        Ok::<_, anyhow::Error>(())
    }.await;
    if let Err(error) = outcome {
        let quarantined = reservation
            .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await;
        if quarantined.is_ok()
            && reservation.settled_before_dispatch()
            && (!cancel.is_cancelled()
                || config["closeOwnedTabOnCancel"]
                    .as_bool()
                    .unwrap_or(config["keepBrowser"] != true))
        {
            let cleanup = async {
                if let Err(first) =
                    retire_pre_dispatch(store, witness, target, &reservation, &journal, &creator)
                        .await
                {
                    // Retry only whilst the original worker still proves its not-started
                    // reservation. The saved marker survives a refused or unacknowledged close.
                    require_pre_dispatch_authority(
                        store,
                        witness,
                        target,
                        &reservation,
                        &journal,
                        &creator,
                        std::time::Instant::now() + Duration::from_secs(1),
                    )?;
                    retire_pre_dispatch(store, witness, target, &reservation, &journal, &creator)
                        .await
                        .with_context(|| {
                            format!("Pre-dispatch cleanup retry failed after {first}")
                        })?;
                }
                runtime_patch(
                    store,
                    id,
                    witness,
                    json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}),
                )?;
                retire_inputs(store, id, &committed)?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(cleanup) = cleanup {
                // Cleanup cannot replace the consultation's original error.
                let _ = runtime_patch(
                    store,
                    id,
                    witness,
                    json!({"preDispatchCleanupWarning":cleanup.to_string()}),
                );
            }
        } else if quarantined.is_ok()
            && cancel.is_cancelled()
            && !reservation.settled_before_dispatch()
            && !grok
            && !capture.deferred()
            && let Err(cleanup) = retire_cancelled_send(
                store,
                id,
                witness,
                target,
                &reservation,
                &journal,
                &creator,
                cancelled_send_settle(),
                limits.command,
            )
            .await
        {
            eprintln!("[browser] {cleanup}; retaining its target and lease.");
            let _ = runtime_patch(
                store,
                id,
                witness,
                json!({"cancelledSendCleanupWarning":cleanup.to_string()}),
            );
        }
        return Err(error);
    }
    if capture.deferred() {
        return store.require(id);
    }
    crate::notify::completed(&store.require(id)?, &answer).await;
    // Completed capture is durable before ownership/target retirement. Cleanup failure cannot
    // erase it or reopen dispatch; recoveryRequired stays true until exact absence is confirmed.
    let cleanup = async {
        let meta = store.require(id)?;
        if grok {
            reservation.release_clean(crate::browser_renderer::CONTROL_ACK_TIMEOUT).await?;
            let disposed=crate::browser_retirement::retire_committed(store,&journal,target.browser(),endpoint,&creator).await?;
            anyhow::ensure!(disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent", "Grok owned target absence remains unconfirmed");
            target.release_retired_lease()?;
            settle_completed_retirement(store,&journal,&creator,&disposed,std::time::Instant::now()+Duration::from_secs(2))?;
            return Ok(());
        }
        let expected = meta["browser"]["runtime"]["conversationId"].as_str().context("Completed target has no saved conversation identity")?;
        let owned = browser::RecoveryTarget { host:endpoint.host.clone(), port:endpoint.port, target_id:target.id().into(), browser_ws_endpoint:Some(endpoint.browser_ws_endpoint.clone()), claim_id:reservation.owner().claim_id.clone(), conversation_id:expected.into() };
        anyhow::ensure!(browser::exact_owned_target(&meta, &owned), "Completed target differs from saved ownership; refusing retirement");
        journal.retire_finalised(&creator,|commit| {
            let commit=commit.clone();
            let target=&mut *target;
            let creator=&creator;
            let journal=&journal;
            let reservation=&mut reservation;
            let meta=&meta;
            async move {
                require_retirement_authority(store,id,target,creator,&commit,journal)?;
                let observed=observe_retirement(reservation.page(),meta,&json!({"owner":reservation.owner(),"receipt":meta["browser"]["runtime"]["userCommitReceipt"],"recovery":false}),&commit,limits.command).await?;
                require_retirement_content(meta,&observed,&commit)?;
                require_retirement_authority(store,id,target,creator,&commit,journal)?;
                reservation.release_clean(crate::browser_renderer::CONTROL_ACK_TIMEOUT).await?;
                let marker=journal.retirement_id(creator,&commit)?;
                mark_completed_target(target, reservation, expected, &marker, limits.command).await?;
                close_marked_target(store,id,target,reservation,creator,&commit,journal,&marker,limits.command).await?;
                Ok(("confirmed-absent".into(),"exact-target-absent".into()))
            }
        }).await?;
        target.release_retired_lease()?;
        retire_inputs(store,id,&committed)?;
        store.update(id, |current| { witness.require(current)?; anyhow::ensure!(current["status"] == "completed", "Completed session changed before retirement acknowledgement"); Ok(Some(json!({"recoveryRequired":false,"browser":merge_object(&current["browser"], &json!({"runtime":merge_object(&current["browser"]["runtime"], &json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}))}))}))) })?;
        Ok::<_, anyhow::Error>(())
    }.await;
    if let Err(first_error) = cleanup {
        // One retry finishes transient close/acknowledgement failures whilst this worker is live.
        // Only a durable completed commitment can enter this route; it never replays submission.
        let retry_deadline = std::time::Instant::now() + Duration::from_secs(8);
        let retry = tokio::time::timeout(Duration::from_secs(8), async {
            let current = store.require(id)?;
            witness.require(&current)?;
            anyhow::ensure!(
                current["status"] == "completed" && current["dispatch"]["state"] == "completed",
                "Completed cleanup retry lost its terminal session"
            );
            let record = journal.read(&creator["allocationId"])?;
            journal.verify(&record)?;
            let marker = journal.retirement_id(&creator, &record["commit"])?;
            if record["disposition"]["decision"] != "retired"
                && !target
                    .observe_absence_until(std::time::Instant::now() + Duration::from_secs(1))
                    .await?
            {
                anyhow::ensure!(
                    crate::browser_retirement::pending_marker(
                        target.page(),
                        &current["browser"]["runtime"]["submissionOwner"],
                        &marker,
                        std::time::Instant::now() + Duration::from_secs(1)
                    )
                    .await?
                    .is_some(),
                    "Automatic cleanup retry requires the original retirement marker"
                );
            }
            let disposed = crate::browser_retirement::retire_committed(
                store,
                &journal,
                target.browser(),
                endpoint,
                &creator,
            )
            .await?;
            anyhow::ensure!(
                disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent",
                "Owned tab cleanup retry remains pending: {}",
                disposed["reason"]
            );
            settle_completed_retirement(store, &journal, &creator, &disposed, retry_deadline)?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Owned tab cleanup retry deadline reached")
        .and_then(|result| result);
        if let Err(retry_error) = retry
            && let Err(persist_error) = store.update_until(
                id,
                std::time::Instant::now() + Duration::from_secs(1),
                |current| {
                    if witness.require(current).is_err() { return Ok(None); }
                    Ok(Some(json!({"recoveryRequired":true,"cleanupWarning":format!("{first_error}; retry: {retry_error}")})))
                },
            ) {
            eprintln!("Answer saved. Owned tab cleanup status could not be persisted: {persist_error:#}; cleanup: {first_error:#}; retry: {retry_error:#}");
        }
    }
    store.require(id)
}
/// Whether a run selects its model and thinking effort or only verifies the current ones.
#[derive(Clone, Copy)]
enum Selection {
    Choose,
    Verify,
}
/// A published answer and the evidence that completion binds to it.
struct Delivered {
    model: Value,
    thinking: Value,
    receipt: CaptureReceipt,
    text: String,
    outcome: OutcomeWitness,
}
#[allow(clippy::too_many_arguments)] // Each witness and the exact target authority stay explicit.
/// Selection, uploads, every turn's send and answer, deliveries and publication for one
/// claimed renderer. Created and borrowed targets differ only in `authority` and `selection`;
/// completion and target disposal stay with the caller.
async fn deliver(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &mut BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    authority: TargetAuthority<'_>,
    selection: Selection,
    config: &Value,
    reservation: &mut RendererReservation,
    bringup: DirectoryLock,
    deferred_reconciliation: bool,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Delivered> {
    // A recovered turn must not fall through into unsent sequential prompts. Borrowed targets
    // keep their existing lifecycle; this episode applies only to one newly owned submission.
    let mut recovery = read_recovery::Budget::new(
        matches!(authority, TargetAuthority::Created(Some(_)))
            && crate::browser_turns::prompts(&store.require(id)?["options"])?.is_empty(),
    );
    runtime_patch(
        store,
        id,
        witness,
        json!({"submissionOwner":reservation.owner()}),
    )?;
    crate::perf::detail("composer-readiness", Some(crate::perf::current_turn()));
    let (_, nav) = if matches!(selection, Selection::Choose) {
        let (_, expected) = navigation_destination(config)?;
        // Reference ensureChatMode before the composer is verified for a new chat.
        let deadline = Instant::now() + limits.command;
        crate::browser_page::ensure_chat_mode(
            reservation,
            deadline,
            limits.command,
            cancel,
            &|line| page_log(store, id, line),
        )
        .await?;
        let left = deadline.saturating_duration_since(Instant::now());
        browser_dom::wait_initial_navigation(reservation, &expected, left, cancel).await?
    } else {
        browser_dom::capture_navigation(reservation, limits.command, cancel).await?
    };
    runtime_patch(
        store,
        id,
        witness,
        json!({"temporaryChat":{"verified":true,"source":"chatgpt-live-ui","capturedAt":timestamp()}}),
    )?;
    crate::perf::phase("mode-selection", None);
    let selected = match (selection, authority) {
        (Selection::Choose, _) => {
            reservation
                .select_pair(
                    config["modelStrategy"].as_str().unwrap_or("select"),
                    config["desiredModel"].as_str().unwrap_or("GPT-6"),
                    limits.command,
                    cancel,
                )
                .await
        }
        // A borrowed tab is verified against Industrious's Pro default unless a saved effort exists.
        (Selection::Verify, TargetAuthority::Borrowed) => {
            reservation
                .verify_pair(
                    config["desiredModel"]
                        .as_str()
                        .context("Borrowed browser run has no saved model")?,
                    config["thinkingTime"].as_str().unwrap_or("pro"),
                    limits.command,
                    cancel,
                )
                .await
        }
        (Selection::Verify, _) => {
            reservation
                .verify_pair(
                    config["desiredModel"]
                        .as_str()
                        .context("Browser follow-up has no saved model")?,
                    config["thinkingTime"]
                        .as_str()
                        .context("Browser follow-up has no saved thinking effort")?,
                    limits.command,
                    cancel,
                )
                .await
        }
    };
    flush_notes(store, id, reservation);
    let (model, thinking, effective) = selected?;
    reservation.seal_selection();
    let selected_level = match thinking.requested_level {
        policy::ThinkingTime::Pro => "pro",
        policy::ThinkingTime::ExtraHigh => "extra-high",
        _ => bail!("Browser selection did not verify a permitted thinking effort"),
    };
    // Save the positively verified pair for later continuations. The original
    // options remain the immutable intent bound to browser-request.json.
    let current = store.require(id)?;
    patch(
        store,
        id,
        witness,
        json!({"browser":merge_object(&current["browser"],&json!({"config":merge_object(&current["browser"]["config"],&json!({"thinkingTime":thinking.requested_level}))}))}),
    )?;
    runtime_patch(
        store,
        id,
        witness,
        json!({"modelSelection":model,"thinkingSelection":thinking,"effectiveModel":effective}),
    )?;
    bringup.release()?;
    if deferred_reconciliation {
        crate::browser_retirement::reconcile(store, target.browser(), endpoint).await?;
    }
    let automation = automation_lock(target, config, limits, cancel).await?;
    crate::perf::phase("upload", None);
    if !committed.upload_paths.is_empty() {
        crate::perf::detail("attachment-transfer", Some(crate::perf::current_turn()));
    }
    let expectations = upload_inputs(
        store,
        id,
        witness,
        reservation,
        committed,
        &nav,
        attachment_budget(config, limits, committed.upload_paths.len()),
        remote_host(endpoint),
        cancel,
    )
    .await?;
    crate::perf::detail("composer-population", Some(crate::perf::current_turn()));
    let mut prepared = match PreparedPrompt::stage(
        reservation,
        &committed.prompt,
        &nav,
        &expectations,
        limits.command,
        cancel,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error)
            if error
                .downcast_ref::<browser_dom::PromptTooLarge>()
                .is_some()
                && committed.fallback.is_some()
                && !committed.fallback_used =>
        {
            let observed = error
                .downcast_ref::<browser_dom::PromptTooLarge>()
                .context("Missing positive truncation evidence")?;
            anyhow::ensure!(
                !cancel.is_cancelled()
                    && reservation.state() == crate::browser_renderer::DispatchState::NotStarted,
                "Fallback refused after cancellation or dispatch"
            );
            reservation.assert_active(limits.command, cancel).await?;
            automation_log(
                store,
                id,
                "[browser] Inline prompt too large; retrying with file uploads.",
            )?;
            if !committed.upload_paths.is_empty() {
                let uploads = store.require(id)?["browser"]["runtime"]["uploads"]
                    .as_array()
                    .cloned()
                    .context("Original raw uploads have no renderer receipts")?;
                anyhow::ensure!(
                    uploads.len() == committed.upload_paths.len(),
                    "Original upload receipt count changed before fallback"
                );
                browser_dom::reset_uploads(reservation, &uploads, &nav, limits.command, cancel)
                    .await?;
                runtime_patch(
                    store,
                    id,
                    witness,
                    json!({"uploads":[],"priorUploads":uploads,"fallbackUploadsCleared":true}),
                )?;
            }
            materialize_fallback(store, id, witness, committed, observed, cancel)?;
            let expectations = upload_inputs(
                store,
                id,
                witness,
                reservation,
                committed,
                &nav,
                attachment_budget(config, limits, committed.upload_paths.len()),
                remote_host(endpoint),
                cancel,
            )
            .await?;
            PreparedPrompt::stage(
                reservation,
                &committed.prompt,
                &nav,
                &expectations,
                limits.command,
                cancel,
            )
            .await?
        }
        Err(error) => return Err(error),
    };
    crate::perf::phase("generation", None);
    let (mut receipt, mut outcome_witness) = Box::pin(send_owned_turn(
        store,
        id,
        witness,
        committed,
        target,
        endpoint,
        authority,
        reservation,
        &mut prepared,
        &committed.prompt,
        ("Latest", selected_level),
        None,
        &mut recovery,
        limits,
        cancel,
    ))
    .await?;
    release_automation(automation);
    let selection =
        json!({"modelSelection":model,"thinkingSelection":thinking,"effectiveModel":effective});
    crate::perf::detail("answer-waiting", Some(crate::perf::current_turn()));
    let mut observed = wait_original_answer(
        store,
        id,
        witness,
        committed,
        target,
        endpoint,
        authority,
        reservation,
        &receipt,
        &selection,
        &mut recovery,
        limits,
        cancel,
    )
    .await?;
    witness.require(&store.require(id)?)?;
    crate::perf::phase("capture", None);
    let initial_markdown =
        capture_markdown(&store.require(id)?, reservation, &observed, limits, cancel).await?;
    remember_turn(
        store,
        id,
        witness,
        &mut outcome_witness,
        &receipt,
        &observed,
        &initial_markdown,
        cancel,
    )?;
    let followups = crate::browser_turns::prompts(&store.require(id)?["options"])?;
    let sequential = !followups.is_empty();
    let total = followups.len();
    for (turn, prompt) in (1u64..).zip(followups) {
        automation_log(
            store,
            id,
            &format!("[browser] Sending follow-up {turn}/{total}"),
        )?;
        let previous = CompletedTurn {
            dispatch: outcome_witness.dispatch.clone(),
            outcome: outcome_witness,
            receipt: json!(receipt),
            observed: observed.clone(),
        };
        previous
            .outcome
            .require(&store.require(id)?, witness, None, cancel)?;
        Box::pin(collect_deliveries(
            store,
            id,
            witness,
            &previous.outcome,
            target,
            endpoint,
            reservation,
            &observed,
            limits,
            false,
            cancel,
        ))
        .await?;
        reservation
            .release_clean(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await?;
        let turn_lock = profile_lock(
            &target
                .lease()
                .profile()
                .path()
                .join("oracle-browser-bringup.lock"),
            limits.profile_lock,
            cancel,
        )
        .await?;
        *reservation = RendererReservation::acquire(
            target.page().clone(),
            reservation.owner().claim_id.clone(),
            limits.command,
            cancel,
        )
        .await?;
        runtime_patch(
            store,
            id,
            witness,
            json!({"submissionOwner":reservation.owner()}),
        )?;
        crate::perf::phase("mode-selection", Some(turn));
        // The previous answer's view can still be remounting. Wait read-only for the live
        // predicate before the turn's first guarded step; only `true` lets the turn continue.
        crate::browser_privacy::require(reservation.page(), limits.command, cancel).await?;
        let (_, nav) = browser_dom::capture_navigation(reservation, limits.command, cancel).await?;
        anyhow::ensure!(
            nav["conversationId"] == receipt.conversation.conversation_id,
            "Sequential browser navigation changed"
        );
        let verified = reservation
            .verify_pair("Latest", selected_level, limits.command, cancel)
            .await;
        flush_notes(store, id, reservation);
        verified?;
        reservation.seal_selection();
        let automation = automation_lock(target, config, limits, cancel).await?;
        crate::perf::phase("upload", None);
        crate::perf::detail("composer-population", Some(turn));
        let mut prepared =
            PreparedPrompt::stage(reservation, &prompt, &nav, &[], limits.command, cancel).await?;
        crate::perf::phase("generation", None);
        (receipt, outcome_witness) = Box::pin(send_owned_turn(
            store,
            id,
            witness,
            committed,
            target,
            endpoint,
            authority,
            reservation,
            &mut prepared,
            &prompt,
            ("Latest", selected_level),
            Some(&previous),
            &mut recovery,
            limits,
            cancel,
        ))
        .await?;
        release_automation(automation);
        turn_lock.release()?;
        crate::perf::detail("answer-waiting", Some(turn));
        observed = wait_original_answer(
            store,
            id,
            witness,
            committed,
            target,
            endpoint,
            authority,
            reservation,
            &receipt,
            &selection,
            &mut recovery,
            limits,
            cancel,
        )
        .await?;
        crate::perf::phase("capture", None);
        let markdown =
            capture_markdown(&store.require(id)?, reservation, &observed, limits, cancel).await?;
        outcome_witness.require(&store.require(id)?, witness, None, cancel)?;
        remember_turn(
            store,
            id,
            witness,
            &mut outcome_witness,
            &receipt,
            &observed,
            &markdown,
            cancel,
        )?;
    }
    let markdown = if sequential {
        crate::browser_turns::saved_transcript(store, &store.require(id)?)?
    } else {
        initial_markdown
    };
    outcome_witness.require(&store.require(id)?, witness, None, cancel)?;
    let text = markdown.as_str();
    crate::perf::detail("output-metadata-persistence", None);
    let mut receipt = capture_outcome(
        store,
        id,
        witness,
        &outcome_witness,
        text,
        Some(&observed),
        cancel,
    )?;
    collect_deliveries(
        store,
        id,
        witness,
        &outcome_witness,
        target,
        endpoint,
        reservation,
        &observed,
        limits,
        false,
        cancel,
    )
    .await?;
    let (receipt_with_images, text_with_images) =
        capture_image_summary(store, id, witness, &outcome_witness, &receipt, cancel)?;
    receipt = receipt_with_images;
    publish_answer(store, id, witness, &outcome_witness, &receipt, cancel)?;
    Ok(Delivered {
        model: json!(model),
        thinking: json!(thinking),
        receipt,
        text: text_with_images,
        outcome: outcome_witness,
    })
}
/// A manual-login profile serialises each turn's upload and send window across runs
/// (reference `acquireProfileRunLock`, `oracle-automation.lock`, `profileLockTimeoutMs`).
async fn automation_lock(
    target: &OwnedTarget,
    config: &Value,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Option<DirectoryLock>> {
    if config["manualLogin"] != true || limits.profile_lock.is_zero() {
        return Ok(None);
    }
    profile_lock(
        &target
            .lease()
            .profile()
            .path()
            .join("oracle-automation.lock"),
        limits.profile_lock,
        cancel,
    )
    .await
    .map(Some)
}
/// Waits for a profile's bring-up or automation lock with the reference's wait and timeout text.
async fn profile_lock(
    path: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<DirectoryLock> {
    let started = Instant::now();
    if fs::symlink_metadata(path).is_ok() {
        let purpose = if path.ends_with("oracle-browser-bringup.lock") {
            "browser setup"
        } else {
            "profile automation"
        };
        eprintln!(
            "[browser] Waiting for ChatGPT {purpose} lock held by {}; up to {}s.",
            crate::storage::lock_holder(path),
            timeout.as_secs_f64().round()
        );
    }
    DirectoryLock::acquire_async(path, timeout, cancel)
        .await
        .map_err(|error| {
            if cancel.is_cancelled() || started.elapsed() < timeout {
                return error;
            }
            error.context(format!(
                "Industrious profile lock still held by {} after {}s",
                crate::storage::lock_holder(path),
                started.elapsed().as_secs_f64().round()
            ))
        })
}
/// The send already happened, so a failed release is reported, never the run's result.
fn release_automation(lock: Option<DirectoryLock>) {
    if let Some(Err(error)) = lock.map(DirectoryLock::release) {
        eprintln!("[browser] Profile automation lock release failed: {error:#}");
    }
}
/// Navigates a page and waits until it reaches exactly the expected provider navigation.
/// Reference runBrowserMode: stale conversation cookies are cleared on the run's own browser
/// before navigation, keeping the resumed conversation and every open conversation tab.
async fn clear_stale_conversation_cookies(
    store: &SessionStore,
    id: &str,
    target: &OwnedTarget,
    config: &Value,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let preserve = [config["resumeConversationUrl"]
        .as_str()
        .and_then(browser::conversation_id)];
    for line in crate::browser_auth::clear_stale_conversation_cookies(
        target.browser(),
        target.page(),
        &preserve,
        limits.command,
        cancel,
    )
    .await
    {
        automation_log(store, id, &line)?;
    }
    Ok(())
}
async fn navigate_exact(
    page: &Cdp,
    url: &str,
    expected: &Value,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let navigation = page
        .call("Page.navigate", json!({"url":url}), limits.command, cancel)
        .await?;
    anyhow::ensure!(
        navigation.get("errorText").is_none(),
        "ChatGPT target navigation failed"
    );
    let deadline = Instant::now() + limits.command;
    loop {
        let observed = page
            .evaluate("location.href", limits.command, cancel)
            .await?;
        if observed.as_str().is_some_and(|observed| {
            browser_dom::navigation(observed).is_ok_and(|nav| nav == *expected)
        }) {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Owned target did not reach its exact provider navigation"
        );
        pause(limits.poll, cancel).await?;
    }
}
fn page_log(store: &SessionStore, id: &str, line: &str) {
    if let Err(error) = automation_log(store, id, line) {
        eprintln!("{line} (session log write failed: {error:#})");
    }
}
/// Reference installJavaScriptDialogAutoDismissal for this run's ChatGPT page.
async fn dismiss_dialogs(
    store: &SessionStore,
    id: &str,
    page: &Cdp,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<crate::browser_page::DialogDismissal> {
    let (store, id) = (store.clone(), id.to_owned());
    crate::browser_page::dismiss_dialogs(page, limits.command, cancel, move |line| {
        page_log(&store, &id, line)
    })
    .await
}
/// Reference ensureNotBlocked with the runBrowserMode Cloudflare handling: a headful run
/// keeps its browser for the check, and a copied profile cannot be retained.
async fn ensure_not_blocked(
    store: &SessionStore,
    id: &str,
    page: &Cdp,
    config: &Value,
    profile: &Path,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let log = |line: &str| page_log(store, id, line);
    match crate::browser_page::ensure_not_blocked(
        page,
        config["headless"] == true,
        limits.command,
        cancel,
        &log,
    )
    .await
    {
        Err(error)
            if error
                .downcast_ref::<crate::browser_page::CloudflareChallenge>()
                .is_some_and(|challenge| !challenge.headless) =>
        {
            if config["copyProfileSource"].is_string() {
                log(
                    "Cloudflare challenge detected; closing Chrome and removing the copied profile because copy-profile runs cannot be retained.",
                );
                return Err(error.context("Cloudflare challenge detected. Copy-profile runs cannot be retained; complete the check in the source Chrome profile, then rerun."));
            }
            log(
                "Cloudflare challenge detected; leaving browser open so you can complete the check.",
            );
            log(&format!(
                "Reuse this browser profile with: industrious --engine browser --browser-manual-login --browser-manual-login-profile-dir {}",
                Value::String(profile.display().to_string())
            ));
            Err(error)
        }
        other => other,
    }
}
/// Reference waitForLogin on a new owned chat, before any selection. A session that appeared
/// only during the manual-login wait reopens the exact landing page and rechecks it.
#[allow(clippy::too_many_arguments)] // The page, its destination and the login hint stay explicit.
async fn wait_for_login(
    store: &SessionStore,
    id: &str,
    page: &Cdp,
    config: &Value,
    hint: crate::browser_page::LoginHint,
    (url, expected): (&str, &Value),
    profile: &Path,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let log = |line: &str| page_log(store, id, line);
    let wait = crate::browser_page::manual_login_wait(
        config["timeoutMs"].as_u64(),
        config["keepBrowser"] == true,
    );
    let manual = config["manualLogin"] == true && !hint.remote;
    if crate::browser_page::wait_for_login(
        page,
        manual,
        hint,
        wait,
        profile,
        limits.command,
        cancel,
        &log,
    )
    .await?
    {
        navigate_exact(page, url, expected, limits, cancel).await?;
        ensure_not_blocked(store, id, page, config, profile, limits, cancel).await?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)] // The borrowed run keeps the same explicit witnesses as run_owned.
/// Reference `--browser-tab`: submits into a user tab Industrious did not create. The renderer
/// claim and reservation guard the submission, but no creator, owned recovery target or
/// retirement authority is ever recorded and the tab is never closed. Model and thinking
/// selections are verified, never changed; the tab is navigated only to open a saved
/// follow-up conversation or to leave Work mode.
async fn run_borrowed(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &mut OwnedTarget,
    endpoint: &AttachmentEndpoint,
    bringup: DirectoryLock,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    startup: Option<DirectoryLock>,
) -> Result<Value> {
    let mut committed = committed.clone();
    let deferred_reconciliation = startup.is_some();
    let claim_id = Uuid::new_v4().to_string();
    runtime_patch(
        store,
        id,
        witness,
        json!({"tabLeaseId":target.lease().id(),"profileDir":target.lease().profile().path(),"chromeHost":endpoint.host,"chromePort":endpoint.port,"chromeTargetId":target.id(),"browserWSEndpoint":endpoint.browser_ws_endpoint,"chromeBrowserWSEndpoint":endpoint.browser_ws_endpoint,"targetClaimId":claim_id,"allocationUncertain":false,"targetDispositionPending":false,"creatorPersistenceIncomplete":false,"borrowedTarget":{"host":endpoint.host,"port":endpoint.port,"targetId":target.id(),"browserWSEndpoint":endpoint.browser_ws_endpoint,"claimId":claim_id}}),
    )?;
    target.require(limits.command, cancel).await?;
    if let Some(startup) = startup {
        startup.release()?;
    }
    let config = store.require(id)?["browser"]["config"].clone();
    let resumed = config["resumeConversationUrl"].is_string();
    let _dialogs = dismiss_dialogs(store, id, target.page(), limits, cancel).await?;
    clear_stale_conversation_cookies(store, id, target, &config, limits, cancel).await?;
    if resumed {
        let (url, expected) = navigation_destination(&config)?;
        navigate_exact(target.page(), &url, &expected, limits, cancel).await?;
    }
    let profile = target.lease().profile().path().to_owned();
    ensure_not_blocked(store, id, target.page(), &config, &profile, limits, cancel).await?;
    crate::browser_page::ensure_logged_in(
        target.page(),
        crate::browser_page::LoginHint {
            applied_cookies: 0,
            remote: config["remoteChrome"].is_object(),
        },
        limits.command,
        cancel,
        &|line| page_log(store, id, line),
    )
    .await?;
    // Reference resetWorkConversation: a borrowed tab in Work mode opens the configured chat.
    if !resumed && browser_dom::work_mode_selected(target.page(), limits.command, cancel).await? {
        let (url, expected) = navigation_destination(&config)?;
        navigate_exact(target.page(), &url, &expected, limits, cancel).await?;
        ensure_not_blocked(store, id, target.page(), &config, &profile, limits, cancel).await?;
    }
    let mut reservation = RendererReservation::acquire(
        target.page().clone(),
        claim_id,
        limits.command,
        cancel,
    )
    .await
    .context(
        "This Industrious tab has active or uncertain ownership; choose another browser tab.",
    )?;
    patch(store, id, witness, json!({"recoveryRequired":true}))?;
    runtime_patch(
        store,
        id,
        witness,
        json!({"borrowedCleanup":{"state":"pending","action":"Close or reload this exact borrowed tab if cleanup cannot be acknowledged."}}),
    )?;
    let mut answer = String::new();
    let outcome = async {
        let Delivered { model, thinking, receipt, text, outcome: outcome_witness } = Box::pin(deliver(store, id, witness, &mut committed, target, endpoint, TargetAuthority::Borrowed, Selection::Verify, &config, &mut reservation, bringup, deferred_reconciliation, limits, cancel)).await?;
        // Preserve the answer before cleanup; only an acknowledged renderer release clears recovery.
        complete(store, id, witness,&outcome_witness, |current| {
            Ok(json!({"status":"completed","completedAt":timestamp(),"capturedAnswer":receipt,"browser":merge_object(&current["browser"], &json!({"modelSelection":model,"thinkingSelection":thinking})),"recoveryRequired":true,"dispatch":merge_object(&current["dispatch"], &json!({"state":"completed"}))}))
        },&json!({"modelSelection":model,"thinkingSelection":thinking}),&text,cancel)?;
        answer = text;
        Ok::<_, anyhow::Error>(())
    }.await;
    let settled = store.stored_for_update(id)?;
    // NotStarted proves no dispatch, not restoration of a partially staged payload.
    // On any delivery failure retain the claim for explicit user cleanup of this borrowed tab.
    let released = match &outcome {
        Ok(()) => match reservation
            .release_clean(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = reservation
                    .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
                    .await;
                Err(error)
            }
        },
        Err(_) => {
            reservation
                .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
                .await
        }
    };
    let cleanup = released.and(target.lease().release().map(|_| ()));
    let manual_cleanup = outcome.is_err() || cleanup.is_err();
    store.update(id, |current| {
        witness.require(current)?;
        anyhow::ensure!(current == &settled, "Borrowed outcome changed during renderer cleanup");
        let mut runtime=json!({"borrowedCleanup":if manual_cleanup {
            json!({"state":"manual-required","action":"Close or reload this exact borrowed tab before reuse; staged text or attachments may remain."})
        } else { json!({"state":"released"}) }});
        if let Err(error)=&cleanup {runtime["cleanupWarning"]=json!(error.to_string());}
        // Cleanup may follow a terminal capture; only this exact outcome may receive its disposition.
        Ok(Some(json!({"recoveryRequired":manual_cleanup,"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&runtime)}))})))
    })?;
    outcome.map_err(|error| {
        anyhow::anyhow!(
            "{error:#}; borrowed tab cleanup required: close or reload this exact tab before reuse"
        )
    })?;
    crate::notify::completed(&store.require(id)?, &answer).await;
    store.require(id)
}
#[allow(clippy::too_many_arguments)] // Each original allocation and execution witness must remain explicit.
fn require_pre_dispatch_authority(
    store: &SessionStore,
    witness: &ExecutionWitness,
    target: &OwnedTarget,
    reservation: &RendererReservation,
    journal: &TargetJournal,
    creator: &Value,
    deadline: std::time::Instant,
) -> Result<()> {
    let id = creator["sessionId"]
        .as_str()
        .context("Pre-dispatch creator has no session")?;
    let metadata = store.require(id)?;
    witness.require(&metadata)?;
    let creation = target
        .creation()
        .context("Pre-dispatch target was not newly allocated")?;
    anyhow::ensure!(
        reservation.settled_before_dispatch()
            && reservation.page().same_attachment(target.page())
            && reservation.page().is_open()
            && target.browser().is_open()
            && creation.allocation_id() == creator["allocationId"].as_str().unwrap_or("")
            && creator["target"]["targetId"] == target.id()
            && creator["target"]["claimId"] == reservation.owner().claim_id
            && metadata["status"] == "running"
            && metadata.get("dispatch").is_none()
            && metadata["browser"]["config"]["resumeConversationUrl"].is_null()
            && metadata["browser"]["config"]["browserTabRef"].is_null()
            && metadata["browser"]["runtime"]["creatorPersistenceIncomplete"] == false
            && metadata["browser"]["runtime"]["ownedRecoveryTarget"]["creator"] == *creator,
        "Pre-dispatch settlement or original target authority changed"
    );
    let record = journal.read(&creator["allocationId"])?;
    anyhow::ensure!(
        record["creator"] == *creator
            && record["nativeOwnerIdentity"] == json!(ProcessIdentity::current()?),
        "Pre-dispatch native creator changed"
    );
    let rows = target.lease().profile().records_until(deadline)?;
    let lease = rows["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|lease| lease["id"] == creator["lease"]["id"])
        .context("Pre-dispatch creator lease is missing")?;
    anyhow::ensure!(
        lease["pid"] == creator["controllerPid"]
            && lease["processStartedAtMs"] == creator["controllerStartedAtMs"]
            && lease["processIdentity"] == record["nativeOwnerIdentity"]
            && lease["sessionId"] == creator["sessionId"]
            && lease["chromeTargetId"] == creator["target"]["targetId"]
            && lease["chromePort"] == creator["target"]["port"]
            && lease["chromeHost"] == creator["target"]["host"]
            && lease["targetDispositionAllocationId"] == creator["allocationId"],
        "Pre-dispatch lease owner or allocation changed"
    );
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "Pre-dispatch disposal deadline reached"
    );
    Ok(())
}
pub(crate) fn pre_dispatch_mark_expression() -> String {
    format!(
        "(cfg)=>{{ /* industrious-dom:predispatch-marker */ {} const claim=readOracleTargetClaim(); const url=new URL(location.href);const conversationId=url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)(?=[/]|$)/)?.[1]??null;if(window!==window.top || claim?.id!==cfg.owner.claimId || claim.submission?.id!==cfg.owner.reservationId || (claim.retiring && claim.retirementId!==cfg.retirementId) || claim.submission.state!=='quarantined' || claim.submission.dispatch!=='not-started' || url.origin!==cfg.navigation.origin || conversationId!==cfg.navigation.conversationId)return false;const child=claim.inspection;if(child&&(child.parentReservationId!==cfg.owner.reservationId || !['quarantined','clean'].includes(child.state)))return false;claim.retiring=true;claim.retirementId=cfg.retirementId;writeOracleTargetClaim(claim);return true;}}",
        include_str!("../browser-scripts/ownership-prelude.js")
    )
}
async fn retire_pre_dispatch(
    store: &SessionStore,
    witness: &ExecutionWitness,
    target: &mut OwnedTarget,
    reservation: &RendererReservation,
    journal: &TargetJournal,
    creator: &Value,
) -> Result<()> {
    require_pre_dispatch_authority(
        store,
        witness,
        target,
        reservation,
        journal,
        creator,
        std::time::Instant::now() + Duration::from_secs(1),
    )?;
    let info = target
        .page()
        .call(
            "Target.getTargetInfo",
            json!({}),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await?;
    anyhow::ensure!(
        info["targetInfo"]["targetId"] == target.id() && info["targetInfo"]["type"] == "page",
        "Pre-dispatch target attachment changed"
    );
    let navigation = browser_dom::navigation(
        info["targetInfo"]["url"]
            .as_str()
            .context("Pre-dispatch target URL is missing")?,
    )?;
    let intent = json!({"submissionOwner":reservation.owner(),"executionReservationId":request(store, creator["sessionId"].as_str().context("Missing pre-dispatch session")?)?.reservation_id,
        "navigation":navigation});
    let marker = journal.begin_pre_dispatch_retirement(creator, &intent)?;
    let retiring_target = &mut *target;
    let disposed=journal.retire_settled_before_dispatch(creator,|record_deadline| async move {
        let target=retiring_target;
        let deadline=record_deadline.min(std::time::Instant::now()+Duration::from_secs(1));
        let remaining=|| -> Result<Duration> {
            let remaining=deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(!remaining.is_zero(),"Pre-dispatch disposal deadline reached");
            Ok(remaining)
        };
        require_pre_dispatch_authority(store,witness,target,reservation,journal,creator,deadline)?;
        if target.observe_absence_until(deadline).await? {
            return Ok(("confirmed-absent".into(),"already-absent".into()));
        }
        target.lease().require_until(deadline)?;
        let info=target.page().call("Target.getTargetInfo",json!({}),remaining()?,&CancellationToken::new()).await?;
        anyhow::ensure!(info["targetInfo"]["targetId"]==target.id()&&info["targetInfo"]["type"]=="page","Pre-dispatch target attachment changed");
        let current_navigation=browser_dom::navigation(info["targetInfo"]["url"].as_str().context("Pre-dispatch target URL is missing")?)?;
        anyhow::ensure!(current_navigation == navigation && journal.read(&creator["allocationId"])?["preDispatchRetirement"] == json!({"version":1,"id":marker,"binding":intent}),
            "Pre-dispatch retirement intent or navigation changed");
        let expression=pre_dispatch_mark_expression();
        let checked=async {
        anyhow::ensure!(target.page().evaluate_with_arguments(&expression,
            &json!([{"owner":reservation.owner(),"navigation":navigation,"retirementId":marker}]),remaining()?,&CancellationToken::new()).await?==true,
            "Original pre-dispatch quarantine marker was not acknowledged");
        require_pre_dispatch_authority(store,witness,target,reservation,journal,creator,deadline)?;
        let listed=target.browser().call("Target.getTargets",json!({}),remaining()?,&CancellationToken::new()).await?;
        let targets=listed["targetInfos"].as_array().context("Pre-dispatch target list is incomplete")?;
        anyhow::ensure!(targets.iter().all(|row|row["targetId"].as_str().is_some_and(|id|!id.is_empty()&&id.len()<=256)),
            "Pre-dispatch target list is malformed");
        if !targets.iter().any(|row|row["type"]=="page"&&row["targetId"]!=target.id()) {
            let survivor=target.browser().call("Target.createTarget",json!({"url":"about:blank"}),remaining()?,&CancellationToken::new()).await?;
            anyhow::ensure!(survivor["targetId"].as_str().is_some_and(|id|!id.is_empty()&&id.len()<=256)&&survivor["targetId"]!=target.id(),
                "Pre-dispatch survivor page is unconfirmed");
        }
        let inspect=format!("(cfg)=>{{ /* industrious-dom:predispatch-inspect */ {} const claim=readOracleTargetClaim();const url=new URL(location.href);const conversationId=url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)(?=[/]|$)/)?.[1]??null;return window===window.top && claim?.id===cfg.owner.claimId && claim.retiring===true && claim.retirementId===cfg.retirementId && claim.submission?.id===cfg.owner.reservationId && claim.submission.state==='quarantined' && claim.submission.dispatch==='not-started' && (!claim.inspection || claim.inspection.parentReservationId===cfg.owner.reservationId && ['quarantined','clean'].includes(claim.inspection.state)) && url.origin===cfg.navigation.origin && conversationId===cfg.navigation.conversationId;}}",include_str!("../browser-scripts/ownership-prelude.js"));
        anyhow::ensure!(target.page().evaluate_with_arguments(&inspect,
            &json!([{"owner":reservation.owner(),"navigation":navigation,"retirementId":marker}]),remaining()?,&CancellationToken::new()).await?==true,
            "Original pre-dispatch retirement ownership changed");
        require_pre_dispatch_authority(store,witness,target,reservation,journal,creator,deadline)?;
        Ok::<_,anyhow::Error>(())
        }.await;
        if let Err(error)=checked {
            if let Ok(timeout)=remaining() {
                let rollback=format!("(cfg)=>{{ /* industrious-dom:retirement-rollback */ {} const claim=readOracleTargetClaim();if(claim?.id===cfg.claimId&&claim.retirementId===cfg.retirementId){{claim.retiring=false;delete claim.retirementId;writeOracleTargetClaim(claim);}}}}",include_str!("../browser-scripts/ownership-prelude.js"));
                let _=target.page().evaluate_with_arguments(&rollback,&json!([{"claimId":reservation.owner().claim_id,"retirementId":marker}]),timeout,&CancellationToken::new()).await;
            }
            return Err(error);
        }
        target.close_until(deadline).await?;
        Ok(("confirmed-absent".into(),"settled-pre-dispatch".into()))
    }).await?;
    anyhow::ensure!(
        disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent",
        "Pre-dispatch target absence remains unconfirmed"
    );
    target.lease().profile().release_disposed(
        journal,
        creator,
        &disposed,
        std::time::Instant::now() + Duration::from_secs(5),
        true,
    )?;
    Ok(())
}
fn retire_inputs(store: &SessionStore, id: &str, request: &BrowserRequest) -> Result<()> {
    if let Some(staged) = &request.staged_uploads {
        staged.cleanup(store, id)?;
    }
    if let Some(fallback) = &request.fallback {
        if let Some(remote) = &fallback.remote_source {
            remote.cleanup(store, id)?;
        }
        for source in &fallback.text_sources {
            source.cleanup(store, id)?;
        }
        if let Some(raw) = &fallback.raw_source {
            raw.cleanup(store, id)?;
        }
    }
    Ok(())
}

pub(crate) fn settle_completed_retirement(
    store: &SessionStore,
    journal: &TargetJournal,
    creator: &Value,
    disposed: &Value,
    deadline: std::time::Instant,
) -> Result<()> {
    let record = journal.read(&creator["allocationId"])?;
    anyhow::ensure!(
        record["creator"] == *creator
            && record["disposition"] == *disposed
            && disposed["decision"] == "retired"
            && disposed["absence"] == "confirmed-absent",
        "Completed cleanup has no exact durable absence receipt"
    );
    if !matches!(
        record["commit"]["kind"].as_str(),
        Some("session" | "research-report" | "grok-session")
    ) {
        return Ok(());
    }
    journal.verify(&record)?;
    let id = creator["sessionId"]
        .as_str()
        .context("Completed cleanup has no session")?;
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "Completed cleanup settlement deadline reached"
    );
    let saved = store.stored_for_update(id)?;
    if record.get("nativeOwnerIdentity").is_some() {
        retire_inputs(store, id, &request(store, id)?)?;
    }
    store.update_until(id,deadline,|current| {
        anyhow::ensure!(current==&saved && current["status"]=="completed", "Completed session changed during cleanup settlement");
        Ok(Some(json!({"recoveryRequired":false,"cleanupWarning":null,"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}))}))})))
    })?;
    Ok(())
}
fn capture_image_summary(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &OutcomeWitness,
    receipt: &CaptureReceipt,
    cancel: &CancellationToken,
) -> Result<(CaptureReceipt, String)> {
    let metadata = store.require(id)?;
    outcome.require(&metadata, witness, None, cancel)?;
    let original = String::from_utf8(artifacts::verify_capture(store, receipt)?)?;
    let source = captured_source_text(store, &metadata)?;
    let image = if crate::browser_turns::prompts(&metadata["options"])?.is_empty() {
        metadata["browser"]["runtime"]["imageDelivery"].clone()
    } else {
        let deliveries = turn_image_deliveries(
            store,
            &metadata,
            crate::browser_turns::saved(&metadata)?.len(),
        )?;
        let summary = ImageDelivery::combined_summary(&deliveries, &source);
        anyhow::ensure!(
            crate::browser_images::requested_destination(&metadata["options"])?.is_none()
                || summary["savedImages"]
                    .as_array()
                    .is_some_and(|images| !images.is_empty()),
            "Required browser image delivery is incomplete; preserve the conversation and captured images for recovery."
        );
        outcome.runtime(store, id, witness, cancel, json!({"imageDelivery":summary}))?;
        summary
    };
    let count = image["imageCount"]
        .as_u64()
        .map(usize::try_from)
        .transpose()?
        .unwrap_or(0);
    if count == 0 {
        return Ok((receipt.clone(), original));
    }
    let suffix = image["markdownSuffix"]
        .as_str()
        .context("Generated images have no delivery summary")?;
    anyhow::ensure!(
        !suffix.is_empty()
            || crate::browser_images::requested_destination(&metadata["options"])?.is_none(),
        "Required generated images have no saved delivery suffix"
    );
    let text = image_answer_text(&source, &image)?;
    anyhow::ensure!(
        original == source || original == text,
        "Saved image capture differs from its original source or delivery summary"
    );
    if original == text {
        return Ok((receipt.clone(), text));
    }
    let capture = capture_outcome(store, id, witness, outcome, &text, None, cancel)?;
    Ok((capture, text))
}
const ATTACHMENT_BUDGET_FLOOR: Duration = Duration::from_secs(45);
#[cfg(test)]
thread_local! {
    static TEST_ATTACHMENT_BUDGET_FLOOR: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}
fn attachment_budget_floor() -> Duration {
    #[cfg(test)]
    if let Some(floor) = TEST_ATTACHMENT_BUDGET_FLOOR.get() {
        return floor;
    }
    ATTACHMENT_BUDGET_FLOOR
}
/// Reference attachment completion budget: max(inputTimeoutMs, 45 s) plus 20 s for each
/// further file, raised to `attachmentTimeoutMs`. All of a batch's uploads share it.
fn attachment_budget(config: &Value, limits: &RunnerLimits, files: usize) -> Duration {
    let budget = limits.command.max(attachment_budget_floor())
        + Duration::from_secs(20).saturating_mul(files.saturating_sub(1) as u32);
    budget.max(Duration::from_millis(
        config["attachmentTimeoutMs"].as_u64().unwrap_or(0),
    ))
}
/// Reference isLocalChromeHost: a browser on another host cannot read local paths or write
/// into local download directories.
fn remote_host(endpoint: &AttachmentEndpoint) -> bool {
    browser::normalise_host(&endpoint.host) != "loopback"
}
#[allow(clippy::too_many_arguments)] // Upload receipts belong to this session, renderer and navigation.
async fn upload_inputs(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    reservation: &mut RendererReservation,
    request: &BrowserRequest,
    nav: &Value,
    budget: Duration,
    data_transfer: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let mut expectations = Vec::new();
    let deadline = Instant::now() + budget;
    for (index, path) in request.upload_paths.iter().enumerate() {
        let timeout = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !timeout.is_zero(),
            "Attachment upload budget expired before every file was ready; nothing was sent. Tune --browser-attachment-timeout."
        );
        let display = request.extra["attachmentDisplayPaths"][index]
            .as_str()
            .map_or_else(|| path.display().to_string(), str::to_owned);
        automation_log(store, id, &format!("Uploading attachment: {display}"))?;
        let mut file = if let Some(staged) = &request.staged_uploads {
            staged.open(index)?
        } else {
            UploadFile::open(path)?
        };
        file.data_transfer = data_transfer;
        if data_transfer {
            automation_log(
                store,
                id,
                &format!("Transferring {} to remote browser...", file.name),
            )?;
        }
        let manifest = browser_dom::upload(reservation, &mut file, nav, timeout, cancel).await?;
        if data_transfer {
            automation_log(
                store,
                id,
                &format!(
                    "File transferred: {} ({} bytes)",
                    file.name, manifest["bytes"]
                ),
            )?;
            automation_log(store, id, "Attachment queued")?;
        }
        expectations.push(browser_dom::receipt_expectation(
            &manifest,
            file.generated_bundle,
        )?);
        store.update(id, |current| { witness.require(current)?; let mut uploads=current["browser"]["runtime"]["uploads"].as_array().cloned().unwrap_or_default(); uploads.push(manifest.clone()); Ok(Some(json!({"browser":merge_object(&current["browser"], &json!({"runtime":merge_object(&current["browser"]["runtime"], &json!({"uploads":uploads}))}))}))) })?;
    }
    if !request.upload_paths.is_empty() {
        automation_log(store, id, "All attachments uploaded")?;
    }
    Ok(expectations)
}
fn materialize_fallback(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    request: &mut BrowserRequest,
    observed: &browser_dom::PromptTooLarge,
    cancel: &CancellationToken,
) -> Result<()> {
    let fallback = request
        .fallback
        .as_ref()
        .context("Missing deferred fallback")?;
    if let Some(remote) = &fallback.remote_source {
        remote.verify(store, id)?;
        let files = remote
            .files
            .iter()
            .enumerate()
            .map(|(index, saved)| {
                let mut file = remote.open(index)?;
                Ok(PromptFile {
                    path: remote.paths()?[index].clone(),
                    display_path: saved.display_path.clone(),
                    content: if saved.manifest["name"]
                        .as_str()
                        .is_some_and(|name| crate::browser_prompt::raw_upload(Path::new(name)))
                    {
                        String::new()
                    } else {
                        String::from_utf8_lossy(&file.contents()?).into_owned()
                    },
                    size_bytes: saved.manifest["bytes"]
                        .as_u64()
                        .context("Fallback source has no size")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = BrowserPrompt::remote_fallback(
            &files,
            &fallback.composer_text,
            fallback.pending_bundle,
            &std::env::temp_dir(),
        )?;
        return commit_fallback(store, id, witness, request, observed, plan, cancel);
    }
    let mut files = Vec::new();
    for source in &fallback.text_sources {
        source.verify(store, id)?;
        anyhow::ensure!(
            source.files.len() == 1,
            "Fallback snapshot must retain one original text file"
        );
        let saved = &source.files[0];
        let mut file = source.open(0)?;
        files.push(PromptFile {
            path: source.paths()?.remove(0),
            display_path: saved.display_path.clone(),
            content: String::from_utf8_lossy(&file.contents()?).into_owned(),
            size_bytes: saved.manifest["bytes"]
                .as_u64()
                .context("Fallback source has no size")?,
        });
    }
    if let Some(raw) = fallback
        .raw_source
        .as_ref()
        .or(request.staged_uploads.as_ref())
    {
        raw.verify(store, id)?;
        let paths = raw.paths()?;
        for (index, saved) in raw.files.iter().enumerate() {
            let path = paths[index].clone();
            anyhow::ensure!(
                !saved.generated_bundle && crate::browser_prompt::raw_upload(&path),
                "Inline fallback upload source must be raw media"
            );
            files.push(PromptFile {
                path,
                display_path: saved.display_path.clone(),
                content: String::new(),
                size_bytes: saved.manifest["bytes"]
                    .as_u64()
                    .context("Raw fallback source has no size")?,
            });
        }
    }
    let options = store.require(id)?["options"].clone();
    let mut upload_options = options.clone();
    upload_options["browserAttachments"] = json!("always");
    let plan = BrowserPrompt::assemble(
        &files,
        options["prompt"]
            .as_str()
            .context("Fallback request lacks prompt")?,
        options["system"].as_str(),
        &upload_options,
        &std::env::temp_dir(),
    )?;
    if let Some(pending) = fallback.pending_bundle {
        anyhow::ensure!(
            plan.bundled
                .as_ref()
                .is_some_and(|bundle| bundle.format == pending.format
                    && bundle.original_count
                        == if pending.scope == crate::browser_prompt::BundleScope::All {
                            files.len()
                        } else {
                            fallback.text_sources.len()
                        }),
            "Deferred fallback bundle changed its original scope"
        );
    }
    commit_fallback(store, id, witness, request, observed, plan, cancel)
}
fn commit_fallback(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    request: &mut BrowserRequest,
    observed: &browser_dom::PromptTooLarge,
    mut plan: BrowserPrompt,
    cancel: &CancellationToken,
) -> Result<()> {
    let staged = StagedUploads::capture(store, id, &plan.attachments)?;
    let mut next = request.clone();
    next.prompt = plan.composer_text.clone();
    next.prompt_sha256 = sha256(next.prompt.as_bytes());
    next.upload_paths = staged.paths()?;
    if let Some(bundle) = &mut plan.bundled {
        bundle.bundle_path = next
            .upload_paths
            .first()
            .context("Fallback bundle has no staged path")?
            .clone();
    }
    next.extra
        .insert("bundled".into(), serde_json::to_value(&plan.bundled)?);
    next.extra
        .insert("attachmentMode".into(), json!(plan.attachment_mode));
    next.extra.insert("inlineFileCount".into(), json!(0));
    next.extra.insert(
        "attachmentDisplayPaths".into(),
        json!(
            plan.attachments
                .iter()
                .map(|attachment| &attachment.display_path)
                .collect::<Vec<_>>()
        ),
    );
    next.staged_uploads = Some(staged);
    next.fallback_used = true;
    let attempt =
        crate::browser_attempt::reserve_fallback(store, id, request, &next, observed, cancel)?;
    let path = store.directory(id)?.join("browser-request.json");
    store.update(id,|current| {
        witness.require(current)?;
        anyhow::ensure!(current["status"]=="running"&&current.get("dispatch").is_none(),"Deferred fallback cannot replace a dispatched request");
        let previous:BrowserRequest=serde_json::from_slice(&fs::read(&path)?)?;
        anyhow::ensure!(!cancel.is_cancelled() && serde_json::to_value(&previous)?==serde_json::to_value(&*request)?,"Browser request changed before fallback commitment");
        if let Some(ticket)=&attempt {ticket.require(store,current,&previous,&next,cancel)?;}
        atomic_json(&path,&next)?;
        let mut browser=merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"fallbackSubmissionUsed":true,"fallbackPromptLength":observed.prompt_length,"fallbackObservedLength":observed.observed_length}))}));
        if let Some(ticket)=&attempt {browser["attemptFallback"]=ticket.seal().clone();}
        Ok(Some(json!({"browser":browser})))
    })?;
    plan.cleanup()?;
    *request = next;
    Ok(())
}
async fn wait_receipt(
    reservation: &RendererReservation,
    before: &CommitSnapshot,
    prompt: &str,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    save_conversation: impl FnOnce(&str) -> Result<()>,
) -> Result<CommitReceipt> {
    let deadline = Instant::now() + limits.commit;
    let mut latch = None;
    let error = loop {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Browser operation cancelled; recovery retained"
        );
        match browser_commit::read_receipt(
            reservation.page(),
            reservation.owner(),
            before,
            prompt,
            limits.command,
            cancel,
            &mut latch,
        )
        .await?
        {
            CommitObservation::Committed(receipt) => return Ok(*receipt),
            CommitObservation::Ambiguous(reason) => {
                anyhow::ensure!(
                    !cancel.is_cancelled(),
                    "Browser operation cancelled; recovery retained"
                );
                break anyhow::Error::new(crate::browser_provider::Failure::CommitAmbiguous(
                    reason.into(),
                ));
            }
            CommitObservation::Pending if Instant::now() >= deadline => {
                anyhow::ensure!(
                    !cancel.is_cancelled(),
                    "Browser operation cancelled; recovery retained"
                );
                break anyhow::Error::new(crate::browser_provider::Failure::CommitDeadline);
            }
            CommitObservation::Pending => pause(limits.poll, cancel).await?,
        }
    };
    // The prompt may have been sent; keep its conversation recoverable without resubmission.
    // As in the reference, a failed save leaves the receipt error in charge.
    if let Some(conversation) = latch
        && let Err(saved) = save_conversation(&conversation)
    {
        // Persistence failure is not provider eligibility. Keep the original
        // acknowledgement diagnostic as text without preserving its typed source.
        return Err(anyhow::anyhow!(
            "Conversation URL was not saved: {saved:#}; original acknowledgement error: {error:#}"
        ));
    }
    Err(error)
}
pub(crate) fn result_expression() -> String {
    crate::browser_privacy::guard(&format!(
        "(cfg) => {{\n/* industrious-dom:result */\n{}\n{}\n{}\nconst readResearchCompletion = minTurnIndex => {{ const cfg={{minTurnIndex}}; return ({}); }};\n{}\n}}",
        include_str!("../browser-scripts/ownership-prelude.js"),
        include_str!("../browser-scripts/result-prelude.js"),
        include_str!("../browser-scripts/artifact-prelude.js"),
        include_str!("../browser-scripts/research-completion-poll.js"),
        include_str!("../browser-scripts/result-body.js")
    ))
}
async fn capture_markdown(
    metadata: &Value,
    reservation: &RendererReservation,
    observed: &Value,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<String> {
    crate::browser_privacy::require(reservation.page(), limits.command, cancel).await?;
    if observed["kind"] == "deep-research-report" {
        let current = crate::browser_research::observe_report(reservation.page(), metadata,
            &json!({"owner":reservation.owner(),"receipt":metadata["browser"]["runtime"]["userCommitReceipt"]}),
            limits.command, cancel).await?.report.context("Completed research report disappeared before capture")?;
        let mut expected = observed.clone();
        crate::browser_privacy::require(reservation.page(), limits.command, cancel).await?;
        expected
            .as_object_mut()
            .context("Invalid report observation")?
            .remove("completionConfirmed");
        anyhow::ensure!(
            crate::browser_research::source_evidence(&current)
                == crate::browser_research::source_evidence(&expected)
                && current["stopExists"] == false
                && current["strongThinkingActive"] == false
                && current["completionVisible"] == true,
            "Research report changed before capture"
        );
        return Ok(current["text"]
            .as_str()
            .context("Completed report text is unavailable")?
            .to_owned());
    }
    let meta = json!({"messageId":observed["lastAssistantMessageId"],"turnId":observed["lastAssistantTurnId"]});
    let receipt = json!({"conversation":{"conversationId":observed["conversationId"],"origin":observed["origin"]}});
    let copied = reservation
        .page()
        .evaluate_with_arguments(
            &crate::browser_privacy::guard(include_str!("../browser-scripts/copy-markdown.js")),
            &json!([{"owner":reservation.owner(),"meta":meta,"receipt":receipt}]),
            limits.command,
            cancel,
        )
        .await?;
    anyhow::ensure!(
        copied["status"] != "conversation-changed",
        "Provider navigation changed during Markdown capture"
    );
    let native_receipt = observed["lastUserMessageId"]
        .as_str()
        .context("Missing owned user identity")?;
    // Recheck the exact original source and assistant identities after the browser copy operation.
    // The full commit receipt comes from the positively acknowledged renderer reservation.
    let function = format!(
        "(cfg)=>{{ {} const claim=readOracleTargetClaim();if(!matchesOracleSubmission(claim,cfg.owner)||claim.submission.dispatch!=='acknowledged')throw Error('Copy ownership changed');return claim.submission.commitReceipt; }}",
        include_str!("../browser-scripts/ownership-prelude.js")
    );
    let full_receipt = reservation
        .page()
        .evaluate_with_arguments(
            &function,
            &json!([{"owner":reservation.owner()}]),
            limits.command,
            cancel,
        )
        .await?;
    anyhow::ensure!(
        full_receipt["messageId"] == native_receipt,
        "Copy commitment changed"
    );
    let current = reservation
        .page()
        .evaluate_with_arguments(
            &result_expression(),
            &json!([{"owner":reservation.owner(),"receipt":full_receipt}]),
            limits.command,
            cancel,
        )
        .await?;
    anyhow::ensure!(
        current["lastAssistantMessageId"] == observed["lastAssistantMessageId"]
            && current["lastAssistantTurnId"] == observed["lastAssistantTurnId"]
            && current["lastUserOriginalSourceText"] == observed["lastUserOriginalSourceText"]
            && current["completionVisible"] == true
            && current["stopExists"] == false
            && current["strongThinkingActive"] == false,
        "Source, assistant or completion changed during Markdown capture"
    );
    if copied["success"] == true {
        let markdown = copied["markdown"]
            .as_str()
            .context("Copy reported success without Markdown bytes")?;
        let markdown = browser::normalise_image_answer(
            markdown,
            observed["images"].as_array().map_or(0, Vec::len),
        );
        anyhow::ensure!(
            !policy::trim(&markdown).is_empty(),
            "Copy reported success without Markdown bytes"
        );
        Ok(markdown)
    } else {
        anyhow::ensure!(
            matches!(
                copied["status"].as_str(),
                Some("missing-button" | "timeout")
            ),
            "Unknown Markdown capture outcome; preserve recovery"
        );
        Ok(observed["text"]
            .as_str()
            .context("Owned fallback text is unavailable")?
            .into())
    }
}
#[allow(clippy::too_many_arguments)] // Recovery retains all original dispatch and allocation witnesses.
async fn wait_original_answer(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    authority: TargetAuthority<'_>,
    reservation: &RendererReservation,
    receipt: &CommitReceipt,
    selection: &Value,
    recovery: &mut read_recovery::Budget,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Value> {
    if crate::browser_images::requested_destination(&store.require(id)?["options"])?.is_some() {
        automation_log(
            store,
            id,
            "[browser] Waiting for ChatGPT generated image response.",
        )?;
    }
    let context = read_recovery::Context::capture(
        store,
        id,
        witness,
        committed,
        target,
        endpoint,
        authority,
        reservation,
    )?;
    let bounded = recovery.limits(limits);
    let initial = recovery
        .answer_within(wait_answer(
            store,
            id,
            reservation,
            receipt,
            selection,
            &bounded,
            false,
            Some(witness),
            cancel,
        ))
        .await;
    let observed = recovery
        .observe(initial, context, limits, cancel, |bounded| async move {
            wait_answer(
                store,
                id,
                reservation,
                receipt,
                selection,
                &bounded,
                false,
                Some(witness),
                cancel,
            )
            .await
        })
        .await;
    match observed {
        // Reference createAssistantTimeoutError; the typed deadline stays the source.
        Err(error)
            if matches!(
                error.downcast_ref::<crate::browser_provider::Failure>(),
                Some(crate::browser_provider::Failure::AnswerDeadline)
            ) && store.require(id)?["browser"]["config"]["researchMode"] != "deep" =>
        {
            match crate::browser_page::ui_warning(
                reservation.page(),
                "the assistant",
                limits.command,
                cancel,
                &|line| page_log(store, id, line),
            )
            .await
            {
                Some(warning) => Err(error.context(warning)),
                None => Err(error),
            }
        }
        other => other,
    }
}
/// How many command budgets the answer wait rides out page reads that give no result (10 minutes
/// at the default 60 s). ChatGPT keeps generating meanwhile, and giving up loses the answer.
const PAGE_STALL_COMMANDS: u32 = 10;
/// Rides out a stalled page read. Only reads pass through here, and each poll repeats them anyway,
/// so nothing is sent again. Privacy is still verified before any later read is accepted.
struct PageStalls {
    since: Option<Instant>,
    window: Duration,
}
impl PageStalls {
    /// The read's value, or `None` to poll again; the stall's error once it outlasts the window.
    fn check<T>(&mut self, read: Result<T>) -> Result<Option<T>> {
        match read {
            Ok(value) => Ok(Some(value)),
            Err(error)
                if error
                    .downcast_ref::<crate::cdp::PageReadStalled>()
                    .is_some() =>
            {
                let since = *self.since.get_or_insert_with(Instant::now);
                if since.elapsed() >= self.window {
                    return Err(error);
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}
#[allow(clippy::too_many_arguments)] // Acceptance binds source, selection, ownership and the polling deadline.
async fn wait_answer(
    store: &SessionStore,
    id: &str,
    reservation: &RendererReservation,
    receipt: &CommitReceipt,
    selection: &Value,
    limits: &RunnerLimits,
    recovery: bool,
    witness: Option<&ExecutionWitness>,
    cancel: &CancellationToken,
) -> Result<Value> {
    let started = Instant::now();
    let mut gate = TerminalGate::default();
    let mut stalls = PageStalls {
        since: None,
        window: limits.command * PAGE_STALL_COMMANDS,
    };
    let metadata = store.require(id)?;
    let deep = metadata["browser"]["config"]["researchMode"] == "deep";
    let mut monitor = crate::browser_log::ThinkingMonitor::new(&metadata["options"]);
    // A provider attempt reports progress in its consultation's user-visible log.
    let progress = metadata["browser"]["attempt"]["parentId"]
        .as_str()
        .unwrap_or(id)
        .to_owned();
    let mut research_observed = false;
    let mut plan_seen = metadata["browser"]["runtime"]["researchPlan"].clone();
    // Reference monitorDeepResearch progress lines for a new research turn.
    let narrate = deep && !recovery;
    let mut progress_logged = started;
    if narrate {
        automation_log(
            store,
            id,
            &format!(
                "Monitoring Deep Research (timeout: {}min)...",
                (limits.answer.as_secs_f64() / 60.0).round()
            ),
        )?;
    }
    loop {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Browser operation cancelled; recovery retained"
        );
        if started.elapsed() >= limits.answer {
            let failure = anyhow::Error::from(crate::browser_provider::Failure::AnswerDeadline);
            // The typed deadline stays the source, so recovery eligibility is unchanged.
            return Err(if deep {
                failure.context(crate::browser_research::deadline_message(
                    limits.answer,
                    started.elapsed(),
                ))
            } else {
                failure
            });
        }
        let verified =
            crate::browser_privacy::require(reservation.page(), limits.command, cancel).await;
        if stalls.check(verified)?.is_none() {
            pause(limits.poll, cancel).await?;
            continue;
        }
        let cfg = json!({"owner":reservation.owner(),"receipt":receipt,"recovery":recovery,"cleanup":reservation.state()==crate::browser_renderer::DispatchState::Released,"uiErrors":true});
        let report = if deep {
            let metadata = store.require(id)?;
            let read = crate::browser_research::observe_report(
                reservation.page(),
                &metadata,
                &cfg,
                limits.command,
                cancel,
            )
            .await?;
            let activity = crate::browser_research::activity_receipt(&metadata);
            if narrate && read.research_observed && !research_observed {
                automation_log(
                    store,
                    id,
                    "[browser] Deep Research activity detected; waiting for plan auto-confirm...",
                )?;
            }
            research_observed |= read.research_observed
                || metadata["browser"]["runtime"]["researchActivityReceipt"] == activity;
            if read.research_observed
                && metadata["browser"]["runtime"]["researchActivityReceipt"] != activity
                && let Some(witness) = witness
            {
                store.update(id, |current| {
                    witness.require(current)?;
                    anyhow::ensure!(current["status"] == "running"
                        && crate::browser_research::activity_receipt(current) == activity,
                        "Research submission changed before recording activity");
                    Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"researchActivityReceipt":activity}))}))})))
                })?;
            }
            if let Some(plan) = &read.plan
                && let Some(next) = crate::browser_research::plan_record(&plan_seen, plan)
            {
                let previous = std::mem::replace(&mut plan_seen, next.clone());
                let log = store.require(&progress)?;
                if previous.is_null() {
                    let line = crate::browser_research::plan_log(&next);
                    store.append_log(&progress, log["model"].as_str(), &format!("{line}\n"))?;
                }
                if next["phase"] == "researching" && previous["phase"] != "researching" {
                    store.append_log(
                        &progress,
                        log["model"].as_str(),
                        "[browser] Deep Research execution started; plan countdown is complete.\n",
                    )?;
                }
                if let Some(witness) = witness {
                    store.update(id, |current| {
                            witness.require(current)?;
                            anyhow::ensure!(current["status"] == "running", "Research submission changed before recording its plan");
                            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&json!({"researchPlan":next}))}))})))
                        })?;
                }
            }
            // shortcut: the phase and character count come from the report frames only; the
            // reference also reads the main page's stop control and text length.
            if narrate && progress_logged.elapsed() >= Duration::from_secs(60) {
                progress_logged = Instant::now();
                automation_log(
                    store,
                    id,
                    &format!(
                        "Deep Research {}... {}s elapsed, ~{} chars",
                        if read.pending_research {
                            "researching"
                        } else {
                            "waiting"
                        },
                        started.elapsed().as_secs_f64().round(),
                        read.report.as_ref().map_or(0, |report| report["text"]
                            .as_str()
                            .map_or(0, |text| text.encode_utf16().count()))
                    ),
                )?;
            }
            if read.pending_research {
                Some(json!({"pending":true}))
            } else {
                if narrate && read.report.is_some() {
                    automation_log(
                        store,
                        id,
                        &format!(
                            "Deep Research completed ({}s elapsed)",
                            started.elapsed().as_secs_f64().round()
                        ),
                    )?;
                }
                read.report
            }
        } else {
            None
        };
        let mut observed = if let Some(report) = report {
            report
        } else {
            let read = reservation
                .page()
                .evaluate_with_arguments(
                    &result_expression(),
                    &json!([cfg]),
                    limits.command,
                    cancel,
                )
                .await;
            let Some(read) = stalls.check(read)? else {
                pause(limits.poll, cancel).await?;
                continue;
            };
            read
        };
        stalls.since = None;
        if observed["uiError"] == "temporary_unavailable" {
            return Err(crate::browser_provider::Failure::RetryOffered.into());
        }
        if let Some(line) = monitor.line(started.elapsed().as_millis() as u64, &observed) {
            let log = store.require(&progress)?;
            store.append_log(&progress, log["model"].as_str(), &format!("{line}\n"))?;
        }
        if deep
            && observed["kind"] != "deep-research-report"
            && observed["withheld"] != true
            && observed["text"].as_str().is_some_and(|text| {
                text.encode_utf16().count() < 40 || crate::browser_research::incomplete_text(text)
            })
        {
            observed = json!({"pending":true});
        }
        if observed["pending"] != true {
            observed["providerText"] = observed["text"].clone();
            observed["images"] = json!(crate::browser_images::normalise_inventory(
                observed["images"]
                    .as_array()
                    .context("Owned image inventory is unavailable")?
            )?);
            if let Some(text) = observed["text"].as_str() {
                observed["text"] = json!(browser::normalise_image_answer(
                    text,
                    observed["images"].as_array().map_or(0, Vec::len)
                ));
            }
            let text = observed["text"]
                .as_str()
                .context("Owned result sample is malformed")?;
            let sample = TerminalSample {
                now_ms: started.elapsed().as_millis() as u64,
                length: text.len(),
                content_key: sha256(&serde_json::to_vec(&json!([
                    observed["lastAssistantMessageId"],
                    observed["lastAssistantTurnId"],
                    observed["report"],
                    text,
                    observed["html"]
                ]))?),
                stop_visible: observed["stopExists"] != false,
                bar_visible: observed["completionVisible"] == true,
                strong_thinking_active: observed["strongThinkingActive"] != false,
            };
            if gate.observe(&sample, limits.confirm_cycles, limits.stable_ms) {
                anyhow::ensure!(
                    !deep || research_observed,
                    "deep-research-not-started: ChatGPT completed a normal answer without starting Deep Research; recover the retained target without sending again"
                );
                observed["completionConfirmed"] = json!(true);
                browser::accept_answer(&store.require(id)?, &observed, selection)?;
                anyhow::ensure!(
                    observed["lastUserMessageId"] == receipt.message_id,
                    "Result refers to a different committed user message"
                );
                return Ok(observed);
            }
        }
        pause(limits.poll, cancel).await?;
    }
}
async fn mark_completed_target(
    target: &OwnedTarget,
    reservation: &RendererReservation,
    expected_conversation: &str,
    marker: &str,
    timeout: Duration,
) -> Result<String> {
    let cancel = CancellationToken::new();
    target.require(timeout, &cancel).await?;
    let function = format!(
        "(cfg) => {{ /* industrious-dom:retire */ {} const claim=readOracleTargetClaim(); const url=new URL(location.href); if(window!==window.top || !claim || claim.id!==cfg.claimId || (claim.retiring && claim.retirementId!==cfg.retirementId) || claim.submission || claim.inspection || url.origin!=='https://chatgpt.com' || url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)/)?.[1]!==cfg.conversationId) return false; const generating=Array.from(document.querySelectorAll('[data-testid=\"stop-button\"], [data-testid=\"composer-stop-button\"], form button[aria-label*=\"stop\" i]:not([aria-label*=\"dictat\" i]):not([aria-label*=\"voice\" i]):not([aria-label*=\"read\" i])')).some(node=>{{ const rect=node.getBoundingClientRect(); return rect.width>0 && rect.height>0; }}); if(generating) return false; claim.retiring=true; claim.retirementId=cfg.retirementId; writeOracleTargetClaim(claim); return true; }}",
        include_str!("../browser-scripts/ownership-prelude.js")
    );
    let info = target.require(timeout, &cancel).await?;
    let conversation = info["url"]
        .as_str()
        .and_then(browser::conversation_id)
        .context("Target retirement has no exact provider conversation")?;
    anyhow::ensure!(
        conversation == expected_conversation,
        "Target conversation changed after capture; preserving it"
    );
    let cfg = json!({"claimId":reservation.owner().claim_id,"conversationId":expected_conversation,"retirementId":marker});
    anyhow::ensure!(
        target
            .page()
            .evaluate_with_arguments(&function, &json!([cfg]), timeout, &cancel)
            .await?
            == true,
        "Exact renderer target retirement was refused; preserving recovery"
    );
    Ok(marker.to_owned())
}
#[allow(clippy::too_many_arguments)] // Final authorisation binds the durable creator and exact renderer marker.
async fn verify_marked_retirement(
    store: &SessionStore,
    id: &str,
    target: &OwnedTarget,
    reservation: &RendererReservation,
    creator: &Value,
    commit: &Value,
    journal: &TargetJournal,
    marker: &str,
    timeout: Duration,
) -> Result<()> {
    let meta = store.require(id)?;
    let cfg = json!({"owner":reservation.owner(),"receipt":meta["browser"]["runtime"]["userCommitReceipt"],"retirementId":marker});
    let observed = observe_retirement(reservation.page(), &meta, &cfg, commit, timeout).await?;
    require_retirement_content(&meta, &observed, commit)?;
    require_retirement_authority(store, id, target, creator, commit, journal)
}
async fn observe_retirement(
    page: &Cdp,
    metadata: &Value,
    cfg: &Value,
    commit: &Value,
    timeout: Duration,
) -> Result<Value> {
    Ok(if commit["kind"] == "research-report" {
        crate::browser_research::observe_report(
            page,
            metadata,
            cfg,
            timeout,
            &CancellationToken::new(),
        )
        .await?
        .report
        .context("Committed research report disappeared before retirement")?
    } else {
        page.evaluate_with_arguments(
            &result_expression(),
            &json!([cfg]),
            timeout,
            &CancellationToken::new(),
        )
        .await?
    })
}
#[allow(clippy::too_many_arguments)] // Rollback releases only the exact marker before any close is issued.
async fn close_marked_target(
    store: &SessionStore,
    id: &str,
    target: &mut OwnedTarget,
    reservation: &RendererReservation,
    creator: &Value,
    commit: &Value,
    journal: &TargetJournal,
    marker: &str,
    timeout: Duration,
) -> Result<()> {
    let result = verify_marked_retirement(
        store,
        id,
        target,
        reservation,
        creator,
        commit,
        journal,
        marker,
        timeout,
    )
    .await;
    if result.is_err() {
        let rollback = format!(
            "(cfg) => {{ /* industrious-dom:retirement-rollback */ {} const claim=readOracleTargetClaim(); if(claim?.id===cfg.claimId && claim.retirementId===cfg.retirementId) {{ claim.retiring=false; delete claim.retirementId; writeOracleTargetClaim(claim); }} }}",
            include_str!("../browser-scripts/ownership-prelude.js")
        );
        let _ = target
            .page()
            .evaluate_with_arguments(
                &rollback,
                &json!([{"claimId":reservation.owner().claim_id,"retirementId":marker}]),
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await;
    }
    result?;
    target.close_observed(timeout).await
}
fn require_retirement_content(metadata: &Value, observed: &Value, commit: &Value) -> Result<()> {
    if commit["kind"] == "research-report" {
        crate::browser_research::require_report(metadata, observed)?;
        anyhow::ensure!(
            observed["report"] == commit["report"]
                && observed["stopExists"] == false
                && observed["strongThinkingActive"] == false
                && observed["completionVisible"] == true
                && commit["reportContent"]["representation"] == "text"
                && observed["report"]["textSha256"] == commit["reportContent"]["sha256"],
            "Completed research report changed before retirement"
        );
        return Ok(());
    }
    browser::require_original_source_pair(metadata, observed)?;
    anyhow::ensure!(observed["stopExists"]==false&&observed["strongThinkingActive"]==false&&observed["completionVisible"]==true&&browser::assistant_identity(observed).is_some_and(|id|id==&commit["assistantMessageId"])&&commit["assistantContent"]["representation"]=="text"&&observed["text"].as_str().is_some_and(|text|sha256(text.as_bytes())==commit["assistantContent"]["sha256"]),"Completed assistant content changed before retirement");
    Ok(())
}
fn require_retirement_authority(
    store: &SessionStore,
    id: &str,
    target: &OwnedTarget,
    creator: &Value,
    commit: &Value,
    journal: &TargetJournal,
) -> Result<()> {
    let current = store.require(id)?;
    journal.verify(&json!({"creator":creator,"commit":commit}))?;
    anyhow::ensure!(
        current["browser"]["runtime"]["ownedRecoveryTarget"]["creator"] == *creator,
        "Completed target creator changed"
    );
    let actual = ProcessIdentity::current()?;
    for pid in [
        current["lifecycle"]["workerPid"].as_u64(),
        current["browser"]["runtime"]["controllerPid"].as_u64(),
    ]
    .into_iter()
    .flatten()
    {
        if pid != u64::from(actual.pid) {
            let pid = u32::try_from(pid)?;
            anyhow::ensure!(
                ProcessIdentity::read(pid).is_ok_and(|identity| identity.is_none()),
                "Another live or unknown controller owns this result"
            );
        }
    }
    for other in store.list(true)? {
        if other["id"] == id {
            continue;
        }
        let runtime = &other["browser"]["runtime"];
        if runtime["chromeTargetId"] != creator["target"]["targetId"]
            || runtime["chromePort"] != creator["target"]["port"]
            || runtime["chromeHost"].as_str().is_none_or(|host| {
                browser::normalise_host(host)
                    != browser::normalise_host(creator["target"]["host"].as_str().unwrap_or(""))
            })
        {
            continue;
        }
        for pid in [
            runtime["controllerPid"].as_u64(),
            other["lifecycle"]["workerPid"].as_u64(),
        ]
        .into_iter()
        .flatten()
        {
            anyhow::ensure!(
                ProcessIdentity::read(u32::try_from(pid)?).is_ok_and(|identity| identity.is_none()),
                "Another session controller owns this target"
            );
        }
    }
    let leases = target.lease().profile().records()?;
    let mut own_lease_seen = false;
    for lease in leases["leases"].as_array().into_iter().flatten() {
        if lease["chromeTargetId"] == target.id()
            && lease["chromePort"] == creator["target"]["port"]
            && lease["chromeHost"].as_str().is_some_and(|host| {
                browser::normalise_host(host)
                    == browser::normalise_host(creator["target"]["host"].as_str().unwrap_or(""))
            })
        {
            anyhow::ensure!(
                lease["id"] == creator["lease"]["id"]
                    && lease["pid"] == creator["controllerPid"]
                    && lease["processStartedAtMs"] == creator["controllerStartedAtMs"]
                    && lease["sessionId"] == creator["sessionId"]
                    && lease["targetDispositionAllocationId"] == creator["allocationId"],
                "Another or replaced lease owns this target"
            );
            own_lease_seen = true;
        }
    }
    anyhow::ensure!(
        own_lease_seen,
        "Exact creator lease no longer names this target"
    );
    Ok(())
}
async fn retire_recovered_target(
    target: &mut OwnedTarget,
    reservation: &mut RendererReservation,
    conversation: &str,
    timeout: Duration,
) -> Result<()> {
    mark_recovered_target(
        target,
        reservation,
        conversation,
        &Uuid::new_v4().to_string(),
        timeout,
    )
    .await?;
    target.close_observed(timeout).await
}
async fn mark_recovered_target(
    target: &OwnedTarget,
    reservation: &mut RendererReservation,
    conversation: &str,
    marker: &str,
    timeout: Duration,
) -> Result<String> {
    if reservation.state() == crate::browser_renderer::DispatchState::Released {
        return mark_completed_target(target, reservation, conversation, marker, timeout).await;
    }
    reservation
        .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
        .await?;
    let function = format!(
        "(cfg) => {{ /* industrious-dom:retire-quarantine */ if(window!==window.top)return false; {} const claim=readOracleTargetClaim(); if(!matchesOracleSubmission(claim,cfg.owner)||(claim.retiring && claim.retirementId!==cfg.retirementId)||!['quarantined','releasing'].includes(claim.submission.state))return false; const child=claim.inspection; if(child && (child.parentReservationId!==cfg.owner.reservationId || !['quarantined','clean'].includes(child.state)))return false; const url=new URL(location.href); if(url.origin!=='https://chatgpt.com'||url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)/)?.[1]!==cfg.conversationId)return false; claim.retiring=true;claim.retirementId=cfg.retirementId;writeOracleTargetClaim(claim);return true; }}",
        include_str!("../browser-scripts/ownership-prelude.js")
    );
    anyhow::ensure!(target.page().evaluate_with_arguments(&function,&json!([{"owner":reservation.owner(),"conversationId":conversation,"retirementId":marker}]),timeout,&CancellationToken::new()).await?==true,"Exact recovered-target quarantine retirement refused");
    Ok(marker.to_owned())
}
#[allow(clippy::too_many_arguments)] // Recovery keeps the same durable creator, renderer and target proof.
async fn retire_recovered_journaled(
    store: &SessionStore,
    id: &str,
    target: &mut OwnedTarget,
    reservation: &mut RendererReservation,
    conversation: &str,
    journal: &TargetJournal,
    creator: &Value,
    timeout: Duration,
) -> Result<()> {
    let completed = store.require(id)?;
    let commit = journal.commit(creator, &journal.session_commit(creator, &completed)?)?;
    journal.finalise(&commit)?;
    let result=journal.retire_finalised(creator,|commit| {
        let commit=commit.clone();let target=&mut *target;let reservation=&mut *reservation;
        async move {
            require_retirement_authority(store,id,target,creator,&commit,journal)?;
            let meta=store.require(id)?;
            let observed=observe_retirement(reservation.page(),&meta,&json!({"owner":reservation.owner(),"receipt":meta["browser"]["runtime"]["userCommitReceipt"],"recovery":true,"cleanup":reservation.state()==crate::browser_renderer::DispatchState::Released}),&commit,timeout).await?;
            require_retirement_content(&meta,&observed,&commit)?;
            require_retirement_authority(store,id,target,creator,&commit,journal)?;
            let marker=journal.retirement_id(creator,&commit)?;
            mark_recovered_target(target,reservation,conversation,&marker,timeout).await?;
            close_marked_target(store,id,target,reservation,creator,&commit,journal,&marker,timeout).await?;
            Ok(("confirmed-absent".into(),"exact-target-absent".into()))
        }
    }).await?;
    anyhow::ensure!(
        result["decision"] == "retired" && result["absence"] == "confirmed-absent",
        "Recovery retirement remains pending"
    );
    target.release_retired_lease()
}
/// How long a cancelled, possibly sent run may keep its slot while its reply settles
/// (reference `CANCELLED_SEND_SETTLE_MS`).
const CANCELLED_SEND_SETTLE: Duration = Duration::from_secs(15);
#[cfg(test)]
thread_local! {
    static TEST_CANCELLED_SEND_SETTLE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}
fn cancelled_send_settle() -> Duration {
    #[cfg(test)]
    if let Some(settle) = TEST_CANCELLED_SEND_SETTLE.get() {
        return settle;
    }
    CANCELLED_SEND_SETTLE
}
/// Frees a cancelled run's slot without resubmitting (reference `retireCancelledSendTarget`,
/// 15c8b9a). Waits read-only, at most `settle`, for any reply to stop generating (Industrious does
/// not press Stop), then closes only this run's quarantined tab. The sent conversation stays in
/// ChatGPT under its saved URL. Only confirmed absence frees the slot; otherwise both remain.
#[allow(clippy::too_many_arguments)] // The exact creator, renderer owner and target bind one close.
async fn retire_cancelled_send(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    target: &mut OwnedTarget,
    reservation: &RendererReservation,
    journal: &TargetJournal,
    creator: &Value,
    settle: Duration,
    timeout: Duration,
) -> Result<()> {
    let current = store.require(id)?;
    witness.require(&current)?;
    let runtime = &current["browser"]["runtime"];
    let sent = runtime
        .get("userCommitReceipt")
        .is_some_and(|r| !r.is_null());
    let mut conversation = runtime["conversationId"].as_str().map(str::to_owned);
    if conversation.is_none() {
        // An unconfirmed send keeps the conversation its tab last reached (reference lastUrl).
        conversation = target.require(timeout, &CancellationToken::new()).await?["url"]
            .as_str()
            .and_then(browser::conversation_id);
    }
    let record = journal.read(&creator["allocationId"])?;
    anyhow::ensure!(
        current["status"] == "running"
            && runtime["ownedRecoveryTarget"]["creator"] == *creator
            && runtime["chromeTargetId"] == target.id()
            && record["creator"] == *creator
            && record["commit"].is_null()
            && reservation.state() == crate::browser_renderer::DispatchState::Quarantined,
        "Cancelled target creator, session or renderer owner changed"
    );
    let probe = "() => { /* industrious-dom:cancelled-settle */ return Array.from(document.querySelectorAll('[data-testid=\"stop-button\"], [data-testid=\"composer-stop-button\"], form button[aria-label*=\"stop\" i]:not([aria-label*=\"dictat\" i]):not([aria-label*=\"voice\" i]):not([aria-label*=\"read\" i])')).some(node=>{ const rect=node.getBoundingClientRect(); return rect.width>0 && rect.height>0; }); }";
    let deadline = std::time::Instant::now() + settle;
    let mut settled = false;
    let quiet = CancellationToken::new();
    while std::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        // An unreadable tab counts as still generating until the bound expires.
        let generating = target
            .page()
            .evaluate_with_arguments(probe, &json!([]), left.min(timeout), &quiet)
            .await
            .map_or(true, |value| value != false);
        if !generating {
            settled = true;
            break;
        }
        tokio::time::sleep(
            Duration::from_secs(1)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        )
        .await;
    }
    let close = async {
        let retire = format!(
            "(cfg) => {{ /* industrious-dom:retire-cancelled-quarantine */ if(window!==window.top)return false; {} const claim=readOracleTargetClaim(); if(!matchesOracleSubmission(claim,cfg.owner)||(claim.retiring && claim.retirementId!==cfg.retirementId)||!['quarantined','releasing'].includes(claim.submission.state))return false; const child=claim.inspection; if(child && (child.parentReservationId!==cfg.owner.reservationId || !['quarantined','clean'].includes(child.state)))return false; const url=new URL(location.href); const reached=url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)/)?.[1]??null; if(url.origin!=='https://chatgpt.com'||reached!==cfg.conversationId)return false; claim.retiring=true;claim.retirementId=cfg.retirementId;writeOracleTargetClaim(claim);return true; }}",
            include_str!("../browser-scripts/ownership-prelude.js")
        );
        let marker = Uuid::new_v4().to_string();
        anyhow::ensure!(
            target
                .page()
                .evaluate_with_arguments(
                    &retire,
                    &json!([{"owner":reservation.owner(),"conversationId":conversation,"retirementId":marker}]),
                    timeout,
                    &quiet
                )
                .await?
                == true,
            "Cancelled target quarantine retirement was refused"
        );
        target.close_observed(timeout).await
    }
    .await;
    let outcome = if settled { "settled" } else { "timed-out" };
    let reason = format!(
        "{}-{outcome}",
        if sent {
            "cancelled-after-send"
        } else {
            "cancelled-send-unconfirmed"
        }
    );
    journal
        .record_disposition(
            creator,
            if close.is_ok() {
                "confirmed-absent"
            } else {
                "unknown"
            },
            &reason,
        )
        .context("Cancelled target disposition was not recorded")?;
    close.context("Cancelled target close is unconfirmed")?;
    target.release_retired_lease()?;
    runtime_patch(
        store,
        id,
        witness,
        json!({"targetDispositionPending":false,"targetRetiredAt":timestamp(),"cancelledTargetDisposition":reason}),
    )?;
    eprintln!(
        "[browser] Cancelled run {} {}; closed its tab without resubmitting and released its browser slot.",
        if sent { "(sent)" } else { "(send unconfirmed)" },
        if settled {
            "was idle"
        } else {
            "was still generating after the wait"
        }
    );
    Ok(())
}
async fn pause(duration: Duration, cancel: &CancellationToken) -> Result<()> {
    tokio::select! { biased; _=cancel.cancelled()=>bail!("Browser operation cancelled; recovery retained"), _=tokio::time::sleep(duration)=>Ok(()) }
}

pub fn captured(store: &SessionStore, id: &str) -> Result<Vec<u8>> {
    let metadata = store.require(id)?;
    let receipt: CaptureReceipt = serde_json::from_value(
        metadata["browser"]
            .get("capturedAnswer")
            .unwrap_or(&metadata["capturedAnswer"])
            .clone(),
    )
    .context("No positively accepted owned capture")?;
    artifacts::verify_capture(store, &receipt)
}
#[allow(clippy::too_many_arguments)] // Publication binds the same native and renderer witnesses as answer capture.
async fn collect_deliveries(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &OutcomeWitness,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    reservation: &RendererReservation,
    observed: &Value,
    limits: &RunnerLimits,
    recovery: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let metadata = store.require(id)?;
    outcome.require(&metadata, witness, None, cancel)?;
    let requested = crate::browser_images::requested_destination(&metadata["options"])?.is_some();
    let followups = crate::browser_turns::prompts(&metadata["options"])?;
    let optional_images = !followups.is_empty()
        && (crate::browser_turns::index(&metadata)? < followups.len()
            || !prior_turn_images(store, &metadata)?.is_empty());
    let from_current_response = |artifact: &Value| {
        artifact["kind"] == "image"
            && artifact["origin"]["targetId"] == target.id()
            && artifact["origin"]["assistantMessageId"] == observed["lastAssistantMessageId"]
            && artifact["origin"]["assistantTurnId"] == observed["lastAssistantTurnId"]
    };
    anyhow::ensure!(
        !recovery
            || metadata["browser"]["runtime"]["downloadContextSettled"] == true
            || metadata["browser"]["runtime"]["downloadContextSettled"].is_null()
                && metadata["browser"]["runtime"]["downloadStage"].is_null(),
        "Native download ownership is unsettled; preserve the target and staging for explicit reconciliation without replaying a click"
    );
    let mut direct_failed = false;
    let saved_buttons = metadata["artifacts"].as_array().is_some_and(|artifacts| {
        artifacts
            .iter()
            .any(|artifact| from_current_response(artifact) && artifact["imageButton"] == true)
    });
    if !requested
        || !saved_buttons
            && ["images", "files"].iter().any(|field| {
                observed[*field]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            })
    {
        let saved = collect_delivery_inventory(
            store,
            id,
            witness,
            outcome,
            target,
            endpoint,
            reservation,
            observed,
            limits,
            recovery,
            false,
            cancel,
        )
        .await?;
        if !requested || saved.iter().any(&from_current_response) {
            return Ok(saved);
        }
        direct_failed = observed["images"]
            .as_array()
            .is_some_and(|images| !images.is_empty());
    }
    let receipt: CommitReceipt =
        serde_json::from_value(metadata["browser"]["runtime"]["userCommitReceipt"].clone())?;
    let mut latest = observed.clone();
    let deadline = Instant::now()
        + limits
            .answer
            .clamp(Duration::from_secs(15), Duration::from_secs(900));
    // Reference checkBlockingUiWarning in pollGeneratedImageOrTextAssistantResponse.
    let blocking = || async move {
        if recovery {
            return Ok(());
        }
        match crate::browser_page::ui_warning(
            reservation.page(),
            "generated image artifacts",
            limits.command,
            cancel,
            &|line| page_log(store, id, line),
        )
        .await
        {
            Some(warning) => Err(anyhow::anyhow!(warning)),
            None => Ok(()),
        }
    };
    for attempt in 0..2 {
        blocking().await?;
        outcome.require(&store.require(id)?, witness, None, cancel)?;
        let buttons = reservation.page().evaluate_with_arguments(
            &image_button_expression(),
            &json!([{"owner":reservation.owner(),"receipt":receipt,"recovery":recovery,"buttonReceiptId":Uuid::new_v4(),"expectedMessageId":observed["lastAssistantMessageId"],"expectedTurnId":observed["lastAssistantTurnId"]}]),
            limits.command,cancel,
        ).await?;
        anyhow::ensure!(
            buttons.is_array(),
            "Owned image button inventory is unavailable"
        );
        let current = store.require(id)?;
        outcome.require(&current, witness, None, cancel)?;
        let mut image_buttons = vec![];
        for button in buttons.as_array().unwrap() {
            if let Some(file) = current["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|file| matches_saved_file(file, button, target.id(), observed))
                && crate::browser_download::image_type(&crate::browser_output::saved_file_bytes(
                    file, cancel,
                )?)
                .is_none()
            {
                continue;
            }
            image_buttons.push(button.clone());
        }
        let had_buttons = !image_buttons.is_empty();
        let mut button_observation = latest.clone();
        button_observation["images"] = json!(image_buttons);
        button_observation["files"] = json!([]);
        let saved = collect_delivery_inventory(
            store,
            id,
            witness,
            outcome,
            target,
            endpoint,
            reservation,
            &button_observation,
            limits,
            recovery,
            true,
            cancel,
        )
        .await?;
        if saved.iter().any(&from_current_response) {
            return Ok(saved);
        }
        if optional_images && !direct_failed {
            anyhow::ensure!(
                !had_buttons,
                "Required browser image delivery is incomplete; preserve the conversation and captured images for recovery."
            );
            return Ok(saved);
        }
        if attempt == 1 || direct_failed {
            break;
        }
        while Instant::now() < deadline {
            pause(Duration::from_millis(1500), cancel).await?;
            blocking().await?;
            latest = reservation
                .page()
                .evaluate_with_arguments(
                    &result_expression(),
                    &json!([{"owner":reservation.owner(),"receipt":receipt,"recovery":recovery}]),
                    limits.command,
                    cancel,
                )
                .await?;
            anyhow::ensure!(
                latest["pending"] != true
                    && latest["lastUserMessageId"] == observed["lastUserMessageId"]
                    && latest["lastAssistantMessageId"] == observed["lastAssistantMessageId"]
                    && latest["lastAssistantTurnId"] == observed["lastAssistantTurnId"],
                "Required image polling changed the owned assistant response"
            );
            let current = store.require(id)?;
            outcome.require(&current, witness, None, cancel)?;
            browser::require_original_source_pair(&current, &latest)?;
            if latest["images"]
                .as_array()
                .is_some_and(|images| !images.is_empty())
                && latest["completionVisible"] == true
                && latest["stopExists"] == false
                && latest["strongThinkingActive"] == false
            {
                let saved = collect_delivery_inventory(
                    store,
                    id,
                    witness,
                    outcome,
                    target,
                    endpoint,
                    reservation,
                    &latest,
                    limits,
                    recovery,
                    false,
                    cancel,
                )
                .await?;
                if saved.iter().any(|artifact| artifact["kind"] == "image") {
                    return Ok(saved);
                }
                direct_failed = true;
                break;
            }
        }
    }
    if direct_failed {
        bail!(
            "Required browser image delivery is incomplete; preserve the conversation and captured images for recovery."
        );
    }
    let text = latest["text"]
        .as_str()
        .filter(|text| !policy::trim(text).is_empty())
        .unwrap_or("(empty response)");
    bail!("No images generated. Response text:\n{text}")
}
fn prior_turn_images(store: &SessionStore, metadata: &Value) -> Result<Vec<Value>> {
    if crate::browser_turns::prompts(&metadata["options"])?.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        turn_image_deliveries(store, metadata, crate::browser_turns::index(metadata)?)?
            .into_iter()
            .flat_map(|delivery| delivery.saved_images().to_vec())
            .collect(),
    )
}
fn turn_image_deliveries(
    store: &SessionStore,
    metadata: &Value,
    count: usize,
) -> Result<Vec<ImageDelivery>> {
    let index = crate::browser_turns::index(metadata)?;
    let turns = crate::browser_turns::saved(metadata)?;
    anyhow::ensure!(
        turns.len() >= index && turns.len() <= index + 1 && count <= turns.len(),
        "Sequential artifact progress changed"
    );
    let cwd = Path::new(
        metadata["cwd"]
            .as_str()
            .context("Sequential artifacts have no saved working directory")?,
    );
    let directory = store
        .directory(
            metadata["id"]
                .as_str()
                .context("Sequential artifacts have no session")?,
        )?
        .join("artifacts");
    let mut deliveries = vec![];
    let mut previous_count = 0;
    for (i, turn) in turns.iter().take(count).enumerate() {
        crate::browser_turns::verify_turn(store, metadata, turn, i)?;
        let origin = json!({"targetId":metadata["browser"]["runtime"]["chromeTargetId"],"assistantMessageId":turn["assistantMessageId"],"assistantTurnId":turn["assistantTurnId"]});
        let receipts = metadata["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| {
                row["kind"] == "image"
                    && row["origin"]["targetId"] == origin["targetId"]
                    && row["origin"]["assistantMessageId"] == origin["assistantMessageId"]
                    && row["origin"]["assistantTurnId"] == origin["assistantTurnId"]
            })
            .cloned()
            .collect::<Vec<_>>();
        let buttons = receipts
            .first()
            .is_some_and(|receipt| receipt["imageButton"] == true);
        let mut delivery = ImageDelivery::after_previous(
            &metadata["options"],
            cwd,
            &directory,
            if buttons {
                &[]
            } else {
                turn["images"]
                    .as_array()
                    .context("Prior image inventory changed")?
            },
            previous_count,
        )?;
        delivery.restore_saved(&receipts, &origin, buttons)?;
        if !delivery.generated.is_empty() || delivery.has_saved_images() {
            delivery.require_complete(&receipts)?;
        }
        previous_count = previous_count
            .checked_add(receipts.len())
            .context("Image index overflow")?;
        deliveries.push(delivery);
    }
    Ok(deliveries)
}
fn save_image_delivery(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &OutcomeWitness,
    delivery: &ImageDelivery,
    observed: &Value,
    cancel: &CancellationToken,
) -> Result<()> {
    let summary = delivery.summary(
        observed["text"]
            .as_str()
            .context("Owned image answer text is unavailable")?,
    );
    let mut before_delivery = None;
    let images = if summary["buttonDownloads"] == true {
        json!([])
    } else {
        json!(delivery.generated)
    };
    let after_delivery = outcome.update(store,id,witness,cancel,|current| {
        let mut changes = json!({"imageDelivery":summary});
        let accepted = &current["browser"]["runtime"]["acceptedObservation"];
        if accepted.is_object() && summary["buttonDownloads"] != true {
            browser::require_original_source_pair(current, observed)?;
            let provider_text = observed.get("providerText").unwrap_or(&observed["text"]);
            anyhow::ensure!(accepted["lastUserMessageId"]==observed["lastUserMessageId"]
                && accepted["lastAssistantMessageId"]==observed["lastAssistantMessageId"]
                && accepted["lastAssistantTurnId"]==observed["lastAssistantTurnId"]
                && accepted.get("providerText").unwrap_or(&accepted["text"])==provider_text
                && observed["completionVisible"]==true && observed["stopExists"]==false
                && observed["strongThinkingActive"]==false,
                "Accepted response changed during artifact delivery");
            let mut refreshed = merge_object(accepted, observed);
            refreshed["providerText"] = provider_text.clone();
            refreshed["images"] = json!(delivery.generated);
            refreshed["text"] = json!(browser::normalise_image_answer(provider_text.as_str().context("Delivered response has no provider text")?,delivery.generated.len()));
            changes["acceptedObservation"] = refreshed;
        }
        if !crate::browser_turns::prompts(&current["options"])?.is_empty() {
            let index = crate::browser_turns::index(current)?;
            let mut turns = crate::browser_turns::saved(current)?.to_vec();
            let row = turns.get_mut(index).context("Delivered image turn has no saved capture")?;
            crate::browser_turns::verify_turn(store,current,row,index)?;
            anyhow::ensure!(row["assistantMessageId"]==observed["lastAssistantMessageId"]&&row["assistantTurnId"]==observed["lastAssistantTurnId"],"Delivered image response changed");
            row["images"] = images.clone();
            changes["completedBrowserTurns"] = json!(turns);
            before_delivery = Some(current.clone());
        }
        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],&changes)}))})))
    })?;
    if let Some(before) = before_delivery {
        witness.seal_recovered_delivery(&before, &after_delivery, &images)?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)] // Every publication is tied to the native and renderer witnesses.
async fn collect_delivery_inventory(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &OutcomeWitness,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    reservation: &RendererReservation,
    observed: &Value,
    limits: &RunnerLimits,
    recovery: bool,
    buttons: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let committed = request(store, id)?;
    let metadata = store.require(id)?;
    outcome.require(&metadata, witness, None, cancel)?;
    anyhow::ensure!(
        sha256(&serde_json::to_vec(&metadata["options"])?) == committed.options_sha256,
        "Browser committed options changed before artifact publication"
    );
    let directory = store.directory(id)?.join("artifacts");
    let cwd = Path::new(
        metadata["cwd"]
            .as_str()
            .context("Image delivery has no saved working directory")?,
    );
    let prior_images = prior_turn_images(store, &metadata)?;
    let mut image_delivery = ImageDelivery::after_previous(
        &metadata["options"],
        cwd,
        &directory,
        if buttons {
            &[]
        } else {
            observed["images"]
                .as_array()
                .context("Owned image inventory is unavailable")?
                .as_slice()
        },
        prior_images.len(),
    )?;
    let previous = metadata["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let current_previous = previous
        .iter()
        .filter(|row| !prior_images.contains(row))
        .cloned()
        .collect::<Vec<_>>();
    image_delivery.restore_saved(&current_previous,&json!({"targetId":target.id(),"assistantMessageId":observed["lastAssistantMessageId"],"assistantTurnId":observed["lastAssistantTurnId"]}),buttons)?;
    let mut candidates = vec![];
    let mut identities = std::collections::BTreeSet::new();
    for (kind, field) in [("image", "images"), ("file", "files")] {
        let inventory = if kind == "image" && !buttons {
            &image_delivery.generated
        } else {
            observed[field]
                .as_array()
                .context("Owned artifact inventory is unavailable")?
        };
        for candidate in inventory {
            if kind == "file"
                && let Some(saved) = previous
                    .iter()
                    .find(|saved| matches_saved_file(saved, candidate, target.id(), observed))
            {
                crate::browser_output::saved_file_bytes(saved, cancel)?;
                continue;
            }
            if kind == "image"
                && image_delivery.saved_images().iter().any(|saved| {
                    if buttons {
                        saved["candidateId"] == candidate["candidateId"]
                            && saved["filename"] == candidate["filename"]
                    } else {
                        saved["fileId"] == candidate["fileId"]
                    }
                })
            {
                continue;
            }
            let raw = candidate["url"]
                .as_str()
                .context("Owned artifact has no provider identity")?;
            let key = if raw == "browser-download" {
                format!(
                    "control:{}",
                    candidate["candidateId"]
                        .as_str()
                        .context("Download control has no stable candidate identity")?
                )
            } else {
                browser_download::download_url(raw)?.to_string()
            };
            if identities.insert(key) {
                candidates.push((kind, candidate.clone()));
            }
        }
    }
    // A remote browser would write into its own host's filesystem. Requested images still fail
    // through require_complete; file candidates stay unsaved, as the reference reports.
    if remote_host(endpoint) && !candidates.is_empty() {
        let files = candidates
            .iter()
            .filter(|(kind, _)| *kind == "file")
            .count();
        if files > 0 {
            automation_log(
                store,
                id,
                &format!(
                    "[browser] WARNING: {files} downloadable candidate(s) existed, but no local browser-host artifact was saved; bridge artifact-ready will not be emitted until ChatGPT file capture succeeds."
                ),
            )?;
        }
        candidates.clear();
    }
    if candidates.is_empty() {
        if !image_delivery.generated.is_empty() || image_delivery.has_saved_images() {
            image_delivery.require_complete(&previous)?;
        }
        if image_delivery.has_saved_images() {
            save_image_delivery(
                store,
                id,
                witness,
                outcome,
                &image_delivery,
                observed,
                cancel,
            )?;
        }
        return Ok(previous);
    }
    outcome.require(&store.require(id)?, witness, None, cancel)?;
    let home = store
        .home()
        .parent()
        .context("Native download ownership home is unavailable")?;
    outcome.runtime(
        store,
        id,
        witness,
        cancel,
        json!({"downloadContextSettled":false}),
    )?;
    let mut owner =
        DownloadOwner::begin(target, endpoint, home, &directory, limits.command, cancel).await?;
    outcome.runtime(
        store,
        id,
        witness,
        cancel,
        json!({"downloadStage":owner.stage_path()}),
    )?;
    let receipt: CommitReceipt = serde_json::from_value(
        store.require(id)?["browser"]["runtime"]["userCommitReceipt"].clone(),
    )?;
    let function = download_click_expression();
    let saved=async {
        let mut saved=previous;
        let mut settled_image_failure=false;
        for (kind,candidate) in candidates {
            outcome.require(&store.require(id)?,witness,None,cancel)?;
            let raw=candidate["url"].as_str().unwrap();
            let url=if raw=="browser-download" {Value::Null}else{json!(browser_download::download_url(raw)?.as_str())};
            let cfg=json!({"owner":reservation.owner(),"receipt":receipt,"recovery":recovery,"candidate":candidate,"kind":kind,"imageButton":buttons,"downloadUrl":url,"expectedMessageId":observed["lastAssistantMessageId"],"expectedTurnId":observed["lastAssistantTurnId"]});
            let (bytes,row)=owner.capture(reservation.page(),&function,cfg,limits.command,cancel).await?;
            outcome.require(&store.require(id)?,witness,None,cancel)?;
            let current=reservation.page().evaluate_with_arguments(&result_expression(),&json!([{"owner":reservation.owner(),"receipt":receipt,"recovery":recovery}]),limits.command,cancel).await?;
            anyhow::ensure!(current["lastAssistantMessageId"]==observed["lastAssistantMessageId"]&&current["lastAssistantTurnId"]==observed["lastAssistantTurnId"]&&current["completionVisible"]==true&&current["stopExists"]==false&&current["strongThinkingActive"]==false,"Result identity or completion changed before artifact publication; staging retained");
            let current_meta=store.require(id)?;
            outcome.require(&current_meta,witness,None,cancel)?;
            browser::require_original_source_pair(&current_meta,&current)?;
            if kind=="image" && browser_download::image_type(&bytes).is_none() {
                owner.discard_non_image(row["guid"].as_str().context("Owned download GUID missing")?,&bytes)?;
                if !buttons {settled_image_failure=true;saved.push(json!({"kind":"warning","code":"image-download-failed","message":"Generated image download returned no verified image signature","fallbackAllowed":true}));outcome.update(store,id,witness,cancel,|current|Ok(Some(json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!(saved),&metadata["artifacts"])}))))?;break;}
                continue;
            }
            let suggested=row["suggestedFilename"].as_str().unwrap_or("download.bin");
            let name=candidate["filename"].as_str().filter(|s|!s.is_empty()).unwrap_or(suggested);
            let origin=json!({"mode":"local","targetId":target.id(),"frameId":row["frameId"],"downloadGuid":row["guid"],"assistantMessageId":observed["lastAssistantMessageId"],"assistantTurnId":observed["lastAssistantTurnId"]});
            let mut image_candidate=candidate.clone();image_candidate["origin"]=origin.clone();
            if buttons {image_candidate["alt"]=candidate["label"].clone();}
            let mut publication=None;
            let mut automatic_save_error=None;
            outcome.update(store,id,witness,cancel,|current| {
                let result=if kind=="image" {image_delivery.publish(&bytes,&image_candidate,cancel)}else{browser_download::publish_download(&bytes,&directory,name,false,cancel)};
                let manifest=match result {
                    Ok(manifest)=>manifest,
                    Err(error) if kind=="image"&&!image_delivery.requested&&!cancel.is_cancelled()=>{automatic_save_error=Some(error);return Ok(None)},
                    Err(error)=>return Err(error),
                };
                let mut manifest=manifest;
                if kind=="file" {manifest["sourceUrl"]=candidate["url"].clone();manifest["filename"]=json!(name);manifest["candidateId"]=candidate["candidateId"].clone();}
                if buttons {image_delivery.record_button_image(&image_candidate);}
                manifest["origin"]=origin;
                owner.publication_confirmed(row["guid"].as_str().unwrap(),&manifest)?;
                publication=Some(manifest.clone());
                Ok(Some(json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!([manifest]),&Value::Null)})))
            })?;
            if let Some(error)=automatic_save_error {
                saved.push(json!({"kind":"warning","code":"image-download-failed","message":error.to_string(),"fallbackAllowed":false}));
                outcome.update(store,id,witness,cancel,|current|Ok(Some(json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!(saved),&metadata["artifacts"])}))))?;
                break;
            }
            let manifest=publication.context("Artifact publication produced no receipt")?;
            saved.push(manifest.clone());
        }
        if (!image_delivery.generated.is_empty() || image_delivery.has_saved_images()) && !(image_delivery.requested && settled_image_failure && !image_delivery.has_saved_images()) {
            image_delivery.require_complete(&saved)?;
            save_image_delivery(store,id,witness,outcome,&image_delivery,observed,cancel)?;
        }
        Ok::<_,anyhow::Error>(saved)
    }.await;
    let finished = owner.finish(Duration::from_secs(1)).await;
    if finished.is_ok() {
        outcome.download_settled(store, id, witness)?;
    }
    match (saved, finished) {
        (Ok(mut saved), Ok(warning)) => {
            if let Some(warning) = warning {
                saved.push(json!({"kind":"warning","code":"artifact-staging-cleanup-failed","message":warning}));
                outcome.update(store,id,witness,cancel,|current|Ok(Some(json!({"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!(saved),&metadata["artifacts"])}))))?;
            }
            Ok(saved)
        }
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    }
}
fn image_button_expression() -> String {
    format!(
        "(cfg) => {{ /* industrious-dom:image-buttons */ const readOwnedResult={}; const observed=readOwnedResult(cfg); if(observed.pending || observed.lastAssistantMessageId!==cfg.expectedMessageId || observed.lastAssistantTurnId!==cfg.expectedTurnId || !observed.completionVisible || observed.stopExists || observed.strongThinkingActive)throw Error('Owned image button response changed'); {} {} {} const turns=readTurns();const root=lastAssistant(turns[observed.lastAssistantTurnIndex]);if(!root)throw Error('Owned image assistant root is unavailable');const claim=readOracleTargetClaim();if(!matchesOracleSubmission(claim,cfg.owner))throw Error('Image button renderer ownership changed');const controls=Array.from(root.querySelectorAll('a[href],a[download],button,[role=\"button\"],[data-testid],[aria-label],[title]'));const candidates=serializeImageButtons(root,observed.lastAssistantTurnIndex);window.__calvinoImageButtonReceipts={{owner:JSON.stringify(cfg.owner),id:cfg.buttonReceiptId,nodes:new Map(candidates.map(candidate=>[candidate.candidateId,controls[Number(candidate.candidateId.split(':').at(-1))]])),spent:new Set()}};return candidates.map(candidate=>({{...candidate,buttonReceiptId:cfg.buttonReceiptId}})); }}",
        result_expression(),
        include_str!("../browser-scripts/ownership-prelude.js"),
        include_str!("../browser-scripts/result-prelude.js"),
        include_str!("../browser-scripts/artifact-prelude.js")
    )
}
fn download_click_expression() -> String {
    format!(
        "(cfg) => {{ /* industrious-dom:download-click */ const readOwnedResult = {}; const observed=readOwnedResult(cfg); if(observed.pending || observed.lastAssistantMessageId!==cfg.expectedMessageId || observed.lastAssistantTurnId!==cfg.expectedTurnId || !observed.completionVisible || observed.stopExists || observed.strongThinkingActive)throw Error('Owned download response changed'); {} {} {} const turns=readTurns(); const root=lastAssistant(turns[observed.lastAssistantTurnIndex]); if(!root)throw Error('Owned assistant root is unavailable'); const claim=readOracleTargetClaim();if(!matchesOracleSubmission(claim,cfg.owner))throw Error('Download renderer ownership changed'); const candidates=cfg.imageButton?serializeImageButtons(root,observed.lastAssistantTurnIndex):cfg.kind==='image'?serializeImages(root):serializeFiles(root,observed.lastAssistantTurnIndex); const matches=candidates.filter(item=>item.url===cfg.candidate.url && (cfg.kind==='image'&&!cfg.imageButton||item.candidateId===cfg.candidate.candidateId&&item.filename===cfg.candidate.filename)); if(matches.length<1)throw Error('Download candidate was removed or replaced'); const id=String(cfg.candidate.candidateId||'').split(':');const controls=Array.from(root.querySelectorAll('a[href],a[download],button,[role=\"button\"],[data-testid],[aria-label],[title]'));const index=Number(id.at(-1));const control=Number.isSafeInteger(index)?controls[index]:null;if(cfg.imageButton){{const seal=window.__calvinoImageButtonReceipts;if(!seal||seal.owner!==JSON.stringify(cfg.owner)||seal.id!==cfg.candidate.buttonReceiptId||seal.nodes.get(cfg.candidate.candidateId)!==control||seal.spent.has(cfg.candidate.candidateId))throw Error('Image download control ownership changed');seal.spent.add(cfg.candidate.candidateId);}} if(cfg.downloadUrl){{ const anchor=document.createElement('a');anchor.href=cfg.downloadUrl;anchor.download=cfg.candidate.filename||'download';anchor.rel='noopener';anchor.style.display='none';document.body.appendChild(anchor);anchor.click();setTimeout(()=>anchor.remove(),0);return {{clicked:1}}; }} if(!control || control.disabled || control.getAttribute('aria-disabled')==='true')throw Error('Download control is unavailable'); const again=serializeCandidate(control,cfg.candidate.candidateId,cfg.imageButton);if(!again || again.filename!==cfg.candidate.filename || again.url!==cfg.candidate.url)throw Error('Download control changed');control.click();return {{clicked:1}}; }}",
        result_expression(),
        include_str!("../browser-scripts/ownership-prelude.js"),
        include_str!("../browser-scripts/result-prelude.js"),
        include_str!("../browser-scripts/artifact-prelude.js")
    )
}
fn require_recovery_admission(saved: &Value, current: &ProcessIdentity) -> Result<()> {
    let orphan = saved["status"] == "running";
    anyhow::ensure!(
        saved["mode"] == "browser"
            && (orphan
                || saved["recoveryRequired"] == true
                    && matches!(
                        saved["status"].as_str(),
                        Some("error" | "cancelled" | "completed")
                    )),
        "Browser recovery requires a retained terminal record or an orphan native worker"
    );
    anyhow::ensure!(
        saved["status"] != "cancelled"
            && (saved["status"] == "completed"
                || saved["models"].as_array().is_none_or(|runs| runs
                    .iter()
                    .filter(|run| run["model"] == saved["model"])
                    .all(|run| !matches!(
                        run["status"].as_str(),
                        Some("completed" | "partial" | "cancelled")
                    )))),
        "A terminal browser outcome cannot be reopened for completion"
    );
    let old: ProcessIdentity =
        serde_json::from_value(saved["lifecycle"]["processIdentity"].clone())
            .context("Recovery has no exact native controller identity")?;
    if orphan {
        let matching: Vec<_> = saved["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|run| run["model"] == saved["model"])
            .collect();
        anyhow::ensure!(
            old != *current
                && old.liveness() == Liveness::Dead
                && saved["lifecycle"]["engine"] == "browser"
                && saved["lifecycle"]["workerPid"].as_u64() == Some(u64::from(old.pid))
                && saved["lifecycle"]["workerProcessStartedAtMs"].as_f64()
                    == Some(old.started_at_ms)
                && !matching.is_empty()
                && matching
                    .iter()
                    .all(|run| matches!(run["status"].as_str(), Some("pending" | "running"))),
            "Running browser recovery requires a positively dead exact worker and a nonterminal model"
        );
    } else {
        anyhow::ensure!(
            old == *current || old.liveness() == Liveness::Dead,
            "A live or unknown foreign worker still owns this recovery"
        );
    }
    Ok(())
}
/// Recovery has no target creation, picker selection or send operation.
pub async fn recover_existing(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Value> {
    let result =
        recover_existing_inner(store, id, profile, browser, endpoint, limits, cancel, true).await;
    if result.is_ok() {
        settle_saved_fresh(store, id).await;
        settle_saved_idle(store, id).await;
        settle_hinted_idle(store, id, true).await;
    }
    result
}
pub(crate) async fn recover_attached(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Value> {
    // Stopping an observer must leave the dead worker's uncertain consultation recoverable.
    let result =
        recover_existing_inner(store, id, profile, browser, endpoint, limits, cancel, false).await;
    if result.is_ok() {
        settle_saved_fresh(store, id).await;
        settle_saved_idle(store, id).await;
        settle_hinted_idle(store, id, true).await;
    }
    result
}
#[allow(clippy::too_many_arguments)] // The caller determines whether cancellation ends observation or the consultation.
async fn recover_existing_inner(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    cancel_session: bool,
) -> Result<Value> {
    endpoint.validate()?;
    let public = store.stored_for_update(id)?;
    if public["browser"]["consultation"].is_object() && !crate::browser_attempt::internal(&public) {
        return crate::browser_attempt::recover_parent(
            store, id, profile, browser, endpoint, limits, cancel,
        )
        .await;
    }
    anyhow::ensure!(
        public["browser"]["attempt"].is_null()
            && public["browser"]["attemptReady"].is_null()
            && public["options"]["providerAttempt"].is_null(),
        "Provider attempts recover through their original public parent session"
    );
    let directory = store.directory(id)?;
    let recovery_lock = DirectoryLock::acquire(
        &directory.join("native-browser-recovery.lock"),
        Duration::from_secs(10),
    )?;
    let saved = store.require(id)?;
    // require() reconciles the model-file view. Compare the actual metadata record inside
    // the write transaction so read reconciliation cannot masquerade as an owner change.
    let saved_raw = store.stored_for_update(id)?;
    let current = ProcessIdentity::current()?;
    require_recovery_admission(&saved, &current)?;
    let committed = request(store, id)?;
    anyhow::ensure!(
        !crate::browser_attempt::is_child(&saved, &committed),
        "Provider attempts recover through their original public parent session"
    );
    let runtime = &saved["browser"]["runtime"];
    anyhow::ensure!(
        runtime.get("borrowedTarget").is_none_or(Value::is_null),
        "A --browser-tab run borrowed the user's tab and holds no recovery authority; read its answer with industrious session <id> --harvest."
    );
    let followups = crate::browser_turns::prompts(&saved["options"])?;
    let turn_index = crate::browser_turns::index(&saved)?;
    if !followups.is_empty() && saved["status"] != "completed" {
        anyhow::ensure!(
            turn_index == followups.len(),
            "Sequential run stopped before all follow-ups were dispatched; retained turns and target require inspection or an explicit restart. Recovery will not send the remaining prompts."
        );
    }
    let recovery_prompt = if followups.is_empty() || turn_index == 0 {
        committed.prompt.as_str()
    } else {
        followups
            .get(turn_index - 1)
            .context("Sequential recovery turn is outside the saved request")?
            .as_str()
    };
    let creator = runtime["ownedRecoveryTarget"]
        .get("creator")
        .filter(|creator| !creator.is_null())
        .cloned();
    let journal = if let Some(creator) = &creator {
        anyhow::ensure!(creator.is_object(), "Recovery target creator is malformed");
        let journal = TargetJournal::open(store)?;
        anyhow::ensure!(
            journal.read(&creator["allocationId"])?["creator"] == *creator,
            "Recovery target creator differs from its durable record"
        );
        Some(journal)
    } else {
        None
    };
    let target_id = runtime["chromeTargetId"]
        .as_str()
        .context("Recovery has no exact allocated target")?;
    anyhow::ensure!(
        runtime["browserWSEndpoint"] == endpoint.browser_ws_endpoint
            && runtime["chromePort"].as_u64() == Some(u64::from(endpoint.port))
            && runtime["chromeHost"].as_str().is_some_and(
                |h| browser::normalise_host(h) == browser::normalise_host(&endpoint.host)
            ),
        "Recovery endpoint differs from the retained browser instance"
    );
    let lease_id = runtime["tabLeaseId"]
        .as_str()
        .context("Recovery has no retained lease")?;
    let lease = profile.recovery_lease(lease_id, id, &endpoint.host, endpoint.port, target_id)?;
    let live = browser
        .call("Target.getTargets", json!({}), limits.command, cancel)
        .await?;
    let targets = live["targetInfos"]
        .as_array()
        .context("Recovery target list is incomplete")?;
    anyhow::ensure!(
        targets.iter().all(|target| target["targetId"]
            .as_str()
            .is_some_and(crate::browser_target::valid_id)),
        "Recovery target list contains malformed identities"
    );
    if targets.iter().all(|target| target["targetId"] != target_id) {
        anyhow::ensure!(
            saved["status"] == "completed" && saved["dispatch"]["state"] == "completed",
            "An absent uncertain target cannot establish provider completion; recovery evidence retained"
        );
        captured(store, id)?;
        let selection = json!({"modelSelection":runtime["modelSelection"],"thinkingSelection":runtime["thinkingSelection"],"effectiveModel":runtime["effectiveModel"]});
        browser::accept_answer(&saved, &runtime["acceptedObservation"], &selection)?;
        let exact: browser::RecoveryTarget =
            serde_json::from_value(runtime["ownedRecoveryTarget"].clone())?;
        anyhow::ensure!(
            browser::exact_owned_target(&saved, &exact)
                && exact.target_id == target_id
                && exact.browser_ws_endpoint.as_deref() == Some(&endpoint.browser_ws_endpoint),
            "Absent-target retirement does not match saved ownership"
        );
        if let (Some(journal), Some(creator)) = (&journal, &creator) {
            let commit = journal.commit(creator, &journal.session_commit(creator, &saved)?)?;
            journal.finalise(&commit)?;
            let result = journal
                .retire_finalised(creator, |commit| {
                    let commit = commit.clone();
                    let saved_raw = &saved_raw;
                    let browser = &browser;
                    async move {
                        anyhow::ensure!(
                            store.stored_for_update(id)? == *saved_raw,
                            "Recovery owner changed during absence inspection"
                        );
                        journal.verify(&json!({"creator":creator,"commit":commit}))?;
                        let listed = browser
                            .call(
                                "Target.getTargets",
                                json!({}),
                                limits.command,
                                &CancellationToken::new(),
                            )
                            .await?;
                        let rows = listed["targetInfos"]
                            .as_array()
                            .context("Target absence evidence is incomplete")?;
                        anyhow::ensure!(
                            rows.iter().all(|row| row["targetId"]
                                .as_str()
                                .is_some_and(crate::browser_target::valid_id))
                                && rows.iter().all(|row| row["targetId"] != target_id),
                            "Target reappeared before retirement persistence"
                        );
                        Ok(("confirmed-absent".into(), "exact-target-absent".into()))
                    }
                })
                .await?;
            anyhow::ensure!(
                result["decision"] == "retired" && result["absence"] == "confirmed-absent",
                "Absent target disposition remains pending"
            );
        }
        lease.confirm_target_absent(target_id)?;
        let result=store.update(id,|meta|{anyhow::ensure!(meta==&saved_raw,"Recovery owner changed during absence inspection");Ok(Some(json!({"recoveryRequired":false,"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"],&json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}))}))})))})?;
        recovery_lock.release()?;
        return Ok(result);
    }
    // A completed close may have left its exact durable marker in the renderer. Resume
    // cleanup without admitting that marked renderer as a new submission/recovery owner.
    if saved["status"] == "completed"
        && saved["dispatch"]["state"] == "completed"
        && let (Some(journal), Some(creator)) = (&journal, &creator)
    {
        let record = journal.read(&creator["allocationId"])?;
        if record["retirement"].is_object() {
            journal.verify(&record)?;
            let marker = journal.retirement_id(creator, &record["commit"])?;
            let page = browser
                .attach_target(target_id, limits.command, cancel)
                .await?;
            let marked = crate::browser_retirement::pending_marker(
                &page,
                &runtime["submissionOwner"],
                &marker,
                std::time::Instant::now() + limits.command,
            )
            .await?;
            if marked.is_some() {
                let cleanup = async {
                    captured(store,id)?;
                    let selection = json!({"modelSelection":runtime["modelSelection"],"thinkingSelection":runtime["thinkingSelection"],"effectiveModel":runtime["effectiveModel"]});
                    browser::accept_answer(&saved,&runtime["acceptedObservation"],&selection)?;
                    anyhow::ensure!(store.stored_for_update(id)? == saved_raw, "Completed cleanup owner changed");
                    let disposed = crate::browser_retirement::retire_committed(store,journal,&browser,&endpoint,creator).await?;
                    anyhow::ensure!(disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent", "Completed target retirement remains pending");
                    retire_inputs(store,id,&committed)?;
                    store.update(id,|meta| {
                        anyhow::ensure!(meta==&saved_raw,"Completed cleanup metadata changed");
                        Ok(Some(json!({"recoveryRequired":false,"cleanupWarning":null,"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"],&json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}))}))})))
                    })
                }.await;
                recovery_lock.release()?;
                return cleanup;
            }
        }
    }
    let owner: browser_commit::SubmissionOwner =
        serde_json::from_value(runtime["submissionOwner"].clone())
            .context("Recovery has no exact renderer submission owner")?;
    let snapshot: CommitSnapshot = serde_json::from_value(runtime["commitSnapshot"].clone())
        .context("Recovery has no retained pre-send user/turn identities")?;
    let mut target =
        OwnedTarget::recover(browser, lease, target_id.into(), limits.command, cancel).await?;
    let info = target.require(limits.command, cancel).await?;
    let conversation = info["url"]
        .as_str()
        .and_then(browser::conversation_id)
        .context("Recovery target has no provider conversation")?;
    let mut reservation = RendererReservation::recover(
        target.page().clone(),
        owner,
        &conversation,
        saved["status"] == "completed",
        limits.command,
        cancel,
    )
    .await?;
    let receipt = if reservation.state() == crate::browser_renderer::DispatchState::Released {
        serde_json::from_value(runtime["userCommitReceipt"].clone())
            .context("Completed cleanup has no saved user commitment")?
    } else {
        // Recovery reads the conversation from its target and must not rewrite the saved record here.
        wait_receipt(
            &reservation,
            &snapshot,
            recovery_prompt,
            limits,
            cancel,
            |_| Ok(()),
        )
        .await?
    };
    anyhow::ensure!(
        receipt.conversation.conversation_id == conversation,
        "Recovered complete source belongs to a different conversation"
    );
    let selection = json!({"modelSelection":runtime["modelSelection"],"thinkingSelection":runtime["thinkingSelection"],"effectiveModel":runtime["effectiveModel"]});
    let model = serde_json::from_value(selection["modelSelection"].clone())?;
    let thinking = serde_json::from_value(selection["thinkingSelection"].clone())?;
    policy::assert_browser_completion(
        Some(&model),
        Some(&thinking),
        selection["effectiveModel"].as_str(),
    )?;
    if saved["status"] == "completed" {
        let cleanup=async {
            anyhow::ensure!(saved["dispatch"]["state"]=="completed","Completed recovery has no durable dispatch outcome");
            captured(store,id)?;
            OutcomeWitness::capture(&saved,&target,&endpoint,&receipt,&committed.options_sha256,TargetAuthority::Created(runtime["ownedRecoveryTarget"].get("creator")))?;
            let observed=wait_answer(store,id,&reservation,&receipt,&selection,limits,true,None,cancel).await?;
            let accepted=&runtime["acceptedObservation"];
            anyhow::ensure!(observed["lastAssistantMessageId"]==accepted["lastAssistantMessageId"]&&observed["lastAssistantTurnId"]==accepted["lastAssistantTurnId"]&&observed["report"]==accepted["report"]&&observed["text"]==accepted["text"],"Completed answer changed before cleanup");
            anyhow::ensure!(store.stored_for_update(id)?==saved_raw,"Completed recovery metadata changed before cleanup");
            if let (Some(journal),Some(creator))=(&journal,&creator) {
                retire_recovered_journaled(store,id,&mut target,&mut reservation,&conversation,journal,creator,limits.command).await?;
            }else{
                retire_recovered_target(&mut target,&mut reservation,&conversation,limits.command).await?;
                target.release_retired_lease()?;
            }
            retire_inputs(store,id,&committed)?;
            store.update(id,|meta| {
                anyhow::ensure!(meta==&saved_raw,"Completed recovery metadata changed during cleanup");
                Ok(Some(json!({"recoveryRequired":false,"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"],&json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}))}))})))
            })?;
            Ok::<_,anyhow::Error>(())
        }.await;
        if let Err(error) = &cleanup {
            store.update(id, |meta| {
                Ok((meta == &saved_raw)
                    .then(|| json!({"cleanupWarning":error.to_string(),"recoveryRequired":true})))
            })?;
        }
        recovery_lock.release()?;
        cleanup?;
        return store.require(id);
    }
    // Change controller incarnation under the native recovery lock; preserve all saved source,
    // submission, capture and extension fields. A late old worker fails its witness afterwards.
    let recovery_model = saved["model"]
        .as_str()
        .context("Recovery model record is unavailable")?;
    store.update_session_and_model(id,recovery_model,|meta,run| {
        anyhow::ensure!(meta==&saved_raw,"Recovery metadata changed during ownership inspection");
        if saved["status"] == "running" {
            anyhow::ensure!(run.is_some_and(|run| matches!(run["status"].as_str(),Some("pending"|"running"))),"Orphan recovery model is no longer nonterminal");
        }
        anyhow::ensure!(run.is_none_or(|run|!matches!(run["status"].as_str(),Some("completed"|"partial"|"cancelled"))),"Recovery model already has a terminal outcome");
        let controller_pid=if creator.is_some(){meta["browser"]["runtime"]["controllerPid"].clone()}else{json!(current.pid)};
        let controller_start=if creator.is_some(){meta["browser"]["runtime"]["controllerProcessStartedAtMs"].clone()}else{json!(current.started_at_ms)};
        let owned=merge_object(&meta["browser"]["runtime"]["ownedRecoveryTarget"],&json!({"host":endpoint.host,"port":endpoint.port,"targetId":target_id,"browserWSEndpoint":endpoint.browser_ws_endpoint,"claimId":reservation.owner().claim_id,"conversationId":conversation}));
        Ok(Some((json!({"status":"running","startedAt":timestamp(),"lifecycle":merge_object(&meta["lifecycle"],&json!({"engine":"browser","workerPid":current.pid,"workerProcessStartedAtMs":current.started_at_ms,"processIdentity":current,"execution":"recovery"})),"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"],&json!({"controllerPid":controller_pid,"controllerProcessStartedAtMs":controller_start,"userCommitReceipt":receipt,"conversationId":conversation,"tabUrl":info["url"],"submittedPromptHash":receipt.message_fingerprint,"submittedPromptHashFormat":"original-source-v1","ownedRecoveryTarget":owned}))}))}),None)))
    })?;
    let witness = ExecutionWitness::capture(store, id, &current)?;
    let mut outcome_witness = OutcomeWitness::capture(
        &store.require(id)?,
        &target,
        &endpoint,
        &receipt,
        &committed.options_sha256,
        TargetAuthority::Created(runtime["ownedRecoveryTarget"].get("creator")),
    )?;
    let recovered=async {
        let (capture, text) = recovery_capture::capture_recovered_answer(
            store, id, &witness, &mut outcome_witness, &saved, &target, &endpoint,
            &reservation, &receipt, &selection, limits, cancel,
        ).await?;
        publish_answer(store,id,&witness,&outcome_witness,&capture,cancel)?;
        complete(store,id,&witness,&outcome_witness,|current| {
            let changes=json!({"status":"completed","completedAt":timestamp(),"capturedAnswer":capture,"browser":merge_object(&current["browser"],&json!({"modelSelection":selection["modelSelection"],"thinkingSelection":selection["thinkingSelection"]})),"dispatch":merge_object(&current["dispatch"],&json!({"state":"completed","recoveredAt":timestamp()})),"recoveryRequired":true});
            if let (Some(journal),Some(creator))=(&journal,&creator) {
                let proposed=merge_object(current,&changes);let commit=journal.session_commit(creator,&proposed)?;
                journal.verify_proposed(&json!({"creator":creator,"commit":commit}),&proposed)?;
            }
            Ok(changes)
        },&json!({"modelSelection":selection["modelSelection"],"thinkingSelection":selection["thinkingSelection"]}),&text,cancel)?;
        // Positive durable capture permits exact quarantine disposal without releasing/replaying
        // the old uncertain submission or deleting its recovery evidence first.
        if let (Some(journal),Some(creator))=(&journal,&creator) {
            retire_recovered_journaled(store,id,&mut target,&mut reservation,&conversation,journal,creator,limits.command).await?;
        }else{
            retire_recovered_target(&mut target,&mut reservation,&conversation,limits.command).await?;
            target.release_retired_lease()?;
        }
        retire_inputs(store,id,&committed)?;
        store.update(id,|meta| {witness.require(meta)?;Ok(Some(json!({"recoveryRequired":false,"browser":merge_object(&meta["browser"],&json!({"runtime":merge_object(&meta["browser"]["runtime"],&json!({"targetDispositionPending":false,"targetRetiredAt":timestamp()}))}))})))})?;
        Ok::<_,anyhow::Error>(())
    }.await;
    if let Err(error) = &recovered {
        store.update(id,|meta| {
            if witness.require(meta).is_err(){return Ok(None);}
            if meta["status"]=="completed" {Ok(Some(json!({"cleanupWarning":error.to_string(),"recoveryRequired":true})))}else if matches!(meta["status"].as_str(),Some("partial"|"cancelled")){Ok(None)}else{Ok(Some(json!({"status":if cancel.is_cancelled()&&cancel_session{"cancelled"}else{"error"},"completedAt":timestamp(),"errorMessage":error.to_string(),"recoveryRequired":true})))}})?;
    }
    recovery_lock.release()?;
    recovered?;
    store.require(id)
}
#[cfg(test)]
#[path = "browser_runner_tests.rs"]
pub(crate) mod tests;

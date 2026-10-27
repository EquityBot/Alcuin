//! Completing an observation never invents the original consultation's authority.
//! Reference: src/cli/recoveredBrowserHarvest.ts and src/cli/sessionOwnership.ts.
use crate::{
    artifacts, browser,
    browser_disposition::TargetJournal,
    browser_output,
    browser_profile::ProfileRegistry,
    browser_runner::{self, RunnerLimits},
    browser_sessions::HarvestSnapshot,
    browser_tabs, policy,
    process::ProcessIdentity,
    sessions::{self, SessionStore},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct HistoricalCompletion {
    pub session: Value,
    pub disposition: Option<Value>,
    pub cleanup_error: Option<String>,
}
impl HistoricalCompletion {
    fn add_cleanup_error(&mut self, error: impl std::fmt::Display) {
        self.cleanup_error = Some(match self.cleanup_error.take() {
            Some(first) => format!("{first}; disconnect also failed: {error}"),
            None => error.to_string(),
        });
    }
}
fn recovered_completion(
    result: Result<Value>,
    cleanup: Result<()>,
) -> Result<HistoricalCompletion> {
    Ok(HistoricalCompletion {
        session: result?,
        disposition: None,
        cleanup_error: cleanup.err().map(|error| error.to_string()),
    })
}
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoricalTarget {
    pub host: String,
    pub port: u16,
    pub target_id: String,
    #[serde(rename = "browserWSEndpoint", alias = "browserWsEndpoint")]
    pub browser_ws_endpoint: Option<String>,
    pub claim_id: Option<String>,
    pub conversation_id: String,
}
impl HistoricalTarget {
    pub(crate) fn matches(&self, meta: &Value) -> bool {
        let runtime = &meta["browser"]["runtime"];
        let owned = &runtime["ownedRecoveryTarget"];
        let url_conversation = runtime["tabUrl"]
            .as_str()
            .and_then(browser::conversation_id);
        let creatorless = owned["creator"].is_null()
            && meta["lifecycle"].get("processIdentity").is_none()
            && runtime.get("submissionOwner").is_none();
        let conversation = if creatorless {
            runtime["conversationId"]
                .as_str()
                .or(url_conversation.as_deref())
        } else {
            url_conversation.as_deref()
        };
        conversation == Some(&self.conversation_id)
            && (!creatorless
                || runtime["tabUrl"].is_null()
                || url_conversation.as_deref() == Some(&self.conversation_id))
            && runtime
                .get("conversationId")
                .is_none_or(|id| id.as_str() == Some(&self.conversation_id))
            && owned
                .get("conversationId")
                .is_none_or(|id| id.as_str() == Some(&self.conversation_id))
            && owned["targetId"] == self.target_id
            && owned["port"].as_u64() == Some(u64::from(self.port))
            && owned["host"].as_str().is_some_and(|host| {
                browser::normalise_host(host) == browser::normalise_host(&self.host)
            })
            && owned["browserWSEndpoint"].as_str() == self.browser_ws_endpoint.as_deref()
            && owned["claimId"].as_str() == self.claim_id.as_deref()
    }
}
fn terminal(value: &Value) -> bool {
    matches!(value.as_str(), Some("completed" | "partial" | "cancelled"))
}
pub(crate) fn legacy_record(meta: &Value) -> bool {
    meta["lifecycle"].get("processIdentity").is_none()
        && meta["browser"]["runtime"].get("submissionOwner").is_none()
}
pub(crate) fn controllers_dead(meta: &Value) -> bool {
    let mut owners = 0;
    for (pid, start) in [
        (
            meta["lifecycle"].get("workerPid"),
            meta["lifecycle"].get("workerProcessStartedAtMs"),
        ),
        (
            meta["browser"]["runtime"].get("controllerPid"),
            meta["browser"]["runtime"].get("controllerProcessStartedAtMs"),
        ),
    ] {
        let Some(pid) = pid else { continue };
        owners += 1;
        let Some(pid) = pid
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
        else {
            return false;
        };
        let start = match start {
            None => None,
            Some(start) => match start
                .as_f64()
                .filter(|start| start.is_finite() && *start > 0.0)
            {
                Some(start) => Some(start),
                None => return false,
            },
        };
        match ProcessIdentity::read(pid) {
            Ok(None) => {}
            Ok(Some(actual)) if start.is_some_and(|start| start != actual.started_at_ms) => {}
            _ => return false,
        }
    }
    owners > 0
}
fn source_files(meta: &Value) -> Vec<Value> {
    meta["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["kind"] == "file")
        .cloned()
        .collect()
}
pub(crate) fn same_basis(current: &Value, saved: &Value) -> bool {
    ["id", "model", "mode", "startedAt", "cwd", "lifecycle"]
        .iter()
        .all(|key| current.get(*key) == saved.get(*key))
        && current["browser"].get("runtime") == saved["browser"].get("runtime")
        && current["options"].get("writeOutputPath") == saved["options"].get("writeOutputPath")
        && (current["options"]["writeArtifacts"] == true)
            == (saved["options"]["writeArtifacts"] == true)
        && (saved["options"]["writeArtifacts"] != true
            || source_files(current) == source_files(saved))
        && current["browser"].get("config") == saved["browser"].get("config")
}
fn foreign_controller(store: &SessionStore, id: &str, owned: &HistoricalTarget) -> Result<bool> {
    for meta in store.list(true)? {
        if meta["id"] == id {
            continue;
        }
        let runtime = &meta["browser"]["runtime"];
        if runtime["chromeTargetId"] == owned.target_id
            && runtime["chromePort"].as_u64() == Some(u64::from(owned.port))
            && browser::normalise_host(runtime["chromeHost"].as_str().unwrap_or("127.0.0.1"))
                == browser::normalise_host(&owned.host)
            && !controllers_dead(&meta)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn eligible(meta: &Value, snapshot: &HarvestSnapshot) -> Result<Option<HistoricalTarget>> {
    let tab = &snapshot.tab;
    let runtime = &meta["browser"]["runtime"];
    if matches!(meta["status"].as_str(), Some("partial" | "cancelled"))
        || snapshot.integrity.status != "matched"
        || snapshot.integrity.explicit_target
        || tab["state"] != "completed"
        || tab["completionConfirmed"] != true
        || tab["stopExists"] == true
        || tab["authenticated"] != true
        || !runtime["submittedPromptHash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
        || !browser_tabs::ready_for_session(meta, tab, false)?
        || !controllers_dead(meta)
    {
        return Ok(None);
    }
    let mut capture = runtime["ownedRecoveryTarget"].clone();
    if !capture.is_object()
        || capture
            .get("creator")
            .is_some_and(|creator| !creator.is_null() && !creator.is_object())
    {
        return Ok(None);
    }
    if capture.get("conversationId").is_none() {
        capture["conversationId"] = runtime["conversationId"]
            .as_str()
            .map(|id| json!(id))
            .or_else(|| {
                runtime["tabUrl"]
                    .as_str()
                    .and_then(browser::conversation_id)
                    .map(|id| json!(id))
            })
            .unwrap_or(Value::Null);
    }
    let Ok(owned) = serde_json::from_value::<HistoricalTarget>(capture) else {
        return Ok(None);
    };
    if !owned.matches(meta)
        || !browser::stable_conversation_id(&owned.conversation_id)
        || !crate::browser_target::valid_id(&owned.target_id)
        || owned.port == 0
        || owned.host.is_empty()
        || owned
            .claim_id
            .as_ref()
            .is_some_and(|id| !browser::provider_identity(&json!(id)))
        || tab["targetId"] != owned.target_id
        || tab["port"].as_u64() != Some(u64::from(owned.port))
        || tab["host"].as_str().is_none_or(|host| {
            browser::normalise_host(host) != browser::normalise_host(&owned.host)
        })
        || owned
            .browser_ws_endpoint
            .as_deref()
            .is_some_and(|endpoint| endpoint != snapshot.endpoint.browser_ws_endpoint)
        || browser::normalise_host(&owned.host) != browser::normalise_host(&snapshot.endpoint.host)
        || owned.port != snapshot.endpoint.port
        || tab["url"]
            .as_str()
            .and_then(browser::conversation_id)
            .as_deref()
            != Some(&owned.conversation_id)
    {
        return Ok(None);
    }
    Ok(Some(owned))
}
fn guard(
    store: &SessionStore,
    current: &Value,
    run: Option<&Value>,
    saved: &Value,
    owned: Option<&HistoricalTarget>,
    cancel: &CancellationToken,
) -> Result<bool> {
    ensure!(!cancel.is_cancelled(), "Historical completion cancelled");
    if let Some(owned) = owned {
        let creator = &saved["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
        require_controllers(store, saved, owned, creator.as_object().map(|_| creator))?;
    }
    Ok(!terminal(&current["status"])
        && !run.is_some_and(|run| terminal(&run["status"]))
        && same_basis(current, saved)
        && controllers_dead(current)
        && match owned {
            Some(owned) => !foreign_controller(
                store,
                saved["id"]
                    .as_str()
                    .context("Missing saved session identity")?,
                owned,
            )?,
            None => legacy_record(current),
        })
}
fn require_controllers(
    store: &SessionStore,
    saved: &Value,
    owned: &HistoricalTarget,
    creator: Option<&Value>,
) -> Result<()> {
    crate::browser_retirement::require_target_controllers(
        store,
        saved["id"].as_str(),
        &serde_json::to_value(owned)?,
        creator,
        std::time::Instant::now() + Duration::from_secs(1),
        false,
    )
}
pub(crate) fn verify_completed_outputs(
    current: &Value,
    answer: &[u8],
    cancel: &CancellationToken,
) -> Result<()> {
    let cwd = Path::new(
        current["cwd"]
            .as_str()
            .context("Historical output has no working directory")?,
    );
    let required = &current["browser"]["requiredTextOutput"];
    if let Some(path) = current["options"]["writeOutputPath"]
        .as_str()
        .filter(|path| !path.is_empty())
    {
        browser_output::reuse_answer(
            required,
            &cwd.join(path),
            &browser_output::required_answer_bytes(required, current, answer)?,
            cancel,
        )?;
    }
    if current["options"]["writeArtifacts"] == true {
        let directory = required["file"]
            .as_str()
            .and_then(|path| Path::new(path).parent())
            .context("Historical file export directory is missing")?;
        let files = source_files(current);
        for receipt in &files {
            let mut file = receipt.clone();
            let path = file["path"]
                .as_str()
                .context("Historical artifact path is missing")?;
            file["path"] = json!(crate::prompts::absolute(cwd, path));
            browser_output::saved_file_bytes(&file, cancel)?;
            ensure!(
                files
                    .iter()
                    .any(|candidate| candidate["sha256"] == file["sha256"]
                        && candidate["sizeBytes"] == file["sizeBytes"]
                        && candidate["path"]
                            .as_str()
                            .is_some_and(|path| Path::new(path).parent() == Some(directory))),
                "Required historical file export is missing"
            );
        }
    }
    Ok(())
}
fn commit_legacy(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    snapshot: &HarvestSnapshot,
    owned: &HistoricalTarget,
    creator: &Value,
    cancel: &CancellationToken,
) -> Result<Option<Value>> {
    commit_historical(store, id, saved, snapshot, owned, Some(creator), cancel)
}
fn commit_historical(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    snapshot: &HarvestSnapshot,
    owned: &HistoricalTarget,
    creator: Option<&Value>,
    cancel: &CancellationToken,
) -> Result<Option<Value>> {
    commit_observed(store, id, saved, snapshot, Some(owned), creator, cancel)
}
fn commit_observed(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    snapshot: &HarvestSnapshot,
    owned: Option<&HistoricalTarget>,
    creator: Option<&Value>,
    cancel: &CancellationToken,
) -> Result<Option<Value>> {
    let authority = creator
        .map(|creator| {
            let journal = TargetJournal::open(store)?;
            let record = journal.read(&creator["allocationId"])?;
            ensure!(
                record["creator"] == *creator && record.get("nativeOwnerIdentity").is_none(),
                "Legacy completion cannot replace native journal authority"
            );
            Ok::<_, anyhow::Error>((journal, record))
        })
        .transpose()?;
    let answer = snapshot
        .tab
        .get("lastAssistantMarkdown")
        .filter(|answer| !answer.is_null())
        .unwrap_or(&snapshot.tab["lastAssistantText"])
        .as_str()
        .context("Historical answer is missing")?;
    ensure!(
        !policy::trim(answer).is_empty(),
        "Historical answer is empty"
    );
    let model = saved["model"]
        .as_str()
        .context("Historical session has no model")?;
    if saved["status"] == "completed" {
        let owned = owned.context("Completed recovery requires exact original target authority")?;
        let completed = store.update_session_and_model(id, model, |current, run| {
            ensure!(
                current == saved
                    && current["status"] == "completed"
                    && run.is_some_and(|run| run["status"] == "completed")
                    && eligible(current, snapshot)?.is_some()
                    && !foreign_controller(store, id, owned)?,
                "Historical completion retry requires its durable completed root and model"
            );
            ensure!(!cancel.is_cancelled(), "Historical completion cancelled");
            require_controllers(store, current, owned, creator)?;
            let capture: artifacts::CaptureReceipt =
                serde_json::from_value(current["browser"]["capturedAnswer"].clone())?;
            ensure!(
                capture.session_id == id
                    && artifacts::verify_capture(store, &capture)? == answer.as_bytes(),
                "Historical completion retry capture changed"
            );
            verify_completed_outputs(current, answer.as_bytes(), cancel)?;
            if let Some((journal, record)) = &authority {
                let commit =
                    journal.historical_session_commit(creator.unwrap(), current, &snapshot.tab)?;
                let mut proof = record.clone();
                proof["commit"] = commit;
                journal.verify_proposed(&proof, current)?;
            }
            Ok(None)
        })?;
        if let Some((journal, _)) = &authority {
            let creator = creator.unwrap();
            let commit = journal.historical_session_commit(creator, &completed, &snapshot.tab)?;
            let commit = journal.commit(creator, &commit)?;
            journal.finalise(&commit)?;
        }
        return Ok(Some(completed));
    }
    let heading = browser_output::answer_heading(saved);
    store.append_log_guarded(id,Some(model),&format!("[reattach] harvested assistant response from existing Chrome tab\n{heading}\n{answer}\n"),|current,run|{ensure!(guard(store,current,run,saved,owned,cancel)?,"Historical completion authority changed before log capture");Ok(())})?;
    let output_tokens = crate::tokenizer::count_text(answer)?;
    let usage = json!({"inputTokens":0,"outputTokens":output_tokens,"reasoningTokens":0,"totalTokens":output_tokens});
    let mut proposed_commit = None;
    let mut committed = false;
    let completed=store.update_session_and_model(id,model,|current,run| {
        if !guard(store,current,run,saved,owned,cancel)? {return Ok(None);}
        let captured=artifacts::capture(store,id,&format!("historical-answer-{}.md",Uuid::new_v4()),answer.as_bytes(),cancel)?;
        let mut browser=current["browser"].clone();browser["capturedAnswer"]=serde_json::to_value(&captured)?;
        let mut additions=vec![];
        if let Some(output)=current["options"]["writeOutputPath"].as_str().filter(|path|!path.is_empty()) {
            let destination=Path::new(current["cwd"].as_str().context("Historical output has no working directory")?).join(output);
            let required=browser_output::save_answer(store,&destination,answer.as_bytes(),&heading,cancel)?;
            additions.push(json!({"kind":"output","path":required["file"],"sizeBytes":required["bytes"],"sha256":required["sha256"]}));browser["requiredTextOutput"]=required;
        }
        if current["options"]["writeArtifacts"]==true {
            let mut files=source_files(current);
            for file in &mut files {
                let path=file["path"].as_str().context("Historical artifact has no path")?;
                if !Path::new(path).is_absolute() {
                    file["path"]=json!(crate::prompts::absolute(Path::new(current["cwd"].as_str().context("Historical artifact has no working directory")?),path));
                }
            }
            let exports=browser_output::export_files(browser["requiredTextOutput"]["file"].as_str().map(Path::new),&files,cancel)?;
            additions.extend(exports);
        }
        artifacts::verify_capture(store,&captured)?;
        ensure!(!cancel.is_cancelled(),"Historical completion cancelled before commit");
        let completed_at=sessions::timestamp();
        let patch=json!({"browser":browser,"artifacts":sessions::merge_artifacts(&current["artifacts"],&json!(additions),&Value::Null),"status":"completed","completedAt":completed_at,"usage":usage,"errorMessage":null,"error":null,"transport":null,"response":sessions::merge_object(&current["response"],&json!({"status":"completed","incompleteReason":null}))});
        let mut proposed=current.clone();sessions::apply_patch(&mut proposed,&patch)?;
        if let Some((journal,record))=&authority {
            let commit=journal.historical_session_commit(creator.unwrap(),&proposed,&snapshot.tab)?;
            let mut proof=record.clone();proof["commit"]=commit.clone();journal.verify_proposed(&proof,&proposed)?;proposed_commit=Some(commit);
        }
        ensure!(guard(store,current,run,saved,owned,cancel)?,"Historical completion authority changed before commit");
        committed=true;
        Ok(Some((patch,Some(json!({"status":"completed","completedAt":completed_at,"usage":usage,"errorMessage":null,"error":null,"response":sessions::merge_object(&run.map(|run|run["response"].clone()).unwrap_or(Value::Null),&json!({"status":"completed","incompleteReason":null}))})))))
    })?;
    if !committed {
        return Ok(None);
    }
    if let Some((journal, _)) = &authority {
        let commit = journal.commit(
            creator.unwrap(),
            &proposed_commit.context("Historical journal commitment missing")?,
        )?;
        journal.finalise(&commit)?;
    }
    Ok(Some(completed))
}
pub(crate) async fn complete_recovered(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    snapshot: &HarvestSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<HistoricalCompletion>> {
    if !legacy_record(saved)
        || saved["mode"] != "browser"
        || !matches!(saved["status"].as_str(), Some("running" | "error"))
        || !controllers_dead(saved)
        || snapshot.integrity.status != "matched"
        || snapshot.integrity.explicit_target
        || snapshot.tab["state"] != "completed"
        || snapshot.tab["completionConfirmed"] != true
        || snapshot.tab["stopExists"] == true
        || snapshot.tab["authenticated"] != true
        || !browser_tabs::ready_for_session(saved, &snapshot.tab, false)?
    {
        return Ok(None);
    }
    let conversation = snapshot.tab["url"]
        .as_str()
        .and_then(browser::conversation_id);
    ensure!(
        conversation
            .as_deref()
            .is_some_and(browser::stable_conversation_id)
            && conversation == snapshot.integrity.observed_conversation_id
            && snapshot
                .integrity
                .captured
                .iter()
                .all(|source| Some(&source.conversation_id) == conversation.as_ref()),
        "Recovered observation conversation evidence is inconsistent"
    );
    if eligible(saved, snapshot)?.is_some() {
        return complete_saved_owned(store, id, saved.clone(), snapshot, cancel).await;
    }
    if let Some(creator) = saved["browser"]["runtime"]["ownedRecoveryTarget"]
        .get("creator")
        .filter(|creator| !creator.is_null())
    {
        ensure!(
            creator.is_object(),
            "Saved recovery target creator is malformed"
        );
        let record = TargetJournal::open(store)?.read(&creator["allocationId"])?;
        ensure!(
            record["creator"] == *creator && record.get("nativeOwnerIdentity").is_none(),
            "Legacy observation cannot replace native journal authority"
        );
    }
    Ok(
        commit_observed(store, id, saved, snapshot, None, None, cancel)?.map(|session| {
            HistoricalCompletion {
                session,
                disposition: None,
                cleanup_error: None,
            }
        }),
    )
}
pub async fn complete_owned(
    store: &SessionStore,
    id: &str,
    snapshot: &HarvestSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<HistoricalCompletion>> {
    // Reconciled model metadata alone does not prove that the root commit reached disk.
    crate::browser_attempt::settle_unsent_switch(store, id, cancel).await?;
    let saved = store.stored_for_update(id)?;
    ensure!(
        saved["browser"]["attempt"].is_null()
            && saved["browser"]["attemptReady"].is_null()
            && saved["options"]["providerAttempt"].is_null(),
        "Provider attempts complete through their original public parent session"
    );
    let saved = if let Some(child) = crate::browser_attempt::active_target(store, &saved)? {
        ensure!(
            child["lifecycle"]["processIdentity"].is_object()
                && child["browser"]["runtime"]["submissionOwner"].is_object(),
            "Provider completion requires its original native attempt"
        );
        child
    } else {
        saved
    };
    complete_saved_owned(store, id, saved, snapshot, cancel).await
}
/// Resume a persisted retirement after the exact original target has disappeared.
/// This path observes inventory only and never creates retirement intent or closes a target.
pub(crate) async fn resume_creatorless_absence(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<Option<HistoricalCompletion>> {
    let saved = store.stored_for_update(id)?;
    let runtime = &saved["browser"]["runtime"];
    let attempt = &runtime["creatorlessRetirement"];
    if attempt.is_null()
        || saved["status"] != "completed"
        || saved["mode"] != "browser"
        || !legacy_record(&saved)
        || !runtime["ownedRecoveryTarget"]["creator"].is_null()
        || runtime["targetDispositionPending"] == false
    {
        return Ok(None);
    }
    let target: HistoricalTarget = serde_json::from_value(attempt["binding"]["target"].clone())?;
    ensure!(
        attempt["version"] == 1
            && attempt["id"]
                .as_str()
                .is_some_and(|id| Uuid::parse_str(id).is_ok())
            && attempt["binding"]
                == json!({"target":target,"capture":saved["browser"]["capturedAnswer"],"completedAt":saved["completedAt"]})
            && target.matches(&saved)
            && crate::browser_target::valid_id(&target.target_id)
            && browser::stable_conversation_id(&target.conversation_id)
            && target
                .claim_id
                .as_ref()
                .is_some_and(|id| browser::provider_identity(&json!(id)))
            && controllers_dead(&saved),
        "Saved creatorless retirement authority changed"
    );
    let endpoint = crate::browser_chrome::AttachmentEndpoint {
        host: target.host.clone(),
        port: target.port,
        browser_ws_endpoint: target
            .browser_ws_endpoint
            .clone()
            .context("Saved retirement has no exact browser endpoint")?,
    };
    endpoint.validate()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let remaining = || -> Result<Duration> {
        ensure!(!cancel.is_cancelled(), "Retirement recovery cancelled");
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        ensure!(!left.is_zero(), "Retirement recovery deadline reached");
        Ok(left)
    };
    crate::browser_retirement::authorise_claimed(store, id, &saved, &target, deadline)?;
    let browser = endpoint.connect(remaining()?, cancel).await?;
    let result = async {
        let rows = browser.call("Target.getTargets", json!({}), remaining()?, cancel).await?;
        let rows = rows["targetInfos"].as_array().context("Retirement target inventory is incomplete")?;
        ensure!(rows.iter().all(|row| row["targetId"].as_str().is_some_and(crate::browser_target::valid_id)), "Retirement target inventory has an invalid identity");
        if rows.iter().any(|row| row["targetId"] == target.target_id) {
            return Ok(None);
        }
        remaining()?;
        crate::browser_retirement::authorise_claimed(store, id, &saved, &target, deadline)?;
        let disposition = json!({"decision":"retired","absence":"confirmed-absent","reason":"already-absent","retirement":attempt});
        settle_creatorless_retirement_until(store, id, &target, &disposition, deadline)?;
        Ok::<_, anyhow::Error>(Some(HistoricalCompletion {session:store.stored_for_update(id)?, disposition:Some(disposition), cleanup_error:None}))
    }.await;
    let cleanup = browser
        .disconnect_and_wait(Duration::from_millis(250))
        .await;
    let mut result = result?;
    if let Some(completion) = &mut result
        && let Err(error) = cleanup
    {
        completion.add_cleanup_error(error);
    }
    Ok(result)
}
async fn complete_saved_owned(
    store: &SessionStore,
    id: &str,
    saved: Value,
    snapshot: &HarvestSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<HistoricalCompletion>> {
    let Some(owned) = eligible(&saved, snapshot)? else {
        return Ok(None);
    };
    if foreign_controller(
        store,
        saved["id"]
            .as_str()
            .context("Completion source ID missing")?,
        &owned,
    )? {
        return Ok(None);
    }
    let runtime = &saved["browser"]["runtime"];
    if saved["lifecycle"].get("processIdentity").is_some()
        || runtime.get("submissionOwner").is_some()
    {
        let profile = ProfileRegistry::open_existing(Path::new(
            runtime["profileDir"]
                .as_str()
                .context("Native recovery has no recorded admission directory")?,
        ))?;
        let timeout = Duration::from_millis(
            saved["browser"]["config"]["inputTimeoutMs"]
                .as_u64()
                .filter(|n| *n > 0)
                .unwrap_or(60_000),
        );
        let browser = snapshot.endpoint.connect(timeout, cancel).await?;
        let result = browser_runner::recover_existing(
            store,
            id,
            profile,
            browser.clone(),
            snapshot.endpoint.clone(),
            &RunnerLimits {
                command: timeout,
                answer: browser_runner::answer_timeout(&saved["browser"]["config"]),
                ..Default::default()
            },
            cancel,
        )
        .await;
        let cleanup = browser
            .disconnect_and_wait(Duration::from_millis(500))
            .await;
        return Ok(Some(recovered_completion(result, cleanup)?));
    }
    let creator = runtime["ownedRecoveryTarget"]
        .get("creator")
        .filter(|creator| creator.is_object());
    let completed = if let Some(creator) = creator {
        commit_legacy(store, id, &saved, snapshot, &owned, creator, cancel)?
    } else {
        commit_historical(store, id, &saved, snapshot, &owned, None, cancel)?
    };
    let Some(session) = completed else {
        return Ok(None);
    };
    let mut completion = HistoricalCompletion {
        session,
        disposition: None,
        cleanup_error: None,
    };
    if creator.is_none() && owned.claim_id.is_none() {
        return Ok(Some(completion));
    }
    match snapshot
        .endpoint
        .connect(Duration::from_secs(5), cancel)
        .await
    {
        Ok(browser) => {
            let result = async {
                if let Some(creator) = creator {
                    let journal = TargetJournal::open(store)?;
                    crate::browser_retirement::retire_committed(
                        store,
                        &journal,
                        &browser,
                        &snapshot.endpoint,
                        creator,
                    )
                    .await
                } else {
                    crate::browser_retirement::retire_claimed_completed(
                        store,
                        id,
                        &completion.session,
                        &owned,
                        &browser,
                    )
                    .await
                }
            }
            .await;
            match result {
                Ok(disposition) => {
                    if disposition["decision"] != "retired"
                        || disposition["absence"] != "confirmed-absent"
                    {
                        completion.cleanup_error = Some(format!(
                            "Owned browser tab cleanup remains pending: {}",
                            disposition["reason"]
                                .as_str()
                                .unwrap_or("absence unconfirmed")
                        ));
                    } else {
                        let settlement = (|| -> Result<()> {
                            if let Some(creator) = creator {
                                let journal = TargetJournal::open(store)?;
                                browser_runner::settle_completed_retirement(
                                    store,
                                    &journal,
                                    creator,
                                    &disposition,
                                    std::time::Instant::now() + Duration::from_secs(1),
                                )
                            } else {
                                settle_creatorless_retirement(store, id, &owned, &disposition)
                            }
                        })();
                        if let Err(error) = settlement {
                            completion.cleanup_error = Some(error.to_string());
                        }
                        if let Ok(current) = store.require(id) {
                            completion.session = current;
                        }
                    }
                    completion.disposition = Some(disposition);
                }
                Err(error) => completion.cleanup_error = Some(error.to_string()),
            }
            if let Err(error) = browser
                .disconnect_and_wait(Duration::from_millis(500))
                .await
            {
                completion.add_cleanup_error(error);
            }
        }
        Err(error) => completion.cleanup_error = Some(error.to_string()),
    }
    if creator.is_some() {
        // A retired legacy tab may have been the last page holding a hinted idle launch open.
        browser_runner::settle_hinted_idle(store, id, true).await;
    }
    Ok(Some(completion))
}

fn settle_creatorless_retirement(
    store: &SessionStore,
    id: &str,
    target: &HistoricalTarget,
    disposition: &Value,
) -> Result<()> {
    settle_creatorless_retirement_until(
        store,
        id,
        target,
        disposition,
        std::time::Instant::now() + Duration::from_secs(1),
    )
}
fn settle_creatorless_retirement_until(
    store: &SessionStore,
    id: &str,
    target: &HistoricalTarget,
    disposition: &Value,
    deadline: std::time::Instant,
) -> Result<()> {
    ensure!(
        disposition["decision"] == "retired" && disposition["absence"] == "confirmed-absent",
        "Creatorless retirement absence is unconfirmed"
    );
    let saved = store.stored_for_update(id)?;
    let attempt = &saved["browser"]["runtime"]["creatorlessRetirement"];
    ensure!(
        saved["status"] == "completed"
            && target.matches(&saved)
            && saved["browser"]["runtime"]["ownedRecoveryTarget"]["creator"].is_null()
            && attempt["version"] == 1
            && disposition["retirement"] == *attempt
            && attempt["binding"]
                == json!({"target":target,"capture":saved["browser"]["capturedAnswer"],"completedAt":saved["completedAt"]}),
        "Creatorless completed target changed before settlement"
    );
    let receipt: artifacts::CaptureReceipt =
        serde_json::from_value(saved["browser"]["capturedAnswer"].clone())?;
    let answer = artifacts::verify_capture(store, &receipt)?;
    ensure!(
        receipt.session_id == id,
        "Creatorless capture belongs to another session"
    );
    verify_completed_outputs(&saved, &answer, &CancellationToken::new())?;
    store.update_until(id, deadline, |current| {
        ensure!(current == &saved, "Creatorless completion changed during settlement");
        Ok(Some(json!({"recoveryRequired":false,"cleanupWarning":null,"browser":sessions::merge_object(&current["browser"],
            &json!({"runtime":sessions::merge_object(&current["browser"]["runtime"],
                &json!({"targetDispositionPending":false,"targetRetiredAt":sessions::timestamp()}))}))})))
    })?;
    Ok(())
}

#[cfg(test)]
#[path = "browser_historical_tests.rs"]
mod tests;

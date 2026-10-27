//! Durable remote capture, final result and exact delivery acknowledgement records.
use crate::{
    artifacts,
    native_fs::{NativeEntry, no_follow, read_stamp},
    remote_json,
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::{fs, io::Read};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug)]
pub struct RemoteRecord {
    wire: String,
    native: Value,
}
impl RemoteRecord {
    pub fn wire(&self) -> &str {
        &self.wire
    }
    /// UTF-8 display/file view. Delivery digests and network responses use the retained wire.
    pub fn native(&self) -> &Value {
        &self.native
    }
    fn decode(contents: &[u8], run_id: &str, phase: Phase) -> Result<Self> {
        let raw: &RawValue = serde_json::from_slice(contents)?;
        let result = remote_json::field(raw, "result")?.context("Remote result is missing")?;
        let artifacts =
            remote_json::field(raw, "artifacts")?.context("Remote artifacts are missing")?;
        let rows: Vec<&RawValue> = serde_json::from_str(artifacts.get())?;
        let digest = remote_json::result_digest_raw(run_id, result, &rows)?;
        let native = remote_json::native_projection(raw)?;
        validate_digest(&native, run_id, phase, &digest)?;
        Ok(Self {
            wire: String::from_utf8(remote_json::stringify(raw.get())?)?,
            native,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Capture,
    Result,
}
impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Result => "result",
        }
    }
    fn record(self) -> &'static str {
        match self {
            Self::Capture => "remote-capture.json",
            Self::Result => "remote-result.json",
        }
    }
    fn ack(self) -> &'static str {
        match self {
            Self::Capture => "remote-capture-ack.json",
            Self::Result => "remote-ack.json",
        }
    }
}
pub fn session_id(run_id: &str) -> Result<String> {
    ensure!(
        run_id.len() == 36
            && run_id
                .bytes()
                .enumerate()
                .all(|(at, byte)| if [8, 13, 18, 23].contains(&at) {
                    byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                }),
        "Invalid remote run identity"
    );
    Ok(format!("remote-{run_id}"))
}
pub fn save(
    store: &SessionStore,
    run_id: &str,
    captured: &Value,
    result: &Value,
    artifacts: &Value,
    phase: Phase,
    cancel: &CancellationToken,
) -> Result<Value> {
    save_until(
        store, run_id, captured, result, artifacts, phase, cancel, None,
    )
}
#[allow(clippy::too_many_arguments)] // Existing transport fields plus the recovery deadline; no new wire schema.
pub(crate) fn save_until(
    store: &SessionStore,
    run_id: &str,
    captured: &Value,
    result: &Value,
    artifacts: &Value,
    phase: Phase,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    let id = session_id(run_id)?;
    ensure!(artifacts.is_array(), "Remote artifacts must be an array");
    let descriptors: Value = artifacts
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["descriptor"].clone())
        .collect();
    let delivery = json!({"version":1,"runId":run_id,"sha256":remote_json::result_digest(run_id,result,&descriptors)?});
    let record = json!({"version":1,"phase":phase.name(),"runId":run_id,"capturedResult":captured,"result":result,"artifacts":artifacts,"delivery":delivery});
    validate(&record, run_id, phase)?;
    let bytes = remote_json::stringify(&serde_json::to_string(&record)?)?;
    let stored = remote_json::native_projection(serde_json::from_slice::<&RawValue>(&bytes)?)?;
    artifacts::capture_until(store, &id, phase.record(), &bytes, cancel, deadline)?;
    Ok(stored)
}
fn read_record(store: &SessionStore, id: &str, file: &str, flush: bool) -> Result<Vec<u8>> {
    read_record_with(store, id, file, flush, || {})
}
fn read_record_with(
    store: &SessionStore,
    id: &str,
    file: &str,
    flush: bool,
    before_verify: impl FnOnce(),
) -> Result<Vec<u8>> {
    let original = store.directory(id)?;
    let root = NativeEntry::open(&store.root(), true)?;
    let directory = NativeEntry::open(&original, true)?;
    let canonical_root = fs::canonicalize(store.root())?;
    let canonical = fs::canonicalize(&original)?;
    ensure!(
        canonical.starts_with(&canonical_root) && canonical != canonical_root,
        "Remote recovery leaves its session boundary"
    );
    let path = canonical.join(file);
    let linked = NativeEntry::open(&path, false)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(flush);
    no_follow(&mut options, false);
    let mut entry = NativeEntry::from_handle(options.open(&path)?, false)?;
    ensure!(
        entry.identity == linked.identity,
        "Remote record changed before opening"
    );
    let before = read_stamp(&entry.handle)?;
    let mut bytes = vec![];
    entry.handle.read_to_end(&mut bytes)?;
    if flush {
        entry.handle.sync_all()?;
    }
    before_verify();
    ensure!(
        before == read_stamp(&entry.handle)?
            && entry.handle.metadata()?.len() == bytes.len() as u64,
        "Remote record changed while reading"
    );
    entry.require_matches(&path)?;
    linked.require_matches(&path)?;
    directory.require_matches(&original)?;
    directory.require_matches(&canonical)?;
    root.require_matches(&store.root())?;
    ensure!(
        fs::canonicalize(store.root())? == canonical_root
            && fs::canonicalize(&original)? == canonical,
        "Remote recovery storage boundary changed"
    );
    Ok(bytes)
}
pub fn read(store: &SessionStore, run_id: &str) -> Result<RemoteRecord> {
    read_phase(store, run_id, Phase::Result).or_else(|_| read_phase(store, run_id, Phase::Capture))
}
pub(crate) fn read_phase(store: &SessionStore, run_id: &str, phase: Phase) -> Result<RemoteRecord> {
    let id = session_id(run_id)?;
    RemoteRecord::decode(
        &read_record(store, &id, phase.record(), false)?,
        run_id,
        phase,
    )
}
/// Promote retained raw fields without replacing either durable record or regenerating IDs.
pub(crate) fn promote_capture(
    store: &SessionStore,
    capture: &RemoteRecord,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<RemoteRecord> {
    artifacts::check_deadline(deadline)?;
    ensure!(!cancel.is_cancelled(), "Remote result promotion cancelled");
    let run = capture.native["runId"]
        .as_str()
        .context("Remote run missing")?;
    ensure!(
        capture.native["phase"] == "capture",
        "Only a capture can be promoted"
    );
    let raw: &RawValue = serde_json::from_str(capture.wire())?;
    let field = |name| -> Result<&str> {
        Ok(remote_json::field(raw, name)?
            .context("Remote promotion field missing")?
            .get())
    };
    let wire = format!(
        "{{\"version\":1,\"phase\":\"result\",\"runId\":{},\"capturedResult\":{},\"result\":{},\"artifacts\":{},\"delivery\":{}}}",
        serde_json::to_string(run)?,
        field("capturedResult")?,
        field("result")?,
        field("artifacts")?,
        field("delivery")?
    );
    let expected = RemoteRecord::decode(wire.as_bytes(), run, Phase::Result)?;
    let id = session_id(run)?;
    let path = store.directory(&id)?.join(Phase::Result.record());
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            artifacts::capture_until(
                store,
                &id,
                Phase::Result.record(),
                expected.wire().as_bytes(),
                cancel,
                deadline,
            )?;
        }
        Err(error) => return Err(error.into()),
    }
    let saved = read_phase(store, run, Phase::Result)?;
    ensure!(
        saved.wire() == expected.wire(),
        "Existing final result differs from its immutable capture"
    );
    Ok(saved)
}
pub(crate) fn require_acknowledged(store: &SessionStore, record: &RemoteRecord) -> Result<()> {
    let run = record.native["runId"]
        .as_str()
        .context("Remote run missing")?;
    let phase = if record.native["phase"] == "result" {
        Phase::Result
    } else {
        Phase::Capture
    };
    let expected = remote_json::stringify(&serde_json::to_string(&record.native["delivery"])?)?;
    ensure!(
        read_record(store, &session_id(run)?, phase.ack(), true)? == expected,
        "Saved remote acknowledgement differs from its delivery"
    );
    Ok(())
}

fn validate(record: &Value, run_id: &str, phase: Phase) -> Result<()> {
    let artifacts = record["artifacts"]
        .as_array()
        .context("Remote record artifacts are invalid")?;
    let descriptors: Value = artifacts
        .iter()
        .map(|artifact| artifact["descriptor"].clone())
        .collect();
    validate_digest(
        record,
        run_id,
        phase,
        &remote_json::result_digest(run_id, &record["result"], &descriptors)?,
    )
}
fn validate_digest(record: &Value, run_id: &str, phase: Phase, digest: &str) -> Result<()> {
    ensure!(
        record["artifacts"].is_array(),
        "Remote record artifacts are invalid"
    );
    ensure!(
        record["version"] == 1
            && record["phase"] == phase.name()
            && record["runId"] == run_id
            && record["result"]["answerText"].is_string()
            && record["result"]["answerMarkdown"].is_string()
            && record["capturedResult"]["answerText"].is_string()
            && record["delivery"]["version"] == 1
            && record["delivery"]["runId"] == run_id
            && record["delivery"]["sha256"] == digest,
        "Remote recovery record is invalid"
    );
    Ok(())
}
/// Persists delivery evidence only. The producer must separately authorise completion and retirement.
pub fn acknowledge(store: &SessionStore, record: &Value) -> Result<()> {
    let run_id = record["runId"]
        .as_str()
        .context("Remote acknowledgement run is missing")?;
    let id = session_id(run_id)?;
    let phase = match record["phase"].as_str() {
        Some("capture") => Phase::Capture,
        Some("result") => Phase::Result,
        _ => anyhow::bail!("Remote acknowledgement phase is invalid"),
    };
    validate(record, run_id, phase)?;
    persist_acknowledgement(store, record, &id, phase)
}
pub fn acknowledge_lossless(store: &SessionStore, record: &RemoteRecord) -> Result<()> {
    let run_id = record.native["runId"]
        .as_str()
        .context("Remote run is missing")?;
    let id = session_id(run_id)?;
    let phase = if record.native["phase"] == "result" {
        Phase::Result
    } else {
        Phase::Capture
    };
    // RemoteRecord can only be constructed by digest-checked decode.
    persist_acknowledgement(store, &record.native, &id, phase)
}
fn persist_acknowledgement(
    store: &SessionStore,
    record: &Value,
    id: &str,
    phase: Phase,
) -> Result<()> {
    let bytes = remote_json::stringify(&serde_json::to_string(&record["delivery"])?)?;
    match artifacts::capture(store, id, phase.ack(), &bytes, &CancellationToken::new()) {
        Ok(_) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            ensure!(
                read_record(store, id, phase.ack(), true)? == bytes,
                "Existing remote acknowledgement differs"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Completes only the producer session whose saved run and creator still match this delivery.
/// The acknowledgement is durable before metadata changes or any subsequent target cleanup.
pub fn complete_acknowledgement(store: &SessionStore, record: &Value) -> Result<(Value, bool)> {
    acknowledge(store, record)?;
    complete_metadata(store, record)
}
pub fn complete_acknowledgement_lossless(
    store: &SessionStore,
    record: &RemoteRecord,
) -> Result<(Value, bool)> {
    acknowledge_lossless(store, record)?;
    complete_metadata(store, &record.native)
}
fn complete_metadata(store: &SessionStore, record: &Value) -> Result<(Value, bool)> {
    let run_id = record["runId"].as_str().context("Remote run is missing")?;
    let id = session_id(run_id)?;
    let final_result = record["phase"] == "result";
    let captured = &record["capturedResult"];
    if !captured["selectedAttempt"].is_null()
        || !store.require(&id)?["browser"]["consultation"].is_null()
    {
        return complete_selected_metadata(store, record);
    }
    let creator = captured["ownedRecoveryTarget"]
        .get("creator")
        .filter(|value| match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::String(value) => !value.is_empty(),
            Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
            _ => true,
        });
    let already_acknowledged = |current: &Value| {
        current["status"] == "completed"
            && current["browser"]["remoteDelivery"]["runId"] == run_id
            && current["browser"]["remoteDelivery"]["acknowledged"] == true
            && current["browser"]["remoteDelivery"]["producerFinished"] == true
            && current
                .get("dispatch")
                .is_none_or(|dispatch| dispatch.is_null() || dispatch["state"] == "completed")
    };
    let snapshot = store.require(&id)?;
    if creator.is_some_and(|creator| !creator.is_object()) {
        return Ok((snapshot, false));
    }
    let candidate = if final_result && creator.is_some() && !already_acknowledged(&snapshot) {
        let text = captured["answerMarkdown"]
            .as_str()
            .filter(|text| !text.is_empty())
            .or_else(|| captured["answerText"].as_str())
            .context("Remote capture answer is missing")?;
        Some(serde_json::to_value(artifacts::capture(
            store,
            &id,
            &format!("captured-answer-{}.md", Uuid::new_v4()),
            text.as_bytes(),
            &CancellationToken::new(),
        )?)?)
    } else {
        None
    };
    let mut committed = false;
    let session = store.update(&id, |current| {
        if current["status"] == "cancelled" || current["browser"]["remoteDelivery"]["runId"] != run_id
            || (!final_result && (current["status"] == "completed" || current["browser"]["remoteDelivery"]["producerFinished"] == true)) {
            return Ok(None);
        }
        let mut native_journal = None;
        if let Some(creator) = creator {
            let runtime = &current["browser"]["runtime"];
            if !matches!(current["status"].as_str(), Some("running"|"partial"|"completed"))
                || !remote_json::deep_equal(&runtime["controllerPid"], &creator["controllerPid"])
                || !remote_json::optional_equal(runtime.get("controllerProcessStartedAtMs"), creator.get("controllerStartedAtMs").filter(|value| !value.is_null()))
                || current["lifecycle"].get("workerPid").is_some_and(|pid| !remote_json::deep_equal(pid,&creator["controllerPid"]))
                || runtime["chromeTargetId"] != creator["target"]["targetId"]
                || runtime["chromeHost"] != creator["target"]["host"]
                || !remote_json::deep_equal(&runtime["chromePort"], &creator["target"]["port"])
                || runtime.get("chromeBrowserWSEndpoint") != creator["target"].get("browserWSEndpoint")
                || runtime.get("conversationId") != captured.get("conversationId")
                || runtime.get("submittedPromptHash") != captured.get("submittedPromptHash")
                || runtime.get("submittedPromptHashFormat") != captured.get("submittedPromptHashFormat")
                || !remote_json::optional_equal(runtime["ownedRecoveryTarget"].get("creator"), Some(creator)) { return Ok(None); }
            if final_result && creator["allocationId"].is_string() {
                let journal = crate::browser_disposition::TargetJournal::open(store)?;
                let saved = journal.read(&creator["allocationId"])?;
                ensure!(remote_json::deep_equal(&saved["creator"],creator), "Remote target creator changed before completion");
                if saved.get("nativeOwnerIdentity").is_some() { native_journal = Some((journal,saved)); }
            }
            if final_result && already_acknowledged(current)
                && (native_journal.is_none() || current["dispatch"]["state"] == "completed") {
                let answer = &current["browser"]["capturedAnswer"];
                if answer["version"] == 1 && answer["sessionId"] == id && answer["encoding"] == "utf8"
                    && answer["file"].as_str().is_some_and(|file| crate::sessions::assert_id(file).is_ok()) {
                    committed = true;
                }
                return Ok(None);
            }
            if final_result && candidate.is_none() { return Ok(None); }
        }
        let mut browser = current["browser"].clone();
        if !browser.is_object() { browser = json!({}); }
        if let Some(answer) = &candidate { browser["capturedAnswer"] = answer.clone(); }
        browser["remoteDelivery"] = json!({"runId":run_id,"acknowledged":true,"producerFinished":final_result});
        let mut changes = json!({"status":if final_result {"completed"} else {"partial"},
            "completedAt":current.get("completedAt").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(crate::sessions::timestamp())),
            "browser":browser});
        if let Some((journal,mut journal_record)) = native_journal {
            ensure!(matches!(current["dispatch"]["state"].as_str(), Some("acknowledged"|"completed")),
                "Remote final ACK requires an acknowledged native dispatch");
            changes["dispatch"] = crate::sessions::merge_object(&current["dispatch"], &json!({"state":"completed"}));
            let proposed = crate::sessions::merge_object(current,&changes);
            journal_record["commit"] = acknowledgement_commit(store,&proposed,&journal_record["creator"],captured,&record["artifacts"])?;
            journal.verify_proposed(&journal_record,&proposed)?;
        }
        committed = true;
        Ok(Some(changes))
    })?;
    Ok((session, committed))
}
fn complete_selected_metadata(store: &SessionStore, record: &Value) -> Result<(Value, bool)> {
    let run = record["runId"].as_str().context("Remote run missing")?;
    let id = session_id(run)?;
    let snapshot = store.require(&id)?;
    if snapshot["status"] == "cancelled" || snapshot["browser"]["remoteDelivery"]["runId"] != run {
        return Ok((snapshot, false));
    }
    let cancel = CancellationToken::new();
    let expected = crate::browser_attempt::captured_selected(store, &id)?;
    ensure!(
        remote_json::deep_equal(&record["capturedResult"], &expected)
            && record["capturedResult"]["ownedRecoveryTarget"].is_null()
            && snapshot["browser"]["consultation"]["callbackIssued"] == true
            && snapshot["browser"]["remoteDelivery"]["captureIssued"] == true,
        "Remote selected capture differs from its parent publication or callback"
    );
    let final_result = record["phase"] == "result";
    let mut committed = false;
    let model = snapshot["model"]
        .as_str()
        .context("Remote consultation model missing")?;
    let session=store.update_session_and_model(&id,model,|current,model_run| {
        crate::browser_attempt::require_selected(store,&id,&cancel)?;
        ensure!(current["browser"]["remoteDelivery"]["runId"]==run
            && matches!(current["status"].as_str(),Some("running"|"partial"|"completed")),
            "Remote consultation changed before selected completion");
        ensure!(remote_json::deep_equal(&crate::browser_attempt::captured_selected(store,&id)?,&expected),
            "Remote selected capture changed during ACK");
        ensure!(model_run.is_none_or(|run|matches!(run["status"].as_str(),Some("running"|"partial"|"completed"))),
            "Remote consultation model outcome changed before ACK");
        if current["status"]=="completed" {
            committed=final_result && current["browser"]["remoteDelivery"]["acknowledged"]==true
                && current["browser"]["remoteDelivery"]["producerFinished"]==true;
            return Ok(None);
        }
        committed=true;
        let changes=json!({"status":if final_result{"completed"}else{"partial"},
            "completedAt":current.get("completedAt").filter(|value|value.is_string()).cloned().unwrap_or_else(||json!(crate::sessions::timestamp())),
            "recoveryRequired":true,"browser":crate::sessions::merge_object(&current["browser"],&json!({"remoteDelivery":crate::sessions::merge_object(&current["browser"]["remoteDelivery"],
                &json!({"runId":run,"acknowledged":true,"producerFinished":final_result}))}))});
        Ok(Some((changes.clone(),Some(json!({"status":changes["status"],"completedAt":changes["completedAt"],"response":crate::sessions::merge_object(&model_run.map_or(Value::Null,|run|run["response"].clone()),&json!({"status":changes["status"]}))})))))
    })?;
    if committed && final_result {
        crate::browser_attempt::complete_selected(store, &id, &cancel)?;
    }
    Ok((session, committed))
}
fn acknowledgement_commit(
    store: &SessionStore,
    session: &Value,
    creator: &Value,
    captured: &Value,
    artifacts: &Value,
) -> Result<Value> {
    let id = session["id"]
        .as_str()
        .context("Remote session identity is missing")?;
    let answer = &session["browser"]["capturedAnswer"];
    let file = answer["file"]
        .as_str()
        .context("Remote answer file is missing")?;
    crate::sessions::assert_id(file)?;
    let rows = artifacts
        .as_array()
        .context("Remote artifacts are missing")?;
    let required: Vec<Value> = rows.iter().map(|artifact| json!({"file":artifact["filePath"],"bytes":artifact["descriptor"]["byteSize"],"sha256":artifact["descriptor"]["sha256"]})).collect();
    let mut commit = json!({"version":1,"kind":"session","allocationId":creator["allocationId"],"claimId":creator["target"]["claimId"],
        "conversationId":captured["conversationId"],"output":{"file":crate::native_fs::canonical_wire_path(&store.directory(id)?.join(file))?,"bytes":answer["bytes"],"sha256":answer["sha256"]},
        "requiredOutputs":required,"committedAt":crate::sessions::timestamp(),"sessionId":id,"completedAt":session["completedAt"],
        "submittedPromptHash":captured["submittedPromptHash"],"assistantMessageId":captured["assistantMessageId"],
        "assistantContent":captured.get("assistantContent").cloned().unwrap_or_else(|| json!({"sha256":answer["sha256"],"representation":"markdown"}))});
    if let Some(format) = captured.get("submittedPromptHashFormat") {
        commit["submittedPromptHashFormat"] = format.clone();
    }
    Ok(commit)
}
/// Delivery remains successful when target cleanup cannot be confirmed.
pub async fn finalise_acknowledgement(
    store: &SessionStore,
    record: &RemoteRecord,
) -> Result<Option<String>> {
    commit_acknowledgement(store, record)?
        .finish(store, record)
        .await
}
/// A durable acknowledgement. Only this commitment decides the remote reply; target
/// retirement and browser cleanup follow in `finish` and can only add a warning.
pub(crate) struct CommittedAcknowledgement {
    session: Value,
    committed: bool,
    /// The owned answer target's committed journal record, or why it is not committed.
    owned: Option<Result<(crate::browser_disposition::TargetJournal, Value)>>,
}
pub(crate) fn commit_acknowledgement(
    store: &SessionStore,
    record: &RemoteRecord,
) -> Result<CommittedAcknowledgement> {
    let (session, committed) = complete_acknowledgement_lossless(store, record)?;
    // A client that has its reply never acknowledges again, and nothing else commits a
    // completed session's owned target, so its journal record is committed before the reply.
    let owned = owned_creator(record, &session, committed)
        .map(|creator| commit_owned(store, record, &session, creator));
    Ok(CommittedAcknowledgement {
        session,
        committed,
        owned,
    })
}
fn owned_creator<'a>(
    record: &'a RemoteRecord,
    session: &Value,
    committed: bool,
) -> Option<&'a Value> {
    let captured = &record.native["capturedResult"];
    if !captured["selectedAttempt"].is_null() || !session["browser"]["consultation"].is_null() {
        return None;
    }
    let creator = captured["ownedRecoveryTarget"]
        .get("creator")
        .filter(|value| value.is_object())?;
    (committed
        && record.native["phase"] == "result"
        && session["browser"]["capturedAnswer"].is_object())
    .then_some(creator)
}
#[cfg(test)]
thread_local! {
    /// Number of journal commits that fail transiently on this test thread.
    static TEST_COMMIT_FAILURES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
fn commit_owned(
    store: &SessionStore,
    record: &RemoteRecord,
    session: &Value,
    creator: &Value,
) -> Result<(crate::browser_disposition::TargetJournal, Value)> {
    #[cfg(test)]
    if TEST_COMMIT_FAILURES.with(|left| left.replace(left.get().saturating_sub(1)) > 0) {
        anyhow::bail!("PUBLIC transient journal failure");
    }
    let journal = crate::browser_disposition::TargetJournal::open(store)?;
    let stored_creator = journal.read(&creator["allocationId"])?["creator"].clone();
    ensure!(
        remote_json::deep_equal(&stored_creator, creator),
        "Remote target creator changed before finalisation"
    );
    let commit = acknowledgement_commit(
        store,
        session,
        &stored_creator,
        &record.native["capturedResult"],
        &record.native["artifacts"],
    )?;
    let committed = journal.commit(&stored_creator, &commit)?;
    journal.finalise(&committed)?;
    Ok((journal, stored_creator))
}
#[cfg(test)]
pub(crate) struct FinishGate {
    pub(crate) run: String,
    pub(crate) entered: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) release: std::sync::Arc<tokio::sync::Semaphore>,
    pub(crate) finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
/// Holds the named run's retirement and browser cleanup until the test releases it.
#[cfg(test)]
pub(crate) static TEST_FINISH_GATE: std::sync::Mutex<Option<FinishGate>> =
    std::sync::Mutex::new(None);
impl CommittedAcknowledgement {
    /// Retires the answer target, then settles the browser it may have kept open.
    pub(crate) async fn finish(
        self,
        store: &SessionStore,
        record: &RemoteRecord,
    ) -> Result<Option<String>> {
        #[cfg(test)]
        let finished = {
            let gate = TEST_FINISH_GATE
                .lock()
                .unwrap()
                .as_ref()
                .filter(|gate| record.native["runId"] == gate.run.as_str())
                .map(|gate| {
                    (
                        gate.entered.clone(),
                        gate.release.clone(),
                        gate.finished.clone(),
                    )
                });
            match gate {
                Some((entered, release, finished)) => {
                    entered.notify_one();
                    let _ = release.acquire().await;
                    Some(finished)
                }
                None => None,
            }
        };
        let result = self.retire_and_settle(store, record).await;
        #[cfg(test)]
        if let Some(finished) = finished {
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }
    async fn retire_and_settle(
        self,
        store: &SessionStore,
        record: &RemoteRecord,
    ) -> Result<Option<String>> {
        let Self {
            session,
            committed,
            owned,
        } = self;
        let captured = &record.native["capturedResult"];
        if !captured["selectedAttempt"].is_null() || !session["browser"]["consultation"].is_null() {
            if !committed || record.native["phase"] != "result" {
                return Ok(None);
            }
            let id = session["id"]
                .as_str()
                .context("Remote selected consultation ID missing")?;
            return Ok(crate::browser_attempt::settle_selected_saved(
                store,
                id,
                &CancellationToken::new(),
            )
            .await
            .err()
            .map(|error| {
                format!(
                    "Remote delivery is saved; selected child retirement remains unconfirmed: {error:#}"
                )
            }));
        }
        let Some(owned) = owned else {
            return Ok(None);
        };
        // The client has its reply and will not acknowledge again, so a transient journal
        // failure before the reply gets one retry here; it is idempotent. The pause lets a
        // brief Windows sharing violation on the journal's rename clear.
        let owned = match owned {
            Ok(owned) => Ok(owned),
            Err(first) => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                owned_creator(record, &session, committed)
                    .context("Remote owned target creator missing")
                    .and_then(|creator| commit_owned(store, record, &session, creator))
                    .map_err(|second| {
                        second.context(format!("after an earlier failure: {first:#}"))
                    })
            }
        };
        finish_owned(store, &session, owned).await
    }
}
async fn finish_owned(
    store: &SessionStore,
    session: &Value,
    owned: Result<(crate::browser_disposition::TargetJournal, Value)>,
) -> Result<Option<String>> {
    let cleanup = async {
        let (journal, creator) = owned?;
        let creator = &creator;
        let target = &creator["target"];
        let host = target["host"]
            .as_str()
            .context("Remote target host is missing")?;
        let port = u16::try_from(
            target["port"]
                .as_u64()
                .context("Remote target port is missing")?,
        )?;
        let cancel = CancellationToken::new();
        let timeout = std::time::Duration::from_secs(1);
        let endpoint = if let Some(ws) = target["browserWSEndpoint"].as_str() {
            crate::browser_chrome::AttachmentEndpoint {
                host: host.into(),
                port,
                browser_ws_endpoint: ws.into(),
            }
        } else {
            crate::browser_chrome::AttachmentEndpoint::inspect(host, port, timeout, &cancel).await?
        };
        let browser = endpoint.connect(timeout, &cancel).await?;
        let disposed = crate::browser_retirement::retire_committed(
            store, &journal, &browser, &endpoint, creator,
        )
        .await?;
        ensure!(
            disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent",
            "Remote target retirement remains unconfirmed"
        );
        crate::browser_runner::settle_completed_retirement(
            store,
            &journal,
            creator,
            &disposed,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // Its fresh Chrome may be disposed only now that the answer tab is retired.
    if let Some(id) = session["id"].as_str() {
        Box::pin(crate::browser_runner::settle_deferred_fresh(
            store,
            Some(id),
        ))
        .await;
    }
    Ok(cleanup.err().map(|error| {
        format!("Remote delivery is saved; target retirement remains unconfirmed: {error:#}")
    }))
}
/// Leaves `run`'s session as a native remote run does once its final result is saved and
/// before the client acknowledges it: owned answer target `creator` (a live page showing the
/// synthetic completed conversation), acknowledged dispatch, capture ACK, final record.
#[cfg(test)]
pub(crate) fn native_ack_fixture(
    store: &SessionStore,
    run: &str,
    creator: &Value,
) -> Result<RemoteRecord> {
    let id = session_id(run)?;
    let native = crate::process::ProcessIdentity::current()?;
    let source = store.require(&id)?["options"]["prompt"]
        .as_str()
        .context("Fixture prompt missing")?
        .to_owned();
    let hash = crate::browser::prompt_fingerprint(&source, "PUBLIC-user");
    let result = json!({"answerText":"PUBLIC answer","answerMarkdown":"PUBLIC answer","tookMs":1.0,
        "ownedRecoveryTarget":{"creator":creator},"conversationId":"PUBLIC-conversation","submittedPromptHash":hash,
        "submittedPromptHashFormat":"original-source-v1","assistantMessageId":"PUBLIC-assistant",
        // As production remote capture records it; retirement checks text against the page.
        "assistantContent":{"sha256":artifacts::sha256(b"PUBLIC answer"),"representation":"text"}});
    let target = &creator["target"];
    store.update(&id, |current| Ok(Some(json!({"status":"running",
        "lifecycle":{"workerPid":native.pid,"workerProcessStartedAtMs":native.started_at_ms},
        "browser":crate::sessions::merge_object(&current["browser"],&json!({"config":{"keepBrowser":false},"remoteDelivery":{"runId":run},
            "runtime":{"controllerPid":native.pid,"controllerProcessStartedAtMs":native.started_at_ms,"chromeTargetId":target["targetId"],
                "chromeHost":target["host"],"chromePort":target["port"],"chromeBrowserWSEndpoint":target["browserWSEndpoint"],
                "conversationId":"PUBLIC-conversation","submittedPromptHash":hash,"submittedPromptHashFormat":"original-source-v1",
                "userCommitReceipt":{"messageId":"PUBLIC-user"},"acceptedObservation":{"lastAssistantMessageId":"PUBLIC-assistant","text":"PUBLIC answer"},
                "ownedRecoveryTarget":{"creator":creator,"targetId":target["targetId"],"claimId":target["claimId"]}}}))}))))?;
    crate::browser_disposition::completed_dispatch_fixture(store, &id, creator);
    store.update(&id, |current| Ok(Some(json!({"dispatch":crate::sessions::merge_object(&current["dispatch"],&json!({"state":"acknowledged"}))}))))?;
    let cancel = CancellationToken::new();
    let capture = save(
        store,
        run,
        &result,
        &result,
        &json!([]),
        Phase::Capture,
        &cancel,
    )?;
    let (partial, committed) = complete_acknowledgement(store, &capture)?;
    ensure!(
        committed && partial["status"] == "partial",
        "Fixture capture ACK did not commit"
    );
    save(
        store,
        run,
        &result,
        &result,
        &json!([]),
        Phase::Result,
        &cancel,
    )?;
    read(store, run)
}
#[cfg(test)]
mod tests {
    use super::*;
    const RUN: &str = "11111111-1111-4111-8111-111111111111";
    fn overlay(current: &Value, changes: &Value) -> Value {
        if let (Some(current), Some(changes)) = (current.as_object(), changes.as_object()) {
            let mut merged = current.clone();
            for (key, value) in changes {
                merged.insert(
                    key.clone(),
                    overlay(current.get(key).unwrap_or(&Value::Null), value),
                );
            }
            Value::Object(merged)
        } else {
            changes.clone()
        }
    }
    fn fixture() -> (tempfile::TempDir, SessionStore, Value) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        store
            .create(
                json!({"prompt":"PUBLIC remote capture"}),
                home.path(),
                Some(&session_id(RUN).unwrap()),
            )
            .unwrap();
        let result = json!({"answerText":"PUBLIC café answer","answerMarkdown":"**PUBLIC café answer**","tookMs":1.0});
        (home, store, result)
    }
    fn owned_fixture(store: &SessionStore, result: &mut Value) -> String {
        let id = session_id(RUN).unwrap();
        let creator = json!({"controllerPid":123,"controllerStartedAtMs":456,"target":{"targetId":"PUBLIC-target","host":"127.0.0.1","port":9222,"browserWSEndpoint":"ws://127.0.0.1:9222/PUBLIC"}});
        result["ownedRecoveryTarget"] = json!({"creator":creator});
        result["conversationId"] = json!("PUBLIC-conversation");
        result["submittedPromptHash"] = json!("PUBLIC-hash");
        result["submittedPromptHashFormat"] = json!("original-source-v1");
        store.update(&id, |_| Ok(Some(json!({"status":"running","lifecycle":{"workerPid":123},"browser":{"remoteDelivery":{"runId":RUN},"runtime":{
            "controllerPid":123,"controllerProcessStartedAtMs":456,"chromeTargetId":"PUBLIC-target","chromeHost":"127.0.0.1","chromePort":9222,
            "chromeBrowserWSEndpoint":"ws://127.0.0.1:9222/PUBLIC","conversationId":"PUBLIC-conversation","submittedPromptHash":"PUBLIC-hash",
            "submittedPromptHashFormat":"original-source-v1","ownedRecoveryTarget":{"creator":creator}}}})))).unwrap();
        id
    }
    #[test]
    fn final_ack_completes_once_and_late_capture_never_downgrades_it() {
        let (_home, store, mut result) = fixture();
        let id = owned_fixture(&store, &mut result);
        let cancel = CancellationToken::new();
        let capture = save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Capture,
            &cancel,
        )
        .unwrap();
        let (partial, committed) = complete_acknowledgement(&store, &capture).unwrap();
        assert!(committed);
        assert_eq!(partial["status"], "partial");
        let record = save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Result,
            &cancel,
        )
        .unwrap();
        let (completed, committed) = complete_acknowledgement(&store, &record).unwrap();
        assert!(committed);
        assert_eq!(completed["status"], "completed");
        let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
        let receipt: artifacts::CaptureReceipt =
            serde_json::from_value(completed["browser"]["capturedAnswer"].clone()).unwrap();
        assert_eq!(
            artifacts::verify_capture(&store, &receipt).unwrap(),
            result["answerMarkdown"].as_str().unwrap().as_bytes()
        );
        let count = fs::read_dir(store.directory(&id).unwrap()).unwrap().count();
        let (duplicate, committed) = complete_acknowledgement(&store, &record).unwrap();
        assert!(committed);
        assert_eq!(duplicate, completed);
        assert_eq!(
            fs::read_dir(store.directory(&id).unwrap()).unwrap().count(),
            count
        );
        assert!(!complete_acknowledgement(&store, &capture).unwrap().1);
        assert_eq!(
            fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
            before
        );
    }
    #[test]
    fn integral_float_controller_and_creator_fields_match_their_javascript_wire_identity() {
        let (_home, store, mut result) = fixture();
        let id = owned_fixture(&store, &mut result);
        result["ownedRecoveryTarget"]["creator"]["controllerStartedAtMs"] = json!(456.0);
        let creator = result["ownedRecoveryTarget"]["creator"].clone();
        store.update(&id, |current| Ok(Some(overlay(current,&json!({"browser":{"runtime":{"controllerProcessStartedAtMs":456.0,"ownedRecoveryTarget":{"creator":creator}}}}))))).unwrap();
        save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Result,
            &CancellationToken::new(),
        )
        .unwrap();
        let record = read(&store, RUN).unwrap();
        assert_eq!(
            record.native()["capturedResult"]["ownedRecoveryTarget"]["creator"]["controllerStartedAtMs"],
            456
        );
        let (completed, committed) = complete_acknowledgement_lossless(&store, &record).unwrap();
        assert!(committed);
        assert_eq!(completed["status"], "completed");
        assert_eq!(
            completed["browser"]["runtime"]["ownedRecoveryTarget"]["creator"],
            creator
        );
    }
    #[test]
    fn malformed_truthy_creators_never_bypass_an_owned_producers_completion_guards() {
        for malformed in [json!("PUBLIC"), json!([]), json!(true), json!(1)] {
            let (_home, store, mut result) = fixture();
            let id = owned_fixture(&store, &mut result);
            result["ownedRecoveryTarget"]["creator"] = malformed;
            save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Result,
                &CancellationToken::new(),
            )
            .unwrap();
            let record = read(&store, RUN).unwrap();
            let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
            assert!(
                !complete_acknowledgement_lossless(&store, &record)
                    .unwrap()
                    .1
            );
            assert_eq!(
                fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
                before
            );
        }
    }
    #[test]
    fn legacy_null_creator_start_requires_a_missing_runtime_start_and_refuses_explicit_null() {
        for explicit_null in [false, true] {
            let (_home, store, mut result) = fixture();
            let id = owned_fixture(&store, &mut result);
            result["ownedRecoveryTarget"]["creator"]["controllerStartedAtMs"] = Value::Null;
            store
                .update(&id, |current| {
                    let mut changed = current.clone();
                    changed["browser"]["runtime"]["ownedRecoveryTarget"]["creator"] =
                        result["ownedRecoveryTarget"]["creator"].clone();
                    if explicit_null {
                        changed["browser"]["runtime"]["controllerProcessStartedAtMs"] = Value::Null;
                    } else {
                        changed["browser"]["runtime"]
                            .as_object_mut()
                            .unwrap()
                            .remove("controllerProcessStartedAtMs");
                    }
                    Ok(Some(changed))
                })
                .unwrap();
            save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Result,
                &CancellationToken::new(),
            )
            .unwrap();
            let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
            let (_, committed) =
                complete_acknowledgement_lossless(&store, &read(&store, RUN).unwrap()).unwrap();
            assert_eq!(committed, !explicit_null);
            if explicit_null {
                assert_eq!(
                    fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
                    before
                );
            }
        }
    }
    #[tokio::test]
    async fn float_creator_finalisation_reuses_the_exact_saved_journal_creator() {
        use crate::{
            browser_disposition::{TargetCreation, TargetJournal},
            process::ProcessIdentity,
        };
        for previously_completed in [false, true] {
            let (_home, store, mut result) = fixture();
            let id = owned_fixture(&store, &mut result);
            let journal = TargetJournal::open(&store).unwrap();
            let native = ProcessIdentity::current().unwrap();
            let creator=journal.record_creation(&TargetCreation::allocated("PUBLIC-target"),
            &json!({"targetId":"PUBLIC-target","host":"127.0.0.1","port":9222,"browserWSEndpoint":"PUBLIC invalid endpoint","claimId":Uuid::new_v4().to_string()}),
            &native,Some(&id),None).unwrap();
            result["ownedRecoveryTarget"] = json!({"creator":creator});
            result["assistantMessageId"] = json!("PUBLIC-assistant");
            crate::browser_disposition::completed_dispatch_fixture(&store, &id, &creator);
            // Native capture returns before ordinary completion. The final ACK
            // must finish this exact acknowledged dispatch before journal commit.
            store.update(&id, |current| Ok(Some(json!({"dispatch":crate::sessions::merge_object(&current["dispatch"], &json!({"state":"acknowledged"}))})))).unwrap();
            store.update(&id, |current| Ok(Some(overlay(current,&json!({"lifecycle":{"workerPid":native.pid,"workerProcessStartedAtMs":native.started_at_ms},"browser":{"runtime":{
            "controllerPid":native.pid,"controllerProcessStartedAtMs":native.started_at_ms,"chromeBrowserWSEndpoint":"PUBLIC invalid endpoint",
            "ownedRecoveryTarget":{"creator":creator,"targetId":"PUBLIC-target","claimId":creator["target"]["claimId"]}}}}))))).unwrap();
            let capture = save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Capture,
                &CancellationToken::new(),
            )
            .unwrap();
            let (partial, committed) = complete_acknowledgement(&store, &capture).unwrap();
            assert!(committed);
            assert_eq!(partial["status"], "partial");
            assert_eq!(partial["dispatch"]["state"], "acknowledged");
            assert!(journal.read(&creator["allocationId"]).unwrap()["commit"].is_null());
            save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Result,
                &CancellationToken::new(),
            )
            .unwrap();
            let record = read(&store, RUN).unwrap();
            let acknowledged = store.stored_for_update(&id).unwrap();
            for changes in [
                json!({"dispatch":Value::Null}),
                json!({"dispatch":{"state":"started"}}),
                json!({"dispatch":{"reservationId":Uuid::new_v4().to_string()}}),
                json!({"dispatch":{"submissionOwner":{"reservationId":Uuid::new_v4().to_string()}}}),
                json!({"browser":{"runtime":{"submissionOwner":{"claimId":Uuid::new_v4().to_string()}}}}),
            ] {
                store
                    .update(&id, |_| Ok(Some(overlay(&acknowledged, &changes))))
                    .unwrap();
                let before = store.stored_for_update(&id).unwrap();
                assert!(
                    complete_acknowledgement_lossless(&store, &record).is_err(),
                    "Invalid dispatch accepted: {changes}"
                );
                assert_eq!(store.stored_for_update(&id).unwrap(), before);
                assert!(journal.read(&creator["allocationId"]).unwrap()["commit"].is_null());
            }
            let mut missing_dispatch = acknowledged.clone();
            missing_dispatch.as_object_mut().unwrap().remove("dispatch");
            crate::storage::atomic_json(
                &store.directory(&id).unwrap().join("meta.json"),
                &missing_dispatch,
            )
            .unwrap();
            assert!(complete_acknowledgement_lossless(&store, &record).is_err());
            assert_eq!(store.stored_for_update(&id).unwrap(), missing_dispatch);
            store
                .update(&id, |_| Ok(Some(acknowledged.clone())))
                .unwrap();
            if previously_completed {
                let prior_capture = artifacts::capture(
                    &store,
                    &id,
                    "PUBLIC-prior-capture.md",
                    result["answerMarkdown"].as_str().unwrap().as_bytes(),
                    &CancellationToken::new(),
                )
                .unwrap();
                store.update(&id,|current|Ok(Some(overlay(current,&json!({"status":"completed","browser":{"capturedAnswer":prior_capture,"remoteDelivery":{"runId":RUN,"acknowledged":true,"producerFinished":true}}}))))).unwrap();
            }
            // A remote host replies after this step alone, so it commits the journal record.
            // A transient failure there leaves it uncommitted until finish retries once.
            let transient = !previously_completed;
            TEST_COMMIT_FAILURES.with(|left| left.set(u32::from(transient)));
            let acknowledgement = commit_acknowledgement(&store, &record).unwrap();
            let saved = journal.read(&creator["allocationId"]).unwrap();
            assert_eq!(
                saved["commit"].is_object() && saved["finalisedAt"].is_string(),
                !transient
            );
            // Endpoint validation deliberately stops before any network operation, after commitment.
            assert!(
                acknowledgement
                    .finish(&store, &record)
                    .await
                    .unwrap()
                    .is_some()
            );
            let saved = journal.read(&creator["allocationId"]).unwrap();
            let completed = store.stored_for_update(&id).unwrap();
            assert_eq!(completed["dispatch"]["state"], "completed");
            assert_eq!(
                completed["dispatch"]["reservationId"],
                acknowledged["dispatch"]["reservationId"]
            );
            assert_eq!(
                completed["dispatch"]["submissionOwner"],
                acknowledged["dispatch"]["submissionOwner"]
            );
            assert_eq!(saved["creator"], creator);
            assert!(saved["commit"].is_object());
            assert!(saved["finalisedAt"].is_string());
            let before = saved["commit"].clone();
            assert!(
                finalise_acknowledgement(&store, &record)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                journal.read(&creator["allocationId"]).unwrap()["commit"],
                before
            );
            assert_eq!(store.stored_for_update(&id).unwrap(), completed);
        }
    }
    #[tokio::test]
    async fn retirement_failure_preserves_completed_delivery_and_durable_ack() {
        let (_home, store, mut result) = fixture();
        let id = owned_fixture(&store, &mut result);
        save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Result,
            &CancellationToken::new(),
        )
        .unwrap();
        let record = read(&store, RUN).unwrap();
        let warning = finalise_acknowledgement(&store, &record).await.unwrap();
        assert!(
            warning
                .unwrap()
                .starts_with("Remote delivery is saved; target retirement remains unconfirmed: ")
        );
        assert_eq!(store.require(&id).unwrap()["status"], "completed");
        assert!(
            store
                .directory(&id)
                .unwrap()
                .join("remote-ack.json")
                .is_file()
        );
        let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
        assert!(
            finalise_acknowledgement(&store, &record)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
            before
        );
    }
    /// A borrowed remote run's acknowledgement retries the launching session's idle browser
    /// through the profile hint, even when its own retirement is unconfirmed.
    #[tokio::test]
    async fn remote_ack_retries_the_hinted_idle_launcher() {
        let (home, store, mut result) = fixture();
        let id = owned_fixture(&store, &mut result);
        let profile = home.path().join("PUBLIC-persistent-profile");
        fs::create_dir(&profile).unwrap();
        let lease = json!({"profileDir":profile});
        result["ownedRecoveryTarget"]["creator"]["lease"] = lease.clone();
        store.update(&id,|current|Ok(Some(overlay(current,&json!({"browser":{"runtime":{"ownedRecoveryTarget":{"creator":{"lease":lease}}}}}))))).unwrap();
        let owner = store
            .create(json!({"prompt":"PUBLIC idle launcher"}), home.path(), None)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // Malformed saved authority refuses before any browser IO and records retention.
        store.update(&owner,|current|Ok(Some(overlay(current,&json!({"status":"completed","recoveryRequired":false,
            "browser":{"runtime":{"persistentBrowserOwner":{"version":1,"kind":"PUBLIC-malformed"},"targetDispositionPending":false}}}))))).unwrap();
        let registry = crate::browser_profile::ProfileRegistry::open_existing(&profile).unwrap();
        let disposal = |store: &SessionStore| {
            store.stored_for_update(&owner).unwrap()["browser"]["runtime"]
                ["persistentBrowserDisposal"]
                .clone()
        };
        // Without a hint nothing is retried. The proof that ACK reaches the hint is that
        // this test fails when settle_deferred_fresh omits settle_hinted_idle.
        crate::browser_runner::settle_hinted_idle(&store, &id, false).await;
        assert!(disposal(&store).is_null());
        registry.set_idle_owner(&owner).unwrap();
        save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Result,
            &CancellationToken::new(),
        )
        .unwrap();
        let record = read(&store, RUN).unwrap();
        assert!(
            finalise_acknowledgement(&store, &record)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(store.require(&id).unwrap()["status"], "completed");
        let retained = disposal(&store);
        assert_eq!(retained["disposition"], "retained");
        assert_eq!(retained["profileRetained"], true);
        assert_eq!(store.require(&owner).unwrap()["status"], "completed");
    }
    #[test]
    fn cancelled_or_replaced_producer_can_never_complete_from_an_old_ack() {
        let changes = [
            json!({"status":"cancelled"}),
            json!({"browser":{"remoteDelivery":{"runId":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"controllerPid":124}}}),
            json!({"browser":{"runtime":{"controllerProcessStartedAtMs":457}}}),
            json!({"lifecycle":{"workerPid":124}}),
            json!({"browser":{"runtime":{"chromeTargetId":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"chromeHost":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"chromePort":9223}}}),
            json!({"browser":{"runtime":{"chromeBrowserWSEndpoint":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"conversationId":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"submittedPromptHash":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"submittedPromptHashFormat":"PUBLIC-other"}}}),
            json!({"browser":{"runtime":{"ownedRecoveryTarget":{"creator":{"controllerPid":124}}}}}),
        ];
        for change in changes {
            let (_home, store, mut result) = fixture();
            let id = owned_fixture(&store, &mut result);
            let record = save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Result,
                &CancellationToken::new(),
            )
            .unwrap();
            store
                .update(&id, |current| Ok(Some(overlay(current, &change))))
                .unwrap();
            let before = fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
            assert!(
                !complete_acknowledgement(&store, &record).unwrap().1,
                "{change}"
            );
            assert_eq!(
                fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
                before
            );
            assert!(
                store
                    .directory(&id)
                    .unwrap()
                    .join("remote-ack.json")
                    .is_file()
            );
        }
    }
    #[test]
    fn final_record_is_preferred_and_corrupt_final_falls_back_to_durable_capture() {
        let (_home, store, result) = fixture();
        let cancel = CancellationToken::new();
        let capture = save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Capture,
            &cancel,
        )
        .unwrap();
        assert_eq!(read(&store, RUN).unwrap().native(), &capture);
        let final_result = save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Result,
            &cancel,
        )
        .unwrap();
        assert_eq!(read(&store, RUN).unwrap().native(), &final_result);
        fs::write(
            store
                .directory(&session_id(RUN).unwrap())
                .unwrap()
                .join("remote-result.json"),
            b"PUBLIC corrupt final",
        )
        .unwrap();
        assert_eq!(read(&store, RUN).unwrap().native(), &capture);
    }
    #[test]
    fn exact_duplicate_ack_preserves_bytes_and_changed_ack_is_refused() {
        let (_home, store, result) = fixture();
        let cancel = CancellationToken::new();
        let record = save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Result,
            &cancel,
        )
        .unwrap();
        acknowledge(&store, &record).unwrap();
        let path = store
            .directory(&session_id(RUN).unwrap())
            .unwrap()
            .join("remote-ack.json");
        let before = fs::read(&path).unwrap();
        let identity = NativeEntry::open(&path, false).unwrap().identity;
        acknowledge(&store, &record).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(NativeEntry::open(&path, false).unwrap().identity, identity);
        let mut changed = record;
        changed["result"]["answerText"] = json!("PUBLIC changed answer");
        changed["delivery"]["sha256"] =
            json!(remote_json::result_digest(RUN, &changed["result"], &json!([])).unwrap());
        assert!(acknowledge(&store, &changed).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    #[test]
    fn capture_and_final_files_never_replace_and_cancelled_capture_publishes_nothing() {
        let (_home, store, result) = fixture();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Capture,
                &cancelled
            )
            .is_err()
        );
        assert!(
            !store
                .directory(&session_id(RUN).unwrap())
                .unwrap()
                .join("remote-capture.json")
                .exists()
        );
        save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Capture,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(
            save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Capture,
                &CancellationToken::new()
            )
            .is_err()
        );
    }
    #[test]
    fn modification_during_a_read_cannot_authorise_recovery() {
        let (_home, store, result) = fixture();
        save(
            &store,
            RUN,
            &result,
            &result,
            &json!([]),
            Phase::Capture,
            &CancellationToken::new(),
        )
        .unwrap();
        let id = session_id(RUN).unwrap();
        let path = store.directory(&id).unwrap().join("remote-capture.json");
        let error = read_record_with(&store, &id, "remote-capture.json", false, || {
            fs::write(&path, b"PUBLIC changed bytes").unwrap();
        });
        assert!(error.is_err());
    }
    #[test]
    fn capture_promotion_preserves_surrogates_and_existing_native_files() {
        let (_home, store, _) = fixture();
        let id = session_id(RUN).unwrap();
        let result =
            r#"{"answerText":"\ud800","answerMarkdown":"\udfff","\ud800":"PUBLIC preserved key"}"#;
        let raw: &RawValue = serde_json::from_str(result).unwrap();
        let digest = remote_json::result_digest_raw(RUN, raw, &[]).unwrap();
        let delivery = json!({"version":1,"runId":RUN,"sha256":digest});
        let wire = format!(
            "{{\"version\":1,\"phase\":\"capture\",\"runId\":\"{RUN}\",\"capturedResult\":{result},\"result\":{result},\"artifacts\":[],\"delivery\":{delivery}}}"
        );
        artifacts::capture(
            &store,
            &id,
            "remote-capture.json",
            wire.as_bytes(),
            &CancellationToken::new(),
        )
        .unwrap();
        let capture = read_phase(&store, RUN, Phase::Capture).unwrap();
        let first = promote_capture(&store, &capture, &CancellationToken::new(), None).unwrap();
        assert!(first.wire().contains(result));
        assert_eq!(first.native()["delivery"], delivery);
        let path = store.directory(&id).unwrap().join("remote-result.json");
        let bytes = fs::read(&path).unwrap();
        let identity = NativeEntry::open(&path, false).unwrap().identity;
        for _ in 0..2 {
            assert_eq!(
                promote_capture(&store, &capture, &CancellationToken::new(), None)
                    .unwrap()
                    .wire(),
                first.wire()
            );
        }
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(NativeEntry::open(&path, false).unwrap().identity, identity);
        assert_eq!(
            fs::read(store.directory(&id).unwrap().join("remote-capture.json")).unwrap(),
            wire.as_bytes()
        );
    }
    #[test]
    fn promotion_refuses_cancelled_expired_or_different_final_record_without_replacement() {
        for case in ["cancelled", "expired", "different"] {
            let (_home, store, result) = fixture();
            let cancel = CancellationToken::new();
            save(
                &store,
                RUN,
                &result,
                &result,
                &json!([]),
                Phase::Capture,
                &cancel,
            )
            .unwrap();
            let capture = read_phase(&store, RUN, Phase::Capture).unwrap();
            let path = store
                .directory(&session_id(RUN).unwrap())
                .unwrap()
                .join("remote-result.json");
            let mut before = None;
            if case == "different" {
                let mut changed = result.clone();
                changed["answerText"] = json!("PUBLIC different answer");
                save(
                    &store,
                    RUN,
                    &changed,
                    &changed,
                    &json!([]),
                    Phase::Result,
                    &cancel,
                )
                .unwrap();
                before = Some(fs::read(&path).unwrap());
            } else if case == "cancelled" {
                cancel.cancel();
            }
            assert!(
                promote_capture(
                    &store,
                    &capture,
                    &cancel,
                    (case == "expired").then(std::time::Instant::now)
                )
                .is_err(),
                "{case}"
            );
            assert_eq!(fs::read(&path).ok(), before);
        }
    }
    #[test]
    fn javascript_surrogate_records_recover_and_acknowledge_without_changing_wire_bytes() {
        let (_home, store, _) = fixture();
        let id = session_id(RUN).unwrap();
        let result =
            r#"{"answerText":"\ud800","answerMarkdown":"\udfff","\ud800":"PUBLIC preserved key"}"#;
        let raw: &RawValue = serde_json::from_str(result).unwrap();
        let digest = remote_json::result_digest_raw(RUN, raw, &[]).unwrap();
        let delivery = json!({"version":1,"runId":RUN,"sha256":digest});
        let bytes = format!(
            "{{\"version\":1,\"phase\":\"result\",\"runId\":\"{RUN}\",\"capturedResult\":{result},\"result\":{result},\"artifacts\":[],\"delivery\":{delivery}}}"
        );
        artifacts::capture(
            &store,
            &id,
            "remote-result.json",
            bytes.as_bytes(),
            &CancellationToken::new(),
        )
        .unwrap();
        store
            .update(&id, |_| {
                Ok(Some(
                    json!({"status":"running","browser":{"remoteDelivery":{"runId":RUN}}}),
                ))
            })
            .unwrap();
        let record = read(&store, RUN).unwrap();
        assert!(record.wire().contains(r#""answerText":"\ud800""#));
        assert!(record.wire().contains(r#""\ud800":"PUBLIC preserved key""#));
        assert_eq!(record.native()["result"]["answerText"], "\u{fffd}");
        assert_eq!(record.native()["delivery"], delivery);
        let (completed, committed) = complete_acknowledgement_lossless(&store, &record).unwrap();
        assert!(committed);
        assert_eq!(completed["status"], "completed");
        let before = fs::read(store.directory(&id).unwrap().join("remote-ack.json")).unwrap();
        acknowledge_lossless(&store, &read(&store, RUN).unwrap()).unwrap();
        assert_eq!(
            fs::read(store.directory(&id).unwrap().join("remote-ack.json")).unwrap(),
            before
        );
        assert_eq!(
            fs::read(store.directory(&id).unwrap().join("remote-result.json")).unwrap(),
            bytes.as_bytes()
        );
    }
}

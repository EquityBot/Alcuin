//! Capture-only recovery keeps original controller identities and reciprocal seals.
use super::*;
use crate::{
    browser_chrome::AttachmentEndpoint, browser_profile::ProfileRegistry,
    browser_runner::RunnerLimits, cdp::Cdp, process::Liveness, storage::DirectoryLock,
};
use std::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Not serialisable or constructible outside this module. Locks are released child first.
pub(crate) struct Guard {
    child_lock: DirectoryLock,
    parent_lock: DirectoryLock,
    store: SessionStore,
    link: Link,
    parent_directory: NativeEntry,
    child_directory: NativeEntry,
    child_controller: Value,
    initial_consultation: Value,
    initial_remote: Value,
    remote_publication_started: AtomicBool,
    remote_publication_sealed: AtomicBool,
    partial_parent: bool,
    adopting: AtomicBool,
    creator_identity: Value,
    source: Value,
    initial_receipt: Value,
    initial_dispatch_state: Value,
    sealed_receipt: OnceLock<Value>,
    initial_turn: Value,
    sealed_turn: OnceLock<Value>,
    delivered_turn: OnceLock<Value>,
    admitted: AtomicBool,
    publishing: AtomicBool,
    operational: ProcessIdentity,
    deadline: Instant,
    cancel: CancellationToken,
}

fn source(child: &Value) -> Value {
    let mut runtime = json!({});
    for key in [
        "chromePid",
        "chromeHost",
        "chromePort",
        "chromeBrowserWSEndpoint",
        "browserWSEndpoint",
        "chromeTargetId",
        "profileDir",
        "tabLeaseId",
        "submissionOwner",
        "commitSnapshot",
        "modelSelection",
        "thinkingSelection",
        "effectiveModel",
        "borrowedTarget",
    ] {
        runtime[key] = child["browser"]["runtime"][key].clone();
    }
    let mut owned = child["browser"]["runtime"]["ownedRecoveryTarget"].clone();
    if let Some(owned) = owned.as_object_mut() {
        owned.remove("conversationId");
    }
    let mut dispatch = json!({});
    for key in [
        "reservationId",
        "submissionOwner",
        "promptSha256",
        "providerDispatchId",
        "startedAt",
        "turnIndex",
        "history",
    ] {
        dispatch[key] = child["dispatch"][key].clone();
    }
    json!({"runtime":runtime,"ownedRecoveryTarget":owned,"dispatch":dispatch})
}
fn turn(child: &Value) -> Value {
    json!([
        child["dispatch"]["turnCapture"],
        child["browser"]["runtime"]["completedBrowserTurns"]
    ])
}
fn receipt(child: &Value) -> Value {
    let runtime = &child["browser"]["runtime"];
    json!([
        runtime["userCommitReceipt"],
        runtime["conversationId"],
        runtime["submittedPromptHash"],
        runtime["submittedPromptHashFormat"],
        runtime["ownedRecoveryTarget"],
        runtime["grokCommitReceipt"]
    ])
}
fn original_owner(meta: &Value) -> Result<ProcessIdentity> {
    let owner: ProcessIdentity =
        serde_json::from_value(meta["lifecycle"]["processIdentity"].clone())?;
    ensure!(
        owner.liveness() == Liveness::Dead
            && meta["lifecycle"]["engine"] == "browser"
            && meta["lifecycle"]["workerPid"] == owner.pid
            && meta["lifecycle"]["workerProcessStartedAtMs"].as_f64() == Some(owner.started_at_ms),
        "Parent recovery requires positively dead exact original workers"
    );
    ensure!(
        meta["browser"]["runtime"]["controllerPid"] == owner.pid
            && meta["browser"]["runtime"]["controllerProcessStartedAtMs"].as_f64()
                == Some(owner.started_at_ms),
        "Recovery runtime controller differs from the exact original worker"
    );
    Ok(owner)
}
/// An undecided provider switch with Grok active, no answer and a positively dead worker.
fn exited_switch(parent: &Value) -> bool {
    let state = &parent["browser"]["consultation"];
    state["switch"].is_object()
        && state["switch"]["outcome"].is_null()
        && state["active"]["provider"] == "grok"
        && state["selected"].is_null()
        && state["adoption"].is_null()
        && parent["capturedAnswer"].is_null()
        && original_owner(parent).is_ok()
}
fn unsent_grok(store: &SessionStore, parent: &Value) -> Result<bool> {
    let grok = store.require(
        parent["browser"]["consultation"]["active"]["childId"]
            .as_str()
            .context("Provider switch has no Grok attempt")?,
    )?;
    Ok(super::unsent_grok(parent, &grok))
}
/// A worker that exited between the provider switch and a Grok Send leaves Grok active.
/// Hand the consultation back to its original ChatGPT attempt before resolving recovery.
/// Anything else, including a Grok attempt that may have sent, is left active for the
/// caller's own checks.
pub(crate) fn restore_after_exit(store: &SessionStore, id: &str) -> Result<bool> {
    let parent = store.stored_for_update(id)?;
    let state = &parent["browser"]["consultation"];
    if !exited_switch(&parent) || !unsent_grok(store, &parent)? {
        return Ok(false);
    }
    store.update(id, |current| {
        ensure!(
            current["browser"]["consultation"] == *state
                && current["lifecycle"] == parent["lifecycle"],
            "Consultation changed before its provider switch restoration"
        );
        let restored = super::unsent_restoration(store, current, "worker-exited-before-grok-send")?;
        Ok(Some(
            json!({"browser":merge_object(&current["browser"],&json!({"consultation":restored}))}),
        ))
    })?;
    Ok(true)
}
/// Entry points resolve endpoints through the active attempt, so restore an unsent switch
/// whose worker exited first, under the parent recovery lock. A refused restoration (Grok
/// may have sent) leaves Grok active for the caller's own checks.
pub(crate) async fn settle_unsent_switch(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<bool> {
    let saved = store.require(id)?;
    let parent_id = if internal(&saved) {
        saved["browser"]["attempt"]["parentId"]
            .as_str()
            .context("Provider attempt has no parent")?
            .to_owned()
    } else {
        id.to_owned()
    };
    let parent = store.stored_for_update(&parent_id)?;
    if !exited_switch(&parent) || !unsent_grok(store, &parent)? {
        return Ok(false);
    }
    let _lock = DirectoryLock::acquire_async(
        &store
            .directory(&parent_id)?
            .join("native-browser-recovery.lock"),
        Duration::from_secs(10),
        cancel,
    )
    .await?;
    match restore_after_exit(store, &parent_id) {
        Ok(restored) => Ok(restored),
        // Changed since the unlocked check; the caller's own checks decide.
        Err(error) => {
            eprintln!("[browser] Provider switch left unrestored: {error:#}");
            Ok(false)
        }
    }
}
fn creator_identity(store: &SessionStore, child: &Value) -> Result<Value> {
    let creator = &child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let record =
        crate::browser_disposition::TargetJournal::open(store)?.read(&creator["allocationId"])?;
    let owner = original_owner(child)?;
    ensure!(
        creator.is_object()
            && record["creator"] == *creator
            && record["nativeOwnerIdentity"] == json!(owner)
            && creator["controllerPid"] == owner.pid
            && creator["controllerStartedAtMs"].as_f64() == Some(owner.started_at_ms),
        "Recovery target journal differs from its original native creator"
    );
    Ok(json!([record["creator"], record["nativeOwnerIdentity"]]))
}
fn eligible(store: &SessionStore, meta: &Value, partial: bool) -> Result<Value> {
    ensure!(
        meta["mode"] == "browser"
            && (matches!(meta["status"].as_str(), Some("running" | "error"))
                || partial && meta["status"] == "partial"),
        "Parent recovery cannot reopen this outcome"
    );
    original_owner(meta)?;
    let runs = store.model_runs(meta["id"].as_str().context("Recovery ID missing")?)?;
    let matching = runs
        .iter()
        .filter(|run| run["model"] == meta["model"])
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == 1
            && (matches!(
                matching[0]["status"].as_str(),
                Some("pending" | "running" | "error")
            ) || partial && matching[0]["status"] == "partial")
            && (meta["status"] == "error" || matching[0]["status"] != "error"),
        "Parent recovery model is missing or has a competing terminal outcome"
    );
    Ok(matching[0].clone())
}

fn remote_record(store: &SessionStore, run: &str) -> Result<crate::remote_result::RemoteRecord> {
    let path = store
        .directory(&crate::remote_result::session_id(run)?)?
        .join("remote-result.json");
    match std::fs::symlink_metadata(path) {
        Ok(_) => crate::remote_result::read_phase(store, run, crate::remote_result::Phase::Result),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::remote_result::read_phase(store, run, crate::remote_result::Phase::Capture)
        }
        Err(error) => Err(error.into()),
    }
}
fn remote_policy(
    store: &SessionStore,
    parent: &Value,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<bool> {
    let delivery = &parent["browser"]["remoteDelivery"];
    let callback = &parent["browser"]["consultation"]["callbackIssued"];
    if delivery.is_null() {
        ensure!(*callback == false, "Local recovery callback policy changed");
        return Ok(false);
    }
    let run = delivery["runId"]
        .as_str()
        .context("Remote recovery run missing")?;
    let id = parent["id"].as_str().context("Remote parent ID missing")?;
    ensure!(
        crate::remote_result::session_id(run)? == id
            && callback.is_boolean()
            && delivery["acknowledged"].is_boolean()
            && delivery["producerFinished"].is_boolean()
            && (delivery["captureIssued"].is_null() || delivery["captureIssued"].is_boolean())
            && (delivery["captureIssued"] == true) == (*callback == true),
        "Remote recovery delivery identity changed"
    );
    if *callback == true {
        let captured = adoption::captured_selected_until(store, id, cancel, Some(deadline))?;
        let record = remote_record(store, run)?;
        crate::remote_protocol::verify_recovered(
            store,
            run,
            &record,
            &captured,
            parent["options"]["imageOutputRequested"] == true,
            cancel,
            Some(deadline),
        )?;
    }
    if parent["status"] == "completed" {
        ensure!(
            delivery["acknowledged"] == true
                && delivery["producerFinished"] == true
                && *callback == true,
            "Remote completed recovery lacks final delivery policy"
        );
        crate::remote_result::require_acknowledged(
            store,
            &crate::remote_result::read_phase(store, run, crate::remote_result::Phase::Result)?,
        )?;
    } else {
        ensure!(
            delivery["producerFinished"] == false,
            "Uncompleted remote producer has final delivery state"
        );
        if delivery["acknowledged"] == true {
            ensure!(*callback == true, "Remote partial delivery has no callback");
            let capture =
                crate::remote_result::read_phase(store, run, crate::remote_result::Phase::Capture)?;
            let selected = adoption::captured_selected_until(store, id, cancel, Some(deadline))?;
            crate::remote_protocol::verify_recovered(
                store,
                run,
                &capture,
                &selected,
                parent["options"]["imageOutputRequested"] == true,
                cancel,
                Some(deadline),
            )?;
            crate::remote_result::require_acknowledged(store, &capture)?;
        }
    }
    let partial = parent["status"] == "partial";
    ensure!(
        !partial || delivery["acknowledged"] == true && *callback == true,
        "Only authenticated remote capture ACK may resume a partial parent"
    );
    Ok(partial)
}

impl Guard {
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    async fn acquire(
        store: &SessionStore,
        id: &str,
        limits: &RunnerLimits,
        cancel: &CancellationToken,
    ) -> Result<Arc<Self>> {
        let duration = limits
            .answer
            .saturating_add(limits.commit)
            .saturating_add(Duration::from_secs(30));
        let deadline = Instant::now()
            .checked_add(duration)
            .context("Invalid parent recovery deadline")?;
        let operational = ProcessIdentity::current()?;
        let parent_lock = DirectoryLock::acquire_async(
            &store.directory(id)?.join("native-browser-recovery.lock"),
            Duration::from_secs(10).min(deadline.saturating_duration_since(Instant::now())),
            cancel,
        )
        .await?;
        restore_after_exit(store, id)?;
        let parent = store.stored_for_update(id)?;
        let partial_parent = remote_policy(store, &parent, cancel, deadline)?;
        let completed = parent["status"] == "completed";
        let selected = parent["browser"]["consultation"]["selected"].is_object();
        let captured = parent["browser"]["consultation"]["adoption"]["phase"] == "captured";
        let reserved = parent["browser"]["consultation"]["adoption"]["phase"] == "reserved";
        if completed {
            original_owner(&parent)?;
            adoption::require_selected_until(store, id, cancel, Some(deadline))?;
        } else {
            eligible(store, &parent, partial_parent)?;
            if selected {
                adoption::require_selected_until(store, id, cancel, Some(deadline))?;
            }
        }
        let link: Link =
            serde_json::from_value(parent["browser"]["consultation"]["active"].clone())?;
        ensure!(
            link.parent_id == id,
            "Recovery does not match the public parent"
        );
        let child_lock = DirectoryLock::acquire_async(
            &store
                .directory(&link.child_id)?
                .join("native-browser-recovery.lock"),
            Duration::from_secs(10).min(deadline.saturating_duration_since(Instant::now())),
            cancel,
        )
        .await?;
        let child = store.stored_for_update(&link.child_id)?;
        // A Grok attempt runs in its parent's worker; another owner is never recovered here.
        ensure!(
            link.provider == Provider::Chatgpt
                || child["lifecycle"]["processIdentity"] == parent["lifecycle"]["processIdentity"]
                    && child["browser"]["runtime"]["provider"] == "grok",
            "Grok recovery requires its exact original parent worker and provider evidence"
        );
        if completed {
            original_owner(&child)?;
        } else {
            eligible(store, &child, false)?;
            if captured {
                adoption::require_captured(store, &parent, &child, cancel, Some(deadline))?;
            } else if reserved {
                adoption::require_reserved(store, &parent, &child, cancel, Some(deadline))?;
            }
        }
        ensure!(
            (completed
                || selected
                || captured
                || reserved
                || parent["browser"]["consultation"]["adoption"].is_null()
                    && parent["browser"]["consultation"]["selected"].is_null()
                    && parent["browser"]["consultation"]["callbackIssued"] == false
                    && parent["browser"]["capturedAnswer"].is_null())
                && child["dispatch"].is_object()
                && child["browser"]["runtime"]["borrowedTarget"].is_null(),
            "Recovery requires an original capture, verified reserved or captured adoption, or fully selected adoption; target borrowing is refused"
        );
        let guard = Arc::new(Self {
            child_lock,
            parent_lock,
            store: store.clone(),
            parent_directory: NativeEntry::open(&store.directory(id)?, true)?,
            child_directory: NativeEntry::open(&store.directory(&link.child_id)?, true)?,
            child_controller: controller_identity(&child),
            initial_consultation: parent["browser"]["consultation"].clone(),
            initial_remote: parent["browser"]["remoteDelivery"].clone(),
            remote_publication_started: AtomicBool::new(false),
            remote_publication_sealed: AtomicBool::new(false),
            partial_parent,
            adopting: AtomicBool::new(completed),
            creator_identity: creator_identity(store, &child)?,
            source: source(&child),
            initial_receipt: receipt(&child),
            initial_dispatch_state: child["dispatch"]["state"].clone(),
            sealed_receipt: OnceLock::new(),
            initial_turn: turn(&child),
            sealed_turn: OnceLock::new(),
            delivered_turn: OnceLock::new(),
            admitted: AtomicBool::new(completed),
            publishing: AtomicBool::new(completed),
            link,
            operational,
            deadline,
            cancel: cancel.clone(),
        });
        guard.require_session(id, &parent)?;
        Ok(guard)
    }
    fn require_base(&self) -> Result<(Value, Value)> {
        ensure!(
            !self.cancel.is_cancelled() && Instant::now() < self.deadline,
            "Parent recovery was cancelled or reached its deadline"
        );
        let (parent, child) = self.require_evidence()?;
        if parent["browser"]["consultation"]["selected"].is_object() {
            adoption::require_selected_until(
                &self.store,
                &self.link.parent_id,
                &self.cancel,
                Some(self.deadline),
            )?;
        } else if parent["browser"]["consultation"]["adoption"]["phase"] == "captured" {
            adoption::require_captured(
                &self.store,
                &parent,
                &child,
                &self.cancel,
                Some(self.deadline),
            )?;
        } else if parent["browser"]["consultation"]["adoption"]["phase"] == "reserved" {
            adoption::require_reserved(
                &self.store,
                &parent,
                &child,
                &self.cancel,
                Some(self.deadline),
            )?;
        }
        if self.admitted.load(Ordering::SeqCst) {
            let publishing = self.publishing.load(Ordering::SeqCst);
            for meta in [&parent, &child] {
                let status = if publishing && meta["status"] == "completed" {
                    "completed"
                } else {
                    "running"
                };
                let runs = self
                    .store
                    .model_runs(meta["id"].as_str().context("Recovery ID missing")?)?;
                let matching = runs
                    .iter()
                    .filter(|run| run["model"] == meta["model"])
                    .collect::<Vec<_>>();
                ensure!(
                    meta["status"] == status
                        && matching.len() == 1
                        && matching[0]["status"] == status,
                    "Public parent or child outcome changed during recovery"
                );
            }
        }
        Ok((parent, child))
    }
    // Failure reporting may run after cancellation/expiry, but grants no capture or close authority.
    fn require_evidence(&self) -> Result<(Value, Value)> {
        ensure!(
            ProcessIdentity::current()? == self.operational
                && self.operational.liveness() == Liveness::Alive,
            "Recovery operational process changed"
        );
        self.parent_lock.require_owned()?;
        self.child_lock.require_owned()?;
        self.parent_directory
            .require_matches(&self.store.directory(&self.link.parent_id)?)?;
        self.child_directory
            .require_matches(&self.store.directory(&self.link.child_id)?)?;
        // These raw reads and request/model reads acquire no metadata transaction.
        let parent = self.store.stored_for_update(&self.link.parent_id)?;
        let child = self.store.stored_for_update(&self.link.child_id)?;
        let unchanged = parent["browser"]["remoteDelivery"] == self.initial_remote
            && parent["browser"]["consultation"]["callbackIssued"]
                == self.initial_consultation["callbackIssued"];
        let issued = self.remote_publication_started.load(Ordering::SeqCst)
            && parent["browser"]["remoteDelivery"]
                == merge_object(&self.initial_remote, &json!({"captureIssued":true}))
            && parent["browser"]["consultation"]["callbackIssued"] == true;
        ensure!(
            (unchanged && !self.remote_publication_sealed.load(Ordering::SeqCst)) || issued,
            "Public recovery delivery policy changed"
        );
        if !self.adopting.load(Ordering::SeqCst) {
            ensure!(
                parent["browser"]["consultation"] == self.initial_consultation,
                "Public consultation changed during original-child observation"
            );
        }
        parent_identity(&parent, &self.link)?;
        require_storage_binding(&self.store, &self.link)?;
        require_payload(
            &self.store,
            &self.link.child_id,
            &parent,
            &child,
            &self.link,
        )?;
        original_owner(&parent)?;
        original_owner(&child)?;
        ensure!(
            controller_identity(&child) == self.child_controller && source(&child) == self.source,
            "Original child controller, source or target authority changed"
        );
        ensure!(
            creator_identity(&self.store, &child)? == self.creator_identity,
            "Original native target journal changed during recovery"
        );
        Ok((parent, child))
    }
    pub(crate) fn require_session(&self, id: &str, meta: &Value) -> Result<()> {
        let (parent, child) = self.require_base()?;
        let expected = if id == self.link.parent_id {
            &parent
        } else {
            ensure!(
                id == self.link.child_id,
                "Recovery witness belongs to a different session"
            );
            &child
        };
        ensure!(
            meta["id"] == id
                && controller_identity(meta) == controller_identity(expected)
                && meta["options"] == expected["options"],
            "Recovery metadata does not match its original session"
        );
        if id == self.link.child_id {
            ensure!(
                source(meta) == source(&child)
                    && receipt(meta) == receipt(&child)
                    && turn(meta) == turn(&child)
                    && meta["browser"]["attempt"] == child["browser"]["attempt"]
                    && meta["browser"]["attemptReady"] == child["browser"]["attemptReady"]
                    && meta["dispatch"]["state"] == child["dispatch"]["state"],
                "Supplied recovery source or commitment changed"
            );
        } else {
            parent_identity(meta, &self.link)?;
        }
        ensure!(
            receipt(&child) == *self.sealed_receipt.get().unwrap_or(&self.initial_receipt),
            "Recovered commitment changed outside its authorised transition"
        );
        let dispatch = if self.sealed_receipt.get().is_some() {
            json!("acknowledged")
        } else {
            self.initial_dispatch_state.clone()
        };
        ensure!(
            child["dispatch"]["state"] == dispatch
                || self.publishing.load(Ordering::SeqCst)
                    && child["status"] == "completed"
                    && child["dispatch"]["state"] == "completed",
            "Recovery dispatch state changed outside its authorised transition"
        );
        ensure!(
            turn(&child) == *self.turn_seal(),
            "Recovery turn capture changed outside its authorised transition"
        );
        Ok(())
    }
    pub(crate) fn seal_turn(&self, before: &Value, after: &Value, observed: &Value) -> Result<()> {
        let (_, current) = self.require_base()?;
        ensure!(
            self.sealed_turn.get().is_none()
                && *after == current
                && controller_identity(before) == self.child_controller
                && source(before) == self.source
                && receipt(before) == receipt(after)
                && turn(before) == self.initial_turn,
            "Recovery turn capture differs from the original admitted state"
        );
        let old = crate::browser_turns::saved(before)?;
        let next = crate::browser_turns::saved(after)?;
        let index = crate::browser_turns::index(after)?;
        ensure!(
            old.len() == index && next.len() == index + 1 && next[..index] == *old,
            "Recovery may capture only the outstanding final turn"
        );
        let row = &next[index];
        crate::browser::accept_answer(after, observed, &after["browser"]["runtime"])?;
        ensure!(
            row["receipt"] == after["browser"]["runtime"]["userCommitReceipt"]
                && row["assistantMessageId"] == observed["lastAssistantMessageId"]
                && row["assistantTurnId"] == observed["lastAssistantTurnId"]
                && row["report"] == observed["report"]
                && row["sourceSha256"]
                    == sha256(
                        observed["text"]
                            .as_str()
                            .context("Recovered turn source missing")?
                            .as_bytes()
                    )
                && after["dispatch"]["turnCapture"] == row["capture"],
            "Recovered final turn differs from the original completed response"
        );
        crate::browser_turns::verify_turn(&self.store, after, row, index)?;
        self.sealed_turn
            .set(turn(after))
            .map_err(|_| anyhow::anyhow!("Recovery turn was already sealed"))?;
        self.require_session(&self.link.child_id, after)
    }
    fn turn_seal(&self) -> &Value {
        self.delivered_turn
            .get()
            .or(self.sealed_turn.get())
            .unwrap_or(&self.initial_turn)
    }
    pub(crate) fn seal_delivery(
        &self,
        before: &Value,
        after: &Value,
        images: &Value,
    ) -> Result<()> {
        if turn(before) == turn(after) {
            return self.require_session(&self.link.child_id, after);
        }
        let (_, current) = self.require_base()?;
        ensure!(
            self.delivered_turn.get().is_none()
                && *after == current
                && controller_identity(before) == self.child_controller
                && source(before) == self.source
                && receipt(before) == receipt(after)
                && turn(before) == *self.turn_seal(),
            "Recovery delivery differs from its admitted final turn"
        );
        let index = crate::browser_turns::index(after)?;
        let old = crate::browser_turns::saved(before)?;
        let next = crate::browser_turns::saved(after)?;
        ensure!(
            old.len() == index + 1 && next.len() == old.len() && next[..index] == old[..index],
            "Recovery delivery changed earlier turns"
        );
        let mut expected = old[index].clone();
        expected["images"] = images.clone();
        ensure!(
            next[index] == expected
                && before["dispatch"]["turnCapture"] == after["dispatch"]["turnCapture"],
            "Recovery delivery changed the final source or capture"
        );
        crate::browser_turns::verify_turn(&self.store, after, &next[index], index)?;
        self.delivered_turn
            .set(turn(after))
            .map_err(|_| anyhow::anyhow!("Recovery delivery was already sealed"))?;
        self.require_session(&self.link.child_id, after)
    }
    pub(crate) fn acknowledge(
        &self,
        before: &Value,
        committed: &crate::browser_commit::CommitReceipt,
        tab_url: &Value,
    ) -> Result<Value> {
        self.require_session(&self.link.child_id, before)?;
        ensure!(
            self.sealed_receipt.get().is_none(),
            "Recovery commitment was already sealed"
        );
        let runtime = &before["browser"]["runtime"];
        let snapshot = &runtime["commitSnapshot"];
        serde_json::from_value::<crate::browser_commit::CommitSnapshot>(snapshot.clone())?
            .require_receipt(committed)?;
        let owner = json!(committed.owner);
        let conversation = &committed.conversation.conversation_id;
        ensure!(
            snapshot["owner"] == owner
                && runtime["submissionOwner"] == owner
                && committed.version == 1
                && committed.kind == "new-user-message"
                && committed.message_fingerprint_format == "original-source-v1"
                && committed.conversation.origin == "https://chatgpt.com"
                && tab_url
                    .as_str()
                    .and_then(crate::browser::conversation_id)
                    .as_ref()
                    == Some(conversation)
                && committed.prompt_text_fingerprint
                    == sha256(
                        snapshot["canonicalPrompt"]
                            .as_str()
                            .context("Original recovery prompt snapshot missing")?
                            .as_bytes()
                    ),
            "Recovered commitment differs from its original owner, prompt or conversation"
        );
        let value = json!(committed);
        // A lost acknowledgement on the final sequential Send can still carry the
        // preceding turn's receipt. Only its verified saved turn may be replaced.
        let replacing_previous =
            !runtime["userCommitReceipt"].is_null() && runtime["userCommitReceipt"] != value;
        if replacing_previous {
            let index = crate::browser_turns::index(before)?;
            let turns = crate::browser_turns::saved(before)?;
            ensure!(
                index > 0
                    && turns.len() == index
                    && before["dispatch"]["state"] != "acknowledged"
                    && turns[index - 1]["receipt"] == runtime["userCommitReceipt"]
                    && runtime["submittedPromptHash"]
                        == runtime["userCommitReceipt"]["messageFingerprint"]
                    && before["dispatch"]["history"][index - 1]["userMessageId"]
                        == runtime["userCommitReceipt"]["messageId"]
                    && runtime["userCommitReceipt"]["messageId"] != committed.message_id,
                "Existing commitment is not the verified preceding sequential turn"
            );
            crate::browser_turns::verify_turn(&self.store, before, &turns[index - 1], index - 1)?;
        }
        ensure!(
            runtime["ownedRecoveryTarget"]["conversationId"].is_null()
                || runtime["ownedRecoveryTarget"]["conversationId"] == ""
                || runtime["ownedRecoveryTarget"]["conversationId"] == *conversation,
            "Recovered conversation differs from the retained owned target"
        );
        for (old, new) in [
            (&runtime["conversationId"], &json!(conversation)),
            (
                &runtime["submittedPromptHashFormat"],
                &json!("original-source-v1"),
            ),
        ] {
            ensure!(
                old.is_null() || old == new,
                "An existing recovered commitment cannot be replaced"
            );
        }
        ensure!(
            replacing_previous
                || runtime["submittedPromptHash"].is_null()
                || runtime["submittedPromptHash"] == committed.message_fingerprint,
            "An existing recovered source fingerprint cannot be replaced"
        );
        let next = self.store.update(&self.link.child_id,|current| {
            self.require_session(&self.link.child_id,current)?;
            ensure!(current == before,"Attempt changed during original commitment inspection");
            let owned = merge_object(&current["browser"]["runtime"]["ownedRecoveryTarget"],
                &json!({"conversationId":conversation}));
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(
                &current["browser"]["runtime"],&json!({"userCommitReceipt":committed,"conversationId":conversation,
                    "tabUrl":tab_url,"submittedPromptHash":committed.message_fingerprint,
                    "submittedPromptHashFormat":"original-source-v1","ownedRecoveryTarget":owned}))})),
                "dispatch":merge_object(&current["dispatch"],&json!({"state":"acknowledged"}))})))
        })?;
        // The transition and its seal are synchronous; an error never grants a replacement seal.
        let (_, current) = self.require_base()?;
        ensure!(
            current == next,
            "Child changed during recovered commitment persistence"
        );
        self.sealed_receipt
            .set(receipt(&next))
            .map_err(|_| anyhow::anyhow!("Recovery commitment was already sealed"))?;
        self.require_session(&self.link.child_id, &next)?;
        Ok(next)
    }

    /// Persist the exact Grok commitment that the original live witness reported and the
    /// original renderer claim acknowledged. It never replaces an existing commitment.
    pub(crate) fn acknowledge_grok(&self, before: &Value, committed: &Value) -> Result<Value> {
        self.require_session(&self.link.child_id, before)?;
        ensure!(
            self.link.provider == Provider::Grok && self.sealed_receipt.get().is_none(),
            "Grok recovery commitment was already sealed or belongs to another provider"
        );
        let runtime = &before["browser"]["runtime"];
        let owner: crate::browser_commit::SubmissionOwner =
            serde_json::from_value(runtime["submissionOwner"].clone())?;
        let request = browser_runner::request(&self.store, &self.link.child_id)?;
        let mode = runtime["modelSelection"]["selectedMode"]
            .as_str()
            .context("Grok recovery mode is missing")?;
        let dispatch = before["dispatch"]["providerDispatchId"]
            .as_str()
            .context("Grok recovery dispatch is missing")?;
        crate::browser_grok::Intent::new(&request.prompt, mode, dispatch)?
            .require(committed, &owner)?;
        ensure!(
            before["dispatch"]["state"] == "started"
                && before["dispatch"]["submissionOwner"] == runtime["submissionOwner"]
                && before["dispatch"]["promptSha256"] == request.prompt_sha256
                && (runtime["grokCommitReceipt"].is_null()
                    || runtime["grokCommitReceipt"] == *committed)
                && runtime.get("conversationId").is_none(),
            "Recovered Grok commitment differs from its original dispatch"
        );
        let next = self.store.update(&self.link.child_id, |current| {
            self.require_session(&self.link.child_id, current)?;
            ensure!(
                current == before,
                "Attempt changed during original Grok commitment inspection"
            );
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(
                &current["browser"]["runtime"],&json!({"grokCommitReceipt":committed,
                    "submittedPromptHash":request.prompt_sha256,"submittedPromptHashFormat":"grok-request-v1"}))})),
                "dispatch":merge_object(&current["dispatch"],&json!({"state":"acknowledged"}))})))
        })?;
        let (_, current) = self.require_base()?;
        ensure!(
            current == next,
            "Child changed during recovered Grok commitment persistence"
        );
        self.sealed_receipt
            .set(receipt(&next))
            .map_err(|_| anyhow::anyhow!("Recovery commitment was already sealed"))?;
        self.require_session(&self.link.child_id, &next)?;
        Ok(next)
    }

    fn publish_remote(self: &Arc<Self>, parent: &mut Parent) -> Result<()> {
        let before = self.store.stored_for_update(&self.link.parent_id)?;
        self.require_session(&self.link.parent_id, &before)?;
        let run = self.initial_remote["runId"]
            .as_str()
            .context("Remote recovery run missing")?
            .to_owned();
        let images_required = before["options"]["imageOutputRequested"] == true;
        if before["browser"]["consultation"]["callbackIssued"] == false {
            self.adopting.store(true, Ordering::SeqCst);
            ensure!(
                !self.remote_publication_started.swap(true, Ordering::SeqCst),
                "Remote callback already reserved by this recovery"
            );
            let guard = self.clone();
            let callback_run = run.clone();
            let hook: browser_runner::CaptureHook = Arc::new(move |captured| {
                guard
                    .remote_publication_sealed
                    .store(true, Ordering::SeqCst);
                guard.require_session(
                    &guard.link.parent_id,
                    &guard.store.stored_for_update(&guard.link.parent_id)?,
                )?;
                let produced = crate::remote_protocol::produced_until(
                    &guard.store,
                    &callback_run,
                    captured,
                    images_required,
                    Some(&guard.cancel),
                    Some(guard.deadline),
                )?;
                guard.require_session(
                    &guard.link.parent_id,
                    &guard.store.stored_for_update(&guard.link.parent_id)?,
                )?;
                crate::remote_result::save_until(
                    &guard.store,
                    &callback_run,
                    &produced.captured,
                    &produced.result,
                    &produced.artifacts,
                    crate::remote_result::Phase::Capture,
                    &guard.cancel,
                    Some(guard.deadline),
                )?;
                Ok(())
            });
            let result = parent.issue_capture(&hook, &self.cancel);
            if self.store.stored_for_update(&self.link.parent_id)?["browser"]["consultation"]["callbackIssued"]
                == true
            {
                self.remote_publication_sealed.store(true, Ordering::SeqCst);
            }
            result?;
        }
        self.require_session(
            &self.link.parent_id,
            &self.store.stored_for_update(&self.link.parent_id)?,
        )?;
        let captured = adoption::captured_selected_until(
            &self.store,
            &self.link.parent_id,
            &self.cancel,
            Some(self.deadline),
        )?;
        let capture_path = self
            .store
            .directory(&self.link.parent_id)?
            .join("remote-capture.json");
        let record = match std::fs::symlink_metadata(&capture_path) {
            Ok(_) => crate::remote_result::read_phase(
                &self.store,
                &run,
                crate::remote_result::Phase::Capture,
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remote_record(&self.store, &run)?
            }
            Err(error) => return Err(error.into()),
        };
        crate::remote_protocol::verify_recovered(
            &self.store,
            &run,
            &record,
            &captured,
            images_required,
            &self.cancel,
            Some(self.deadline),
        )?;
        if record.native()["phase"] == "capture" {
            crate::remote_result::promote_capture(
                &self.store,
                &record,
                &self.cancel,
                Some(self.deadline),
            )?;
        }
        self.require_session(
            &self.link.parent_id,
            &self.store.stored_for_update(&self.link.parent_id)?,
        )?;
        Ok(())
    }

    fn restore(&self, id: &str) -> Result<()> {
        let snapshot = self.store.stored_for_update(id)?;
        let model = snapshot["model"]
            .as_str()
            .context("Recovery model missing")?;
        let run_snapshot = eligible(
            &self.store,
            &snapshot,
            self.partial_parent && id == self.link.parent_id,
        )?;
        self.store.update_session_and_model(id,model,|current,run| {
            self.require_session(id,current)?;
            ensure!(*current == snapshot && run == Some(&run_snapshot),"Recovery outcome changed during admission");
            if current["status"] == "running" && run.is_some_and(|run|run["status"] == "running") {return Ok(None);}
            Ok(Some((json!({"status":"running","completedAt":null,"errorMessage":null,"error":null}),
                Some(json!({"status":"running","completedAt":null,"errorMessage":null,"error":null})))))
        })?;
        Ok(())
    }
    fn record_failure(&self, error: &anyhow::Error) -> Result<()> {
        let (parent, child) = self.require_evidence()?;
        ensure!(
            receipt(&child) == *self.sealed_receipt.get().unwrap_or(&self.initial_receipt),
            "Recovery commitment changed before failure settlement"
        );
        // Failure reporting grants no publication or close authority. Preserve the child
        // for every started durable adoption, even when its files need later verification.
        // Do not restart unbounded validation after this observer has expired.
        let state = &parent["browser"]["consultation"];
        let adoption_durable = state["selected"].is_object()
            || matches!(
                state["adoption"]["phase"].as_str(),
                Some("reserved" | "captured")
            );
        for id in [&self.link.parent_id, &self.link.child_id] {
            let snapshot = self.store.stored_for_update(id)?;
            let model = snapshot["model"]
                .as_str()
                .context("Recovery model missing")?;
            self.store.update_session_and_model(id,model,|current,run| {
                self.require_evidence()?;
                ensure!(*current == snapshot,"Recovery changed before failure settlement");
                if adoption_durable && id == &self.link.child_id {
                    return Ok(None);
                }
                if current["status"] == "completed" {
                    return Ok(Some((json!({"cleanupWarning":error.to_string(),"recoveryRequired":true}),None)));
                }
                if !matches!(current["status"].as_str(),Some("running"|"error"))
                    || run.is_some_and(|run|matches!(run["status"].as_str(),Some("completed"|"partial"|"cancelled"))) {
                    return Ok(None);
                }
                let completed = crate::sessions::timestamp();
                Ok(Some((json!({"status":"error","completedAt":completed,"errorMessage":error.to_string(),"recoveryRequired":true}),
                    Some(json!({"status":"error","completedAt":completed,"errorMessage":error.to_string()})))))
            })?;
        }
        Ok(())
    }
}

impl Guard {
    pub(super) fn require_retirement(&self) -> Result<()> {
        let (parent, child) = self.require_base()?;
        remote_policy(&self.store, &parent, &self.cancel, self.deadline)?;
        ensure!(
            parent["status"] == "completed"
                && child["status"] == "completed"
                && child["dispatch"]["state"] == "completed"
                && receipt(&child) == *self.sealed_receipt.get().unwrap_or(&self.initial_receipt)
                && turn(&child) == *self.turn_seal(),
            "Recovery cannot close before its original child and durable parent complete"
        );
        adoption::require_selected_until(
            &self.store,
            &self.link.parent_id,
            &self.cancel,
            Some(self.deadline),
        )?;
        ensure!(
            !self.cancel.is_cancelled() && Instant::now() < self.deadline,
            "Recovery close authority expired or was cancelled"
        );
        Ok(())
    }
}
impl Parent {
    fn resume_recovery(
        store: &SessionStore,
        id: &str,
        guard: Arc<Guard>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let parent = store.stored_for_update(id)?;
        let result = Self {
            store: store.clone(),
            id: id.into(),
            witness: Arc::new(ExecutionWitness::capture_recovery(store, id, guard)?),
            request: browser_runner::request(store, id)?,
            state: parent["browser"]["consultation"].clone(),
            controller: controller_identity(&parent),
            directory: NativeEntry::open(&store.directory(id)?, true)?.identity,
        };
        result.require(&parent, cancel)?;
        Ok(result)
    }
}

/// A child that failed before it started sending has no answer to capture. Refuse before recovery
/// takes ownership, so the consultation keeps the child's own error instead of a capture failure.
fn require_dispatched_child(store: &SessionStore, parent: &str) -> Result<()> {
    let active = store.stored_for_update(parent)?["browser"]["consultation"]["active"].clone();
    let Ok(link) = serde_json::from_value::<Link>(active) else {
        return Ok(());
    };
    let child = store.stored_for_update(&link.child_id)?;
    ensure!(
        child["status"] != "error" || child["dispatch"]["state"] != "not-started",
        "Nothing to recover: provider attempt {} failed before sending its prompt ({}); nothing was sent",
        link.child_id,
        child["errorMessage"]
            .as_str()
            .unwrap_or("no error recorded")
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // The caller supplies only the existing pinned browser and admission registry.
pub(crate) async fn recover(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Value> {
    require_dispatched_child(store, id)?;
    let guard = Guard::acquire(store, id, limits, cancel).await?;
    let result = tokio::time::timeout_at(tokio::time::Instant::from_std(guard.deadline), async {
        if store.stored_for_update(id)?["status"] != "completed" {
            guard.restore(id)?;
            guard.restore(&guard.link.child_id)?;
            guard.admitted.store(true, Ordering::SeqCst);
            let mut parent = Parent::resume_recovery(store, id, guard.clone(), cancel)?;
            if store.stored_for_update(id)?["browser"]["consultation"]["selected"].is_object() {
                adoption::require_selected_until(store, id, cancel, Some(guard.deadline))?;
            } else if store.stored_for_update(id)?["browser"]["consultation"]["adoption"]["phase"]
                == "captured"
            {
                let witness =
                    ExecutionWitness::capture_recovery(store, &guard.link.child_id, guard.clone())?;
                guard.adopting.store(true, Ordering::SeqCst);
                parent.select_captured_with_witness(
                    &guard.link.child_id,
                    &witness,
                    true,
                    cancel,
                )?;
            } else if store.stored_for_update(id)?["browser"]["consultation"]["adoption"]["phase"]
                == "reserved"
            {
                let witness =
                    ExecutionWitness::capture_recovery(store, &guard.link.child_id, guard.clone())?;
                guard.adopting.store(true, Ordering::SeqCst);
                parent.resume_reserved_with_witness(&guard.link.child_id, &witness, cancel)?;
            } else {
                browser_runner::recover_child_capture(
                    store,
                    &guard.link.child_id,
                    profile,
                    browser.clone(),
                    endpoint.clone(),
                    limits,
                    cancel,
                    guard.clone(),
                )
                .await?;
                let witness =
                    ExecutionWitness::capture_recovery(store, &guard.link.child_id, guard.clone())?;
                guard.adopting.store(true, Ordering::SeqCst);
                parent.adopt_with_witness(&guard.link.child_id, &witness, cancel)?;
            }
            if !guard.initial_remote.is_null() {
                guard.publish_remote(&mut parent)?;
                return store.require(id);
            }
            guard.publishing.store(true, Ordering::SeqCst);
            parent.complete(cancel)?;
        }
        guard.require_session(id, &store.stored_for_update(id)?)?;
        // Retirement rechecks the original creator and durable parent outputs. The observer
        // never acquires submission authority or changes the original controller identity.
        completion::settle_recovered(store, id, &browser, &endpoint, cancel, &guard).await?;
        store.require(id)
    })
    .await
    .context("Public parent recovery deadline reached; original attempt retained")
    .and_then(|result| result);
    if let Err(error) = &result
        && let Err(settlement) = guard.record_failure(error)
    {
        return Err(anyhow::anyhow!(
            "{error:#}; recovery failure settlement retained: {settlement:#}"
        ));
    }
    if result.is_ok() && store.stored_for_update(id)?["status"] == "completed" {
        browser_runner::settle_saved_fresh(store, &guard.link.child_id).await;
        browser_runner::settle_saved_idle(store, &guard.link.child_id).await;
        browser_runner::settle_hinted_idle(store, &guard.link.child_id, true).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, SessionStore, Value) {
        let (root, manifest) = crate::browser_runner::tests::dead_provider_attempt();
        let store = SessionStore::new(
            std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
        );
        (root, store, manifest)
    }
    #[test]
    fn unsent_child_keeps_its_own_error_and_recovery_takes_no_ownership() {
        let (_root, store, manifest) = fixture();
        let parent = manifest["parentId"].as_str().unwrap();
        let child = manifest["childId"].as_str().unwrap();
        require_dispatched_child(&store, parent).unwrap();
        // Still preparing: ordinary recovery decides.
        store
            .update(child, |current| {
                Ok(Some(json!({"status":"running","dispatch":merge_object(&current["dispatch"],&json!({"state":"not-started"}))})))
            })
            .unwrap();
        require_dispatched_child(&store, parent).unwrap();
        store
            .update(child, |_| {
                Ok(Some(
                    json!({"status":"error","errorMessage":"PUBLIC prompt check failed"}),
                ))
            })
            .unwrap();
        let before = store.stored_for_update(parent).unwrap();
        let error = require_dispatched_child(&store, parent)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "failed before sending its prompt (PUBLIC prompt check failed); nothing was sent"
            ),
            "{error}"
        );
        assert_eq!(store.stored_for_update(parent).unwrap(), before);
    }
    #[tokio::test]
    async fn remote_callback_without_record_and_unacknowledged_partial_parent_never_resume() {
        for kind in [
            "remote-callback-missing",
            "remote-partial",
            "remote-partial-result",
            "selected",
        ] {
            let (_root, manifest) = if kind == "selected" {
                crate::browser_runner::tests::selected_parent_attempt()
            } else {
                crate::browser_runner::tests::remote_parent_attempt(kind)
            };
            let store = SessionStore::new(
                std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
            );
            let parent = manifest["parentId"].as_str().unwrap();
            let child = manifest["childId"].as_str().unwrap();
            if kind == "remote-partial" {
                std::fs::write(
                    store
                        .directory(parent)
                        .unwrap()
                        .join("remote-capture-ack.json"),
                    b"PUBLIC changed ACK",
                )
                .unwrap();
            } else if kind == "remote-partial-result" {
                let run = "12345678-1234-1234-1234-123456789abc";
                let mut capture = crate::remote_result::read_phase(
                    &store,
                    run,
                    crate::remote_result::Phase::Capture,
                )
                .unwrap()
                .native()
                .clone();
                capture["capturedResult"]["answerText"] =
                    json!("PUBLIC unrelated acknowledged capture");
                crate::storage::atomic_json(
                    &store.directory(parent).unwrap().join("remote-capture.json"),
                    &capture,
                )
                .unwrap();
            } else if kind == "selected" {
                store
                    .update(parent, |_| Ok(Some(json!({"status":"partial"}))))
                    .unwrap();
                let model = store.require(parent).unwrap()["model"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                store
                    .update_model(parent, &model, &json!({"status":"partial"}))
                    .unwrap();
            }
            let before = store.stored_for_update(parent).unwrap();
            let before_child = store.stored_for_update(child).unwrap();
            let runs = store.model_runs(parent).unwrap();
            let final_path = store.directory(parent).unwrap().join("remote-result.json");
            let final_before = std::fs::read(&final_path).ok();
            assert!(
                Guard::acquire(
                    &store,
                    parent,
                    &RunnerLimits::default(),
                    &CancellationToken::new()
                )
                .await
                .is_err(),
                "{kind}"
            );
            assert_eq!(store.stored_for_update(parent).unwrap(), before);
            assert_eq!(store.stored_for_update(child).unwrap(), before_child);
            assert_eq!(store.model_runs(parent).unwrap(), runs);
            assert_eq!(std::fs::read(&final_path).ok(), final_before);
        }
    }
    #[tokio::test]
    async fn invalid_capture_entry_cannot_be_bypassed_by_a_valid_final_record() {
        #[cfg(unix)]
        let kinds = ["directory", "dangling-link"].as_slice();
        #[cfg(not(unix))]
        let kinds = ["directory"].as_slice();
        for kind in kinds {
            let (_root, manifest) =
                crate::browser_runner::tests::remote_parent_attempt("remote-result");
            let store = SessionStore::new(
                std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
            );
            let parent = manifest["parentId"].as_str().unwrap();
            let child = manifest["childId"].as_str().unwrap();
            let cancel = CancellationToken::new();
            let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
                .await
                .unwrap();
            guard.restore(parent).unwrap();
            guard.restore(child).unwrap();
            guard.admitted.store(true, Ordering::SeqCst);
            let mut resumed =
                Parent::resume_recovery(&store, parent, guard.clone(), &cancel).unwrap();
            let directory = store.directory(parent).unwrap();
            let capture = directory.join("remote-capture.json");
            let final_path = directory.join("remote-result.json");
            let final_before = std::fs::read(&final_path).unwrap();
            let final_identity = NativeEntry::open(&final_path, false).unwrap().identity;
            std::fs::remove_file(&capture).unwrap();
            if *kind == "directory" {
                std::fs::create_dir(&capture).unwrap();
            } else {
                #[cfg(unix)]
                std::os::unix::fs::symlink(directory.join("PUBLIC-missing-capture"), &capture)
                    .unwrap();
            }
            assert!(guard.publish_remote(&mut resumed).is_err(), "{kind}");
            assert_eq!(std::fs::read(&final_path).unwrap(), final_before);
            assert_eq!(
                NativeEntry::open(&final_path, false).unwrap().identity,
                final_identity
            );
            assert_ne!(store.require(parent).unwrap()["status"], "completed");
            assert_ne!(store.require(child).unwrap()["status"], "completed");
        }
    }
    #[tokio::test]
    async fn remote_callback_reservation_cannot_roll_back_while_publication_is_active() {
        let (_root, manifest) =
            crate::browser_runner::tests::remote_parent_attempt("remote-selected");
        let store = SessionStore::new(
            std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
        );
        let parent = manifest["parentId"].as_str().unwrap();
        let child = manifest["childId"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
            .await
            .unwrap();
        guard.restore(parent).unwrap();
        guard.restore(child).unwrap();
        guard.admitted.store(true, Ordering::SeqCst);
        guard.adopting.store(true, Ordering::SeqCst);
        guard
            .remote_publication_started
            .store(true, Ordering::SeqCst);
        let mut resumed = Parent::resume_recovery(&store, parent, guard.clone(), &cancel).unwrap();
        let callback_guard = guard.clone();
        let hook: browser_runner::CaptureHook = Arc::new(move |_| {
            callback_guard
                .remote_publication_sealed
                .store(true, Ordering::SeqCst);
            let id = &callback_guard.link.parent_id;
            callback_guard.require_session(id, &callback_guard.store.stored_for_update(id)?)?;
            let mut rolled_back = callback_guard.store.stored_for_update(id)?;
            rolled_back["browser"]["consultation"]["callbackIssued"] = json!(false);
            rolled_back["browser"]["remoteDelivery"] = callback_guard.initial_remote.clone();
            crate::storage::atomic_json(
                &callback_guard.store.directory(id)?.join("meta.json"),
                &rolled_back,
            )?;
            callback_guard.require_session(id, &rolled_back)
        });
        assert!(resumed.issue_capture(&hook, &cancel).is_err());
        assert!(
            guard
                .require_session(parent, &store.stored_for_update(parent).unwrap())
                .is_err()
        );
        for leaf in ["remote-capture.json", "remote-result.json"] {
            assert!(
                !store.directory(parent).unwrap().join(leaf).exists(),
                "{leaf}"
            );
        }
    }
    #[tokio::test]
    async fn remote_delivery_and_callback_mutations_revoke_original_observer_authority() {
        let (_root, manifest) =
            crate::browser_runner::tests::remote_parent_attempt("remote-capture");
        let store = SessionStore::new(
            std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
        );
        let parent = manifest["parentId"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
            .await
            .unwrap();
        let original = store.stored_for_update(parent).unwrap();
        for pointer in [
            "/browser/remoteDelivery/runId",
            "/browser/remoteDelivery/acknowledged",
            "/browser/remoteDelivery/producerFinished",
            "/browser/remoteDelivery/captureIssued",
            "/browser/consultation/callbackIssued",
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).unwrap() = if pointer.ends_with("runId") {
                json!("PUBLIC-changed-run")
            } else {
                json!(changed.pointer(pointer) != Some(&json!(true)))
            };
            crate::storage::atomic_json(
                &store.directory(parent).unwrap().join("meta.json"),
                &changed,
            )
            .unwrap();
            assert!(
                guard.require_session(parent, &changed).is_err(),
                "{pointer}"
            );
            crate::storage::atomic_json(
                &store.directory(parent).unwrap().join("meta.json"),
                &original,
            )
            .unwrap();
        }
        guard.require_session(parent, &original).unwrap();
    }
    #[tokio::test]
    async fn guard_reads_under_metadata_transactions_and_never_grants_worker_authority() {
        let (_root, store, manifest) = fixture();
        let parent = manifest["parentId"].as_str().unwrap();
        let child = manifest["childId"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
            .await
            .unwrap();
        guard.restore(parent).unwrap();
        guard.restore(child).unwrap();
        for id in [parent, child] {
            let witness = ExecutionWitness::capture_recovery(&store, id, guard.clone()).unwrap();
            store
                .update(id, |current| {
                    witness.require(current)?;
                    ensure!(
                        witness.require_worker(current).is_err(),
                        "Recovery gained worker authority"
                    );
                    Ok(None)
                })
                .unwrap();
            let dead = serde_json::from_value(manifest["identity"].clone()).unwrap();
            assert!(ExecutionWitness::capture(&store, id, &dead).is_err());
            assert!(
                Parent::resume_recovery(&store, parent, guard.clone(), &cancel)
                    .unwrap()
                    .reserve_first(&cancel)
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn guard_rejects_changed_supplied_and_persisted_source_receipt_and_owner() {
        let (_root, store, manifest) = fixture();
        let parent = manifest["parentId"].as_str().unwrap();
        let child = manifest["childId"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
            .await
            .unwrap();
        let original = store.stored_for_update(child).unwrap();
        for pointer in [
            "/browser/runtime/commitSnapshot/canonicalPrompt",
            "/browser/runtime/submissionOwner/claimId",
            "/browser/runtime/submittedPromptHash",
            "/browser/runtime/userCommitReceipt",
            "/browser/runtime/ownedRecoveryTarget/conversationId",
            "/dispatch/turnCapture",
            "/dispatch/history",
            "/dispatch/state",
        ] {
            let mut changed = original.clone();
            let mut pieces = pointer.trim_start_matches('/').split('/').peekable();
            let mut destination = &mut changed;
            while let Some(key) = pieces.next() {
                if pieces.peek().is_none() {
                    destination[key] = json!("PUBLIC changed");
                    break;
                }
                destination = &mut destination[key];
            }
            assert!(
                guard.require_session(child, &changed).is_err(),
                "supplied {pointer}"
            );
            store.update(child, |_| Ok(Some(changed.clone()))).unwrap();
            assert!(
                guard.require_session(child, &changed).is_err(),
                "persisted {pointer}"
            );
            crate::storage::atomic_json(
                &store.directory(child).unwrap().join("meta.json"),
                &original,
            )
            .unwrap();
        }
        guard.require_session(child, &original).unwrap();
    }
    #[tokio::test]
    async fn observation_authority_ends_if_parent_delivery_or_adoption_policy_changes() {
        let (_root, store, manifest) = fixture();
        let parent = manifest["parentId"].as_str().unwrap();
        let child = manifest["childId"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
            .await
            .unwrap();
        guard.restore(parent).unwrap();
        guard.restore(child).unwrap();
        guard.admitted.store(true, Ordering::SeqCst);
        let original = store.stored_for_update(parent).unwrap();
        let witness = ExecutionWitness::capture_recovery(&store, child, guard.clone()).unwrap();
        for pointer in [
            "/browser/remoteDelivery",
            "/browser/consultation/selected",
            "/browser/consultation/adoption",
            "/browser/consultation/callbackIssued",
        ] {
            let mut changed = original.clone();
            let (container, field) = pointer.rsplit_once('/').unwrap();
            changed.pointer_mut(container).unwrap()[field] = json!(true);
            store.update(parent, |_| Ok(Some(changed.clone()))).unwrap();
            assert!(
                witness
                    .require(&store.stored_for_update(child).unwrap())
                    .is_err(),
                "{pointer}"
            );
            crate::storage::atomic_json(
                &store.directory(parent).unwrap().join("meta.json"),
                &original,
            )
            .unwrap();
        }
        witness
            .require(&store.stored_for_update(child).unwrap())
            .unwrap();
    }
    #[tokio::test]
    async fn guard_cancel_and_deadline_preserve_original_evidence_and_recoverable_status() {
        for cancelled in [true, false] {
            let (_root, store, manifest) = fixture();
            let parent = manifest["parentId"].as_str().unwrap();
            let child = manifest["childId"].as_str().unwrap();
            let original = store.stored_for_update(child).unwrap();
            let cancel = CancellationToken::new();
            let mut guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
                .await
                .unwrap();
            guard.restore(parent).unwrap();
            guard.restore(child).unwrap();
            if cancelled {
                cancel.cancel();
            } else {
                Arc::get_mut(&mut guard).unwrap().deadline = Instant::now();
            }
            assert!(
                guard
                    .require_session(child, &store.stored_for_update(child).unwrap())
                    .is_err()
            );
            guard
                .record_failure(&anyhow::anyhow!("PUBLIC stopped observer"))
                .unwrap();
            for id in [parent, child] {
                let current = store.stored_for_update(id).unwrap();
                assert_eq!(current["status"], "error");
                assert_eq!(current["recoveryRequired"], true);
                assert_eq!(
                    current["lifecycle"]["processIdentity"],
                    manifest["identity"]
                );
            }
            let retained = store.stored_for_update(child).unwrap();
            assert_eq!(source(&retained), source(&original));
            assert_eq!(receipt(&retained), receipt(&original));
        }
    }
    #[tokio::test]
    async fn failed_settlement_preserves_the_running_selected_child_after_parent_completion() {
        for phase in ["selected", "completed", "captured", "reserved"] {
            let completed = phase == "completed";
            for expired in [false, true] {
                let (_root, manifest) = if phase == "reserved" {
                    crate::browser_runner::tests::reserved_parent_attempt()
                } else if phase == "captured" {
                    crate::browser_runner::tests::captured_parent_attempt()
                } else if completed {
                    crate::browser_runner::tests::completed_parent_attempt()
                } else {
                    crate::browser_runner::tests::selected_parent_attempt()
                };
                let store = SessionStore::new(
                    std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
                );
                let parent = manifest["parentId"].as_str().unwrap();
                let child = manifest["childId"].as_str().unwrap();
                let before = store.stored_for_update(child).unwrap();
                let runs = store.model_runs(child).unwrap();
                assert_eq!(before["status"], "running");
                assert_eq!(
                    store.require(parent).unwrap()["status"],
                    if completed { "completed" } else { "running" }
                );
                let cancel = CancellationToken::new();
                let mut guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
                    .await
                    .unwrap();
                if expired {
                    Arc::get_mut(&mut guard).unwrap().deadline = Instant::now();
                } else {
                    cancel.cancel();
                }
                guard
                    .record_failure(&anyhow::anyhow!(
                        "PUBLIC interrupted before child completion"
                    ))
                    .unwrap();
                assert_eq!(store.stored_for_update(child).unwrap()["status"], "running");
                assert_eq!(store.stored_for_update(child).unwrap(), before);
                assert_eq!(store.model_runs(child).unwrap(), runs);
                assert_eq!(store.require(parent).unwrap()["recoveryRequired"], true);
                drop(guard);
                let retry_cancel = CancellationToken::new();
                let retry = Guard::acquire(&store, parent, &RunnerLimits::default(), &retry_cancel)
                    .await
                    .unwrap();
                if !completed {
                    retry.restore(parent).unwrap();
                    retry.restore(child).unwrap();
                    retry.admitted.store(true, Ordering::SeqCst);
                    retry.publishing.store(true, Ordering::SeqCst);
                    let mut resumed =
                        Parent::resume_recovery(&store, parent, retry.clone(), &retry_cancel)
                            .unwrap();
                    if phase == "reserved" {
                        let witness =
                            ExecutionWitness::capture_recovery(&store, child, retry.clone())
                                .unwrap();
                        retry.adopting.store(true, Ordering::SeqCst);
                        resumed
                            .resume_reserved_with_witness(child, &witness, &retry_cancel)
                            .unwrap();
                    } else if phase == "captured" {
                        let witness =
                            ExecutionWitness::capture_recovery(&store, child, retry.clone())
                                .unwrap();
                        retry.adopting.store(true, Ordering::SeqCst);
                        resumed
                            .select_captured_with_witness(child, &witness, true, &retry_cancel)
                            .unwrap();
                    }
                    resumed.complete(&retry_cancel).unwrap();
                }
                retry
                    .require_session(parent, &store.require(parent).unwrap())
                    .unwrap();
                completion::complete_selected(&store, parent, &retry_cancel).unwrap();
                retry.require_retirement().unwrap();
                assert_eq!(store.require(child).unwrap()["status"], "completed");
            }
        }
    }
    #[tokio::test]
    async fn selected_resume_revalidates_delivery_policy_outputs_and_child_evidence_before_restore()
    {
        for phase in ["selected", "captured", "reserved"] {
            let captured = phase == "captured";
            let reserved = phase == "reserved";
            for case in [
                "callback",
                "remote",
                "output",
                "child-evidence",
                "parent-model",
            ] {
                if reserved && case == "parent-model" {
                    continue;
                }
                let (_root, manifest) = if reserved {
                    crate::browser_runner::tests::reserved_parent_attempt()
                } else if captured {
                    crate::browser_runner::tests::captured_parent_attempt()
                } else {
                    crate::browser_runner::tests::selected_parent_attempt()
                };
                let store = SessionStore::new(
                    std::path::Path::new(manifest["home"].as_str().unwrap()).join(".oracle"),
                );
                let parent = manifest["parentId"].as_str().unwrap();
                let child = manifest["childId"].as_str().unwrap();
                let cancel = CancellationToken::new();
                let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
                    .await
                    .unwrap();
                let saved = store.require(parent).unwrap();
                match case {
                    "callback" => {
                        store.update(parent, |meta| {
                    let mut state = meta["browser"]["consultation"].clone();
                    state["callbackIssued"] = json!(true);
                    Ok(Some(json!({"browser":merge_object(&meta["browser"], &json!({"consultation":state}))})))
                }).unwrap();
                    }
                    "remote" => {
                        store.update(parent, |meta| Ok(Some(json!({"browser":merge_object(&meta["browser"], &json!({"remoteDelivery":{"runId":"PUBLIC-changed"}}))})))).unwrap();
                    }
                    "output" => {
                        let path = if reserved {
                            let original = store.require(child).unwrap();
                            store.directory(child).unwrap().join(
                                original["browser"]["capturedAnswer"]["file"]
                                    .as_str()
                                    .unwrap(),
                            )
                        } else if captured {
                            store
                                .directory(parent)
                                .unwrap()
                                .join(saved["browser"]["capturedAnswer"]["file"].as_str().unwrap())
                        } else {
                            std::path::PathBuf::from(
                                saved["browser"]["requiredTextOutput"]["file"]
                                    .as_str()
                                    .unwrap(),
                            )
                        };
                        std::fs::write(path, "PUBLIC changed output").unwrap();
                    }
                    "child-evidence" => {
                        store.update(child, |meta| {
                    let mut observation = meta["browser"]["runtime"]["acceptedObservation"].clone();
                    observation["html"] = json!("<p>PUBLIC changed evidence</p>");
                    Ok(Some(json!({"browser":merge_object(&meta["browser"], &json!({"runtime":merge_object(&meta["browser"]["runtime"], &json!({"acceptedObservation":observation}))}))})))
                }).unwrap();
                    }
                    "parent-model" => {
                        store.update(parent, |meta|Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"effectiveModel":"PUBLIC-changed"}))})))).unwrap();
                    }
                    _ => unreachable!(),
                }
                let before_parent = store.stored_for_update(parent).unwrap();
                let before_child = store.stored_for_update(child).unwrap();
                let parent_runs = store.model_runs(parent).unwrap();
                let child_runs = store.model_runs(child).unwrap();
                assert!(guard.restore(parent).is_err(), "{case}");
                assert_eq!(
                    store.stored_for_update(parent).unwrap(),
                    before_parent,
                    "{case}"
                );
                assert_eq!(
                    store.stored_for_update(child).unwrap(),
                    before_child,
                    "{case}"
                );
                assert_eq!(store.model_runs(parent).unwrap(), parent_runs, "{case}");
                assert_eq!(store.model_runs(child).unwrap(), child_runs, "{case}");
                drop(guard);
                assert!(
                    Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
                        .await
                        .is_err(),
                    "{case}"
                );
            }
        }
    }
    #[tokio::test]
    async fn public_parent_terminal_change_revokes_child_capture_authority() {
        let (_root, store, manifest) = fixture();
        let parent = manifest["parentId"].as_str().unwrap();
        let child = manifest["childId"].as_str().unwrap();
        let cancel = CancellationToken::new();
        let guard = Guard::acquire(&store, parent, &RunnerLimits::default(), &cancel)
            .await
            .unwrap();
        guard.restore(parent).unwrap();
        guard.restore(child).unwrap();
        guard.admitted.store(true, Ordering::SeqCst);
        let original = store.stored_for_update(parent).unwrap();
        let witness = ExecutionWitness::capture_recovery(&store, child, guard.clone()).unwrap();
        for status in ["cancelled", "partial", "completed", "error"] {
            store
                .update(parent, |_| Ok(Some(json!({"status":status}))))
                .unwrap();
            assert!(
                witness
                    .require(&store.stored_for_update(child).unwrap())
                    .is_err(),
                "{status}"
            );
            crate::storage::atomic_json(
                &store.directory(parent).unwrap().join("meta.json"),
                &original,
            )
            .unwrap();
        }
        witness
            .require(&store.stored_for_update(child).unwrap())
            .unwrap();
        store
            .update_model(
                parent,
                original["model"].as_str().unwrap(),
                &json!({"status":"cancelled"}),
            )
            .unwrap();
        assert!(
            witness
                .require(&store.stored_for_update(child).unwrap())
                .is_err()
        );
    }
    #[tokio::test]
    async fn replacing_a_recovery_lock_owner_revokes_authority_and_preserves_replacement() {
        let (_root, store, manifest) = fixture();
        let parent = manifest["parentId"].as_str().unwrap();
        let guard = Guard::acquire(
            &store,
            parent,
            &RunnerLimits::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let path = store
            .directory(parent)
            .unwrap()
            .join("native-browser-recovery.lock/owner.json");
        let mut owner: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        owner["id"] = json!("PUBLIC replacement owner");
        std::fs::write(&path, serde_json::to_vec(&owner).unwrap()).unwrap();
        assert!(
            guard
                .require_session(parent, &store.stored_for_update(parent).unwrap())
                .is_err()
        );
        drop(guard);
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            owner
        );
    }
}

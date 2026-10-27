//! Observe an original provider attempt without giving the observer submission authority.
use super::*;

#[allow(clippy::too_many_arguments)] // The original target, request and commitment remain independent witnesses.
pub(crate) async fn recover_child_capture(
    store: &SessionStore,
    id: &str,
    profile: Arc<ProfileRegistry>,
    browser: Cdp,
    endpoint: AttachmentEndpoint,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    guard: Arc<crate::browser_attempt::RecoveryGuard>,
) -> Result<Value> {
    let saved = store.stored_for_update(id)?;
    guard.require_session(id, &saved)?;
    crate::browser_attempt::require_child(store, id, cancel)?;
    let witness = ExecutionWitness::capture_recovery(store, id, guard.clone())?;
    let committed = request(store, id)?;
    let runtime = &saved["browser"]["runtime"];
    let followups = crate::browser_turns::prompts(&saved["options"])?;
    let turn_index = crate::browser_turns::index(&saved)?;
    anyhow::ensure!(
        followups.is_empty() || turn_index == followups.len(),
        "Recovery cannot send remaining sequential prompts; original attempt retained"
    );
    let prompt = if turn_index == 0 {
        committed.prompt.as_str()
    } else {
        followups
            .get(turn_index - 1)
            .context("Recovery turn is outside the committed request")?
    };
    let creator = &runtime["ownedRecoveryTarget"]["creator"];
    let journal = TargetJournal::open(store)?;
    let record = journal.read(&creator["allocationId"])?;
    anyhow::ensure!(
        creator.is_object()
            && record["creator"] == *creator
            && record["commit"].is_null()
            && record["disposition"]["decision"] != "retired"
            && record["disposition"]["absence"] != "confirmed-absent",
        "Capture recovery requires the original retained uncommitted target"
    );
    let target_id = runtime["chromeTargetId"]
        .as_str()
        .context("Recovery target missing")?;
    anyhow::ensure!(
        runtime["browserWSEndpoint"] == endpoint.browser_ws_endpoint
            && runtime["chromePort"].as_u64() == Some(u64::from(endpoint.port))
            && runtime["chromeHost"].as_str().is_some_and(
                |host| browser::normalise_host(host) == browser::normalise_host(&endpoint.host)
            ),
        "Recovery endpoint differs from the retained browser instance"
    );
    let lease = profile.recovery_lease(
        runtime["tabLeaseId"]
            .as_str()
            .context("Recovery lease missing")?,
        id,
        &endpoint.host,
        endpoint.port,
        target_id,
    )?;
    let target =
        OwnedTarget::recover(browser, lease, target_id.into(), limits.command, cancel).await?;
    let info = target.require(limits.command, cancel).await?;
    if saved["browser"]["attempt"]["provider"] == "grok" {
        super::grok::recover(
            store, id, &witness, &saved, &committed, &target, &info, &guard, limits, cancel,
        )
        .await?;
        witness.require(&store.stored_for_update(id)?)?;
        return store.require(id);
    }
    let conversation = info["url"]
        .as_str()
        .and_then(browser::conversation_id)
        .context("Original recovery target has no provider conversation")?;
    let owner = serde_json::from_value(runtime["submissionOwner"].clone())?;
    let reservation = RendererReservation::recover(
        target.page().clone(),
        owner,
        &conversation,
        false,
        limits.command,
        cancel,
    )
    .await?;
    let snapshot = serde_json::from_value(runtime["commitSnapshot"].clone())?;
    let receipt = wait_receipt(&reservation, &snapshot, prompt, limits, cancel, |_| Ok(())).await?;
    anyhow::ensure!(
        receipt.conversation.conversation_id == conversation,
        "Recovered source belongs to a different conversation"
    );
    let selection = json!({"modelSelection":runtime["modelSelection"],
        "thinkingSelection":runtime["thinkingSelection"],"effectiveModel":runtime["effectiveModel"]});
    policy::assert_browser_completion(
        Some(&serde_json::from_value(
            selection["modelSelection"].clone(),
        )?),
        Some(&serde_json::from_value(
            selection["thinkingSelection"].clone(),
        )?),
        selection["effectiveModel"].as_str(),
    )?;
    let acknowledged = guard.acknowledge(&saved, &receipt, &info["url"])?;
    let mut outcome = OutcomeWitness::capture(
        &acknowledged,
        &target,
        &endpoint,
        &receipt,
        &committed.options_sha256,
        TargetAuthority::Created(Some(creator)),
    )?;
    capture_recovered_answer(
        store,
        id,
        &witness,
        &mut outcome,
        &saved,
        &target,
        &endpoint,
        &reservation,
        &receipt,
        &selection,
        limits,
        cancel,
    )
    .await?;
    witness.require(&store.stored_for_update(id)?)?;
    store.require(id)
}

#[allow(clippy::too_many_arguments)] // Both recovery routes use this capture boundary and keep their own publication policy.
pub(super) async fn capture_recovered_answer(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    outcome: &mut OutcomeWitness,
    saved: &Value,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    reservation: &RendererReservation,
    receipt: &CommitReceipt,
    selection: &Value,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<(CaptureReceipt, String)> {
    let observed = wait_answer(
        store,
        id,
        reservation,
        receipt,
        selection,
        limits,
        true,
        Some(witness),
        cancel,
    )
    .await?;
    outcome.require(&store.require(id)?, witness, None, cancel)?;
    let reused = saved["browser"]["requiredTextOutput"].is_object()
        || saved["browser"]["runtime"]["answerSourceCapture"].is_object();
    let capture = if reused {
        reuse_published_capture(store, id, witness, outcome, saved, &observed, cancel)?
    } else {
        let text = if crate::browser_turns::prompts(&saved["options"])?.is_empty() {
            observed["text"]
                .as_str()
                .context("Recovered answer text is unavailable")?
                .to_owned()
        } else {
            let current = store.require(id)?;
            let index = crate::browser_turns::index(&current)?;
            let markdown = if let Some(row) = crate::browser_turns::saved(&current)?.get(index) {
                anyhow::ensure!(
                    row["assistantMessageId"] == observed["lastAssistantMessageId"]
                        && row["assistantTurnId"] == observed["lastAssistantTurnId"]
                        && row["report"] == observed["report"]
                        && row["sourceSha256"]
                            == sha256(
                                observed["text"]
                                    .as_str()
                                    .context("Recovered turn has no source text")?
                                    .as_bytes()
                            ),
                    "Saved completed turn differs from the recovered response"
                );
                crate::browser_turns::verify_turn(store, &current, row, index)?
            } else {
                observed["text"]
                    .as_str()
                    .context("Recovered turn has no source text")?
                    .to_owned()
            };
            remember_turn(
                store, id, witness, outcome, receipt, &observed, &markdown, cancel,
            )?;
            crate::browser_turns::saved_transcript(store, &store.require(id)?)?
        };
        capture_outcome(store, id, witness, outcome, &text, Some(&observed), cancel)?
    };
    collect_deliveries(
        store,
        id,
        witness,
        outcome,
        target,
        endpoint,
        reservation,
        &observed,
        limits,
        true,
        cancel,
    )
    .await?;
    if reused && saved["browser"]["requiredTextOutput"].is_object() {
        let text = String::from_utf8(artifacts::verify_capture(store, &capture)?)?;
        Ok((capture, text))
    } else {
        capture_image_summary(store, id, witness, outcome, &capture, cancel)
    }
}

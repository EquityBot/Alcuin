//! Grok's owned Private text attempt uses the common allocation, capture and
//! parent acknowledgement lifecycle, with its own provider evidence throughout.
use super::*;

#[cfg(test)]
#[path = "grok_tests.rs"]
pub(crate) mod tests;

pub(super) async fn navigate(
    page: &Cdp,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let deadline = Instant::now() + limits.command;
    let navigated = page
        .call(
            "Page.navigate",
            json!({"url":"https://grok.com/"}),
            left(deadline)?,
            cancel,
        )
        .await?;
    anyhow::ensure!(
        navigated.get("errorText").is_none(),
        "Grok initial navigation failed"
    );
    loop {
        let arrived = page.evaluate(
            "window===window.top&&location.origin==='https://grok.com'&&location.pathname==='/'&&document.readyState!=='loading'",
            left(deadline)?, cancel,
        ).await?;
        if arrived == true {
            return Ok(());
        }
        pause(limits.poll.min(left(deadline)?), cancel).await?;
    }
}

fn left(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "Original Grok observation deadline reached; preserve without replay"
    );
    Ok(remaining)
}

#[allow(clippy::too_many_arguments)] // The provider shares the exact native allocation witnesses.
fn require_original(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    creator: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let link = crate::browser_attempt::require_child(store, id, cancel)?;
    let current = store.stored_for_update(id)?;
    witness.require_worker(&current)?;
    let runtime = &current["browser"]["runtime"];
    let owned = &runtime["ownedRecoveryTarget"];
    anyhow::ensure!(
        !cancel.is_cancelled()
            && link.provider == crate::browser_attempt::Provider::Grok
            && current["status"] == "running"
            && serde_json::to_value(request(store, id)?)? == serde_json::to_value(committed)?
            && runtime["chromeTargetId"] == target.id()
            && runtime["tabLeaseId"] == target.lease().id()
            && runtime["browserWSEndpoint"] == endpoint.browser_ws_endpoint
            && runtime["chromeBrowserWSEndpoint"] == endpoint.browser_ws_endpoint
            && owned["creator"] == *creator
            && owned["targetId"] == target.id()
            && owned["host"] == endpoint.host
            && owned["port"] == endpoint.port
            && owned["browserWSEndpoint"] == endpoint.browser_ws_endpoint
            && runtime.get("conversationId").is_none(),
        "Grok attempt, source or original allocation changed"
    );
    crate::browser_grok::require_text_request(committed, &current["options"])?;
    Ok(current)
}

#[allow(clippy::too_many_arguments)] // Keep original provider, request and target ownership explicit.
pub(super) async fn deliver(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    creator: &Value,
    reservation: &mut RendererReservation,
    bringup: DirectoryLock,
    deferred_reconciliation: bool,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<Delivered> {
    let dispatch = Uuid::new_v4().to_string();
    let result = deliver_inner(
        store,
        id,
        witness,
        committed,
        target,
        endpoint,
        creator,
        reservation,
        bringup,
        deferred_reconciliation,
        limits,
        cancel,
        &dispatch,
    )
    .await;
    if result.is_err() {
        // Stop releases prompt-bearing callbacks even if privacy or CDP failed.
        // It never installs another witness or repeats the physical Send.
        if let Err(error) = reservation.stop_grok_witness(Duration::from_secs(1)).await {
            let _ = runtime_patch(
                store,
                id,
                witness,
                json!({"grokWitnessCleanupWarning":error.to_string()}),
            );
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn deliver_inner(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    endpoint: &AttachmentEndpoint,
    creator: &Value,
    reservation: &mut RendererReservation,
    bringup: DirectoryLock,
    deferred_reconciliation: bool,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
    dispatch: &str,
) -> Result<Delivered> {
    let original = require_original(
        store, id, witness, committed, target, endpoint, creator, cancel,
    )?;
    anyhow::ensure!(
        original.get("dispatch").is_none(),
        "Grok dispatch already exists; refusing replay"
    );
    runtime_patch(
        store,
        id,
        witness,
        json!({"submissionOwner":reservation.owner(),"provider":"grok","tabUrl":"https://grok.com/"}),
    )?;
    patch(
        store,
        id,
        witness,
        json!({"browser":merge_object(&store.require(id)?["browser"],&json!({"provider":"grok"}))}),
    )?;
    let mode = reservation
        .prepare_grok_prompt(&committed.prompt, limits.command, cancel)
        .await?;
    let model = json!({"verified":true,"source":"grok-mode-picker","selectedMode":mode,"resolvedLabel":format!("Grok {mode}")});
    runtime_patch(
        store,
        id,
        witness,
        json!({"modelSelection":model,"thinkingSelection":null,
        "effectiveModel":if mode=="Auto"{"grok-web-auto"}else{"grok-web-fast"}}),
    )?;
    bringup.release()?;
    if deferred_reconciliation {
        crate::browser_retirement::reconcile(store, target.browser(), endpoint).await?;
    }
    let before = require_original(
        store, id, witness, committed, target, endpoint, creator, cancel,
    )?;
    let started = json!({"state":"started","reservationId":committed.reservation_id,
        "submissionOwner":reservation.owner(),"promptSha256":committed.prompt_sha256,
        "startedAt":timestamp(),"providerDispatchId":dispatch});
    store.update(id, |current| {
        witness.require(current)?;
        crate::browser_attempt::require_if_child(store, id, cancel)?;
        anyhow::ensure!(
            !cancel.is_cancelled() && *current == before,
            "Grok changed before durable dispatch reservation"
        );
        Ok(Some(json!({"dispatch":started,"recoveryRequired":true})))
    })?;
    reservation.dispatch_started(limits.command, cancel).await?;
    reservation
        .install_grok_witness(
            &committed.prompt,
            &mode,
            dispatch,
            limits
                .answer
                .saturating_add(limits.commit)
                .saturating_add(limits.command),
            limits.command,
            cancel,
        )
        .await?;
    require_original(
        store, id, witness, committed, target, endpoint, creator, cancel,
    )?;
    reservation
        .send_grok_once(&committed.prompt, limits.command, cancel)
        .await?;
    let deadline = Instant::now() + limits.commit;
    let receipt = loop {
        let current = require_original(
            store, id, witness, committed, target, endpoint, creator, cancel,
        )?;
        anyhow::ensure!(
            current["dispatch"] == started,
            "Grok dispatch changed before acknowledgement"
        );
        let observed = reservation
            .grok_witness(dispatch, false, left(deadline)?.min(limits.command), cancel)
            .await?;
        match observed["state"].as_str() {
            Some("committed" | "completed") => break observed,
            Some("pending") => pause(limits.poll.min(left(deadline)?), cancel).await?,
            _ => bail!("Original Grok submission identity is invalid; preserve without replay"),
        }
    };
    store.update(id,|current| {
        witness.require(current)?;
        crate::browser_attempt::require_if_child(store,id,cancel)?;
        anyhow::ensure!(!cancel.is_cancelled() && current["status"]=="running" && current["dispatch"]==started,
            "Grok dispatch changed before commitment persistence");
        Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"runtime":merge_object(&current["browser"]["runtime"],
            &json!({"grokCommitReceipt":receipt,"submittedPromptHash":committed.prompt_sha256,"submittedPromptHashFormat":"grok-request-v1"}))}))})))
    })?;
    #[cfg(test)]
    if let Some(crash) = super::TEST_BEFORE_GROK_ACKNOWLEDGE.take() {
        crash();
    }
    reservation
        .acknowledge_grok(&receipt, limits.command, cancel)
        .await?;
    target
        .lease()
        .update(&json!({"promptSubmitted":true,"tabUrl":"https://grok.com/"}))?;
    let acknowledged = merge_object(&started, &json!({"state":"acknowledged"}));
    store.update(id, |current| {
        witness.require(current)?;
        crate::browser_attempt::require_if_child(store, id, cancel)?;
        anyhow::ensure!(
            !cancel.is_cancelled()
                && current["status"] == "running"
                && current["dispatch"] == started
                && current["browser"]["runtime"]["grokCommitReceipt"] == receipt,
            "Grok acknowledgement changed"
        );
        Ok(Some(json!({"dispatch":acknowledged})))
    })?;
    let saved = require_original(
        store, id, witness, committed, target, endpoint, creator, cancel,
    )?;
    let outcome = OutcomeWitness {
        recovery: recovery_identity(&saved),
        options_sha256: committed.options_sha256.clone(),
        dispatch: dispatch_identity(&saved),
    };
    let deadline = Instant::now() + limits.answer;
    let observed = loop {
        let current = require_original(
            store, id, witness, committed, target, endpoint, creator, cancel,
        )?;
        outcome.require(&current, witness, None, cancel)?;
        let observed = reservation
            .observe_grok_answer(
                &committed.prompt,
                left(deadline)?.min(limits.command),
                cancel,
            )
            .await?;
        if observed["state"] == "completed" {
            crate::browser_grok::accept_saved_answer(committed, &current, &observed)?;
            break observed;
        }
        anyhow::ensure!(
            observed["state"] == "pending",
            "Grok answer observation is invalid"
        );
        pause(limits.poll.min(left(deadline)?), cancel).await?;
    };
    let text = observed["text"]
        .as_str()
        .context("Grok completed answer has no text")?
        .to_owned();
    let receipt = capture_outcome(store, id, witness, &outcome, &text, Some(&observed), cancel)?;
    publish_answer(store, id, witness, &outcome, &receipt, cancel)?;
    Ok(Delivered {
        model,
        thinking: Value::Null,
        receipt,
        text,
        outcome,
    })
}

/// Finish an original Grok Private attempt whose worker exited after a possible Send.
/// Only the exact original target, renderer claim and live witness count as evidence:
/// a Private chat cannot be reopened from history, and nothing is navigated, typed or sent.
#[allow(clippy::too_many_arguments)] // Recovery keeps every original witness explicit.
pub(super) async fn recover(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    saved: &Value,
    committed: &BrowserRequest,
    target: &OwnedTarget,
    info: &Value,
    guard: &crate::browser_attempt::RecoveryGuard,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let runtime = &saved["browser"]["runtime"];
    crate::browser_grok::require_text_request(committed, &saved["options"])?;
    let mode = runtime["modelSelection"]["selectedMode"]
        .as_str()
        .context("Grok recovery mode is missing")?;
    let dispatch = saved["dispatch"]["providerDispatchId"]
        .as_str()
        .context("Grok recovery dispatch is missing")?
        .to_owned();
    anyhow::ensure!(
        saved["browser"]["provider"] == "grok"
            && runtime["provider"] == "grok"
            && runtime.get("conversationId").is_none()
            && runtime["modelSelection"]["verified"] == true
            && runtime["modelSelection"]["source"] == "grok-mode-picker"
            && runtime["effectiveModel"]
                == if mode == "Auto" {
                    "grok-web-auto"
                } else {
                    "grok-web-fast"
                }
            && saved["dispatch"]["promptSha256"] == committed.prompt_sha256
            && saved["dispatch"]["submissionOwner"] == runtime["submissionOwner"]
            && matches!(
                saved["dispatch"]["state"].as_str(),
                Some("started" | "acknowledged")
            )
            && info["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://grok.com/")),
        "Grok recovery source, dispatch or original Private target changed"
    );
    let deadline = Instant::from_std(guard.deadline());
    let owner: crate::browser_commit::SubmissionOwner =
        serde_json::from_value(runtime["submissionOwner"].clone())?;
    let intent = crate::browser_grok::Intent::new(&committed.prompt, mode, &dispatch)?;
    let rebound = RendererReservation::recover_grok(
        target.page().clone(),
        owner,
        intent,
        left(deadline)?.min(limits.command),
        cancel,
    )
    .await
    .map(|(reservation, mut claimed)| {
        if let Some(receipt) = claimed.as_object_mut() {
            receipt.remove("digest");
        }
        (reservation, claimed)
    });
    let saved_receipt = runtime["grokCommitReceipt"].clone();
    if runtime["answerSourceCapture"].is_object() {
        // A capture saved before the worker exited is revalidated by adoption, never recaptured.
        anyhow::ensure!(
            saved["dispatch"]["state"] == "acknowledged",
            "Saved Grok capture has no acknowledged commitment"
        );
        crate::browser_grok::accept_saved_answer(
            committed,
            saved,
            &runtime["acceptedObservation"],
        )?;
        // Seal the witness the worker left before its own clean release, so retirement reads a
        // sealed completion. A claim the worker already released cannot be rebound; retirement
        // then applies its own checks to the sealed or remaining witness. Every rebind failure
        // (released, quarantined, damaged, non-Private or unreachable) is deliberately treated
        // alike: adoption stays guarded and retirement retains a target it cannot verify.
        if let Ok((mut reservation, claimed)) = rebound {
            anyhow::ensure!(
                reservation.state() == crate::browser_renderer::DispatchState::Acknowledged
                    && claimed == saved_receipt,
                "Grok renderer and saved commitments differ; preserve without replay"
            );
            // A failed seal quarantines the claim. The validated durable capture still completes,
            // as it would on any later recovery, and retirement then retains the target.
            let sealed = async {
                reservation
                    .release_clean(left(deadline)?.min(limits.command))
                    .await
            }
            .await;
            if let Err(error) = sealed {
                eprintln!(
                    "Recovered Grok witness was not sealed; its target is retained: {error:#}"
                );
            }
        }
        return Ok(());
    }
    let (mut reservation, claimed_receipt) = rebound?;
    let renderer_acknowledged =
        reservation.state() == crate::browser_renderer::DispatchState::Acknowledged;
    if saved["dispatch"]["state"] == "acknowledged" {
        anyhow::ensure!(
            renderer_acknowledged && claimed_receipt == saved_receipt,
            "Grok renderer and saved commitments differ; preserve without replay"
        );
    } else {
        let receipt = if renderer_acknowledged {
            // The renderer acknowledged before the worker exited; persist only that commitment.
            claimed_receipt
        } else {
            let observed = if saved_receipt.is_null() {
                loop {
                    let observed = reservation
                        .grok_witness(
                            &dispatch,
                            false,
                            left(deadline)?.min(limits.command),
                            cancel,
                        )
                        .await?;
                    match observed["state"].as_str() {
                        // A terminal wire state cannot advance between the read and the
                        // renderer's own acknowledgement against the same observer.
                        Some("completed") => break observed,
                        Some("pending" | "committed") => {
                            pause(limits.poll.min(left(deadline)?), cancel).await?
                        }
                        _ => bail!(
                            "Original Grok submission identity is invalid; preserve without replay"
                        ),
                    }
                }
            } else {
                // The worker saved its commitment, then exited before the renderer acknowledged
                // it. The live witness must report that same turn, possibly later.
                let observed = reservation
                    .grok_witness(
                        &dispatch,
                        false,
                        left(deadline)?.min(limits.command),
                        cancel,
                    )
                    .await?;
                anyhow::ensure!(
                    crate::browser_grok::extends(&saved_receipt, &observed),
                    "Grok witness contradicts the commitment saved before the worker exited; preserve without replay"
                );
                saved_receipt.clone()
            };
            reservation
                .acknowledge_grok(&observed, left(deadline)?.min(limits.command), cancel)
                .await?;
            observed
        };
        guard.acknowledge_grok(&store.stored_for_update(id)?, &receipt)?;
    }
    let acknowledged = store.stored_for_update(id)?;
    let outcome = OutcomeWitness {
        recovery: recovery_identity(&acknowledged),
        options_sha256: committed.options_sha256.clone(),
        dispatch: dispatch_identity(&acknowledged),
    };
    let observed = loop {
        let current = store.require(id)?;
        outcome.require(&current, witness, None, cancel)?;
        let observed = reservation
            .observe_grok_answer(
                &committed.prompt,
                left(deadline)?.min(limits.command),
                cancel,
            )
            .await?;
        if observed["state"] == "completed" {
            crate::browser_grok::accept_saved_answer(committed, &current, &observed)?;
            break observed;
        }
        anyhow::ensure!(
            observed["state"] == "pending",
            "Grok answer observation is invalid"
        );
        pause(limits.poll.min(left(deadline)?), cancel).await?;
    };
    let text = observed["text"]
        .as_str()
        .context("Grok completed answer has no text")?
        .to_owned();
    let receipt = capture_outcome(store, id, witness, &outcome, &text, Some(&observed), cancel)?;
    publish_answer(store, id, witness, &outcome, &receipt, cancel)?;
    // Seal the completed witness and release the claim, as the original worker would have
    // before parent acknowledgement. Retirement then reads only the sealed completion.
    reservation
        .release_clean(left(deadline)?.min(limits.command))
        .await
}

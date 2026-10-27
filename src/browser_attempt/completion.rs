//! A selected child keeps its creator and completes only after parent publication or ACK.
use super::*;
use crate::{
    browser_chrome::AttachmentEndpoint, browser_disposition::TargetJournal, cdp::Cdp,
    sessions::timestamp,
};
use std::time::{Duration, Instant};

impl Parent {
    pub(crate) fn complete(&self, cancel: &CancellationToken) -> Result<Value> {
        let snapshot = self.store.require(&self.id)?;
        self.require(&snapshot, cancel)?;
        adoption::require_selected_until(
            &self.store,
            &self.id,
            cancel,
            self.witness.recovery_deadline(),
        )?;
        ensure!(
            snapshot["browser"]["remoteDelivery"].is_null(),
            "Remote consultation requires delivery acknowledgement"
        );
        let model = snapshot["model"]
            .as_str()
            .context("Consultation model missing")?;
        let receipt: crate::artifacts::CaptureReceipt =
            serde_json::from_value(snapshot["browser"]["capturedAnswer"].clone())?;
        let text = String::from_utf8(crate::artifacts::verify_capture_until(
            &self.store,
            &receipt,
            Some(cancel),
            self.witness.recovery_deadline(),
        )?)?;
        let require = |current: &Value, run: Option<&Value>| -> Result<()> {
            self.require(current, cancel)?;
            adoption::require_selected_until(
                &self.store,
                &self.id,
                cancel,
                self.witness.recovery_deadline(),
            )?;
            ensure!(
                run.is_none_or(|run| !crate::sessions::terminal(
                    run["status"].as_str().unwrap_or("")
                )),
                "Consultation model outcome changed before completion"
            );
            Ok(())
        };
        let committed = crate::browser_runner::request(&self.store, &self.id)?;
        let added = crate::browser_log::completion_warnings(&snapshot, &committed);
        let mut warnings = snapshot["browser"]["warnings"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        warnings.extend(added.iter().cloned());
        let link: Link = serde_json::from_value(
            snapshot["browser"]["consultation"]["adoption"]["link"].clone(),
        )?;
        let view = adoption::with_conversation(&snapshot, &self.store.require(&link.child_id)?);
        let (archive, line) = crate::browser_log::archive(&view);
        let block = line + &crate::browser_log::completion(&snapshot, &committed, &warnings, &text);
        self.store
            .append_log_guarded(&self.id, Some(model), &block, require)?;
        self.store
            .update_session_and_model(&self.id, model, |current, run| {
                require(current, run)?;
                let mut response = merge_object(
                    &run.map_or(Value::Null, |run| run["response"].clone()),
                    &json!({"status":"completed"}),
                );
                response
                    .as_object_mut()
                    .context("Consultation response missing")?
                    .remove("incompleteReason");
                let completed = timestamp();
                let mut changes =
                    json!({"status":"completed","completedAt":completed,"recoveryRequired":true});
                changes["browser"] = if added.is_empty() {
                    current["browser"].clone()
                } else {
                    crate::browser_log::with_warnings(&current["browser"], &added)
                };
                changes["browser"]["archive"] = archive.clone();
                Ok(Some((
                    changes,
                    Some(json!({"status":"completed","completedAt":completed,"response":response})),
                )))
            })
    }
}

fn acknowledged_parent(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<(Link, Value)> {
    let link = require_selected(store, id, cancel)?;
    let parent = store.require(id)?;
    let delivery = &parent["browser"]["remoteDelivery"];
    ensure!(
        parent["status"] == "completed"
            && (delivery.is_null()
                || delivery["acknowledged"] == true && delivery["producerFinished"] == true),
        "Selected child requires durable parent completion or final remote ACK"
    );
    require_completed_model(store, &parent)?;
    Ok((link, parent))
}

fn require_completed_model(store: &SessionStore, meta: &Value) -> Result<()> {
    let runs = store.model_runs(meta["id"].as_str().context("Selected session ID missing")?)?;
    let matching = runs
        .iter()
        .filter(|run| run["model"] == meta["model"])
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == 1 && matching[0]["status"] == "completed",
        "Selected session model outcome changed before retirement"
    );
    Ok(())
}

fn commitment(
    store: &SessionStore,
    journal: &TargetJournal,
    creator: &Value,
    child: &Value,
    parent: &Value,
) -> Result<Value> {
    let mut commit = journal.session_commit(creator, child)?;
    let required = commit["requiredOutputs"]
        .as_array_mut()
        .context("Child commitment output set missing")?;
    let capture = &parent["browser"]["capturedAnswer"];
    required.push(json!({"file":crate::native_fs::canonical_wire_path(&store.directory(parent["id"].as_str().context("Parent ID missing")?)?.join(capture["file"].as_str().context("Parent capture missing")?))?,
        "bytes":capture["bytes"],"sha256":capture["sha256"]}));
    if parent["browser"]["requiredTextOutput"].is_object() {
        required.push(parent["browser"]["requiredTextOutput"].clone());
    }
    for artifact in parent["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| matches!(row["kind"].as_str(), Some("file" | "image")))
    {
        required.push(json!({"file":artifact["path"],"bytes":artifact["sizeBytes"],"sha256":artifact["sha256"],"nativeIdentity":artifact["nativeIdentity"]}));
    }
    Ok(commit)
}

/// Idempotent after worker exit. The immutable selection binds the original child controller.
pub(crate) fn complete_selected(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<Value> {
    let (link, parent) = acknowledged_parent(store, id, cancel)?;
    let snapshot = store.stored_for_update(&link.child_id)?;
    let creator = snapshot["browser"]["runtime"]["ownedRecoveryTarget"]["creator"].clone();
    let journal = TargetJournal::open(store)?;
    let model = snapshot["model"]
        .as_str()
        .context("Selected child model missing")?;
    let completed=store.update_session_and_model(&link.child_id,model,|current,run| {
        ensure!(*current==snapshot,"Selected child changed before completion");
        let (active,latest)=acknowledged_parent(store,id,cancel)?;
        ensure!(active==link && latest==parent,"Selected parent changed before child completion");
        ensure!(matches!(current["status"].as_str(),Some("running"|"completed"))
            && run.is_none_or(|run|matches!(run["status"].as_str(),Some("running"|"completed"))),
            "Selected child model outcome changed");
        let completed=current.get("completedAt").filter(|value|value.is_string()).cloned().unwrap_or_else(||json!(timestamp()));
        let changes=json!({"status":"completed","completedAt":completed,"recoveryRequired":true,
            "dispatch":merge_object(&current["dispatch"],&json!({"state":"completed"}))});
        let proposed=merge_object(current,&changes);
        let mut record=journal.read(&creator["allocationId"])?;
        ensure!(record["creator"]==creator,"Selected child creator changed");
        record["commit"]=commitment(store,&journal,&creator,&proposed,&parent)?;
        journal.verify_proposed(&record,&proposed)?;
        if current["status"]=="completed" {return Ok(None);}
        Ok(Some((changes,Some(json!({"status":"completed","completedAt":completed,"response":merge_object(&run.map_or(Value::Null,|run|run["response"].clone()),&json!({"status":"completed"}))})))))
    })?;
    acknowledged_parent(store, id, cancel)?;
    let committed = journal.commit(
        &creator,
        &commitment(store, &journal, &creator, &completed, &parent)?,
    )?;
    journal.finalise(&committed)?;
    Ok(completed)
}

/// Uses the child's existing exact-target retirement policy; the parent gains no close capability.
#[cfg(test)]
pub(crate) async fn settle_selected(
    store: &SessionStore,
    id: &str,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    cancel: &CancellationToken,
) -> Result<()> {
    retry_retirement(store, id, cancel, || {
        settle_selected_once(store, id, browser, endpoint, cancel, None)
    })
    .await
}

pub(super) async fn settle_recovered(
    store: &SessionStore,
    id: &str,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    cancel: &CancellationToken,
    guard: &super::recovery::Guard,
) -> Result<()> {
    retry_retirement(store, id, cancel, || async {
        guard.require_session(id, &store.stored_for_update(id)?)?;
        let check = || guard.require_retirement();
        settle_selected_once(store, id, browser, endpoint, cancel, Some(&check)).await
    })
    .await
}

async fn retry_retirement<F, Fut>(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
    mut retire: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let first = retire().await;
    let Err(first) = first else {
        return Ok(());
    };
    // Retry only the durable selected outcome. The original retirement marker and
    // fresh source/owner checks remain mandatory, including after a lost close ACK.
    acknowledged_parent(store, id, cancel)?;
    tokio::time::timeout(Duration::from_secs(8), retire())
        .await
        .context("Selected child cleanup retry deadline reached")
        .and_then(|result| result)
        .with_context(|| format!("Selected child cleanup retry failed after: {first:#}"))
}

async fn settle_selected_once(
    store: &SessionStore,
    id: &str,
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    cancel: &CancellationToken,
    check: Option<&(dyn Fn() -> Result<()> + Send + Sync)>,
) -> Result<()> {
    let child = complete_selected(store, id, cancel)?;
    let creator = &child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
    let journal = TargetJournal::open(store)?;
    acknowledged_parent(store, id, cancel)?;
    let disposition = crate::browser_retirement::retire_committed_guarded(
        store, &journal, browser, endpoint, creator, check, cancel,
    )
    .await?;
    finish_retirement(store, id, &journal, creator, &disposition, cancel)
}

fn finish_retirement(
    store: &SessionStore,
    id: &str,
    journal: &TargetJournal,
    creator: &Value,
    disposition: &Value,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(
        disposition["decision"] == "retired" && disposition["absence"] == "confirmed-absent",
        "Selected child target absence remains unconfirmed: {}",
        disposition["reason"]
            .as_str()
            .unwrap_or("No cleanup reason recorded")
    );
    crate::browser_runner::settle_completed_retirement(
        store,
        journal,
        creator,
        disposition,
        Instant::now() + Duration::from_secs(2),
    )?;
    // A duplicate ACK must remain a no-close repair even after deferred Chrome has exited.
    acknowledged_parent(store, id, cancel)?;
    store.update(id, |current| {
        acknowledged_parent(store, id, cancel)?;
        ensure!(
            current["status"] == "completed",
            "Parent outcome changed during selected-child settlement"
        );
        Ok(Some(
            json!({"recoveryRequired":false,"cleanupWarning":null}),
        ))
    })?;
    Ok(())
}

pub(crate) async fn settle_selected_saved(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let (link, parent) = acknowledged_parent(store, id, cancel)?;
    let check = || {
        let (latest_link, latest) = acknowledged_parent(store, id, cancel)?;
        ensure!(
            latest_link == link
                && latest["browser"]["consultation"]["selected"]
                    == parent["browser"]["consultation"]["selected"]
                && latest["browser"]["remoteDelivery"] == parent["browser"]["remoteDelivery"],
            "Selected delivery changed during saved settlement"
        );
        let child = store.stored_for_update(&link.child_id)?;
        ensure!(
            child["status"] == "completed" && child["dispatch"]["state"] == "completed",
            "Selected child completion changed during saved settlement"
        );
        require_completed_model(store, &child)?;
        ensure!(
            !cancel.is_cancelled(),
            "Selected child settlement was cancelled"
        );
        Ok(())
    };
    let result = retry_retirement(store, id, cancel, || async {
        let child = complete_selected(store, id, cancel)?;
        let creator = &child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"];
        let journal = TargetJournal::open(store)?;
        let record = journal.read(&creator["allocationId"])?;
        if record["disposition"]["decision"] == "retired" {
            return finish_retirement(store, id, &journal, creator, &record["disposition"], cancel);
        }
        let target = &creator["target"];
        let endpoint = AttachmentEndpoint {
            host: target["host"]
                .as_str()
                .context("Selected child host missing")?
                .into(),
            port: u16::try_from(
                target["port"]
                    .as_u64()
                    .context("Selected child port missing")?,
            )?,
            browser_ws_endpoint: target["browserWSEndpoint"]
                .as_str()
                .context("Selected child pinned endpoint missing")?
                .into(),
        };
        // Each attempt reconnects only to this pinned endpoint. A lost close ACK
        // cannot make the retry discover or launch another browser.
        let browser = endpoint.connect(Duration::from_secs(1), cancel).await?;
        let retired =
            settle_selected_once(store, id, &browser, &endpoint, cancel, Some(&check)).await;
        if let Err(error) = browser.disconnect_and_wait(Duration::from_secs(1)).await {
            eprintln!("Selected child connection cleanup remains pending: {error:#}");
        }
        retired
    })
    .await;
    Box::pin(crate::browser_runner::settle_deferred_fresh(
        store,
        Some(&link.child_id),
    ))
    .await;
    result
}

//! Saved log observation and recovery never prepare or replay a dispatch.
use crate::{
    browser_chrome::AttachmentEndpoint,
    browser_historical, browser_inspection,
    browser_profile::ProfileRegistry,
    browser_runner::{self, RunnerLimits},
    config::Environment,
    policy,
    process::{Liveness, ProcessIdentity},
    session_display,
    sessions::{self, SessionStore},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    path::Path,
    time::Duration,
};

/// Reference MAX_RENDER_BYTES: larger saved logs are never offered for rendering.
const MAX_RENDER_BYTES: usize = 200_000;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct LogCursor {
    saved: BTreeMap<String, String>,
    last_model: Option<String>,
}
impl LogCursor {
    fn emit(&mut self, model: &str, text: String, output: &mut impl Write) -> Result<()> {
        let previous = self.saved.get(model).map(String::as_str).unwrap_or("");
        if text.is_empty() || text == previous || text.trim_end() == previous.trim_end() {
            return Ok(());
        }
        let heading = format!("=== {model} ===\n");
        let tail = text.strip_prefix(previous).or_else(|| {
            text.strip_prefix(&heading)
                .and_then(|body| body.strip_prefix(previous))
        });
        let changed = tail.is_none();
        let chunk = tail.unwrap_or(&text);
        if self.last_model.as_deref().is_some_and(|last| last != model) || changed {
            write!(output, "\n\n")?;
            if !chunk.starts_with(&heading) {
                writeln!(output, "=== {model} ===")?;
            }
        }
        output.write_all(chunk.as_bytes())?;
        output.flush()?;
        self.saved.insert(model.into(), text);
        self.last_model = Some(model.into());
        Ok(())
    }
    fn drain(
        &mut self,
        store: &SessionStore,
        id: &str,
        meta: &Value,
        model: Option<&str>,
        output: &mut impl Write,
    ) -> Result<()> {
        if let Some(model) = model.filter(|model| !policy::trim(model).is_empty()) {
            return self.emit(model, store.log_for_model(id, model)?, output);
        }
        let mut runs = meta["models"].as_array().cloned().unwrap_or_default();
        runs.sort_by(|a, b| {
            a["startedAt"]
                .as_str()
                .cmp(&b["startedAt"].as_str())
                .then_with(|| a["model"].as_str().cmp(&b["model"].as_str()))
        });
        if runs.is_empty() {
            return self.emit(
                meta["model"].as_str().unwrap_or("session"),
                store.log(id)?,
                output,
            );
        }
        let mut logs = Vec::with_capacity(runs.len());
        for run in runs {
            let name = run["model"]
                .as_str()
                .context("Saved model log has no model")?;
            logs.push((name.to_owned(), store.log_for_model(id, name)?));
        }
        if logs.iter().all(|(_, text)| policy::trim(text).is_empty()) {
            return self.emit("session", store.log(id)?, output);
        }
        for (name, text) in logs {
            self.emit(&name, text, output)?;
        }
        Ok(())
    }
}
fn dead_owner(meta: &Value) -> Option<ProcessIdentity> {
    let identity: ProcessIdentity =
        serde_json::from_value(meta["lifecycle"]["processIdentity"].clone()).ok()?;
    (identity.liveness() == Liveness::Dead
        && meta["lifecycle"]["workerPid"].as_u64() == Some(u64::from(identity.pid))
        && meta["lifecycle"]["workerProcessStartedAtMs"].as_f64() == Some(identity.started_at_ms))
    .then_some(identity)
}
fn same_controller(current: &Value, saved: &Value) -> bool {
    current["lifecycle"] == saved["lifecycle"]
        && current["browser"]["runtime"].get("controllerPid")
            == saved["browser"]["runtime"].get("controllerPid")
        && current["browser"]["runtime"].get("controllerProcessStartedAtMs")
            == saved["browser"]["runtime"].get("controllerProcessStartedAtMs")
}
fn dead_legacy_worker(meta: &Value) -> bool {
    meta["lifecycle"]["detached"] == true
        && browser_historical::legacy_record(meta)
        && browser_historical::controllers_dead(meta)
}
fn legacy_recovery(meta: &Value) -> bool {
    let runtime = &meta["browser"]["runtime"];
    let incomplete = matches!(
        meta["response"]["incompleteReason"].as_str(),
        Some("chrome-disconnected" | "incomplete-capture")
    );
    let endpoint = ["chromePort", "chromeBrowserWSEndpoint", "chromeProfileRoot"]
        .iter()
        .any(|key| !runtime[*key].is_null());
    let conversation = runtime["conversationId"]
        .as_str()
        .is_some_and(crate::browser::stable_conversation_id)
        || runtime["tabUrl"]
            .as_str()
            .and_then(crate::browser::conversation_id)
            .is_some();
    browser_historical::legacy_record(meta)
        && browser_historical::controllers_dead(meta)
        && meta["mode"] == "browser"
        && (meta["status"] == "running" || meta["status"] == "error" && incomplete)
        && (endpoint || conversation)
        && (incomplete || runtime.get("controllerPid").is_some())
}
fn settle_dead(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    cancel: &CancellationToken,
) -> Result<bool> {
    let model = saved["model"]
        .as_str()
        .context("Dead worker session has no model")?;
    let mut changed = false;
    store.update_session_and_model(id,model,|current,run| {
        if cancel.is_cancelled()||!same_controller(current,saved)
            || !(dead_owner(current).is_some() || dead_legacy_worker(current) && browser_historical::same_basis(current,saved))
            ||!matches!(current["status"].as_str(),Some("pending"|"running"))
            ||run.is_some_and(|run|!matches!(run["status"].as_str(),Some("pending"|"running"))){return Ok(None)}
        if let Some(pid)=current["browser"]["runtime"].get("controllerPid") {
            let pid=pid.as_u64().and_then(|pid|u32::try_from(pid).ok()).context("Saved browser controller is malformed")?;
            if let Some(actual)=ProcessIdentity::read(pid)? {
                let saved_start=current["browser"]["runtime"]["controllerProcessStartedAtMs"].as_f64();
                ensure!(saved_start.is_some_and(|start|start.is_finite()&&start>0.0&&start!=actual.started_at_ms),"Another browser controller remains live or uncertain");
            }
        }
        changed=true;let completed=sessions::timestamp();let message="Detached worker exited before the session reached a terminal state.";
        let failure=json!({"category":"internal","message":message});
        let response=sessions::merge_object(&current["response"],&json!({"status":"incomplete","incompleteReason":"incomplete-capture"}));
        Ok(Some((json!({"status":"error","completedAt":completed,"errorMessage":message,"error":failure,"response":response,"recoveryRequired":current["recoveryRequired"]==true||current.get("dispatch").is_some()||current["browser"]["runtime"].get("ownedRecoveryTarget").is_some()}),Some(json!({"status":"error","completedAt":completed,"error":failure,"response":sessions::merge_object(&run.map(|run|run["response"].clone()).unwrap_or(Value::Null),&json!({"status":"incomplete","incompleteReason":"incomplete-capture"}))})))))
    })?;
    Ok(changed)
}
async fn recover_native(
    store: &SessionStore,
    id: &str,
    meta: &Value,
    cancel: &CancellationToken,
) -> Result<()> {
    let restored = crate::browser_attempt::settle_unsent_switch(store, id, cancel).await?;
    let current = if restored {
        Some(store.stored_for_update(id)?)
    } else {
        None
    };
    let meta = current.as_ref().unwrap_or(meta);
    let active = crate::browser_attempt::active_target(store, meta)?;
    let target_meta = active.as_ref().unwrap_or(meta);
    let runtime = &target_meta["browser"]["runtime"];
    let endpoint = AttachmentEndpoint {
        host: runtime["chromeHost"]
            .as_str()
            .context("Native recovery endpoint host is missing")?
            .into(),
        port: runtime["chromePort"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .context("Native recovery endpoint port is missing")?,
        browser_ws_endpoint: runtime["chromeBrowserWSEndpoint"]
            .as_str()
            .context("Native recovery has no pinned browser socket")?
            .into(),
    };
    endpoint.validate()?;
    let profile = ProfileRegistry::open_existing(Path::new(
        runtime["profileDir"]
            .as_str()
            .context("Native recovery has no recorded admission directory")?,
    ))?;
    let timeout = Duration::from_millis(
        meta["browser"]["config"]["inputTimeoutMs"]
            .as_u64()
            .unwrap_or(60_000),
    );
    let browser = endpoint.connect(timeout, cancel).await?;
    let result = browser_runner::recover_attached(
        store,
        id,
        profile,
        browser.clone(),
        endpoint,
        &RunnerLimits {
            command: timeout,
            answer: browser_runner::answer_timeout(&meta["browser"]["config"]),
            ..Default::default()
        },
        cancel,
    )
    .await;
    let cleanup = browser
        .disconnect_and_wait(Duration::from_millis(500))
        .await;
    match (result, cleanup) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup.context("Recovery connection cleanup is unconfirmed")),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "Recovery connection cleanup is unconfirmed: {cleanup:#}"
        ))),
    }
}
async fn recover_legacy(
    store: &SessionStore,
    id: &str,
    saved: &Value,
    environment: &Environment,
    cancel: &CancellationToken,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "Attempting to reattach to the existing Chrome session..."
    )?;
    output.flush()?;
    let operation_cancel = cancel.child_token();
    let mut write_error = None;
    let snapshot =
        browser_inspection::reattach(store, id, environment, &operation_cancel, |line| {
            if write_error.is_none()
                && let Err(error) = writeln!(output, "{line}").and_then(|_| output.flush())
            {
                write_error = Some(error);
                operation_cancel.cancel();
            }
        })
        .await;
    let snapshot = match (write_error, snapshot) {
        (Some(write), Err(error)) => {
            return Err(error.context(format!("Recovery status output failed: {write}")));
        }
        (Some(write), Ok(_)) => return Err(write.into()),
        (None, result) => result?,
    };
    let completion=browser_historical::complete_recovered(store,id,saved,&snapshot,cancel).await?
        .context("Recovered metadata was preserved because session ownership, source or terminal state changed")?;
    if let Some(line) = recovered_cleanup_line(&completion) {
        writeln!(output, "{line}")?;
    }
    Ok(())
}
/// Reference attachSession's closing lines once a followed session reaches a terminal state.
fn finished(meta: &Value, output: &mut impl Write) -> Result<()> {
    if let Some(error) = meta["errorMessage"]
        .as_str()
        .filter(|_| meta["status"] == "error")
    {
        writeln!(output, "\nResult:\nSession failed: {error}")?;
    }
    if matches!(meta["status"].as_str(), Some("completed" | "partial")) && meta["usage"].is_object()
    {
        match session_display::completion_summary(meta) {
            Some(summary) => writeln!(output, "\n{summary}")?,
            None => {
                let usage = &meta["usage"];
                let count = |key: &str| match &usage[key] {
                    Value::Null => "undefined".to_owned(),
                    value => value.to_string(),
                };
                writeln!(
                    output,
                    "\nFinished (tok i/o/r/t: {}/{}/{}/{})",
                    count("inputTokens"),
                    count("outputTokens"),
                    count("reasoningTokens"),
                    count("totalTokens")
                )?;
            }
        }
    }
    Ok(())
}

/// The reference's `retireRecoveredBrowserTarget` lines after a recovered answer is saved.
fn recovered_cleanup_line(completion: &browser_historical::HistoricalCompletion) -> Option<String> {
    if let Some(error) = &completion.cleanup_error {
        return Some(format!(
            "Recovered answer is saved; browser tab cleanup failed: {error}"
        ));
    }
    let disposition = completion.disposition.as_ref()?;
    (disposition["decision"] == "retired" && disposition["absence"] == "confirmed-absent").then(
        || "Retired Industrious-owned browser tab after saving the recovered answer.".to_owned(),
    )
}
fn report_recovery_error(output: &mut impl Write, error: anyhow::Error) -> Result<()> {
    match writeln!(output, "\nBrowser recovery is incomplete: {error:#}")
        .and_then(|_| output.flush())
    {
        Ok(()) => Ok(()),
        Err(report) => Err(error.context(format!(
            "Recovery diagnostics could not be written: {report}"
        ))),
    }
}
/// What an attachment shows: the model filter, the saved prompt and the render request.
pub(crate) struct AttachView<'a> {
    pub model: Option<&'a str>,
    pub hide_prompt: bool,
    pub render: bool,
}
pub(crate) async fn attach(
    store: &SessionStore,
    id: &str,
    view: &AttachView<'_>,
    environment: &Environment,
    cancel: &CancellationToken,
    output: &mut impl Write,
) -> Result<i32> {
    attach_inner(store, id, view, environment, cancel, output, None).await
}
pub(crate) async fn attach_launched(
    store: &SessionStore,
    id: &str,
    worker: &ProcessIdentity,
    environment: &Environment,
    cancel: &CancellationToken,
    output: &mut impl Write,
) -> Result<i32> {
    let view = AttachView {
        model: None,
        hide_prompt: true,
        render: false,
    };
    attach_inner(store, id, &view, environment, cancel, output, Some(worker)).await
}
#[allow(clippy::too_many_arguments)] // The optional exact launch witness distinguishes cancellation authority from observation.
async fn attach_inner(
    store: &SessionStore,
    id: &str,
    view: &AttachView<'_>,
    environment: &Environment,
    cancel: &CancellationToken,
    output: &mut impl Write,
    launched: Option<&ProcessIdentity>,
) -> Result<i32> {
    let model = view.model;
    let initial = store.require(id)?;
    if let Some(model) = model {
        store.log_for_model(id, model)?;
    }
    if launched.is_none() {
        let chain = session_display::chain_line(&initial, || store.list(true).unwrap_or_default());
        for line in session_display::attach_metadata(&initial, &chain) {
            writeln!(output, "{line}")?;
        }
    }
    if launched.is_none()
        && !view.hide_prompt
        && let Some(prompt) = session_display::stored_prompt(store, id, &initial)
    {
        writeln!(output, "Prompt:\n{prompt}\n---")?;
    }
    let recoverable = (initial["mode"] == "browser"
        && (initial["status"] == "running" || initial["recoveryRequired"] == true)
        && dead_owner(&initial).is_some())
        || legacy_recovery(&initial);
    if launched.is_none()
        && sessions::terminal(initial["status"].as_str().unwrap_or(""))
        && !recoverable
    {
        // Reference attachSession for a session that was already finished: the saved log from
        // its first answer heading, then the completion summary. Output stays plain text.
        let log = session_display::display_log(store, id, &initial, model)?;
        let shown = session_display::trim_before_first_answer(&log);
        if view.render && shown.len() > MAX_RENDER_BYTES {
            writeln!(
                output,
                "Render skipped (log too large: {} bytes > {MAX_RENDER_BYTES}). Showing raw text.",
                shown.len()
            )?;
        } else if view.render && !std::io::stdout().is_terminal() {
            writeln!(
                output,
                "Render requested but stdout is not a TTY; showing raw text."
            )?;
        }
        output.write_all(shown.as_bytes())?;
        if let Some(summary) = session_display::completion_summary(&store.require(id)?) {
            writeln!(output, "\n{summary}")?;
        }
        output.flush()?;
        return Ok(0);
    }
    if launched.is_none() && view.render {
        writeln!(
            output,
            "Render will apply after completion; streaming raw text meanwhile..."
        )?;
    }
    let mut cursor = LogCursor::default();
    let mut attempted = None;
    let mut attempted_legacy = None;
    let mut cancellation_requested = false;
    let mut cancellation_marker = None;
    loop {
        if cancel.is_cancelled() && launched.is_none() {
            return Ok(130);
        }
        if cancel.is_cancelled() && !cancellation_requested {
            let worker = launched.context("Missing launch cancellation authority")?;
            match store.cancel_launched_worker(id, worker) {
                Ok(marker) => cancellation_marker = marker,
                Err(error) => {
                    writeln!(
                        output,
                        "Unable to request cancellation from launched worker: {error:#}"
                    )?;
                    output.flush()?;
                    return Ok(130);
                }
            }
            cancellation_requested = true;
        }
        let meta = store.require(id)?;
        cursor.drain(store, id, &meta, model, output)?;
        let selected = model.is_none_or(|model| {
            policy::trim(model).is_empty()
                || meta["model"]
                    .as_str()
                    .is_some_and(|saved| saved.eq_ignore_ascii_case(policy::trim(model)))
        });
        let owner = dead_owner(&meta);
        if !cancellation_requested
            && selected
            && meta["mode"] == "browser"
            && meta["status"] != "cancelled"
            && (meta["status"] == "running" || meta["recoveryRequired"] == true)
            && owner.is_some()
            && owner != attempted
        {
            attempted = owner;
            if let Err(error) = recover_native(store, id, &meta, cancel).await {
                report_recovery_error(output, error)?;
                if cancel.is_cancelled() {
                    return Ok(130);
                }
            }
            continue;
        }
        if !cancellation_requested
            && selected
            && legacy_recovery(&meta)
            && attempted_legacy
                .as_ref()
                .is_none_or(|saved| !browser_historical::same_basis(&meta, saved))
        {
            let saved = store.stored_for_update(id)?;
            attempted_legacy = Some(saved.clone());
            if let Err(error) = recover_legacy(store, id, &saved, environment, cancel, output).await
            {
                report_recovery_error(output, error)?;
                if cancel.is_cancelled() {
                    return Ok(130);
                }
            }
            continue;
        }
        if sessions::terminal(meta["status"].as_str().unwrap_or("")) {
            let final_meta = store.require(id)?;
            cursor.drain(store, id, &final_meta, model, output)?;
            if launched.is_none() {
                finished(&final_meta, output)?;
            }
            output.flush()?;
            if let Some(marker) = cancellation_marker {
                marker.clear()?;
            }
            return Ok(
                if cancellation_requested
                    && !matches!(final_meta["status"].as_str(), Some("completed" | "partial"))
                {
                    130
                } else if launched.is_some() && final_meta["status"] == "error" {
                    1
                } else {
                    0
                },
            );
        }
        if owner.is_some() || dead_legacy_worker(&meta) {
            let settlement_cancel = if cancellation_requested {
                CancellationToken::new()
            } else {
                cancel.clone()
            };
            let changed = settle_dead(store, id, &meta, &settlement_cancel)?;
            let current = store.require(id)?;
            cursor.drain(store, id, &current, model, output)?;
            if cancel.is_cancelled() && !cancellation_requested {
                return Ok(130);
            }
            if changed {
                continue;
            }
            if !same_controller(&current, &meta)
                || dead_owner(&current).is_none() && !dead_legacy_worker(&current)
                || sessions::terminal(current["status"].as_str().unwrap_or(""))
            {
                continue;
            }
            writeln!(
                output,
                "\nModel metadata is terminal while the session commit is incomplete. Session ownership was preserved."
            )?;
            output.flush()?;
            if let Some(marker) = cancellation_marker {
                marker.clear()?;
            }
            return Ok(if cancellation_requested {
                130
            } else if launched.is_some() {
                1
            } else {
                0
            });
        }
        tokio::select! {biased;_=cancel.cancelled(), if !cancellation_requested => {},_=tokio::time::sleep(Duration::from_secs(1))=>{}}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recovered_cleanup_lines_match_the_reference() {
        let completion = |disposition: Option<Value>, error: Option<&str>| {
            super::recovered_cleanup_line(&browser_historical::HistoricalCompletion {
                session: json!({}),
                disposition,
                cleanup_error: error.map(str::to_owned),
            })
        };
        let retired = json!({"decision":"retired","absence":"confirmed-absent"});
        assert_eq!(
            completion(Some(retired.clone()), None).as_deref(),
            Some("Retired Industrious-owned browser tab after saving the recovered answer.")
        );
        assert_eq!(
            completion(Some(retired), Some("PUBLIC close refused")).as_deref(),
            Some("Recovered answer is saved; browser tab cleanup failed: PUBLIC close refused")
        );
        assert_eq!(
            completion(
                Some(json!({"decision":"retained","absence":"unknown"})),
                None
            ),
            None
        );
        assert_eq!(completion(None, None), None);
    }
    use super::*;
    fn fixture() -> (tempfile::TempDir, SessionStore, String) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta=store.create(json!({"prompt":"PUBLIC attached session","model":"PUBLIC-a","models":["PUBLIC-a","PUBLIC-b"]}),home.path(),None).unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        (home, store, id)
    }
    #[test]
    fn a_failed_diagnostic_write_preserves_the_original_recovery_and_cleanup_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "PUBLIC closed pipe",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = anyhow::anyhow!("PUBLIC original recovery error")
            .context("PUBLIC observation cleanup error");
        let error = report_recovery_error(&mut Broken, error).unwrap_err();
        let text = format!("{error:#}");
        for message in [
            "PUBLIC closed pipe",
            "PUBLIC original recovery error",
            "PUBLIC observation cleanup error",
        ] {
            assert!(text.contains(message), "{text}");
        }
    }
    #[test]
    fn a_later_write_to_an_earlier_model_never_repeats_another_models_output() {
        let (_home, store, id) = fixture();
        store
            .append_log(&id, Some("PUBLIC-a"), "PUBLIC A1\n")
            .unwrap();
        store
            .append_log(&id, Some("PUBLIC-b"), "PUBLIC B1\n")
            .unwrap();
        let mut cursor = LogCursor::default();
        let mut output = vec![];
        cursor
            .drain(&store, &id, &store.require(&id).unwrap(), None, &mut output)
            .unwrap();
        store
            .append_log(&id, Some("PUBLIC-a"), "PUBLIC A2 café 🦀\n")
            .unwrap();
        cursor
            .drain(&store, &id, &store.require(&id).unwrap(), None, &mut output)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        for text in ["PUBLIC A1", "PUBLIC B1", "PUBLIC A2 café 🦀"] {
            assert_eq!(output.matches(text).count(), 1, "{output}");
        }
    }
    #[test]
    fn root_only_multi_model_log_is_emitted_once_before_model_logs_appear() {
        let (_home, store, id) = fixture();
        store
            .append_log(&id, None, "PUBLIC combined log\n")
            .unwrap();
        store
            .update(&id, |_| Ok(Some(json!({"status":"completed"}))))
            .unwrap();
        let mut cursor = LogCursor::default();
        let mut output = vec![];
        for _ in 0..2 {
            cursor
                .drain(&store, &id, &store.require(&id).unwrap(), None, &mut output)
                .unwrap();
        }
        for model in ["PUBLIC-a", "PUBLIC-b"] {
            store
                .append_log(&id, Some(model), &format!("PUBLIC answer {model}\n"))
                .unwrap();
        }
        for _ in 0..2 {
            cursor
                .drain(&store, &id, &store.require(&id).unwrap(), None, &mut output)
                .unwrap();
        }
        let output = String::from_utf8(output).unwrap();
        for text in [
            "PUBLIC combined log",
            "PUBLIC answer PUBLIC-a",
            "PUBLIC answer PUBLIC-b",
        ] {
            assert_eq!(output.matches(text).count(), 1, "{output}");
        }
    }
    #[test]
    fn occupied_controller_pid_requires_positive_evidence_of_a_different_incarnation() {
        let actual = ProcessIdentity::current().unwrap();
        for start in [
            Value::Null,
            json!("malformed"),
            json!(0),
            json!(-1),
            json!(actual.started_at_ms),
            json!(actual.started_at_ms - 1.0),
        ] {
            let (_home, store, id) = fixture();
            let mut dead = actual.clone();
            dead.pid = u32::MAX;
            dead.started_at_ms = 1.0;
            let saved = store.update(&id, |_| Ok(Some(json!({"status":"running","lifecycle":{"processIdentity":dead,"workerPid":dead.pid,"workerProcessStartedAtMs":dead.started_at_ms},"browser":{"runtime":{"controllerPid":actual.pid,"controllerProcessStartedAtMs":start}}})))).unwrap();
            let before = store.stored_for_update(&id).unwrap();
            let result = settle_dead(&store, &id, &saved, &CancellationToken::new());
            if start == json!(actual.started_at_ms - 1.0) {
                assert!(result.unwrap());
                assert_eq!(store.require(&id).unwrap()["status"], "error");
            } else {
                assert!(result.is_err(), "{start}");
                assert_eq!(store.stored_for_update(&id).unwrap(), before);
            }
        }
        assert_eq!(actual.liveness(), Liveness::Alive);
    }
    #[tokio::test]
    async fn final_log_is_drained_and_error_status_still_has_successful_attachment_exit() {
        let (_home, store, id) = fixture();
        let writer_store = store.clone();
        let writer_id = id.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer_store
                .append_log(&writer_id, Some("PUBLIC-a"), "PUBLIC final answer\n")
                .unwrap();
            writer_store
                .update(&writer_id, |_| {
                    Ok(Some(
                        json!({"status":"error","errorMessage":"PUBLIC stored error"}),
                    ))
                })
                .unwrap();
        });
        let mut output = vec![];
        assert_eq!(
            attach(
                &store,
                &id,
                &AttachView {
                    model: Some("PUBLIC-a"),
                    hide_prompt: true,
                    render: false,
                },
                &Environment::new(),
                &CancellationToken::new(),
                &mut output
            )
            .await
            .unwrap(),
            0
        );
        writer.await.unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("PUBLIC final answer").count(), 1);
        assert!(output.contains("PUBLIC stored error"));
        assert!(!output.contains("PUBLIC attached session"));
    }
    #[tokio::test]
    async fn cancelling_attachment_leaves_a_live_worker_and_session_unchanged() {
        let (_home, store, id) = fixture();
        let owner = ProcessIdentity::current().unwrap();
        store.update(&id,|_|Ok(Some(json!({"status":"running","lifecycle":{"detached":true,"processIdentity":owner,"workerPid":owner.pid,"workerProcessStartedAtMs":owner.started_at_ms}})))).unwrap();
        let before = std::fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let signal = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        assert_eq!(
            attach(
                &store,
                &id,
                &AttachView {
                    model: None,
                    hide_prompt: false,
                    render: false,
                },
                &Environment::new(),
                &cancel,
                &mut vec![]
            )
            .await
            .unwrap(),
            130
        );
        signal.await.unwrap();
        assert_eq!(
            std::fs::read(store.directory(&id).unwrap().join("meta.json")).unwrap(),
            before
        );
        assert_eq!(owner.liveness(), Liveness::Alive);
    }
    #[test]
    fn dead_worker_settlement_preserves_dispatch_and_a_winning_model_or_replaced_owner() {
        for case in ["settle", "model", "owner", "cancel"] {
            let (_home, store, id) = fixture();
            let mut dead = ProcessIdentity::current().unwrap();
            dead.pid = u32::MAX;
            dead.started_at_ms = 1.0;
            assert_eq!(dead.liveness(), Liveness::Dead);
            let saved=store.update(&id,|_|Ok(Some(json!({"status":"running","lifecycle":{"detached":true,"processIdentity":dead,"workerPid":dead.pid,"workerProcessStartedAtMs":dead.started_at_ms},"dispatch":{"state":"uncertain","PUBLIC":"preserve"},"response":{"id":"PUBLIC-response","PUBLIC":"preserve"}})))).unwrap();
            let cancel = CancellationToken::new();
            match case {
                "model" => {
                    store
                        .update_model(
                            &id,
                            "PUBLIC-a",
                            &json!({"status":"completed","PUBLIC":"winner"}),
                        )
                        .unwrap();
                }
                "owner" => {
                    let owner = ProcessIdentity::current().unwrap();
                    store.update(&id,|_|Ok(Some(json!({"lifecycle":{"detached":true,"processIdentity":owner,"workerPid":owner.pid,"workerProcessStartedAtMs":owner.started_at_ms}})))).unwrap();
                }
                "cancel" => cancel.cancel(),
                _ => {}
            }
            let before = store.stored_for_update(&id).unwrap();
            assert_eq!(
                settle_dead(&store, &id, &saved, &cancel).unwrap(),
                case == "settle"
            );
            let current = store.stored_for_update(&id).unwrap();
            if case == "settle" {
                assert_eq!(current["status"], "error");
                assert_eq!(current["dispatch"], saved["dispatch"]);
                assert_eq!(current["response"]["id"], "PUBLIC-response");
                assert_eq!(current["response"]["PUBLIC"], "preserve");
                assert_eq!(current["recoveryRequired"], true);
            } else {
                assert_eq!(current, before);
            }
        }
    }
    #[test]
    fn legacy_recovery_requires_a_dead_recorded_controller_and_recoverable_browser_state() {
        let meta = json!({"mode":"browser","status":"running","lifecycle":{"workerPid":u32::MAX},"browser":{"runtime":{"controllerPid":u32::MAX,"chromePort":9222}}});
        assert!(legacy_recovery(&meta));
        for case in [
            "live",
            "malformed",
            "native-null",
            "submission-null",
            "terminal",
            "api",
            "no-endpoint",
            "unqualified-error",
        ] {
            let mut changed = meta.clone();
            match case {
                "live" => changed["lifecycle"]["workerPid"] = json!(std::process::id()),
                "malformed" => changed["browser"]["runtime"]["controllerPid"] = Value::Null,
                "native-null" => changed["lifecycle"]["processIdentity"] = Value::Null,
                "submission-null" => changed["browser"]["runtime"]["submissionOwner"] = Value::Null,
                "terminal" => changed["status"] = json!("completed"),
                "api" => changed["mode"] = json!("api"),
                "no-endpoint" => {
                    changed["browser"]["runtime"]
                        .as_object_mut()
                        .unwrap()
                        .remove("chromePort");
                }
                _ => changed["status"] = json!("error"),
            }
            assert!(!legacy_recovery(&changed), "{case}");
        }
        for reason in ["chrome-disconnected", "incomplete-capture"] {
            let mut changed = meta.clone();
            changed["status"] = json!("error");
            changed["response"] = json!({"incompleteReason":reason});
            assert!(legacy_recovery(&changed));
        }
    }
    #[test]
    fn legacy_settlement_requires_detached_positive_absence_and_preserves_competing_outcomes() {
        for case in [
            "settle",
            "foreground",
            "owner",
            "model",
            "cancel",
            "native-null",
            "submission-null",
            "runtime",
        ] {
            let (_home, store, id) = fixture();
            let saved=store.update(&id,|_|Ok(Some(json!({"status":"running","lifecycle":{"detached":case!="foreground","workerPid":u32::MAX},"dispatch":{"state":"uncertain","PUBLIC":"retain"},"response":{"id":"PUBLIC-response","PUBLIC":"retain"}})))).unwrap();
            let token = CancellationToken::new();
            match case {
                "owner" => {
                    store.update(&id,|_|Ok(Some(json!({"lifecycle":{"detached":true,"workerPid":std::process::id()}})))).unwrap();
                }
                "model" => {
                    store
                        .update_model(
                            &id,
                            "PUBLIC-a",
                            &json!({"status":"completed","PUBLIC":"winner"}),
                        )
                        .unwrap();
                }
                "cancel" => token.cancel(),
                "native-null" => {
                    store.update(&id,|_|Ok(Some(json!({"lifecycle":{"detached":true,"workerPid":u32::MAX,"processIdentity":null}})))).unwrap();
                }
                "submission-null" => {
                    store
                        .update(&id, |_| {
                            Ok(Some(
                                json!({"browser":{"runtime":{"submissionOwner":null}}}),
                            ))
                        })
                        .unwrap();
                }
                "runtime" => {
                    store.update(&id,|_|Ok(Some(json!({"browser":{"runtime":{"chromeTargetId":"PUBLIC-new-target"}}})))).unwrap();
                }
                _ => {}
            }
            let before = store.stored_for_update(&id).unwrap();
            assert_eq!(
                settle_dead(&store, &id, &saved, &token).unwrap(),
                case == "settle",
                "{case}"
            );
            let current = store.stored_for_update(&id).unwrap();
            if case == "settle" {
                assert_eq!(current["status"], "error");
                assert_eq!(current["dispatch"], saved["dispatch"]);
                assert_eq!(current["response"]["id"], "PUBLIC-response");
                assert_eq!(current["response"]["PUBLIC"], "retain");
            } else {
                assert_eq!(current, before, "{case}");
            }
        }
    }
}

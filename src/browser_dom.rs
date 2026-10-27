//! Prompt and upload operations use retained renderer guards and serialised Rust arguments.
use crate::{
    browser::{conversation_id, provider_url},
    browser_renderer::RendererReservation,
    native_fs::NativeEntry,
    storage::private_file,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
const PREPARE: &str = include_str!("../browser-scripts/prompt-prepare.js");
const CHECK: &str = include_str!("../browser-scripts/prompt-check.js");
const READY: &str = include_str!("../browser-scripts/attachment-ready.js");
const EVIDENCE: &str = include_str!("../browser-scripts/attachment-evidence.js");
const RESET_UPLOADS: &str = include_str!("../browser-scripts/upload-reset.js");
const FILE_PREPARE: &str = include_str!("../browser-scripts/file-input-prepare.js");
const SEND_FOCUS: &str = include_str!("../browser-scripts/send-focus.js");
const SEND_PREPARE: &str = include_str!("../browser-scripts/send-prepare.js");
const WITNESS_CLEAN: &str = "(cfg) => { /* alcuin-dom:submission-witness-clean */ const record=window.__oracleSubmissionWitness; if(!record)return true; if(record.witnessId!==cfg.witnessId || record.dispatchGuardId!==cfg.dispatchGuardId || record.owner.claimId!==cfg.owner.claimId || record.owner.reservationId!==cfg.owner.reservationId) return false; if(typeof record.stop==='function')return cfg.retain===true ? record.stop() : record.cleanup(); if(cfg.retain!==true)delete window.__oracleSubmissionWitness; return true; }";
const INSTALL: &str = include_str!("../browser-scripts/completion-install.js");
// Reference buildChatListRateLimitExpression: read-only and advisory.
const CHAT_LIST_RATE_LIMIT: &str = r#"(cfg) => { /* alcuin-dom:chat-list-rate-limit */
    // A resumed conversation may still be usable while the unrelated sidebar list is limited.
    if (/\/c\/[^/]+/.test(location.pathname || '')) return false;
    const chatListUnavailable = Array.from(document.querySelectorAll('[role="status"]')).some((node) =>
      /loading chats|unable to load history|チャット.*読み込|履歴.*読み込/i.test((node.textContent || '').trim())
    );
    if (!chatListUnavailable) return false;
    if (typeof performance === 'undefined' || typeof performance.getEntriesByType !== 'function') return false;
    return performance.getEntriesByType('resource')
      .filter((entry) => {
        try { return new URL(entry.name, location.href).pathname === '/backend-api/conversations'; }
        catch { return false; }
      })
      .slice(-5)
      .some((entry) => entry.responseStatus === 429);
}"#;
const NAVIGATION: &str = include_str!("../browser-scripts/navigation.js");
// Uploads run before prompt staging, so the composer '+' scripts bind to the visible
// composer form that owns the prompt editor rather than to a staged prompt guard.
const ATTACHMENT_PLUS_CONTEXT: &str = r#"
const searchVisible = node => {
  if (!(node instanceof HTMLElement) || !node.isConnected) return false;
  const rect = node.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return false;
  for (let current = node; current; current = current.parentElement) {
    const style = getComputedStyle(current);
    if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0' || current.getAttribute('aria-hidden') === 'true') return false;
  }
  return true;
};
const searchContext = () => {
  const editor = document.querySelector('#prompt-textarea, form[data-chatgpt-composer] [contenteditable="true"][role="textbox"]');
  const form = editor?.closest('form');
  return editor && form && searchVisible(editor) ? { editor, form } : null;
};
const searchMenuRoots = () => Array.from(document.querySelectorAll('main .popover, [data-radix-popper-content-wrapper], [data-floating-ui-portal], [role="menu"], [role="listbox"], [class*="ComposerTopMenuShell"]')).filter(searchVisible);
const searchBinding = () => {
  const binding = window.__oracleWebSearchContexts?.[cfg.plusGuardId];
  const context = searchContext();
  return binding && context && binding.editor === context.editor && binding.form === context.form &&
    searchVisible(binding.plus) && binding.plus.closest('form') === context.form ? binding : null;
};
"#;
const ATTACHMENT_PLUS: crate::browser_search::PlusMenu = crate::browser_search::PlusMenu {
    context: ATTACHMENT_PLUS_CONTEXT,
    label: "Attachment",
    navigation: attachment_navigation,
    focus: "Attachment menu focus changed before activation; upload was stopped.",
    uncertain: "Attachment menu activation could not be verified; do not retry automatically.",
    blocked: |state| {
        attachment_navigation(state).err().unwrap_or_else(|| {
            anyhow::anyhow!("Attachment menu focus changed at activation; upload was stopped.")
        })
    },
    unverified: "Attachment menu activation could not be verified; do not retry automatically.",
};
/// Reference assertComposerNavigationSnapshot wording for attachment preparation.
fn attachment_navigation(state: &Value) -> Result<()> {
    anyhow::ensure!(
        state["workSelected"] != true
            && (state["contextMatches"] == true || state["modeUnverified"] == true),
        "ChatGPT navigated to Work or another ChatGPT context during attachment preparation; upload/send was stopped before prompt submission."
    );
    anyhow::ensure!(
        state["modeUnverified"] != true,
        "ChatGPT mode could not be verified; attachment upload/send was stopped."
    );
    Ok(())
}
/// Reference activateComposerPlus: press the composer '+' control once so ChatGPT mounts its
/// attachment input, then confirm the page stayed in place. False when no single control exists.
async fn open_attachment_menu(
    reservation: &mut RendererReservation,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    use crate::browser_search::{NAVIGATION, PROBE, evaluate_in, press_plus};
    let probe = evaluate_in(&ATTACHMENT_PLUS, reservation, PROBE, cfg, timeout, cancel).await?;
    if probe["status"] == "context-changed" && probe["navigation"].is_object() {
        attachment_navigation(&probe["navigation"])?;
    }
    if probe["status"] != "focused" {
        return Ok(false);
    }
    press_plus(&ATTACHMENT_PLUS, reservation, cfg, timeout, cancel).await?;
    tokio::select! { biased; _ = cancel.cancelled() => bail!("Upload cancelled; ownership retained"), _ = tokio::time::sleep(Duration::from_millis(350)) => {} }
    attachment_navigation(
        &evaluate_in(
            &ATTACHMENT_PLUS,
            reservation,
            NAVIGATION,
            cfg,
            timeout,
            cancel,
        )
        .await?,
    )?;
    Ok(true)
}
const INSERT: &str = "(cfg) => { const guard = window.__oracleAttachmentPromptGuards?.[cfg.guardId]; if (!guard) return {ready:false}; guard.insert(cfg.prompt); return guard.summary(); }";
const EDIT: &str = "(cfg) => { const guard = window.__oracleAttachmentPromptGuards?.[cfg.guardId]; if (!guard) return {ready:false}; const before = guard.summary(); if (before.ready && !before.length) guard.fallback(cfg.prompt); return guard.summary(); }";
/// Reference stageAttachmentPrompt waits this long after inserting: ChatGPT's editor renders a blank
/// line as an extra bare break until it re-renders.
pub(crate) const EDITOR_SETTLE: Duration = Duration::from_millis(500);
const SEAL: &str = "(cfg) => window.__oracleAttachmentPromptGuards?.[cfg.guardId]?.seal?.()";
const PROMPT_CLEAN: &str = "(cfg) => { const guards=window.__oracleAttachmentPromptGuards; const guard=guards?.[cfg.guardId]; if (!guard) return {cleaned:false}; guard.cleanup(); delete guards[cfg.guardId]; return {cleaned:true}; }";
const PROMPT_ROLLBACK: &str = "(cfg) => { /* alcuin-dom:prompt-rollback */ return window.__oracleAttachmentPromptGuards?.[cfg.guardId]?.rollback?.() ?? {restored:false}; }";
const SEND_CLEAN: &str = "(cfg) => { const guard=window.__oracleAttachmentDispatchGuard; if (guard?.id !== cfg.dispatchGuardId) return {cleaned:false}; const summary={cleaned:true,sawKeyDown:guard.sawKeyDown,sawClick:guard.sawClick,admitted:guard.admitted,blocked:guard.blocked}; guard.cleanup(); return summary; }";
const FILE_CLEAN: &str = r#"(cfg) => { /* alcuin-dom:file-input-clean */
    const records=window.__oracleFileInputGuards;
    const record=records?.[cfg.id];
    if(!record) {
      if(cfg.abandon===true)window.__oracleAttachmentEvidence?.delete(cfg.id);
      return {cleaned:false,blocked:'upload-record-missing'};
    }
    if(record.id!==cfg.id || record.owner?.claimId!==cfg.owner.claimId || record.owner?.reservationId!==cfg.owner.reservationId)
      return {cleaned:false,blocked:'upload-record-mismatch'};
    const valid=record.guard.validate();
    record.guard.cleanup();
    const result={id:record.id,owner:record.owner,navigation:record.navigation,abandon:cfg.abandon===true,
      cleaned:valid,blocked:record.guard.blocked,rollbackComplete:record.guard.rollbackComplete};
    delete records[cfg.id];
    if(cfg.abandon===true){const receipts=window.__oracleAttachmentEvidence;receipts?.delete(cfg.id);result.evidenceCleared=!receipts?.has(cfg.id);}
    return result;
}"#;
const FILE_SEAL: &str = "(cfg) => { /* alcuin-dom:file-input-seal */ const receipts=window.__oracleAttachmentEvidence; const record=window.__oracleFileInputGuards?.[cfg.id]; const receipt=receipts?.get(cfg.id); if (!record || !record.guard.validate() || !receipt?.composer?.isConnected || !receipt.control?.isConnected || receipt.document!==document || receipt.owner.claimId!==cfg.owner.claimId || receipt.owner.reservationId!==cfg.owner.reservationId || !(record.input instanceof HTMLInputElement) || !record.input.isConnected || !receipt.composer.contains(record.input)) throw Error('Original upload input changed'); const files=record.guard.sealFiles(); receipt.input=record.input; for(const previous of receipts.values()) if(previous.input===record.input && previous.composer===receipt.composer && previous.document===document && previous.owner.claimId===cfg.owner.claimId && previous.owner.reservationId===cfg.owner.reservationId) previous.inputFiles=files; return true; }";
pub const SEND_SELECTORS: &[&str] = &[
    "button[data-testid=\"send-button\"]",
    "button[data-testid*=\"composer-send\"]",
    "form button[type=\"submit\"]",
    "button[type=\"submit\"][data-testid*=\"send\"]",
    "button[aria-label*=\"Send\"]",
];

pub fn navigation(raw: &str) -> Result<Value> {
    let url = provider_url(raw).context("Unqualified prompt navigation origin")?;
    anyhow::ensure!(
        url.host_str() == Some("chatgpt.com"),
        "Owned dispatch requires the maintained ChatGPT origin"
    );
    let id = conversation_id(raw);
    let path = if id.is_none() {
        let path = url.path().trim_end_matches('/');
        Some(if path.is_empty() { "/" } else { path })
    } else {
        None
    };
    Ok(json!({"origin":url.origin().ascii_serialization(),"conversationId":id,"landingPath":path}))
}
/// True when the page has ChatGPT Work mode selected; probed before any renderer claim.
pub async fn work_mode_selected(
    page: &crate::cdp::Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    let probe = page
        .evaluate_with_arguments(NAVIGATION, &json!([{}]), timeout, cancel)
        .await?;
    Ok(probe["workSelected"] == true)
}
pub async fn capture_navigation(
    reservation: &RendererReservation,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(String, Value)> {
    let probe = reservation
        .page()
        .evaluate_with_arguments(
            &crate::browser_privacy::guard(NAVIGATION),
            &json!([{}]),
            timeout,
            cancel,
        )
        .await?;
    anyhow::ensure!(
        probe["workSelected"] == false && probe["modeUnverified"] == false,
        "Work mode or unverified composer context cannot receive a consultation"
    );
    let raw = probe["currentUrl"]
        .as_str()
        .context("Missing composer navigation")?
        .to_owned();
    let identity = navigation(&raw)?;
    Ok((raw, identity))
}
/// Initial navigation reaches its URL before ChatGPT mounts its Temporary Chat
/// controls. Wait using observations only, before any selection or payload entry.
pub(crate) async fn wait_initial_navigation(
    reservation: &RendererReservation,
    expected: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(String, Value)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !left.is_zero(),
            "Initial composer readiness deadline reached"
        );
        let current = reservation
            .page()
            .evaluate("location.href", left, cancel)
            .await?;
        anyhow::ensure!(
            current
                .as_str()
                .is_some_and(|url| navigation(url).is_ok_and(|nav| nav == *expected)),
            "Initial ChatGPT navigation changed before composer readiness"
        );
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !left.is_zero(),
            "Initial composer readiness deadline reached"
        );
        match capture_navigation(reservation, left, cancel).await {
            Ok(captured) => {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Initial composer readiness completed after its deadline"
                );
                anyhow::ensure!(
                    captured.1 == *expected,
                    "Initial ChatGPT navigation changed whilst checking its composer"
                );
                return Ok(captured);
            }
            Err(error) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || cancel.is_cancelled() {
                    return Err(error).context("Initial Temporary Chat composer did not become verifiable before the input deadline");
                }
                tokio::select! {
                    _ = cancel.cancelled() => bail!("Initial composer readiness cancelled; nothing was sent"),
                    _ = tokio::time::sleep(left.min(Duration::from_millis(100))) => {}
                }
            }
        }
    }
}
/// Reference warnIfChatListRateLimited probe. A failed probe reads as not limited.
pub(crate) async fn chat_list_rate_limited(
    reservation: &RendererReservation,
    timeout: Duration,
    cancel: &CancellationToken,
) -> bool {
    reservation
        .page()
        .evaluate_with_arguments(
            CHAT_LIST_RATE_LIMIT,
            &json!([{}]),
            timeout.min(Duration::from_secs(2)),
            cancel,
        )
        .await
        .is_ok_and(|limited| limited == true)
}
pub struct PreparedPrompt {
    cfg: Value,
}
#[derive(Debug)]
pub struct PromptTooLarge {
    pub prompt_length: usize,
    pub observed_length: u64,
}
impl std::fmt::Display for PromptTooLarge {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("Prompt appears truncated in the composer (likely too large).")
    }
}
impl std::error::Error for PromptTooLarge {}
impl PreparedPrompt {
    pub async fn stage(
        reservation: &mut RendererReservation,
        prompt: &str,
        nav: &Value,
        expectations: &[Value],
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        reservation.seal_retirement();
        let canonical = prompt.replace("\r\n", "\n").replace('\r', "\n");
        let cfg = json!({"prompt":prompt,"promptSha256":crate::artifacts::sha256(canonical.as_bytes()),"witnessId":Uuid::new_v4().to_string(),"navigation":nav,"guardId":Uuid::new_v4().to_string(),"dispatchGuardId":Uuid::new_v4().to_string(),"expectations":expectations,"sendSelectors":SEND_SELECTORS,"admit":false});
        let staged = Self { cfg: cfg.clone() };
        let result = async {
            let prepared = reservation
                .evaluate_mutation(PREPARE, cfg.clone(), timeout, cancel)
                .await?;
            require_prepared(&prepared, false)?;
            let inserted = reservation
                .evaluate_mutation(INSERT, cfg.clone(), timeout, cancel)
                .await?;
            require_prepared(&inserted, false)?;
            tokio::select! {
                () = tokio::time::sleep(EDITOR_SETTLE) => {}
                () = cancel.cancelled() => bail!("Prompt preparation was cancelled; nothing was sent"),
            }
            let edited = reservation
                .evaluate_mutation(EDIT, cfg.clone(), timeout, cancel)
                .await?;
            require_prepared(&edited, false)?;
            anyhow::ensure!(
                edited["sawInput"] == true,
                "Prompt input was not observed; nothing was sent"
            );
            let length = prompt.encode_utf16().count();
            if edited["complete"] != true
                && length >= 50_000
                && edited["length"]
                    .as_u64()
                    .is_some_and(|observed| observed > 0 && observed < (length - 2_000) as u64)
            {
                anyhow::ensure!(
                    reservation.state() == crate::browser_renderer::DispatchState::NotStarted,
                    "Truncated prompt cannot authorise fallback after dispatch starts"
                );
                return Err(PromptTooLarge {
                    prompt_length: length,
                    observed_length: edited["length"]
                        .as_u64()
                        .context("Missing observed prompt length")?,
                }
                .into());
            }
            require_prepared(&edited, true)?;
            let sealed = reservation
                .evaluate_mutation(SEAL, cfg.clone(), timeout, cancel)
                .await?;
            require_prepared(&sealed, true)?;
            anyhow::ensure!(
                sealed["guardId"] == cfg["guardId"]
                    && sealed["listenersDetached"] == true
                    && sealed["guardRetained"] == true,
                "Prompt preparation sealing was not acknowledged"
            );
            reservation
                .evaluate_mutation(INSTALL, cfg.clone(), timeout, cancel)
                .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let rollback = if error.downcast_ref::<PromptTooLarge>().is_some() {
                reservation
                    .evaluate_mutation(PROMPT_ROLLBACK, cfg, timeout, cancel)
                    .await
                    .map(|v| v["restored"] == true)
            } else {
                Ok(true)
            };
            if let Err(cleanup) = staged.cleanup(reservation, Duration::from_secs(1)).await {
                let _ = reservation
                    .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
                    .await;
                bail!(
                    "Prompt preparation failed ({error}); exact cleanup is uncertain ({cleanup})"
                );
            }
            if !matches!(rollback, Ok(true)) {
                let _ = reservation
                    .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
                    .await;
                bail!(
                    "Truncated prompt rollback is uncertain; fallback is refused and recovery retained"
                );
            }
            return Err(error);
        }
        Ok(staged)
    }
    pub(crate) async fn activate_search(
        &mut self,
        reservation: &mut RendererReservation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        self.check(reservation, timeout, cancel).await?;
        let outcome =
            crate::browser_search::activate(reservation, self.cfg.clone(), timeout, cancel).await?;
        self.check(reservation, timeout, cancel).await?;
        self.expect_web_search();
        Ok(outcome)
    }
    /// The send collector accepts the provider's inline Web search form only after this.
    pub(crate) fn expect_web_search(&mut self) {
        self.cfg["webSearch"] = json!(true);
    }
    pub(crate) async fn activate_research(
        &self,
        reservation: &mut RendererReservation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        self.check(reservation, timeout, cancel).await?;
        let outcome =
            crate::browser_research::activate(reservation, self.cfg.clone(), timeout, cancel)
                .await?;
        self.check(reservation, timeout, cancel).await?;
        Ok(outcome)
    }
    pub async fn check(
        &self,
        reservation: &mut RendererReservation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let value = reservation
            .evaluate_mutation(CHECK, self.cfg.clone(), timeout, cancel)
            .await?;
        require_prepared(&value, true)
    }
    /// Node sendButtonTimeoutMs: 20 s for text; with attachments, attachmentTimeoutMs (at
    /// least 1 s) or 45 s.
    pub(crate) fn send_budget(&self, config: &Value) -> Duration {
        if !self.has_attachments() {
            return Duration::from_secs(20);
        }
        config["attachmentTimeoutMs"]
            .as_u64()
            .map_or(Duration::from_secs(45), |ms| {
                Duration::from_millis(ms.max(1_000))
            })
    }
    fn has_attachments(&self) -> bool {
        self.cfg["expectations"]
            .as_array()
            .is_some_and(|expectations| !expectations.is_empty())
    }
    /// Waits up to `budget` for the exact send control to become ready, as the reference
    /// readiness loop does, then arms it. Nothing is dispatched here.
    pub async fn arm_send(
        &self,
        reservation: &mut RendererReservation,
        budget: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let deadline = Instant::now() + budget;
        let call = |deadline: Instant| {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1))
                .min(budget)
        };
        self.check(reservation, call(deadline), cancel).await?;
        let attachments = self.has_attachments();
        let mut found = false;
        loop {
            let focus = reservation
                .evaluate_mutation(SEND_FOCUS, self.cfg.clone(), call(deadline), cancel)
                .await?;
            match focus["status"].as_str() {
                Some("focused") => break,
                Some("unavailable") => found = true,
                Some("absent") if attachments => {}
                _ => bail!("Exact send control is not ready; refusing dispatch"),
            }
            if Instant::now() >= deadline {
                if attachments {
                    bail!(
                        "Attachments never reached the exact ready send button after {}s; tune --browser-attachment-timeout.",
                        budget.as_secs_f64().ceil()
                    );
                }
                anyhow::ensure!(!found, "Send button did not settle before input timeout.");
                bail!("Exact send control is not ready; refusing dispatch");
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("Send readiness was cancelled before dispatch"),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
        let armed = reservation
            .evaluate_mutation(SEND_PREPARE, self.cfg.clone(), call(deadline), cancel)
            .await?;
        anyhow::ensure!(
            armed["guardInstalled"] == true
                && armed["focused"] == true
                && armed["attachmentsReady"] == true
                && armed["promptReady"] == true
                && armed["preparationReady"] == true
                && armed["contextMatches"] == true,
            "Original prompt, upload readiness or send ownership changed"
        );
        Ok(())
    }
    pub(crate) fn submission_intent(&self) -> crate::browser_commit::SubmissionIntent {
        crate::browser_commit::SubmissionIntent {
            witness_id: self.cfg["witnessId"]
                .as_str()
                .expect("prepared witness identity")
                .into(),
            dispatch_guard_id: self.cfg["dispatchGuardId"]
                .as_str()
                .expect("prepared dispatch identity")
                .into(),
            web_search: self.cfg["webSearch"] == true,
            prompt_sha256: self.cfg["promptSha256"]
                .as_str()
                .expect("prepared source identity")
                .into(),
        }
    }
    pub(crate) async fn cleanup_witness(
        &self,
        reservation: &RendererReservation,
        retain: bool,
        timeout: Duration,
    ) -> Result<()> {
        let mut cfg = self.cfg.clone();
        cfg["owner"] = json!(reservation.owner());
        cfg["retain"] = json!(retain);
        let cleaned = reservation
            .page()
            .evaluate_with_arguments(
                WITNESS_CLEAN,
                &json!([cfg]),
                timeout,
                &CancellationToken::new(),
            )
            .await?;
        anyhow::ensure!(
            cleaned == true,
            "Submission witness cleanup is uncertain; retain exact target"
        );
        Ok(())
    }
    pub async fn send_once(
        &self,
        reservation: &mut RendererReservation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        reservation.consume_input_send()?;
        let key = json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13,"text":"\r","unmodifiedText":"\r"});
        let down = reservation
            .page()
            .call("Input.dispatchKeyEvent", key, timeout, cancel)
            .await;
        // Always settle the matching key release, including after lost key-down acknowledgement.
        let up = reservation.page().call("Input.dispatchKeyEvent", json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13}), Duration::from_secs(1), &CancellationToken::new()).await;
        let clean = reservation
            .page()
            .evaluate_with_arguments(
                SEND_CLEAN,
                &json!([self.cfg]),
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await;
        let clean = clean.context("Send guard cleanup is uncertain")?;
        if down.is_err()
            || up.is_err()
            || cancel.is_cancelled()
            || clean["cleaned"] != true
            || clean["admitted"] != true
            || clean["sawKeyDown"] != true
            || clean["sawClick"] != true
            || !clean["blocked"].is_null()
        {
            let _ = reservation
                .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
                .await;
            bail!(
                "Send acknowledgement or exact input cleanup is uncertain; preserve recovery and never replay automatically"
            );
        }
        Ok(())
    }
    pub async fn cleanup(
        &self,
        reservation: &mut RendererReservation,
        timeout: Duration,
    ) -> Result<()> {
        let raw = reservation
            .page()
            .evaluate_with_arguments(
                PROMPT_CLEAN,
                &json!([self.cfg]),
                timeout,
                &CancellationToken::new(),
            )
            .await?;
        anyhow::ensure!(
            raw["cleaned"] == true,
            "Prompt preparation cleanup was not positively acknowledged"
        );
        Ok(())
    }
}
fn require_prepared(value: &Value, complete: bool) -> Result<()> {
    anyhow::ensure!(
        value["ready"] == true
            && value["blocked"].is_null()
            && value["convertedToFile"] != true
            && (!complete || value["complete"] == true),
        "Complete prompt is unavailable in its original owned editor"
    );
    Ok(())
}
pub struct UploadFile {
    path: PathBuf,
    entry: NativeEntry,
    digest: String,
    bytes: u64,
    pub name: String,
    pub generated_bundle: bool,
    /// The browser host cannot read this machine's paths, so the bytes go through the page.
    pub data_transfer: bool,
}
impl UploadFile {
    pub fn open(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path)?;
        let mut entry = NativeEntry::open(&path, false)?;
        let bytes = entry.handle.metadata()?.len();
        let digest = upload_digest(&mut entry.handle, bytes)?;
        entry.require_matches(&path)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("Upload name must use Unicode")?
            .to_owned();
        Ok(Self {
            path,
            entry,
            digest,
            bytes,
            name,
            generated_bundle: false,
            data_transfer: false,
        })
    }
    pub fn manifest(&self) -> Value {
        json!({"path":self.path,"name":self.name,"bytes":self.bytes,"sha256":self.digest,"nativeIdentity":self.entry.identity})
    }
    pub fn verify(&mut self) -> Result<()> {
        self.entry.require_matches(&self.path)?;
        self.entry.handle.seek(SeekFrom::Start(0))?;
        anyhow::ensure!(
            upload_digest(&mut self.entry.handle, self.bytes)? == self.digest,
            "Upload source bytes or native identity changed"
        );
        self.entry.require_matches(&self.path)?;
        Ok(())
    }
    pub fn require_manifest(&mut self, expected: &Value) -> Result<()> {
        self.verify()?;
        anyhow::ensure!(
            self.manifest() == *expected,
            "Staged upload receipt differs from its saved source"
        );
        Ok(())
    }
    pub fn snapshot(&mut self, destination: &Path) -> Result<Self> {
        self.verify()?;
        self.entry.handle.seek(SeekFrom::Start(0))?;
        let mut saved = private_file(destination, true)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut self.entry.handle)
                .take(self.bytes.checked_add(1).context("Upload size overflow")?),
            &mut saved,
        )?;
        saved.sync_all()?;
        drop(saved);
        self.entry.require_matches(&self.path)?;
        let captured = Self::open(destination)?;
        anyhow::ensure!(
            copied == self.bytes && captured.bytes == self.bytes && captured.digest == self.digest,
            "Upload changed while staging its durable copy"
        );
        Ok(captured)
    }
    pub fn contents(&mut self) -> Result<Vec<u8>> {
        self.verify()?;
        self.entry.handle.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.entry.handle)
            .take(self.bytes.checked_add(1).context("Upload size overflow")?)
            .read_to_end(&mut bytes)?;
        self.entry.require_matches(&self.path)?;
        anyhow::ensure!(
            bytes.len() as u64 == self.bytes && crate::artifacts::sha256(&bytes) == self.digest,
            "Staged upload changed while reading its saved content"
        );
        Ok(bytes)
    }
}
fn upload_digest(handle: &mut fs::File, expected: u64) -> Result<String> {
    let mut reader =
        Read::by_ref(handle).take(expected.checked_add(1).context("Upload size overflow")?);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        hash.update(&buffer[..count]);
    }
    anyhow::ensure!(total == expected, "Upload changed during capture");
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
pub fn attachment_expectation(name: &str) -> Value {
    attachment_expectation_for(name, false)
}
pub fn attachment_expectation_for(name: &str, generated_bundle: bool) -> Value {
    let name = name
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let escape = |s: &str| {
        s.chars()
            .map(|c| {
                if ".*+?^${}()|[]\\".contains(c) {
                    format!("\\{c}")
                } else {
                    c.to_string()
                }
            })
            .collect::<String>()
    };
    let chars = r"\p{L}\p{N}\p{M}._-";
    let boundary = format!("(?=$|[^{chars}])");
    let pattern = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => format!(
            "{}(?:\\s*\\([0-9]+(?:-[0-9]+)*\\))?\\.{}{boundary}",
            escape(stem),
            escape(ext)
        ),
        _ => format!("{}{boundary}", escape(&name)),
    };
    let stem = name
        .rsplit_once('.')
        .filter(|(_, ext)| {
            !ext.is_empty() && ext.len() <= 10 && ext.bytes().all(|c| c.is_ascii_alphanumeric())
        })
        .map_or(name.as_str(), |(stem, _)| stem);
    let image = name.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "heif" | "avif"
        )
    });
    json!({"name":name,"stem":stem,"namePattern":format!("(?:^|[^{chars}]){pattern}"),"generatedBundle":generated_bundle,"image":image})
}
pub(crate) fn receipt_expectation(manifest: &Value, generated_bundle: bool) -> Result<Value> {
    let mut expected = attachment_expectation_for(
        manifest["name"]
            .as_str()
            .context("Upload receipt name is missing")?,
        generated_bundle,
    );
    let id = manifest["rendererReceiptId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("Original upload receipt ID is missing")?;
    expected["rendererReceiptId"] = json!(id);
    Ok(expected)
}
const MAX_DATA_TRANSFER_BYTES: u64 = 20 * 1024 * 1024;
/// Raw bytes per chunk. Its base64 form (1 MiB) stays under the 2 MiB CDP request limit.
const DATA_TRANSFER_CHUNK: usize = 768 * 1024;
const DATA_CHUNK: &str = "(cfg) => { /* alcuin-dom:data-transfer-chunk */ const record=window.__oracleFileInputGuards?.[cfg.id]; if (!record || !record.guard.validate()) throw Error('File input ownership changed'); const chunks=(record.chunks ??= []); if (chunks.length!==cfg.index) throw Error('Transfer chunk order changed'); const text=atob(cfg.chunk); const bytes=new Uint8Array(text.length); for (let i=0;i<text.length;i++) bytes[i]=text.charCodeAt(i); chunks.push(bytes); return chunks.length; }";
/// Reference transferAttachmentViaDataTransfer, bound to the guarded input from FILE_PREPARE.
/// It assigns through the native setter only, so the guard can seal and roll back a real FileList.
const DATA_ASSIGN: &str = r#"(cfg) => { /* alcuin-dom:data-transfer-assign */
    if (!('File' in window) || !('Blob' in window) || !('DataTransfer' in window) || typeof atob !== 'function')
      return {success:false,error:'Required file APIs are not available in this browser'};
    const record=window.__oracleFileInputGuards?.[cfg.id];
    if (!record) return {success:false,error:'File input not found'};
    const input=record.input;
    if (!(input instanceof HTMLInputElement) || input.type!=='file') return {success:false,error:'Found element is not a file input'};
    const chunks=record.chunks ?? [];
    delete record.chunks;
    if (!record.guard.validate(true)) return {success:false,navigationBlocked:record.guard.blocked};
    if (chunks.length!==cfg.chunks) return {success:false,error:'Transferred content is incomplete'};
    const file=new File(chunks,cfg.name,{type:cfg.mimeType,lastModified:Date.now()});
    if (file.size!==cfg.fileBytes) return {success:false,error:'Transferred content is incomplete'};
    const transfer=new DataTransfer();
    transfer.items.add(file);
    const setter=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'files')?.set;
    try { if (!setter) throw Error('missing'); setter.call(input,transfer.files); }
    catch { return {success:false,error:'Unable to assign FileList to input'}; }
    if (!record.guard.validate(true)) return {success:false,navigationBlocked:record.guard.blocked};
    input.dispatchEvent(new Event('change',{bubbles:true}));
    if (!record.guard.validate(true)) return {success:false,navigationBlocked:record.guard.blocked};
    return {success:true,fileName:file.name,size:file.size};
}"#;
/// Reference guessMimeType.
fn guess_mime_type(name: &str) -> &'static str {
    let extension = name
        .rfind('.')
        .filter(|index| *index > 0)
        .map_or(String::new(), |index| name[index..].to_lowercase());
    match extension.as_str() {
        ".txt" => "text/plain",
        ".md" => "text/markdown",
        ".csv" => "text/csv",
        ".json" => "application/json",
        ".js" | ".jsx" => "text/javascript",
        ".ts" | ".tsx" => "text/typescript",
        ".py" => "text/x-python",
        ".java" => "text/x-java",
        ".c" | ".h" => "text/x-c",
        ".cpp" | ".hpp" => "text/x-c++",
        ".sh" | ".bash" => "text/x-sh",
        ".html" => "text/html",
        ".css" => "text/css",
        ".xml" => "text/xml",
        ".yaml" | ".yml" => "text/yaml",
        ".pdf" => "application/pdf",
        ".doc" => "application/msword",
        ".docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ".xls" => "application/vnd.ms-excel",
        ".xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ".ppt" => "application/vnd.ms-powerpoint",
        ".pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".svg" => "image/svg+xml",
        ".webp" => "image/webp",
        ".zip" => "application/zip",
        ".tar" => "application/x-tar",
        ".gz" => "application/gzip",
        ".7z" => "application/x-7z-compressed",
        _ => "application/octet-stream",
    }
}
/// Sends the verified bytes in bounded chunks, then assigns them to the guarded input. The
/// caller's confirm, readiness and seal checks apply unchanged; any failure quarantines.
async fn transfer_in_page(
    reservation: &mut RendererReservation,
    file: &mut UploadFile,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let bytes = file.contents()?;
    let chunks = bytes.chunks(DATA_TRANSFER_CHUNK).collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        let sent = reservation
            .evaluate_mutation(
                DATA_CHUNK,
                json!({"id":cfg["id"],"index":index,"chunk":STANDARD.encode(chunk)}),
                timeout,
                cancel,
            )
            .await?;
        anyhow::ensure!(
            sent == json!(index + 1),
            "Failed to transfer file to browser: unexpected evaluation result"
        );
    }
    let assigned = reservation
        .evaluate_mutation(
            DATA_ASSIGN,
            json!({"id":cfg["id"],"chunks":chunks.len(),"name":file.name,"fileBytes":file.bytes,"mimeType":guess_mime_type(&file.name)}),
            timeout,
            cancel,
        )
        .await?;
    anyhow::ensure!(
        assigned.is_object(),
        "Failed to transfer file to browser: unexpected evaluation result"
    );
    if !assigned["navigationBlocked"].is_null() {
        attachment_navigation(&assigned["navigationBlocked"])?;
    }
    anyhow::ensure!(
        assigned["success"] == true
            && assigned["fileName"] == file.name.as_str()
            && assigned["size"] == file.bytes,
        "Failed to transfer file to browser: {}",
        assigned["error"].as_str().unwrap_or("Unknown error")
    );
    file.verify()
}
pub async fn upload(
    reservation: &mut RendererReservation,
    file: &mut UploadFile,
    nav: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    file.verify()?;
    // Reference MAX_DATA_TRANSFER_BYTES, refused here before any page work.
    anyhow::ensure!(
        !file.data_transfer || file.bytes <= MAX_DATA_TRANSFER_BYTES,
        "Attachment {} is too large for data transfer ({} bytes). Maximum size is {MAX_DATA_TRANSFER_BYTES} bytes.",
        file.name,
        file.bytes
    );
    let id = Uuid::new_v4().to_string();
    // ChatGPT mounts camera and media inputs before its general attachment input.
    // Use the unrestricted composer input so text and document files reach upload.
    let cfg = json!({"id":id,"navigation":nav,"selector":"form input[type=\"file\"]:not([accept]),form input[type=\"file\"][accept=\"\"]","names":[file.name],"fileBytes":file.bytes,"action":"begin"});
    reservation.begin_upload_work()?;
    let result = async {
    anyhow::ensure!(
        reservation
            .evaluate_mutation(EVIDENCE, cfg.clone(), timeout, cancel)
            .await?
            == true,
        "Upload ownership baseline was not acknowledged"
    );
    let mut prepared = reservation
        .evaluate_mutation(FILE_PREPARE, cfg.clone(), timeout, cancel)
        .await?;
    // No composer file input is mounted yet: ChatGPT mounts it from the '+' menu.
    if prepared["ready"] == false && prepared.get("blocked").is_none() {
        let mut plus = cfg.clone();
        plus["plusGuardId"] = json!(Uuid::new_v4().to_string());
        if open_attachment_menu(reservation, &plus, timeout, cancel).await? {
            prepared = reservation
                .evaluate_mutation(FILE_PREPARE, cfg.clone(), timeout, cancel)
                .await?;
        }
    }
    anyhow::ensure!(
        prepared["ready"] == true && prepared["blocked"].is_null(),
        "File input is not owned by the original composer"
    );
    if file.data_transfer {
        transfer_in_page(reservation, file, &cfg, timeout, cancel).await?;
    } else {
        let function = "(cfg) => { const record=window.__oracleFileInputGuards?.[cfg.id]; if (!record || !record.guard.validate()) throw Error('File input ownership changed'); return record.input; }";
        let expression = format!("({function})(...{})", serde_json::to_string(&json!([cfg]))?);
        let object = reservation.call_tracked("Runtime.evaluate", json!({"expression":expression,"returnByValue":false}), timeout, cancel).await?;
        anyhow::ensure!(object.get("exceptionDetails").is_none(), "File input renderer guard was refused");
        let object_id = object["result"]["objectId"].as_str().context("Owned file input has no CDP object identity")?;
        let described = reservation.call_tracked("DOM.describeNode", json!({"objectId":object_id}), timeout, cancel).await?;
        let node_id = described["node"]["backendNodeId"].as_u64().filter(|n| *n > 0).context("Owned input has no backend node identity")?;
        file.verify()?;
        reservation.call_tracked("DOM.setFileInputFiles", json!({"backendNodeId":node_id,"files":[file.path]}), timeout, cancel).await?;
    }
        let deadline = Instant::now() + timeout;
        loop {
            let mut confirm = cfg.clone(); confirm["action"] = json!("confirm");
            let owned = reservation.evaluate_mutation(EVIDENCE, confirm, timeout, cancel).await.context("Confirm owned attachment control")? == true;
            if owned {
                let expectation = attachment_expectation_for(&file.name,file.generated_bundle);
                if reservation.evaluate_mutation(READY, json!({"expectations":[expectation],"uploadReceiptId":id}), timeout, cancel).await.context("Verify owned attachment readiness")? == true {
                    anyhow::ensure!(reservation.evaluate_mutation(FILE_SEAL,cfg.clone(),timeout,cancel).await.context("Seal original upload input")? == true,"Original upload input receipt was not sealed");
                    file.verify()?; let mut manifest=file.manifest();manifest["rendererReceiptId"]=json!(id);return Ok(manifest);
                }
            }
            anyhow::ensure!(Instant::now() < deadline, "Owned upload did not reach ready state (new attachment control acknowledged: {owned}); nothing was sent");
            tokio::select! { biased; _ = cancel.cancelled() => bail!("Upload cancelled; ownership retained"), _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
        }
    }.await;
    let cleanup = reservation
        .evaluate_tracked(
            FILE_CLEAN,
            &json!([{ "id":cfg["id"], "owner":reservation.owner(), "navigation":nav, "abandon":result.is_err() }]),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
    let cleaned = cleanup.as_ref().is_ok_and(|v| {
        v["id"] == cfg["id"]
            && v["owner"] == json!(reservation.owner())
            && v["navigation"] == *nav
            && v["abandon"] == result.is_err()
            && v["cleaned"] == true
            && v["rollbackComplete"] == true
            && v["blocked"].is_null()
            && (result.is_ok() || v["evidenceCleared"] == true)
    });
    if !cleaned {
        reservation.mark_input_uncertain();
    }
    if result.is_err() || !cleaned {
        let _ = reservation
            .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await;
        reservation.finish_upload_work(result.is_err() && cleaned);
        if let Err(error) = result {
            if reservation.settled_before_dispatch() {
                bail!("Upload failed before Send and exact cleanup completed: {error:#}");
            }
            bail!(
                "Upload or exact file-input cleanup is uncertain; recovery is retained: {error:#}"
            );
        }
        if let Err(error) = cleanup {
            bail!(
                "Upload or exact file-input cleanup is uncertain; recovery is retained: {error:#}"
            );
        }
        bail!("Upload or exact file-input cleanup is uncertain; recovery is retained");
    }
    reservation.finish_upload_work(false);
    result
}
pub async fn reset_uploads(
    reservation: &mut RendererReservation,
    uploads: &[Value],
    nav: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    reservation.seal_retirement();
    anyhow::ensure!(
        reservation.state() == crate::browser_renderer::DispatchState::NotStarted,
        "Uploaded files cannot be reset after dispatch"
    );
    let (_, current) = capture_navigation(reservation, timeout, cancel).await?;
    anyhow::ensure!(current == *nav, "Upload reset navigation changed");
    let id = Uuid::new_v4().to_string();
    let mut cfg = json!({"id":id,"action":"begin","uploads":uploads,"navigation":nav});
    let deadline = Instant::now() + timeout;
    loop {
        let observed = reservation
            .evaluate_mutation(RESET_UPLOADS, cfg.clone(), timeout, cancel)
            .await?;
        anyhow::ensure!(
            observed.get("blocked").is_none_or(Value::is_null),
            "Owned upload reset was blocked; nothing was sent"
        );
        if observed["complete"] == true {
            anyhow::ensure!(
                observed["removed"].as_u64() == Some(uploads.len() as u64)
                    && observed["originalInputsCleared"] == true,
                "Owned upload reset was not positively acknowledged"
            );
            let (_, current) = capture_navigation(reservation, timeout, cancel).await?;
            anyhow::ensure!(
                current == *nav,
                "Upload reset navigation changed after cleanup"
            );
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Owned attachment removal did not complete before fallback; nothing was sent"
        );
        cfg["action"] = json!("confirm");
        tokio::select! {biased;_=cancel.cancelled()=>bail!("Upload reset cancelled; ownership retained"),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
    }
}
#[cfg(test)]
pub(crate) async fn native_upload_selection_case(
    reservation: &mut RendererReservation,
    changed: bool,
    consumed: bool,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let timeout = Duration::from_secs(2);
    let mut cfg = json!({"id":"PUBLIC-selection-receipt","navigation":navigation("https://chatgpt.com/")?,"selector":"form input[type=file]","names":["PUBLIC-selection.txt"],"fileBytes":3,"action":"begin"});
    anyhow::ensure!(
        reservation
            .evaluate_mutation(EVIDENCE, cfg.clone(), timeout, &cancel)
            .await?
            == true,
        "Selection baseline failed"
    );
    anyhow::ensure!(
        reservation
            .evaluate_mutation(FILE_PREPARE, cfg.clone(), timeout, &cancel)
            .await?["ready"]
            == true,
        "Selection guard failed"
    );
    reservation.page().evaluate_with_arguments("(cfg)=>{const input=document.querySelector('input[type=file]');const files=new DataTransfer();files.items.add(new File(['abc'],'PUBLIC-selection.txt'));input.files=files.files;input.dispatchEvent(new Event('input',{bubbles:true}));input.dispatchEvent(new Event('change',{bubbles:true}));const chip=document.createElement('div');chip.dataset.testid='attachment-chip';chip.innerHTML='<button type=button aria-label=\"Remove file PUBLIC-selection.txt\">Remove</button>';document.querySelector('form').append(chip);return true;}", &json!([{}]), timeout, &cancel).await?;
    cfg["action"] = json!("confirm");
    anyhow::ensure!(
        reservation
            .evaluate_mutation(EVIDENCE, cfg.clone(), timeout, &cancel)
            .await?
            == true,
        "Selection control confirmation failed"
    );
    anyhow::ensure!(reservation.evaluate_mutation(READY, json!({"uploadReceiptId":cfg["id"],"expectations":[attachment_expectation("PUBLIC-selection.txt")]}), timeout, &cancel).await? == true, "Selection did not become ready");
    if changed || consumed {
        reservation.page().evaluate_with_arguments("(cfg)=>{const files=new DataTransfer();if(!cfg.consumed)files.items.add(new File(['abc'],'PUBLIC-selection.txt'));document.querySelector('input[type=file]').files=files.files;return true;}", &json!([{"consumed":consumed}]), timeout, &cancel).await?;
    }
    let sealed = reservation
        .evaluate_mutation(FILE_SEAL, cfg.clone(), timeout, &cancel)
        .await;
    anyhow::ensure!(
        sealed.is_ok() != changed,
        "A changed FileList was adopted after readiness: changed={changed}, consumed={consumed}"
    );
    let cleanup = reservation
        .evaluate_tracked(
            FILE_CLEAN,
            &json!([{"id":cfg["id"],"owner":reservation.owner(),"abandon":true}]),
            timeout,
            &cancel,
        )
        .await?;
    anyhow::ensure!(
        cleanup["cleaned"] == !changed,
        "Selection cleanup result differs"
    );
    Ok(())
}
#[cfg(test)]
mod upload_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn initial_readiness_shares_one_deadline_across_observations() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::{
            WebSocketStream,
            tungstenite::{Message, protocol::Role},
        };

        let (client, server) = tokio::io::duplex(16 * 1024);
        let page = crate::cdp::Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut socket = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let peer = tokio::spawn(async move {
            for observation in 0..3 {
                let Some(Ok(Message::Text(frame))) = socket.next().await else {
                    return;
                };
                let request: Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                let expression = request["params"]["expression"].as_str().unwrap();
                let value = match observation {
                    0 => {
                        assert!(expression.contains("\"action\":\"acquire\""));
                        json!(true)
                    }
                    1 => {
                        assert_eq!(expression, "location.href");
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        json!("https://chatgpt.com/")
                    }
                    _ => {
                        assert!(expression.contains(NAVIGATION));
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        json!({"currentUrl":"https://chatgpt.com/","workSelected":false,"modeUnverified":false})
                    }
                };
                if socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":{"result":{"value":value}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let cancel = CancellationToken::new();
        let reservation = RendererReservation::acquire(
            page,
            "PUBLIC initial readiness claim".into(),
            Duration::from_secs(1),
            &cancel,
        )
        .await
        .unwrap();
        let started = Instant::now();
        let result = wait_initial_navigation(
            &reservation,
            &navigation("https://chatgpt.com/").unwrap(),
            Duration::from_millis(120),
            &cancel,
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() <= Duration::from_millis(120));
        assert_eq!(
            reservation.state(),
            crate::browser_renderer::DispatchState::NotStarted
        );
        reservation
            .page()
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
    }

    #[test]
    fn attachment_menu_refusals_use_the_reference_navigation_wording() {
        let navigated = "ChatGPT navigated to Work or another ChatGPT context during attachment preparation; upload/send was stopped before prompt submission.";
        for (state, expected) in [
            (
                json!({"contextMatches":false,"workSelected":false,"modeUnverified":false}),
                Some(navigated),
            ),
            (
                json!({"contextMatches":false,"workSelected":true,"modeUnverified":false}),
                Some(navigated),
            ),
            (
                json!({"contextMatches":false,"workSelected":false,"modeUnverified":true}),
                Some("ChatGPT mode could not be verified; attachment upload/send was stopped."),
            ),
            (
                json!({"contextMatches":true,"workSelected":false,"modeUnverified":false}),
                None,
            ),
        ] {
            assert_eq!(
                attachment_navigation(&state)
                    .err()
                    .map(|e| e.to_string())
                    .as_deref(),
                expected,
                "{state}"
            );
        }
        let focus = json!({"contextMatches":true,"workSelected":false,"modeUnverified":false,"focused":false});
        assert_eq!(
            (ATTACHMENT_PLUS.blocked)(&focus).to_string(),
            "Attachment menu focus changed at activation; upload was stopped."
        );
    }

    #[test]
    fn streamed_upload_hashing_supports_empty_files_and_sizes_above_the_old_cap() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty.txt");
        fs::write(&empty, []).unwrap();
        let mut file = UploadFile::open(&empty).unwrap();
        file.verify().unwrap();
        assert_eq!(file.manifest()["bytes"], 0);
        let large = root.path().join("public.zip");
        fs::File::create(&large)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        let mut file = UploadFile::open(&large).unwrap();
        file.verify().unwrap();
        assert_eq!(file.manifest()["bytes"], 64 * 1024 * 1024 + 1);
    }
}

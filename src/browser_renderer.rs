//! Renderer reservation and owned GPT-6 picker orchestration. No host JavaScript runtime.
use crate::{
    artifacts::sha256,
    browser_commit::{CommitReceipt, SubmissionOwner},
    cdp::Cdp,
    policy::{self, ModelEvidence, ThinkingEvidence},
    sessions::timestamp,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
const SUBMISSION: &str = include_str!("../browser-scripts/submission.js");
const PREPARE: &str = include_str!("../browser-scripts/inspection-prepare.js");
const FINISH: &str = include_str!("../browser-scripts/inspection-finish.js");
const MODEL: &str = include_str!("../browser-scripts/model.js");
const THINKING: &str = include_str!("../browser-scripts/thinking.js");
/// Node MODEL_BUTTON_WAIT_MS: how long a missing composer pill may take to mount.
const MODEL_BUTTON_WAIT: Duration = Duration::from_secs(20);
/// How long one batch of renderer ownership controls (quarantine, release, picker cleanup) may
/// take before the target is treated as unconfirmed. A follow-up's renderer may still be
/// hydrating the conversation, and on a loaded host these can exceed 1 s.
pub(crate) const CONTROL_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Why an inspection's cleanup is unconfirmed. The page reports only fixed state names, and
/// anything else is withheld so page text never reaches the session error.
fn cleanup_detail(finished: &Result<Value>, owned: bool) -> String {
    let Ok(value) = finished else {
        return "finish did not answer".into();
    };
    if !owned {
        return "finish not acknowledged".into();
    }
    let state = value["cleanupState"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 120
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c == ' ')
        })
        .unwrap_or("unknown");
    format!("still open: {state}")
}
pub const MODEL_BUTTON: &str = "button[aria-label=\"Select ChatGPT model\"], button[data-codex-intelligence-trigger=\"true\"], [data-testid=\"model-switcher-dropdown-button\"], button.__composer-pill[aria-haspopup=\"menu\"]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchState {
    NotStarted,
    Started,
    Acknowledged,
    Quarantined,
    Released,
}
pub struct RendererReservation {
    page: Cdp,
    provider: RendererProvider,
    grok_intent: Option<crate::browser_grok::Intent>,
    owner: SubmissionOwner,
    state: DispatchState,
    input_send_issued: bool,
    inspection: Option<Value>,
    epoch: u64,
    dispatch_not_started: bool,
    selection_sealed: bool,
    retirement_barrier: bool,
    pending_work: u32,
    unconfirmed_work: bool,
    settlement_confirmed: bool,
    /// Picker progress lines for the session log, drained by the runner.
    notes: Vec<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum RendererProvider {
    Chatgpt,
    Grok,
}
// This permit is created only from the exact positively cleaned disabled-Pro inspection.
// It cannot be copied, deserialised, or constructed by another module.
struct ProUnavailablePermit {
    owner: SubmissionOwner,
    epoch: u64,
    inspection: Value,
}
impl RendererReservation {
    pub async fn acquire(
        page: Cdp,
        claim_id: String,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::acquire_provider(page, claim_id, RendererProvider::Chatgpt, timeout, cancel).await
    }
    /// Grok uses the same reservation state machine with separate live privacy
    /// and receipt validators. This does not enable provider fallback by itself.
    pub async fn acquire_grok(
        page: Cdp,
        claim_id: String,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        crate::browser_grok::require(&page, timeout, cancel).await?;
        Self::acquire_provider(page, claim_id, RendererProvider::Grok, timeout, cancel).await
    }
    /// Initial entry is separate from recovery, which must preserve the original
    /// document and never changes privacy or creates a second request.
    pub async fn acquire_initial_grok(
        page: Cdp,
        claim_id: String,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + timeout;
        crate::browser_grok::enter_private(&page, timeout, cancel).await?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Initial Grok reservation deadline reached"
        );
        Self::acquire_grok(page, claim_id, remaining, cancel).await
    }
    async fn acquire_provider(
        page: Cdp,
        claim_id: String,
        provider: RendererProvider,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let mut reservation = Self {
            page,
            provider,
            grok_intent: None,
            owner: SubmissionOwner {
                claim_id,
                reservation_id: Uuid::new_v4().to_string(),
            },
            state: DispatchState::NotStarted,
            input_send_issued: false,
            inspection: None,
            epoch: 0,
            dispatch_not_started: true,
            selection_sealed: false,
            retirement_barrier: false,
            pending_work: 0,
            unconfirmed_work: false,
            settlement_confirmed: false,
            notes: Vec::new(),
        };
        reservation
            .transition("acquire", None, timeout, cancel)
            .await?;
        Ok(reservation)
    }
    pub fn owner(&self) -> &SubmissionOwner {
        &self.owner
    }
    pub fn page(&self) -> &Cdp {
        &self.page
    }
    pub fn state(&self) -> DispatchState {
        self.state
    }
    pub(crate) fn seal_selection(&mut self) {
        self.selection_sealed = true;
    }
    pub(crate) fn seal_retirement(&mut self) {
        self.retirement_barrier = true;
    }
    pub(crate) fn begin_upload_work(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.state == DispatchState::NotStarted && self.dispatch_not_started,
            "Upload cannot begin after dispatch or quarantine"
        );
        self.begin_work()?;
        self.selection_sealed = true;
        Ok(())
    }
    pub(crate) fn finish_upload_work(&mut self, failed_with_exact_cleanup: bool) {
        self.pending_work -= 1;
        // Only a failed upload with finished guard/evidence cleanup may return to the
        // original-worker pre-prompt abandonment boundary. Earlier attachments may
        // remain in this exact no-Send target. A dropped future leaves the scope pending;
        // a successful final upload and any uncertain command keep selection sealed.
        if failed_with_exact_cleanup
            && self.pending_work == 0
            && !self.unconfirmed_work
            && self.dispatch_not_started
            && !self.input_send_issued
            && !self.retirement_barrier
        {
            self.selection_sealed = false;
        }
    }
    pub(crate) fn settled_before_dispatch(&self) -> bool {
        self.dispatch_not_started
            && !self.selection_sealed
            && !self.retirement_barrier
            && !self.input_send_issued
            && self.pending_work == 0
            && !self.unconfirmed_work
            && self.settlement_confirmed
            && matches!(
                self.state,
                DispatchState::Released | DispatchState::Quarantined
            )
    }
    fn begin_work(&mut self) -> Result<()> {
        self.pending_work = self
            .pending_work
            .checked_add(1)
            .context("Renderer work limit reached")?;
        self.settlement_confirmed = false;
        Ok(())
    }
    fn finish_work(&mut self, result: Result<Value>) -> Result<Value> {
        self.pending_work -= 1;
        if result.is_err() {
            self.unconfirmed_work = true;
        }
        result
    }
    pub(crate) async fn call_tracked(
        &mut self,
        method: &str,
        arguments: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.begin_work()?;
        let result = self.page.call(method, arguments, timeout, cancel).await;
        self.finish_work(result)
    }
    pub(crate) async fn evaluate_tracked(
        &mut self,
        function: &str,
        arguments: &Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.begin_work()?;
        // Keep nested picker futures bounded on the native Windows thread stack.
        let result = Box::pin(
            self.page
                .evaluate_with_arguments(function, arguments, timeout, cancel),
        )
        .await;
        self.finish_work(result)
    }
    pub(crate) fn consume_input_send(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.state == DispatchState::Started,
            "Durable dispatch reservation must precede Send"
        );
        anyhow::ensure!(
            !self.input_send_issued,
            "Physical send already attempted; preserve recovery and never replay automatically"
        );
        self.input_send_issued = true;
        Ok(())
    }
    pub(crate) async fn recover(
        page: Cdp,
        owner: SubmissionOwner,
        conversation: &str,
        completed: bool,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let function = format!(
            "(cfg) => {{ /* industrious-dom:recovery */ {} const claim=readOracleTargetClaim(); const url=new URL(location.href); if(window!==window.top || !claim || claim.id!==cfg.owner.claimId || claim.retiring || url.origin!=='https://chatgpt.com' || url.pathname.match(/(?:^|\\/)c\\/([a-zA-Z0-9-]+)/)?.[1]!==cfg.conversationId) return false; if(cfg.completed===true && !claim.submission && !claim.inspection)return 'clean'; if(!matchesOracleSubmission(claim,cfg.owner) || !['active','releasing','quarantined'].includes(claim.submission.state) || !['started','acknowledged','ambiguous'].includes(claim.submission.dispatch))return false; return 'submission'; }}",
            include_str!("../browser-scripts/ownership-prelude.js")
        );
        let observed = page
            .evaluate_with_arguments(
                &crate::browser_privacy::guard(&function),
                &json!([{"owner":owner,"conversationId":conversation,"completed":completed}]),
                timeout,
                cancel,
            )
            .await?;
        anyhow::ensure!(
            observed == "submission" || completed && observed == "clean",
            "Recovery renderer has changed owners or conversation; preserving target"
        );
        Ok(Self {
            page,
            provider: RendererProvider::Chatgpt,
            grok_intent: None,
            owner,
            state: if observed == "clean" {
                DispatchState::Released
            } else {
                DispatchState::Quarantined
            },
            input_send_issued: true,
            inspection: None,
            epoch: 0,
            dispatch_not_started: false,
            selection_sealed: true,
            retirement_barrier: true,
            pending_work: 0,
            unconfirmed_work: false,
            settlement_confirmed: false,
            notes: Vec::new(),
        })
    }
    /// Rebind the original live Grok submission after its worker exited. The reservation
    /// cannot prepare, arm a witness or Send; it reads, acknowledges the original witness
    /// commitment and seals that witness. Returns the renderer's saved commitment, if any.
    pub(crate) async fn recover_grok(
        page: Cdp,
        owner: SubmissionOwner,
        intent: crate::browser_grok::Intent,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(Self, Value)> {
        let observed = page
            .evaluate_with_arguments(
                &crate::browser_grok::recovery_owner(),
                &json!([{"owner":owner,"dispatchId":intent.dispatch_id}]),
                timeout,
                cancel,
            )
            .await?;
        let state = match observed["dispatch"].as_str() {
            Some("started") => DispatchState::Started,
            Some("acknowledged") => DispatchState::Acknowledged,
            _ => bail!(
                "Original Grok renderer owner, live witness or dispatch changed; preserving target"
            ),
        };
        let reservation = Self {
            page,
            provider: RendererProvider::Grok,
            grok_intent: Some(intent),
            owner,
            state,
            input_send_issued: true,
            inspection: None,
            epoch: 0,
            dispatch_not_started: false,
            selection_sealed: true,
            retirement_barrier: true,
            pending_work: 0,
            unconfirmed_work: false,
            settlement_confirmed: false,
            notes: Vec::new(),
        };
        Ok((reservation, observed["commitReceipt"].clone()))
    }
    pub async fn assert_active(
        &mut self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(
                self.state,
                DispatchState::NotStarted | DispatchState::Started | DispatchState::Acknowledged
            ),
            "Renderer reservation is inactive; refusing mutation"
        );
        self.transition("assert", None, timeout, cancel).await
    }
    async fn transition(
        &mut self,
        action: &str,
        receipt: Option<Value>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let cfg = json!({"owner":self.owner,"action":action,"receipt":receipt,"inspection":self.inspection.take(),"provider":if self.provider==RendererProvider::Grok{"grok"}else{"chatgpt"}});
        let result = self
            .evaluate_tracked(SUBMISSION, &json!([cfg]), timeout, cancel)
            .await;
        match result {
            Ok(Value::Bool(true)) => Ok(()),
            refused => {
                self.unconfirmed_work = true;
                self.state = DispatchState::Quarantined;
                if action == "acquire" && refused.is_ok() {
                    bail!(
                        "This Industrious target is busy or quarantined; choose another browser tab."
                    );
                }
                bail!(
                    "Renderer reservation {action} was not positively acknowledged; preserving recovery"
                )
            }
        }
    }
    pub async fn quarantine(&mut self, timeout: Duration) -> Result<()> {
        self.state = DispatchState::Quarantined;
        self.inspection = None;
        self.transition("quarantine", None, timeout, &CancellationToken::new())
            .await?;
        self.settlement_confirmed = true;
        Ok(())
    }
    pub(crate) async fn quarantine_sources_on(
        &mut self,
        page: Cdp,
        project_path: &str,
        timeout: Duration,
    ) -> Result<()> {
        anyhow::ensure!(
            self.dispatch_not_started && !self.input_send_issued,
            "Sources cleanup cannot quarantine a dispatched consultation"
        );
        let function = format!(
            "(cfg)=>{{ /* industrious-dom:sources-quarantine */ {} const u=new URL(location.href); const claim=readOracleTargetClaim(); if(window!==window.top || u.protocol!=='https:' || !['chatgpt.com','chat.openai.com'].includes(u.hostname) || u.pathname.replace(/\\/$/,'')!==cfg.projectPath || u.searchParams.get('tab')!=='sources' || !matchesOracleSubmission(claim,cfg.owner) || !['active','quarantined'].includes(claim.submission.state) || claim.submission.dispatch!=='not-started' || claim.inspection) return false; return ({SUBMISSION})(cfg); }}",
            include_str!("../browser-scripts/ownership-prelude.js")
        );
        self.state = DispatchState::Quarantined;
        self.inspection = None;
        let original = std::mem::replace(&mut self.page, page);
        let result = self
            .evaluate_tracked(
                &function,
                &json!([{"owner":self.owner,"action":"quarantine","projectPath":project_path}]),
                timeout,
                &CancellationToken::new(),
            )
            .await;
        self.page = original;
        if result.as_ref().is_ok_and(|value| value == true) {
            self.settlement_confirmed = true;
            Ok(())
        } else {
            self.unconfirmed_work = true;
            bail!("Project Sources quarantine was not positively acknowledged; retaining recovery")
        }
    }
    pub(crate) fn mark_input_uncertain(&mut self) {
        self.unconfirmed_work = true;
    }
    pub async fn dispatch_started(
        &mut self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(
            self.state == DispatchState::NotStarted,
            "Submission was already attempted; refusing replay"
        );
        self.dispatch_not_started = false;
        self.state = DispatchState::Started;
        self.transition("dispatch", None, timeout, cancel).await
    }
    pub async fn acknowledge(
        &mut self,
        receipt: &CommitReceipt,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(
            self.state == DispatchState::Started
                && self.provider == RendererProvider::Chatgpt
                && receipt.owner == self.owner
                && receipt.version == 1
                && receipt.kind == "new-user-message",
            "Receipt belongs to a different renderer submission"
        );
        let mut value = serde_json::to_value(receipt)?;
        value["digest"] = json!(sha256(&serde_json::to_vec(receipt)?));
        self.transition("acknowledge", Some(value), timeout, cancel)
            .await?;
        self.state = DispatchState::Acknowledged;
        Ok(())
    }
    /// A Grok-specific WebSocket chain is never converted into a ChatGPT receipt.
    pub async fn acknowledge_grok(
        &mut self,
        receipt: &Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(
            self.provider == RendererProvider::Grok && self.state == DispatchState::Started,
            "Grok receipt requires its original started reservation"
        );
        self.grok_intent
            .as_ref()
            .context("Grok dispatch intent is missing")?
            .require(receipt, &self.owner)?;
        crate::browser_grok::require(&self.page, timeout, cancel).await?;
        let mut value = receipt.clone();
        value["digest"] = json!(sha256(&serde_json::to_vec(receipt)?));
        self.transition("acknowledge", Some(value), timeout, cancel)
            .await?;
        self.state = DispatchState::Acknowledged;
        Ok(())
    }
    /// Select an available Auto or Fast mode and verify the complete text in the
    /// Private composer. Every action stays synchronous in the owned renderer.
    pub async fn prepare_grok_prompt(
        &mut self,
        prompt: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<String> {
        anyhow::ensure!(
            self.provider == RendererProvider::Grok
                && self.state == DispatchState::NotStarted
                && self.grok_intent.is_none()
                && !prompt.is_empty()
                && prompt.encode_utf16().count() <= 524288,
            "Grok prompt preparation needs its original undispatched reservation"
        );
        self.selection_sealed = true;
        let deadline = Instant::now() + timeout;
        self.evaluate_mutation(
            crate::browser_grok::DOM,
            json!({"action":"open-mode"}),
            timeout,
            cancel,
        )
        .await?;
        let mode = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(!left.is_zero(), "Grok mode selection deadline reached");
            let selected = self
                .evaluate_mutation(
                    crate::browser_grok::DOM,
                    json!({"action":"choose-mode"}),
                    left,
                    cancel,
                )
                .await?;
            if selected["status"] == "selected" {
                break selected["mode"]
                    .as_str()
                    .filter(|mode| matches!(*mode, "Auto" | "Fast"))
                    .context("Grok selected mode is unsupported")?
                    .to_owned();
            }
            anyhow::ensure!(
                selected["status"] == "pending",
                "Grok mode selection is unconfirmed"
            );
            pause(cancel).await?;
        };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !left.is_zero(),
                "Grok selected mode did not become verifiable"
            );
            let observed = self
                .evaluate_mutation(
                    crate::browser_grok::DOM,
                    json!({"action":"inspect"}),
                    left,
                    cancel,
                )
                .await?;
            if observed["mode"] == mode {
                break;
            }
            pause(cancel).await?;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!left.is_zero(), "Grok prompt entry deadline reached");
        anyhow::ensure!(
            self.evaluate_mutation(
                crate::browser_grok::DOM,
                json!({"action":"type","prompt":prompt,"mode":mode}),
                left,
                cancel
            )
            .await?
                == true,
            "Complete Grok prompt entry was not acknowledged"
        );
        Ok(mode)
    }
    /// Capture only the original wire-completed DOM turn. Incomplete UI evidence
    /// is pending; a conflicting identity or unsupported asset is never an answer.
    pub async fn observe_grok_answer(
        &mut self,
        prompt: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        anyhow::ensure!(
            self.provider == RendererProvider::Grok && self.state == DispatchState::Acknowledged,
            "Grok answer requires its original acknowledged reservation"
        );
        let intent = self
            .grok_intent
            .as_ref()
            .context("Grok dispatch intent is missing")?;
        let mut cfg = intent.send_config(prompt)?;
        let dispatch = intent.dispatch_id.clone();
        let deadline = Instant::now() + timeout;
        let receipt = self.grok_witness(&dispatch, false, timeout, cancel).await?;
        if receipt["state"] == "committed" {
            return Ok(json!({"state":"pending"}));
        }
        anyhow::ensure!(
            receipt["state"] == "completed",
            "Grok completion witness is invalid"
        );
        cfg["action"] = json!("observe");
        cfg["receipt"] = receipt.clone();
        let left = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!left.is_zero(), "Grok answer observation deadline reached");
        let observed = self
            .evaluate_mutation(crate::browser_grok::DOM, cfg, left, cancel)
            .await?;
        if observed["state"] == "pending" {
            return Ok(observed);
        }
        self.grok_intent
            .as_ref()
            .context("Grok dispatch intent is missing")?
            .require_answer(&receipt, &self.owner, &observed)?;
        Ok(observed)
    }
    /// Arm one original-document text witness after durable dispatch reservation.
    pub async fn install_grok_witness(
        &mut self,
        prompt: &str,
        mode: &str,
        dispatch_id: &str,
        lifetime: Duration,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(
            self.provider == RendererProvider::Grok
                && self.state == DispatchState::Started
                && !self.input_send_issued
                && self.grok_intent.is_none(),
            "Grok witness must precede the single physical Send"
        );
        self.grok_intent = Some(crate::browser_grok::Intent::new(prompt, mode, dispatch_id)?);
        let installed = self.evaluate_mutation(crate::browser_grok::WITNESS,
            json!({"prompt":prompt,"promptSha256":sha256(prompt.as_bytes()),"mode":mode,"dispatchId":dispatch_id,
                "lifetimeMs":u64::try_from(lifetime.as_millis())?}),timeout,cancel).await?;
        anyhow::ensure!(
            installed == true,
            "Grok witness installation was not acknowledged"
        );
        Ok(())
    }
    /// Read or stop only this dispatch's original witness. Stopping drops prompt
    /// references even when the Private predicate or owner has since changed.
    pub async fn grok_witness(
        &mut self,
        dispatch_id: &str,
        cleanup: bool,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        anyhow::ensure!(
            self.provider == RendererProvider::Grok,
            "Grok observation needs its own provider reservation"
        );
        let intent = self
            .grok_intent
            .as_ref()
            .context("Grok dispatch intent is missing")?;
        anyhow::ensure!(
            intent.dispatch_id == dispatch_id,
            "Grok observation dispatch changed"
        );
        let value = self
            .evaluate_tracked(
                &crate::browser_grok::read_witness(cleanup),
                &crate::browser_grok::witness_config(&self.owner, dispatch_id),
                timeout,
                cancel,
            )
            .await?;
        if matches!(value["state"].as_str(), Some("committed" | "completed")) {
            self.grok_intent
                .as_ref()
                .context("Grok dispatch intent is missing")?
                .require(&value, &self.owner)?;
        }
        Ok(value)
    }
    pub(crate) async fn stop_grok_witness(&mut self, timeout: Duration) -> Result<()> {
        let Some(intent) = &self.grok_intent else {
            return Ok(());
        };
        let dispatch = intent.dispatch_id.clone();
        self.grok_witness(&dispatch, true, timeout, &CancellationToken::new())
            .await?;
        Ok(())
    }
    /// Consume the physical Send once and prove the exact live witness in the
    /// same renderer expression as the click, even after a delayed native call.
    pub async fn send_grok_once(
        &mut self,
        prompt: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        anyhow::ensure!(
            self.provider == RendererProvider::Grok,
            "Grok Send needs its provider reservation"
        );
        let cfg = self
            .grok_intent
            .as_ref()
            .context("Grok dispatch intent is missing")?
            .send_config(prompt)?;
        self.consume_input_send()?;
        anyhow::ensure!(
            self.evaluate_mutation(crate::browser_grok::SEND, cfg, timeout, cancel)
                .await?
                == true,
            "Grok physical Send was not acknowledged; preserve without replay"
        );
        Ok(())
    }
    pub async fn release_clean(&mut self, timeout: Duration) -> Result<()> {
        anyhow::ensure!(
            matches!(
                self.state,
                DispatchState::NotStarted | DispatchState::Acknowledged
            ),
            "Uncertain submission cannot release renderer ownership"
        );
        let cancel = CancellationToken::new();
        if self.provider == RendererProvider::Grok && self.state == DispatchState::Acknowledged {
            // Seal whilst the original active submission still exists. Releasing
            // it first would invalidate the live witness and strand a completed
            // Private tab even though its durable answer and owner still match.
            let dispatch = self
                .grok_intent
                .as_ref()
                .context("Grok dispatch intent is missing")?
                .dispatch_id
                .clone();
            let sealed = self
                .grok_witness(&dispatch, true, timeout, &cancel)
                .await
                .and_then(|receipt| {
                    anyhow::ensure!(
                        receipt["state"] == "completed",
                        "Unfinished Grok submission cannot release its owner"
                    );
                    Ok(())
                });
            if let Err(error) = sealed {
                let _ = self.quarantine(timeout).await;
                return Err(error.context("Grok completion was not sealed before clean release"));
            }
        }
        self.assert_active(timeout, &cancel).await?;
        self.transition("releasing", None, timeout, &cancel).await?;
        self.transition("clean", None, timeout, &cancel).await?;
        self.transition("release", None, timeout, &cancel).await?;
        self.state = DispatchState::Released;
        self.settlement_confirmed = true;
        Ok(())
    }
    pub async fn evaluate_mutation(
        &mut self,
        function: &str,
        mut cfg: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.assert_active(timeout, cancel).await?;
        cfg["owner"] = json!(self.owner);
        let function = match self.provider {
            RendererProvider::Chatgpt => crate::browser_privacy::guard(function),
            RendererProvider::Grok => crate::browser_grok::guard(function),
        };
        let result = self
            .evaluate_tracked(&function, &json!([cfg]), timeout, cancel)
            .await;
        if result.is_err() {
            let _ = self.quarantine(CONTROL_ACK_TIMEOUT).await;
        }
        result
    }
    async fn inspect(
        &mut self,
        function: &str,
        mut cfg: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        anyhow::ensure!(
            self.state == DispatchState::NotStarted,
            "Picker selection has been sealed or dispatch has started"
        );
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Picker inspection was cancelled before reservation"
        );
        self.epoch += 1;
        let token = Uuid::new_v4().to_string();
        cfg["owner"] = json!(self.owner);
        cfg["token"] = json!(token);
        cfg["inspection"] = self.inspection.take().unwrap_or(Value::Null);
        let prepared = self
            .evaluate_tracked(PREPARE, &json!([cfg]), timeout, cancel)
            .await;
        let prepared = match prepared {
            Ok(v)
                if v["token"] == token
                    && v["parentReservationId"] == self.owner.reservation_id
                    && v["acknowledged"] == true
                    && bounded(&v["rendererId"]) =>
            {
                v
            }
            _ => {
                self.unconfirmed_work = true;
                let _ = self.quarantine(CONTROL_ACK_TIMEOUT).await;
                bail!("Picker inspection reservation was not acknowledged")
            }
        };
        cfg["rendererId"] = prepared["rendererId"].clone();
        let function = crate::browser_privacy::guard(function);
        let outcome = self
            .evaluate_tracked(&function, &json!([cfg]), timeout, cancel)
            .await;
        // Stop retained renderer timers and require exact cleanup even after cancellation.
        let finished = self
            .evaluate_tracked(
                FINISH,
                &json!([cfg]),
                CONTROL_ACK_TIMEOUT,
                &CancellationToken::new(),
            )
            .await;
        let owned = |v: &Value| {
            v["token"] == token
                && v["parentReservationId"] == self.owner.reservation_id
                && v["rendererId"] == prepared["rendererId"]
                && v["acknowledged"] == true
        };
        let finished = match finished {
            Ok(v) if owned(&v) && v["cleanupComplete"] == true => v,
            other => {
                let detail = cleanup_detail(&other, other.as_ref().is_ok_and(owned));
                let picker = if function.contains("industrious-dom:thinking") {
                    "effort"
                } else {
                    "model"
                };
                self.unconfirmed_work = true;
                let _ = self.quarantine(CONTROL_ACK_TIMEOUT).await;
                bail!(
                    "Picker cleanup is unconfirmed ({picker} picker, {detail}); refusing target reuse"
                )
            }
        };
        if outcome.is_err() || cancel.is_cancelled() {
            let _ = self.quarantine(CONTROL_ACK_TIMEOUT).await;
            bail!("Picker evaluation was cancelled or uncertain; preserving recovery");
        }
        self.inspection = Some(
            json!({"token":token,"parentReservationId":self.owner.reservation_id,"rendererId":finished["rendererId"]}),
        );
        outcome
    }
    fn model_cfg(strategy: &str, inspect_current: Value) -> Value {
        // Exact maintained GPT-6 matcher output. New dispatch cannot choose a legacy family.
        json!({"model":"GPT-6","strategy":strategy,"inspectCurrent":inspect_current,
            "labelTokens":["gpt-6","chatgpt gpt-6","gpt gpt-6","latest","最新","최신"],
            "testIdTokens":["gpt-6","model-switcher-gpt-6"]})
    }
    fn note(&mut self, line: String) {
        self.notes.push(line);
    }
    /// Picker progress lines (Node's logger calls) for the runner to write to the session log.
    pub(crate) fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }
    /// Node throwIfThrottled and throwIfModelsNotLoaded: a rate-limit notice or a model list
    /// that never loaded replaces the misleading picker failure. A failed probe keeps the
    /// caller's own error.
    async fn reclassify(
        &mut self,
        stage: &str,
        throttle: bool,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let Ok(found) = self
            .evaluate_tracked(
                crate::browser_health::GUARD,
                &json!([{}]),
                timeout.min(CONTROL_ACK_TIMEOUT),
                cancel,
            )
            .await
        else {
            return Ok(());
        };
        if throttle && found["throttled"] == true {
            let notice = found["throttleMessage"]
                .as_str()
                .unwrap_or("ChatGPT is rate limiting this session.");
            self.note(format!(
                "[browser] ChatGPT rate limit detected during {stage}: {notice}"
            ));
            bail!(
                "ChatGPT is rate limiting this session: {notice} This is a temporary limit on the account, not a problem with the request — wait a few minutes and retry, and reduce how many conversations are started at once."
            );
        }
        if found["modelsLoadFailed"] == true {
            let notice = found["modelsMessage"].as_str().unwrap_or_default();
            let place = found["modelsWhere"].as_str().unwrap_or("menu");
            self.note(format!(
                "[browser] ChatGPT model list failed to load during {stage} ({place}): {notice}"
            ));
            bail!(
                "ChatGPT could not load its model list ({notice}). The page is broken, not the request: reload the tab and retry."
            );
        }
        Ok(())
    }
    fn diagnostic(&mut self, outcome: &Value) {
        if outcome["diagnostic"].is_object() {
            self.note(format!(
                "[browser] Model picker diagnostic: {}",
                outcome["diagnostic"]
            ));
        }
    }
    /// `pro_selected` admits the Pro radio for "current" only: see `pro_label`.
    async fn select_model(
        &mut self,
        strategy: &str,
        pro_selected: bool,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ModelEvidence> {
        let started = Instant::now();
        let deadline = started + timeout;
        let button_wait = timeout.min(MODEL_BUTTON_WAIT);
        let mut announced = false;
        let result = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                picker("model hydration did not produce owned evidence")
            );
            let result = self
                .inspect(
                    MODEL,
                    Self::model_cfg(
                        strategy,
                        if strategy == "current" {
                            json!("evidence")
                        } else {
                            json!(false)
                        },
                    ),
                    remaining,
                    cancel,
                )
                .await?;
            if matches!(
                result["status"].as_str(),
                Some("pending" | "button-missing")
            ) && started.elapsed() < button_wait
            {
                if !announced {
                    announced = true;
                    self.note(format!(
                        "Model picker button not mounted yet; waiting up to {}s for the composer pill to render.",
                        button_wait.as_secs_f64().round()
                    ));
                }
                pause(cancel).await?;
                continue;
            }
            break result;
        };
        let label = result["label"]
            .as_str()
            .map(str::trim)
            .filter(|label| !label.is_empty());
        match (result["status"].as_str().unwrap_or_default(), label) {
            ("already-selected" | "switched", Some(_)) => {}
            ("selection-invalid", _) => {
                return Err(picker(
                    result["reason"]
                        .as_str()
                        .unwrap_or("model selection returned unknown evidence"),
                ));
            }
            ("option-not-found", _) => {
                self.reclassify("model-selection", true, timeout, cancel)
                    .await?;
                return Err(picker("model option is unavailable or ambiguous"));
            }
            ("pending" | "button-missing", _) => {
                self.reclassify("model-selection", true, timeout, cancel)
                    .await?;
                return Err(picker("model hydration did not produce owned evidence"));
            }
            _ => return Err(picker("model selection returned unknown evidence")),
        }
        let label = label.unwrap_or_default().to_owned();
        let accepted = |label: &Value| {
            latest_label(label) || pro_selected && strategy == "current" && pro_label(label)
        };
        if strategy == "current" {
            self.note(format!("Model picker: {label}"));
            anyhow::ensure!(
                accepted(&result["label"]),
                "Current model differs from the requested model; refusing to submit."
            );
        } else {
            anyhow::ensure!(
                latest_label(&result["label"]),
                picker("selected model does not match")
            );
            self.note(format!("Model picker: {label}"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let observed = self
            .inspect(
                MODEL,
                Self::model_cfg("current", json!("evidence")),
                remaining,
                cancel,
            )
            .await?;
        anyhow::ensure!(
            observed["status"] == "already-selected"
                && accepted(&observed["label"])
                && observed["checkedCount"] == 1
                && bounded(&observed["ownerKey"]),
            "Independent model observation is ambiguous or belongs to the wrong family"
        );
        Ok(serde_json::from_value(
            json!({"verified":true,"source":"chatgpt-model-picker","requestedModel":"GPT-6","resolvedLabel":"GPT-6","observedLabel":observed["label"],"ownerKey":observed["ownerKey"],"strategy":strategy,"status":result["status"],"capturedAt":timestamp()}),
        )?)
    }
    pub(crate) async fn inspect_current_model(
        &mut self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.inspect(
            MODEL,
            Self::model_cfg("current", json!("evidence")),
            timeout,
            cancel,
        )
        .await
    }
    pub(crate) async fn inspect_current_effort(
        &mut self,
        level: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.inspect(THINKING, Self::thinking_cfg(level, true), timeout, cancel)
            .await
    }
    fn thinking_cfg(level: &str, inspection: bool) -> Value {
        json!({"level":level,"inspectionOnly":inspection,"inspectionEvidence":inspection,"modelButtonSelector":MODEL_BUTTON})
    }
    /// Node ensureThinkingTime. Pro fails closed with the reference wording; a disabled Pro
    /// row returns its outcome for the permit decision. The non-strict Extra High fallback
    /// logs the reference line, then fails the joint verification as the reference does.
    async fn select_effort(
        &mut self,
        level: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let outcome = self
            .inspect(THINKING, Self::thinking_cfg(level, false), timeout, cancel)
            .await?;
        let strict = level == "pro";
        let requested = if strict { "Pro" } else { "Extra-high" };
        let label = outcome["label"].as_str().unwrap_or(requested).to_owned();
        let unverified = || {
            anyhow!("Browser model and thinking effort could not be verified; refusing to submit.")
        };
        let status = outcome["status"].as_str().unwrap_or_default();
        match status {
            "already-selected" => {
                self.note(format!(
                    "[browser] Thinking time: {label} (already selected)"
                ));
            }
            "switched" => self.note(format!("[browser] Thinking time: {label}")),
            "pending" if outcome["selectionIssued"] == true => {}
            "pending" => return Err(picker("effort selection closed before verification")),
            "selection-invalid" => {
                return Err(picker(
                    outcome["reason"]
                        .as_str()
                        .unwrap_or("owned effort evidence is unavailable"),
                ));
            }
            "option-disabled" => {
                self.reclassify("thinking-time", true, timeout, cancel)
                    .await?;
                self.diagnostic(&outcome);
                if strict {
                    return Ok(outcome);
                }
                let notice = outcome["notice"].as_str().unwrap_or("no reason given");
                self.note(format!(
                    "[browser] Thinking time: {label} is unavailable on this account ({notice}); keeping the effort already selected in ChatGPT."
                ));
                return Err(unverified());
            }
            "chip-not-found"
            | "menu-not-found"
            | "option-not-found"
            | "selection-unverified"
            | "model-kind-not-found" => {
                self.diagnostic(&outcome);
                self.reclassify("thinking-time", false, timeout, cancel)
                    .await?;
                let hint = match outcome["modelKind"].as_str() {
                    Some(kind) if status == "model-kind-not-found" => format!(" for {kind}"),
                    _ => String::new(),
                };
                let message = format!("{}{hint} (requested {requested})", status.replace('-', " "));
                if strict {
                    bail!("Thinking time: {message}; refusing to submit without confirmed Pro.");
                }
                let kept = if status == "selection-unverified" {
                    "the effort in ChatGPT is unconfirmed"
                } else {
                    "keeping the effort already selected in ChatGPT"
                };
                self.note(format!("[browser] Thinking time: {message}; {kept}."));
                return Err(unverified());
            }
            _ => {
                self.diagnostic(&outcome);
                self.reclassify("thinking-time", false, timeout, cancel)
                    .await?;
                if strict {
                    bail!(
                        "Thinking time: unknown outcome selecting Pro; refusing to submit without confirmed Pro."
                    );
                }
                self.note(format!(
                    "[browser] Thinking time: unknown outcome selecting {requested}; continuing with ChatGPT default."
                ));
                return Err(unverified());
            }
        }
        Ok(outcome)
    }
    async fn select_pro(
        &mut self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<std::result::Result<Value, ProUnavailablePermit>> {
        let outcome = self.select_effort("pro", timeout, cancel).await?;
        if outcome["status"] != "option-disabled" {
            return Ok(Ok(outcome));
        }
        if outcome["unavailableModel"] == "GPT-6"
            && outcome["label"]
                .as_str()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("pro"))
        {
            return Ok(Err(ProUnavailablePermit {
                owner: self.owner.clone(),
                epoch: self.epoch,
                inspection: self
                    .inspection
                    .clone()
                    .context("No positive disabled-Pro cleanup receipt")?,
            }));
        }
        // Node ThinkingTierUnavailableError; generic unavailability grants no fallback.
        bail!(
            "Thinking time: {} is unavailable on this account ({}); refusing to submit without confirmed Pro.",
            outcome["label"].as_str().unwrap_or("Pro"),
            outcome["notice"].as_str().unwrap_or("no reason given")
        )
    }
    async fn fallback(
        &mut self,
        permit: ProUnavailablePermit,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        anyhow::ensure!(
            self.state == DispatchState::NotStarted
                && permit.owner == self.owner
                && permit.epoch == self.epoch
                && self.inspection.as_ref() == Some(&permit.inspection),
            "Pro-unavailable fallback authority is stale or foreign"
        );
        self.select_effort("extra-high", timeout, cancel).await
    }
    pub async fn select_pair(
        &mut self,
        strategy: &str,
        desired: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(ModelEvidence, ThinkingEvidence, String)> {
        anyhow::ensure!(
            matches!(strategy, "select" | "current"),
            "Ignoring model verification cannot authorise browser dispatch"
        );
        // Node selectSingleComposerPair: "current" observes without verifying a fresh choice,
        // so the reference refuses before any thinking-picker change.
        anyhow::ensure!(
            strategy == "select",
            picker(
                "Requested browser model could not be verified; refusing to select thinking effort."
            )
        );
        // "Latest" is the only saved alias for the GPT-6 matcher; other labels keep GPT-6.
        let requested = if desired == "Latest" {
            "Latest"
        } else {
            "GPT-6"
        };
        let mut model = self.select_model(strategy, false, timeout, cancel).await?;
        model.requested_model = Some(requested.to_owned());
        let (level, outcome) = match self.select_pro(timeout, cancel).await? {
            Ok(outcome) => ("pro", outcome),
            Err(permit) => {
                self.note(format!(
                    "[browser] Confirmed owned Pro unavailability; trying {requested} with Extra High."
                ));
                ("extra-high", self.fallback(permit, timeout, cancel).await?)
            }
        };
        // Node ensureThinkingTime evidence: a selection that closed the picker verified as
        // already selected, and the later verification keeps a switched status. Only GPT-6 and
        // Latest reach here, and inferThinkingTargetModelKind finds no kind in either.
        let selection = json!({
            "status": if outcome["status"] == "switched" {"switched"} else {"already-selected"},
            "targetModelKind": null,
            "observedModelKind": outcome["modelKind"].as_str(),
        });
        self.observe_pair(model, level, selection, timeout, cancel)
            .await
    }
    pub async fn verify_pair(
        &mut self,
        expected_model: &str,
        expected_level: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(ModelEvidence, ThinkingEvidence, String)> {
        anyhow::ensure!(
            matches!(expected_model, "GPT-6" | "Latest"),
            "Saved browser follow-up model is not Latest (GPT-6)"
        );
        anyhow::ensure!(
            matches!(expected_level, "pro" | "extra-high"),
            "Saved browser follow-up has no qualified thinking effort"
        );
        let mut model = self
            .select_model("current", expected_level == "pro", timeout, cancel)
            .await?;
        model.requested_model = Some(expected_model.to_owned());
        model.extra.insert("verificationOnly".into(), json!(true));
        // Node verifyThinkingTime evidence for a verify-only follow-up.
        let selection = json!({"status":"already-selected"});
        self.observe_pair(model, expected_level, selection, timeout, cancel)
            .await
    }
    async fn observe_pair(
        &mut self,
        model: ModelEvidence,
        level: &str,
        selection: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(ModelEvidence, ThinkingEvidence, String)> {
        let effective = if level == "pro" {
            "gpt-6-pro"
        } else {
            "gpt-6-astra"
        };
        let effort = self
            .inspect(THINKING, Self::thinking_cfg(level, true), timeout, cancel)
            .await?;
        // Only GPT-6's direct Power slider proves Pro; a legacy Effort row may prove Extra High.
        anyhow::ensure!(
            effort["status"] == "ready"
                && effort["selectedLevel"] == level
                && bounded(&effort["ownerKey"])
                && match effort["basis"].as_str() {
                    Some("slider-label-and-index") => true,
                    Some("checked-tier-row") => level != "pro",
                    _ => false,
                },
            "Independent thinking-tier observation is missing or incorrect"
        );
        // Recheck the family after observing effort. A coupled model change cannot pass.
        let observed = self
            .inspect(
                MODEL,
                Self::model_cfg("current", json!("evidence")),
                timeout,
                cancel,
            )
            .await?;
        // The same trigger owns the model, the effort and this recheck.
        let owner = &observed["ownerKey"];
        anyhow::ensure!(
            observed["status"] == "already-selected"
                && (latest_label(&observed["label"])
                    || level == "pro" && pro_label(&observed["label"]))
                && observed["checkedCount"] == 1
                && bounded(owner)
                && effort["ownerKey"] == *owner
                && model.extra.get("ownerKey") == Some(owner),
            "Model changed while selecting effort"
        );
        let mut thinking: ThinkingEvidence = serde_json::from_value(
            json!({"verified":true,"strictFailClosed":true,"source":"chatgpt-thinking-picker","requestedLevel":level,"resolvedLabel":if level == "pro" {"Pro"} else {"Extra High"},"observation":effort,"observedModelLabel":observed["label"],"capturedAt":timestamp()}),
        )?;
        if let Value::Object(selection) = selection {
            thinking.extra.extend(selection);
        }
        policy::assert_browser_completion(Some(&model), Some(&thinking), Some(effective))?;
        Ok((model, thinking, effective.into()))
    }
}
/// Node PickerSelectionError wording.
fn picker(reason: &str) -> anyhow::Error {
    anyhow!("Picker selection is unverified ({reason}); refusing to submit.")
}
fn bounded(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.encode_utf16().count() <= 256)
}
fn latest_label(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| matches!(s.trim(), "GPT-6" | "Latest" | "最新" | "최신"))
}
/// Selecting GPT-6's top Power step, labelled Pro, also checks the Pro model radio (observed
/// 27 October 2026). Callers accept it only after that step was selected or is expected.
fn pro_label(value: &Value) -> bool {
    value.as_str().is_some_and(|s| s.trim() == "Pro")
}
async fn pause(cancel: &CancellationToken) -> Result<()> {
    tokio::select! { biased; _ = cancel.cancelled() => bail!("Picker observation was cancelled"), _ = tokio::time::sleep(Duration::from_millis(250)) => Ok(()) }
}

#[cfg(test)]
mod settlement_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    async fn reservation() -> (RendererReservation, tokio::task::JoinHandle<()>) {
        reservation_with_quarantine_delay(Duration::ZERO).await
    }
    async fn reservation_with_quarantine_delay(
        delay: Duration,
    ) -> (RendererReservation, tokio::task::JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let page = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut socket = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let peer = tokio::spawn(async move {
            while let Some(Ok(Message::Text(frame))) = socket.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                if request["params"]["expression"]
                    .as_str()
                    .unwrap()
                    .contains("PUBLIC pending work")
                {
                    continue;
                }
                if request["params"]["expression"]
                    .as_str()
                    .unwrap()
                    .contains("\"action\":\"quarantine\"")
                {
                    tokio::time::sleep(delay).await;
                }
                if socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":{"result":{"value":true}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let owner = RendererReservation::acquire(
            page,
            "PUBLIC settlement claim".into(),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        (owner, peer)
    }
    async fn stop(owner: RendererReservation, peer: tokio::task::JoinHandle<()>) {
        owner
            .page
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        peer.await.unwrap();
    }
    #[tokio::test]
    async fn acknowledged_quarantine_needs_settled_work_and_unsealed_selection() {
        let (mut owner, peer) = reservation().await;
        assert!(!owner.settled_before_dispatch());
        owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
        assert!(owner.settled_before_dispatch());
        owner.seal_selection();
        assert!(!owner.settled_before_dispatch());
        stop(owner, peer).await;
    }
    #[tokio::test(start_paused = true)]
    async fn slow_quarantine_acknowledgement_within_the_control_window_settles() {
        // A loaded renderer can take several seconds to acknowledge an ownership control.
        let (mut owner, peer) = reservation_with_quarantine_delay(Duration::from_secs(3)).await;
        owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
        assert!(owner.settled_before_dispatch());
        stop(owner, peer).await;
    }
    #[tokio::test]
    async fn only_a_fully_settled_failed_upload_returns_to_pre_payload_cleanup() {
        for failed in [false, true] {
            let (mut owner, peer) = reservation().await;
            owner.seal_selection();
            owner.begin_upload_work().unwrap();
            owner
                .call_tracked(
                    "Runtime.evaluate",
                    json!({"expression":"true"}),
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
            assert!(!owner.settled_before_dispatch());
            owner.finish_upload_work(failed);
            assert_eq!(owner.settled_before_dispatch(), failed);
            stop(owner, peer).await;
        }
    }
    #[tokio::test]
    async fn failed_upload_cleanup_cannot_clear_a_dropped_or_unconfirmed_command() {
        for dropped in [false, true] {
            let (mut owner, peer) = reservation().await;
            owner.seal_selection();
            owner.begin_upload_work().unwrap();
            let cancel = CancellationToken::new();
            if !dropped {
                cancel.cancel();
            }
            let command = owner.call_tracked(
                "Runtime.evaluate",
                json!({"expression":"true /* PUBLIC pending work */"}),
                Duration::from_secs(1),
                &cancel,
            );
            if dropped {
                assert!(
                    tokio::time::timeout(Duration::from_millis(25), command)
                        .await
                        .is_err()
                );
            } else {
                assert!(command.await.is_err());
            }
            owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
            owner.finish_upload_work(true);
            assert!(!owner.settled_before_dispatch());
            assert_eq!(owner.pending_work, u32::from(dropped));
            stop(owner, peer).await;
        }
    }
    #[tokio::test]
    async fn failed_upload_cleanup_cannot_reopen_prompt_or_reset_retirement_barrier() {
        for sealed_before_upload in [false, true] {
            let (mut owner, peer) = reservation().await;
            if sealed_before_upload {
                owner.seal_retirement();
            }
            owner.begin_upload_work().unwrap();
            if !sealed_before_upload {
                owner.seal_retirement();
            }
            owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
            owner.finish_upload_work(true);
            assert!(!owner.settled_before_dispatch());
            stop(owner, peer).await;
        }
    }
    #[tokio::test]
    async fn acknowledged_quarantine_does_not_clear_unconfirmed_work() {
        let (mut owner, peer) = reservation().await;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            owner
                .evaluate_tracked("()=>true", &json!([]), Duration::from_secs(1), &cancelled)
                .await
                .is_err()
        );
        assert_eq!(owner.pending_work, 0);
        owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
        assert!(!owner.settled_before_dispatch());
        stop(owner, peer).await;
    }
    #[tokio::test]
    async fn dropped_renderer_future_keeps_pending_work_after_quarantine() {
        let (mut owner, peer) = reservation().await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                owner.evaluate_tracked(
                    "()=>true /* PUBLIC pending work */",
                    &json!([]),
                    Duration::from_secs(1),
                    &CancellationToken::new()
                )
            )
            .await
            .is_err()
        );
        assert_eq!(owner.pending_work, 1);
        owner.quarantine(CONTROL_ACK_TIMEOUT).await.unwrap();
        assert!(!owner.settled_before_dispatch());
        stop(owner, peer).await;
    }
}

#[cfg(test)]
mod cleanup_detail_tests {
    use super::cleanup_detail;
    use serde_json::json;

    #[test]
    fn cleanup_detail_names_state_and_withholds_page_text() {
        let state = |s: &str| cleanup_detail(&Ok(json!({"cleanupState": s})), true);
        assert_eq!(
            state("menu-visible overlay-open"),
            "still open: menu-visible overlay-open"
        );
        assert_eq!(state("Usage limit reached"), "still open: unknown");
        assert_eq!(state(&"a".repeat(121)), "still open: unknown");
        assert_eq!(cleanup_detail(&Ok(json!({})), true), "still open: unknown");
        assert_eq!(
            cleanup_detail(&Ok(json!({"cleanupState": "menu-visible"})), false),
            "finish not acknowledged"
        );
        assert_eq!(
            cleanup_detail(&Err(anyhow::anyhow!("timed out")), true),
            "finish did not answer"
        );
    }
}

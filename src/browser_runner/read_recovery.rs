//! One bounded observation episode on the original live reservation, without resubmission.
use super::*;
use anyhow::Context as _;

pub(super) struct Budget {
    enabled: bool,
    deadline: Option<Instant>,
}
impl Budget {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            deadline: None,
        }
    }
    /// A recovered commitment cannot start a fresh full answer timeout.
    pub(super) fn limits(&self, limits: &RunnerLimits) -> RunnerLimits {
        let mut bounded = limits.clone();
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let reading = remaining.saturating_sub(Duration::from_millis(50));
            bounded.command = bounded.command.min(remaining);
            bounded.commit = bounded.commit.min(reading);
            bounded.answer = bounded.answer.min(reading);
            bounded.poll = bounded.poll.min(remaining);
        }
        bounded
    }
    pub(super) async fn within<T>(
        &self,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let Some(deadline) = self.deadline else {
            return future.await;
        };
        anyhow::ensure!(
            Instant::now() < deadline,
            "Original-target observation budget expired; recovery retained"
        );
        tokio::time::timeout_at(deadline, future).await.context(
            "Original-target observation did not settle before its deadline; recovery retained",
        )?
    }
    pub(super) async fn observe<T, F, Fut>(
        &mut self,
        initial: Result<T>,
        context: Context<'_>,
        limits: &RunnerLimits,
        cancel: &CancellationToken,
        mut read: F,
    ) -> Result<T>
    where
        F: FnMut(RunnerLimits) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let original = match initial {
            Ok(value) => {
                if self.deadline.is_some() {
                    context.require(cancel)?;
                }
                return Ok(value);
            }
            Err(error) => error,
        };
        if !self.enabled || !eligible(&original) {
            return Err(original);
        }
        if self.deadline.is_some() {
            context.require(cancel)?;
            return Err(original);
        }
        context.require(cancel)?;
        let duration = Duration::from_secs(30)
            .min(limits.commit)
            .min(limits.answer);
        if duration.is_zero() {
            return Err(original);
        }
        let deadline = Instant::now() + duration;
        self.deadline = Some(deadline);
        context.record("observing", cancel)?;
        loop {
            context.require(cancel)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            // Leave time to verify identity and persist the outcome after the read loop ends.
            if remaining <= Duration::from_millis(50) {
                context.record("expired", cancel)?;
                return Err(original.context("Bounded original-target observation expired"));
            }
            let result = tokio::time::timeout_at(deadline, async {
                context
                    .require_browser(limits.command.min(remaining), cancel)
                    .await?;
                let mut bounded = self.limits(limits);
                let reading = deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(Duration::from_millis(50));
                if reading.is_zero() {
                    return Err(crate::browser_provider::Failure::AnswerDeadline.into());
                }
                bounded.commit = reading;
                bounded.answer = reading;
                let result = read(bounded).await;
                context.require(cancel)?;
                result
            })
            .await;
            match result {
                Ok(Ok(value)) => {
                    context.record("recovered", cancel)?;
                    return Ok(value);
                }
                Ok(Err(error)) if eligible(&error) => {
                    if !matches!(
                        error.downcast_ref::<crate::browser_provider::Failure>(),
                        Some(crate::browser_provider::Failure::ResponseLoadFailed)
                    ) {
                        context.record("expired", cancel)?;
                        return Err(original.context("Bounded original-target observation expired"));
                    }
                    // A load-error label can disappear. Keep observing within the same deadline.
                    pause(
                        limits
                            .poll
                            .min(deadline.saturating_duration_since(Instant::now())),
                        cancel,
                    )
                    .await?;
                }
                // Privacy, source, ownership, storage and transport failures must not retain a
                // provider-eligible error in their source chain.
                Ok(Err(error)) => return Err(error),
                Err(_) => bail!(
                    "Original-target observation did not settle before its deadline; recovery retained"
                ),
            }
        }
    }
}

fn eligible(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<crate::browser_provider::Failure>(),
        Some(
            crate::browser_provider::Failure::CommitDeadline
                | crate::browser_provider::Failure::AnswerDeadline
                | crate::browser_provider::Failure::ResponseLoadFailed
        )
    )
}

/// Snapshot only the existing authority. This type grants no target, renderer or close permit.
pub(super) struct Context<'a> {
    store: &'a SessionStore,
    id: &'a str,
    witness: &'a ExecutionWitness,
    committed: &'a BrowserRequest,
    target: &'a OwnedTarget,
    endpoint: &'a AttachmentEndpoint,
    authority: TargetAuthority<'a>,
    owner: Value,
    identity: Value,
}
fn identity(metadata: &Value) -> Value {
    let mut recovery = recovery_identity(metadata);
    // A Pending commitment may save its observed conversation URL. Receipt/source identity
    // is separately pinned below and cannot be changed by that diagnostic persistence.
    recovery.as_object_mut().unwrap().remove("conversationId");
    let runtime = &metadata["browser"]["runtime"];
    json!({"recovery":recovery,"dispatch":dispatch_identity(metadata),
        "config":metadata["browser"]["config"],"commitSnapshot":runtime["commitSnapshot"],
        "receipt":runtime["userCommitReceipt"],"model":runtime["modelSelection"],
        "thinking":runtime["thinkingSelection"],"effectiveModel":runtime["effectiveModel"],
        "profileDir":runtime["profileDir"],"leaseId":runtime["tabLeaseId"]})
}
impl<'a> Context<'a> {
    #[allow(clippy::too_many_arguments)] // Preserve the same explicit witnesses as the original send.
    pub(super) fn capture(
        store: &'a SessionStore,
        id: &'a str,
        witness: &'a ExecutionWitness,
        committed: &'a BrowserRequest,
        target: &'a OwnedTarget,
        endpoint: &'a AttachmentEndpoint,
        authority: TargetAuthority<'a>,
        reservation: &RendererReservation,
    ) -> Result<Self> {
        Ok(Self {
            store,
            id,
            witness,
            committed,
            target,
            endpoint,
            authority,
            owner: json!(reservation.owner()),
            identity: identity(&store.require(id)?),
        })
    }
    fn require(&self, cancel: &CancellationToken) -> Result<()> {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Original-target observation cancelled; recovery retained"
        );
        let current = self.store.require(self.id)?;
        self.witness.require(&current)?;
        crate::browser_attempt::require_if_child(self.store, self.id, cancel)?;
        let runtime = &current["browser"]["runtime"];
        let model = current["models"].as_array().and_then(|models| {
            models
                .iter()
                .find(|model| model["model"] == current["model"])
        });
        anyhow::ensure!(
            current["status"] == "running"
                && model.is_none_or(|model| !sessions::terminal(
                    model["status"].as_str().unwrap_or("")
                ))
                && identity(&current) == self.identity
                && serde_json::to_value(request(self.store, self.id)?)?
                    == serde_json::to_value(self.committed)?
                && runtime["chromeTargetId"] == self.target.id()
                && runtime["chromeHost"] == self.endpoint.host
                && runtime["chromePort"] == self.endpoint.port
                && runtime["browserWSEndpoint"] == self.endpoint.browser_ws_endpoint
                && runtime["submissionOwner"] == self.owner
                && current["dispatch"]["submissionOwner"] == self.owner
                && current["dispatch"]["reservationId"] == self.committed.reservation_id
                && current["dispatch"]["promptSha256"] == self.committed.prompt_sha256
                && matches!(
                    current["dispatch"]["state"].as_str(),
                    Some("started" | "acknowledged")
                )
                && self.authority.holds(runtime, self.target.id()),
            "Original-target observation lost its request, dispatch or target authority"
        );
        let TargetAuthority::Created(Some(creator)) = self.authority else {
            bail!("Original-target observation needs its original allocated target creator");
        };
        let record = TargetJournal::open(self.store)?.read(&creator["allocationId"])?;
        anyhow::ensure!(
            record["creator"] == *creator
                && record["commit"].is_null()
                && record["finalisedAt"].is_null()
                && record["retirement"].is_null()
                && record["disposition"]["decision"] != "retired",
            "Original-target observation found a changed or completed target journal"
        );
        self.target.lease().require()?;
        Ok(())
    }
    async fn require_browser(&self, timeout: Duration, cancel: &CancellationToken) -> Result<()> {
        self.target.require(timeout, cancel).await?;
        self.require(cancel)?;
        crate::browser_privacy::require(self.target.page(), timeout, cancel).await?;
        self.require(cancel)
    }
    fn record(&self, state: &str, cancel: &CancellationToken) -> Result<()> {
        self.require(cancel)?;
        runtime_patch(
            self.store,
            self.id,
            self.witness,
            json!({"providerReadRecovery":{
                "version":1,"state":state,"checkedAt":timestamp(),"readOnly":true,
                "targetId":self.target.id(),"submissionOwner":self.owner
            }}),
        )?;
        self.require(cancel)
    }
}

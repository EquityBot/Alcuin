//! Reciprocal, immutable identities for attempts of one browser consultation.
use crate::{
    artifacts::sha256,
    browser_chrome::AttachmentEndpoint,
    browser_runner::{self, BrowserRequest},
    execution::{ExecutionWitness, controller_identity},
    native_fs::{NativeEntry, NativeIdentity},
    process::ProcessIdentity,
    sessions::{SessionStore, merge_object},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod adoption;
pub(crate) use adoption::{captured_selected, captured_selected_until, require_selected};
mod completion;
mod recovery;
#[cfg(test)]
pub(crate) use completion::settle_selected;
pub(crate) use completion::{complete_selected, settle_selected_saved};
pub(crate) use recovery::{
    Guard as RecoveryGuard, recover as recover_parent, settle_unsent_switch,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    Chatgpt,
    Grok,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Link {
    version: u8,
    coordinator_id: String,
    attempt_id: String,
    pub parent_id: String,
    pub child_id: String,
    pub ordinal: u32,
    pub provider: Provider,
    parent_options_sha256: String,
    parent_request_sha256: String,
    parent_controller: Value,
    parent_directory: NativeIdentity,
    child_directory: NativeIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ready {
    version: u8,
    link: Link,
    child_request_sha256: String,
    child_options_sha256: String,
}

/// One authorised inline-to-upload transformation. Reserving it blocks admission until
/// the child commits the same bytes; a partial write never restores the original seal.
pub(crate) struct FallbackTicket {
    link: Link,
    ready: Ready,
    seal: Value,
}
impl FallbackTicket {
    pub(crate) fn require(
        &self,
        store: &SessionStore,
        child: &Value,
        previous: &BrowserRequest,
        next: &BrowserRequest,
        cancel: &CancellationToken,
    ) -> Result<()> {
        ensure!(
            !cancel.is_cancelled()
                && child["browser"]["attempt"] == json!(self.link)
                && child["browser"]["attemptReady"] == json!(self.ready)
                && child["browser"]["attemptFallback"].is_null()
                && request_hash(previous)? == self.ready.child_request_sha256
                && request_hash(next)? == self.seal["derivedRequestSha256"],
            "Provider fallback changed before child commitment"
        );
        let parent = store.require(&self.link.parent_id)?;
        require_parent_binding(store, &parent, &self.link)?;
        ensure!(
            parent["browser"]["consultation"]["ready"] == json!(self.ready)
                && parent["browser"]["consultation"]["fallback"] == self.seal,
            "Provider fallback reservation changed"
        );
        Ok(())
    }
    pub(crate) fn seal(&self) -> &Value {
        &self.seal
    }
}

pub(crate) fn reserve_fallback(
    store: &SessionStore,
    id: &str,
    previous: &BrowserRequest,
    next: &BrowserRequest,
    observed: &crate::browser_dom::PromptTooLarge,
    cancel: &CancellationToken,
) -> Result<Option<FallbackTicket>> {
    let child = store.require(id)?;
    if !is_child(&child, previous) {
        return Ok(None);
    }
    let link = require_child(store, id, cancel)?;
    let ready: Ready = serde_json::from_value(child["browser"]["attemptReady"].clone())?;
    ensure!(
        !previous.fallback_used
            && previous.fallback.is_some()
            && next.fallback_used
            && request_hash(previous)? == ready.child_request_sha256
            && observed.prompt_length == previous.prompt.encode_utf16().count()
            && observed.prompt_length >= 50_000
            && observed.observed_length > 0
            && observed.observed_length < (observed.prompt_length - 2_000) as u64,
        "Provider fallback lacks its original positive truncation evidence"
    );
    let staged = next
        .staged_uploads
        .as_ref()
        .context("Provider fallback lacks staged inputs")?;
    staged.verify(store, id)?;
    let seal = json!({"version":1,"link":link,"originalRequestSha256":ready.child_request_sha256,
        "derivedRequestSha256":request_hash(next)?,"promptLength":observed.prompt_length,
        "observedLength":observed.observed_length});
    store.update(&link.parent_id,|parent| {
        ensure!(!cancel.is_cancelled(),"Provider fallback was cancelled");
        require_parent_binding(store,parent,&link)?;
        let current=store.require(id)?;
        ensure!(parent["browser"]["consultation"]["fallback"].is_null()
            && parent["browser"]["consultation"]["ready"]==json!(ready)
            && current["browser"]["attemptReady"]==json!(ready)
            && current["browser"]["attemptFallback"].is_null()
            && current["status"]=="running" && current.get("dispatch").is_none()
            && request_hash(&browser_runner::request(store,id)?)?==ready.child_request_sha256,
            "Provider child changed before fallback reservation");
        Ok(Some(json!({"browser":merge_object(&parent["browser"],&json!({"consultation":merge_object(&parent["browser"]["consultation"],&json!({"fallback":seal}))}))})))
    })?;
    Ok(Some(FallbackTicket { link, ready, seal }))
}

pub(crate) fn is_child(meta: &Value, request: &BrowserRequest) -> bool {
    !meta["browser"]["attempt"].is_null()
        || !meta["browser"]["attemptReady"].is_null()
        || !meta["options"]["providerAttempt"].is_null()
        || request
            .extra
            .get("providerAttempt")
            .is_some_and(|value| !value.is_null())
}
/// Internal attempts remain addressable for diagnosis and visible to retirement scans.
pub(crate) fn internal(meta: &Value) -> bool {
    meta["options"]["providerAttempt"] == json!({"version":1,"role":"browser-provider-attempt"})
}
pub(crate) fn needs_recovery(store: &SessionStore, parent: &Value) -> bool {
    let active = &parent["browser"]["consultation"]["active"];
    if active.is_null() {
        return false;
    }
    let Ok(link) = serde_json::from_value::<Link>(active.clone()) else {
        return true;
    };
    let Ok(child) = store.require(&link.child_id) else {
        return true;
    };
    if parent["id"] != link.parent_id || child["browser"]["attempt"] != *active {
        return true;
    }
    let runtime = &child["browser"]["runtime"];
    child["recoveryRequired"] == true
        || child.get("dispatch").is_some()
        || runtime["chromeTargetId"].is_string()
        || runtime["freshProfileOwner"].is_object()
            && !matches!(
                runtime["freshProfileDisposal"]["disposition"].as_str(),
                Some("removed" | "kept")
            )
}

/// Resolve endpoint evidence through reciprocal seals, without transferring target authority.
pub(crate) fn active_target(store: &SessionStore, parent: &Value) -> Result<Option<Value>> {
    if !parent["browser"]["consultation"].is_object() || internal(parent) {
        return Ok(None);
    }
    let link: Link = serde_json::from_value(parent["browser"]["consultation"]["active"].clone())?;
    parent_identity(parent, &link)?;
    require_storage_binding(store, &link)?;
    let child = store.stored_for_update(&link.child_id)?;
    require_payload(store, &link.child_id, parent, &child, &link)?;
    Ok(Some(child))
}

// Temporary attempts can only be observed in the exact original live target.
pub(crate) fn pinned_target(
    meta: &Value,
    reference: Option<&str>,
) -> Result<Option<(AttachmentEndpoint, String)>> {
    if !crate::browser_attempt::internal(meta) {
        return Ok(None);
    }
    let runtime = &meta["browser"]["runtime"];
    let endpoint = AttachmentEndpoint {
        host: runtime["chromeHost"]
            .as_str()
            .context("Provider inspection host missing")?
            .into(),
        port: runtime["chromePort"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .context("Provider inspection port missing")?,
        browser_ws_endpoint: runtime["browserWSEndpoint"]
            .as_str()
            .context("Provider inspection pinned socket missing")?
            .into(),
    };
    endpoint.validate()?;
    let target = runtime["chromeTargetId"]
        .as_str()
        .context("Provider inspection target missing")?;
    let owned = &runtime["ownedRecoveryTarget"];
    ensure!(
        crate::browser_target::valid_id(target)
            && runtime["chromeBrowserWSEndpoint"] == endpoint.browser_ws_endpoint
            && owned["browserWSEndpoint"] == endpoint.browser_ws_endpoint
            && owned["targetId"] == target
            && owned["port"] == endpoint.port
            && owned["host"]
                .as_str()
                .is_some_and(|host| crate::browser::normalise_host(host)
                    == crate::browser::normalise_host(&endpoint.host)),
        "Provider inspection endpoint differs from the original owned target"
    );
    ensure!(
        reference.is_none_or(|reference| reference == target),
        "Provider inspection requires its exact original target ID"
    );
    Ok(Some((endpoint, target.into())))
}
pub(crate) fn require_if_child(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let request = browser_runner::request(store, id)?;
    if is_child(&store.require(id)?, &request) {
        require_child(store, id, cancel)?;
    }
    Ok(())
}

fn request_hash(request: &BrowserRequest) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(request)?))
}
fn parent_identity(meta: &Value, link: &Link) -> Result<()> {
    ensure!(
        link.version == 1
            && link.parent_id != link.child_id
            && link.ordinal > 0
            && Uuid::parse_str(&link.coordinator_id).is_ok()
            && Uuid::parse_str(&link.attempt_id).is_ok()
            && meta["id"] == link.parent_id
            && meta["mode"] == "browser"
            && meta.get("dispatch").is_none()
            && controller_identity(meta) == link.parent_controller
            && sha256(&serde_json::to_vec(&meta["options"])?) == link.parent_options_sha256,
        "Provider consultation identity or options changed"
    );
    let state = &meta["browser"]["consultation"];
    ensure!(
        state["version"] == 1
            && state["id"] == link.coordinator_id
            && state["parentOptionsSha256"] == link.parent_options_sha256
            && state["parentRequestSha256"] == link.parent_request_sha256
            && state["active"] == json!(link),
        "Provider attempt is no longer the active consultation attempt"
    );
    let attempts = state["attempts"]
        .as_array()
        .context("Provider attempt history missing")?;
    ensure!(
        attempts.len() == link.ordinal as usize && attempts.last() == Some(&json!(link)),
        "Provider attempt history changed"
    );
    Ok(())
}

fn running_parent(meta: &Value, link: &Link) -> Result<()> {
    parent_identity(meta, link)?;
    let state = &meta["browser"]["consultation"];
    ensure!(
        meta["status"] == "running"
            && state["callbackIssued"] == false
            && state["selected"].is_null()
            && state["adoption"].is_null(),
        "Provider attempt no longer has dispatch authority"
    );
    Ok(())
}

fn require_parent_binding(store: &SessionStore, parent: &Value, link: &Link) -> Result<()> {
    running_parent(parent, link)?;
    require_storage_binding(store, link)
}
fn require_storage_binding(store: &SessionStore, link: &Link) -> Result<()> {
    ensure!(
        NativeEntry::open(&store.directory(&link.parent_id)?, true)?.identity
            == link.parent_directory
            && NativeEntry::open(&store.directory(&link.child_id)?, true)?.identity
                == link.child_directory,
        "Provider parent or child native directory changed"
    );
    ensure!(
        request_hash(&browser_runner::request(store, &link.parent_id)?)?
            == link.parent_request_sha256,
        "Committed parent input changed after attempt reservation"
    );
    Ok(())
}

/// A single worker may reserve attempts. A partially written link remains a barrier.
pub(crate) struct Parent {
    store: SessionStore,
    id: String,
    witness: Arc<ExecutionWitness>,
    request: BrowserRequest,
    state: Value,
    controller: Value,
    directory: NativeIdentity,
}
impl Parent {
    pub(crate) fn begin(
        store: &SessionStore,
        id: &str,
        identity: &ProcessIdentity,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        ensure!(
            !cancel.is_cancelled(),
            "Provider consultation was cancelled"
        );
        let witness = Arc::new(ExecutionWitness::capture(store, id, identity)?);
        let request = browser_runner::request(store, id)?;
        let current = store.stored_for_update(id)?;
        witness.require(&current)?;
        ensure!(
            current["status"] == "running"
                && current["mode"] == "browser"
                && current.get("dispatch").is_none()
                && !is_child(&current, &request)
                && current["browser"]["consultation"].is_null(),
            "Consultation already has execution or attempt evidence; refusing replay"
        );
        let state = json!({"version":1,"id":Uuid::new_v4().to_string(),
            "parentOptionsSha256":request.options_sha256,"parentRequestSha256":request_hash(&request)?,
            "attempts":[],"active":null,"ready":null,"selected":null,"callbackIssued":false});
        store.update(id, |meta| {
            witness.require(meta)?;
            ensure!(
                !cancel.is_cancelled() && *meta == current,
                "Consultation changed before coordinator reservation"
            );
            Ok(Some(
                json!({"browser":merge_object(&meta["browser"],&json!({"consultation":state}))}),
            ))
        })?;
        Ok(Self {
            store: store.clone(),
            id: id.into(),
            witness,
            request,
            state,
            controller: controller_identity(&current),
            directory: NativeEntry::open(&store.directory(id)?, true)?.identity,
        })
    }

    fn require(&self, meta: &Value, cancel: &CancellationToken) -> Result<()> {
        self.witness.require(meta)?;
        let committed = browser_runner::request(&self.store, &self.id)?;
        let mut current_state = meta["browser"]["consultation"].clone();
        let fallback = current_state
            .as_object_mut()
            .context("Consultation state missing")?
            .remove("fallback")
            .filter(|value| !value.is_null());
        let mut expected = self.state.clone();
        expected
            .as_object_mut()
            .context("Consultation state missing")?
            .remove("fallback");
        ensure!(
            !cancel.is_cancelled()
                && meta["status"] == "running"
                && meta.get("dispatch").is_none()
                && current_state == expected
                && controller_identity(meta) == self.controller
                && committed.options_sha256 == self.request.options_sha256
                && request_hash(&committed)? == self.state["parentRequestSha256"],
            "Consultation changed or was cancelled; refusing another attempt"
        );
        if fallback.is_some() {
            let child = meta["browser"]["consultation"]["active"]["childId"]
                .as_str()
                .context("Fallback child missing")?;
            let child_meta = self.store.require(child)?;
            let link: Link = serde_json::from_value(child_meta["browser"]["attempt"].clone())?;
            parent_identity(meta, &link)?;
            require_storage_binding(&self.store, &link)?;
            require_payload(&self.store, child, meta, &child_meta, &link)?;
        }
        Ok(())
    }

    pub(crate) fn reserve_first(&mut self, cancel: &CancellationToken) -> Result<Link> {
        ensure!(
            self.state["attempts"].as_array().is_some_and(Vec::is_empty)
                && self.state["active"].is_null(),
            "The first provider attempt was already reserved"
        );
        self.reserve(Provider::Chatgpt, None, cancel)
    }

    /// Component fixtures exercise Grok without inventing a production fallback
    /// permit or sending a preliminary request to ChatGPT.
    #[cfg(test)]
    pub(crate) fn reserve_grok_fixture(
        &mut self,
        environment: &crate::config::Environment,
        cancel: &CancellationToken,
    ) -> Result<Link> {
        self.reserve(Provider::Grok, Some(environment), cancel)
    }

    /// The single consented provider switch (reference `withGrokFallback`). Only a typed
    /// ChatGPT failure whose bounded read-only original-target recovery expired qualifies;
    /// every other failure or request keeps the original error. The failed child stays
    /// inert evidence with its retained target; it is never replayed or reactivated.
    pub(crate) fn switch_to_grok(
        &mut self,
        failed: &Link,
        error: &anyhow::Error,
        environment: &crate::config::Environment,
        cancel: &CancellationToken,
    ) -> Result<Option<Link>> {
        let Some(failure) = switch_failure(error) else {
            return Ok(None);
        };
        if !self.switch_eligible(&self.store.require(&self.id)?, failed, cancel)? {
            return Ok(None);
        }
        let child = self.store.require(&failed.child_id)?;
        let decision = json!({"version":1,"from":failed,"to":"grok","failure":failure,
            "dispatch":"uncertain","recovery":"expired","decidedAt":crate::sessions::timestamp(),
            "retainedTarget":child["browser"]["runtime"]["chromeTargetId"]});
        let reserved = self.reserve_with(
            Provider::Grok,
            Some(environment),
            Some((failed, decision)),
            cancel,
        );
        match reserved {
            Ok(link) => Ok(Some(link)),
            // The decision is durable only with its Grok reservation; restore the
            // original attempt so its read-only recovery remains available.
            Err(error) if self.state["switch"].is_object() => {
                Err(match self.revert_unsent_switch("grok-reservation-failed") {
                    Ok(()) => error.context("Provider switch decided but its Grok reservation failed; the ChatGPT attempt is restored for recovery"),
                    Err(revert) => error.context(format!("Provider switch decided but its Grok reservation failed and the ChatGPT attempt could not be restored ({revert:#})")),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// After a Grok attempt that never started its single Send, make the original
    /// ChatGPT attempt active again so public recovery can still read its target.
    /// The decision record stays, so no second switch is possible. Restoring
    /// evidence is not a new attempt, so it proceeds after cancellation.
    pub(crate) fn revert_unsent_switch(&mut self, reason: &str) -> Result<()> {
        let mut restored = None;
        self.store.update(&self.id, |current| {
            self.witness.require(current)?;
            ensure!(
                current["browser"]["consultation"] == self.state,
                "Provider switch changed before its restoration"
            );
            let state = unsent_restoration(&self.store, current, reason)?;
            restored = Some(state.clone());
            Ok(Some(
                json!({"browser":merge_object(&current["browser"],&json!({"consultation":state}))}),
            ))
        })?;
        self.state = restored.context("Provider switch restoration was not written")?;
        Ok(())
    }

    fn switch_eligible(
        &self,
        parent: &Value,
        failed: &Link,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let config = &parent["browser"]["config"];
        let state = &parent["browser"]["consultation"];
        // Ineligible failures keep their own error: no attempt record is read for them.
        if !(!cancel.is_cancelled()
            && config["fallbackProvider"] == "grok"
            && matches!(
                config["fallbackConsentSource"].as_str(),
                Some("request" | "personal-config")
            )
            && config["resumeConversationUrl"].is_null()
            && config["researchMode"] != "deep"
            && config["captureProviderNative"] != true
            && parent["options"]["writeArtifacts"] != true
            && crate::browser_grok::require_text_request(&self.request, &parent["options"]).is_ok()
            && failed.provider == Provider::Chatgpt
            && failed.ordinal == 1
            && state["active"] == json!(failed)
            && state["attempts"] == json!([failed])
            && state["switch"].is_null()
            && state["fallback"].is_null()
            && state["callbackIssued"] == false
            && state["selected"].is_null()
            && state["adoption"].is_null())
        {
            return Ok(false);
        }
        let child = self.store.require(&failed.child_id)?;
        let recovery = &child["browser"]["runtime"]["providerReadRecovery"];
        Ok(child["browser"]["attempt"] == json!(failed)
            && child["status"] == "error"
            && child["capturedAnswer"].is_null()
            && child["dispatch"]["state"] == "uncertain"
            && recovery["state"] == "expired"
            && recovery["readOnly"] == true
            && recovery["targetId"].is_string()
            && recovery["targetId"] == child["browser"]["runtime"]["chromeTargetId"])
    }

    fn reserve(
        &mut self,
        provider: Provider,
        environment: Option<&crate::config::Environment>,
        cancel: &CancellationToken,
    ) -> Result<Link> {
        self.reserve_with(provider, environment, None, cancel)
    }

    /// A provider switch decision is written in the same parent update that makes
    /// its Grok child active, so neither can exist without the other.
    fn reserve_with(
        &mut self,
        provider: Provider,
        environment: Option<&crate::config::Environment>,
        decision: Option<(&Link, Value)>,
        cancel: &CancellationToken,
    ) -> Result<Link> {
        let parent = self.store.require(&self.id)?;
        self.witness.require_worker(&parent)?;
        self.require(&parent, cancel)?;
        ensure!(
            parent["browser"]["consultation"]["adoption"].is_null()
                && parent["capturedAnswer"].is_null()
                && parent["browser"]["capturedAnswer"].is_null(),
            "Captured or adopting consultation cannot reserve another attempt"
        );
        let ordinal = u32::try_from(
            self.state["attempts"]
                .as_array()
                .context("Missing attempts")?
                .len(),
        )?
        .checked_add(1)
        .context("Provider attempt count overflow")?;
        let mut options = parent["options"].clone();
        // Children use only committed BrowserRequest bytes, never the original source paths.
        for field in ["slug", "writeOutputPath", "generateImage", "outputPath"] {
            options
                .as_object_mut()
                .context("Missing parent options")?
                .remove(field);
        }
        options["file"] = json!([]);
        options["files"] = json!([]);
        options["writeArtifacts"] = json!(false);
        options["browserFallback"] = json!("none");
        options["providerAttempt"] = json!({"version":1,"role":"browser-provider-attempt"});
        options["browserConfig"] = match provider {
            Provider::Chatgpt => parent["browser"]["config"].clone(),
            Provider::Grok => {
                crate::browser_grok::require_text_request(&self.request, &parent["options"])?;
                crate::browser_grok::child_config(
                    &parent["browser"]["config"],
                    environment
                        .context("Grok profile requires the original execution environment")?,
                    Path::new(parent["cwd"].as_str().context("Consultation cwd missing")?),
                )?
            }
        };
        options["browserConfig"]["fallbackProvider"] = json!("none");
        let mut candidate = self.store.create(
            options,
            Path::new(parent["cwd"].as_str().context("Consultation cwd missing")?),
            Some(&format!("{}-attempt-{ordinal}", self.id)),
        )?;
        let child_id = candidate["id"]
            .as_str()
            .context("Child session identity missing")?
            .to_owned();
        let mut child_options = candidate["options"].clone();
        for field in ["generateImage", "outputPath"] {
            if parent["options"][field]
                .as_str()
                .is_some_and(|path| !path.is_empty())
            {
                child_options[field] = json!(
                    self.store
                        .directory(&child_id)?
                        .join("artifacts/attempt-requested-image")
                );
            }
        }
        if child_options != candidate["options"] {
            candidate = self.store.update(&child_id, |current| {
                self.require(&self.store.require(&self.id)?, cancel)?;
                ensure!(
                    *current == candidate,
                    "Pending image attempt changed before local destination binding"
                );
                Ok(Some(json!({"options":child_options})))
            })?;
        }
        let link = Link {
            version: 1,
            coordinator_id: self.state["id"]
                .as_str()
                .context("Coordinator identity missing")?
                .into(),
            attempt_id: Uuid::new_v4().to_string(),
            parent_id: self.id.clone(),
            child_id: child_id.clone(),
            ordinal,
            provider,
            parent_options_sha256: self.request.options_sha256.clone(),
            parent_request_sha256: request_hash(&self.request)?,
            parent_controller: self.controller.clone(),
            parent_directory: self.directory.clone(),
            child_directory: NativeEntry::open(&self.store.directory(&child_id)?, true)?.identity,
        };
        let mut proposed = self.state.clone();
        if let Some((_, decision)) = &decision {
            proposed["switch"] = decision.clone();
        }
        proposed["active"] = json!(link);
        proposed["attempts"]
            .as_array_mut()
            .context("Missing attempt history")?
            .push(json!(link));
        // No child lock is held whilst reserving its exact identity in the parent.
        self.store.update(&self.id, |current| {
            self.require(current,cancel)?;
            if let Some((failed, _)) = &decision {
                ensure!(self.switch_eligible(current, failed, cancel)?,
                    "Provider switch evidence changed before its decision");
            }
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"consultation":proposed}))})))
        })?;
        self.state = proposed;
        #[cfg(test)]
        ensure!(
            !TEST_FAIL_AFTER_PARENT_RESERVATION.replace(false),
            "PUBLIC injected reservation failure"
        );
        self.store.update(&child_id, |current| {
            running_parent(&self.store.require(&self.id)?, &link)?;
            ensure!(
                !cancel.is_cancelled() && *current == candidate,
                "Pending provider child changed before reciprocal link"
            );
            Ok(Some(
                json!({"browser":merge_object(&current["browser"],&json!({"attempt":link}))}),
            ))
        })?;
        let child_request = self
            .request
            .recapture_into(&self.store, &self.id, &child_id)?;
        crate::storage::atomic_json(
            &self
                .store
                .directory(&child_id)?
                .join("browser-request.json"),
            &child_request,
        )?;
        let ready = Ready {
            version: 1,
            link: link.clone(),
            child_request_sha256: request_hash(&child_request)?,
            child_options_sha256: child_request.options_sha256.clone(),
        };
        self.store.update(&child_id, |current| {
            self.require(&self.store.require(&self.id)?, cancel)?;
            ensure!(
                current["status"] == "pending"
                    && current.get("dispatch").is_none()
                    && current["browser"]["attempt"] == json!(link)
                    && current["browser"]["attemptReady"].is_null()
                    && request_hash(&browser_runner::request(&self.store, &child_id)?)?
                        == ready.child_request_sha256,
                "Provider child changed before ready seal"
            );
            Ok(Some(
                json!({"browser":merge_object(&current["browser"],&json!({"attemptReady":ready}))}),
            ))
        })?;
        let mut ready_state = self.state.clone();
        ready_state["ready"] = json!(ready);
        self.store.update(&self.id,|current| {
            self.require(current,cancel)?;
            ensure!(self.store.require(&child_id)?["browser"]["attemptReady"]==json!(ready),
                "Provider child ready seal changed before parent admission");
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"consultation":ready_state}))})))
        })?;
        self.state = ready_state;
        require_child(&self.store, &child_id, cancel)?;
        self.require(&self.store.require(&self.id)?, cancel)?;
        Ok(link)
    }
}

#[cfg(test)]
thread_local! {
    /// Fails this thread's next reservation after its parent write, before the child link.
    static TEST_FAIL_AFTER_PARENT_RESERVATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A switch's Grok attempt that has the parent's owner, or none, and positively never sent.
/// A missing dispatch, or the not-started record failure writes only when none exists, proves
/// no Send: the started record precedes the single physical Send.
pub(super) fn unsent_grok(parent: &Value, child: &Value) -> bool {
    let owner = &child["lifecycle"]["processIdentity"];
    (owner.is_null() || *owner == parent["lifecycle"]["processIdentity"])
        && child["capturedAnswer"].is_null()
        && (child.get("dispatch").is_none() || child["dispatch"]["state"] == "not-started")
}

/// The pre-switch consultation, rebuilt from durable evidence: the switch only changed
/// active, attempts and ready. Refused unless the Grok attempt positively never sent.
pub(super) fn unsent_restoration(
    store: &SessionStore,
    parent: &Value,
    reason: &str,
) -> Result<Value> {
    let state = &parent["browser"]["consultation"];
    let switch = &state["switch"];
    let from: Link = serde_json::from_value(switch["from"].clone())?;
    let grok: Link = serde_json::from_value(state["active"].clone())?;
    ensure!(
        switch["version"] == 1
            && switch["outcome"].is_null()
            && from.provider == Provider::Chatgpt
            && from.ordinal == 1
            && grok.provider == Provider::Grok
            && grok.ordinal == 2
            && state["attempts"] == json!([from, grok])
            && state["fallback"].is_null()
            && state["callbackIssued"] == false
            && state["selected"].is_null()
            && state["adoption"].is_null()
            && parent["capturedAnswer"].is_null()
            && parent["browser"]["capturedAnswer"].is_null(),
        "Provider switch cannot be restored"
    );
    ensure!(
        unsent_grok(parent, &store.require(&grok.child_id)?),
        "Grok attempt may have sent or has another owner; its record and target are retained"
    );
    let original = store.require(&from.child_id)?;
    let ready = &original["browser"]["attemptReady"];
    ensure!(
        original["browser"]["attempt"] == json!(from) && ready["link"] == json!(from),
        "Original ChatGPT attempt changed after the provider switch"
    );
    let mut restored = state.clone();
    restored["active"] = json!(from);
    restored["attempts"] = json!([from]);
    restored["ready"] = ready.clone();
    restored["switch"]["outcome"] = json!({"state":"restored","reason":reason,
        "grokChildId":grok.child_id,"restoredAt":crate::sessions::timestamp()});
    Ok(restored)
}

/// Typed failures only; error prose never qualifies. A contradictory commitment never does.
fn switch_failure(error: &anyhow::Error) -> Option<&'static str> {
    use crate::browser_provider::Failure;
    match error.downcast_ref::<Failure>()? {
        Failure::CommitDeadline => Some("commit-deadline"),
        Failure::AnswerDeadline => Some("answer-deadline"),
        Failure::ResponseLoadFailed => Some("response-load-failed"),
        Failure::CommitAmbiguous(_) | Failure::ContentWithheld(_) | Failure::RetryOffered => None,
    }
}

/// Validate both sides before starting or allocating for a child. It never repairs a link.
pub(crate) fn require_child(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<Link> {
    ensure!(!cancel.is_cancelled(), "Provider attempt was cancelled");
    let child = store.require(id)?;
    let link: Link = serde_json::from_value(child["browser"]["attempt"].clone())?;
    ensure!(
        link.version == 1
            && link.child_id == id
            && link.parent_id != id
            && Uuid::parse_str(&link.coordinator_id).is_ok()
            && Uuid::parse_str(&link.attempt_id).is_ok(),
        "Invalid provider attempt link"
    );
    let parent = store.require(&link.parent_id)?;
    require_parent_binding(store, &parent, &link)?;
    require_payload(store, id, &parent, &child, &link)?;
    Ok(link)
}
fn require_payload(
    store: &SessionStore,
    id: &str,
    parent: &Value,
    child: &Value,
    link: &Link,
) -> Result<()> {
    let committed = browser_runner::request(store, id)?;
    let ready: Ready = serde_json::from_value(child["browser"]["attemptReady"].clone())?;
    if link.provider == Provider::Grok {
        ensure!(
            child["browser"]["config"] == child["options"]["browserConfig"],
            "Grok saved config differs from its sealed options"
        );
        crate::browser_grok::require_config(&child["browser"]["config"])?;
        crate::browser_grok::require_text_request(&committed, &child["options"])?;
    }
    ensure!(
        ready.version == 1
            && child["id"] == id
            && link.child_id == id
            && child["browser"]["attempt"] == json!(link)
            && ready.link == *link
            && parent["browser"]["consultation"]["ready"] == json!(ready)
            && committed.extra.get("providerAttempt") == Some(&json!(link))
            && child["options"]["providerAttempt"]
                == json!({"version":1,"role":"browser-provider-attempt"})
            && committed.options_sha256 == ready.child_options_sha256,
        "Provider child payload or reciprocal ready seal changed"
    );
    let fallback = &parent["browser"]["consultation"]["fallback"];
    if fallback.is_null() {
        ensure!(
            child["browser"]["attemptFallback"].is_null()
                && request_hash(&committed)? == ready.child_request_sha256,
            "Provider child payload changed after ready seal"
        );
    } else {
        ensure!(
            fallback["version"] == 1
                && fallback["link"] == json!(link)
                && child["browser"]["attemptFallback"] == *fallback
                && fallback["originalRequestSha256"] == ready.child_request_sha256
                && fallback["derivedRequestSha256"] == request_hash(&committed)?
                && committed.fallback_used,
            "Provider fallback payload or reciprocal derivation seal changed"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{browser_inputs::StagedUploads, browser_prompt::Attachment, storage::atomic_json};
    use std::fs;

    fn parent() -> (tempfile::TempDir, SessionStore, ProcessIdentity) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path().join(".oracle"));
        let resolved = crate::execution::resolve(
            &json!({"prompt":"PUBLIC immutable consultation source","engine":"browser","model":"gpt-6-pro"}),
            &json!({"browser":{"cookieSync":false,"manualLogin":true}}),
            &crate::config::Environment::new(),root.path(),
        ).unwrap();
        let meta = browser_runner::prepare(&store, &resolved, root.path()).unwrap();
        let id = meta["id"].as_str().unwrap();
        let identity = ProcessIdentity::current().unwrap();
        browser_runner::start_owner(&store, id, &identity).unwrap();
        (root, store, identity)
    }
    fn id(store: &SessionStore) -> String {
        store.list(false).unwrap()[0]["id"].as_str().unwrap().into()
    }
    fn source(store: &SessionStore, parent: &str, name: &str, bytes: &[u8]) -> StagedUploads {
        let path = store.directory(parent).unwrap().join(name);
        fs::write(&path, bytes).unwrap();
        StagedUploads::capture(
            store,
            parent,
            &[Attachment {
                path,
                display_path: name.into(),
                size_bytes: bytes.len() as u64,
                generated_bundle: false,
            }],
        )
        .unwrap()
    }
    fn save_request(store: &SessionStore, request: &BrowserRequest) {
        atomic_json(
            &store
                .directory(&request.session_id)
                .unwrap()
                .join("browser-request.json"),
            request,
        )
        .unwrap();
    }
    #[test]
    fn grok_child_profile_is_sealed_and_attachment_refusal_precedes_child_creation() {
        for attached in [false, true] {
            let (root, store, identity) = parent();
            let id = id(&store);
            if attached {
                let mut request = browser_runner::request(&store, &id).unwrap();
                let staged = source(&store, &id, "PUBLIC.txt", b"PUBLIC attachment");
                request.upload_paths = staged.paths().unwrap();
                request.staged_uploads = Some(staged);
                save_request(&store, &request);
            }
            let original = store.require(&id).unwrap();
            let original_request =
                std::fs::read(store.directory(&id).unwrap().join("browser-request.json")).unwrap();
            let environment = crate::config::Environment::from([
                (
                    "ORACLE_HOME_DIR".into(),
                    store.home().to_string_lossy().into(),
                ),
                (
                    (if cfg!(windows) { "USERPROFILE" } else { "HOME" }).into(),
                    root.path().to_string_lossy().into(),
                ),
            ]);
            let cancel = CancellationToken::new();
            let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
            let reserved = coordinator.reserve(Provider::Grok, Some(&environment), &cancel);
            if attached {
                assert!(reserved.is_err());
                assert_eq!(store.list(false).unwrap().len(), 1);
                assert!(
                    store.require(&id).unwrap()["browser"]["consultation"]["attempts"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            } else {
                let link = reserved.unwrap();
                assert_eq!(
                    require_child(&store, &link.child_id, &cancel).unwrap(),
                    link
                );
                let child = store.require(&link.child_id).unwrap();
                assert_eq!(
                    child["browser"]["config"],
                    child["options"]["browserConfig"]
                );
                assert_eq!(child["browser"]["config"]["cookieSync"], false);
                store.update(&link.child_id,|meta|Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"config":merge_object(&meta["browser"]["config"],&json!({"keepBrowser":true}))}))})))).unwrap();
                assert!(require_child(&store, &link.child_id, &cancel).is_err());
            }
            assert_eq!(store.require(&id).unwrap()["options"], original["options"]);
            assert_eq!(
                std::fs::read(store.directory(&id).unwrap().join("browser-request.json")).unwrap(),
                original_request
            );
        }
    }
    #[test]
    fn failed_preflight_has_no_recovery_until_the_child_has_owned_browser_evidence() {
        let (_root, store, identity) = parent();
        let id = id(&store);
        let cancel = CancellationToken::new();
        let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
        let link = coordinator.reserve_first(&cancel).unwrap();
        let parent = store.require(&id).unwrap();
        assert!(!needs_recovery(&store, &parent));
        let child = store.require(&link.child_id).unwrap();
        assert!(internal(&child) && !internal(&parent));
        store
            .update(&link.child_id, |_| {
                Ok(Some(
                    json!({"status":"error","errorMessage":"PUBLIC failed before launch"}),
                ))
            })
            .unwrap();
        assert!(!needs_recovery(&store, &parent));
        for patch in [
            json!({"recoveryRequired":true}),
            json!({"dispatch":{"state":"started"}}),
            json!({"browser":{"runtime":{"chromeTargetId":"PUBLIC-owned"}}}),
            json!({"browser":{"runtime":{"freshProfileOwner":{"sessionId":link.child_id}}}}),
        ] {
            store
                .update(&link.child_id, |_| Ok(Some(merge_object(&child, &patch))))
                .unwrap();
            assert!(needs_recovery(&store, &parent), "{patch}");
            crate::storage::atomic_json(
                &store.directory(&link.child_id).unwrap().join("meta.json"),
                &child,
            )
            .unwrap();
        }
        let mut foreign = parent;
        foreign["browser"]["consultation"]["active"]["childId"] = json!("PUBLIC-missing");
        assert!(needs_recovery(&store, &foreign));
    }

    #[test]
    fn derived_request_needs_both_commitments_and_rejects_changed_payload_or_cancellation() {
        for mutation in [
            "reserved",
            "request-only",
            "seal-only",
            "complete",
            "cancel",
            "payload",
            "parent-seal",
        ] {
            let (_root, store, identity) = parent();
            let parent_id = id(&store);
            let mut original = browser_runner::request(&store, &parent_id).unwrap();
            original.prompt = "PUBLIC ".repeat(8_000);
            original.prompt_sha256 = sha256(original.prompt.as_bytes());
            original.fallback = Some(browser_runner::BrowserFallback {
                composer_text: "PUBLIC shorter attachment request".into(),
                pending_bundle: None,
                text_sources: vec![],
                raw_source: None,
                remote_source: None,
            });
            save_request(&store, &original);
            let cancel = CancellationToken::new();
            let mut coordinator = Parent::begin(&store, &parent_id, &identity, &cancel).unwrap();
            let link = coordinator.reserve_first(&cancel).unwrap();
            browser_runner::start_owner(&store, &link.child_id, &identity).unwrap();
            let previous = browser_runner::request(&store, &link.child_id).unwrap();
            let mut next = previous.clone();
            next.prompt = previous.fallback.as_ref().unwrap().composer_text.clone();
            next.prompt_sha256 = sha256(next.prompt.as_bytes());
            let staged = source(
                &store,
                &link.child_id,
                "PUBLIC-derived.txt",
                previous.prompt.as_bytes(),
            );
            next.upload_paths = staged.paths().unwrap();
            next.staged_uploads = Some(staged);
            next.fallback_used = true;
            let ticket = reserve_fallback(
                &store,
                &link.child_id,
                &previous,
                &next,
                &crate::browser_dom::PromptTooLarge {
                    prompt_length: previous.prompt.encode_utf16().count(),
                    observed_length: 1_000,
                },
                &cancel,
            )
            .unwrap()
            .unwrap();
            assert!(require_child(&store, &link.child_id, &cancel).is_err());
            ticket
                .require(
                    &store,
                    &store.require(&link.child_id).unwrap(),
                    &previous,
                    &next,
                    &cancel,
                )
                .unwrap();
            match mutation {
                "request-only" => save_request(&store, &next),
                "seal-only" => {
                    store.update(&link.child_id, |meta| Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"attemptFallback":ticket.seal()}))})))).unwrap();
                }
                "complete" => {
                    store.update(&link.child_id, |meta| {
                        ticket.require(&store, meta, &previous, &next, &cancel)?;
                        save_request(&store, &next);
                        Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"attemptFallback":ticket.seal()}))})))
                    }).unwrap();
                    require_child(&store, &link.child_id, &cancel).unwrap();
                    coordinator
                        .require(&store.require(&parent_id).unwrap(), &cancel)
                        .unwrap();
                }
                "cancel" => cancel.cancel(),
                "payload" => {
                    next.prompt.push_str(" PUBLIC replacement");
                    next.prompt_sha256 = sha256(next.prompt.as_bytes());
                }
                "parent-seal" => {
                    store.update(&parent_id, |meta| {
                    let mut state=meta["browser"]["consultation"].clone();
                    state["fallback"]["derivedRequestSha256"]=json!("PUBLIC replacement");
                    Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"consultation":state}))})))
                }).unwrap();
                }
                "reserved" => {}
                _ => unreachable!(),
            }
            if mutation != "complete" {
                assert!(
                    require_child(&store, &link.child_id, &cancel).is_err(),
                    "{mutation}"
                );
                assert!(
                    coordinator
                        .require(&store.require(&parent_id).unwrap(), &cancel)
                        .is_err(),
                    "{mutation}"
                );
            }
            if matches!(mutation, "cancel" | "payload" | "parent-seal") {
                assert!(
                    ticket
                        .require(
                            &store,
                            &store.require(&link.child_id).unwrap(),
                            &previous,
                            &next,
                            &cancel
                        )
                        .is_err(),
                    "{mutation}"
                );
            }
            assert!(
                store
                    .require(&link.child_id)
                    .unwrap()
                    .get("dispatch")
                    .is_none()
            );
        }
    }

    #[test]
    fn bundle_path_is_rebound_to_the_child_committed_bundle() {
        let (_root, store, identity) = parent();
        let id = id(&store);
        let mut staged = source(&store, &id, "PUBLIC-bundle.txt", b"PUBLIC bundled bytes");
        staged.files[0].generated_bundle = true;
        let mut request = browser_runner::request(&store, &id).unwrap();
        request.upload_paths = staged.paths().unwrap();
        request.extra.insert(
            "bundled".into(),
            json!({"bundlePath":request.upload_paths[0],"originalCount":3}),
        );
        request.staged_uploads = Some(staged);
        save_request(&store, &request);
        let cancel = CancellationToken::new();
        let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
        let link = coordinator.reserve_first(&cancel).unwrap();
        let child = browser_runner::request(&store, &link.child_id).unwrap();
        assert_eq!(
            child.extra["bundled"]["bundlePath"],
            json!(child.upload_paths[0])
        );
        assert_ne!(
            child.extra["bundled"]["bundlePath"],
            request.extra["bundled"]["bundlePath"]
        );
        assert_eq!(child.extra["bundled"]["originalCount"], 3);
        require_child(&store, &link.child_id, &cancel).unwrap();
    }
    #[test]
    fn first_child_has_reciprocal_links_distinct_request_and_no_dispatch() {
        let (_root, store, identity) = parent();
        let id = id(&store);
        let cancel = CancellationToken::new();
        let original = browser_runner::request(&store, &id).unwrap();
        let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
        assert!(Parent::begin(&store, &id, &identity, &cancel).is_err());
        // SessionStore may suffix the candidate. The link must use the returned ID.
        store
            .create(
                json!({"prompt":"PUBLIC unrelated collision"}),
                Path::new("."),
                Some(&format!("{id}-attempt-1")),
            )
            .unwrap();
        let link = coordinator.reserve_first(&cancel).unwrap();
        assert!(link.child_id.ends_with("-attempt-1-2"));
        assert_eq!(link.provider, Provider::Chatgpt);
        assert_eq!(
            require_child(&store, &link.child_id, &cancel).unwrap(),
            link
        );
        let child = browser_runner::request(&store, &link.child_id).unwrap();
        assert_eq!(child.prompt, original.prompt);
        assert_ne!(child.reservation_id, original.reservation_id);
        assert_eq!(child.session_id, link.child_id);
        assert_ne!(child.options_sha256, original.options_sha256);
        assert_eq!(store.require(&link.child_id).unwrap()["status"], "pending");
        assert!(
            store
                .require(&link.child_id)
                .unwrap()
                .get("dispatch")
                .is_none()
        );
        assert!(coordinator.reserve_first(&cancel).is_err());
        assert_eq!(
            serde_json::to_value(browser_runner::request(&store, &id).unwrap()).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }

    #[test]
    fn every_deferred_source_is_recaptured_with_child_native_identities() {
        let (_root, store, identity) = parent();
        let id = id(&store);
        let initial = source(&store, &id, "PUBLIC-initial.png", b"PUBLIC initial bytes");
        let text = source(&store, &id, "PUBLIC-deferred.txt", b"PUBLIC deferred text");
        let raw = source(&store, &id, "PUBLIC-deferred.png", b"PUBLIC deferred media");
        let remote = source(&store, &id, "PUBLIC-remote.txt", b"PUBLIC remote bytes");
        let mut request = browser_runner::request(&store, &id).unwrap();
        request.upload_paths = initial.paths().unwrap();
        request.staged_uploads = Some(initial.clone());
        request.fallback = Some(browser_runner::BrowserFallback {
            composer_text: "PUBLIC deferred composer".into(),
            pending_bundle: None,
            text_sources: vec![text.clone()],
            raw_source: Some(raw.clone()),
            remote_source: Some(remote.clone()),
        });
        save_request(&store, &request);
        let cancel = CancellationToken::new();
        let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
        let link = coordinator.reserve_first(&cancel).unwrap();
        let child = browser_runner::request(&store, &link.child_id).unwrap();
        let fallback = child.fallback.as_ref().unwrap();
        let copies = [
            child.staged_uploads.as_ref().unwrap(),
            &fallback.text_sources[0],
            fallback.raw_source.as_ref().unwrap(),
            fallback.remote_source.as_ref().unwrap(),
        ];
        for (before, after) in [&initial, &text, &raw, &remote].into_iter().zip(copies) {
            before.verify(&store, &id).unwrap();
            after.verify(&store, &link.child_id).unwrap();
            assert_ne!(before.directory, after.directory);
            assert_ne!(before.session_identity, after.session_identity);
            assert_eq!(
                before.open(0).unwrap().contents().unwrap(),
                after.open(0).unwrap().contents().unwrap()
            );
        }
        assert_eq!(fallback.composer_text, "PUBLIC deferred composer");
    }

    #[test]
    fn changed_staged_bytes_leave_the_reserved_child_blocked_without_dispatch() {
        let (_root, store, identity) = parent();
        let id = id(&store);
        let staged = source(&store, &id, "PUBLIC-source.txt", b"PUBLIC committed");
        let mut request = browser_runner::request(&store, &id).unwrap();
        request.upload_paths = staged.paths().unwrap();
        request.staged_uploads = Some(staged.clone());
        save_request(&store, &request);
        let cancel = CancellationToken::new();
        let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
        fs::write(&request.upload_paths[0], b"PUBLIC changed").unwrap();
        assert!(coordinator.reserve_first(&cancel).is_err());
        let parent = store.require(&id).unwrap();
        let child_id = parent["browser"]["consultation"]["active"]["childId"]
            .as_str()
            .unwrap();
        assert!(require_child(&store, child_id, &cancel).is_err());
        let child = store.require(child_id).unwrap();
        assert_eq!(child["status"], "pending");
        assert!(child.get("dispatch").is_none());
        assert!(coordinator.reserve_first(&cancel).is_err());
    }

    #[test]
    fn cancellation_changed_options_owner_or_links_never_admit_a_child() {
        for mutation in ["cancel", "options", "owner", "active", "reciprocal"] {
            let (_root, store, identity) = parent();
            let id = id(&store);
            let cancel = CancellationToken::new();
            let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
            let link = coordinator.reserve_first(&cancel).unwrap();
            match mutation {
                "cancel" => cancel.cancel(),
                "options" => {
                    store.update(&id,|meta|Ok(Some(json!({"options":merge_object(&meta["options"],&json!({"prompt":"PUBLIC changed options"}))})))).unwrap();
                }
                "owner" => {
                    store.update(&id,|meta|Ok(Some(json!({"lifecycle":merge_object(&meta["lifecycle"],&json!({"workerPid":1}))})))).unwrap();
                }
                "active" => {
                    store.update(&id,|meta|Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"consultation":merge_object(&meta["browser"]["consultation"],&json!({"active":null}))}))})))).unwrap();
                }
                "reciprocal" => {
                    store.update(&link.child_id,|meta|Ok(Some(json!({"browser":merge_object(&meta["browser"],&json!({"attempt":null}))})))).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                require_child(&store, &link.child_id, &cancel).is_err(),
                "{mutation}"
            );
            assert!(coordinator.reserve_first(&cancel).is_err(), "{mutation}");
            if mutation != "cancel" {
                assert!(
                    browser_runner::start_owner(&store, &link.child_id, &identity).is_err(),
                    "{mutation}: a damaged child was admitted as an ordinary session"
                );
            }
            assert!(
                store
                    .require(&link.child_id)
                    .unwrap()
                    .get("dispatch")
                    .is_none()
            );
        }
    }

    #[test]
    fn coherent_child_request_edits_do_not_replace_the_parent_ready_seal() {
        for field in ["prompt", "fallback", "extra"] {
            let (_root, store, identity) = parent();
            let id = id(&store);
            let cancel = CancellationToken::new();
            let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
            let link = coordinator.reserve_first(&cancel).unwrap();
            let mut request = browser_runner::request(&store, &link.child_id).unwrap();
            match field {
                "prompt" => {
                    request.prompt = "PUBLIC replaced attempt payload".into();
                    request.prompt_sha256 = sha256(request.prompt.as_bytes());
                }
                "fallback" => {
                    request.fallback = Some(browser_runner::BrowserFallback {
                        composer_text: "PUBLIC replaced deferred payload".into(),
                        pending_bundle: None,
                        text_sources: vec![],
                        raw_source: None,
                        remote_source: None,
                    });
                }
                "extra" => {
                    request.extra.insert("PUBLIC changed".into(), json!(true));
                }
                _ => unreachable!(),
            }
            save_request(&store, &request);
            // The ordinary request's self-consistency check cannot establish parent authority.
            browser_runner::request(&store, &link.child_id).unwrap();
            assert!(
                require_child(&store, &link.child_id, &cancel).is_err(),
                "{field}"
            );
            assert!(
                browser_runner::start_owner(&store, &link.child_id, &identity).is_err(),
                "{field}"
            );
            assert!(
                store
                    .require(&link.child_id)
                    .unwrap()
                    .get("dispatch")
                    .is_none()
            );
        }
    }

    struct Switched {
        _root: tempfile::TempDir,
        store: SessionStore,
        id: String,
        coordinator: Parent,
        first: Link,
        environment: crate::config::Environment,
        cancel: CancellationToken,
    }
    /// A ChatGPT child as `fail_owned` leaves it after its bounded recovery expired.
    fn switch_fixture(input: Value, personal: Value) -> Switched {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path().join(".oracle"));
        let mut request =
            json!({"prompt":"PUBLIC switch consultation","engine":"browser","model":"gpt-6-pro"});
        merge_into(&mut request, &input);
        let environment = crate::config::Environment::from([
            (
                "ORACLE_HOME_DIR".into(),
                store.home().to_string_lossy().into(),
            ),
            (
                (if cfg!(windows) { "USERPROFILE" } else { "HOME" }).into(),
                root.path().to_string_lossy().into(),
            ),
        ]);
        let mut browser = json!({"cookieSync":false,"manualLogin":true});
        merge_into(&mut browser, &personal);
        let resolved = crate::execution::resolve(
            &request,
            &json!({"browser":browser}),
            &environment,
            root.path(),
        )
        .unwrap();
        let meta = browser_runner::prepare(&store, &resolved, root.path()).unwrap();
        let id = meta["id"].as_str().unwrap().to_owned();
        let identity = ProcessIdentity::current().unwrap();
        browser_runner::start_owner(&store, &id, &identity).unwrap();
        let cancel = CancellationToken::new();
        let mut coordinator = Parent::begin(&store, &id, &identity, &cancel).unwrap();
        let first = coordinator.reserve_first(&cancel).unwrap();
        store
            .update(&first.child_id, |meta| {
                let mut runtime = meta["browser"]["runtime"].clone();
                if !runtime.is_object() {
                    runtime = json!({});
                }
                runtime["chromeTargetId"] = json!("PUBLIC-chatgpt-target");
                runtime["providerReadRecovery"] = json!({"version":1,"state":"expired",
                    "readOnly":true,"targetId":"PUBLIC-chatgpt-target"});
                Ok(Some(json!({"status":"error","recoveryRequired":true,
                    "dispatch":{"state":"uncertain","reservationId":"PUBLIC"},
                    "browser":merge_object(&meta["browser"],&json!({"runtime":runtime}))})))
            })
            .unwrap();
        Switched {
            _root: root,
            store,
            id,
            coordinator,
            first,
            environment,
            cancel,
        }
    }
    fn merge_into(target: &mut Value, patch: &Value) {
        for (key, value) in patch.as_object().unwrap() {
            target[key] = value.clone();
        }
    }
    fn typed() -> anyhow::Error {
        anyhow::Error::new(crate::browser_provider::Failure::AnswerDeadline)
            .context("Bounded original-target observation expired")
    }
    fn patch_meta(store: &SessionStore, id: &str, edit: impl Fn(&mut Value)) {
        let mut meta = store.require(id).unwrap();
        edit(&mut meta);
        crate::storage::atomic_json(&store.directory(id).unwrap().join("meta.json"), &meta)
            .unwrap();
    }

    #[test]
    fn only_a_typed_failure_after_expired_recovery_switches_once_to_grok() {
        use crate::browser_provider::Failure;
        let consent = json!({"browserFallback":"grok"});
        type Edit = fn(&SessionStore, &Switched);
        let none: Edit = |_, _| {};
        // (case, request input, personal browser config, error, state edit, expected)
        let cases: Vec<(&str, Value, Value, anyhow::Error, Edit, bool)> = vec![
            ("answer", consent.clone(), json!({}), typed(), none, true),
            (
                "commit",
                consent.clone(),
                json!({}),
                anyhow::Error::new(Failure::CommitDeadline),
                none,
                true,
            ),
            (
                "response-load",
                consent.clone(),
                json!({}),
                anyhow::Error::new(Failure::ResponseLoadFailed),
                none,
                true,
            ),
            (
                "personal-config",
                json!({}),
                json!({"fallbackProvider":"grok"}),
                typed(),
                none,
                true,
            ),
            (
                "consent-none",
                json!({"browserFallback":"none"}),
                json!({"fallbackProvider":"grok"}),
                typed(),
                none,
                false,
            ),
            ("no-consent", json!({}), json!({}), typed(), none, false),
            (
                "default-source",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.id, |meta| {
                        meta["browser"]["config"]["fallbackConsentSource"] = json!("default");
                    })
                },
                false,
            ),
            (
                "untyped",
                consent.clone(),
                json!({}),
                anyhow::anyhow!("PUBLIC answer deadline expired"),
                none,
                false,
            ),
            (
                "flattened",
                consent.clone(),
                json!({}),
                anyhow::anyhow!("{:#}", typed()),
                none,
                false,
            ),
            (
                "ambiguous",
                consent.clone(),
                json!({}),
                anyhow::Error::new(Failure::CommitAmbiguous("PUBLIC".into())),
                none,
                false,
            ),
            (
                "recovered",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.first.child_id, |meta| {
                        meta["browser"]["runtime"]["providerReadRecovery"]["state"] =
                            json!("recovered");
                    })
                },
                false,
            ),
            (
                "other-target",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.first.child_id, |meta| {
                        meta["browser"]["runtime"]["providerReadRecovery"]["targetId"] =
                            json!("PUBLIC-other-target");
                    })
                },
                false,
            ),
            (
                "not-started",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.first.child_id, |meta| {
                        meta["dispatch"]["state"] = json!("not-started");
                    })
                },
                false,
            ),
            (
                "captured",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.first.child_id, |meta| {
                        meta["capturedAnswer"] = json!({"file":"PUBLIC"});
                    })
                },
                false,
            ),
            (
                "callback-issued",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.id, |meta| {
                        meta["browser"]["consultation"]["callbackIssued"] = json!(true);
                    })
                },
                false,
            ),
            (
                "upload-fallback",
                consent.clone(),
                json!({}),
                typed(),
                |store, f| {
                    patch_meta(store, &f.id, |meta| {
                        meta["browser"]["consultation"]["fallback"] = json!({"version":1});
                    })
                },
                false,
            ),
            (
                "cancelled",
                consent.clone(),
                json!({}),
                typed(),
                |_, f| f.cancel.cancel(),
                false,
            ),
            (
                "follow-up",
                json!({"browserFallback":"grok","browserFollowUps":["PUBLIC second turn"]}),
                json!({}),
                typed(),
                none,
                false,
            ),
            (
                "write-artifacts",
                json!({"browserFallback":"grok","writeArtifacts":true}),
                json!({}),
                typed(),
                none,
                false,
            ),
        ];
        for (case, input, personal, error, edit, expected) in cases {
            let mut f = switch_fixture(input, personal);
            edit(&f.store, &f);
            let store = f.store.clone();
            let before = store.require(&f.id).unwrap();
            let first = f.first.clone();
            let (environment, cancel) = (f.environment.clone(), f.cancel.clone());
            let switched = f
                .coordinator
                .switch_to_grok(&first, &error, &environment, &cancel)
                .unwrap();
            assert_eq!(switched.is_some(), expected, "{case}");
            let state = store.require(&f.id).unwrap()["browser"]["consultation"].clone();
            if !expected {
                assert_eq!(
                    store.require(&f.id).unwrap(),
                    before,
                    "{case} changed the parent"
                );
                assert_eq!(
                    store.list(false).unwrap().len(),
                    2,
                    "{case} created a child"
                );
                continue;
            }
            let grok = switched.unwrap();
            assert!(
                grok.provider == Provider::Grok && grok.ordinal == 2,
                "{case}"
            );
            assert_eq!(state["active"], json!(grok), "{case}");
            assert_eq!(state["attempts"], json!([first, grok]), "{case}");
            assert_eq!(state["switch"]["from"], json!(first), "{case}");
            assert_eq!(
                state["switch"]["retainedTarget"], "PUBLIC-chatgpt-target",
                "{case}"
            );
            let failure = match case {
                "commit" => "commit-deadline",
                "response-load" => "response-load-failed",
                _ => "answer-deadline",
            };
            assert_eq!(state["switch"]["failure"], failure, "{case}");
            assert_eq!(
                require_child(&store, &grok.child_id, &cancel).unwrap(),
                grok
            );
            // The superseded ChatGPT child has no authority while Grok is active.
            assert!(
                require_child(&store, &first.child_id, &cancel).is_err(),
                "{case}"
            );
            assert!(
                f.coordinator
                    .switch_to_grok(&first, &typed(), &environment, &cancel)
                    .unwrap()
                    .is_none(),
                "{case} switched twice"
            );
            assert_eq!(store.list(false).unwrap().len(), 3, "{case}");
        }
    }

    #[test]
    fn only_an_unsent_grok_attempt_restores_the_original_chatgpt_attempt() {
        // (Grok child status, dispatch state or none, restorable)
        for (status, dispatch, restorable) in [
            ("pending", None, true),
            ("error", Some("not-started"), true),
            ("error", Some("started"), false),
            ("error", Some("acknowledged"), false),
            ("error", Some("uncertain"), false),
        ] {
            let mut f = switch_fixture(json!({"browserFallback":"grok"}), json!({}));
            let (first, environment, cancel) =
                (f.first.clone(), f.environment.clone(), f.cancel.clone());
            let ready = f.store.require(&f.id).unwrap()["browser"]["consultation"]["ready"].clone();
            let grok = f
                .coordinator
                .switch_to_grok(&first, &typed(), &environment, &cancel)
                .unwrap()
                .unwrap();
            f.store
                .update(&grok.child_id, |_| {
                    let mut patch = json!({"status":status});
                    if let Some(state) = dispatch {
                        patch["dispatch"] = json!({"state":state,"reservationId":"PUBLIC"});
                    }
                    Ok(Some(patch))
                })
                .unwrap();
            let restored = f.coordinator.revert_unsent_switch("grok-not-sent");
            let state = f.store.require(&f.id).unwrap()["browser"]["consultation"].clone();
            if !restorable {
                assert!(restored.is_err(), "{dispatch:?}");
                assert_eq!(state["active"], json!(grok), "{dispatch:?}");
                assert!(require_child(&f.store, &first.child_id, &cancel).is_err());
                continue;
            }
            restored.unwrap();
            assert_eq!(state["active"], json!(first));
            assert_eq!(state["attempts"], json!([first]));
            assert_eq!(state["ready"], ready);
            assert_eq!(state["switch"]["outcome"]["state"], "restored");
            assert_eq!(
                state["switch"]["outcome"]["grokChildId"],
                grok.child_id.as_str()
            );
            // Recovery authority returns to the original attempt; Grok becomes inert.
            assert_eq!(
                require_child(&f.store, &first.child_id, &cancel).unwrap(),
                first
            );
            assert!(require_child(&f.store, &grok.child_id, &cancel).is_err());
            assert!(
                f.coordinator
                    .switch_to_grok(&first, &typed(), &environment, &cancel)
                    .unwrap()
                    .is_none()
            );
            assert!(f.coordinator.revert_unsent_switch("again").is_err());
        }
    }

    #[test]
    fn a_reservation_failure_after_the_decision_restores_the_chatgpt_attempt() {
        let mut f = switch_fixture(json!({"browserFallback":"grok"}), json!({}));
        let (first, environment, cancel) =
            (f.first.clone(), f.environment.clone(), f.cancel.clone());
        let before = f.store.require(&f.id).unwrap()["browser"]["consultation"].clone();
        TEST_FAIL_AFTER_PARENT_RESERVATION.set(true);
        let error = f
            .coordinator
            .switch_to_grok(&first, &typed(), &environment, &cancel)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("the ChatGPT attempt is restored"),
            "{error:#}"
        );
        let state = f.store.require(&f.id).unwrap()["browser"]["consultation"].clone();
        let mut expected = before;
        expected["switch"] = state["switch"].clone();
        assert_eq!(state, expected);
        assert_eq!(
            state["switch"]["outcome"]["reason"],
            "grok-reservation-failed"
        );
        assert_eq!(
            require_child(&f.store, &first.child_id, &cancel).unwrap(),
            first
        );
    }

    #[test]
    fn recovery_after_worker_exit_restores_only_an_unsent_switch() {
        // (Grok attempt may have sent, Grok child owner)
        for (sent, owner) in [
            (false, "none"),
            (false, "same"),
            (false, "other"),
            (true, "none"),
        ] {
            let restorable = !sent && owner != "other";
            let mut f = switch_fixture(json!({"browserFallback":"grok"}), json!({}));
            let (first, environment, cancel) =
                (f.first.clone(), f.environment.clone(), f.cancel.clone());
            let grok = f
                .coordinator
                .switch_to_grok(&first, &typed(), &environment, &cancel)
                .unwrap()
                .unwrap();
            if sent {
                f.store
                    .update(&grok.child_id, |_| {
                        Ok(Some(json!({"status":"running","dispatch":{"state":"started","reservationId":"PUBLIC"}})))
                    })
                    .unwrap();
            }
            // The live worker is not an exited one: nothing is restored or changed.
            let live = f.store.require(&f.id).unwrap();
            assert!(!recovery::restore_after_exit(&f.store, &f.id).unwrap());
            assert_eq!(f.store.require(&f.id).unwrap(), live);
            // Same identity with another native start: the exact original worker exited.
            patch_meta(&f.store, &f.id, |meta| {
                let start = meta["lifecycle"]["processIdentity"]["nativeStart"].clone();
                let next = match &start {
                    Value::String(text) => json!((text.parse::<u64>().unwrap() + 1).to_string()),
                    other => json!(other.as_u64().unwrap() + 1),
                };
                meta["lifecycle"]["processIdentity"]["nativeStart"] = next;
            });
            // A real Grok attempt carries the worker's own identity (start_owner).
            if owner != "none" {
                let mut lifecycle = f.store.require(&f.id).unwrap()["lifecycle"].clone();
                if owner == "other" {
                    let pid = lifecycle["processIdentity"]["pid"].as_u64().unwrap();
                    lifecycle["processIdentity"]["pid"] = json!(pid + 1);
                }
                f.store
                    .update(&grok.child_id, |_| Ok(Some(json!({"lifecycle":lifecycle}))))
                    .unwrap();
            }
            // Entry points settle through the active attempt's own id, under the parent lock.
            let settled = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(settle_unsent_switch(&f.store, &grok.child_id, &cancel))
                .unwrap();
            let state = f.store.require(&f.id).unwrap()["browser"]["consultation"].clone();
            if !restorable {
                assert!(!settled, "{sent} {owner}");
                // A Grok attempt that may have sent, or has another owner, stays active.
                assert!(!recovery::restore_after_exit(&f.store, &f.id).unwrap());
                assert_eq!(
                    f.store.require(&f.id).unwrap()["browser"]["consultation"],
                    state
                );
                assert_eq!(state["active"], json!(grok));
                continue;
            }
            assert!(settled);
            assert_eq!(state["active"], json!(first));
            assert_eq!(state["attempts"], json!([first]));
            assert_eq!(
                state["switch"]["outcome"]["reason"],
                "worker-exited-before-grok-send"
            );
            // Idempotent: a restored consultation is left as it is.
            assert!(!recovery::restore_after_exit(&f.store, &f.id).unwrap());
        }
    }

    #[test]
    fn deferred_attachment_fallbacks_never_switch_to_grok() {
        for used in [false, true] {
            let mut f = switch_fixture(json!({"browserFallback":"grok"}), json!({}));
            let (first, environment, cancel) =
                (f.first.clone(), f.environment.clone(), f.cancel.clone());
            if used {
                f.coordinator.request.fallback_used = true;
            } else {
                f.coordinator.request.fallback = Some(browser_runner::BrowserFallback {
                    composer_text: "PUBLIC deferred composer text".into(),
                    pending_bundle: None,
                    text_sources: Vec::new(),
                    raw_source: None,
                    remote_source: None,
                });
            }
            assert!(
                crate::browser_grok::require_text_request(&f.coordinator.request, &json!({}))
                    .is_err()
            );
            let before = f.store.require(&f.id).unwrap();
            assert!(
                f.coordinator
                    .switch_to_grok(&first, &typed(), &environment, &cancel)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(f.store.require(&f.id).unwrap(), before);
        }
    }

    #[test]
    fn ineligible_failures_never_read_the_attempt_record() {
        for cancelled in [false, true] {
            let consent = if cancelled { "grok" } else { "none" };
            let mut f = switch_fixture(json!({"browserFallback":consent}), json!({}));
            let (first, environment, cancel) =
                (f.first.clone(), f.environment.clone(), f.cancel.clone());
            let meta = f
                .store
                .directory(&first.child_id)
                .unwrap()
                .join("meta.json");
            std::fs::write(&meta, b"PUBLIC unreadable").unwrap();
            if cancelled {
                cancel.cancel();
            }
            let before = f.store.require(&f.id).unwrap();
            assert!(
                f.coordinator
                    .switch_to_grok(&first, &typed(), &environment, &cancel)
                    .unwrap()
                    .is_none(),
                "{consent}"
            );
            assert_eq!(f.store.require(&f.id).unwrap(), before);
        }
    }

    #[test]
    fn a_grok_attempt_with_another_owner_is_never_restored() {
        let mut f = switch_fixture(json!({"browserFallback":"grok"}), json!({}));
        let (first, environment, cancel) =
            (f.first.clone(), f.environment.clone(), f.cancel.clone());
        let grok = f
            .coordinator
            .switch_to_grok(&first, &typed(), &environment, &cancel)
            .unwrap()
            .unwrap();
        let mut lifecycle = f.store.require(&f.id).unwrap()["lifecycle"].clone();
        let pid = lifecycle["processIdentity"]["pid"].as_u64().unwrap();
        lifecycle["processIdentity"]["pid"] = json!(pid + 1);
        f.store
            .update(&grok.child_id, |_| Ok(Some(json!({"lifecycle":lifecycle}))))
            .unwrap();
        let before = f.store.require(&f.id).unwrap();
        assert!(f.coordinator.revert_unsent_switch("grok-not-sent").is_err());
        assert_eq!(f.store.require(&f.id).unwrap(), before);
        assert_eq!(before["browser"]["consultation"]["active"], json!(grok));
    }
}

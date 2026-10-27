//! Parent output belongs to the consultation; retirement authority stays on its child.
use super::*;
use crate::{
    artifacts::{self, CaptureReceipt},
    browser_disposition::TargetJournal,
    sessions::timestamp,
};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selected {
    version: u8,
    link: Link,
    child_evidence_sha256: String,
    parent_capture_sha256: String,
    parent_outputs_sha256: String,
}

fn evidence(store: &SessionStore, child: &Value) -> Result<String> {
    let id = child["id"].as_str().context("Attempt ID missing")?;
    let mut dispatch = child["dispatch"].clone();
    dispatch
        .as_object_mut()
        .context("Attempt dispatch missing")?
        .remove("state");
    let runtime = &child["browser"]["runtime"];
    let mut evidence = json!({"controller":controller_identity(child),
        "request":request_hash(&browser_runner::request(store,id)?)?,"dispatch":dispatch,
        "capture":child["browser"]["capturedAnswer"],"artifacts":child["artifacts"],
        "acceptedObservation":runtime["acceptedObservation"],"ownedRecoveryTarget":runtime["ownedRecoveryTarget"],
        "modelSelection":runtime["modelSelection"],"thinkingSelection":runtime["thinkingSelection"],
        "effectiveModel":runtime["effectiveModel"],"userCommitReceipt":runtime["userCommitReceipt"],
        "submittedPromptHash":runtime["submittedPromptHash"],"submittedPromptHashFormat":runtime["submittedPromptHashFormat"]});
    // Existing ChatGPT selection digests retain their exact byte representation.
    if child["browser"]["attempt"]["provider"] == "grok" {
        evidence["grok"] =
            json!({"provider":runtime["provider"],"commitReceipt":runtime["grokCommitReceipt"]});
    }
    Ok(sha256(&serde_json::to_vec(&evidence)?))
}
/// The parent view with the selected child's conversation identity, for the
/// parent's transcript and archive record.
pub(super) fn with_conversation(parent: &Value, child: &Value) -> Value {
    let runtime = &child["browser"]["runtime"];
    let conversation =
        json!({"tabUrl":runtime["tabUrl"],"acceptedObservation":runtime["acceptedObservation"]});
    let runtime = merge_object(&parent["browser"]["runtime"], &conversation);
    merge_object(
        parent,
        &json!({"browser":merge_object(&parent["browser"],&json!({"runtime":runtime}))}),
    )
}

fn parent_outputs(parent: &Value) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&json!([
        parent["artifacts"],
        parent["browser"]["requiredTextOutput"],
        parent["browser"]["modelSelection"],
        parent["browser"]["thinkingSelection"],
        parent["browser"]["effectiveModel"],
        parent["browser"]["provider"]
    ]))?))
}

fn verify_outputs(
    store: &SessionStore,
    parent: &Value,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    let receipt: CaptureReceipt =
        serde_json::from_value(parent["browser"]["capturedAnswer"].clone())?;
    let bytes = artifacts::verify_capture_until(store, &receipt, Some(cancel), deadline)?;
    if let Some(output) = parent["options"]["writeOutputPath"].as_str() {
        let destination = Path::new(
            parent["cwd"]
                .as_str()
                .context("Output working directory missing")?,
        )
        .join(output);
        let required = &parent["browser"]["requiredTextOutput"];
        let labelled = crate::browser_output::required_answer_bytes(required, parent, &bytes)?;
        crate::browser_output::reuse_answer(required, &destination, &labelled, cancel)?;
        ensure!(
            parent["artifacts"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["kind"] == "output"
                    && row["path"] == required["file"]
                    && row["bytes"] == required["bytes"]
                    && row["sha256"] == required["sha256"])),
            "Required parent output is absent from the adopted artifact set"
        );
    }
    let rows = parent["artifacts"]
        .as_array()
        .context("Parent artifact set missing")?;
    for artifact in rows
        .iter()
        .filter(|row| matches!(row["kind"].as_str(), Some("file" | "image")))
    {
        ensure!(
            artifact["nativeIdentity"].is_object(),
            "Adopted artifact native identity missing"
        );
        crate::browser_output::verify_saved_file_until(artifact, cancel, deadline)?;
        if parent["options"]["writeArtifacts"] == true && artifact["kind"] == "file" {
            let directory = parent["browser"]["requiredTextOutput"]["file"]
                .as_str()
                .map(Path::new)
                .and_then(Path::parent);
            ensure!(
                directory.is_some()
                    && rows.iter().any(|row| row["kind"] == "file"
                        && row["path"].as_str().map(Path::new).and_then(Path::parent) == directory
                        && row["sizeBytes"] == artifact["sizeBytes"]
                        && row["sha256"] == artifact["sha256"]),
                "Required parent file export is missing"
            );
        }
    }
    ensure!(
        !cancel.is_cancelled(),
        "Parent output verification cancelled"
    );
    Ok(())
}
fn capture(
    store: &SessionStore,
    parent: &Value,
    child: &Value,
    link: &Link,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u8>> {
    ensure!(!cancel.is_cancelled(), "Attempt adoption was cancelled");
    parent_identity(parent, link)?;
    require_storage_binding(store, link)?;
    require_payload(store, &link.child_id, parent, child, link)?;
    ensure!(
        matches!(
            child["status"].as_str(),
            Some("running" | "partial" | "completed")
        ) && matches!(
            child["dispatch"]["state"].as_str(),
            Some("acknowledged" | "completed")
        ),
        "Attempt has no positively acknowledged capture"
    );
    let receipt: CaptureReceipt =
        serde_json::from_value(child["browser"]["capturedAnswer"].clone())?;
    ensure!(
        receipt.session_id == link.child_id
            && child["capturedAnswer"] == json!(receipt)
            && child["browser"]["runtime"]["capturedAnswer"] == json!(receipt),
        "Attempt capture is not child-bound"
    );
    let runtime = &child["browser"]["runtime"];
    match link.provider {
        Provider::Chatgpt => {
            crate::browser::accept_answer(child, &runtime["acceptedObservation"], runtime)?
        }
        Provider::Grok => crate::browser_grok::accept_saved_answer(
            &browser_runner::request(store, &link.child_id)?,
            child,
            &runtime["acceptedObservation"],
        )?,
    }
    let creator = &runtime["ownedRecoveryTarget"]["creator"];
    let journal = TargetJournal::open(store)?;
    let mut record = journal.read(&creator["allocationId"])?;
    ensure!(
        record["creator"] == *creator && record["nativeOwnerIdentity"].is_object(),
        "Attempt capture has no original native creator"
    );
    let proposed = merge_object(
        child,
        &json!({"status":"completed","completedAt":child.get("completedAt").filter(|value|value.is_string()).cloned().unwrap_or_else(||json!(timestamp())),
        "dispatch":merge_object(&child["dispatch"],&json!({"state":"completed"}))}),
    );
    record["commit"] = journal.session_commit(creator, &proposed)?;
    journal.verify_proposed_until(&record, &proposed, cancel, deadline)?;
    artifacts::verify_capture_until(store, &receipt, Some(cancel), deadline)
}

/// Stream one verified child artifact through the existing bounded native publisher.
fn copy_artifact(
    source_root: &Path,
    artifact: &Value,
    destination: &Path,
    unique: bool,
    resumed: Option<&[Value]>,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    ensure!(
        matches!(artifact["kind"].as_str(), Some("file" | "image")),
        "Unsupported attempt artifact"
    );
    let source_path = Path::new(
        artifact["path"]
            .as_str()
            .context("Attempt artifact path missing")?,
    );
    let root = NativeEntry::open(source_root, true)?;
    let canonical_root = source_root.canonicalize()?;
    let canonical = source_path.canonicalize()?;
    ensure!(
        canonical.starts_with(&canonical_root) && canonical != canonical_root,
        "Attempt artifact leaves its child storage"
    );
    let source = NativeEntry::open(source_path, false)?;
    let identity: NativeIdentity = serde_json::from_value(
        artifact
            .get("nativeIdentity")
            .unwrap_or(&artifact["publicationReceipt"]["nativeIdentity"])
            .clone(),
    )?;
    ensure!(
        source.identity == identity,
        "Attempt artifact native identity changed"
    );
    crate::storage::private_dir(
        destination
            .parent()
            .context("Attempt destination has no parent")?,
    )?;
    let mut receipt = artifact.clone();
    receipt["nativeIdentity"] = json!(identity);
    let publication = if let Some(used) = resumed {
        crate::browser_output::resume_saved_file(
            &receipt,
            destination,
            unique,
            None,
            used,
            cancel,
            deadline,
        )?
    } else {
        crate::browser_output::recapture_saved_file_until(
            &receipt,
            destination,
            unique,
            cancel,
            deadline,
        )?
    };
    source.require_matches(source_path)?;
    root.require_matches(source_root)?;
    ensure!(
        source_path.canonicalize()? == canonical,
        "Attempt artifact route changed during recapture"
    );
    let mut result = artifact.clone();
    result["path"] = json!(publication.path);
    result["nativeIdentity"] = json!(publication.native_identity);
    result["publicationReceipt"] = json!(publication);
    result
        .as_object_mut()
        .context("Artifact metadata is not an object")?
        .remove("cleanupWarning");
    if let Some(warning) = &publication.cleanup_error {
        result["cleanupWarning"] = json!(warning);
    }
    Ok(result)
}

fn rewrite_paths(markdown: &str, mut mappings: Vec<(String, String)>) -> String {
    mappings.sort_by_key(|(source, _)| std::cmp::Reverse(source.len()));
    let mut remaining = markdown;
    let mut rewritten = String::with_capacity(markdown.len());
    while let Some((index, source, destination)) = mappings
        .iter()
        .filter_map(|(source, destination)| {
            remaining
                .find(source)
                .map(|index| (index, source, destination))
        })
        .min_by_key(|(index, _, _)| *index)
    {
        rewritten.push_str(&remaining[..index]);
        rewritten.push_str(destination);
        remaining = &remaining[index + source.len()..];
    }
    rewritten.push_str(remaining);
    rewritten
}

/// Verify the durable capture phase without recopying artifacts or reading the provider again.
pub(super) fn require_captured(
    store: &SessionStore,
    parent: &Value,
    child: &Value,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u8>> {
    let id = parent["id"]
        .as_str()
        .context("Captured parent ID missing")?;
    let state = &parent["browser"]["consultation"];
    let link: Link = serde_json::from_value(state["adoption"]["link"].clone())?;
    ensure!(
        state["adoption"]["version"] == 1
            && state["adoption"]["phase"] == "captured"
            && state["active"] == json!(link)
            && state["selected"].is_null()
            && state["callbackIssued"] == false
            && parent["browser"]["requiredTextOutput"].is_null(),
        "Captured consultation phase or delivery changed"
    );
    let original = capture(store, parent, child, &link, cancel, deadline)?;
    ensure!(
        state["adoption"]["childEvidenceSha256"] == evidence(store, child)?,
        "Captured child evidence changed"
    );
    let receipt: CaptureReceipt =
        serde_json::from_value(parent["browser"]["capturedAnswer"].clone())?;
    ensure!(
        receipt.session_id == id
            && parent["capturedAnswer"] == json!(receipt)
            && parent["browser"]["provider"] == json!(link.provider)
            && parent["browser"]["modelSelection"] == child["browser"]["runtime"]["modelSelection"]
            && parent["browser"]["thinkingSelection"]
                == child["browser"]["runtime"]["thinkingSelection"]
            && parent["browser"]["effectiveModel"] == child["browser"]["runtime"]["effectiveModel"],
        "Captured parent receipt or model identity changed"
    );
    let sources = child["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| matches!(row["kind"].as_str(), Some("file" | "image")))
        .collect::<Vec<_>>();
    let adopted = parent["artifacts"]
        .as_array()
        .context("Captured artifact set missing")?;
    ensure!(
        adopted.len() == sources.len(),
        "Captured artifact set changed"
    );
    let source_root = store.directory(&link.child_id)?.join("artifacts");
    let destination_root = store.directory(id)?.join("artifacts");
    let cwd = PathBuf::from(
        parent["cwd"]
            .as_str()
            .context("Captured working directory missing")?,
    );
    let requested = crate::browser_images::requested_destination(&parent["options"])?;
    let mut image_index = 0;
    let mut mappings = vec![];
    for (source, adopted) in sources.into_iter().zip(adopted) {
        ensure!(
            !cancel.is_cancelled(),
            "Captured artifact verification cancelled"
        );
        let source_path = Path::new(
            source["path"]
                .as_str()
                .context("Captured source path missing")?,
        );
        let path = Path::new(
            adopted["path"]
                .as_str()
                .context("Captured artifact path missing")?,
        );
        ensure!(
            source_path.starts_with(&source_root) && source_path != source_root,
            "Captured source leaves child storage"
        );
        if source["kind"] == "image"
            && let Some(requested) = requested
        {
            let extension = source_path
                .extension()
                .and_then(|value| value.to_str())
                .context("Captured image extension missing")?;
            ensure!(
                path == crate::browser_images::sibling_path(
                    &cwd.join(requested),
                    image_index,
                    extension
                )?,
                "Captured image destination changed"
            );
        } else {
            ensure!(
                path.parent() == Some(destination_root.as_path()),
                "Captured artifact leaves parent storage"
            );
        }
        let mut expected = source.clone();
        for field in [
            "path",
            "nativeIdentity",
            "publicationReceipt",
            "cleanupWarning",
        ] {
            if let Some(value) = adopted.get(field) {
                expected[field] = value.clone();
            } else {
                expected
                    .as_object_mut()
                    .context("Captured source is not an object")?
                    .remove(field);
            }
        }
        ensure!(
            *adopted == expected
                && adopted["nativeIdentity"].is_object()
                && adopted["publicationReceipt"]["path"] == adopted["path"]
                && adopted["publicationReceipt"]["nativeIdentity"] == adopted["nativeIdentity"]
                && adopted["publicationReceipt"]["sizeBytes"] == source["sizeBytes"]
                && adopted["publicationReceipt"]["sha256"] == source["sha256"],
            "Captured artifact provenance changed"
        );
        crate::browser_output::verify_saved_file_until(source, cancel, deadline)?;
        crate::browser_output::verify_saved_file_until(adopted, cancel, deadline)?;
        mappings.push((
            source_path.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ));
        if source["kind"] == "image" {
            image_index += 1;
        }
    }
    ensure!(
        requested.is_none() || image_index > 0,
        "Captured required images missing"
    );
    let rewritten = rewrite_paths(std::str::from_utf8(&original)?, mappings);
    let bytes = artifacts::verify_capture_until(store, &receipt, Some(cancel), deadline)?;
    ensure!(
        bytes == rewritten.as_bytes(),
        "Captured parent answer differs from its verified child"
    );
    Ok(bytes)
}

/// A reserved phase has no committed parent delivery yet. Its child remains immutable.
pub(super) fn require_reserved(
    store: &SessionStore,
    parent: &Value,
    child: &Value,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u8>> {
    let state = &parent["browser"]["consultation"];
    let link: Link = serde_json::from_value(state["adoption"]["link"].clone())?;
    ensure!(
        state["adoption"]["version"] == 1
            && state["adoption"]["phase"] == "reserved"
            && state["active"] == json!(link)
            && state["selected"].is_null()
            && state["callbackIssued"] == false
            && parent["capturedAnswer"].is_null()
            && parent["browser"]["capturedAnswer"].is_null()
            && parent["browser"]["requiredTextOutput"].is_null()
            && (parent["artifacts"].is_null()
                || parent["artifacts"].as_array().is_some_and(Vec::is_empty)),
        "Reserved consultation already has parent delivery or changed phase"
    );
    let bytes = capture(store, parent, child, &link, cancel, deadline)?;
    ensure!(
        state["adoption"]["childEvidenceSha256"] == evidence(store, child)?,
        "Reserved child evidence changed"
    );
    Ok(bytes)
}

fn parent_capture(
    store: &SessionStore,
    id: &str,
    bytes: &[u8],
    resume: bool,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<CaptureReceipt> {
    if resume {
        let path = store.directory(id)?;
        let directory = NativeEntry::open(&path, true)?;
        let expected = sha256(bytes);
        for row in std::fs::read_dir(&path)? {
            artifacts::check_deadline(deadline)?;
            ensure!(!cancel.is_cancelled(), "Parent capture recovery cancelled");
            directory.require_matches(&path)?;
            let row = row?;
            let name = row.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(uuid) = name
                .strip_prefix("captured-answer-")
                .and_then(|value| value.strip_suffix(".md"))
            else {
                continue;
            };
            if Uuid::parse_str(uuid).is_err() || !row.file_type()?.is_file() {
                continue;
            }
            let mut entry = NativeEntry::open(&row.path(), false)?;
            if entry.handle.metadata()?.len() != bytes.len() as u64 {
                continue;
            }
            let stamp = crate::native_fs::read_stamp(&entry.handle)?;
            if artifacts::verify_streamed_file_until(
                &mut entry.handle,
                bytes.len() as u64,
                &expected,
                Some(cancel),
                deadline,
            )
            .is_err()
            {
                artifacts::check_deadline(deadline)?;
                ensure!(!cancel.is_cancelled(), "Parent capture recovery cancelled");
                continue;
            }
            ensure!(
                crate::native_fs::read_stamp(&entry.handle)? == stamp,
                "Pending parent capture changed during verification"
            );
            entry.require_matches(&row.path())?;
            directory.require_matches(&path)?;
            return Ok(CaptureReceipt {
                version: 1,
                session_id: id.to_owned(),
                file: name.to_owned(),
                encoding: "utf8".into(),
                bytes: bytes.len() as u64,
                sha256: expected,
                saved_at: timestamp(),
            });
        }
        directory.require_matches(&path)?;
    }
    artifacts::check_deadline(deadline)?;
    artifacts::capture_until(
        store,
        id,
        &format!("captured-answer-{}.md", Uuid::new_v4()),
        bytes,
        cancel,
        deadline,
    )
}

impl Parent {
    /// Reserve before filesystem work. Any interruption blocks another provider attempt.
    pub(crate) fn adopt(&mut self, child_id: &str, cancel: &CancellationToken) -> Result<Value> {
        let witness =
            ExecutionWitness::capture(&self.store, child_id, &ProcessIdentity::current()?)?;
        self.adopt_with_witness(child_id, &witness, cancel)
    }

    pub(super) fn adopt_with_witness(
        &mut self,
        child_id: &str,
        witness: &ExecutionWitness,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.capture_with_witness(child_id, witness, false, cancel, |_| Ok(()))?;
        self.select_captured_with_witness(child_id, witness, false, cancel)
    }

    #[cfg(test)]
    pub(crate) fn capture_only_for_test(
        &mut self,
        child_id: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let witness =
            ExecutionWitness::capture(&self.store, child_id, &ProcessIdentity::current()?)?;
        self.capture_with_witness(child_id, &witness, false, cancel, |_| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn capture_until_for_test(
        &mut self,
        child_id: &str,
        stop: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let witness =
            ExecutionWitness::capture(&self.store, child_id, &ProcessIdentity::current()?)?;
        self.capture_with_witness(child_id, &witness, false, cancel, |phase| {
            ensure!(phase != stop, "PUBLIC interrupted at {phase}");
            Ok(())
        })
    }

    pub(super) fn resume_reserved_with_witness(
        &mut self,
        child_id: &str,
        witness: &ExecutionWitness,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.capture_with_witness(child_id, witness, true, cancel, |_| Ok(()))?;
        self.select_captured_with_witness(child_id, witness, true, cancel)
    }

    fn capture_with_witness(
        &mut self,
        child_id: &str,
        witness: &ExecutionWitness,
        resume: bool,
        cancel: &CancellationToken,
        mut phase: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let parent = self.store.require(&self.id)?;
        self.require(&parent, cancel)?;
        ensure!(
            (resume || parent["browser"]["consultation"]["adoption"].is_null())
                && parent["browser"]["capturedAnswer"].is_null(),
            "Consultation capture or adoption already exists"
        );
        let link = if resume {
            serde_json::from_value(parent["browser"]["consultation"]["adoption"]["link"].clone())?
        } else {
            require_child(&self.store, child_id, cancel)?
        };
        ensure!(
            link.parent_id == self.id,
            "Attempt belongs to a different consultation"
        );
        let child = self.store.stored_for_update(child_id)?;
        witness.require(&child)?;
        let original = if resume {
            require_reserved(
                &self.store,
                &parent,
                &child,
                cancel,
                witness.recovery_deadline(),
            )?
        } else {
            capture(
                &self.store,
                &parent,
                &child,
                &link,
                cancel,
                witness.recovery_deadline(),
            )?
        };
        let record = TargetJournal::open(&self.store)?
            .read(&child["browser"]["runtime"]["ownedRecoveryTarget"]["creator"]["allocationId"])?;
        ensure!(
            child["status"] == "running"
                && child["dispatch"]["state"] == "acknowledged"
                && record["commit"].is_null()
                && record["disposition"]["decision"] != "retired"
                && record["disposition"]["absence"] != "confirmed-absent",
            "Initial adoption requires a retained uncompleted child capture"
        );
        let child_evidence = evidence(&self.store, &child)?;
        let mut state = parent["browser"]["consultation"].clone();
        if !resume {
            state["adoption"] = json!({"version":1,"link":link,"childEvidenceSha256":child_evidence,"phase":"reserved"});
            self.store.update(&self.id, |current| {
            self.require(current, cancel)?;
            ensure!(
                self.store.stored_for_update(child_id)? == child,
                "Attempt changed before adoption reservation"
            );
            Ok(Some(
                json!({"browser":merge_object(&current["browser"],&json!({"consultation":state}))}),
            ))
        })?;
        }
        self.state = state;
        phase("reserved")?;
        let markdown = String::from_utf8(original)?;
        let mut mappings = vec![];
        let mut adopted = vec![];
        let source_root = self.store.directory(child_id)?.join("artifacts");
        let destination_root = self.store.directory(&self.id)?.join("artifacts");
        let cwd = PathBuf::from(
            parent["cwd"]
                .as_str()
                .context("Consultation working directory missing")?,
        );
        let image_destination = crate::browser_images::requested_destination(&parent["options"])?;
        let mut image_index = 0;
        for artifact in child["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|artifact| matches!(artifact["kind"].as_str(), Some("file" | "image")))
        {
            self.require(&self.store.require(&self.id)?, cancel)?;
            let filename = Path::new(artifact["path"].as_str().context("Artifact path missing")?)
                .file_name()
                .context("Artifact filename missing")?
                .to_string_lossy();
            let destination = if artifact["kind"] == "image"
                && let Some(requested) = image_destination
            {
                let extension = Path::new(filename.as_ref())
                    .extension()
                    .and_then(|value| value.to_str())
                    .context("Image extension missing")?;
                crate::browser_images::sibling_path(&cwd.join(requested), image_index, extension)?
            } else {
                destination_root.join(crate::browser_output::filename(&filename))
            };
            let copy = copy_artifact(
                &source_root,
                artifact,
                &destination,
                artifact["kind"] != "image" || image_destination.is_none(),
                resume.then_some(adopted.as_slice()),
                cancel,
                witness.recovery_deadline(),
            )?;
            mappings.push((
                artifact["path"]
                    .as_str()
                    .context("Artifact source missing")?
                    .to_owned(),
                copy["path"]
                    .as_str()
                    .context("Adopted artifact path missing")?
                    .to_owned(),
            ));
            if artifact["kind"] == "image" {
                image_index += 1;
            }
            adopted.push(copy);
            phase("artifact")?;
        }
        let markdown = rewrite_paths(&markdown, mappings);
        ensure!(
            image_destination.is_none() || image_index > 0,
            "Required consultation images are missing"
        );
        let receipt = parent_capture(
            &self.store,
            &self.id,
            markdown.as_bytes(),
            resume,
            cancel,
            witness.recovery_deadline(),
        )?;
        phase("parent-file")?;
        let mut captured_state = self.state.clone();
        captured_state["adoption"]["phase"] = json!("captured");
        self.store.update(&self.id,|current| {
            self.require(current,cancel)?;
            ensure!(evidence(&self.store,&self.store.require(child_id)?)?==child_evidence,"Attempt changed during adoption");
            capture(&self.store,current,&self.store.require(child_id)?,&link,cancel,witness.recovery_deadline())?;
            Ok(Some(json!({"capturedAnswer":receipt,"artifacts":adopted,"browser":merge_object(&current["browser"],&json!({"capturedAnswer":receipt,
                "modelSelection":child["browser"]["runtime"]["modelSelection"],"thinkingSelection":child["browser"]["runtime"]["thinkingSelection"],
                "effectiveModel":child["browser"]["runtime"]["effectiveModel"],"provider":link.provider,"consultation":captured_state}))})))
        })?;
        self.state = captured_state;
        Ok(())
    }

    pub(super) fn select_captured_with_witness(
        &mut self,
        child_id: &str,
        witness: &ExecutionWitness,
        resume: bool,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let parent = self.store.require(&self.id)?;
        self.require(&parent, cancel)?;
        let child = self.store.require(child_id)?;
        witness.require(&child)?;
        let markdown = require_captured(
            &self.store,
            &parent,
            &child,
            cancel,
            witness.recovery_deadline(),
        )?;
        let receipt: CaptureReceipt =
            serde_json::from_value(parent["browser"]["capturedAnswer"].clone())?;
        let link: Link =
            serde_json::from_value(parent["browser"]["consultation"]["adoption"]["link"].clone())?;
        ensure!(
            link.child_id == child_id,
            "Captured selection belongs to another child"
        );
        let child_evidence = evidence(&self.store, &child)?;
        let cwd = PathBuf::from(
            parent["cwd"]
                .as_str()
                .context("Consultation working directory missing")?,
        );
        let mut selected_state = self.state.clone();
        selected_state["adoption"]["phase"] = json!("selected");
        let prompt = browser_runner::request(&self.store, &self.id)?.prompt;
        let mut failure = None;
        let selected=self.store.update(&self.id,|current| {
            self.require(current,cancel)?;
            witness.require(&self.store.require(child_id)?)?;
            ensure!(require_captured(&self.store,current,&self.store.require(child_id)?,cancel,witness.recovery_deadline())?==markdown,"Captured adoption changed before parent publication");
            ensure!(evidence(&self.store,&self.store.require(child_id)?)?==child_evidence,"Attempt changed before parent publication");
            let mut changes=json!({});
            let output=current["options"]["writeOutputPath"].as_str().map(|path|cwd.join(path));
            let mut rows=current["artifacts"].as_array().cloned().unwrap_or_default();
            if let Some(output)=&output {
                let save = if resume { crate::browser_output::resume_answer } else { crate::browser_output::save_answer };
                let required=save(&self.store,output,&markdown,&crate::browser_output::answer_heading(current),cancel)?;
                rows.push(json!({"kind":"output","path":required["file"],"bytes":required["bytes"],"sha256":required["sha256"]}));
                changes["requiredTextOutput"]=required;
            }
            if current["options"]["writeArtifacts"]==true {
                let files=rows.iter().filter(|row|row["kind"]=="file").cloned().collect::<Vec<_>>();
                let exports = if resume { crate::browser_output::resume_exports(output.as_deref(),&files,cancel,witness.recovery_deadline())? } else { crate::browser_output::export_files(output.as_deref(),&files,cancel)? };
                rows.extend(exports);
            }
            // Reference saveOptionalArtifact: the transcript is sealed with the parent outputs.
            let view=with_conversation(&merge_object(current,&json!({"artifacts":rows})),&child);
            if browser_runner::transcript::wanted(&view) {
                match browser_runner::transcript::save(&self.store,&self.id,&view,&prompt,&String::from_utf8_lossy(&markdown),cancel) {
                    Ok(saved) => rows.extend(saved),
                    Err(error) if !cancel.is_cancelled() => failure=Some(error),
                    Err(error) => return Err(error),
                }
            }
            let proposed=merge_object(current,&json!({"artifacts":rows,"browser":merge_object(&current["browser"],&changes)}));
            verify_outputs(&self.store,&proposed,cancel,witness.recovery_deadline())?;
            self.require(current,cancel)?;
            ensure!(evidence(&self.store,&self.store.require(child_id)?)?==child_evidence,"Attempt changed during parent publication");
            let seal=Selected {version:1,link:link.clone(),child_evidence_sha256:child_evidence.clone(),parent_capture_sha256:receipt.sha256.clone(),parent_outputs_sha256:parent_outputs(&proposed)?};
            selected_state["selected"]=json!(seal);
            Ok(Some(json!({"artifacts":rows,"browser":merge_object(&proposed["browser"],&json!({"consultation":selected_state}))})))
        })?;
        self.state = selected_state;
        if let Some(error) = failure {
            self.store.append_log(
                &self.id,
                selected["model"].as_str(),
                &format!("[browser] Failed to save session artifact: {error:#}\n"),
            )?;
        }
        require_selected_until(&self.store, &self.id, cancel, witness.recovery_deadline())?;
        Ok(selected)
    }
}

/// Selection validates child provenance without granting the parent target ownership.
pub(crate) fn require_selected(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
) -> Result<Link> {
    require_selected_until(store, id, cancel, None)
}
pub(super) fn require_selected_until(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Link> {
    artifacts::check_deadline(deadline)?;
    ensure!(
        !cancel.is_cancelled(),
        "Selected consultation was cancelled"
    );
    let parent = store.require(id)?;
    let state = &parent["browser"]["consultation"];
    let selected: Selected = serde_json::from_value(state["selected"].clone())?;
    ensure!(
        selected.version == 1
            && selected.link.parent_id == id
            && matches!(
                parent["status"].as_str(),
                Some("running" | "partial" | "completed" | "error")
            )
            && state["adoption"]["phase"] == "selected"
            && state["adoption"]["link"] == json!(selected.link)
            && state["adoption"]["childEvidenceSha256"] == selected.child_evidence_sha256
            && state["callbackIssued"].is_boolean(),
        "Selected consultation identity or phase changed"
    );
    let child = store.require(&selected.link.child_id)?;
    capture(store, &parent, &child, &selected.link, cancel, deadline)?;
    ensure!(
        evidence(store, &child)? == selected.child_evidence_sha256,
        "Selected child evidence changed"
    );
    let receipt: CaptureReceipt =
        serde_json::from_value(parent["browser"]["capturedAnswer"].clone())?;
    ensure!(
        receipt.session_id == id
            && parent["capturedAnswer"] == json!(receipt)
            && receipt.sha256 == selected.parent_capture_sha256
            && parent_outputs(&parent)? == selected.parent_outputs_sha256,
        "Selected parent capture or outputs changed"
    );
    verify_outputs(store, &parent, cancel, deadline)?;
    ensure!(
        store.require(id)? == parent
            && evidence(store, &store.require(&selected.link.child_id)?)?
                == selected.child_evidence_sha256,
        "Selected parent or child changed during verification"
    );
    Ok(selected.link)
}

/// The public capture has parent outputs and a selected-child seal, never a borrowed creator.
pub(crate) fn captured_selected(store: &SessionStore, id: &str) -> Result<Value> {
    captured_selected_until(store, id, &CancellationToken::new(), None)
}
pub(crate) fn captured_selected_until(
    store: &SessionStore,
    id: &str,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    let link = require_selected_until(store, id, cancel, deadline)?;
    let parent = store.require(id)?;
    let mut result =
        crate::remote_protocol::captured_until(store, &link.child_id, cancel, deadline)?;
    let receipt: CaptureReceipt =
        serde_json::from_value(parent["browser"]["capturedAnswer"].clone())?;
    result["answerMarkdown"] = json!(String::from_utf8(artifacts::verify_capture_until(
        store,
        &receipt,
        Some(cancel),
        deadline
    )?)?);
    result["artifacts"] = parent["artifacts"].clone();
    result["selectedAttempt"] = parent["browser"]["consultation"]["selected"].clone();
    result["provider"] = json!(link.provider);
    result
        .as_object_mut()
        .context("Selected capture missing")?
        .remove("ownedRecoveryTarget");
    for (kind, field) in [("file", "savedFiles"), ("image", "savedImages")] {
        let saved = parent["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| row["kind"] == kind)
            .cloned()
            .collect::<Vec<_>>();
        result
            .as_object_mut()
            .context("Selected capture missing")?
            .remove(field);
        if !saved.is_empty() {
            result[field] = json!(saved);
        }
    }
    require_selected_until(store, id, cancel, deadline)?;
    Ok(result)
}

impl Parent {
    pub(crate) fn issue_capture(
        &mut self,
        hook: &browser_runner::CaptureHook,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let parent = self.store.require(&self.id)?;
        self.require(&parent, cancel)?;
        let run = parent["browser"]["remoteDelivery"]["runId"]
            .as_str()
            .context("Consultation remote identity missing")?;
        ensure!(
            crate::remote_result::session_id(run)? == self.id
                && parent["browser"]["remoteDelivery"]["acknowledged"] != true
                && parent["browser"]["remoteDelivery"]["captureIssued"] != true
                && self.state["callbackIssued"] == false,
            "Consultation delivery was already attempted or changed"
        );
        let captured = captured_selected_until(
            &self.store,
            &self.id,
            cancel,
            self.witness.recovery_deadline(),
        )?;
        let mut state = self.state.clone();
        state["callbackIssued"] = json!(true);
        self.store.update(&self.id,|current| {
            self.require(current,cancel)?;
            ensure!(current["browser"]["remoteDelivery"]==parent["browser"]["remoteDelivery"],"Remote delivery changed before callback reservation");
            require_selected_until(&self.store,&self.id,cancel,self.witness.recovery_deadline())?;
            Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"consultation":state,
                "remoteDelivery":merge_object(&current["browser"]["remoteDelivery"],&json!({"captureIssued":true}))}))})))
        })?;
        self.state = state;
        ensure!(
            !cancel.is_cancelled(),
            "Consultation capture callback cancelled"
        );
        hook(captured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifact_paths_are_rewritten_once_with_longest_source_first() {
        for reversed in [false, true] {
            let mut mappings = vec![
                (
                    "/child/artifacts/a.png".into(),
                    "/requested/image.png".into(),
                ),
                (
                    "/child/artifacts/a.png.txt".into(),
                    "/parent/artifacts/a.png.txt".into(),
                ),
                ("/requested/image.png".into(), "/parent/original.txt".into()),
            ];
            if reversed {
                mappings.reverse();
            }
            assert_eq!(
                rewrite_paths(
                    "![image](/child/artifacts/a.png) [file](/child/artifacts/a.png.txt) [original](/requested/image.png)",
                    mappings
                ),
                "![image](/requested/image.png) [file](/parent/artifacts/a.png.txt) [original](/parent/original.txt)"
            );
        }
    }
}

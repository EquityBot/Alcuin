//! Host-owned artifact registration and the public remote result projection.
use crate::{
    artifacts,
    native_fs::{NativeEntry, no_follow, read_stamp},
    remote_host::{MAX_ARTIFACT_BYTES, ProducedRun, artifact_mime},
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    path::Path,
};
use uuid::Uuid;

pub(crate) fn cleanup_warning(kind: &str, origin: &str, path: &Path) -> Value {
    let base = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let mut details = json!({"artifactKind":kind,"origin":origin,"filename":if origin=="host" {crate::browser_output::filename(&base)}else{base.into_owned()}});
    if origin != "host" {
        details["path"] = json!(path);
    }
    json!({"code":"artifact-staging-cleanup-failed","severity":"warning",
        "message":"Artifact publication succeeded and was verified, but staging cleanup failed; residual staging data may remain.","details":details})
}
fn receipts(result: &Value) -> Vec<&Value> {
    ["savedFiles", "savedImages", "artifacts"]
        .into_iter()
        .flat_map(|field| result[field].as_array().into_iter().flatten())
        .collect()
}
pub(crate) fn sanitise(result: &Value, registration_warnings: &[Value]) -> Value {
    let receipts = receipts(result);
    let paths = receipts
        .iter()
        .filter_map(|artifact| artifact["path"].as_str())
        .collect::<Vec<_>>();
    let images = ["savedImages", "artifacts"]
        .into_iter()
        .flat_map(|field| {
            result[field]
                .as_array()
                .into_iter()
                .flatten()
                .filter(move |artifact| field == "savedImages" || artifact["kind"] == "image")
                .filter_map(|artifact| artifact["path"].as_str())
        })
        .collect::<Vec<_>>();
    let image_count = images.iter().copied().collect::<BTreeSet<_>>().len();
    let answer = |value: Option<&str>| {
        value.map(|value| {
            let mut value = value.to_owned();
            for path in &images {
                value = value.replace(&format!(" Saved to: {path}"), "").replace(
                    &format!(" Saved {image_count} file(s) starting at: {path}"),
                    "",
                );
            }
            for path in &paths {
                value = value.replace(
                    path,
                    Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default()
                        .as_ref(),
                );
            }
            value
        })
    };
    let mut public = json!({"answerText":answer(result["answerText"].as_str()).unwrap_or_default(),
        "answerMarkdown":answer(result["answerMarkdown"].as_str()).unwrap_or_default()});
    if let Some(html) = answer(result["answerHtml"].as_str()) {
        public["answerHtml"] = json!(html);
    }
    for field in [
        "provider",
        "tookMs",
        "answerTokens",
        "answerChars",
        "modelSelection",
        "effectiveModel",
        "thinkingSelection",
        "providerNativeCapture",
        "researchPlan",
        "archive",
        "tabUrl",
        "conversationId",
        "promptSubmitted",
        "submittedPromptHash",
        "submittedPromptHashFormat",
    ] {
        if let Some(value) = result.get(field) {
            public[field] = value.clone();
        }
    }
    let mut warnings = vec![];
    for warning in result["warnings"].as_array().into_iter().flatten() {
        let kind = warning["details"]["artifactKind"].as_str();
        let path = warning["details"]["path"].as_str();
        if warning["code"] == "artifact-staging-cleanup-failed"
            && warning["severity"] == "warning"
            && let (Some(kind @ ("file" | "image")), Some(path)) = (kind, path)
            && receipts.iter().any(|receipt| {
                receipt["kind"] == kind
                    && receipt["path"] == path
                    && receipt["sizeBytes"].as_f64().is_some_and(|n| {
                        n > 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0
                    })
                    && receipt["sha256"].as_str().is_some_and(|h| {
                        h.len() == 64
                            && h.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                    && receipt["validation"]["ok"] != false
            })
        {
            warnings.push(cleanup_warning(kind, "host", Path::new(path)));
        }
    }
    warnings.extend_from_slice(registration_warnings);
    if !warnings.is_empty() {
        public["warnings"] = json!(warnings);
    }
    public
}
#[cfg(test)]
fn register(store: &SessionStore, run: &str, artifact: &Value) -> Result<Value> {
    register_until(store, run, artifact, None, None)
}
fn register_until(
    store: &SessionStore,
    run: &str,
    artifact: &Value,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    artifacts::check_deadline(deadline)?;
    let source = Path::new(artifact["path"].as_str().context("Artifact has no path")?);
    ensure!(
        !source.to_string_lossy().ends_with(".crdownload"),
        "Artifact is still a partial download"
    );
    let root = NativeEntry::open(&store.root(), true)?;
    let canonical_root = store.root().canonicalize()?;
    let path = source.canonicalize()?;
    let relative = path
        .strip_prefix(&canonical_root)
        .context("Remote artifact leaves session storage")?;
    let components = relative.components().collect::<Vec<_>>();
    ensure!(
        components.len() >= 3 && components[1].as_os_str() == "artifacts",
        "Remote artifact leaves the session artifact boundary"
    );
    let parent_path = path.parent().context("Artifact has no parent")?;
    let parent = NativeEntry::open(parent_path, true)?;
    let linked = NativeEntry::open(&path, false)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    no_follow(&mut options, false);
    let mut entry = NativeEntry::from_handle(options.open(&path)?, false)?;
    ensure!(
        entry.identity == linked.identity,
        "Registered artifact changed identity"
    );
    entry.handle.sync_all()?;
    let stamp = read_stamp(&entry.handle)?;
    let size = entry.handle.metadata()?.len();
    ensure!(
        size > 0 && size <= MAX_ARTIFACT_BYTES,
        "Artifact is empty or exceeds the bridge limit"
    );
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        artifacts::check_deadline(deadline)?;
        ensure!(
            cancel.is_none_or(|cancel| !cancel.is_cancelled()),
            "Remote artifact registration cancelled"
        );
        let count = entry.handle.read(&mut buffer)?;
        if count == 0 {
            break;
        };
        bytes += count as u64;
        ensure!(bytes <= size, "Artifact grew during registration");
        hash.update(&buffer[..count]);
    }
    let hash: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    ensure!(
        bytes == size
            && artifact
                .get("sha256")
                .filter(|value| !value.is_null())
                .is_none_or(|value| value.as_str() == Some(hash.as_str())),
        "Captured artifact bytes changed before durable registration"
    );
    let filename = crate::browser_output::filename(
        path.file_name()
            .context("Artifact has no filename")?
            .to_string_lossy()
            .as_ref(),
    );
    let mime = artifact_mime(artifact["mimeType"].as_str());
    entry.handle.seek(SeekFrom::Start(0))?;
    let validation=crate::remote_transfer::validation(&mut entry.handle,&filename,mime.as_deref()).unwrap_or_else(|error|json!({"type":"zip","ok":false,"error":error.to_string().strip_prefix("zip validation failed: ").unwrap_or(&error.to_string())}));
    ensure!(
        stamp == read_stamp(&entry.handle)?,
        "Artifact changed during registration"
    );
    entry.require_matches(&path)?;
    linked.require_matches(&path)?;
    parent.require_matches(parent_path)?;
    root.require_matches(&store.root())?;
    ensure!(
        source.canonicalize()? == path && store.root().canonicalize()? == canonical_root,
        "Artifact storage boundary changed"
    );
    let kind = if artifact["kind"] == "image" {
        "image"
    } else {
        "file"
    };
    let mut descriptor = json!({"artifactId":Uuid::new_v4().to_string(),"runId":run,"kind":kind,"filename":filename,
        "byteSize":size,"sha256":hash,"validation":validation,"sourceUrlKind":source_url_kind(artifact),"transferStatus":"ready"});
    if let Some(mime) = mime {
        descriptor["mimeType"] = json!(mime);
    }
    if kind == "image" {
        descriptor["image"] = image_metadata(artifact);
    }
    Ok(json!({"descriptor":descriptor,"filePath":path}))
}
fn source_url_kind(artifact: &Value) -> &'static str {
    let url = artifact["sourceUrl"]
        .as_str()
        .or(artifact["url"].as_str())
        .unwrap_or("");
    if url.starts_with("sandbox:") {
        "sandbox"
    } else if url.contains("/backend-api/") {
        "chatgpt-file-endpoint"
    } else {
        "browser-download"
    }
}
pub(crate) fn image_metadata(artifact: &Value) -> Value {
    let mut image = json!({});
    for field in ["width", "height"] {
        if let Some(value) = artifact[field].as_f64().filter(|n| {
            n.is_finite() && n.fract() == 0.0 && *n > 0.0 && *n <= 9_007_199_254_740_991.0
        }) {
            image[field] = json!(value);
        }
    }
    if let Some(id) = artifact["fileId"].as_str().filter(|id| {
        let lower = id.to_ascii_lowercase();
        matches!(lower.get(..5), Some("file-" | "file_"))
            && id.len() > 5
            && id.len() <= 205
            && id[5..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    }) {
        image["fileId"] = json!(id);
    }
    image
}
pub(crate) fn produced(
    store: &SessionStore,
    run: &str,
    captured: Value,
    images_required: bool,
) -> Result<ProducedRun> {
    produced_until(store, run, captured, images_required, None, None)
}
pub(crate) fn produced_until(
    store: &SessionStore,
    run: &str,
    captured: Value,
    images_required: bool,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    deadline: Option<std::time::Instant>,
) -> Result<ProducedRun> {
    let mut seen = BTreeSet::new();
    let mut artifacts = vec![];
    let mut warnings = vec![];
    for artifact in receipts(&captured) {
        let Some(path) = artifact["path"].as_str().filter(|path| !path.is_empty()) else {
            continue;
        };
        if !matches!(artifact["kind"].as_str(), Some("file" | "image")) || !seen.insert(path) {
            continue;
        }
        match register_until(store,run,artifact,cancel,deadline) {
            Ok(registered)=>artifacts.push(registered),
            Err(_)=>warnings.push(json!({"code":if artifact["kind"]=="image" {"remote-image-registration-failed"}else{"remote-artifact-registration-failed"},"severity":"warning",
                "message":format!("Industrious captured the browser text response, but the bridge host could not prepare {} for transfer. Open the ChatGPT browser on the bridge host, download the file shown in the current response, and copy it to a cloud-readable path.",crate::browser_output::filename(Path::new(path).file_name().unwrap_or_default().to_string_lossy().as_ref()))})),
        }
    }
    artifacts::check_deadline(deadline)?;
    ensure!(
        cancel.is_none_or(|cancel| !cancel.is_cancelled()),
        "Remote capture registration cancelled"
    );
    ensure!(
        !images_required
            || artifacts
                .iter()
                .any(|artifact| artifact["descriptor"]["kind"] == "image")
                && warnings
                    .iter()
                    .all(|warning| warning["code"] != "remote-image-registration-failed"),
        "Required generated image delivery is incomplete"
    );
    let result = sanitise(&captured, &warnings);
    Ok(ProducedRun {
        captured,
        result,
        artifacts: json!(artifacts),
    })
}

/// Validate a retained transport record against the selected capture without minting IDs.
pub(crate) fn verify_recovered(
    store: &SessionStore,
    run: &str,
    record: &crate::remote_result::RemoteRecord,
    captured: &Value,
    images_required: bool,
    cancel: &tokio_util::sync::CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    artifacts::check_deadline(deadline)?;
    ensure!(
        !cancel.is_cancelled(),
        "Remote recovery verification cancelled"
    );
    let saved = record.native();
    ensure!(
        saved["runId"] == run && crate::remote_json::deep_equal(&saved["capturedResult"], captured),
        "Remote recovery capture differs from its selected parent"
    );
    let warnings = saved["result"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| {
            matches!(
                warning["code"].as_str(),
                Some("remote-image-registration-failed" | "remote-artifact-registration-failed")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        crate::remote_json::deep_equal(&saved["result"], &sanitise(captured, &warnings)),
        "Remote recovery public result differs from its capture"
    );
    let root = NativeEntry::open(&store.root(), true)?;
    let canonical_root = store.root().canonicalize()?;
    let receipts = receipts(captured);
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut image = false;
    for row in saved["artifacts"]
        .as_array()
        .context("Remote recovery artifacts missing")?
    {
        artifacts::check_deadline(deadline)?;
        ensure!(
            !cancel.is_cancelled(),
            "Remote recovery artifact verification cancelled"
        );
        let descriptor = &row["descriptor"];
        crate::remote_transfer::descriptor(descriptor)?;
        crate::remote_host::require_artifact_identity(descriptor, &json!(run))?;
        let path = Path::new(
            row["filePath"]
                .as_str()
                .context("Remote recovery artifact path missing")?,
        )
        .canonicalize()?;
        ensure!(
            descriptor["runId"] == run
                && ids.insert(
                    descriptor["artifactId"]
                        .as_str()
                        .context("Remote artifact ID missing")?
                )
                && paths.insert(path.clone()),
            "Remote recovery artifact identity is duplicated or changed"
        );
        let relative = path
            .strip_prefix(&canonical_root)
            .context("Recovered remote artifact leaves session storage")?;
        let components = relative.components().collect::<Vec<_>>();
        ensure!(
            components.len() >= 3 && components[1].as_os_str() == "artifacts",
            "Recovered remote artifact leaves session artifact storage"
        );
        let source = receipts
            .iter()
            .copied()
            .find(|source| {
                source["kind"] == descriptor["kind"]
                    && crate::remote_json::deep_equal(&source["sizeBytes"], &descriptor["byteSize"])
                    && source["sha256"] == descriptor["sha256"]
                    && source["path"].as_str().is_some_and(|source| {
                        Path::new(source)
                            .canonicalize()
                            .is_ok_and(|source| source == path)
                    })
            })
            .context("Remote recovery artifact does not belong to its selected capture")?;
        crate::browser_output::verify_saved_file_until(source, cancel, deadline)?;
        image |= descriptor["kind"] == "image";
    }
    for source in receipts
        .into_iter()
        .filter(|source| matches!(source["kind"].as_str(), Some("file" | "image")))
    {
        let Some(path) = source["path"].as_str().filter(|path| !path.is_empty()) else {
            continue;
        };
        let code = if source["kind"] == "image" {
            "remote-image-registration-failed"
        } else {
            "remote-artifact-registration-failed"
        };
        ensure!(
            paths.contains(&Path::new(path).canonicalize()?)
                || warnings.iter().any(|warning| warning["code"] == code),
            "Remote recovery omitted an artifact without its original registration warning"
        );
    }
    ensure!(
        !images_required
            || image
                && warnings
                    .iter()
                    .all(|warning| warning["code"] != "remote-image-registration-failed"),
        "Required recovered image delivery is incomplete"
    );
    root.require_matches(&store.root())?;
    ensure!(
        store.root().canonicalize()? == canonical_root,
        "Remote recovery storage changed"
    );
    artifacts::check_deadline(deadline)?;
    ensure!(
        !cancel.is_cancelled(),
        "Remote recovery verification cancelled"
    );
    Ok(())
}

pub(crate) fn captured(store: &SessionStore, id: &str) -> Result<Value> {
    captured_until(store, id, &tokio_util::sync::CancellationToken::new(), None)
}
pub(crate) fn captured_until(
    store: &SessionStore,
    id: &str,
    cancel: &tokio_util::sync::CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    artifacts::check_deadline(deadline)?;
    let metadata = store.require(id)?;
    if !metadata["browser"]["consultation"].is_null() {
        return crate::browser_attempt::captured_selected_until(store, id, cancel, deadline);
    }
    let browser = &metadata["browser"];
    let runtime = &browser["runtime"];
    let receipt =
        serde_json::from_value::<artifacts::CaptureReceipt>(browser["capturedAnswer"].clone())?;
    let bytes = artifacts::verify_capture_until(store, &receipt, Some(cancel), deadline)?;
    let markdown = String::from_utf8(bytes)?;
    let observed = &runtime["acceptedObservation"];
    let provider_text = observed["providerText"]
        .as_str()
        .or(observed["text"].as_str())
        .context("Capture has no accepted provider content")?;
    let mut result = json!({"answerText":provider_text,"answerMarkdown":markdown,"answerHtml":observed["html"],
        "answerChars":provider_text.encode_utf16().count(),"answerTokens":crate::tokenizer::count_text(provider_text)?,
        "modelSelection":runtime["modelSelection"],"thinkingSelection":runtime["thinkingSelection"],"effectiveModel":runtime["effectiveModel"],
        "promptSubmitted":true,"conversationId":runtime["conversationId"],"tabUrl":runtime["tabUrl"],
        "submittedPromptHash":runtime["submittedPromptHash"],"submittedPromptHashFormat":runtime["submittedPromptHashFormat"],
        "ownedRecoveryTarget":runtime["ownedRecoveryTarget"],"assistantMessageId":observed["lastAssistantMessageId"],
        "assistantContent":{"sha256":artifacts::sha256(provider_text.as_bytes()),"representation":"text"},"artifacts":metadata["artifacts"]});
    for (kind, field) in [("file", "savedFiles"), ("image", "savedImages")] {
        let saved = metadata["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|artifact| artifact["kind"] == kind)
            .cloned()
            .collect::<Vec<_>>();
        if !saved.is_empty() {
            result[field] = json!(saved);
        }
    }
    if let Some(warnings) = metadata.get("warnings") {
        result["warnings"] = warnings.clone();
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovered_transport_requires_original_artifacts_projection_and_serving_identity() {
        use crate::remote_result::{Phase, read_phase, save, session_id};
        use tokio_util::sync::CancellationToken;
        let run = "12345678-1234-1234-1234-123456789abc";
        for mode in [
            "valid",
            "short-id",
            "other-session",
            "changed-kind",
            "changed-hash",
            "changed-size",
            "public-answer",
            "duplicate-id",
            "duplicate-path",
            "omitted",
        ] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path().join(".oracle"));
            let id = session_id(run).unwrap();
            store
                .create(json!({"prompt":"PUBLIC recovery"}), home.path(), Some(&id))
                .unwrap();
            let directory = store.directory(&id).unwrap().join("artifacts");
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("PUBLIC-file.txt");
            std::fs::write(&path, b"PUBLIC file").unwrap();
            let receipt = json!({"kind":"file","path":path,"sizeBytes":11,
                "sha256":artifacts::sha256(b"PUBLIC file"),
                "nativeIdentity":NativeEntry::open(&path,false).unwrap().identity});
            let captured = json!({"answerText":"PUBLIC answer","answerMarkdown":"PUBLIC answer","artifacts":[receipt]});
            let mut produced = produced(&store, run, captured.clone(), false).unwrap();
            let original_id = produced.artifacts[0]["descriptor"]["artifactId"].clone();
            match mode {
                "valid" => {}
                "short-id" => produced.artifacts[0]["descriptor"]["artifactId"] = json!("a"),
                "other-session" => {
                    let other = store
                        .create(
                            json!({"prompt":"PUBLIC other"}),
                            home.path(),
                            Some("PUBLIC-other"),
                        )
                        .unwrap();
                    let other_directory = store
                        .directory(other["id"].as_str().unwrap())
                        .unwrap()
                        .join("artifacts");
                    std::fs::create_dir(&other_directory).unwrap();
                    let other_path = other_directory.join("PUBLIC-file.txt");
                    std::fs::write(&other_path, b"PUBLIC file").unwrap();
                    produced.artifacts[0]["filePath"] = json!(other_path);
                }
                "changed-kind" => produced.artifacts[0]["descriptor"]["kind"] = json!("image"),
                "changed-hash" => {
                    produced.artifacts[0]["descriptor"]["sha256"] =
                        json!(artifacts::sha256(b"PUBLIC other"))
                }
                "changed-size" => produced.artifacts[0]["descriptor"]["byteSize"] = json!(12),
                "public-answer" => produced.result["answerText"] = json!("PUBLIC unrelated"),
                "duplicate-id" | "duplicate-path" => {
                    let mut duplicate = produced.artifacts[0].clone();
                    if mode == "duplicate-path" {
                        duplicate["descriptor"]["artifactId"] = json!(Uuid::new_v4().to_string());
                    }
                    produced.artifacts.as_array_mut().unwrap().push(duplicate);
                }
                "omitted" => produced.artifacts = json!([]),
                _ => unreachable!(),
            }
            let cancel = CancellationToken::new();
            save(
                &store,
                run,
                &captured,
                &produced.result,
                &produced.artifacts,
                Phase::Capture,
                &cancel,
            )
            .unwrap();
            let record = read_phase(&store, run, Phase::Capture).unwrap();
            let verified = verify_recovered(&store, run, &record, &captured, false, &cancel, None);
            assert_eq!(verified.is_ok(), mode == "valid", "{mode}: {verified:?}");
            if mode == "valid" {
                assert_eq!(
                    record.native()["artifacts"][0]["descriptor"]["artifactId"],
                    original_id
                );
                assert!(
                    verify_recovered(&store, run, &record, &captured, true, &cancel, None).is_err()
                );
                assert!(
                    verify_recovered(
                        &store,
                        run,
                        &record,
                        &captured,
                        false,
                        &cancel,
                        Some(std::time::Instant::now())
                    )
                    .is_err()
                );
                cancel.cancel();
                assert!(
                    verify_recovered(&store, run, &record, &captured, false, &cancel, None)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn public_projection_removes_host_paths_and_only_keeps_verified_cleanup_warnings() {
        let path = std::env::temp_dir().join("PUBLIC-host-output.png");
        let other = std::env::temp_dir().join("PUBLIC-unverified.png");
        let valid = cleanup_warning("image", "local", &path);
        let invalid = cleanup_warning("image", "local", &other);
        let captured = json!({"answerText":format!("PUBLIC image Saved to: {}",path.display()),"answerMarkdown":format!("PUBLIC link {}",path.display()),
            "chromePid":123,"chromePort":456,"userDataDir":"PUBLIC private profile","ownedRecoveryTarget":{"creator":"PUBLIC private"},
            "savedImages":[{"kind":"image","path":path,"sizeBytes":4,"sha256":artifacts::sha256(b"DATA"),"validation":{"ok":true}}],"warnings":[valid,invalid,{"code":"PUBLIC unrelated warning"}]});
        let public = sanitise(&captured, &[]);
        assert_eq!(public["answerText"], "PUBLIC image");
        assert_eq!(
            public["answerMarkdown"],
            "PUBLIC link PUBLIC-host-output.png"
        );
        for field in [
            "chromePid",
            "chromePort",
            "userDataDir",
            "ownedRecoveryTarget",
            "savedImages",
        ] {
            assert!(public.get(field).is_none(), "{field}");
        }
        assert_eq!(public["warnings"].as_array().unwrap().len(), 1);
        assert_eq!(public["warnings"][0]["details"]["origin"], "host");
        assert!(public["warnings"][0]["details"].get("path").is_none());
    }
    #[test]
    fn registration_rehashes_only_session_artifacts_and_incomplete_images_fail() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let metadata = store
            .create(
                json!({"prompt":"PUBLIC artifacts"}),
                home.path(),
                Some("PUBLIC-session"),
            )
            .unwrap();
        let directory = store
            .directory(metadata["id"].as_str().unwrap())
            .unwrap()
            .join("artifacts");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("PUBLIC-file.txt");
        std::fs::write(&path, b"PUBLIC file").unwrap();
        let outside = home.path().join("PUBLIC-outside.txt");
        std::fs::write(&outside, b"PUBLIC outside").unwrap();
        let result = json!({"answerText":"PUBLIC text","artifacts":[{"kind":"file","path":path,"sha256":artifacts::sha256(b"PUBLIC file")},{"kind":"image","path":outside}]});
        let produced = produced(
            &store,
            "12345678-1234-1234-1234-123456789abc",
            result.clone(),
            false,
        )
        .unwrap();
        assert_eq!(produced.artifacts.as_array().unwrap().len(), 1);
        assert_eq!(
            produced.result["warnings"][0]["code"],
            "remote-image-registration-failed"
        );
        assert!(
            super::produced(&store, "12345678-1234-1234-1234-123456789abc", result, true).is_err()
        );
        std::fs::write(&path, b"PUBLIC changed").unwrap();
        assert!(
            register(
                &store,
                "12345678-1234-1234-1234-123456789abc",
                &json!({"kind":"file","path":path,"sha256":artifacts::sha256(b"PUBLIC file")})
            )
            .is_err()
        );
    }
}

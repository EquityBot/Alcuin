//! Required file exports beside a durably saved browser answer.
//! Reference: src/cli/browserTextOutput.ts, copyBrowserOutputArtifacts.
use crate::{
    artifacts::{ArtifactStage, PublishedArtifact, sha256},
    browser_download::MAX_DOWNLOAD_BYTES,
    native_fs::{NativeEntry, NativeIdentity, canonical_wire_path, no_follow, reject_link},
    policy,
    sessions::{SessionStore, timestamp},
    storage::{private_dir, replace_file},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn clean_label(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(policy::trim)
        .filter(|text| !text.is_empty())
}
pub(crate) fn answer_heading(metadata: &Value) -> String {
    let browser = &metadata["browser"];
    let runtime = &browser["runtime"];
    let model = if browser["modelSelection"].is_object() {
        &browser["modelSelection"]
    } else {
        &runtime["modelSelection"]
    };
    let thinking = if browser["thinkingSelection"].is_object() {
        &browser["thinkingSelection"]
    } else {
        &runtime["thinkingSelection"]
    };
    let observed = (model["verified"] == true)
        .then(|| clean_label(&model["resolvedLabel"]))
        .flatten();
    let strategy = browser["config"]["modelStrategy"]
        .as_str()
        .or_else(|| model["strategy"].as_str());
    let configured =
        clean_label(&metadata["model"]).or_else(|| clean_label(&browser["config"]["desiredModel"]));
    let selected = if matches!(strategy, Some("current" | "ignore"))
        || observed.is_some_and(|label| {
            !label.eq_ignore_ascii_case("Latest") && !label.eq_ignore_ascii_case("GPT-6")
        }) {
        observed
    } else {
        configured.or(observed)
    };
    let mut label = String::new();
    for character in selected.unwrap_or("ChatGPT").chars() {
        if policy::javascript_whitespace(character) {
            if !label.ends_with(' ') {
                label.push(' ');
            }
        } else {
            label.push(character);
        }
    }
    if label
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gpt-"))
    {
        label.replace_range(..4, "GPT-");
    }
    for (suffix, replacement) in [("-pro", " Pro"), ("-sol", " Sol")] {
        if label.to_ascii_lowercase().ends_with(suffix) {
            label.replace_range(label.len() - suffix.len().., replacement);
        }
    }
    if thinking["verified"] == true
        && let Some(effort) = clean_label(&thinking["resolvedLabel"])
        && !label.to_lowercase().contains(&effort.to_lowercase())
    {
        label.push_str(&format!(" ({effort})"));
    }
    format!("{label} said:")
}
fn labelled_answer(answer: &[u8], heading: &str) -> Result<Vec<u8>> {
    let answer = std::str::from_utf8(answer)?;
    ensure!(
        !policy::trim(answer).is_empty(),
        "Required browser output must contain text"
    );
    let mut labelled = format!("{heading}\n\n{answer}");
    if !labelled.ends_with('\n') {
        labelled.push('\n');
    }
    Ok(labelled.into_bytes())
}
pub(crate) fn required_answer_bytes(
    required: &Value,
    metadata: &Value,
    answer: &[u8],
) -> Result<Vec<u8>> {
    if required.get("answerHeading").is_none() {
        return Ok(answer.to_vec());
    }
    let heading = answer_heading(metadata);
    ensure!(
        required["answerHeading"] == heading && required["captureSha256"] == sha256(answer),
        "Saved answer configuration or capture changed"
    );
    labelled_answer(answer, &heading)
}
fn future_parent(parent: &Path) -> Result<PathBuf> {
    let mut ancestor = parent;
    let mut missing = vec![];
    loop {
        match canonical_wire_path(ancestor) {
            Ok(mut canonical) => {
                for name in missing.iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                missing.push(
                    ancestor
                        .file_name()
                        .context("Required output ancestor has no filename")?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .context("Required output ancestor is unavailable")?;
            }
            Err(error) => return Err(error),
        }
    }
}
pub(crate) fn save_answer(
    store: &SessionStore,
    destination: &Path,
    answer: &[u8],
    heading: &str,
    cancel: &CancellationToken,
) -> Result<Value> {
    save_answer_with(
        store,
        destination,
        answer,
        Some(heading),
        false,
        cancel,
        |_, _| {},
    )
}
/// Finish interrupted delivery without replacing any occupied destination.
pub(crate) fn resume_answer(
    store: &SessionStore,
    destination: &Path,
    answer: &[u8],
    heading: &str,
    cancel: &CancellationToken,
) -> Result<Value> {
    save_answer_with(
        store,
        destination,
        answer,
        Some(heading),
        true,
        cancel,
        |_, _| {},
    )
}
pub(crate) fn save_inspection(
    store: &SessionStore,
    destination: &Path,
    answer: &[u8],
    cancel: &CancellationToken,
) -> Result<Value> {
    save_answer_with(store, destination, answer, None, false, cancel, |_, _| {})
}
fn save_answer_with(
    store: &SessionStore,
    destination: &Path,
    answer: &[u8],
    heading: Option<&str>,
    resume: bool,
    cancel: &CancellationToken,
    phase: impl Fn(&str, &Path),
) -> Result<Value> {
    check(cancel)?;
    let contents = match heading {
        Some(heading) => labelled_answer(answer, heading)?,
        None => answer.to_vec(),
    };
    let requested =
        crate::prompts::absolute(&std::env::current_dir()?, &destination.to_string_lossy());
    let parent = requested
        .parent()
        .context("Required output directory is unavailable")?;
    let name = requested
        .file_name()
        .context("Required output filename is unavailable")?;
    let store_root = canonical_wire_path(&store.root())?;
    let store_entry = NativeEntry::open(&store_root, true)?;
    ensure!(
        !requested.starts_with(crate::prompts::absolute(
            &std::env::current_dir()?,
            &store.root().to_string_lossy()
        )) && !future_parent(parent)?.join(name).starts_with(&store_root),
        "Required browser output cannot replace session storage"
    );
    private_dir(parent)?;
    let directory = canonical_wire_path(parent)?;
    let target = directory.join(name);
    ensure!(
        !target.starts_with(&store_root),
        "Required browser output cannot replace session storage"
    );
    let occupied = match fs::symlink_metadata(&target) {
        Ok(_) => {
            ensure!(
                reject_link(&target)?.is_file(),
                "Required browser output must be a regular file without links"
            );
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let directory_entry = NativeEntry::open(&directory, true)?;
    let expected_hash = sha256(&contents);
    let validate = |entry: &mut NativeEntry, file: &Path| -> Result<()> {
        check(cancel)?;
        let stamp = crate::native_fs::read_stamp(&entry.handle)?;
        entry.handle.seek(SeekFrom::Start(0))?;
        crate::artifacts::verify_streamed_file(
            &mut entry.handle,
            contents.len() as u64,
            &expected_hash,
            Some(cancel),
        )?;
        ensure!(
            crate::native_fs::read_stamp(&entry.handle)? == stamp,
            "Required browser output changed during verification"
        );
        entry.require_matches(file)?;
        directory_entry.require_matches(&directory)?;
        store_entry.require_matches(&store_root)?;
        ensure!(
            canonical_wire_path(parent)? == directory
                && canonical_wire_path(&store.root())? == store_root,
            "Required browser output storage identity changed"
        );
        check(cancel)
    };
    let receipt = |entry: &NativeEntry| -> Result<Value> {
        let (dev, ino) = NativeIdentity::legacy_stat(&entry.handle)?;
        let mut receipt = json!({"version":1,"requestedPath":requested,"file":target,"encoding":"utf8","bytes":contents.len(),"sha256":expected_hash,"savedAt":timestamp(),"dev":dev,"ino":ino,"captureSha256":sha256(answer)});
        if let Some(heading) = heading {
            receipt["answerHeading"] = json!(heading);
        }
        Ok(receipt)
    };
    if resume && occupied {
        // No old output receipt exists at this phase. Bind a fresh receipt to the current
        // verified delivery, without claiming its publisher or changing its native identity.
        let mut entry = NativeEntry::open(&target, false)?;
        validate(&mut entry, &target)?;
        phase("existing", &target);
        validate(&mut entry, &target)?;
        return receipt(&entry);
    }
    let staged = directory.join(format!(".calvino-output-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    no_follow(&mut options, false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    check(cancel)?;
    let mut entry = NativeEntry::from_handle(options.open(&staged)?, false)?;
    entry.handle.write_all(&contents)?;
    entry.handle.sync_all()?;
    phase("flushed", &staged);
    validate(&mut entry, &staged)?;
    phase("before-rename", &staged);
    validate(&mut entry, &staged)?;
    if resume {
        crate::native_fs::rename_no_replace(&staged, &target)?;
    } else {
        replace_file(&staged, &target)?;
    }
    phase("renamed", &target);
    validate(&mut entry, &target)?;
    receipt(&entry)
}

fn check(cancel: &CancellationToken) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "Required browser output was cancelled"
    );
    Ok(())
}
pub(crate) fn filename(value: &str) -> String {
    let normalised = value.replace('\0', "").replace('\\', "/");
    let basename = normalised
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let basename = if basename.to_ascii_lowercase().ends_with(".crdownload") {
        &basename[..basename.len() - 11]
    } else {
        basename
    };
    let mut name = String::new();
    for character in basename.to_lowercase().chars() {
        let character = if character.is_ascii_alphanumeric() || "._-".contains(character) {
            character
        } else {
            '-'
        };
        if character != '-' || !name.ends_with('-') {
            name.push(character);
        }
    }
    let name = name.trim_matches('-').chars().take(80).collect::<String>();
    if name.is_empty() || name.chars().all(|character| character == '.') {
        "artifact.bin".into()
    } else {
        name
    }
}
pub(crate) fn saved_file_bytes(receipt: &Value, cancel: &CancellationToken) -> Result<Vec<u8>> {
    read_saved(receipt, MAX_DOWNLOAD_BYTES, cancel)
}
fn read_saved(receipt: &Value, limit: u64, cancel: &CancellationToken) -> Result<Vec<u8>> {
    let (mut entry, path, expected, expected_hash) = saved_entry(receipt, limit, cancel)?;
    let stamp = crate::native_fs::read_stamp(&entry.handle)?;
    let mut bytes = vec![];
    Read::by_ref(&mut entry.handle)
        .take(expected + 1)
        .read_to_end(&mut bytes)?;
    entry.require_matches(&path)?;
    check(cancel)?;
    ensure!(
        crate::native_fs::read_stamp(&entry.handle)? == stamp
            && bytes.len() as u64 == expected
            && sha256(&bytes) == expected_hash,
        "Saved file bytes changed"
    );
    Ok(bytes)
}
/// Validate large saved artifacts with a bounded hashing buffer rather than another payload copy.
pub(crate) fn verify_saved_file(receipt: &Value, cancel: &CancellationToken) -> Result<()> {
    verify_saved_file_until(receipt, cancel, None)
}
pub(crate) fn verify_saved_file_until(
    receipt: &Value,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    crate::artifacts::check_deadline(deadline)?;
    let (mut entry, path, expected, expected_hash) =
        saved_entry(receipt, MAX_DOWNLOAD_BYTES, cancel)?;
    let stamp = crate::native_fs::read_stamp(&entry.handle)?;
    crate::artifacts::verify_streamed_file_until(
        &mut entry.handle,
        expected,
        &expected_hash,
        Some(cancel),
        deadline,
    )?;
    entry.require_matches(&path)?;
    check(cancel)?;
    ensure!(
        crate::native_fs::read_stamp(&entry.handle)? == stamp,
        "Saved artifact changed during verification"
    );
    Ok(())
}
/// Recapture verified bytes without materialising either the source or exported file.
pub(crate) fn recapture_saved_file(
    receipt: &Value,
    destination: &Path,
    unique: bool,
    cancel: &CancellationToken,
) -> Result<PublishedArtifact> {
    recapture_saved_file_until(receipt, destination, unique, cancel, None)
}
pub(crate) fn recapture_saved_file_until(
    receipt: &Value,
    destination: &Path,
    unique: bool,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<PublishedArtifact> {
    crate::artifacts::check_deadline(deadline)?;
    let (mut source, path, size, hash) = saved_entry(receipt, MAX_DOWNLOAD_BYTES, cancel)?;
    let stamp = crate::native_fs::read_stamp(&source.handle)?;
    let mut stage = ArtifactStage::create(destination)?;
    let copied = (|| -> Result<()> {
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            crate::artifacts::check_deadline(deadline)?;
            check(cancel)?;
            let count = source.handle.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .context("Artifact size overflow")?;
            ensure!(total <= size, "Saved artifact exceeded its receipt");
            stage.write(&buffer[..count])?;
        }
        source.require_matches(&path)?;
        ensure!(
            total == size && crate::native_fs::read_stamp(&source.handle)? == stamp,
            "Saved artifact changed during recapture"
        );
        Ok(())
    })();
    if let Err(error) = copied {
        return Err(stage.abort(error));
    }
    stage.publish_until(destination, unique, size, &hash, cancel, deadline)
}
fn saved_entry(
    receipt: &Value,
    limit: u64,
    cancel: &CancellationToken,
) -> Result<(NativeEntry, PathBuf, u64, String)> {
    check(cancel)?;
    let path = Path::new(
        receipt["path"]
            .as_str()
            .context("Saved file path is unavailable")?,
    );
    let expected = receipt["sizeBytes"]
        .as_u64()
        .filter(|bytes| *bytes <= limit && *bytes <= 9_007_199_254_740_991)
        .context("Saved file size is unavailable or exceeds its capture bound")?;
    let expected_hash = receipt["sha256"]
        .as_str()
        .filter(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .context("Saved file SHA-256 evidence is unavailable")?;
    let entry = NativeEntry::open(path, false)?;
    ensure!(
        entry.handle.metadata()?.len() == expected,
        "Saved file size changed"
    );
    if let Some(identity) = receipt.get("nativeIdentity") {
        let expected: NativeIdentity = serde_json::from_value(identity.clone())?;
        ensure!(
            entry.identity == expected,
            "Saved file native identity changed"
        );
    }
    if receipt.get("dev").is_some() || receipt.get("ino").is_some() {
        let (dev, ino) = NativeIdentity::legacy_stat(&entry.handle)?;
        ensure!(
            receipt["dev"] == dev && receipt["ino"] == ino,
            "Saved answer native identity changed"
        );
    }
    Ok((entry, path.to_owned(), expected, expected_hash.to_owned()))
}
pub(crate) fn reuse_answer(
    required: &Value,
    destination: &Path,
    answer: &[u8],
    cancel: &CancellationToken,
) -> Result<()> {
    let saved = Path::new(
        required["file"]
            .as_str()
            .context("Saved answer output receipt is incomplete")?,
    );
    ensure!(
        canonical_wire_path(destination)? == saved
            && required["bytes"] == answer.len() as u64
            && required["sha256"] == sha256(answer)
            && required["dev"].is_string()
            && required["ino"].is_string(),
        "Saved answer output no longer matches this capture or destination"
    );
    let evidence = json!({"path":saved,"sizeBytes":required["bytes"],"sha256":required["sha256"],"dev":required["dev"],"ino":required["ino"]});
    ensure!(
        read_saved(&evidence, answer.len() as u64, cancel)? == answer,
        "Saved answer output changed before recovery"
    );
    Ok(())
}
pub(crate) fn export_files(
    output: Option<&Path>,
    saved: &[Value],
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    export_files_with(output, saved, cancel, |_| {})
}
pub(crate) fn resume_exports(
    output: Option<&Path>,
    saved: &[Value],
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<Value>> {
    export_files_mode(output, saved, true, cancel, deadline, |_| {})
}
fn export_files_with(
    output: Option<&Path>,
    saved: &[Value],
    cancel: &CancellationToken,
    after_publish: impl Fn(&Path),
) -> Result<Vec<Value>> {
    export_files_mode(output, saved, false, cancel, None, after_publish)
}
fn export_files_mode(
    output: Option<&Path>,
    saved: &[Value],
    resume: bool,
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
    after_publish: impl Fn(&Path),
) -> Result<Vec<Value>> {
    if saved.is_empty() {
        return Ok(vec![]);
    }
    check(cancel)?;
    let output = output.context("Required file exports need a saved answer path")?;
    let original = output
        .parent()
        .context("Required file export directory is missing")?;
    let canonical = canonical_wire_path(original)?;
    ensure!(
        canonical == original,
        "Required file export directory changed after saving the answer"
    );
    let directory = NativeEntry::open(original, true)?;
    let mut exports = vec![];
    for receipt in saved {
        let artifact = (|| -> Result<Value> {
        check(cancel)?;
        let source = PathBuf::from(
            receipt["path"]
                .as_str()
                .context("Required file export source is missing")?,
        );
        let expected = receipt["sizeBytes"]
            .as_u64()
            .filter(|bytes| *bytes <= MAX_DOWNLOAD_BYTES)
            .context("Required file export size is unavailable or exceeds its capture bound")?;
        let expected_hash = receipt["sha256"]
            .as_str()
            .filter(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            })
            .context("Required file exports need captured size and SHA-256 evidence")?;
        directory.require_matches(original)?;
        ensure!(
            canonical_wire_path(original)? == canonical,
            "Required file export directory changed"
        );
        let name = filename(receipt["filename"].as_str().unwrap_or_else(|| {
            source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("artifact.bin")
        }));
        let publication = if resume {
            resume_saved_file(
                receipt,
                &canonical.join(&name),
                true,
                Some(output),
                &exports,
                cancel,
                deadline,
            )?
        } else {
            recapture_saved_file(receipt, &canonical.join(&name), true, cancel)?
        };
        after_publish(&publication.path);
        check(cancel)?;
        verify_saved_file(
            &json!({"path":publication.path,"sizeBytes":expected,"sha256":expected_hash,"nativeIdentity":publication.native_identity}),
            cancel,
        )?;
        directory.require_matches(original)?;
        ensure!(
            canonical_wire_path(original)? == canonical,
            "Required file export bytes or directory changed while saving"
        );
        check(cancel)?;
        let mut artifact = json!({"kind":"file","path":canonical_wire_path(&publication.path)?,"nativeIdentity":publication.native_identity,"label":format!("{} (write-output)",receipt["label"].as_str().unwrap_or(&name)),"sizeBytes":expected,"sha256":expected_hash,"transfer":{"status":"not-needed"},"origin":{"mode":"local"}});
        for field in ["mimeType", "sourceUrl", "validation"] {
            if let Some(value) = receipt.get(field) {
                artifact[field] = value.clone();
            }
        }
        if let Some(warning) = publication.cleanup_error {
            artifact["cleanupWarning"] = json!(warning);
        }
        Ok(artifact)
        })()
        .map_err(|error| {
            if cancel.is_cancelled() {
                return error;
            }
            // Reference copyBrowserOutputArtifacts flattens the reason into one message, which
            // the session records as its error.
            anyhow::anyhow!(
                "Failed to copy saved file artifact {} beside {}: {error:#}",
                receipt["path"].as_str().unwrap_or_default(),
                output.display()
            )
        })?;
        exports.push(artifact);
    }
    Ok(exports)
}

/// Rebind a verified existing delivery or create the first available destination without
/// replacement. Repeated interrupted observers therefore do not accumulate export copies.
pub(crate) fn resume_saved_file(
    source: &Value,
    destination: &Path,
    unique: bool,
    forbidden: Option<&Path>,
    used: &[Value],
    cancel: &CancellationToken,
    deadline: Option<std::time::Instant>,
) -> Result<PublishedArtifact> {
    verify_saved_file_until(source, cancel, deadline)?;
    let size = source["sizeBytes"]
        .as_u64()
        .context("Export size missing")?;
    let hash = source["sha256"].as_str().context("Export hash missing")?;
    if !unique {
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                let entry = NativeEntry::open(destination, false)?;
                verify_saved_file_until(
                    &json!({"path":destination,"sizeBytes":size,"sha256":hash,"nativeIdentity":entry.identity}),
                    cancel,
                    deadline,
                )?;
                entry.require_matches(destination)?;
                return Ok(PublishedArtifact {
                    path: destination.to_owned(),
                    size_bytes: size,
                    sha256: hash.to_owned(),
                    native_identity: entry.identity,
                    cleanup_error: None,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return recapture_saved_file_until(source, destination, false, cancel, deadline);
            }
            Err(error) => return Err(error.into()),
        }
    }
    let parent = destination.parent().context("Export directory missing")?;
    let base = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("Export filename missing")?;
    let stem = destination
        .file_stem()
        .context("Export filename missing")?
        .to_string_lossy();
    let prefix = format!("{stem}-");
    let extension = destination
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    let mut best: Option<(u64, PublishedArtifact)> = None;
    // Look for verified deliveries before filling a filename hole. An unrelated blocker
    // may have disappeared after the interrupted publication. Keep only one candidate.
    for row in fs::read_dir(parent)? {
        crate::artifacts::check_deadline(deadline)?;
        check(cancel)?;
        let row = row?;
        let name = row.file_name();
        let Some(name) = name.to_str() else { continue };
        let index = if name == base {
            1
        } else {
            let Some(number) = name
                .strip_prefix(&prefix)
                .and_then(|value| value.strip_suffix(&extension))
            else {
                continue;
            };
            let Ok(index) = number.parse::<u64>() else {
                continue;
            };
            if index < 2 || index.to_string() != number {
                continue;
            }
            index
        };
        if best.as_ref().is_some_and(|(known, _)| *known <= index) {
            continue;
        }
        let candidate = parent.join(name);
        if forbidden == Some(candidate.as_path())
            || used
                .iter()
                .any(|row| row["path"].as_str().map(Path::new) == Some(candidate.as_path()))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&candidate)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let entry = NativeEntry::open(&candidate, false)?;
        if entry.handle.metadata()?.len() == size && verify_saved_file_until(
            &json!({"path":candidate,"sizeBytes":size,"sha256":hash,"nativeIdentity":entry.identity}), cancel, deadline,
        ).is_ok() {
            entry.require_matches(&candidate)?;
            best = Some((index, PublishedArtifact {path:candidate,size_bytes:size,sha256:hash.to_owned(),native_identity:entry.identity,cleanup_error:None}));
        }
        check(cancel)?;
    }
    crate::artifacts::check_deadline(deadline)?;
    if let Some((_, publication)) = best {
        return Ok(publication);
    }
    recapture_saved_file_until(source, destination, true, cancel, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn headings_follow_verified_picker_strategy_and_effort() {
        for (metadata, expected) in [
            (json!({"model":"gpt-6-pro"}), "GPT-6 Pro said:"),
            (json!({"model":"gpt-5.6-sol"}), "GPT-5.6 Sol said:"),
            (
                json!({"model":"gpt-6-pro","browser":{"config":{"modelStrategy":"ignore"}}}),
                "ChatGPT said:",
            ),
            (
                json!({"model":"gpt-6-pro","browser":{"config":{"modelStrategy":"current"},"modelSelection":{"verified":true,"resolvedLabel":"Thinking"},"thinkingSelection":{"verified":true,"resolvedLabel":"Extended"}}}),
                "Thinking (Extended) said:",
            ),
            (
                json!({"model":"gpt-6-pro","browser":{"runtime":{"modelSelection":{"verified":true,"resolvedLabel":" Latest ","strategy":"select"},"thinkingSelection":{"verified":true,"resolvedLabel":"Pro"}}}}),
                "GPT-6 Pro said:",
            ),
            (
                json!({"model":"gpt-6-astra","browser":{"runtime":{"modelSelection":{"verified":true,"resolvedLabel":"GPT-6"},"thinkingSelection":{"verified":true,"resolvedLabel":"Extra High"}}}}),
                "GPT-6-astra (Extra High) said:",
            ),
            (
                json!({"model":"gpt-6-pro","browser":{"modelSelection":{"verified":false,"resolvedLabel":"PUBLIC wrong model"},"thinkingSelection":{"verified":false,"resolvedLabel":"PUBLIC wrong effort"}}}),
                "GPT-6 Pro said:",
            ),
        ] {
            assert_eq!(answer_heading(&metadata), expected);
        }
    }
    #[test]
    fn required_answers_replace_regular_files_with_exact_formatted_bytes_and_create_parents() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("store"));
        store.ensure_storage().unwrap();
        let target = home.path().join("output/nested/answer.md");
        let cancel = CancellationToken::new();
        let first =
            save_answer(&store, &target, b"PUBLIC first", "GPT-6 Pro said:", &cancel).unwrap();
        assert_eq!(
            fs::read(&target).unwrap(),
            b"GPT-6 Pro said:\n\nPUBLIC first\n"
        );
        let sentinel = target.parent().unwrap().join("PUBLIC-sentinel");
        fs::write(&sentinel, b"PUBLIC preserved").unwrap();
        let answer = "  PUBLIC café 🐕\r\nanswer\n\n ";
        let expected = format!("GPT-6 Pro said:\n\n{answer}\n").into_bytes();
        let second = save_answer(
            &store,
            &target,
            answer.as_bytes(),
            "GPT-6 Pro said:",
            &cancel,
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), expected);
        assert_eq!(second["version"], 1);
        assert_eq!(second["encoding"], "utf8");
        assert_eq!(Path::new(second["requestedPath"].as_str().unwrap()), target);
        assert_eq!(second["sha256"], sha256(&expected));
        assert_eq!(second["bytes"], expected.len());
        assert!(second["savedAt"].is_string());
        assert_ne!(first["ino"], second["ino"]);
        assert_eq!(fs::read(&sentinel).unwrap(), b"PUBLIC preserved");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 2);
        let metadata = json!({"model":"gpt-6-pro"});
        let reused = required_answer_bytes(&second, &metadata, answer.as_bytes()).unwrap();
        reuse_answer(&second, &target, &reused, &cancel).unwrap();
        assert!(
            required_answer_bytes(&second, &json!({"model":"gpt-5.6-sol"}), answer.as_bytes())
                .is_err()
        );
        assert!(required_answer_bytes(&second, &metadata, b"PUBLIC changed capture").is_err());
    }
    #[test]
    fn inspection_output_replaces_with_raw_bytes_without_label_or_newline() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("store"));
        store.ensure_storage().unwrap();
        let target = home.path().join("inspection/answer.md");
        let cancel = CancellationToken::new();
        for answer in ["  PUBLIC café 🐕\r\nanswer\n\n ", ""] {
            let receipt = save_inspection(&store, &target, answer.as_bytes(), &cancel).unwrap();
            assert_eq!(fs::read(&target).unwrap(), answer.as_bytes());
            assert!(receipt.get("answerHeading").is_none());
            assert_eq!(receipt["sha256"], sha256(answer.as_bytes()));
        }
        assert!(
            save_inspection(
                &store,
                &store.root().join("PUBLIC-protected"),
                b"PUBLIC",
                &cancel
            )
            .is_err()
        );
    }
    #[test]
    fn required_answer_cancellation_and_changed_bytes_preserve_evidence_without_receipts() {
        for fault in ["before", "flushed", "before-rename", "renamed", "changed"] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path().join("store"));
            store.ensure_storage().unwrap();
            let target = home.path().join("answer.md");
            fs::write(&target, b"PUBLIC original").unwrap();
            let cancel = CancellationToken::new();
            if fault == "before" {
                cancel.cancel();
            }
            let result = save_answer_with(
                &store,
                &target,
                b"PUBLIC answer",
                Some("GPT-6 Pro said:"),
                false,
                &cancel,
                |phase, file| {
                    if phase == fault {
                        cancel.cancel();
                    }
                    if fault == "changed" && phase == "renamed" {
                        fs::write(file, b"PUBLIC changed output").unwrap();
                    }
                },
            );
            assert!(result.is_err(), "{fault}");
            let expected: &[u8] = match fault {
                "renamed" => b"GPT-6 Pro said:\n\nPUBLIC answer\n",
                "changed" => b"PUBLIC changed output",
                _ => b"PUBLIC original",
            };
            assert_eq!(fs::read(&target).unwrap(), expected);
            if matches!(fault, "flushed" | "before-rename") {
                assert_eq!(
                    fs::read_dir(home.path())
                        .unwrap()
                        .filter_map(Result::ok)
                        .filter(|entry| entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".calvino-output-"))
                        .count(),
                    1
                );
            }
        }
    }
    #[test]
    fn required_answers_refuse_session_storage_blank_text_and_directory_destinations() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("store"));
        store.ensure_storage().unwrap();
        let cancel = CancellationToken::new();
        assert!(
            save_answer(
                &store,
                &store.root().join("new/answer.md"),
                b"PUBLIC answer",
                "GPT-6 Pro said:",
                &cancel
            )
            .is_err()
        );
        assert!(!store.root().join("new").exists());
        let target = home.path().join("new/output.md");
        assert!(save_answer(&store, &target, b" \r\n\t", "GPT-6 Pro said:", &cancel).is_err());
        assert!(!target.parent().unwrap().exists());
        fs::create_dir_all(&target).unwrap();
        assert!(
            save_answer(
                &store,
                &target,
                b"PUBLIC answer",
                "GPT-6 Pro said:",
                &cancel
            )
            .is_err()
        );
        assert!(target.is_dir());
    }
    #[cfg(unix)]
    #[test]
    fn required_answers_refuse_store_aliases_output_links_and_directory_replacement() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("store"));
        store.ensure_storage().unwrap();
        let alias = home.path().join("PUBLIC-store-alias");
        std::os::unix::fs::symlink(store.root(), &alias).unwrap();
        let cancel = CancellationToken::new();
        assert!(
            save_answer(
                &store,
                &alias.join("new/answer.md"),
                b"PUBLIC answer",
                "GPT-6 Pro said:",
                &cancel
            )
            .is_err()
        );
        assert!(!store.root().join("new").exists());
        let sentinel = home.path().join("PUBLIC-sentinel");
        fs::write(&sentinel, b"PUBLIC preserved").unwrap();
        let link = home.path().join("answer.md");
        std::os::unix::fs::symlink(&sentinel, &link).unwrap();
        assert!(save_answer(&store, &link, b"PUBLIC answer", "GPT-6 Pro said:", &cancel).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"PUBLIC preserved");
        let parent = home.path().join("output");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("answer.md");
        let original = home.path().join("PUBLIC-original-directory");
        let result = save_answer_with(
            &store,
            &target,
            b"PUBLIC answer",
            Some("GPT-6 Pro said:"),
            false,
            &cancel,
            |phase, _| {
                if phase == "flushed" {
                    fs::rename(&parent, &original).unwrap();
                    fs::create_dir(&parent).unwrap();
                }
            },
        );
        assert!(result.is_err());
        assert!(!target.exists());
        assert_eq!(fs::read_dir(&original).unwrap().count(), 1);
    }
    #[cfg(windows)]
    #[test]
    fn required_answers_refuse_windows_session_store_junctions_before_creating_directories() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("store"));
        store.ensure_storage().unwrap();
        let alias = home.path().join("PUBLIC-store-alias");
        // mklink is a Windows shell builtin; a junction needs no symbolic-link privilege.
        let mut child = Command::new("C:\\Windows\\System32\\cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&alias)
            .arg(store.root())
            .env_clear()
            .env("SystemRoot", "C:/Windows")
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("PUBLIC junction helper failed or exceeded its deadline: {result:?}");
                }
            }
        };
        assert!(status.success(), "PUBLIC junction helper failed: {status}");
        assert!(
            save_answer(
                &store,
                &alias.join("new/answer.md"),
                b"PUBLIC answer",
                "GPT-6 Pro said:",
                &CancellationToken::new()
            )
            .is_err()
        );
        assert!(!store.root().join("new").exists());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
        fs::remove_dir(&alias).unwrap();
        assert!(store.root().is_dir());
    }
    fn fixture(home: &Path) -> (PathBuf, Value) {
        let source = home.join("PUBLIC-source.csv");
        let bytes = b"PUBLIC captured file bytes";
        fs::write(&source, bytes).unwrap();
        let directory = home.join("output");
        fs::create_dir(&directory).unwrap();
        let answer = canonical_wire_path(&directory.join(".."))
            .unwrap()
            .join("output/answer.md");
        (
            answer,
            json!({"path":source,"filename":"../PUBLIC REPORT.csv.crdownload","sizeBytes":bytes.len(),"sha256":sha256(bytes),"validation":{"type":"generic","ok":true}}),
        )
    }
    #[test]
    fn file_exports_preserve_collisions_and_verify_captured_bytes() {
        let home = tempfile::tempdir().unwrap();
        let (answer, receipt) = fixture(home.path());
        let collision = answer.parent().unwrap().join("public-report.csv");
        fs::write(&collision, b"PUBLIC existing output").unwrap();
        let result = export_files(
            Some(&answer),
            std::slice::from_ref(&receipt),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["sha256"], receipt["sha256"]);
        assert!(
            result[0]["path"]
                .as_str()
                .unwrap()
                .ends_with("public-report-2.csv")
        );
        assert_eq!(fs::read(&collision).unwrap(), b"PUBLIC existing output");
        assert_eq!(
            fs::read(result[0]["path"].as_str().unwrap()).unwrap(),
            b"PUBLIC captured file bytes"
        );
        fs::write(
            receipt["path"].as_str().unwrap(),
            b"PUBLIC replaced source bytes",
        )
        .unwrap();
        assert!(export_files(Some(&answer), &[receipt], &CancellationToken::new()).is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "measures process peak RSS; run alone with --ignored --exact --test-threads=1"]
    fn streamed_file_exports_have_bounded_peak_rss() {
        use sha2::{Digest, Sha256};

        fn peak_kib() -> u64 {
            fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("VmHWM:"))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        }

        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("PUBLIC-large-artifact.bin");
        let directory = home.path().join("output");
        fs::create_dir(&directory).unwrap();
        let answer = directory.join("answer.md");
        let chunk = [0x52u8; 64 * 1024];
        let mut file = fs::File::create(&source).unwrap();
        let mut hasher = Sha256::new();
        for _ in 0..MAX_DOWNLOAD_BYTES / chunk.len() as u64 {
            file.write_all(&chunk).unwrap();
            hasher.update(chunk);
        }
        file.sync_all().unwrap();
        drop(file);
        let entry = NativeEntry::open(&source, false).unwrap();
        let receipt = json!({
            "path":source,"filename":"PUBLIC-large-artifact.bin",
            "sizeBytes":MAX_DOWNLOAD_BYTES,"sha256":hasher.finalize().iter().map(|byte|format!("{byte:02x}")).collect::<String>(),
            "nativeIdentity":entry.identity
        });
        drop(entry);
        let cancel = CancellationToken::new();
        let before = peak_kib();
        let exports = export_files(Some(&answer), std::slice::from_ref(&receipt), &cancel).unwrap();
        let streamed = peak_kib();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0]["sizeBytes"], MAX_DOWNLOAD_BYTES);
        assert_eq!(exports[0]["sha256"], receipt["sha256"]);
        verify_saved_file(&exports[0], &cancel).unwrap();
        let streamed_delta = streamed.saturating_sub(before);
        // The streamed copy has measured 8,364 and 8,480 KiB on ll; 16 MiB stays well below the
        // 128 MiB a whole-file read costs and the 64 MiB calibration floor below.
        assert!(
            streamed_delta < 16 * 1024,
            "streamed export grew peak RSS by {streamed_delta} KiB"
        );

        // Calibrate the same process metric against a verified whole-file read, retaining the
        // bytes until after measurement. This is a scoped artifact check, not a Chrome baseline.
        let bytes = saved_file_bytes(&receipt, &cancel).unwrap();
        std::hint::black_box(&bytes);
        let buffered = peak_kib();
        let buffered_delta = buffered.saturating_sub(streamed);
        println!(
            "PUBLIC_ARTIFACT_RSS {}",
            json!({
                "fileBytes":MAX_DOWNLOAD_BYTES,"beforeKiB":before,"streamedKiB":streamed,
                "bufferedKiB":buffered,"streamedDeltaKiB":streamed_delta,
                "bufferedDeltaKiB":buffered_delta
            })
        );
        assert_eq!(bytes.len() as u64, MAX_DOWNLOAD_BYTES);
        assert!(
            buffered_delta >= 64 * 1024,
            "whole-file control did not calibrate peak RSS: {buffered_delta} KiB"
        );
    }
    #[test]
    fn cancellation_and_post_publication_mutation_preserve_copies_without_receipts() {
        for mutation in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let (answer, receipt) = fixture(home.path());
            let cancel = CancellationToken::new();
            let result = export_files_with(Some(&answer), &[receipt], &cancel, |copy| {
                if mutation {
                    fs::write(copy, b"PUBLIC changed exported bytes").unwrap();
                } else {
                    cancel.cancel();
                }
            });
            assert!(result.is_err());
            assert!(answer.parent().unwrap().join("public-report.csv").exists());
        }
        assert!(
            export_files(None, &[], &CancellationToken::new())
                .unwrap()
                .is_empty()
        );
        let home = tempfile::tempdir().unwrap();
        let (_, receipt) = fixture(home.path());
        assert!(export_files(None, &[receipt], &CancellationToken::new()).is_err());
    }
    #[test]
    fn export_names_follow_the_maintained_ascii_component_rule() {
        for (source, expected) in [
            ("C:\\public\\REPORT.csv.crdownload", "report.csv"),
            ("../...", "artifact.bin"),
            (" A   B.CSV ", "a-b.csv"),
        ] {
            assert_eq!(filename(source), expected);
        }
        assert_eq!(filename("\0PUBLIC.csv"), "public.csv");
    }

    #[test]
    fn answer_reuse_requires_exact_content_destination_and_native_receipt() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("PUBLIC-answer.md");
        let bytes = b"PUBLIC committed answer";
        fs::write(&file, bytes).unwrap();
        let entry = NativeEntry::open(&file, false).unwrap();
        let (dev, ino) = NativeIdentity::legacy_stat(&entry.handle).unwrap();
        drop(entry);
        let receipt = json!({"file":canonical_wire_path(&file).unwrap(),"bytes":bytes.len(),"sha256":sha256(bytes),"dev":dev,"ino":ino});
        reuse_answer(&receipt, &file, bytes, &CancellationToken::new()).unwrap();
        let mut unverified = receipt.clone();
        unverified.as_object_mut().unwrap().remove("ino");
        assert!(reuse_answer(&unverified, &file, bytes, &CancellationToken::new()).is_err());
        assert!(
            reuse_answer(
                &receipt,
                &file,
                b"PUBLIC different answer",
                &CancellationToken::new()
            )
            .is_err()
        );
        fs::rename(&file, home.path().join("PUBLIC-original-answer.md")).unwrap();
        fs::write(&file, bytes).unwrap();
        assert!(reuse_answer(&receipt, &file, bytes, &CancellationToken::new()).is_err());
        assert_eq!(fs::read(&file).unwrap(), bytes);
    }
    #[test]
    fn resumed_answer_binds_exact_existing_delivery_or_publishes_without_replacement() {
        for occupied in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path().join("store"));
            store.ensure_storage().unwrap();
            let target = home.path().join("PUBLIC-answer.md");
            let answer = b"**PUBLIC formatted answer**";
            let heading = "GPT-6 Pro said:";
            let expected = labelled_answer(answer, heading).unwrap();
            let old_identity = if occupied {
                fs::write(&target, &expected).unwrap();
                Some(NativeEntry::open(&target, false).unwrap().identity)
            } else {
                None
            };
            let receipt =
                resume_answer(&store, &target, answer, heading, &CancellationToken::new()).unwrap();
            assert_eq!(fs::read(&target).unwrap(), expected);
            if let Some(identity) = old_identity {
                assert_eq!(
                    NativeEntry::open(&target, false).unwrap().identity,
                    identity
                );
            }
            reuse_answer(&receipt, &target, &expected, &CancellationToken::new()).unwrap();
            assert_eq!(receipt["captureSha256"], sha256(answer));
            assert_eq!(receipt["answerHeading"], heading);
        }
    }

    #[test]
    fn resumed_answer_preserves_conflicts_and_rechecks_races_and_cancellation() {
        for fault in ["conflict", "appeared", "cancelled", "changed"] {
            let home = tempfile::tempdir().unwrap();
            let store = SessionStore::new(home.path().join("store"));
            store.ensure_storage().unwrap();
            let target = home.path().join("PUBLIC-answer.md");
            let expected = labelled_answer(b"PUBLIC answer", "GPT-6 Pro said:").unwrap();
            if fault == "conflict" {
                fs::write(&target, b"PUBLIC external output").unwrap();
            } else if fault != "appeared" {
                fs::write(&target, &expected).unwrap();
            }
            let cancel = CancellationToken::new();
            let result = save_answer_with(
                &store,
                &target,
                b"PUBLIC answer",
                Some("GPT-6 Pro said:"),
                true,
                &cancel,
                |phase, _| {
                    if fault == "appeared" && phase == "before-rename" {
                        fs::write(&target, b"PUBLIC external output").unwrap();
                    } else if phase == "existing" {
                        if fault == "cancelled" {
                            cancel.cancel();
                        }
                        if fault == "changed" {
                            fs::write(&target, b"PUBLIC changed output").unwrap();
                        }
                    }
                },
            );
            assert!(result.is_err(), "{fault} unexpectedly published a receipt");
            assert_eq!(
                fs::read(&target).unwrap(),
                match fault {
                    "cancelled" => expected,
                    "changed" => b"PUBLIC changed output".to_vec(),
                    _ => b"PUBLIC external output".to_vec(),
                }
            );
        }
    }
    #[test]
    fn resumed_exports_reuse_verified_copies_without_accumulating_or_merging_deliveries() {
        let home = tempfile::tempdir().unwrap();
        let (answer, receipt) = fixture(home.path());
        let collision = answer.parent().unwrap().join("public-report.csv");
        fs::write(&collision, b"PUBLIC external file").unwrap();
        let saved = [receipt.clone(), receipt];
        let cancel = CancellationToken::new();
        let first = export_files(Some(&answer), &saved, &cancel).unwrap();
        assert_eq!(first.len(), 2);
        assert_ne!(first[0]["path"], first[1]["path"]);
        let count = fs::read_dir(answer.parent().unwrap()).unwrap().count();
        for _ in 0..3 {
            let resumed = resume_exports(Some(&answer), &saved, &cancel, None).unwrap();
            assert_eq!(resumed, first);
            assert_eq!(
                fs::read_dir(answer.parent().unwrap()).unwrap().count(),
                count
            );
            for row in &resumed {
                verify_saved_file(row, &cancel).unwrap();
            }
        }
        assert_eq!(fs::read(&collision).unwrap(), b"PUBLIC external file");
        fs::remove_file(&collision).unwrap();
        let resumed = resume_exports(Some(&answer), &saved, &cancel, None).unwrap();
        assert_eq!(
            resumed, first,
            "Removing an unrelated collision must not create another export"
        );
        assert_eq!(
            fs::read_dir(answer.parent().unwrap()).unwrap().count(),
            count - 1
        );
    }
    #[test]
    fn expired_recovery_refuses_reuse_or_publication() {
        let home = tempfile::tempdir().unwrap();
        let (answer, receipt) = fixture(home.path());
        let cancel = CancellationToken::new();
        let existing =
            export_files(Some(&answer), std::slice::from_ref(&receipt), &cancel).unwrap();
        let before = fs::read_dir(answer.parent().unwrap()).unwrap().count();
        let deadline = Some(std::time::Instant::now());
        for unique in [false, true] {
            let destination = std::path::PathBuf::from(existing[0]["path"].as_str().unwrap());
            assert!(
                resume_saved_file(&receipt, &destination, unique, None, &[], &cancel, deadline)
                    .is_err()
            );
            assert!(
                resume_saved_file(
                    &receipt,
                    &destination.with_file_name("PUBLIC-never-created.bin"),
                    unique,
                    None,
                    &[],
                    &cancel,
                    deadline
                )
                .is_err()
            );
        }
        assert_eq!(
            fs::read_dir(answer.parent().unwrap()).unwrap().count(),
            before
        );
        verify_saved_file(&existing[0], &cancel).unwrap();
    }
}

//! Historical browser observations retain provenance without granting completion or retirement.
//! Reference: src/cli/harvestIntegrity.ts.
use crate::{
    browser::conversation_id,
    native_fs::{NativeEntry, canonical_wire_path, read_stamp},
    sessions::{SessionStore, merge_object, timestamp},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

const WARNING: &str = "browser-harvest-integrity";
const MESSAGE: &str = "Harvest or saved conversation identities conflict. Original artifacts were preserved; inspect browser.harvest.integrity before using the answer.";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapturedIdentity {
    pub source: String,
    pub conversation_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HarvestIntegrity {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_conversation_id: Option<String>,
    pub captured: Vec<CapturedIdentity>,
    pub unverified_sources: Vec<String>,
    pub explicit_target: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_harvest_conversation_id: Option<String>,
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}
fn basis(meta: &Value) -> Value {
    json!([
        meta["browser"]["runtime"]["conversationId"],
        meta["browser"]["runtime"]["tabUrl"],
        meta["browser"]["archive"]["conversationUrl"],
        meta["artifacts"].as_array().map(|artifacts| artifacts
            .iter()
            .map(|a| json!([a["kind"], a["path"], a["sourceUrl"]]))
            .collect::<Vec<_>>()),
        meta["browser"]["harvest"]["conversationId"],
        meta["browser"]["harvest"]["integrity"],
    ])
}
fn add(captured: &mut Vec<CapturedIdentity>, source: &str, id: Option<String>) {
    if let Some(conversation_id) = id {
        captured.push(CapturedIdentity {
            source: source.into(),
            conversation_id,
        });
    }
}
fn metadata_evidence(meta: &Value) -> (Vec<CapturedIdentity>, Vec<String>) {
    let mut captured = vec![];
    let mut unverified = vec![];
    add(
        &mut captured,
        "runtime",
        text(&meta["browser"]["runtime"]["conversationId"]).map(str::to_owned),
    );
    let mut add_url = |source: &str, value: &Value, required: bool| {
        if let Some(url) = text(value) {
            let id = conversation_id(url);
            if id.is_none() && required {
                unverified.push(source.into());
            }
            add(&mut captured, source, id);
        }
    };
    add_url("runtime-url", &meta["browser"]["runtime"]["tabUrl"], true);
    add_url(
        "archive",
        &meta["browser"]["archive"]["conversationUrl"],
        true,
    );
    for (index, artifact) in meta["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        add_url(
            &format!("artifact:{index}"),
            &artifact["sourceUrl"],
            matches!(
                artifact["kind"].as_str(),
                Some("transcript" | "deep-research-report")
            ),
        );
    }
    (captured, unverified)
}

fn transcript_id(
    directory: &NativeEntry,
    artifact_dir: &Path,
    filename: &Path,
) -> Result<Option<String>> {
    let entry = directory.open_child(
        artifact_dir,
        filename
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Missing transcript filename"))?,
        false,
    )?;
    let stamp = read_stamp(&entry.handle)?;
    let mut bytes = vec![];
    (&entry.handle).take(2048).read_to_end(&mut bytes)?;
    ensure!(
        stamp == read_stamp(&entry.handle)?,
        "Transcript changed during inspection"
    );
    directory.require_matches(artifact_dir)?;
    entry.require_matches(filename)?;
    let contents = String::from_utf8_lossy(&bytes);
    let mut lines = contents.split_inclusive('\n');
    fn line(s: &str) -> &str {
        let s = s.strip_suffix('\n').unwrap_or(s);
        s.strip_suffix('\r').unwrap_or(s)
    }
    if lines
        .next()
        .is_none_or(|s| line(s) != "# Oracle Browser Transcript")
        || lines.next().is_none_or(|s| !line(s).is_empty())
    {
        return Ok(None);
    }
    let Some(third) = lines.next().filter(|s| s.ends_with('\n')) else {
        return Ok(None);
    };
    Ok(line(third)
        .strip_prefix("Conversation: ")
        .and_then(conversation_id))
}
fn normalise(path: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    Some(result)
}
fn inspect(
    meta: &Value,
    harvested: &Value,
    explicit: bool,
    artifact_dir: &Path,
) -> HarvestIntegrity {
    let (mut captured, mut unverified) = metadata_evidence(meta);
    match NativeEntry::open(artifact_dir, true) {
        Ok(directory) => {
            let mut transcripts = BTreeMap::from([(artifact_dir.join("transcript.md"), false)]);
            for artifact in meta["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["kind"] == "transcript")
            {
                let candidate = text(&artifact["path"]).and_then(|raw| {
                    let path = Path::new(raw);
                    let candidate = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        normalise(&artifact_dir.parent()?.join(path))?
                    };
                    let parent = candidate.parent()?;
                    let witness = NativeEntry::open(parent, true).ok()?;
                    (witness.identity == directory.identity
                        && canonical_wire_path(parent).ok()?.as_path() == artifact_dir)
                        .then(|| artifact_dir.join(candidate.file_name().unwrap_or_default()))
                });
                if let Some(candidate) = candidate.filter(|p| p.parent() == Some(artifact_dir)) {
                    transcripts.insert(candidate, true);
                } else {
                    unverified.push("external-transcript-path".into());
                }
            }
            for (filename, expected) in transcripts {
                let source = format!(
                    "transcript:{}",
                    filename.file_name().unwrap_or_default().to_string_lossy()
                );
                let id = transcript_id(&directory, artifact_dir, &filename)
                    .ok()
                    .flatten();
                if id.is_none()
                    && (expected
                        || !matches!(fs::symlink_metadata(&filename), Err(e) if e.kind() == std::io::ErrorKind::NotFound))
                {
                    unverified.push(source.clone());
                }
                add(&mut captured, &source, id);
            }
            if directory.require_matches(artifact_dir).is_err() {
                unverified.push("artifact-directory".into());
            }
        }
        Err(_)
            if !matches!(fs::symlink_metadata(artifact_dir), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
                || meta["artifacts"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["kind"] == "transcript")) =>
        {
            unverified.push("artifact-directory".into());
        }
        Err(_) => {}
    }
    build_integrity(meta, harvested, explicit, captured, unverified)
}
fn build_integrity(
    meta: &Value,
    harvested: &Value,
    explicit: bool,
    captured: Vec<CapturedIdentity>,
    unverified_sources: Vec<String>,
) -> HarvestIntegrity {
    let observed = harvested["conversationId"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| harvested["url"].as_str().and_then(conversation_id));
    let ids: HashSet<_> = captured
        .iter()
        .map(|entry| &entry.conversation_id)
        .collect();
    let previous = &meta["browser"]["harvest"];
    let mismatch = ids.len() > 1
        || observed
            .as_ref()
            .map_or(previous["integrity"]["status"] == "mismatch", |id| {
                captured.iter().any(|entry| &entry.conversation_id != id)
            });
    HarvestIntegrity {
        status: if mismatch {
            "mismatch"
        } else if observed.as_ref().is_some_and(|s| !s.is_empty())
            && !captured.is_empty()
            && unverified_sources.is_empty()
        {
            "matched"
        } else {
            "unverified"
        }
        .into(),
        observed_conversation_id: observed,
        captured,
        unverified_sources,
        explicit_target: explicit,
        previous_harvest_conversation_id: previous["conversationId"]
            .as_str()
            .or_else(|| previous["integrity"]["previousHarvestConversationId"].as_str())
            .map(str::to_owned),
    }
}
fn patch(current: &Value, harvested: &Value, integrity: &HarvestIntegrity) -> Value {
    let browser = &current["browser"];
    let mut warnings: Vec<_> = browser["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| integrity.status == "unverified" || warning["code"] != WARNING)
        .cloned()
        .collect();
    if integrity.status == "mismatch" {
        warnings.push(json!({"code":WARNING,"severity":"warning","message":MESSAGE}));
    }
    let answer = harvested["lastAssistantMarkdown"]
        .as_str()
        .or_else(|| harvested["lastAssistantText"].as_str())
        .unwrap_or("");
    let mut harvest = merge_object(
        &browser["harvest"],
        &json!({
            "harvestedAt":timestamp(),"assistantHash":Sha1::digest(answer.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>(),"integrity":integrity,
        }),
    );
    for key in [
        "targetId",
        "url",
        "state",
        "stopExists",
        "sendExists",
        "assistantCount",
        "currentModelLabel",
        "lastAssistantSnippet",
    ] {
        if let Some(value) = harvested.get(key) {
            harvest[key] = value.clone();
        } else {
            harvest.as_object_mut().unwrap().remove(key);
        }
    }
    if let Some(id) = &integrity.observed_conversation_id {
        harvest["conversationId"] = json!(id);
    } else {
        harvest.as_object_mut().unwrap().remove("conversationId");
    }
    json!({"browser":merge_object(browser, &json!({"warnings":warnings,"harvest":harvest}))})
}

pub fn persist(
    store: &SessionStore,
    id: &str,
    harvested: &Value,
    explicit: bool,
) -> Result<HarvestIntegrity> {
    persist_with(store, id, harvested, explicit, |meta, directory| {
        Ok(inspect(meta, harvested, explicit, directory))
    })
}
fn persist_with(
    store: &SessionStore,
    id: &str,
    harvested: &Value,
    explicit: bool,
    inspect: impl Fn(&Value, &Path) -> Result<HarvestIntegrity>,
) -> Result<HarvestIntegrity> {
    let session_path = store.directory(id)?;
    let session = NativeEntry::open(&session_path, true)?;
    let resolved = canonical_wire_path(&session_path)?;
    session.require_matches(&resolved)?;
    let directory = resolved.join("artifacts");
    for _ in 0..3 {
        let meta = store.require(id)?;
        let original = basis(&meta);
        let integrity = inspect(&meta, &directory)?;
        let mut committed = false;
        store.update(id, |current| {
            session.require_matches(&session_path)?;
            session.require_matches(&resolved)?;
            if basis(current) != original {
                return Ok(None);
            }
            committed = true;
            Ok(Some(patch(current, harvested, &integrity)))
        })?;
        if committed {
            ensure!(integrity.status != "mismatch" || explicit, "{MESSAGE}");
            return Ok(integrity);
        }
    }
    let mut result = None;
    store.update(id, |current| {
        session.require_matches(&session_path)?;
        session.require_matches(&resolved)?;
        let (captured, mut unverified) = metadata_evidence(current);
        unverified.push("transcript-inspection-changed".into());
        let mut integrity = build_integrity(current, harvested, explicit, captured, unverified);
        integrity.status = "unverified".into();
        let patch = patch(current, harvested, &integrity);
        result = Some(integrity);
        Ok(Some(patch))
    })?;
    Ok(result.expect("committed harvest integrity"))
}

#[cfg(test)]
#[path = "browser_harvest_tests.rs"]
mod tests;

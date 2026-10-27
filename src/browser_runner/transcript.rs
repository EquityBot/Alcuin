//! Reference session artifacts for the user session: the browser transcript and, for Deep
//! Research, the report (src/browser/artifacts.ts, ensureSessionArtifacts).
use super::*;

/// Kinds the reference lists under "## Artifacts" (saved images, files and the report).
const LISTED: [&str; 3] = ["image", "file", "deep-research-report"];

/// One transcript per local user session; a remote host's run is the client's to record.
pub(crate) fn wanted(current: &Value) -> bool {
    current["browser"]["remoteDelivery"].is_null()
        && !current["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|artifact| artifact["kind"] == "transcript")
}

/// Write the reference artifacts and return their records.
pub(crate) fn save(
    store: &SessionStore,
    id: &str,
    current: &Value,
    prompt: &str,
    answer: &str,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let directory = store.directory(id)?.join("artifacts");
    let runtime = &current["browser"]["runtime"];
    let conversation = runtime["acceptedObservation"]["currentUrl"]
        .as_str()
        .or_else(|| runtime["tabUrl"].as_str());
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(vec![]);
    }
    // Reference ensureSessionArtifacts: a deep run keeps one report, the trimmed answer.
    let mut saved = vec![];
    let reported = current["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|artifact| artifact["kind"] == "deep-research-report");
    if current["browser"]["config"]["researchMode"] == "deep"
        && !reported
        && answer.encode_utf16().count() >= 40
        && !crate::browser_research::incomplete_text(answer)
    {
        saved.push(write(
            &directory,
            "deep-research-report",
            "deep-research-report.md",
            "Deep Research report",
            answer,
            conversation,
            cancel,
        )?);
    }
    let listed = current["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(&saved)
        .filter(|artifact| LISTED.contains(&artifact["kind"].as_str().unwrap_or("")))
        .map(|artifact| {
            let label = artifact["label"]
                .as_str()
                .or_else(|| artifact["kind"].as_str())
                .unwrap_or("");
            let mut line = format!("- {label}: {}", artifact["path"].as_str().unwrap_or(""));
            if let Some(hash) = artifact["sha256"].as_str() {
                line += &format!(" sha256={hash}");
            }
            if let Some(status) = artifact["transfer"]["status"].as_str() {
                line += &format!(" transfer={status}");
            }
            if artifact["validation"].is_object() {
                let state = if artifact["validation"]["ok"] == true {
                    "ok"
                } else {
                    artifact["validation"]["error"].as_str().unwrap_or("failed")
                };
                line += &format!(" validation={state}");
            }
            line
        })
        .collect::<Vec<_>>();
    let mut body = vec!["# Oracle Browser Transcript".to_owned()];
    match conversation {
        Some(url) => body.extend([String::new(), format!("Conversation: {url}"), String::new()]),
        None => body.extend([String::new(), String::new()]),
    }
    body.extend([
        "## Prompt".into(),
        String::new(),
        prompt.trim().into(),
        String::new(),
        format!("## {}", crate::browser_output::answer_heading(current)),
        String::new(),
        answer.into(),
    ]);
    if !listed.is_empty() {
        body.extend([String::new(), "## Artifacts".into(), String::new()]);
        body.extend(listed);
    }
    saved.push(write(
        &directory,
        "transcript",
        "transcript.md",
        "Browser transcript",
        &body.join("\n"),
        conversation,
        cancel,
    )?);
    Ok(saved)
}

/// Reference writeTextBrowserArtifact: contents plus a newline under a unique name.
fn write(
    directory: &Path,
    kind: &str,
    filename: &str,
    label: &str,
    contents: &str,
    source: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Value> {
    let bytes = format!("{contents}\n");
    let published = artifacts::publish(bytes.as_bytes(), &directory.join(filename), true, cancel)?;
    let mut record = json!({"kind":kind,"path":published.path,"label":label,"mimeType":"text/markdown",
        "sizeBytes":published.size_bytes,"sha256":published.sha256,"nativeIdentity":published.native_identity,
        "validation":{"type":"generic","ok":true},"transfer":{"status":"not-needed"},"origin":{"mode":"local"}});
    if let Some(source) = source {
        record["sourceUrl"] = json!(source);
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_research_runs_save_the_report_and_list_it_in_the_transcript() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta = store
            .create(json!({"prompt":"PUBLIC research"}), home.path(), None)
            .unwrap();
        let id = meta["id"].as_str().unwrap();
        let report = "PUBLIC research report with enough words to keep.";
        let current = json!({"browser":{"config":{"researchMode":"deep"},"runtime":{"acceptedObservation":{"currentUrl":"https://chatgpt.com/c/public"}}},
            "artifacts":[{"kind":"image","path":"/public/a.png","sha256":"abc","validation":{"ok":false,"error":"bad"}},{"kind":"output","path":"/public/out.md"}]});
        let saved = save(
            &store,
            id,
            &current,
            " PUBLIC prompt ",
            report,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0]["kind"], "deep-research-report");
        assert_eq!(saved[0]["label"], "Deep Research report");
        assert_eq!(saved[0]["sourceUrl"], "https://chatgpt.com/c/public");
        let path = |n: usize| saved[n]["path"].as_str().unwrap().to_owned();
        assert_eq!(fs::read_to_string(path(0)).unwrap(), format!("{report}\n"));
        let heading = crate::browser_output::answer_heading(&current);
        assert_eq!(
            fs::read_to_string(path(1)).unwrap(),
            format!(
                "# Oracle Browser Transcript\n\nConversation: https://chatgpt.com/c/public\n\n## Prompt\n\nPUBLIC prompt\n\n## {heading}\n\n{report}\n\n## Artifacts\n\n- image: /public/a.png sha256=abc validation=bad\n- Deep Research report: {} sha256={} transfer=not-needed validation=ok\n",
                path(0),
                saved[0]["sha256"].as_str().unwrap()
            )
        );
        // Short or placeholder reports are not saved; an empty answer writes no transcript.
        let short = save(
            &store,
            id,
            &current,
            "p",
            "Researching",
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(short.len(), 1);
        assert_eq!(short[0]["kind"], "transcript");
        assert!(
            save(&store, id, &current, "p", " \n", &CancellationToken::new())
                .unwrap()
                .is_empty()
        );
    }
}

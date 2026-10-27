//! Historical tab observation borrows CDP connections without submission or retirement authority.
//! Reference: src/browser/liveTabs.ts and src/cli/browserTabs.ts.
use crate::{
    browser::{TerminalGate, TerminalSample, conversation_id, prompt_fingerprint, provider_url},
    browser_chrome::AttachmentEndpoint,
    cdp::Cdp,
    policy,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const INSPECT: &str = include_str!("../browser-scripts/historical-inspection.js");
const COPY: &str = include_str!("../browser-scripts/historical-copy-markdown.js");

fn normalise(text: &str) -> String {
    text.split(policy::javascript_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
fn snippet(text: &str) -> String {
    let normalised = normalise(text);
    if normalised.encode_utf16().count() <= 140 {
        return normalised;
    }
    let mut units = 0;
    let prefix: String = normalised
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 139
        })
        .collect();
    format!("{}…", policy::trim(&prefix))
}
fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !policy::trim(s).is_empty())
}
pub fn classify(summary: &Value) -> &'static str {
    if summary["authenticated"] != true {
        "detached"
    } else if summary["stopExists"] == true {
        "running"
    } else if summary["completionConfirmed"] == true {
        "completed"
    } else if summary["sendExists"] == true
        || summary["promptReady"] == true
        || summary["assistantCount"].as_f64().is_some_and(|n| n > 0.0)
    {
        "running"
    } else {
        "detached"
    }
}
fn summary(endpoint: &AttachmentEndpoint, target: &Value, info: &Value) -> Result<Value> {
    ensure!(
        info.is_object() && info["identityChanged"] != true,
        "ChatGPT conversation changed or inspection failed; refusing mixed capture."
    );
    let snapshot = &info["assistantSnapshot"];
    let inspected_index = info["lastAssistantTurnIndex"].as_u64();
    let matches = snapshot["turnIndex"]
        .as_u64()
        .is_some_and(|index| Some(index) == inspected_index)
        || snapshot["turnIndex"].is_null()
            && inspected_index.is_none()
            && nonempty(&snapshot["text"]).is_some_and(|text| {
                normalise(text).to_lowercase()
                    == normalise(info["lastAssistantText"].as_str().unwrap_or("")).to_lowercase()
            });
    let answer = if matches {
        nonempty(&snapshot["text"])
    } else {
        None
    }
    .unwrap_or_else(|| info["lastAssistantText"].as_str().unwrap_or(""));
    let answer = policy::trim(answer);
    let user = policy::trim(info["lastUserText"].as_str().unwrap_or(""));
    let url = policy::trim(
        info["url"]
            .as_str()
            .or_else(|| target["url"].as_str())
            .unwrap_or(""),
    );
    ensure!(
        provider_url(url).is_some(),
        "Historical inspection left the provider origin"
    );
    let mut output = json!({
        "host":endpoint.host,"port":endpoint.port,"targetId":target["targetId"],
        "url":url,"title":normalise(info["title"].as_str().or_else(||target["title"].as_str()).unwrap_or("")),
        "currentModelLabel":normalise(info["currentModelLabel"].as_str().unwrap_or("")),
        "completionVisible":matches && info["completionVisible"] == true,"completionConfirmed":false,
        "strongThinkingActive":info["strongThinkingActive"] == true,
        "assistantCount":info["assistantCount"].as_f64().filter(|n| n.is_finite()).unwrap_or(0.0),
        "lastAssistantText":answer,"lastAssistantSnippet":snippet(answer),"lastAssistantMarkdown":null,
        "lastUserText":user,"lastUserSnippet":snippet(user),
        "visibilityState":info["visibilityState"].as_str().unwrap_or(""),
    });
    for key in [
        "stopExists",
        "sendExists",
        "promptReady",
        "loginButtonExists",
        "authenticated",
        "assistantFollowsLatestUser",
        "focused",
    ] {
        output[key] = json!(info[key] == true);
    }
    for key in [
        "lastUserTextRaw",
        "lastUserContentText",
        "lastUserOriginalSourceText",
        "lastUserOriginalSourceMessageId",
        "lastUserMessageId",
    ] {
        if let Some(value) = info.get(key) {
            output[key] = value.clone();
        }
    }
    for key in ["lastUserTurnIndex", "lastAssistantTurnIndex"] {
        if let Some(index) = info[key].as_u64() {
            output[key] = json!(index);
        }
    }
    if matches {
        for (from, to) in [
            ("messageId", "lastAssistantMessageId"),
            ("turnId", "lastAssistantTurnId"),
        ] {
            if snapshot[from].is_string() {
                output[to] = snapshot[from].clone();
            }
        }
    }
    if let Some(id) = conversation_id(url) {
        output["conversationId"] = json!(id);
    }
    output["state"] = json!(classify(&output));
    let fingerprint = format!(
        "{}|{}|{}",
        target["targetId"].as_str().unwrap_or(""),
        url,
        answer
    );
    output["fingerprint"] = json!(
        Sha1::digest(fingerprint.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    Ok(output)
}
pub async fn targets(
    browser: &Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let raw = browser
        .call("Target.getTargets", json!({}), timeout, cancel)
        .await?;
    let rows = raw["targetInfos"]
        .as_array()
        .context("Chrome target list is incomplete")?;
    ensure!(
        rows.iter().all(|row| nonempty(&row["targetId"]).is_some()),
        "Chrome target list is malformed"
    );
    Ok(rows
        .iter()
        .filter(|row| {
            row["type"] == "page"
                && row["url"]
                    .as_str()
                    .is_some_and(|url| provider_url(url).is_some())
        })
        .cloned()
        .collect())
}
async fn inspect_page(
    page: &Cdp,
    endpoint: &AttachmentEndpoint,
    target: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    let url = target["url"]
        .as_str()
        .and_then(provider_url)
        .context("Historical target must be a provider page")?;
    let args = json!([{"expectedConversationId":conversation_id(url.as_str()),"expectedOrigin":url.origin().ascii_serialization()}]);
    let info = page
        .evaluate_with_arguments(INSPECT, &args, timeout, cancel)
        .await?;
    summary(endpoint, target, &info)
}
pub async fn inspect(
    endpoint: &AttachmentEndpoint,
    target: &Value,
    markdown: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    // Each observation owns only its transport. Disconnecting releases all attached sessions.
    let browser = endpoint.connect(timeout, cancel).await?;
    let result = async {
        ensure!(
            target["url"]
                .as_str()
                .is_some_and(|url| provider_url(url).is_some()),
            "Historical target must be a provider page"
        );
        let page = browser
            .attach_target(
                target["targetId"]
                    .as_str()
                    .context("Missing historical target ID")?,
                timeout,
                cancel,
            )
            .await?;
        page.call("Runtime.enable", json!({}), timeout, cancel)
            .await?;
        page.call("DOM.enable", json!({}), timeout, cancel).await?;
        let mut observed = inspect_page(&page, endpoint, target, timeout, cancel).await?;
        if markdown
            && observed["stopExists"] != true
            && (nonempty(&observed["lastAssistantMessageId"]).is_some()
                || nonempty(&observed["lastAssistantTurnId"]).is_some())
        {
            let copied = copy_markdown(&page, endpoint, target, &observed, timeout, cancel).await?;
            ensure!(!cancel.is_cancelled(), "Historical capture cancelled");
            let current = inspect_page(&page, endpoint, target, timeout, cancel).await?;
            ensure!(
                capture_identity(&current, true) == capture_identity(&observed, true),
                "ChatGPT conversation changed during Markdown capture; refusing mixed output."
            );
            ensure!(
                copied["status"] != "conversation-changed",
                "ChatGPT conversation changed during Markdown capture"
            );
            if copied["success"] == true
                && let Some(text) = nonempty(&copied["markdown"])
            {
                observed["lastAssistantMarkdown"] = json!(text);
            }
        }
        if markdown && observed["lastAssistantMarkdown"].is_null() {
            observed["lastAssistantMarkdown"] = observed["lastAssistantText"].clone();
        }
        Ok(observed)
    }
    .await;
    let cleanup = browser
        .disconnect_and_wait(Duration::from_millis(500))
        .await;
    match (result, cleanup) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    }
}
async fn copy_markdown(
    page: &Cdp,
    endpoint: &AttachmentEndpoint,
    target: &Value,
    observed: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    let mut cfg = json!({"action":"prepare","captureToken":uuid::Uuid::new_v4().to_string(),"timeoutMs":timeout.as_millis().clamp(1,10_000) as u64,"meta":{"messageId":observed["lastAssistantMessageId"],"turnId":observed["lastAssistantTurnId"]},"receipt":{"conversation":{"conversationId":observed["conversationId"],"origin":provider_url(observed["url"].as_str().context("Missing observed provider URL")?).context("Provider URL changed")?.origin().ascii_serialization()}}});
    let result = async {
        let prepared = page
            .evaluate_with_arguments(COPY, &json!([&cfg]), timeout, cancel)
            .await?;
        ensure!(
            prepared["prepared"] == true,
            "Historical copy preparation was refused"
        );
        cfg["action"] = json!("run");
        page.evaluate_with_arguments(COPY, &json!([&cfg]), timeout, cancel)
            .await
    }
    .await;
    if result.is_err() {
        cfg["action"] = json!("cancel");
        cancel_copy(endpoint, target, &cfg)
            .await
            .context("Historical copy timer/clipboard cleanup could not be acknowledged")?;
    }
    result
}
async fn cancel_copy(endpoint: &AttachmentEndpoint, target: &Value, cfg: &Value) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    let remaining = || {
        deadline
            .checked_duration_since(tokio::time::Instant::now())
            .context("Historical copy cleanup deadline reached")
    };
    let token = CancellationToken::new();
    let browser = endpoint.connect(remaining()?, &token).await?;
    let result = async {
        let page = browser
            .attach_target(
                target["targetId"]
                    .as_str()
                    .context("Missing historical cleanup target")?,
                remaining()?,
                &token,
            )
            .await?;
        let ack = page
            .evaluate_with_arguments(COPY, &json!([cfg]), remaining()?, &token)
            .await?;
        ensure!(
            ack["cancelled"] == true && ack["settled"] == true,
            "Historical copy cleanup is unsettled"
        );
        Ok(())
    }
    .await;
    let cleanup = browser
        .disconnect_and_wait(Duration::from_millis(100))
        .await;
    result?;
    cleanup
}
pub fn capture_identity(tab: &Value, assistant: bool) -> Value {
    let mut identity = json!([
        tab["url"],
        tab["conversationId"],
        tab["authenticated"],
        tab["lastUserMessageId"],
        tab["lastUserTurnIndex"],
        tab["lastUserOriginalSourceText"],
        tab["lastUserOriginalSourceMessageId"],
        tab.get("lastUserContentText")
            .filter(|v| !v.is_null())
            .or_else(|| tab.get("lastUserTextRaw").filter(|v| !v.is_null()))
            .unwrap_or(&tab["lastUserText"]),
    ]);
    if assistant {
        identity.as_array_mut().unwrap().extend([
            tab["assistantFollowsLatestUser"].clone(),
            tab["assistantCount"].clone(),
            tab["lastAssistantMessageId"].clone(),
            tab["lastAssistantTurnId"].clone(),
            tab["lastAssistantTurnIndex"].clone(),
            tab["lastAssistantText"].clone(),
            tab["stopExists"].clone(),
            tab["completionVisible"].clone(),
            tab["strongThinkingActive"].clone(),
        ]);
    }
    identity
}
pub async fn collect(
    endpoint: &AttachmentEndpoint,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let browser = endpoint.connect(timeout, cancel).await?;
    let list = targets(&browser, timeout, cancel).await;
    let cleanup = browser
        .disconnect_and_wait(Duration::from_millis(500))
        .await;
    let list = list?;
    cleanup?;
    let mut summaries = vec![];
    for target in list {
        let observed = inspect(endpoint, &target, false, timeout, cancel).await;
        ensure!(!cancel.is_cancelled(), "Historical collection cancelled");
        summaries.push(match observed {Ok(value)=>value,Err(error)=>json!({"host":endpoint.host,"port":endpoint.port,"targetId":target["targetId"],"title":target["title"],"url":target["url"],"conversationId":target["url"].as_str().and_then(conversation_id),"state":"detached","error":error.to_string()})});
    }
    summaries.sort_by_key(|row| {
        std::cmp::Reverse(
            u8::from(row["focused"] == true) * 100
                + u8::from(row["url"].as_str().is_some_and(|s| s.contains("/c/"))) * 10,
        )
    });
    Ok(summaries)
}
pub fn resolve(summaries: &[Value], reference: Option<&str>) -> Result<Value> {
    let reference = reference
        .map(policy::trim)
        .filter(|s| !s.is_empty() && *s != "current");
    let Some(reference) = reference else {
        return summaries
            .first()
            .cloned()
            .context("No ChatGPT tabs found on Chrome.");
    };
    if let Some(found) = summaries.iter().find(|row| {
        ["targetId", "url", "conversationId"]
            .iter()
            .any(|key| row[key].as_str() == Some(reference))
    }) {
        return Ok(found.clone());
    }
    let needle = reference.to_lowercase();
    let matches: Vec<_> = summaries
        .iter()
        .filter(|row| {
            row["title"]
                .as_str()
                .is_some_and(|title| title.to_lowercase().contains(&needle))
        })
        .collect();
    match matches.as_slice() {
        [found] => Ok((*found).clone()),
        [] => bail!("No ChatGPT tab matched \"{reference}\"."),
        _ => bail!(
            "Multiple ChatGPT tabs matched \"{reference}\"; use a target ID, conversation ID or URL."
        ),
    }
}
pub async fn resolve_target(
    endpoint: &AttachmentEndpoint,
    reference: Option<&str>,
    strict: bool,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    if let Some(reference) = reference
        .map(policy::trim)
        .filter(|s| !s.is_empty() && *s != "current")
    {
        let browser = endpoint.connect(timeout, cancel).await?;
        let list = targets(&browser, timeout, cancel).await;
        let cleanup = browser
            .disconnect_and_wait(Duration::from_millis(500))
            .await;
        let list = list?;
        cleanup?;
        if let Some(found) = list.into_iter().find(|row| {
            ["targetId", "url", "conversationId"].iter().any(|key| {
                if *key == "conversationId" {
                    row["url"].as_str().and_then(conversation_id).as_deref() == Some(reference)
                } else {
                    row[key].as_str() == Some(reference)
                }
            })
        }) {
            return Ok(found);
        }
    }
    ensure!(
        !strict,
        "No ChatGPT tab matched the saved conversation identity; refusing unrelated tab inspection."
    );
    resolve(&collect(endpoint, timeout, cancel).await?, reference)
}

pub fn matches_submitted_prompt(
    tab: &Value,
    fingerprint: Option<&Value>,
    format: Option<&Value>,
) -> bool {
    if let Some(format) = format {
        return format == "original-source-v1"
            && fingerprint.and_then(Value::as_str).is_some_and(|hash| {
                nonempty(&tab["lastUserOriginalSourceMessageId"]).is_some_and(|id| {
                    tab["lastUserOriginalSourceText"]
                        .as_str()
                        .is_some_and(|source| prompt_fingerprint(source, id) == hash)
                })
            });
    }
    fingerprint.is_none()
        || fingerprint.and_then(Value::as_str).is_some_and(|hash| {
            nonempty(&tab["lastUserMessageId"]).is_some_and(|id| {
                [
                    tab.get("lastUserTextRaw")
                        .filter(|v| !v.is_null())
                        .unwrap_or(&tab["lastUserText"]),
                    &tab["lastUserContentText"],
                ]
                .iter()
                .any(|value| {
                    value
                        .as_str()
                        .is_some_and(|source| prompt_fingerprint(source, id) == hash)
                })
            })
        })
}
pub fn ready_for_session(meta: &Value, tab: &Value, explicit: bool) -> Result<bool> {
    let runtime = &meta["browser"]["runtime"];
    ensure!(
        explicit || runtime.get("submittedPromptHash") != Some(&Value::Null),
        "This browser session has no confirmed submitted user turn; retry after submission or use --browser-tab to inspect a specific tab."
    );
    let paired = tab["assistantFollowsLatestUser"] == true
        && nonempty(
            tab.get("lastAssistantMarkdown")
                .filter(|v| !v.is_null())
                .unwrap_or(&tab["lastAssistantText"]),
        )
        .is_some();
    Ok(paired
        && (explicit
            || tab["completionConfirmed"] == true
                && matches_submitted_prompt(
                    tab,
                    runtime.get("submittedPromptHash"),
                    runtime.get("submittedPromptHashFormat"),
                )))
}
#[derive(Default)]
pub struct HarvestCompletionGate(TerminalGate);
impl HarvestCompletionGate {
    pub fn observe(
        &mut self,
        tab: &mut Value,
        now_ms: u64,
        confirm_cycles: u32,
        min_stable_ms: u64,
    ) {
        let source = [
            "lastUserOriginalSourceText",
            "lastUserContentText",
            "lastUserTextRaw",
            "lastUserText",
        ]
        .iter()
        .find_map(|key| tab.get(key).filter(|v| !v.is_null()));
        let key = json!([
            tab["targetId"],
            tab["conversationId"],
            tab["lastUserMessageId"],
            tab["lastUserOriginalSourceMessageId"],
            source,
            tab["lastAssistantMessageId"],
            tab["lastAssistantTurnId"],
            tab["lastAssistantTurnIndex"],
            tab["lastAssistantText"]
        ]);
        let terminal = self.0.observe(
            &TerminalSample {
                now_ms,
                length: tab["lastAssistantText"]
                    .as_str()
                    .map_or(0, |s| s.encode_utf16().count()),
                content_key: key.to_string(),
                stop_visible: tab["stopExists"] == true,
                bar_visible: tab["completionVisible"] == true
                    && tab["assistantFollowsLatestUser"] == true,
                strong_thinking_active: tab["strongThinkingActive"] != false,
            },
            confirm_cycles,
            min_stable_ms,
        );
        tab["completionConfirmed"] = json!(terminal);
        tab["state"] = json!(if tab["authenticated"] != true {
            "detached"
        } else if terminal {
            "completed"
        } else {
            "running"
        });
    }
}

#[cfg(test)]
#[path = "browser_tabs_tests.rs"]
mod tests;

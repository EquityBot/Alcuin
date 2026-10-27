//! Grok Private and message identity evidence. ChatGPT validators stay separate.
use crate::{browser_commit::SubmissionOwner, cdp::Cdp};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PRIVATE: &str = include_str!("../browser-scripts/grok-private.js");
const ENTRY: &str = include_str!("../browser-scripts/grok-entry.js");
pub(crate) const WITNESS: &str = include_str!("../browser-scripts/grok-witness.js");
pub(crate) const SEND: &str = include_str!("../browser-scripts/grok-send.js");
pub(crate) const DOM: &str = include_str!("../browser-scripts/grok-dom.js");
const RETIREMENT: &str = include_str!("../browser-scripts/grok-retirement.js");
mod profile;
pub(crate) use profile::{child_config, require_config, require_distinct, require_text_request};

pub(crate) struct Intent {
    pub(crate) dispatch_id: String,
    prompt_sha256: String,
    listed_sha256: Option<String>,
    mode: String,
}
impl Intent {
    pub(crate) fn new(prompt: &str, mode: &str, dispatch_id: &str) -> Result<Self> {
        ensure!(
            !prompt.is_empty()
                && prompt.encode_utf16().count() <= 524288
                && matches!(mode, "Auto" | "Fast")
                && uuid::Uuid::parse_str(dispatch_id)
                    .is_ok_and(|value| value.hyphenated().to_string() == dispatch_id),
            "Invalid Grok dispatch intent"
        );
        Ok(Self {
            dispatch_id: dispatch_id.into(),
            prompt_sha256: crate::artifacts::sha256(prompt.as_bytes()),
            listed_sha256: listed_prompt(prompt)
                .map(|listed| crate::artifacts::sha256(listed.as_bytes())),
            mode: mode.into(),
        })
    }
    pub(crate) fn require(&self, receipt: &Value, owner: &SubmissionOwner) -> Result<()> {
        require_receipt(receipt, owner)?;
        ensure!(
            receipt["dispatchId"] == self.dispatch_id
                && receipt["promptSha256"] == self.prompt_sha256
                && receipt["mode"] == self.mode,
            "Grok receipt differs from its original native dispatch intent"
        );
        Ok(())
    }
    pub(crate) fn send_config(&self, prompt: &str) -> Result<Value> {
        ensure!(
            crate::artifacts::sha256(prompt.as_bytes()) == self.prompt_sha256,
            "Grok send prompt differs from its original intent"
        );
        Ok(
            json!({"prompt":prompt,"promptSha256":self.prompt_sha256,"dispatchId":self.dispatch_id,"mode":self.mode}),
        )
    }
    pub(crate) fn require_answer(
        &self,
        receipt: &Value,
        owner: &SubmissionOwner,
        observed: &Value,
    ) -> Result<()> {
        self.require(receipt, owner)?;
        ensure!(
            receipt["state"] == "completed"
                && observed["state"] == "completed"
                && observed["provider"] == "grok"
                && observed["kind"] == "grok-private-answer"
                && observed["privateChat"] == true
                && observed["mode"] == self.mode
                && observed["wireReceipt"] == *receipt
                && observed["userMessageId"] == receipt["userMessageId"]
                && observed["assistantMessageId"] == receipt["assistantMessageId"]
                && observed["completionConfirmed"] == true
                && observed["stopExists"] == false
                && observed["strongThinkingActive"] == false
                && observed["userText"].as_str().is_some_and(|source| {
                    // grok-dom.js names the match only for a tolerated rendered list, so
                    // exact observations are unchanged.
                    match observed.get("promptMatch") {
                        None => crate::artifacts::sha256(source.as_bytes()) == self.prompt_sha256,
                        // grok-dom.js has also matched the turn's list items to the markers.
                        Some(kind) if kind == "rendered-list" => {
                            self.listed_sha256.as_ref().is_some_and(|listed| {
                                *listed == crate::artifacts::sha256(turn_lines(source).as_bytes())
                            })
                        }
                        Some(_) => false,
                    }
                })
                && observed["text"]
                    .as_str()
                    .is_some_and(|answer| !crate::policy::trim(answer).is_empty())
                && observed["providerText"] == observed["text"]
                && observed["images"].as_array().is_some_and(Vec::is_empty)
                && observed["files"].as_array().is_some_and(Vec::is_empty),
            "Grok answer lacks its complete original Private source, wire completion or qualified text capture"
        );
        Ok(())
    }
}

/// JavaScript's `\s`, which `trim` also removes.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Trimmed non-blank lines of a Grok user turn, as `lines` in grok-dom.js.
fn lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| line.trim_matches(js_space))
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A line without its list marker, as `marker` in grok-dom.js: `-`, `*`, `+`, or 1 to 9
/// ASCII digits then `.` or `)`, followed by whitespace.
fn unmarked(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let rest = match line.strip_prefix(['-', '*', '+']) {
        Some(rest) => rest,
        None if (1..=9).contains(&digits) => line[digits..].strip_prefix(['.', ')'])?,
        None => return None,
    };
    let body = rest.trim_start_matches(js_space);
    (body.len() < rest.len()).then_some(body)
}

/// The lines a rendered list turn must show, or None when the prompt has no list-marker line.
fn listed_prompt(prompt: &str) -> Option<String> {
    let sent = lines(prompt);
    sent.iter().any(|line| unmarked(line).is_some()).then(|| {
        sent.iter()
            .map(|line| unmarked(line).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn turn_lines(source: &str) -> String {
    lines(source).join("\n")
}

pub(crate) fn guard(function: &str) -> String {
    format!(
        "(cfg)=>{{const grokPrivate=()=>({PRIVATE});if(!grokPrivate())throw Error('Verified Grok Private Chat is required');return ({function})(cfg,grokPrivate);}}"
    )
}

pub(crate) async fn require(
    page: &Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(
        page.evaluate(PRIVATE, timeout, cancel).await? == true,
        "Verified Grok Private Chat is required; preserve uncertain requests without replay"
    );
    Ok(())
}

/// The caller owns a freshly allocated target. Toggle at most once, before
/// creating a submission claim, and retain uncertainty if the command fails.
pub(crate) async fn enter_private(
    page: &Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let function = format!("(cfg)=>({ENTRY})(cfg,()=>({PRIVATE}))");
    let deadline = tokio::time::Instant::now() + timeout;
    let mut activation_issued = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        ensure!(
            !remaining.is_zero(),
            "Initial Grok Private entry timed out; retain the original target"
        );
        let observed = page
            .evaluate_with_arguments(&function, &json!([{"action":"inspect"}]), remaining, cancel)
            .await?;
        match observed["state"].as_str() {
            Some("private") => return Ok(()),
            Some("ordinary") if !activation_issued => {
                // Never retry an uncertain click, including a lost CDP reply.
                activation_issued = true;
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "Initial Grok Private activation deadline reached"
                );
                let activated = page
                    .evaluate_with_arguments(
                        &function,
                        &json!([{"action":"activate"}]),
                        remaining,
                        cancel,
                    )
                    .await?;
                ensure!(
                    matches!(
                        activated["state"].as_str(),
                        Some("private" | "activation-issued")
                    ),
                    "Initial Grok Private activation was not confirmed"
                );
            }
            Some("ordinary" | "pending") => {}
            _ => anyhow::bail!("Initial Grok Private observation is invalid"),
        }
        tokio::select! { biased;
            _ = cancel.cancelled() => anyhow::bail!("Initial Grok Private entry cancelled"),
            _ = tokio::time::sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(tokio::time::Instant::now())) ) => {}
        }
    }
}

pub(crate) fn require_receipt(receipt: &Value, owner: &SubmissionOwner) -> Result<()> {
    let uuid = |key: &str| {
        receipt[key].as_str().is_some_and(|id| {
            uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
        })
    };
    let message = |key: &str| {
        receipt[key]
            .as_str()
            .and_then(|id| id.strip_prefix("response-"))
            .is_some_and(|id| {
                uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
            })
    };
    ensure!(
        receipt["version"] == 1
            && receipt["provider"] == "grok"
            && receipt["kind"] == "grok-websocket-v1"
            && receipt["claimId"] == owner.claim_id
            && receipt["reservationId"] == owner.reservation_id
            && matches!(receipt["state"].as_str(), Some("committed" | "completed"))
            && receipt["privateChat"] == true
            && matches!(receipt["mode"].as_str(), Some("Auto" | "Fast"))
            && uuid("dispatchId")
            && uuid("clientMessageId")
            && message("userMessageId")
            && receipt["promptSha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        "Grok receipt does not bind the original Private submission"
    );
    ensure!(
        (receipt["responseId"].is_null() || uuid("responseId"))
            && (receipt["assistantMessageId"].is_null()
                || message("assistantMessageId")
                    && receipt["assistantMessageId"] != receipt["userMessageId"])
            && (receipt["assistantMessageId"].is_null() || uuid("responseId")),
        "Grok partial response identity is invalid"
    );
    if receipt["state"] == "completed" {
        ensure!(
            uuid("responseId")
                && message("assistantMessageId")
                && receipt["assistantMessageId"] != receipt["userMessageId"],
            "Grok completion identity is invalid"
        );
    }
    Ok(())
}

pub(crate) fn read_witness(cleanup: bool) -> String {
    // Cleanup must release payload references even after privacy fails. The
    // retained observer itself checks the exact original document and owner.
    format!(
        "(cfg)=>{{const witness=window.__oracleGrokWitness;if(!witness||witness.owner.claimId!==cfg.owner.claimId||witness.owner.reservationId!==cfg.owner.reservationId||witness.dispatchId!==cfg.dispatchId)throw Error('Original Grok witness is unavailable');return witness.{}();}}",
        if cleanup { "stop" } else { "read" }
    )
}

/// Recovery rebinds only the exact original active submission and its live witness.
/// It reads the renderer claim and changes nothing.
pub(crate) fn recovery_owner() -> String {
    guard(
        "(cfg)=>{const claim=JSON.parse(sessionStorage.getItem('oracle:target-claim')??'null'),witness=window.__oracleGrokWitness;if(!claim||claim.id!==cfg.owner.claimId||claim.retiring||claim.inspection||claim.submission?.id!==cfg.owner.reservationId||claim.submission.state!=='active'||!['started','acknowledged'].includes(claim.submission.dispatch)||!witness||witness.owner.claimId!==cfg.owner.claimId||witness.owner.reservationId!==cfg.owner.reservationId||witness.dispatchId!==cfg.dispatchId)return null;return {dispatch:claim.submission.dispatch,commitReceipt:claim.submission.commitReceipt??null};}",
    )
}

pub(crate) fn witness_config(owner: &SubmissionOwner, dispatch: &str) -> Value {
    json!([{"owner":owner,"dispatchId":dispatch}])
}

pub(crate) fn retirement_mark() -> String {
    guard(RETIREMENT)
}

pub(crate) fn retirement_config(session: &Value, marker: &str) -> Value {
    let runtime = &session["browser"]["runtime"];
    json!({"owner":runtime["submissionOwner"],"retirementId":marker,
        "committedReceipt":runtime["grokCommitReceipt"],"receipt":runtime["acceptedObservation"]["wireReceipt"]})
}

pub(crate) async fn observe_retirement(
    store: &crate::sessions::SessionStore,
    session: &Value,
    page: &Cdp,
    marker: &str,
    timeout: Duration,
) -> Result<Value> {
    let request = crate::browser_runner::request(
        store,
        session["id"].as_str().context("Grok session is missing")?,
    )?;
    accept_saved_answer(
        &request,
        session,
        &session["browser"]["runtime"]["acceptedObservation"],
    )?;
    let mut cfg = retirement_config(session, marker);
    cfg["action"] = json!("observe-retirement");
    cfg["mode"] = session["browser"]["runtime"]["grokCommitReceipt"]["mode"].clone();
    cfg["prompt"] = json!(request.prompt);
    let observed = page
        .evaluate_with_arguments(
            &guard(DOM),
            &json!([cfg]),
            timeout,
            &CancellationToken::new(),
        )
        .await?;
    accept_saved_answer(&request, session, &observed)?;
    ensure!(
        observed == session["browser"]["runtime"]["acceptedObservation"],
        "Grok original Private answer changed before retirement"
    );
    Ok(observed)
}

/// A live witness read extends a saved commitment when it reports the same turn, possibly
/// later: a response identity first seen, or completion. The renderer applies the same rule.
pub(crate) fn extends(saved: &Value, observed: &Value) -> bool {
    const LATER: [&str; 3] = ["state", "responseId", "assistantMessageId"];
    let (Some(saved_fields), Some(observed_fields)) = (saved.as_object(), observed.as_object())
    else {
        return false;
    };
    let same = |key: &str| saved[key] == observed[key];
    saved_fields.len() == observed_fields.len()
        && observed_fields.keys().all(|key| {
            saved_fields.contains_key(key) && (LATER.contains(&key.as_str()) || same(key))
        })
        && matches!(observed["state"].as_str(), Some("committed" | "completed"))
        && (saved["state"] != "completed" || LATER.into_iter().all(same))
        && ["responseId", "assistantMessageId"]
            .into_iter()
            .all(|key| saved[key].is_null() || same(key))
}

/// Revalidate saved native Grok evidence without borrowing ChatGPT's conversation
/// or model contracts. The original request and completed observation are sealed
/// by the existing child/parent lifecycle before callers publish or retire.
pub(crate) fn accept_saved_answer(
    request: &crate::browser_runner::BrowserRequest,
    session: &Value,
    observed: &Value,
) -> Result<()> {
    let runtime = &session["browser"]["runtime"];
    let committed = &runtime["grokCommitReceipt"];
    let completed = &observed["wireReceipt"];
    let owner: SubmissionOwner = serde_json::from_value(runtime["submissionOwner"].clone())?;
    let mode = committed["mode"]
        .as_str()
        .context("Saved Grok mode is missing")?;
    let dispatch = committed["dispatchId"]
        .as_str()
        .context("Saved Grok dispatch is missing")?;
    let intent = Intent::new(&request.prompt, mode, dispatch)?;
    intent.require(committed, &owner)?;
    intent.require_answer(completed, &owner, observed)?;
    let effective = match mode {
        "Auto" => "grok-web-auto",
        "Fast" => "grok-web-fast",
        _ => anyhow::bail!("Unsupported saved Grok mode"),
    };
    ensure!(
        request.session_id == session["id"]
            && request.prompt_sha256 == crate::artifacts::sha256(request.prompt.as_bytes())
            && request.upload_paths.is_empty()
            && session["browser"]["provider"] == "grok"
            && runtime["provider"] == "grok"
            && runtime.get("conversationId").is_none()
            && runtime["submittedPromptHash"] == request.prompt_sha256
            && runtime["submittedPromptHashFormat"] == "grok-request-v1"
            && runtime["effectiveModel"] == effective
            && runtime["modelSelection"]["verified"] == true
            && runtime["modelSelection"]["source"] == "grok-mode-picker"
            && runtime["modelSelection"]["selectedMode"] == mode
            && runtime["modelSelection"]["resolvedLabel"] == format!("Grok {mode}")
            && runtime["thinkingSelection"].is_null()
            && [
                "dispatchId",
                "clientMessageId",
                "userMessageId",
                "promptSha256",
                "mode"
            ]
            .into_iter()
            .all(|key| committed[key] == completed[key]),
        "Saved Grok source, original turn or selected mode differs from its native request"
    );
    ensure!(
        ["responseId", "assistantMessageId"]
            .into_iter()
            .all(|key| committed[key].is_null() || committed[key] == completed[key]),
        "Grok completion contradicts an already observed response identity"
    );
    Ok(())
}

#[cfg(test)]
#[path = "browser_grok_tests.rs"]
pub(crate) mod tests;

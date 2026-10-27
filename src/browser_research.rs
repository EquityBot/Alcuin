//! Deep Research reports retain native frame evidence rather than invented assistant IDs.
use crate::{artifacts::sha256, browser, cdp::Cdp, policy};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const FRAME_STATUS: &str = include_str!("../browser-scripts/research-frame-status.js");
const SELECT: &str = include_str!("../browser-scripts/research-select.js");
const VERIFY: &str = include_str!("../browser-scripts/research-verify.js");
const CLEAN: &str = include_str!("../browser-scripts/research-select-clean.js");

fn selection_expression(body: &str) -> String {
    format!(
        "(()=>{{const researchNavigation=()=>({});return({body});}})()",
        include_str!("../browser-scripts/composer-navigation-check.js")
    )
}
async fn selected(
    reservation: &mut crate::browser_renderer::RendererReservation,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    let read = crate::browser_search::evaluate(
        reservation,
        &selection_expression(VERIFY),
        cfg,
        timeout,
        cancel,
    )
    .await?;
    ensure!(
        read["promptMatches"] == true,
        "Research composer, navigation or complete staged prompt changed; nothing was sent"
    );
    Ok(read["selected"] == true)
}
pub(crate) async fn activate(
    reservation: &mut crate::browser_renderer::RendererReservation,
    mut cfg: Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    cfg["owner"] = json!(reservation.owner());
    cfg["plusGuardId"] = json!(uuid::Uuid::new_v4().to_string());
    cfg["researchGuardId"] = json!(uuid::Uuid::new_v4().to_string());
    if selected(reservation, &cfg, timeout, cancel).await? {
        return Ok(true);
    }
    crate::browser_search::open_tools(
        reservation,
        &cfg,
        timeout,
        cancel,
        "Could not find the composer plus button to activate Deep Research.",
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let selection=async { loop {
        let read=crate::browser_search::evaluate(reservation,&selection_expression(SELECT),&cfg,timeout,cancel).await?;
        if read["status"]=="ready" {break Ok::<_,anyhow::Error>(read);}
        if read["status"]!="missing" {reservation.mark_input_uncertain();}
        ensure!(read["status"]=="missing","One owned Deep Research menu control could not be verified; nothing was sent");
        if Instant::now()>=deadline {
            let available=read["available"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
            let hint=if available.is_empty() {String::new()} else {format!(" Available options: {}",available.join(", "))};
            anyhow::bail!("\"Deep research\" option not found in composer dropdown.{hint} This feature may require a ChatGPT Plus or Pro subscription.");
        }
        tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Research selection cancelled"),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
    }}.await;
    let point = match selection {
        Ok(point) => point,
        Err(error) => {
            let _ = reservation
                .page()
                .evaluate_with_arguments(
                    &crate::browser_search::expression(CLEAN, false),
                    &json!([cfg]),
                    Duration::from_secs(1),
                    &CancellationToken::new(),
                )
                .await;
            reservation.mark_input_uncertain();
            let _ = reservation
                .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
                .await;
            return Err(error);
        }
    };
    let delivered=async {
        let x=point["x"].as_f64().filter(|x|x.is_finite()).context("Research selection point is missing")?;
        let y=point["y"].as_f64().filter(|y|y.is_finite()).context("Research selection point is missing")?;
        let down=reservation.page().call("Input.dispatchMouseEvent",json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),timeout,cancel).await;
        let up=reservation.page().call("Input.dispatchMouseEvent",json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),Duration::from_secs(1),&CancellationToken::new()).await;
        down?;up?;
        ensure!(!cancel.is_cancelled(),"Research selection cancelled during input delivery");
        Ok::<_,anyhow::Error>(())
    }.await;
    let clean = reservation
        .page()
        .evaluate_with_arguments(
            &crate::browser_search::expression(CLEAN, false),
            &json!([cfg]),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
    if delivered.is_err()
        || !clean.as_ref().is_ok_and(|read| {
            read["cleaned"] == true
                && read["present"] == true
                && read["sawPress"] == true
                && read["sawClick"] == true
                && read["blocked"] == false
        })
    {
        reservation.mark_input_uncertain();
        let _ = reservation
            .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await;
        delivered?;
        anyhow::bail!(
            "Research input or guard cleanup is uncertain; retain recovery without replay"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if selected(reservation, &cfg, timeout, cancel).await? {
            return Ok(false);
        }
        ensure!(
            Instant::now() < deadline,
            "Deep Research pill did not appear after selection. The UI may have changed."
        );
        tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Research selection cancelled"),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
    }
}

pub(crate) async fn observe_report(
    page: &Cdp,
    metadata: &Value,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ReportObservation> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Invalid report observation deadline")?;
    let runtime = &metadata["browser"]["runtime"];
    let target = runtime["chromeTargetId"]
        .as_str()
        .context("Research page target is unavailable")?;
    let baseline = &runtime["researchTargetBaseline"];
    ensure!(
        baseline["version"] == 1
            && baseline["pageTargetId"] == target
            && baseline["submissionOwner"].is_object()
            && baseline["submissionOwner"] == metadata["dispatch"]["submissionOwner"],
        "Research pre-submission target baseline is unavailable"
    );
    let ignored = baseline["targetIds"]
        .as_array()
        .context("Research target baseline is malformed")?
        .iter()
        .map(|id| {
            id.as_str()
                .filter(|_| browser::provider_identity(id))
                .map(str::to_owned)
                .context("Research baseline target identity is malformed")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut cfg = cfg.clone();
    cfg["sourceOnly"] = json!(true);
    let expression = crate::browser_runner::result_expression();
    let before = page
        .evaluate_with_arguments(&expression, &json!([cfg]), remaining(deadline)?, cancel)
        .await?;
    browser::require_original_user_source(metadata, &before)?;
    let frame_count = before["scopedResearchIframeCount"]
        .as_u64()
        .context("Research iframe presence is unavailable")?;
    let (_, before_activity) = main_status(&before)?;
    let conversation = before["conversationId"]
        .as_str()
        .context("Research conversation is unavailable")?;
    let user = before["lastUserTurnIndex"]
        .as_u64()
        .context("Research user turn is unavailable")?;
    let scan = scan_reports(
        page,
        target,
        Some((conversation, user)),
        &ignored,
        remaining(deadline)?,
        cancel,
    )
    .await?;
    let after = page
        .evaluate_with_arguments(&expression, &json!([cfg]), remaining(deadline)?, cancel)
        .await?;
    browser::require_original_user_source(metadata, &after)?;
    ensure!(
        source_evidence(&before) == source_evidence(&after),
        "Research original source changed during capture"
    );
    let (main_pending, after_activity) = main_status(&after)?;
    let after_frames = after["scopedResearchIframeCount"]
        .as_u64()
        .context("Research iframe presence is unavailable")?;
    let research_observed = scan.inspected_frames > 0
        || frame_count > 0
        || after_frames > 0
        || before_activity
        || after_activity;
    let Some(report) = scan.report else {
        return Ok(ReportObservation {
            plan: scan.plan,
            report: None,
            pending_research: scan.pending_research
                || scan.inspected_frames == 0 && after_frames > 0
                || main_pending,
            research_observed,
        });
    };
    ensure!(
        !scan.pending_research,
        "Completed and active research reports are ambiguous"
    );
    let mut observed = after;
    let fields = observed
        .as_object_mut()
        .context("Research source observation is malformed")?;
    fields.insert("kind".into(), json!("deep-research-report"));
    fields.insert("text".into(), json!(report.text));
    fields.insert("providerText".into(), json!(report.text));
    fields.insert("html".into(), json!(report.html));
    fields.insert("report".into(), report.evidence);
    fields.insert("lastAssistantMessageId".into(), Value::Null);
    fields.insert("lastAssistantTurnId".into(), Value::Null);
    fields.insert("images".into(), json!([]));
    fields.insert("files".into(), json!([]));
    fields.insert("completionVisible".into(), json!(true));
    require_report(metadata, &observed)?;
    Ok(ReportObservation {
        plan: None,
        report: Some(observed),
        pending_research: false,
        research_observed: true,
    })
}
/// Reference deep-research-timeout message.
pub(crate) fn deadline_message(timeout: Duration, elapsed: Duration) -> String {
    let round = |value: f64| (value + 0.5).floor();
    format!(
        "deep-research-timeout: Deep Research did not complete within {} minutes ({}s elapsed). Use 'alcuin session <id>' to reattach later, or increase --timeout.",
        round(timeout.as_millis() as f64 / 60_000.0),
        round(elapsed.as_millis() as f64 / 1000.0)
    )
}
/// Reference capturePlan: the saved plan record, or None when it has not changed.
pub(crate) fn plan_record(previous: &Value, plan: &Value) -> Option<Value> {
    let phase = if plan["researchStarted"] == true {
        "researching"
    } else {
        "planning"
    };
    let mut next = json!({"title":plan["title"],"steps":plan["steps"],"phase":phase,
        "capturedAt":previous["capturedAt"].as_str().map_or_else(crate::sessions::timestamp, str::to_owned)});
    if plan["actionText"].is_string() {
        next["actionText"] = plan["actionText"].clone();
    }
    let same = ["title", "steps", "phase", "actionText"]
        .iter()
        .all(|key| previous[*key] == next[*key]);
    (!same).then_some(next)
}
/// Reference plan log, written once when the plan first appears.
pub(crate) fn plan_log(plan: &Value) -> String {
    let mut lines = vec![plan["title"].as_str().unwrap_or("").to_owned()];
    for (index, step) in plan["steps"].as_array().into_iter().flatten().enumerate() {
        lines.push(format!("{}. {}", index + 1, step.as_str().unwrap_or("")));
    }
    format!(
        "[browser] Deep Research plan detected:\n{}",
        lines.join("\n")
    )
}
pub(crate) fn source_evidence(observation: &Value) -> Value {
    let mut source = observation.clone();
    if let Some(fields) = source.as_object_mut() {
        for key in [
            "researchStatus",
            "stopExists",
            "strongThinkingActive",
            "scopedResearchIframeCount",
        ] {
            fields.remove(key);
        }
    }
    source
}
fn main_status(observation: &Value) -> Result<(bool, bool)> {
    let status = &observation["researchStatus"];
    ensure!(
        [
            "accountBlocked",
            "researchActivity",
            "hasActiveScopedResearch",
            "incompleteResult"
        ]
        .iter()
        .all(|key| status[*key].is_boolean()),
        "Research main-page status is unavailable"
    );
    ensure!(
        status["accountBlocked"] == false,
        "chatgpt-account-blocked: ChatGPT detected suspicious activity; secure the account in Chrome before retrying"
    );
    let activity = status["researchActivity"] == true || status["hasActiveScopedResearch"] == true;
    Ok((activity || status["incompleteResult"] == true, activity))
}
pub(crate) fn activity_receipt(metadata: &Value) -> Value {
    let runtime = &metadata["browser"]["runtime"];
    let mut receipt = runtime["userCommitReceipt"].clone();
    if let Some(fields) = receipt.as_object_mut() {
        fields.remove("turnsCount");
    }
    json!({"version":1,"pageTargetId":runtime["chromeTargetId"],
        "submissionOwner":metadata["dispatch"]["submissionOwner"],
        "userCommitReceipt":receipt,
        "conversationId":runtime["conversationId"],
        "submittedPromptHash":runtime["submittedPromptHash"],
        "submittedPromptHashFormat":runtime["submittedPromptHashFormat"]})
}
pub(crate) struct ReportObservation {
    /// The request's visible research plan: title, steps, actionText and researchStarted.
    pub plan: Option<Value>,
    pub report: Option<Value>,
    pub pending_research: bool,
    pub research_observed: bool,
}

#[derive(Clone, Debug)]
pub struct ReportRead {
    pub text: String,
    pub html: Option<String>,
    pub evidence: Value,
}
#[derive(Debug)]
pub struct ReportScan {
    pub target_keys: Vec<String>,
    pub plan: Option<Value>,
    pub report: Option<ReportRead>,
    pub pending_research: bool,
    pub inspected_frames: usize,
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Research frame scan deadline reached");
    Ok(remaining)
}
fn attached(event: &Value, parent: &str, sessions: &mut BTreeMap<String, Value>) {
    if event["method"] != "Target.attachedToTarget" || event["sessionId"] != parent {
        return;
    }
    if let Some(session) = event["params"]["sessionId"]
        .as_str()
        .filter(|id| !id.is_empty())
    {
        sessions.insert(session.to_owned(), event["params"]["targetInfo"].clone());
    }
}
fn drain(
    events: &mut tokio::sync::broadcast::Receiver<Value>,
    parent: &str,
    sessions: &mut BTreeMap<String, Value>,
) -> Result<()> {
    loop {
        match events.try_recv() {
            Ok(event) => attached(&event, parent, sessions),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(()),
            Err(_) => anyhow::bail!("Research page attachment inventory is incomplete"),
        }
    }
}
fn iframe_target_keys(sessions: &BTreeMap<String, Value>) -> Result<Vec<String>> {
    ensure!(
        sessions.len() <= 64,
        "Research page has too many attached frames"
    );
    sessions
        .values()
        .filter(|target| target["type"] == "iframe")
        .map(|target| {
            target["targetId"]
                .as_str()
                .filter(|id| browser::provider_identity(&json!(id)))
                .map(str::to_owned)
                .context("Attached research frame identity is missing")
        })
        .collect()
}

/// A missing scope captures only pre-submission iframe IDs and never reads their contents.
/// A report read requires a current conversation and its positively observed user-turn index.
pub async fn scan_reports(
    page: &Cdp,
    page_target: &str,
    scope: Option<(&str, u64)>,
    ignored_targets: &[String],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ReportScan> {
    let parent = page
        .session_id()
        .context("Research requires an attached page session")?;
    ensure!(
        browser::provider_identity(&json!(page_target)),
        "Research page identity is invalid"
    );
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Invalid research scan deadline")?;
    let info = page
        .call(
            "Target.getTargetInfo",
            json!({}),
            remaining(deadline)?,
            cancel,
        )
        .await?;
    require_page(
        &info,
        page_target,
        scope.map(|(conversation, _)| conversation),
    )?;
    let mut events = page.events();
    let mut sessions = BTreeMap::new();
    let mut scanned_sessions = BTreeMap::new();
    let result = async {
        page.call("Target.setAutoAttach", json!({"autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true}), remaining(deadline)?, cancel).await?;
        tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Research frame scan cancelled"),_=tokio::time::sleep(Duration::from_millis(100).min(remaining(deadline)?))=>{}}
        drain(&mut events, parent, &mut sessions)?;
        let keys = iframe_target_keys(&sessions)?;
        scanned_sessions = sessions.clone();
        let mut reports = vec![];
        let mut pending_research = false;
        let mut inspected_frames = 0;
        let mut plan = None;
        for (session_id, target) in &sessions {
            if target["type"] != "iframe" { continue; }
            let target_id = target["targetId"].as_str().filter(|id| browser::provider_identity(&json!(id)))
                .context("Attached research frame identity is missing")?;
            let Some((_, minimum_turn)) = scope else { continue; };
            if ignored_targets.iter().any(|ignored| ignored == target_id) { continue; }
            let frame = page.with_session(session_id.clone());
            let tree = match frame.call("Page.getFrameTree", json!({}), remaining(deadline)?, cancel).await {
                Ok(tree) => tree,
                Err(_) if !target["url"].as_str().is_some_and(report_url) => continue,
                Err(error) => return Err(error),
            };
            let frames = research_frames(&tree["frameTree"])?;
            if frames.is_empty() { continue; }
            let outer_id = tree["frameTree"]["frame"]["id"].as_str().context("Research frame tree identity is missing")?;
            let Some(owner) = owner_turn(page, outer_id, deadline, cancel).await? else { continue; };
            if owner < minimum_turn { continue; }
            for descriptor in frames {
                let frame_id = descriptor["id"].as_str().context("Research report frame ID is missing")?;
                let url = descriptor["url"].as_str().context("Research report URL is missing")?;
                let read = read_frame(&frame, frame_id, url, deadline, cancel).await?;
                inspected_frames += 1;
                let Some(text) = absorb(&read, &mut plan, &mut pending_research)? else { continue; };
                ensure!(owner_turn(page, outer_id, deadline, cancel).await? == Some(owner), "Research report changed its owning provider turn during capture");
                reports.push(ReportRead { text:text.to_owned(),html:read["html"].as_str().map(str::to_owned),evidence:json!({"version":1,"scope":"page-frame","pageTargetId":page_target,"targetId":target_id,"frameId":frame_id,"url":url,"ownerTurnIndex":owner,"completed":true,"researchStarted":false,"textSha256":sha256(text.as_bytes())}) });
            }
        }
        // Reference legacy inline rendering: same-process report frames in the page's own tree.
        // A completed attached-frame read is preferred, as in the reference.
        if let Some((_, minimum_turn)) = scope {
            let attached = sessions.values().filter_map(|target| target["targetId"].as_str()).collect::<std::collections::BTreeSet<_>>();
            let tree = page.call("Page.getFrameTree", json!({}), remaining(deadline)?, cancel).await?;
            let mut inline = vec![];
            for descriptor in research_frames(&tree["frameTree"])? {
                let frame_id = descriptor["id"].as_str().context("Research report frame ID is missing")?;
                let url = descriptor["url"].as_str().context("Research report URL is missing")?;
                if attached.contains(frame_id) { continue; }
                let Some(owner) = owner_turn(page, frame_id, deadline, cancel).await? else { continue; };
                if owner < minimum_turn { continue; }
                let read = read_frame(page, frame_id, url, deadline, cancel).await?;
                inspected_frames += 1;
                let Some(text) = absorb(&read, &mut plan, &mut pending_research)? else { continue; };
                ensure!(owner_turn(page, frame_id, deadline, cancel).await? == Some(owner), "Research report changed its owning provider turn during capture");
                inline.push(ReportRead { text:text.to_owned(),html:read["html"].as_str().map(str::to_owned),evidence:json!({"version":1,"scope":"page-frame","pageTargetId":page_target,"targetId":page_target,"frameId":frame_id,"url":url,"ownerTurnIndex":owner,"completed":true,"researchStarted":false,"textSha256":sha256(text.as_bytes())}) });
            }
            if reports.is_empty() { reports = inline; }
        }
        ensure!(reports.len() <= 1, "Multiple completed research reports belong to this request; refusing ambiguity");
        let info = page.call("Target.getTargetInfo", json!({}), remaining(deadline)?, cancel).await?;
        require_page(&info, page_target, scope.map(|(conversation, _)| conversation))?;
        Ok::<_, anyhow::Error>(ReportScan { target_keys:keys, plan, report:reports.pop(), pending_research, inspected_frames })
    }.await;
    // Even a lost enable acknowledgement may have attached children. Cleanup owns only this page's events.
    let late = drain(&mut events, parent, &mut sessions);
    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    let cleanup = async {
        page.call(
            "Target.setAutoAttach",
            json!({"autoAttach":false,"waitForDebuggerOnStart":false,"flatten":true}),
            remaining(cleanup_deadline)?,
            &CancellationToken::new(),
        )
        .await?;
        drain(&mut events, parent, &mut sessions)?;
        let root = page.browser_scope();
        for session_id in sessions.keys() {
            // Disabling auto-attach may already have detached this child; no target is closed.
            let _ = root
                .call(
                    "Target.detachFromTarget",
                    json!({"sessionId":session_id}),
                    remaining(cleanup_deadline)?,
                    &CancellationToken::new(),
                )
                .await;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let mut value = result?;
    late?;
    cleanup.context("Research attachment cleanup was not confirmed")?;
    ensure!(!cancel.is_cancelled(), "Research frame scan cancelled");
    let final_keys = iframe_target_keys(&sessions)?;
    if scope.is_some() {
        ensure!(
            sessions == scanned_sessions,
            "Research page attachment inventory changed during capture"
        );
    } else {
        value.target_keys = final_keys;
    }
    Ok(value)
}
/// One report frame's status, read in an isolated world bound to its exact URL.
async fn read_frame(
    frame: &Cdp,
    frame_id: &str,
    url: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Value> {
    let context = frame
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId":frame_id,"worldName":"alcuin-research-report"}),
            remaining(deadline)?,
            cancel,
        )
        .await?;
    let context_id = context["executionContextId"]
        .as_u64()
        .context("Research isolated context is unavailable")?;
    let expression = format!(
        "(()=>{{if(location.href!=={})throw Error('Research frame navigation changed');return ({});}})()",
        serde_json::to_string(url)?,
        FRAME_STATUS
    );
    let mut value = frame
        .call(
            "Runtime.evaluate",
            json!({"expression":expression,"contextId":context_id,"returnByValue":true}),
            remaining(deadline)?,
            cancel,
        )
        .await?;
    ensure!(
        value.get("exceptionDetails").is_none(),
        "Research frame evaluation failed"
    );
    let read = value["result"]["value"].take();
    ensure!(
        read.is_object() && read["completed"].is_boolean() && read["researchStarted"].is_boolean(),
        "Research frame completion status is malformed"
    );
    Ok(read)
}
/// Records the plan and pending state; returns the completed report text.
fn absorb<'a>(
    read: &'a Value,
    plan: &mut Option<Value>,
    pending_research: &mut bool,
) -> Result<Option<&'a str>> {
    let text_length = read["textLength"]
        .as_u64()
        .context("Research frame text length is malformed")?;
    // Reference capturePlan needs a title and at least one step.
    if plan.is_none()
        && read["planTitle"]
            .as_str()
            .is_some_and(|title| !title.is_empty())
        && read["planSteps"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty() && steps.iter().all(Value::is_string))
    {
        *plan = Some(json!({"title":read["planTitle"],"steps":read["planSteps"],
            "actionText":read["planActionText"].as_str().filter(|text| !text.is_empty()),
            "researchStarted":read["researchStarted"]}));
    }
    *pending_research |= read["researchStarted"] == true
        || read["completed"] != true
            && (text_length == 0
                || read["planSteps"]
                    .as_array()
                    .is_some_and(|steps| !steps.is_empty()));
    Ok(read["text"].as_str().filter(|text| {
        read["completed"] == true
            && read["researchStarted"] == false
            && text.encode_utf16().count() >= 40
            && !incomplete_text(text)
    }))
}
fn require_page(info: &Value, target: &str, conversation: Option<&str>) -> Result<()> {
    let info = &info["targetInfo"];
    let url = info["url"]
        .as_str()
        .context("Research parent page URL is unavailable")?;
    ensure!(
        info["type"] == "page"
            && info["targetId"] == target
            && browser::provider_url(url).is_some_and(|url| url.host_str() == Some("chatgpt.com"))
            && conversation
                .is_none_or(|expected| browser::conversation_id(url).as_deref() == Some(expected)),
        "Research parent page or conversation changed"
    );
    Ok(())
}
fn research_frames(tree: &Value) -> Result<Vec<&Value>> {
    let mut pending = vec![tree];
    let mut frames = vec![];
    let mut visited = 0;
    while let Some(tree) = pending.pop() {
        visited += 1;
        ensure!(
            visited <= 64 && pending.len() <= 64,
            "Research frame tree is too large"
        );
        if tree["frame"]["url"].as_str().is_some_and(report_url) {
            frames.push(&tree["frame"]);
        }
        if let Some(children) = tree["childFrames"].as_array() {
            ensure!(
                visited + pending.len() + children.len() <= 64,
                "Research frame tree is too large"
            );
            pending.extend(children);
        }
    }
    Ok(frames)
}
async fn owner_turn(
    page: &Cdp,
    frame: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Option<u64>> {
    let owner = page
        .call(
            "DOM.getFrameOwner",
            json!({"frameId":frame}),
            remaining(deadline)?,
            cancel,
        )
        .await?;
    let Some(node) = owner["backendNodeId"].as_u64() else {
        return Ok(None);
    };
    let object = page
        .call(
            "DOM.resolveNode",
            json!({"backendNodeId":node}),
            remaining(deadline)?,
            cancel,
        )
        .await?;
    let Some(object) = object["object"]["objectId"].as_str() else {
        return Ok(None);
    };
    let function = format!(
        "function(){{ {} const turns=readTurns();const index=turns.findIndex(turn=>turn===this||turn.contains?.(this));return index<0?null:index;}}",
        include_str!("../browser-scripts/result-prelude.js")
    );
    let result = page
        .call(
            "Runtime.callFunctionOn",
            json!({"objectId":object,"functionDeclaration":function,"returnByValue":true}),
            remaining(deadline)?,
            cancel,
        )
        .await;
    let release = page
        .call(
            "Runtime.releaseObject",
            json!({"objectId":object}),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
    let result = result?;
    release?;
    ensure!(
        result.get("exceptionDetails").is_none(),
        "Research frame owner evaluation failed"
    );
    Ok(result["result"]["value"].as_u64())
}

pub(crate) fn incomplete_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    let normalised = lower
        .split(policy::javascript_whitespace)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let lines: Vec<_> = text
        .split('\n')
        .map(policy::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let planning_panel = text.encode_utf16().count() <= 1500
        && (4..=20).contains(&lines.len())
        && lines[1].eq_ignore_ascii_case("Update")
        && lines[lines.len() - 1].eq_ignore_ascii_case("Stop research")
        && lines[lines.len() - 2]
            .strip_suffix("...")
            .unwrap_or(lines[lines.len() - 2])
            .eq_ignore_ascii_case("Determining steps for creating a report");
    let plan_reminder = text
        .trim_start_matches(policy::javascript_whitespace)
        .starts_with("<system-reminder>")
        && lower.match_indices('#').any(|(offset, _)| {
            let tail = lower[offset + 1..].trim_start_matches(policy::javascript_whitespace);
            tail.strip_prefix("plan mode").is_some_and(|rest| {
                rest.chars()
                    .next()
                    .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
            })
        });
    matches!(
        normalised.as_str(),
        "called tool"
            | "used tool"
            | "użyto narzędzia"
            | "narzędzie wywołane"
            | "planning"
            | "researching"
            | "searching the web"
    ) || plan_reminder
        || planning_panel
}

pub(crate) fn report_url(raw: &str) -> bool {
    reqwest::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.host_str().is_some_and(|host| {
                host == "chatgpt.com" && url.path().contains("deep-research")
                    || host
                        .strip_prefix("connector_openai_deep_research")
                        .is_some_and(|rest| rest.starts_with(['.', '-']))
                        && host.ends_with(".oaiusercontent.com")
            })
    })
}

pub(crate) fn require_report(metadata: &Value, observed: &Value) -> Result<()> {
    let runtime = &metadata["browser"]["runtime"];
    let report = &observed["report"];
    let text = observed["providerText"]
        .as_str()
        .or_else(|| observed["text"].as_str())
        .context("Research report text is missing")?;
    ensure!(
        metadata["browser"]["config"]["researchMode"] == "deep"
            && observed["kind"] == "deep-research-report"
            && runtime["submittedPromptHashFormat"] == "original-source-v1",
        "Research report requires its original committed research request"
    );
    browser::require_original_user_source(metadata, observed)?;
    ensure!(
        report["version"] == 1
            && report["scope"] == "page-frame"
            && browser::provider_identity(&report["pageTargetId"])
            && report["pageTargetId"] == runtime["chromeTargetId"]
            && report["pageTargetId"] == runtime["ownedRecoveryTarget"]["targetId"]
            && browser::provider_identity(&report["targetId"])
            && browser::provider_identity(&report["frameId"])
            && report["url"].as_str().is_some_and(report_url),
        "Research report frame is not bound to the owned provider page"
    );
    let user = observed["lastUserTurnIndex"]
        .as_u64()
        .context("Research report user turn is missing")?;
    ensure!(
        report["ownerTurnIndex"]
            .as_u64()
            .is_some_and(|owner| owner >= user)
            && runtime["userCommitReceipt"]["messageId"] == observed["lastUserMessageId"]
            && runtime["userCommitReceipt"]["messageFingerprint"] == runtime["submittedPromptHash"],
        "Research report is not owned by the committed user turn"
    );
    ensure!(
        report["completed"] == true
            && report["researchStarted"] == false
            && text.encode_utf16().count() >= 40
            && !incomplete_text(text)
            && observed["text"].as_str() == Some(text)
            && report["textSha256"] == sha256(text.as_bytes())
            && observed["lastAssistantMessageId"].is_null()
            && observed["lastAssistantTurnId"].is_null(),
        "Research report completion or content evidence is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    async fn scan_fixture(kind: &str) -> (Result<ReportScan>, Vec<Value>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::{
            WebSocketStream,
            tungstenite::{Message, protocol::Role},
        };
        let (client, server) = tokio::io::duplex(64 * 1024);
        let root = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(2),
        );
        let page = root.with_session("PUBLIC-PAGE-SESSION".into());
        let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let fault = kind.to_owned();
        let cancel = CancellationToken::new();
        let fixture_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut trace = vec![];
            let mut owner_reads = 0;
            let mut page_reads = 0;
            while let Some(Ok(Message::Text(frame))) = peer.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let method = request["method"].as_str().unwrap();
                trace.push(request.clone());
                let child = request["sessionId"] == "PUBLIC-CHILD-SESSION";
                if method != "Target.detachFromTarget" {
                    let frame_method = matches!(
                        method,
                        "Page.getFrameTree" | "Page.createIsolatedWorld" | "Runtime.evaluate"
                    );
                    assert!(
                        child && frame_method
                            || request["sessionId"] == "PUBLIC-PAGE-SESSION"
                                && (!frame_method
                                    || method == "Page.getFrameTree"
                                    || fault.starts_with("inline")),
                        "{request}"
                    );
                }
                if method == "Target.getTargetInfo" {
                    page_reads += 1;
                }
                if (fault == "late-report" && method == "Runtime.evaluate")
                    || (fault == "late-final"
                        && method == "Target.getTargetInfo"
                        && page_reads == 2)
                    || (fault == "baseline-late"
                        && method == "Target.setAutoAttach"
                        && request["params"]["autoAttach"] == false)
                {
                    peer.send(Message::Text(json!({"method":"Target.attachedToTarget","sessionId":"PUBLIC-PAGE-SESSION","params":{"sessionId":"PUBLIC-LATE-SESSION","targetInfo":{"type":"iframe","targetId":"PUBLIC-late-report","url":"https://connector_openai_deep_research-public.oaiusercontent.com/report"}}}).to_string().into())).await.unwrap();
                }
                let result = match method {
                    "Target.getTargetInfo" => {
                        json!({"targetInfo":{"type":"page","targetId":"PUBLIC-page","url":"https://chatgpt.com/c/public-conversation"}})
                    }
                    "Target.setAutoAttach" => {
                        assert_eq!(request["params"]["waitForDebuggerOnStart"], false);
                        assert_eq!(request["params"]["flatten"], true);
                        if fault == "cancel-cleanup" && request["params"]["autoAttach"] == false {
                            fixture_cancel.cancel();
                        }
                        if request["params"]["autoAttach"] == true {
                            for (parent, session, target) in [
                                (
                                    "FOREIGN-PAGE-SESSION",
                                    "FOREIGN-CHILD-SESSION",
                                    "FOREIGN-report",
                                ),
                                (
                                    "PUBLIC-PAGE-SESSION",
                                    "PUBLIC-CHILD-SESSION",
                                    "PUBLIC-report",
                                ),
                            ] {
                                peer.send(Message::Text(json!({"method":"Target.attachedToTarget","sessionId":parent,"params":{"sessionId":session,"targetInfo":{"type":"iframe","targetId":target,"url":"https://connector_openai_deep_research-public.oaiusercontent.com/report"}}}).to_string().into())).await.unwrap();
                            }
                            if fault == "enable-error" {
                                peer.send(Message::Text(json!({"id":request["id"],"error":{"message":"PUBLIC baseline unavailable"}}).to_string().into())).await.unwrap();
                                continue;
                            }
                        }
                        json!({})
                    }
                    "Page.getFrameTree" if !child => {
                        let report = "https://connector_openai_deep_research-public.oaiusercontent.com/report";
                        // An attached frame can also appear in the page tree; it is read only once.
                        let children = if fault.starts_with("inline") {
                            json!([{"frame":{"id":"PUBLIC-report","url":report}},{"frame":{"id":"PUBLIC-inline-frame","url":report}}])
                        } else {
                            json!([])
                        };
                        json!({"frameTree":{"frame":{"id":"PUBLIC-page-frame","url":"https://chatgpt.com/c/public-conversation"},"childFrames":children}})
                    }
                    "Page.getFrameTree" => {
                        json!({"frameTree":{"frame":{"id":"PUBLIC-frame","url":"https://connector_openai_deep_research-public.oaiusercontent.com/report"}}})
                    }
                    "DOM.getFrameOwner" => {
                        assert!(matches!(
                            request["params"]["frameId"].as_str(),
                            Some("PUBLIC-frame" | "PUBLIC-inline-frame")
                        ));
                        json!({"backendNodeId":17})
                    }
                    "DOM.resolveNode" => json!({"object":{"objectId":"PUBLIC-owner-object"}}),
                    "Runtime.callFunctionOn" => {
                        owner_reads += 1;
                        json!({"result":{"value":if fault == "old-owner" || fault == "moved-owner" && owner_reads > 1 {1}else{2}}})
                    }
                    "Runtime.releaseObject" => json!({}),
                    "Page.createIsolatedWorld" => json!({"executionContextId":7}),
                    "Runtime.evaluate" => {
                        assert_eq!(request["params"]["contextId"], 7);
                        assert!(
                            request["params"]["expression"]
                                .as_str()
                                .unwrap()
                                .contains("location.href!==")
                        );
                        let completed = !matches!(
                            fault.as_str(),
                            "awaiting-plan" | "blank-loading" | "tool-stub"
                        ) && !(fault == "inline" && child);
                        json!({"result":{"value":{"completed":completed,"researchStarted":if fault == "missing-status" {Value::Null}else{json!(fault == "active-plan")},"textLength":match fault.as_str() {"blank-loading"=>0,"tool-stub"=>11,_=>80},"planSteps":if matches!(fault.as_str(),"awaiting-plan"|"active-plan") {json!(["PUBLIC next research step","PUBLIC second step"])}else{Value::Null},"planTitle":if matches!(fault.as_str(),"awaiting-plan"|"active-plan") {json!("PUBLIC plan title")}else{Value::Null},"planActionText":if fault == "awaiting-plan" {json!("Edit")}else{Value::Null},"text":completed.then_some("PUBLIC completed report with more than forty characters of exact report content."),"html":completed.then_some("<p>PUBLIC completed report</p>")}}})
                    }
                    "Target.detachFromTarget" => {
                        assert!(request.get("sessionId").is_none());
                        assert!(matches!(
                            request["params"]["sessionId"].as_str(),
                            Some("PUBLIC-CHILD-SESSION" | "PUBLIC-LATE-SESSION")
                        ));
                        json!({})
                    }
                    other => panic!("Unexpected research operation {other}"),
                };
                if peer
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            trace
        });
        let scope = (!kind.starts_with("baseline")).then_some(("public-conversation", 2));
        let result = scan_reports(
            &page,
            "PUBLIC-page",
            scope,
            &[],
            Duration::from_secs(2),
            &cancel,
        )
        .await;
        root.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        (result, task.await.unwrap())
    }
    #[tokio::test]
    async fn report_collection_reads_only_owned_page_frames_and_rechecks_owner_turn() {
        let (read, trace) = scan_fixture("read").await;
        let read = read.unwrap();
        assert_eq!(read.target_keys, ["PUBLIC-report"]);
        let report = read.report.unwrap();
        assert_eq!(report.evidence["pageTargetId"], "PUBLIC-page");
        assert_eq!(report.evidence["targetId"], "PUBLIC-report");
        assert_eq!(report.evidence["frameId"], "PUBLIC-frame");
        assert_eq!(report.evidence["ownerTurnIndex"], 2);
        assert_eq!(
            report.evidence["textSha256"],
            sha256(report.text.as_bytes())
        );
        assert_eq!(
            trace
                .iter()
                .filter(|request| request["method"] == "Runtime.callFunctionOn")
                .count(),
            2
        );
        let (changed, _) = scan_fixture("moved-owner").await;
        assert!(changed.is_err());
    }
    #[tokio::test]
    async fn baseline_never_reads_report_content_and_failed_attachment_is_not_an_empty_baseline() {
        let (baseline, trace) = scan_fixture("baseline").await;
        let baseline = baseline.unwrap();
        assert_eq!(baseline.target_keys, ["PUBLIC-report"]);
        assert!(baseline.report.is_none());
        assert!(trace.iter().all(|request| !matches!(
            request["method"].as_str(),
            Some("Page.getFrameTree" | "Runtime.evaluate" | "DOM.getFrameOwner")
        )));
        let (failed, trace) = scan_fixture("enable-error").await;
        assert!(failed.is_err());
        assert!(
            trace
                .iter()
                .any(|request| request["method"] == "Target.setAutoAttach"
                    && request["params"]["autoAttach"] == false)
        );
    }
    #[tokio::test]
    async fn legacy_inline_report_frames_are_read_when_no_attached_frame_completes() {
        let (read, trace) = scan_fixture("inline").await;
        let report = read.unwrap().report.unwrap();
        assert_eq!(report.evidence["pageTargetId"], "PUBLIC-page");
        assert_eq!(report.evidence["targetId"], "PUBLIC-page");
        assert_eq!(report.evidence["frameId"], "PUBLIC-inline-frame");
        assert_eq!(report.evidence["ownerTurnIndex"], 2);
        assert!(
            trace
                .iter()
                .any(|request| request["method"] == "Page.createIsolatedWorld"
                    && request["sessionId"] == "PUBLIC-PAGE-SESSION"
                    && request["params"]["frameId"] == "PUBLIC-inline-frame")
        );
        assert!(
            trace
                .iter()
                .all(|request| request["params"]["frameId"] != "PUBLIC-report")
        );
        // A completed attached frame wins over the inline read.
        let (both, _) = scan_fixture("inline-both").await;
        assert_eq!(
            both.unwrap().report.unwrap().evidence["targetId"],
            "PUBLIC-report"
        );
    }
    #[tokio::test]
    async fn a_report_owned_by_an_older_turn_is_not_read() {
        let (read, trace) = scan_fixture("old-owner").await;
        assert!(read.unwrap().report.is_none());
        assert!(
            trace
                .iter()
                .all(|request| request["method"] != "Runtime.evaluate")
        );
    }
    #[tokio::test]
    async fn late_owned_frames_refuse_report_ambiguity_and_are_included_in_baselines() {
        for kind in ["late-report", "late-final", "baseline-late"] {
            let (read, trace) = scan_fixture(kind).await;
            if kind == "baseline-late" {
                let read = read.unwrap();
                assert_eq!(read.target_keys, ["PUBLIC-report", "PUBLIC-late-report"]);
                assert!(read.report.is_none());
            } else {
                assert!(read.unwrap_err().to_string().contains("inventory changed"));
            }
            let detached: Vec<_> = trace
                .iter()
                .filter(|request| request["method"] == "Target.detachFromTarget")
                .map(|request| request["params"]["sessionId"].as_str().unwrap())
                .collect();
            assert_eq!(detached, ["PUBLIC-CHILD-SESSION", "PUBLIC-LATE-SESSION"]);
        }
    }
    #[test]
    fn research_deadlines_name_the_reference_code_and_reattach_hint() {
        let failure = anyhow::Error::from(crate::browser_provider::Failure::AnswerDeadline)
            .context(deadline_message(
                Duration::from_secs(40 * 60),
                Duration::from_millis(2_400_500),
            ));
        assert_eq!(
            failure.to_string(),
            "deep-research-timeout: Deep Research did not complete within 40 minutes (2401s elapsed). Use 'alcuin session <id>' to reattach later, or increase --timeout."
        );
        assert!(matches!(
            failure.downcast_ref::<crate::browser_provider::Failure>(),
            Some(crate::browser_provider::Failure::AnswerDeadline)
        ));
    }
    #[tokio::test]
    async fn visible_research_plans_are_captured_logged_and_advance_to_researching() {
        let planning = scan_fixture("awaiting-plan").await.0.unwrap().plan.unwrap();
        let record = plan_record(&Value::Null, &planning).unwrap();
        assert_eq!(record["title"], "PUBLIC plan title");
        assert_eq!(
            record["steps"],
            json!(["PUBLIC next research step", "PUBLIC second step"])
        );
        assert_eq!(
            (record["phase"].as_str(), record["actionText"].as_str()),
            (Some("planning"), Some("Edit"))
        );
        assert_eq!(
            plan_log(&record),
            "[browser] Deep Research plan detected:\nPUBLIC plan title\n1. PUBLIC next research step\n2. PUBLIC second step"
        );
        assert!(plan_record(&record, &planning).is_none());
        let researching = scan_fixture("active-plan").await.0.unwrap().plan.unwrap();
        let advanced = plan_record(&record, &researching).unwrap();
        assert_eq!(advanced["phase"], "researching");
        assert_eq!(advanced["capturedAt"], record["capturedAt"]);
        assert!(advanced.get("actionText").is_none());
        assert!(
            scan_fixture("blank-loading")
                .await
                .0
                .unwrap()
                .plan
                .is_none()
        );
    }
    #[tokio::test]
    async fn active_or_missing_research_status_never_returns_a_completed_report() {
        for kind in ["active-plan", "awaiting-plan", "blank-loading"] {
            let scan = scan_fixture(kind).await.0.unwrap();
            assert!(scan.report.is_none());
            assert!(scan.pending_research);
        }
        let stub = scan_fixture("tool-stub").await.0.unwrap();
        assert!(stub.report.is_none() && !stub.pending_research);
        assert!(
            scan_fixture("missing-status")
                .await
                .0
                .unwrap_err()
                .to_string()
                .contains("malformed")
        );
    }
    #[tokio::test]
    async fn cancellation_during_cleanup_never_returns_a_successful_report() {
        let (read, trace) = scan_fixture("cancel-cleanup").await;
        assert!(read.unwrap_err().to_string().contains("cancelled"));
        assert!(
            trace
                .iter()
                .any(|request| request["method"] == "Target.detachFromTarget")
        );
    }
    #[test]
    fn incomplete_reports_reject_tool_placeholders_and_only_narrow_planning_panels() {
        for text in [
            "Called tool",
            " USED\nTOOL ",
            "UŻYTO NARZĘDZIA",
            "narzędzie wywołane",
            "Planning",
            "Researching",
            "Searching the web",
            "<system-reminder>\n# Plan mode\nPUBLIC instructions",
            "PUBLIC report\nUpdate\nDetermining steps for creating a report...\nStop research",
        ] {
            assert!(incomplete_text(text), "{text}");
        }
        for text in [
            "PUBLIC called tool analysis",
            "PUBLIC completed report\nUpdate\nDetailed findings\nStop research",
            "<system-reminder>\n# Plan models",
            "PUBLIC report\nUpdate\nDetermining steps for creating a report......\nStop research",
        ] {
            assert!(!incomplete_text(text), "{text}");
        }
    }
    #[test]
    fn report_frames_require_exact_provider_domains_and_https_without_credentials() {
        for url in [
            "https://chatgpt.com/deep-research/PUBLIC",
            "https://connector_openai_deep_research-public.oaiusercontent.com/report",
        ] {
            assert!(report_url(url));
        }
        for url in [
            "http://chatgpt.com/",
            "https://chatgpt.com/c/public-unrelated",
            "https://chatgpt.com:9333/",
            "https://PUBLIC@chatgpt.com/",
            "https://example.invalid/deep-research",
            "https://connector_openai_deep_research-public.oaiusercontent.com.example.invalid/report",
        ] {
            assert!(!report_url(url));
        }
    }
    #[test]
    fn null_assistant_ids_require_separate_report_and_original_source_evidence() {
        let source = "PUBLIC complete research request";
        let hash = browser::prompt_fingerprint(source, "PUBLIC-user");
        let text = "PUBLIC completed report with more than forty characters of independently scoped research content.";
        let metadata = json!({"browser":{"config":{"researchMode":"deep"},"runtime":{"chromeTargetId":"PUBLIC-page","conversationId":"public-conversation","tabUrl":"https://chatgpt.com/c/public-conversation","submittedPromptHash":hash,"submittedPromptHashFormat":"original-source-v1","ownedRecoveryTarget":{"targetId":"PUBLIC-page","conversationId":"public-conversation"},"userCommitReceipt":{"messageId":"PUBLIC-user","messageFingerprint":hash}}}});
        let observed = json!({"kind":"deep-research-report","currentUrl":"https://chatgpt.com/c/public-conversation","conversationId":"public-conversation","lastUserMessageId":"PUBLIC-user","lastUserOriginalSourceMessageId":"PUBLIC-user","lastUserOriginalSourceText":source,"lastUserTurnIndex":2,"text":text,"report":{"version":1,"scope":"page-frame","pageTargetId":"PUBLIC-page","targetId":"PUBLIC-report","frameId":"PUBLIC-frame","url":"https://connector_openai_deep_research-public.oaiusercontent.com/report","ownerTurnIndex":2,"completed":true,"researchStarted":false,"textSha256":sha256(text.as_bytes())}});
        require_report(&metadata, &observed).unwrap();
        assert!(browser::require_original_source_pair(&metadata, &observed).is_err());
        for case in [
            "source",
            "page",
            "frame",
            "owner",
            "url",
            "content",
            "completed",
            "active-plan",
            "missing-status",
            "assistant",
        ] {
            let mut changed = observed.clone();
            match case {
                "source" => changed["lastUserOriginalSourceText"] = json!("PUBLIC changed source"),
                "page" => changed["report"]["pageTargetId"] = json!("PUBLIC-foreign"),
                "frame" => changed["report"]["frameId"] = json!(""),
                "owner" => changed["report"]["ownerTurnIndex"] = json!(1),
                "url" => changed["report"]["url"] = json!("https://example.invalid/"),
                "content" => {
                    changed["text"] = json!(
                        "PUBLIC changed completed report with enough content to exceed the threshold."
                    )
                }
                "completed" => changed["report"]["completed"] = json!(false),
                "active-plan" => changed["report"]["researchStarted"] = json!(true),
                "missing-status" => changed["report"]
                    .as_object_mut()
                    .unwrap()
                    .remove("researchStarted")
                    .map(|_| ())
                    .unwrap(),
                _ => changed["lastAssistantMessageId"] = json!("PUBLIC-invented"),
            }
            assert!(require_report(&metadata, &changed).is_err(), "{case}");
        }
    }
    #[test]
    fn research_activity_is_bound_to_the_exact_saved_submission() {
        let metadata = json!({"dispatch":{"submissionOwner":{"reservationId":"PUBLIC-reservation"}},
            "browser":{"runtime":{"chromeTargetId":"PUBLIC-page","userCommitReceipt":{"messageId":"PUBLIC-user"},
            "conversationId":"PUBLIC-conversation","submittedPromptHash":"PUBLIC-source","submittedPromptHashFormat":"original-source-v1"}}});
        let receipt = activity_receipt(&metadata);
        for field in [
            "chromeTargetId",
            "userCommitReceipt",
            "conversationId",
            "submittedPromptHash",
            "submittedPromptHashFormat",
        ] {
            let mut changed = metadata.clone();
            changed["browser"]["runtime"][field] = json!("PUBLIC-replaced");
            assert_ne!(activity_receipt(&changed), receipt, "{field}");
        }
        let mut changed = metadata.clone();
        changed["dispatch"]["submissionOwner"] = json!({"reservationId":"PUBLIC-next"});
        assert_ne!(activity_receipt(&changed), receipt);
        let mut later = metadata.clone();
        later["browser"]["runtime"]["userCommitReceipt"]["turnsCount"] = json!(9);
        assert_eq!(
            activity_receipt(&later),
            receipt,
            "Assistant growth does not change the committed user identity"
        );
    }
    #[test]
    fn research_progress_can_change_without_changing_original_source_evidence() {
        let before = json!({"conversationId":"PUBLIC-conversation","lastUserMessageId":"PUBLIC-user",
            "lastUserOriginalSourceText":"PUBLIC full source","researchStatus":{"textLength":7,"researchActivity":true},
            "stopExists":true,"strongThinkingActive":true,"scopedResearchIframeCount":0});
        let mut after = before.clone();
        after["researchStatus"] =
            json!({"textLength":900,"researchActivity":false,"finished":true});
        after["stopExists"] = json!(false);
        after["strongThinkingActive"] = json!(false);
        after["scopedResearchIframeCount"] = json!(1);
        assert_eq!(source_evidence(&before), source_evidence(&after));
        after["lastUserOriginalSourceText"] = json!("PUBLIC truncated");
        assert_ne!(source_evidence(&before), source_evidence(&after));
    }
    #[test]
    fn answer_budget_preserves_deep_defaults_and_explicit_saved_values() {
        for (config, milliseconds) in [
            (json!({}), 1_200_000),
            (json!({"researchMode":"deep"}), 2_400_000),
            (json!({"researchMode":"deep","timeoutMs":17}), 17),
            (json!({"researchMode":"off","timeoutMs":0}), 0),
        ] {
            assert_eq!(
                crate::browser_runner::answer_timeout(&config),
                Duration::from_millis(milliseconds)
            );
        }
    }
}

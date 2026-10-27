//! Owned Web Search selection happens after staging and before any prompt dispatch.
use crate::browser_renderer::RendererReservation;
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) const NAVIGATION: &str = include_str!("../browser-scripts/composer-navigation-check.js");
pub(crate) const PROBE: &str = include_str!("../browser-scripts/composer-plus-probe.js");
const PREPARE: &str = include_str!("../browser-scripts/composer-plus-prepare.js");
const CLEAN: &str = include_str!("../browser-scripts/composer-plus-clean.js");
const SELECT: &str = include_str!("../browser-scripts/web-search-select.js");
const VERIFY: &str = include_str!("../browser-scripts/web-search-verify.js");

pub(crate) fn expression(body: &str, require_active: bool) -> String {
    expression_in(
        include_str!("../browser-scripts/web-search-context.js"),
        "Web Search",
        body,
        require_active,
    )
}
/// `context` defines searchVisible, searchContext, searchMenuRoots and searchBinding for the
/// composer-plus scripts; `label` names the owner in the ownership refusal.
pub(crate) fn expression_in(
    context: &str,
    label: &str,
    body: &str,
    require_active: bool,
) -> String {
    let entry = if require_active {
        "assertOracleSubmission();"
    } else {
        ""
    };
    format!(
        r#"(cfg)=>{{ /* alcuin-dom:web-search */ {} {}
        const assertOracleSubmission=()=>{{if(!oracleTemporaryChat())throw Error('Verified Temporary Chat required');if(window!==window.top)throw Error('Top renderer required');
            const claim=readOracleTargetClaim();if(!matchesOracleSubmission(claim,cfg.owner)||claim.submission.state!=='active'||claim.submission.dispatch!=='not-started'||claim.inspection)throw Error('{label} ownership changed');}};
        const oracleSubmissionIsActive=()=>{{try{{assertOracleSubmission();return true;}}catch{{return false;}}}};
        {entry} return ({body});}}"#,
        include_str!("../browser-scripts/ownership-prelude.js"),
        context
    )
}

fn require_navigation(value: &Value) -> Result<()> {
    ensure!(
        value["contextMatches"] == true
            && value["workSelected"] == false
            && value["modeUnverified"] == false,
        "Web Search composer navigation or mode changed; nothing was sent"
    );
    Ok(())
}

pub(crate) async fn evaluate(
    reservation: &mut RendererReservation,
    body: &str,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    evaluate_in(&WEB_SEARCH, reservation, body, cfg, timeout, cancel).await
}
pub(crate) async fn evaluate_in(
    plus: &PlusMenu,
    reservation: &mut RendererReservation,
    body: &str,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    reservation
        .evaluate_mutation(
            &expression_in(plus.context, plus.label, body, true),
            cfg.clone(),
            timeout,
            cancel,
        )
        .await
}

async fn verify(
    reservation: &mut RendererReservation,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_navigation(reservation, cfg, timeout, cancel).await?;
    let value = evaluate(reservation, VERIFY, cfg, timeout, cancel).await?;
    ensure!(
        value["promptMatches"] == true,
        "Web Search changed the staged prompt; nothing was sent"
    );
    Ok(value["selected"] == true)
}

async fn pause(cancel: &CancellationToken) -> Result<()> {
    tokio::select! {biased;_=cancel.cancelled()=>bail!("Web Search selection cancelled"),_=tokio::time::sleep(Duration::from_millis(100))=>Ok(())}
}
pub(crate) async fn check_navigation(
    reservation: &mut RendererReservation,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    require_navigation(&evaluate(reservation, NAVIGATION, cfg, timeout, cancel).await?)
}
/// `unavailable` is the reference refusal when the composer tools control is missing.
pub(crate) async fn open_tools(
    reservation: &mut RendererReservation,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
    unavailable: &str,
) -> Result<()> {
    let probe = evaluate(reservation, PROBE, cfg, timeout, cancel).await?;
    ensure!(
        probe["status"] == "focused" && probe["focused"] == true,
        "{unavailable}"
    );
    press_plus(&WEB_SEARCH, reservation, cfg, timeout, cancel).await
}
/// One guarded composer '+' activation: its renderer context and its refusal wording.
pub(crate) struct PlusMenu {
    pub context: &'static str,
    pub label: &'static str,
    pub navigation: fn(&Value) -> Result<()>,
    pub focus: &'static str,
    pub uncertain: &'static str,
    pub blocked: fn(&Value) -> anyhow::Error,
    pub unverified: &'static str,
}
const WEB_SEARCH: PlusMenu = PlusMenu {
    context: include_str!("../browser-scripts/web-search-context.js"),
    label: "Web Search",
    navigation: require_navigation,
    focus: "Web Search tools control lost focus; nothing was sent",
    uncertain: "Web Search tools activation acknowledgement is uncertain; never retry automatically",
    blocked: |_| {
        anyhow::anyhow!(
            "Web Search tools activation or exact guard cleanup is uncertain; nothing was sent"
        )
    },
    unverified: "Web Search tools activation or exact guard cleanup is uncertain; nothing was sent",
};
/// Press the probed '+' control once with a trusted Enter behind the prepare guard, then
/// clean that guard. Any uncertain delivery quarantines the renderer and is never retried.
pub(crate) async fn press_plus(
    plus: &PlusMenu,
    reservation: &mut RendererReservation,
    cfg: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let activated=async {
        let prepared=evaluate_in(plus,reservation,PREPARE,cfg,timeout,cancel).await?;
        (plus.navigation)(&prepared)?;
        ensure!(prepared["focused"]==true,"{}",plus.focus);
        let down=reservation.page().call("Input.dispatchKeyEvent",json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13,"text":"\r","unmodifiedText":"\r"}),timeout,cancel).await;
        let up=reservation.page().call("Input.dispatchKeyEvent",json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13}),Duration::from_secs(1),&CancellationToken::new()).await;
        if down.is_err()||up.is_err()||cancel.is_cancelled(){reservation.mark_input_uncertain();bail!("{}",plus.uncertain);}
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup = reservation
        .page()
        .evaluate_with_arguments(
            &expression_in(plus.context, plus.label, CLEAN, false),
            &json!([cfg]),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
    if activated.is_err()
        || !cleanup.as_ref().is_ok_and(|v| {
            v["cleaned"] == true
                && v["sawKeyDown"] == true
                && v["clicked"] == true
                && v["blocked"].is_null()
        })
    {
        reservation.mark_input_uncertain();
        let _ = reservation
            .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await;
        activated?;
        if let Ok(summary) = &cleanup
            && summary["cleaned"] == true
            && !summary["blocked"].is_null()
        {
            return Err((plus.blocked)(&summary["blocked"]));
        }
        bail!("{}", plus.unverified);
    }
    Ok(())
}
const UNVERIFIED: &str = "Web Search selection could not be verified; the prompt was not submitted. This pilot supports the English ChatGPT Web search control.";
pub(crate) async fn activate(
    reservation: &mut RendererReservation,
    mut cfg: Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    cfg["owner"] = json!(reservation.owner());
    cfg["plusGuardId"] = json!(uuid::Uuid::new_v4().to_string());
    if verify(reservation, &cfg, timeout, cancel).await? {
        return Ok(false);
    }
    open_tools(
        reservation,
        &cfg,
        timeout,
        cancel,
        "Web Search requires the ChatGPT composer tools menu.",
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let selected = evaluate(reservation, SELECT, &cfg, timeout, cancel).await?;
        if selected == "clicked" {
            break;
        }
        if selected != "missing" {
            reservation.mark_input_uncertain();
        }
        ensure!(
            selected == "missing" && Instant::now() < deadline,
            "{UNVERIFIED}"
        );
        pause(cancel).await?;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if verify(reservation, &cfg, timeout, cancel).await? {
            return Ok(true);
        }
        ensure!(Instant::now() < deadline, "{UNVERIFIED}");
        pause(cancel).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::Cdp;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    async fn exercise(fault: &str, deep: bool) -> (Result<bool>, Vec<String>, bool) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let page = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(2),
        );
        let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let trace = Arc::new(Mutex::new(vec![]));
        let events = trace.clone();
        let fault = fault.to_owned();
        let task = tokio::spawn(async move {
            let mut selected = fault == "already-selected";
            while let Some(Ok(Message::Text(frame))) = peer.next().await {
                let request: Value = serde_json::from_str(&frame).unwrap();
                let expression = request["params"]["expression"].as_str().unwrap_or("");
                let phase = if matches!(
                    request["method"].as_str(),
                    Some("Input.dispatchKeyEvent" | "Input.dispatchMouseEvent")
                ) {
                    request["params"]["type"].as_str().unwrap()
                } else if expression.contains("window.__alcuinResearchSelections?.get") {
                    "research-cleanup"
                } else if expression.contains("const summary = { cleaned: true, sawKeyDown") {
                    "cleanup"
                } else if expression.contains("window.addEventListener('keydown', onKey") {
                    "prepare"
                } else if expression.contains("promptMatches:") {
                    "verify"
                } else if expression
                    .contains("const candidates = Array.from(context.form.querySelectorAll")
                {
                    "probe"
                } else if expression.contains("const matchesLabel =") {
                    "select"
                } else if expression.contains("const expected = cfg.navigation") {
                    "navigation"
                } else {
                    "ownership"
                };
                events.lock().unwrap().push(phase.to_owned());
                if fault == "lost-key-down" && phase == "keyDown" {
                    continue;
                }
                if fault == "lost-mouse-press" && phase == "mousePressed" {
                    continue;
                }
                let value = match phase {
                    "cleanup" => {
                        json!({"cleaned":fault!="unclean","sawKeyDown":true,"clicked":true,"blocked":null})
                    }
                    "research-cleanup" => {
                        json!({"cleaned":fault!="research-unclean","present":true,"sawPress":true,"sawClick":true,"blocked":false})
                    }
                    "prepare" => {
                        json!({"contextMatches":true,"workSelected":false,"modeUnverified":false,"focused":true})
                    }
                    "probe" if fault == "no-menu" => json!({"status":"missing","focused":false}),
                    "probe" => json!({"status":"focused","focused":true}),
                    "verify" => {
                        json!({"selected":selected,"promptMatches":fault!="changed-prompt" && (!deep || fault!="changed-navigation")})
                    }
                    "navigation" => {
                        json!({"contextMatches":fault!="changed-navigation","workSelected":false,"modeUnverified":false})
                    }
                    "select" if fault == "missing" => {
                        if deep {
                            json!({"status":"missing","available":["Create image","Agent mode"]})
                        } else {
                            json!("missing")
                        }
                    }
                    "select" if fault == "no-chip" => {
                        if deep {
                            json!({"status":"ready","x":10,"y":20})
                        } else {
                            json!("clicked")
                        }
                    }
                    "select" => {
                        if deep {
                            json!({"status":"ready","x":10,"y":20})
                        } else {
                            selected = true;
                            json!("clicked")
                        }
                    }
                    "mouseReleased" => {
                        selected = fault != "no-chip";
                        json!({})
                    }
                    "mousePressed" => json!({}),
                    "keyDown" | "keyUp" => json!({}),
                    _ => json!(true),
                };
                if peer
                    .send(Message::Text(
                        json!({"id":request["id"],"result":{"result":{"value":value}}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut reservation = RendererReservation::acquire(
            page,
            "PUBLIC-search-owner".into(),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let cfg = json!({"prompt":"PUBLIC owned research prompt","navigation":{"origin":"https://chatgpt.com","conversationId":null,"landingPath":"/"}});
        let result = if deep {
            crate::browser_research::activate(
                &mut reservation,
                cfg,
                Duration::from_millis(100),
                &CancellationToken::new(),
            )
            .await
        } else {
            activate(
                &mut reservation,
                cfg,
                Duration::from_millis(100),
                &CancellationToken::new(),
            )
            .await
        };
        let _ = reservation
            .quarantine(crate::browser_renderer::CONTROL_ACK_TIMEOUT)
            .await;
        let settled = reservation.settled_before_dispatch();
        reservation
            .page()
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        let trace = trace.lock().unwrap().clone();
        (result, trace, settled)
    }

    #[tokio::test]
    async fn verified_search_uses_one_guarded_input_pair_and_one_selection() {
        let (result, trace, _) = exercise("success", false).await;
        assert!(result.unwrap());
        for phase in ["keyDown", "keyUp", "cleanup", "select"] {
            assert_eq!(
                trace.iter().filter(|entry| entry.as_str() == phase).count(),
                1,
                "{trace:?}"
            );
        }
        assert!(
            trace.iter().position(|v| v == "cleanup").unwrap()
                < trace.iter().position(|v| v == "select").unwrap()
        );
        let (result, trace, _) = exercise("already-selected", false).await;
        assert!(!result.unwrap());
        assert!(
            !trace
                .iter()
                .any(|v| matches!(v.as_str(), "probe" | "keyDown" | "select"))
        );
    }

    #[tokio::test]
    async fn changed_prompt_or_navigation_stops_before_tools_input() {
        for fault in ["changed-prompt", "changed-navigation"] {
            let (result, trace, _) = exercise(fault, false).await;
            assert!(result.is_err());
            assert!(
                !trace
                    .iter()
                    .any(|v| matches!(v.as_str(), "probe" | "keyDown" | "select")),
                "{trace:?}"
            );
        }
    }

    #[tokio::test]
    async fn uncertain_input_or_cleanup_settles_key_release_and_preserves_recovery() {
        for fault in ["lost-key-down", "unclean"] {
            let (result, trace, settled) = exercise(fault, false).await;
            assert!(result.is_err());
            assert_eq!(trace.iter().filter(|v| v.as_str() == "keyDown").count(), 1);
            assert_eq!(trace.iter().filter(|v| v.as_str() == "keyUp").count(), 1);
            assert_eq!(trace.iter().filter(|v| v.as_str() == "cleanup").count(), 1);
            assert!(!trace.iter().any(|v| v == "select"));
            assert!(
                !settled,
                "Uncertain input cannot authorise pre-dispatch retirement"
            );
        }
    }
    #[tokio::test]
    async fn deep_selection_delivers_one_guarded_mouse_pair_and_cleans_both_guards() {
        let (result, trace, _) = exercise("success", true).await;
        assert!(!result.unwrap());
        for phase in [
            "keyDown",
            "keyUp",
            "cleanup",
            "select",
            "mousePressed",
            "mouseReleased",
            "research-cleanup",
        ] {
            assert_eq!(
                trace.iter().filter(|step| *step == phase).count(),
                1,
                "{trace:?}"
            );
        }
        let (result, trace, _) = exercise("already-selected", true).await;
        assert!(result.unwrap());
        assert!(
            !trace
                .iter()
                .any(|step| matches!(step.as_str(), "keyDown" | "mousePressed" | "select"))
        );
    }
    #[tokio::test]
    async fn deep_selection_never_replays_uncertain_mouse_input_and_rejects_changed_composers() {
        for fault in ["lost-mouse-press", "research-unclean"] {
            let (result, trace, settled) = exercise(fault, true).await;
            assert!(result.is_err() && !settled, "{fault}");
            for phase in ["mousePressed", "mouseReleased", "research-cleanup"] {
                assert_eq!(
                    trace.iter().filter(|step| *step == phase).count(),
                    1,
                    "{trace:?}"
                );
            }
        }
        for fault in ["changed-prompt", "changed-navigation"] {
            let (result, trace, _) = exercise(fault, true).await;
            assert!(result.is_err());
            assert!(
                !trace
                    .iter()
                    .any(|step| matches!(step.as_str(), "keyDown" | "mousePressed" | "select")),
                "{trace:?}"
            );
        }
    }
    #[tokio::test]
    async fn missing_tools_options_and_chips_refuse_with_the_reference_messages() {
        for (fault, deep, expected) in [
            (
                "no-menu",
                false,
                "Web Search requires the ChatGPT composer tools menu.",
            ),
            ("missing", false, UNVERIFIED),
            ("no-chip", false, UNVERIFIED),
            (
                "no-menu",
                true,
                "Could not find the composer plus button to activate Deep Research.",
            ),
            (
                "missing",
                true,
                "\"Deep research\" option not found in composer dropdown. Available options: Create image, Agent mode This feature may require a ChatGPT Plus or Pro subscription.",
            ),
            (
                "no-chip",
                true,
                "Deep Research pill did not appear after selection. The UI may have changed.",
            ),
        ] {
            let (result, _, _) = exercise(fault, deep).await;
            assert_eq!(result.unwrap_err().to_string(), expected, "{fault} {deep}");
        }
    }
}

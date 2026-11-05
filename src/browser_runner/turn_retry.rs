//! One retry of a failed turn through the page's own control. The user message stays the one
//! already acknowledged, so this cannot become a second send. ChatGPT does generate the turn
//! again, which is why the budget is one, recorded before the click, and a visible rate-limit or
//! account warning holds it back.
//!
//! Temporary Chat keeps no copy to reopen: reloading its tab or opening its URL elsewhere lands on
//! an empty chat, so the failed page itself is the only place left to recover the turn.
//!
//! It gets up again, once (Proverbs 24:16, NET).
use super::*;

/// How long the failed page may take to start over after the click.
const SETTLE: Duration = Duration::from_secs(30);
/// How long the turn's menu may take to open in a background tab.
const MENU_OPEN: Duration = Duration::from_secs(10);

pub(crate) fn expression() -> String {
    format!(
        "(cfg) => {{ /* alcuin-dom:turn-retry */ const readOwnedResult = {}; const observed = readOwnedResult(cfg);\n{}\n{}\n}}",
        result_expression(),
        include_str!("../../browser-scripts/result-prelude.js"),
        include_str!("../../browser-scripts/turn-retry.js")
    )
}

pub(super) fn eligible(error: &anyhow::Error) -> bool {
    use crate::browser_provider::Failure;
    matches!(
        error.downcast_ref::<Failure>(),
        Some(Failure::GenerationFailed | Failure::ResponseLoadFailed)
    )
}

async fn click(
    page: &Cdp,
    point: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let (x, y) = (&point["x"], &point["y"]);
    for event in [
        json!({"type":"mouseMoved","x":x,"y":y}),
        json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),
        json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),
    ] {
        page.call("Input.dispatchMouseEvent", event, timeout, cancel)
            .await?;
    }
    Ok(())
}

/// One retry per session. A Deep Research turn is left to its own flow.
fn may_retry(metadata: &Value) -> bool {
    metadata["browser"]["config"]["researchMode"] != "deep"
        && !metadata["browser"]["runtime"]["providerRetry"].is_object()
}

/// Leaves the failure standing, and says why in the session log.
fn stands(store: &SessionStore, id: &str, why: &str) -> Result<bool> {
    automation_log(
        store,
        id,
        &format!("[browser] Not retrying the failed turn: {why}."),
    )?;
    Ok(false)
}

/// True once the failed turn has started over. False leaves the failure standing, which also keeps
/// the harvest for a load failure: the budget is spent, a warning holds it back, the page no longer
/// shows the failure or offers no control, or the turn never started again. The model and thinking
/// level were verified before the send and cannot be again, so the record says so and names what
/// was pressed.
#[allow(clippy::too_many_arguments)] // Keep the durable session and target witnesses explicit.
pub(super) async fn attempt(
    store: &SessionStore,
    id: &str,
    witness: &ExecutionWitness,
    context: &read_recovery::Context<'_>,
    reservation: &RendererReservation,
    receipt: &CommitReceipt,
    limits: &RunnerLimits,
    cancel: &CancellationToken,
) -> Result<bool> {
    if !may_retry(&store.require(id)?) {
        return stands(store, id, "its one retry is spent, or it is Deep Research");
    }
    context.require_browser(limits.command, cancel).await?;
    let page = reservation.page();
    if crate::browser_page::warns_against_retry(page, limits.command, cancel).await? {
        return stands(store, id, "ChatGPT shows a warning");
    }
    let function = expression();
    let cfg = json!({"owner":reservation.owner(),"receipt":receipt,"recovery":false,"cleanup":false,"uiErrors":true});
    let read = |menu: bool, before: &Value| {
        let mut cfg = cfg.clone();
        cfg["menu"] = json!(menu);
        cfg["before"] = before.clone();
        let function = &function;
        async move {
            page.evaluate_with_arguments(function, &json!([cfg]), limits.command, cancel)
                .await
        }
    };
    let control = read(false, &Value::Null).await?;
    if control["failed"] == false {
        return stands(store, id, "the page no longer shows the failure");
    }
    if control["ok"] != true {
        return stands(store, id, "the page offers no control for it");
    }
    let mut target = control.clone();
    if control["menu"] == true {
        click(page, &control, limits.command, cancel).await?;
        let deadline = Instant::now() + MENU_OPEN;
        target = loop {
            let item = read(true, &Value::Null).await?;
            if item["ok"] == true {
                break item;
            }
            if Instant::now() >= deadline {
                return stands(store, id, "its menu offered no Try again");
            }
            pause(limits.poll.min(Duration::from_millis(500)), cancel).await?;
        };
    }
    crate::browser_dom::rearm_completion(reservation, &control["baseline"], limits.command, cancel)
        .await?;
    context.require(cancel)?;
    runtime_patch(
        store,
        id,
        witness,
        json!({"providerRetry":{
            "version":1,"at":timestamp(),"control":target["label"],"modelVerified":false
        }}),
    )?;
    automation_log(
        store,
        id,
        &format!(
            "[browser] Retrying the failed turn once in its page with {}; no new message is sent; the model and thinking level are not verified again.",
            target["label"]
        ),
    )?;
    click(page, &target, limits.command, cancel).await?;
    let deadline = Instant::now() + SETTLE.min(limits.answer);
    loop {
        if matches!(read(false, &control["messageId"]).await, Ok(state) if state["started"] == true)
        {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return stands(store, id, "the click did not start it over");
        }
        pause(limits.poll.min(Duration::from_secs(1)), cancel).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_retries_once_and_deep_research_never() {
        assert!(may_retry(&json!({})));
        assert!(!may_retry(
            &json!({"browser":{"runtime":{"providerRetry":{"version":1}}}})
        ));
        assert!(!may_retry(
            &json!({"browser":{"config":{"researchMode":"deep"}}})
        ));
    }
}

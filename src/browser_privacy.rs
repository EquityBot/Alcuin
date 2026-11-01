//! Temporary Chat is live renderer authority, never a URL or message-text claim.
use crate::cdp::Cdp;
use anyhow::Result;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PROBE: &str = include_str!("../browser-scripts/temporary-chat.js");
pub(crate) const REFUSAL: &str =
    "Verified ChatGPT Temporary Chat is required; preserve uncertain requests without replay";
/// Re-read interval whilst the live view is unmounted (the initial composer wait uses the same).
const SETTLE_POLL: Duration = Duration::from_millis(100);

/// A query requests the mode. Only `PROBE` can prove that the app activated it.
pub(crate) fn landing_url(raw: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(raw)?;
    anyhow::ensure!(
        url.origin().ascii_serialization() == "https://chatgpt.com"
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/"),
        "Temporary Chat requires the ChatGPT landing page; saved conversation URLs cannot reopen it"
    );
    let query: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "temporary-chat")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(query)
        .append_pair("temporary-chat", "true");
    Ok(url.into())
}

/// The predicate is lexical so retained input/send guards keep the same verifier.
/// It is not placed in a mutable page-global property.
pub(crate) fn guard(function: &str) -> String {
    with_predicate(&format!(
        "(cfg) => {{ if (!oracleTemporaryChat()) throw Error({}); return (\n{function}\n)(cfg); }}",
        serde_json::to_string(REFUSAL).expect("literal privacy refusal")
    ))
}

/// Bind the live predicate without granting mutation authority. Read-only
/// receipt polling may wait for an unmounted view; acceptance still needs true.
pub(crate) fn with_predicate(function: &str) -> String {
    format!(
        "(cfg) => {{ const oracleTemporaryChat = () => (\n{PROBE}\n); return (\n{function}\n)(cfg); }}"
    )
}

/// Proceeds only on `true`. A conversation route can precede or remount its
/// view, so `null` (unknown) is re-read until `timeout`; `false`, any other
/// value and the deadline refuse. Waiting never grants authority by itself.
pub(crate) async fn require(
    page: &Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!left.is_zero(), REFUSAL);
        let verified = page
            .evaluate_with_arguments(
                &format!("(cfg) => {{ /* alcuin-dom:temporary-chat */ return (\n{PROBE}\n); }}"),
                &json!([{}]),
                left,
                cancel,
            )
            .await?;
        if verified == true {
            return Ok(());
        }
        anyhow::ensure!(verified.is_null(), REFUSAL);
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!left.is_zero(), REFUSAL);
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("Temporary Chat verification was cancelled; {REFUSAL}"),
            _ = tokio::time::sleep(left.min(SETTLE_POLL)) => {}
        }
    }
}

#[cfg(test)]
#[path = "browser_privacy_tests.rs"]
mod tests;

//! Reference page gates and notices around a ChatGPT run: ensureNotBlocked, ensureLoggedIn and
//! the manual-login wait (actions/navigation.ts, manualLoginProfile.ts), ensureChatMode on an
//! owned landing page, JavaScript dialog dismissal and ChatGPT UI warnings (uiWarnings.ts).
//! None of them enters payload or sends.
use crate::{browser_renderer::RendererReservation, cdp::Cdp};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{path::Path, sync::LazyLock, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const BLOCKED: &str = include_str!("../browser-scripts/blocked.js");
const LOGIN: &str = include_str!("../browser-scripts/health-login.js");
const UI_WARNINGS: &str = include_str!("../browser-scripts/ui-warnings.js");
const CHAT_MODE: &str = include_str!("../browser-scripts/chat-mode.js");
// Stricter than the reference coordinate click: the probed Chat control must still be the
// element under the point, or nothing is pressed.
const CHAT_TARGET: &str = "(point) => { /* industrious-dom:chat-mode-target */ const normalize=(value)=>String(value||'').toLowerCase().replace(/\\s+/g,' ').trim(); const hit=document.elementFromPoint(point.x,point.y)?.closest('button'); if(!(hit instanceof HTMLElement) || normalize(hit.textContent)!=='chat') return false; return Boolean(hit.closest('[role=\"group\"][aria-label=\"Composer mode\"]')) || hit.getAttribute('role')==='radio'; }";
/// Reference CLOUDFLARE_HYDRATION_GRACE_MS.
const HYDRATION_GRACE: Duration = Duration::from_secs(12);
/// Reference navigateToChatGPT waitForDocumentReady.
const DOCUMENT_READY: Duration = Duration::from_secs(45);
/// Reference LOGIN_CHECK_TIMEOUT_MS.
const LOGIN_CHECK_TIMEOUT_MS: u64 = 5_000;
/// Reference NON_INTERACTIVE_MANUAL_LOGIN_WAIT_MS.
const NON_INTERACTIVE_MANUAL_LOGIN_WAIT: Duration = Duration::from_secs(120);

async fn pause(duration: Duration, cancel: &CancellationToken) -> Result<()> {
    tokio::select! { biased; _ = cancel.cancelled() => bail!("Browser operation cancelled; nothing was sent"), _ = tokio::time::sleep(duration) => Ok(()) }
}
fn prefix(text: &str, units: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(units).collect::<Vec<_>>())
}

/// Reference ensureNotBlocked Cloudflare refusal; a headful run keeps its tab for the check.
#[derive(Debug)]
pub(crate) struct CloudflareChallenge {
    pub headless: bool,
}
impl std::fmt::Display for CloudflareChallenge {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(if self.headless {
            "Cloudflare challenge detected in headless mode. Re-run with --headful so you can solve the challenge."
        } else {
            "Cloudflare challenge detected. Complete the “Just a moment…” check in the open browser, then rerun."
        })
    }
}
impl std::error::Error for CloudflareChallenge {}

/// Reference waitForDocumentReady after navigateToChatGPT.
pub(crate) async fn wait_document_ready(
    page: &Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let deadline = Instant::now() + DOCUMENT_READY;
    loop {
        let state = page
            .evaluate("document.readyState", timeout, cancel)
            .await?;
        if matches!(state.as_str(), Some("complete" | "interactive")) {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Page did not reach ready state in time"
        );
        pause(Duration::from_millis(100), cancel).await?;
    }
}
/// Reference waitForDocumentReady then ensureNotBlocked: strong challenge evidence refuses at
/// once, an app shell never does, and script-only evidence must survive the hydration grace.
pub(crate) async fn ensure_not_blocked(
    page: &Cdp,
    headless: bool,
    timeout: Duration,
    cancel: &CancellationToken,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    wait_document_ready(page, timeout, cancel).await?;
    let grace = Instant::now() + HYDRATION_GRACE;
    loop {
        let verdict = page
            .evaluate_with_arguments(BLOCKED, &json!([{}]), timeout, cancel)
            .await?;
        let (strong, shell, weak) = (
            verdict["strong"] == true,
            verdict["shell"] == true,
            verdict["weak"] == true,
        );
        if !strong && !shell && weak && Instant::now() < grace {
            pause(Duration::from_millis(500), cancel).await?;
            continue;
        }
        if strong || !shell && weak {
            log("Cloudflare anti-bot page detected");
            return Err(CloudflareChallenge { headless }.into());
        }
        if verdict["securityBlock"] == true {
            log("ChatGPT account security block detected");
            bail!(
                "ChatGPT account security block detected. Open chatgpt.com in Chrome, secure the account, then rerun Industrious."
            );
        }
        return Ok(());
    }
}

/// Reference ensureLoggedIn refusal; the manual-login wait retries only this error.
#[derive(Debug)]
pub(crate) struct SessionMissing(String);
impl std::fmt::Display for SessionMissing {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for SessionMissing {}

/// How the login check words its hint (reference ensureLoggedIn options).
#[derive(Clone, Copy)]
pub(crate) struct LoginHint {
    pub applied_cookies: u64,
    pub remote: bool,
}

/// Reference ensureLoggedIn. Industrious never selects a Welcome back account (decision 7b), so a
/// failed probe refuses without the account picker attempt or its hints.
pub(crate) async fn ensure_logged_in(
    page: &Cdp,
    hint: LoginHint,
    timeout: Duration,
    cancel: &CancellationToken,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let probe = page
        .evaluate_with_arguments(
            LOGIN,
            &json!([{"timeoutMs":LOGIN_CHECK_TIMEOUT_MS}]),
            timeout,
            cancel,
        )
        .await?;
    // Reference normalizeLoginProbe.
    let status = match &probe["status"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) if s.trim().parse::<f64>().is_ok_and(f64::is_finite) => {
            s.trim().parse::<f64>().unwrap_or(0.0).to_string()
        }
        _ => "0".into(),
    };
    let backend = probe["backendStatus"]
        .as_number()
        .map_or("n/a".into(), ToString::to_string);
    let flag = |key: &str| probe[key] == true;
    if probe["ok"] == true {
        log(&format!(
            "Login check passed (sessionStatus={status}, sessionAuthenticated={}, backendStatus={backend}, domLoginCta={}, appAuthenticated={})",
            flag("sessionAuthenticated"),
            flag("domLoginCta"),
            flag("appAuthenticated")
        ));
        return Ok(());
    }
    log(&format!(
        "Login probe failed (sessionStatus={status}, sessionAuthenticated={}, sessionResolved={}, backendStatus={backend}, domLoginCta={}, onAuthPage={}, appAuthenticated={}, cfBlocked={}, url={}, error={})",
        flag("sessionAuthenticated"),
        flag("sessionResolved"),
        flag("domLoginCta"),
        flag("onAuthPage"),
        flag("appAuthenticated"),
        flag("cfBlocked"),
        probe["pageUrl"].as_str().unwrap_or("n/a"),
        probe["error"].as_str().unwrap_or("none")
    ));
    let dom = if flag("domLoginCta") {
        " Login button detected on page."
    } else {
        ""
    };
    let cookies = if hint.remote {
        "The remote Chrome session is not signed into ChatGPT. Sign in there, then rerun."
    } else if hint.applied_cookies == 0 {
        "No ChatGPT cookies were applied; sign in to chatgpt.com in Chrome or pass inline cookies (--browser-inline-cookies[(-file)] / ORACLE_BROWSER_COOKIES_JSON)."
    } else {
        "ChatGPT login appears missing; open chatgpt.com in Chrome to refresh the session or provide inline cookies (--browser-inline-cookies[(-file)] / ORACLE_BROWSER_COOKIES_JSON)."
    };
    Err(SessionMissing(format!("ChatGPT session not detected.{dom} {cookies}")).into())
}

/// Reference resolveManualLoginWaitMs.
pub(crate) fn manual_login_wait(timeout_ms: Option<u64>, keep_browser: bool) -> Duration {
    let configured = Duration::from_millis(timeout_ms.unwrap_or(1_200_000).min(20 * 60_000));
    if keep_browser {
        configured
    } else {
        configured.min(NON_INTERACTIVE_MANUAL_LOGIN_WAIT)
    }
}

/// Reference waitForLogin and waitForProjectSourcesLogin. Without manual login this is one
/// ensureLoggedIn; with it the check repeats each second until `wait` ends. True when the
/// session appeared only after a failed check, so the caller reopens its own page.
#[allow(clippy::too_many_arguments)] // The reference wait keeps each of its inputs explicit.
pub(crate) async fn wait_for_login(
    page: &Cdp,
    manual: bool,
    hint: LoginHint,
    wait: Duration,
    profile: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
    log: &(dyn Fn(&str) + Sync),
) -> Result<bool> {
    if !manual {
        ensure_logged_in(page, hint, timeout, cancel, log).await?;
        return Ok(false);
    }
    let deadline = Instant::now() + wait;
    let mut notice: Option<Instant> = None;
    let mut retried = false;
    while Instant::now() < deadline {
        match ensure_logged_in(page, hint, timeout, cancel, log).await {
            Ok(()) => return Ok(retried),
            Err(error) if error.is::<SessionMissing>() => {
                if notice.is_none_or(|at| at.elapsed() > Duration::from_secs(5)) {
                    log(
                        "Manual login mode: please sign into chatgpt.com in the opened Chrome window; waiting for session to appear...",
                    );
                    notice = Some(Instant::now());
                }
                retried = true;
                pause(Duration::from_secs(1), cancel).await?;
            }
            Err(error) => return Err(error),
        }
    }
    bail!(
        "Manual login mode timed out waiting for ChatGPT session. Browser mode is using Industrious's private Chrome profile at {}, not your normal Chrome profile. Run first-time setup, sign in there, then retry: {}",
        profile.display(),
        crate::browser_chrome::manual_login_setup_command(profile)
    )
}

/// JavaScript dialogs on this page: alert and beforeunload are accepted; confirm, prompt and
/// any other type are dismissed (deliberate; the reference accepts every dialog). Dropping
/// the guard removes the handler.
pub(crate) struct DialogDismissal(tokio::task::JoinHandle<()>);
impl Drop for DialogDismissal {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(crate) async fn dismiss_dialogs(
    page: &Cdp,
    timeout: Duration,
    cancel: &CancellationToken,
    log: impl Fn(&str) + Send + 'static,
) -> Result<DialogDismissal> {
    let mut events = page.events();
    page.call("Page.enable", json!({}), timeout, cancel).await?;
    let page = page.clone();
    let session = page.session_id().map(str::to_owned);
    Ok(DialogDismissal(tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            if event["method"] != "Page.javascriptDialogOpening"
                || event["sessionId"].as_str() != session.as_deref()
            {
                continue;
            }
            let kind = event["params"]["type"].as_str().unwrap_or("unknown");
            let message = event["params"]["message"].as_str().unwrap_or("");
            let accept = matches!(kind, "alert" | "beforeunload");
            log(&format!(
                "[nav] {} JS dialog ({kind}){}",
                if accept { "accepting" } else { "dismissing" },
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {}", prefix(message, 140))
                }
            ));
            if let Err(error) = page
                .call(
                    "Page.handleJavaScriptDialog",
                    json!({"accept":accept}),
                    Duration::from_secs(5),
                    &CancellationToken::new(),
                )
                .await
            {
                log(&format!("[nav] failed to answer JS dialog: {error}"));
            }
        }
    })))
}

static RATE_LIMIT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?-u:\b)(?:too many requests|sending too many requests|too quickly|temporarily limited access|please wait a few minutes|rate limit(?:ed)?|slow down)(?-u:\b)").expect("rate-limit pattern")
});
static UNAVAILABLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?-u:\b)(?:temporarily unavailable|something went wrong|failed to generate|try again later)(?-u:\b)").expect("unavailable pattern")
});
static CHALLENGE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?-u:\b)(?:verify you are human|unusual activity|cloudflare|challenge|login required|sign in)(?-u:\b)").expect("challenge pattern")
});
static EMAIL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(?-u:\b)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}(?-u:\b)")
        .expect("email pattern")
});
static TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)(?-u:\b)((?:access|auth|session)[-_ ]?token|token)\s*[:=]\s*["']?[^\s"',;]+"#,
    )
    .expect("token pattern")
});
static KEY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?-u:\b)(?:sk-(?:ant-|or-)?|xai-)[A-Za-z0-9_-]{8,}(?-u:\b)")
        .expect("key pattern")
});
/// Reference classifyChatGptUiWarningText.
pub(crate) fn classify_ui_warning(text: &str) -> Option<&'static str> {
    let normalized = text.to_lowercase();
    [
        (&*RATE_LIMIT, "rate_limit"),
        (&*UNAVAILABLE, "temporary_unavailable"),
        (&*CHALLENGE, "auth_or_challenge"),
    ]
    .into_iter()
    .find(|(pattern, _)| pattern.is_match(&normalized))
    .map(|(_, kind)| kind)
}
/// Reference sanitizeChatGptUiWarningText.
fn sanitise_ui_warning(text: &str) -> String {
    let text = EMAIL.replace_all(text, "[redacted-email]");
    let text = TOKEN.replace_all(&text, "$1=[redacted]");
    KEY.replace_all(&text, "[redacted-token]").into_owned()
}
/// Reference collectChatGptUiWarnings, first warning only: its type and sanitised message.
pub(crate) fn first_ui_warning(raw: &Value) -> Option<(&'static str, String)> {
    raw.as_array()?.iter().find_map(|candidate| {
        let text = candidate["text"].as_str()?;
        let text = sanitise_ui_warning(
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .as_str(),
        );
        let text = prefix(&text, 300);
        if text.is_empty() {
            return None;
        }
        Some((classify_ui_warning(&text)?, text))
    })
}
/// Reference createChatGptUiWarningError: the warning wording when ChatGPT shows one, logged
/// as the reference does. A failed read finds no warning.
pub(crate) async fn ui_warning(
    page: &Cdp,
    wait_target: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    log: &(dyn Fn(&str) + Sync),
) -> Option<String> {
    let raw = page
        .evaluate_with_arguments(UI_WARNINGS, &json!([{}]), timeout, cancel)
        .await
        .ok()?;
    let (kind, message) = first_ui_warning(&raw)?;
    log(&format!(
        "[browser] ChatGPT UI warning detected ({kind}): {message}"
    ));
    let label = match kind {
        "rate_limit" => "rate-limit",
        "temporary_unavailable" => "temporary-unavailable",
        _ => "authentication/challenge",
    };
    Some(format!(
        "ChatGPT displayed a {label} warning while waiting for {wait_target}: {message}"
    ))
}

/// Reference ensureChatMode for a new owned chat: once the composer is ready, a selected
/// Work mode is switched to Chat with one press of the Chat control, and the mode must then
/// verify within 10 seconds. Without Chat/Work controls the run continues unchanged. The
/// owned landing page has no saved conversation, so no history row is revealed or reset.
pub(crate) async fn ensure_chat_mode(
    reservation: &mut RendererReservation,
    deadline: Instant,
    timeout: Duration,
    cancel: &CancellationToken,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let window = timeout.min(Duration::from_secs(10));
    let mut verify: Option<Instant> = None;
    let (mut changed, mut clicked) = (false, false);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if verify.is_none() && left.is_zero() {
            // The composer readiness check that follows reports the missing composer.
            return Ok(());
        }
        let probe = reservation
            .page()
            .evaluate_with_arguments(CHAT_MODE, &json!([{}]), timeout, cancel)
            .await?;
        let status = probe["status"].as_str().unwrap_or("");
        if status == "prompt-pending" && verify.is_none() {
            pause(left.min(Duration::from_millis(200)), cancel).await?;
            continue;
        }
        let until = *verify.get_or_insert_with(|| Instant::now() + window);
        let expired = Instant::now() >= until;
        match status {
            "chat-selected" | "chat-conversation" => {
                log(if changed {
                    "ChatGPT mode: Chat (switched from Work)"
                } else {
                    "ChatGPT mode: Chat"
                });
                return Ok(());
            }
            "conversation-unresolved" if !expired => {}
            "conversation-unresolved" if changed => break,
            "conversation-unresolved" => bail!(
                "Industrious could not verify whether the selected ChatGPT conversation uses Chat or Work mode, so it cannot safely resume that conversation."
            ),
            "controls-absent" if !changed => return Ok(()),
            "work-conversation" if changed => break,
            "work-conversation" => bail!(
                "The selected ChatGPT tab is an existing Work conversation and cannot be resumed as an ordinary Chat conversation. Start a new Chat conversation instead."
            ),
            "work-selected" if !clicked => {
                if let (Some(x), Some(y)) = (
                    probe["chatPoint"]["x"].as_f64(),
                    probe["chatPoint"]["y"].as_f64(),
                ) {
                    log("ChatGPT mode: Work; switching to Chat");
                    press_chat(reservation, x, y, timeout, cancel).await?;
                    (changed, clicked) = (true, true);
                    verify = Some(Instant::now() + window);
                    pause(Duration::from_millis(200), cancel).await?;
                    continue;
                }
            }
            _ => {}
        }
        if expired {
            break;
        }
        pause(Duration::from_millis(200), cancel).await?;
    }
    bail!(if changed {
        "Industrious could not verify Chat mode after leaving Work."
    } else {
        "ChatGPT exposed Chat/Work controls but Industrious could not verify the active mode."
    })
}
async fn press_chat(
    reservation: &mut RendererReservation,
    x: f64,
    y: f64,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let point = json!({"x":x,"y":y});
    anyhow::ensure!(
        reservation
            .evaluate_tracked(CHAT_TARGET, &json!([point]), timeout, cancel)
            .await?
            == true,
        "ChatGPT Chat mode control moved before activation; nothing was sent"
    );
    for (kind, press) in [
        ("mouseMoved", false),
        ("mousePressed", true),
        ("mouseReleased", true),
    ] {
        let mut event = json!({"type":kind,"x":x,"y":y});
        if press {
            event["button"] = json!("left");
            event["clickCount"] = json!(1);
        }
        if reservation
            .call_tracked("Input.dispatchMouseEvent", event, timeout, cancel)
            .await
            .is_err()
        {
            reservation.mark_input_uncertain();
            bail!("ChatGPT Chat mode switch delivery is uncertain; nothing was sent");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "browser_page_tests.rs"]
pub(crate) mod tests;

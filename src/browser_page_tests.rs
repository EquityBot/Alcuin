use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

#[test]
fn ui_warning_text_classifies_and_sanitises_like_the_reference() {
    for (text, kind) in [
        ("You are sending too many requests", Some("rate_limit")),
        ("Please slow down", Some("rate_limit")),
        ("You have been rate limited", Some("rate_limit")),
        (
            "Something went wrong. Try again later.",
            Some("temporary_unavailable"),
        ),
        (
            "This model is temporarily unavailable",
            Some("temporary_unavailable"),
        ),
        (
            "Failed to generate a response",
            Some("temporary_unavailable"),
        ),
        ("Please verify you are human", Some("auth_or_challenge")),
        ("Sign in to continue", Some("auth_or_challenge")),
        ("Signing information", None),
        ("PUBLIC ordinary status", None),
    ] {
        assert_eq!(classify_ui_warning(text), kind, "{text}");
    }
    let raw = json!([
        {"text":"   "},
        {"text":"PUBLIC unrelated banner"},
        {"text":" Rate limit\n reached for PUBLIC@example.com token: abc123 sk-ant-PUBLICKEY12345 "},
    ]);
    assert_eq!(
        first_ui_warning(&raw),
        Some((
            "rate_limit",
            "Rate limit reached for [redacted-email] token=[redacted] [redacted-token]".to_owned()
        ))
    );
    let long = json!([{ "text": format!("too many requests {}", "x".repeat(400)) }]);
    assert_eq!(
        first_ui_warning(&long).unwrap().1.encode_utf16().count(),
        300
    );
}

#[test]
fn manual_login_wait_follows_resolve_manual_login_wait_ms() {
    assert_eq!(manual_login_wait(None, true), Duration::from_secs(1_200));
    assert_eq!(manual_login_wait(None, false), Duration::from_secs(120));
    assert_eq!(
        manual_login_wait(Some(3_600_000), true),
        Duration::from_secs(1_200)
    );
    assert_eq!(
        manual_login_wait(Some(30_000), false),
        Duration::from_secs(30)
    );
}

#[tokio::test]
async fn run_page_accepts_alert_and_beforeunload_and_dismisses_confirm_and_prompt() {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let browser = Cdp::from_socket(
        WebSocketStream::from_raw_socket(client, Role::Client, None).await,
        Duration::from_secs(1),
    );
    let page = browser.with_session("SESSION-PUBLIC".into());
    let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let handled = Arc::new(Mutex::new(vec![]));
    let seen = handled.clone();
    let task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(frame))) = peer.next().await {
            let request: Value = serde_json::from_str(&frame).unwrap();
            if request["method"] == "Page.enable" {
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":{}}).to_string().into(),
                ))
                .await
                .unwrap();
                // Another page's dialog is not this run's to answer.
                for (session, kind, message) in [
                    ("SESSION-OTHER", "beforeunload", "PUBLIC foreign"),
                    ("SESSION-PUBLIC", "alert", "PUBLIC notice"),
                    ("SESSION-PUBLIC", "beforeunload", "PUBLIC leave?"),
                    ("SESSION-PUBLIC", "confirm", "PUBLIC delete?"),
                    ("SESSION-PUBLIC", "prompt", "PUBLIC name?"),
                ] {
                    peer.send(Message::Text(json!({"method":"Page.javascriptDialogOpening","sessionId":session,"params":{"type":kind,"message":message,"url":"https://chatgpt.com/"}}).to_string().into())).await.unwrap();
                }
                continue;
            }
            seen.lock().unwrap().push((
                request["method"].clone(),
                request["sessionId"].clone(),
                request["params"].clone(),
            ));
            peer.send(Message::Text(
                json!({"id":request["id"],"result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
        }
    });
    let lines = Arc::new(Mutex::new(vec![]));
    let logged = lines.clone();
    let guard = dismiss_dialogs(
        &page,
        Duration::from_secs(1),
        &CancellationToken::new(),
        move |line| logged.lock().unwrap().push(line.to_owned()),
    )
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while handled.lock().unwrap().len() < 4 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        *handled.lock().unwrap(),
        [true, true, false, false]
            .map(|accept| (
                json!("Page.handleJavaScriptDialog"),
                json!("SESSION-PUBLIC"),
                json!({"accept":accept})
            ))
            .to_vec()
    );
    assert_eq!(
        *lines.lock().unwrap(),
        [
            "[nav] accepting JS dialog (alert): PUBLIC notice",
            "[nav] accepting JS dialog (beforeunload): PUBLIC leave?",
            "[nav] dismissing JS dialog (confirm): PUBLIC delete?",
            "[nav] dismissing JS dialog (prompt): PUBLIC name?",
        ]
        .map(str::to_owned)
        .to_vec()
    );
    drop(guard);
    browser
        .disconnect_and_wait(Duration::from_secs(1))
        .await
        .unwrap();
    task.await.unwrap();
}

async fn mount(page: &Cdp, html: &str, title: &str, setup: &str) -> Result<()> {
    page.evaluate_with_arguments(
        &format!(
            "(cfg)=>{{sessionStorage.removeItem('oracle:target-claim');history.replaceState(null,'','/');document.title=cfg.title;document.body.innerHTML=cfg.html;window.PUBLIC_clicks=[];({setup})();return true;}}"
        ),
        &json!([{"html":html,"title":title}]),
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await?;
    Ok(())
}

/// Native Chrome cases for the page gates, run from the opt-in privacy fixture on an
/// intercepted ChatGPT origin whose CSP refuses every network request.
pub(crate) async fn native_cases(page: &Cdp) -> Result<()> {
    use anyhow::Context;
    let cancel = CancellationToken::new();
    let short = Duration::from_secs(5);
    let lines = Mutex::new(Vec::<String>::new());
    let log = |line: &str| lines.lock().unwrap().push(line.to_owned());
    // Cloudflare interstitial and account security block.
    mount(
        page,
        "<h1>chatgpt.com</h1><p>Checking your browser before accessing.</p>",
        "Just a moment...",
        "()=>{}",
    )
    .await?;
    for headless in [false, true] {
        let error = ensure_not_blocked(page, headless, short, &cancel, &log)
            .await
            .err()
            .context("A Just a moment page was admitted")?;
        anyhow::ensure!(
            error
                .downcast_ref::<CloudflareChallenge>()
                .is_some_and(|c| c.headless == headless),
            "Wrong challenge refusal: {error:#}"
        );
    }
    mount(
        page,
        "<p>Suspicious activity detected. Secure your account to regain access.</p>",
        "",
        "()=>{}",
    )
    .await?;
    let error = ensure_not_blocked(page, false, short, &cancel, &log).await;
    anyhow::ensure!(
        error.as_ref().is_err_and(|e| e.to_string()
            == "ChatGPT account security block detected. Open chatgpt.com in Chrome, secure the account, then rerun Industrious."),
        "Account security block: {error:?}"
    );
    // A bot-management script beside the app shell is not a challenge, and script-only
    // evidence waits for the shell to hydrate.
    mount(
        page,
        "<script src='/cdn-cgi/challenge-platform/PUBLIC.js'></script><form><textarea id='prompt-textarea'></textarea></form>",
        "ChatGPT",
        "()=>{}",
    )
    .await?;
    ensure_not_blocked(page, false, short, &cancel, &log)
        .await
        .context("App shell read as a challenge")?;
    mount(
        page,
        "<script src='/cdn-cgi/challenge-platform/PUBLIC.js'></script>",
        "",
        "()=>{setTimeout(()=>document.body.insertAdjacentHTML('beforeend',\"<form><textarea id='prompt-textarea'></textarea></form>\"),700);}",
    )
    .await?;
    let started = Instant::now();
    ensure_not_blocked(page, false, short, &cancel, &log)
        .await
        .context("Hydrating shell read as a challenge")?;
    anyhow::ensure!(
        started.elapsed() >= Duration::from_millis(500),
        "Script-only evidence was not given the hydration grace"
    );
    // Login probe: the fixture refuses every request, so only DOM signals decide.
    const SIGNED_IN: &str = "<header><button data-testid='accounts-profile-button'>PUBLIC</button></header><form><textarea id='prompt-textarea'></textarea></form>";
    const SIGNED_OUT: &str = "<header><button>Log in</button></header><form><textarea id='prompt-textarea'></textarea></form>";
    let hint = LoginHint {
        applied_cookies: 0,
        remote: false,
    };
    mount(page, SIGNED_IN, "ChatGPT", "()=>{}").await?;
    ensure_logged_in(page, hint, short, &cancel, &log)
        .await
        .context("Signed-in fixture was refused")?;
    mount(page, SIGNED_OUT, "ChatGPT", "()=>{}").await?;
    for (hint, expected) in [
        (
            hint,
            "ChatGPT session not detected. Login button detected on page. No ChatGPT cookies were applied; sign in to chatgpt.com in Chrome or pass inline cookies (--browser-inline-cookies[(-file)] / ORACLE_BROWSER_COOKIES_JSON).",
        ),
        (
            LoginHint {
                applied_cookies: 3,
                remote: true,
            },
            "ChatGPT session not detected. Login button detected on page. The remote Chrome session is not signed into ChatGPT. Sign in there, then rerun.",
        ),
    ] {
        let error = ensure_logged_in(page, hint, short, &cancel, &log)
            .await
            .err()
            .context("Signed-out fixture was admitted")?;
        anyhow::ensure!(
            error.is::<SessionMissing>() && error.to_string() == expected,
            "{error:#}"
        );
    }
    mount(
        page,
        SIGNED_OUT,
        "ChatGPT",
        &format!(
            "()=>{{setTimeout(()=>{{document.body.innerHTML={};}},1200);}}",
            json!(SIGNED_IN)
        ),
    )
    .await?;
    let profile = Path::new("PUBLIC-profile");
    anyhow::ensure!(
        wait_for_login(
            page,
            true,
            hint,
            Duration::from_secs(10),
            profile,
            short,
            &cancel,
            &log
        )
        .await?,
        "A late sign-in did not report its wait"
    );
    anyhow::ensure!(
        lines.lock().unwrap().iter().any(|line| line
            == "Manual login mode: please sign into chatgpt.com in the opened Chrome window; waiting for session to appear..."),
        "Manual login notice was not logged"
    );
    // UI warnings: visible warning containers only.
    mount(
        page,
        "<div role='status' style='display:none'>Something went wrong</div><div aria-live='polite'>PUBLIC ordinary progress</div><div role='alert'>Too many requests. Try again later.</div>",
        "ChatGPT",
        "()=>{}",
    )
    .await?;
    let warning = ui_warning(page, "the assistant", short, &cancel, &log).await;
    anyhow::ensure!(
        warning.as_deref()
            == Some(
                "ChatGPT displayed a rate-limit warning while waiting for the assistant: Too many requests. Try again later."
            ),
        "Unexpected UI warning: {warning:?}"
    );
    mount(
        page,
        "<div role='alert' style='visibility:hidden'>Too many requests</div><div role='status'>PUBLIC ready</div>",
        "ChatGPT",
        "()=>{}",
    )
    .await?;
    let hidden = ui_warning(page, "the assistant", short, &cancel, &log).await;
    anyhow::ensure!(hidden.is_none(), "Hidden warning was reported: {hidden:?}");
    // Work mode: one trusted press of the exact Chat control, then verified Chat.
    const WORK: &str = "<header data-app-shell-titlebar='true'><button aria-label='Turn off temporary chat'>Temporary</button></header><form><textarea id='prompt-textarea'></textarea><div role='group' aria-label='Composer mode'><button id='PUBLIC-chat' aria-pressed='false'>Chat</button><button id='PUBLIC-work' aria-pressed='true'>Work</button></div></form>";
    for (case, setup, expected, clicks) in [
        (
            "switch",
            "()=>{const chat=document.getElementById('PUBLIC-chat');chat.addEventListener('click',(event)=>{window.PUBLIC_clicks.push(event.isTrusted);chat.setAttribute('aria-pressed','true');document.getElementById('PUBLIC-work').setAttribute('aria-pressed','false');});}",
            None,
            json!([true]),
        ),
        (
            "stuck",
            "()=>{document.getElementById('PUBLIC-chat').addEventListener('click',(event)=>window.PUBLIC_clicks.push(event.isTrusted));}",
            Some("Industrious could not verify Chat mode after leaving Work."),
            json!([true]),
        ),
        (
            "covered",
            "()=>{const chat=document.getElementById('PUBLIC-chat');chat.addEventListener('click',()=>window.PUBLIC_clicks.push('chat'));const rect=chat.getBoundingClientRect();const cover=document.createElement('button');cover.textContent='PUBLIC cover';cover.style.cssText=`position:fixed;left:${rect.left}px;top:${rect.top}px;width:${rect.width}px;height:${rect.height}px;z-index:9`;cover.addEventListener('click',()=>window.PUBLIC_clicks.push('cover'));document.body.append(cover);}",
            Some("ChatGPT Chat mode control moved before activation; nothing was sent"),
            json!([]),
        ),
    ] {
        mount(page, WORK, "ChatGPT", setup).await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            format!("PUBLIC-chat-mode-{case}"),
            short,
            &cancel,
        )
        .await?;
        let result = ensure_chat_mode(
            &mut reservation,
            Instant::now() + short,
            Duration::from_secs(2),
            &cancel,
            &log,
        )
        .await;
        let seen = page
            .evaluate("window.PUBLIC_clicks", short, &cancel)
            .await?;
        let matched = match expected {
            None => result.is_ok(),
            Some(message) => result.as_ref().is_err_and(|e| e.to_string() == message),
        };
        anyhow::ensure!(matched && seen == clicks, "{case}: {result:?} {seen}");
        anyhow::ensure!(
            reservation.state() == crate::browser_renderer::DispatchState::NotStarted,
            "{case}: the mode switch started dispatch"
        );
    }
    mount(
        page,
        &WORK.replace("aria-label='Composer mode'", ""),
        "ChatGPT",
        "()=>{}",
    )
    .await?;
    let mut reservation = RendererReservation::acquire(
        page.clone(),
        "PUBLIC-chat-mode-absent".into(),
        short,
        &cancel,
    )
    .await?;
    ensure_chat_mode(
        &mut reservation,
        Instant::now() + short,
        Duration::from_secs(2),
        &cancel,
        &log,
    )
    .await
    .context("Composer without mode controls was refused")?;
    mount(page, "", "", "()=>{}").await
}

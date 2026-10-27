//! Opt-in native Chrome fixtures. No account, cookies or provider traffic are used.
use super::*;
use crate::{
    browser_chrome::{ChromeConnection, ChromeLaunch},
    browser_dom::{self, PreparedPrompt},
    browser_profile::ProfileRegistry,
    browser_renderer::RendererReservation,
};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::FutureExt;
use serde_json::Value;
use std::time::Instant;

const SHORT: Duration = Duration::from_secs(5);
/// An unknown predicate is re-read until its whole timeout, so refusal cases use a short one.
const UNKNOWN: Duration = Duration::from_millis(300);
const ACTIVE: &str = "<main><header data-app-shell-titlebar='true'><button aria-label='Turn off temporary chat'>Temporary</button></header><form><textarea id='prompt-textarea' name='prompt-textarea'></textarea><input type='file' id='PUBLIC-file'><button data-testid='send-button' type='submit'>Send</button></form><div id='view'>PUBLIC view</div></main>";

#[test]
fn landing_url_requests_temporary_mode_without_claiming_it_is_verified() {
    assert_eq!(
        landing_url("https://chatgpt.com/?temporary-chat=false&public=1").unwrap(),
        "https://chatgpt.com/?public=1&temporary-chat=true"
    );
    for raw in [
        "http://chatgpt.com/",
        "https://chatgpt.com.evil/",
        "https://user@chatgpt.com/",
        "https://chatgpt.com/c/PUBLIC-id",
        "https://chatgpt.com/g/PUBLIC-project",
    ] {
        assert!(landing_url(raw).is_err(), "{raw}");
    }
}

async fn mount(page: &Cdp, html: &str, path: &str, setup: &str) -> Result<()> {
    page.evaluate_with_arguments(
        "(cfg) => { window.__oracleSubmissionWitness?.cleanup?.();delete window.__oracleSubmissionWitness; history.replaceState(null,'',cfg.path); document.body.innerHTML=cfg.html; return true; }",
        &json!([{"html":html,"path":path}]),
        SHORT,
        &CancellationToken::new(),
    )
    .await?;
    if !setup.is_empty() {
        page.evaluate_with_arguments(
            &format!("(cfg)=>{{ ({setup})(cfg); return true; }}"),
            &json!([{}]),
            SHORT,
            &CancellationToken::new(),
        )
        .await?;
    }
    Ok(())
}

async fn cases(page: &Cdp, file: &std::path::Path) -> Result<()> {
    crate::browser_page::tests::native_cases(page)
        .await
        .context("Page gate regression")?;
    crate::project_sources::native_surface_scope_cases(page)
        .await
        .context("Sources scope/list regression")?;
    initial_readiness_cases(page)
        .await
        .context("Initial readiness regression")?;
    generic_upload_input_cases(page, file)
        .await
        .context("Generic upload regression")?;
    plus_menu_upload_case(page, file)
        .await
        .context("Composer plus upload regression")?;
    web_search_captured_menu_case(page)
        .await
        .context("Web search captured menu regression")?;
    pro_model_picker_cases(page)
        .await
        .context("GPT-6 Pro picker regression")?;
    file_card_candidate_cases(page)
        .await
        .context("File card download candidate regression")?;
    unrendered_completion_cases(page)
        .await
        .context("Unrendered answer completion regression")?;
    deferred_editor_render_case(page)
        .await
        .context("Deferred composer render regression")?;
    data_transfer_upload_case(
        page,
        file.parent().context("Fixture file has no directory")?,
    )
    .await
    .context("Remote data-transfer upload regression")?;
    stale_conversation_cookie_case(page)
        .await
        .context("Stale conversation cookie regression")?;
    for (changed, consumed) in [(false, false), (false, true), (true, false)] {
        mount(
            page,
            ACTIVE,
            "/",
            "()=>{sessionStorage.removeItem('oracle:target-claim');}",
        )
        .await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &CancellationToken::new(),
        )
        .await?;
        browser_dom::native_upload_selection_case(&mut reservation, changed, consumed)
            .await
            .context("File selection seal regression")?;
        page.evaluate(
            "sessionStorage.removeItem('oracle:target-claim');true",
            SHORT,
            &CancellationToken::new(),
        )
        .await?;
    }
    attachment_identity_cases(page)
        .await
        .context("Attachment identity regression")?;
    upload_reset_privacy_cases(page, file)
        .await
        .context("Reset privacy regression")?;
    failed_upload_settlement_cases(page, file)
        .await
        .context("Failed upload settlement regression")?;
    witness_cases(page).await.context("Witness regression")?;
    witness_closure_release_cases(page)
        .await
        .context("Witness closure regression")?;
    witness_route_transition_cases(page)
        .await
        .context("Route-transition regression")?;
    unknown_privacy_send_case(page).await?;
    retirement_marker_cases(page).await?;
    quarantine_guard_disposal_case(page)
        .await
        .context("Quarantine guard disposal regression")?;
    let cancel = CancellationToken::new();
    mount(page, ACTIVE, "/", "").await?;
    require(page, SHORT, &cancel).await?;
    for html in [
        ACTIVE.replace("Turn off temporary chat", "Turn on temporary chat"),
        ACTIVE.replace("<header ", "<header hidden "),
        ACTIVE.replace("<button aria-label", "<button disabled aria-label"),
        ACTIVE.replace(
            "</header>",
            "<button aria-label='Turn on temporary chat'>Ordinary</button></header>",
        ),
        ACTIVE.replace(
            "</header>",
            "<button aria-label='Turn off temporary chat'>Duplicate</button></header>",
        ),
        format!("<article data-message-author-role='assistant'>{ACTIVE}</article>"),
        ACTIVE.replace(
            "data-app-shell-titlebar='true'",
            "data-app-shell-titlebar='false'",
        ),
    ] {
        mount(page, &html, "/", "").await?;
        assert!(
            require(page, UNKNOWN, &cancel).await.is_err(),
            "Accepted unverified fixture: {html}"
        );
    }
    let current_view = "(cfg) => { const root={return:null,stateNode:{}}; root.stateNode.current=root; const props={scrollStateConversationId:'PUBLIC-id',isChatConversation:true,isTemporaryChat:true,isWorkConversation:false,isSharedConversation:false}; document.querySelector('#view').__reactFiber$PUBLIC={return:root,memoizedProps:props}; }";
    for temporary in [false, true] {
        let setup = current_view
            .replace(
                "PUBLIC-id",
                "local-chatgpt:11111111-2222-4333-8444-555555555555",
            )
            .replace(
                "isTemporaryChat:true",
                &format!("isTemporaryChat:{temporary}"),
            );
        mount(
            page,
            ACTIVE,
            "/c/local-chatgpt%3A11111111-2222-4333-8444-555555555555",
            &setup,
        )
        .await?;
        let state = page.evaluate(PROBE, SHORT, &cancel).await?;
        assert_eq!(
            state,
            if temporary { json!(null) } else { json!(false) },
            "Provisional client route authorised privacy"
        );
        assert!(require(page, UNKNOWN, &cancel).await.is_err());
    }
    mount(page, ACTIVE, "/c/PUBLIC-id", current_view).await?;
    require(page, SHORT, &cancel).await?;
    page.evaluate_with_arguments("(cfg)=>{const node=document.querySelector('#view'),old=node.__reactFiber$PUBLIC,root={return:null,stateNode:{}};root.stateNode.current=root;old.return.stateNode.current=root;old.alternate={return:root,memoizedProps:old.memoizedProps};return true;}",&json!([{}]),SHORT,&cancel).await?;
    require(page, SHORT, &cancel).await?;
    for change in [
        "(cfg)=>{document.querySelector('#view').__reactFiber$PUBLIC.memoizedProps.isTemporaryChat=false;}",
        "(cfg)=>{document.querySelector('#view').__reactFiber$PUBLIC.memoizedProps.isSharedConversation=true;}",
        "(cfg)=>{document.querySelector('#view').__reactFiber$PUBLIC.memoizedProps.isWorkConversation=true;}",
        "(cfg)=>{document.querySelector('#view').__reactFiber$PUBLIC.memoizedProps.scrollStateConversationId='PUBLIC-other';}",
        "(cfg)=>{document.querySelector('#view').__reactFiber$PUBLIC.return.stateNode.current={};}",
        "(cfg)=>{const n=document.createElement('div');n.textContent='PUBLIC conflicting view';document.querySelector('main').append(n);const f=document.querySelector('#view').__reactFiber$PUBLIC;n.__reactFiber$PUBLIC={return:f.return,memoizedProps:{...f.memoizedProps,isTemporaryChat:false}};}",
    ] {
        mount(page, ACTIVE, "/c/PUBLIC-id", current_view).await?;
        page.evaluate_with_arguments(
            &format!("(cfg)=>{{ ({change})(cfg); return true; }}"),
            &json!([{}]),
            SHORT,
            &cancel,
        )
        .await?;
        assert!(
            require(page, SHORT, &cancel).await.is_err(),
            "Accepted conflicting or stale React view"
        );
    }
    // The guard is installed whilst Temporary Chat is valid, then the mode changes
    // before the native key event. A preflight-only implementation fails this case.
    mount(page, ACTIVE, "/", "(cfg)=>{window.PUBLIC_sends=0;document.querySelector('form').onsubmit=e=>{e.preventDefault();window.PUBLIC_sends++;};}").await?;
    let mut reservation =
        RendererReservation::acquire(page.clone(), "PUBLIC-claim".into(), SHORT, &cancel).await?;
    let (_, nav) = browser_dom::capture_navigation(&reservation, SHORT, &cancel).await?;
    // CDP changes the FileList before emitting input/change. The capture-phase
    // guard must restore that list before the app can read any file after a flip.
    page.evaluate_with_arguments("(cfg)=>{window.PUBLIC_fileReads=0;document.querySelector('#PUBLIC-file').addEventListener('input',()=>{window.PUBLIC_fileReads+=document.querySelector('#PUBLIC-file').files.length;});document.querySelector('#PUBLIC-file').addEventListener('change',()=>{window.PUBLIC_fileReads+=document.querySelector('#PUBLIC-file').files.length;});return true;}",&json!([{}]),SHORT,&cancel).await?;
    for changed in [false, true] {
        let cfg = json!({"id":"PUBLIC-file-guard","navigation":nav,"selector":"#PUBLIC-file","names":[file.file_name().unwrap().to_str().unwrap()],"fileBytes":std::fs::metadata(file)?.len()});
        let prepared = reservation
            .evaluate_mutation(
                include_str!("../browser-scripts/file-input-prepare.js"),
                cfg.clone(),
                SHORT,
                &cancel,
            )
            .await?;
        anyhow::ensure!(
            prepared["ready"] == true,
            "Fixture file guard was not prepared: {prepared}"
        );
        if changed {
            page.evaluate_with_arguments("(cfg)=>{document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');return true;}",&json!([{}]),SHORT,&cancel).await?;
        }
        let document = page
            .call("DOM.getDocument", json!({}), SHORT, &cancel)
            .await?;
        let input = page
            .call(
                "DOM.querySelector",
                json!({"nodeId":document["root"]["nodeId"],"selector":"#PUBLIC-file"}),
                SHORT,
                &cancel,
            )
            .await?;
        page.call(
            "DOM.setFileInputFiles",
            json!({"nodeId":input["nodeId"],"files":[file]}),
            SHORT,
            &cancel,
        )
        .await?;
        let state=page.evaluate_with_arguments("(cfg)=>{const r=window.__oracleFileInputGuards[cfg.id];const out={blocked:r.guard.blocked,rollbackComplete:r.guard.rollbackComplete,files:r.input.files.length,reads:window.PUBLIC_fileReads};r.guard.cleanup();delete window.__oracleFileInputGuards[cfg.id];r.input.value='';window.PUBLIC_fileReads=0;document.querySelector('header button').setAttribute('aria-label','Turn off temporary chat');return out;}",&json!([cfg]),SHORT,&cancel).await?;
        if changed {
            anyhow::ensure!(
                !state["blocked"].is_null()
                    && state["rollbackComplete"] == true
                    && state["files"] == 0
                    && state["reads"] == 0,
                "Mode flip exposed a file to the app: {state}"
            );
        } else {
            anyhow::ensure!(
                state["blocked"].is_null()
                    && state["files"] == 1
                    && state["reads"].as_u64().is_some_and(|n| n > 0),
                "Valid file event fixture never reached the app: {state}"
            );
        }
    }
    let prompt =
        PreparedPrompt::stage(&mut reservation, "PUBLIC prompt", &nav, &[], SHORT, &cancel).await?;
    page.evaluate_with_arguments("(cfg)=>{const button=document.createElement('button');button.id='composer-plus-btn';button.type='button';button.textContent='Tools';window.PUBLIC_toolClicks=0;button.onclick=()=>window.PUBLIC_toolClicks++;document.querySelector('form').append(button);return true;}",&json!([{}]),SHORT,&cancel).await?;
    let guard_id = page
        .evaluate_with_arguments(
            "(cfg)=>Object.keys(window.__oracleAttachmentPromptGuards)[0]",
            &json!([{}]),
            SHORT,
            &cancel,
        )
        .await?;
    for changed in [false, true] {
        let cfg = json!({"guardId":guard_id,"plusGuardId":"PUBLIC-tools","navigation":nav,"owner":reservation.owner()});
        let probe = crate::browser_search::evaluate(
            &mut reservation,
            include_str!("../browser-scripts/composer-plus-probe.js"),
            &cfg,
            SHORT,
            &cancel,
        )
        .await?;
        anyhow::ensure!(
            probe["focused"] == true,
            "Fixture tools control was not focused: {probe}"
        );
        let prepared = crate::browser_search::evaluate(
            &mut reservation,
            include_str!("../browser-scripts/composer-plus-prepare.js"),
            &cfg,
            SHORT,
            &cancel,
        )
        .await?;
        anyhow::ensure!(
            prepared["focused"] == true,
            "Fixture tools guard was not prepared: {prepared}"
        );
        if changed {
            page.evaluate_with_arguments("(cfg)=>{document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');return true;}",&json!([{}]),SHORT,&cancel).await?;
        }
        for event in ["keyDown", "keyUp"] {
            page.call("Input.dispatchKeyEvent",json!({"type":event,"key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13,"text":if event=="keyDown" {"\r"} else {""}}),SHORT,&cancel).await?;
        }
        let cleaned = page
            .evaluate_with_arguments(
                &crate::browser_search::expression(
                    include_str!("../browser-scripts/composer-plus-clean.js"),
                    false,
                ),
                &json!([cfg]),
                SHORT,
                &cancel,
            )
            .await?;
        let clicks=page.evaluate_with_arguments("(cfg)=>{const count=window.PUBLIC_toolClicks;window.PUBLIC_toolClicks=0;document.querySelector('header button').setAttribute('aria-label','Turn off temporary chat');return count;}",&json!([{}]),SHORT,&cancel).await?;
        assert_eq!(
            clicks,
            usize::from(!changed),
            "Tool callback crossed the privacy boundary: {cleaned}"
        );
        assert_eq!(cleaned["blocked"].is_null(), !changed);
    }
    // Start the real asynchronous copy poll without awaiting it. The button appears
    // only after the privacy mode can change, so a preflight-only guard fails.
    let copy = guard(&format!(
        "(cfg)=>{{window.PUBLIC_copyResult=null;({})(cfg).then(value=>window.PUBLIC_copyResult=value,error=>window.PUBLIC_copyResult={{error:String(error)}});return true;}}",
        include_str!("../browser-scripts/copy-markdown.js")
    ));
    for changed in [false, true] {
        page.evaluate_with_arguments("(cfg)=>{document.querySelector('[data-turn-key]')?.remove();window.PUBLIC_copyClicks=0;return true;}",&json!([{}]),SHORT,&cancel).await?;
        page.evaluate_with_arguments(&copy,&json!([{"owner":reservation.owner(),"receipt":{"conversation":{"origin":"https://chatgpt.com","conversationId":null}},"meta":{"messageId":"PUBLIC-assistant","turnId":"PUBLIC-turn"}}]),SHORT,&cancel).await?;
        page.evaluate_with_arguments(r#"(cfg)=>{if(cfg.changed)document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-turn');turn.innerHTML='<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><button type="button" data-testid="copy-turn-action-button">Copy</button></div>';turn.querySelector('button').onclick=()=>{window.PUBLIC_copyClicks++;navigator.clipboard.writeText('PUBLIC answer');};document.querySelector('main').append(turn);return true;}"#,&json!([{"changed":changed}]),SHORT,&cancel).await?;
        let deadline = tokio::time::Instant::now() + SHORT;
        let result = loop {
            let result = page
                .evaluate_with_arguments(
                    "(cfg)=>window.PUBLIC_copyResult",
                    &json!([{}]),
                    SHORT,
                    &cancel,
                )
                .await?;
            if !result.is_null() {
                break result;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "Copy fixture did not settle"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let clicks=page.evaluate_with_arguments("(cfg)=>{document.querySelector('header button').setAttribute('aria-label','Turn off temporary chat');return window.PUBLIC_copyClicks;}",&json!([{}]),SHORT,&cancel).await?;
        assert_eq!(
            clicks,
            usize::from(!changed),
            "Delayed copy crossed the privacy boundary: {result}"
        );
        if changed {
            assert_eq!(result["status"], "conversation-changed");
        } else {
            assert_eq!(result["success"], true);
        }
    }
    // Multiple actions cannot substitute a different message's Markdown, in either DOM order.
    for historical in [false, true] {
        let copy = if historical {
            format!(
                "(cfg)=>{{window.PUBLIC_copyResult=null;({})(cfg).then(value=>window.PUBLIC_copyResult=value,error=>window.PUBLIC_copyResult={{error:String(error)}});return true;}}",
                include_str!("../browser-scripts/historical-copy-markdown.js")
            )
        } else {
            copy.clone()
        };
        for (html, expected) in [
            (
                r#"<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><button id="correct" data-testid="copy-turn-action-button">Copy</button></div><div data-message-author-role="assistant" data-message-id="PUBLIC-other"><button id="wrong" data-testid="copy-turn-action-button">Copy</button></div>"#,
                true,
            ),
            (
                r#"<div data-message-author-role="assistant" data-message-id="PUBLIC-other"><button id="wrong" data-testid="copy-turn-action-button">Copy</button></div><div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><button id="correct" data-testid="copy-turn-action-button">Copy</button></div>"#,
                true,
            ),
            (
                r#"<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><button id="correct" data-testid="copy-turn-action-button">Copy</button><button id="wrong" data-testid="copy-turn-action-button">Copy</button></div>"#,
                false,
            ),
            (
                r#"<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant">PUBLIC answer</div><button id="correct" data-testid="copy-turn-action-button">Copy</button><button id="wrong" data-testid="copy-turn-action-button">Copy</button>"#,
                false,
            ),
            (
                r#"<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><button id="correct" data-testid="copy-turn-action-button">Copy</button></div><div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><button id="wrong" data-testid="copy-turn-action-button">Copy</button></div>"#,
                false,
            ),
            (
                r#"<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant"><pre><button id="wrong" data-testid="copy-turn-action-button">Copy code</button></pre><button id="correct" data-testid="copy-turn-action-button">Copy</button></div>"#,
                true,
            ),
            (
                r#"<div data-content-search-unit-key="PUBLIC:1:user"><div class="turn-action-controls"><button id="wrong" aria-label="Copy message"></button></div></div><div data-content-search-unit-key="PUBLIC:2:assistant" data-chatgpt-search-message-ids="PUBLIC-assistant PUBLIC-assistant">PUBLIC answer<button id="wrong" aria-label="Copy code"></button></div><div class="turn-action-controls"><button id="correct" aria-label="Copy"></button></div>"#,
                true,
            ),
        ] {
            page.evaluate_with_arguments(r#"(cfg)=>{document.querySelector('[data-turn-key]')?.remove();const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-turn');turn.innerHTML=cfg.html;window.PUBLIC_copyClicks=0;for(const button of turn.querySelectorAll('button'))button.onclick=()=>{window.PUBLIC_copyClicks++;if(button.getAttribute('aria-label')==='Copy')button.setAttribute('aria-label','Copied');navigator.clipboard.writeText(button.id==='correct'?'PUBLIC exact answer':'PUBLIC wrong answer');};document.querySelector('main').append(turn);return true;}"#,&json!([{"html":html}]),SHORT,&cancel).await?;
            let mut cfg = json!({"owner":reservation.owner(),"receipt":{"conversation":{"origin":"https://chatgpt.com","conversationId":null}},"meta":{"messageId":"PUBLIC-assistant","turnId":"PUBLIC-turn"},"captureToken":uuid::Uuid::new_v4().to_string(),"timeoutMs":5000,"action":"prepare"});
            if historical {
                let prepared = page
                    .evaluate_with_arguments(
                        include_str!("../browser-scripts/historical-copy-markdown.js"),
                        &json!([cfg]),
                        SHORT,
                        &cancel,
                    )
                    .await?;
                assert_eq!(prepared["prepared"], true);
            }
            cfg["action"] = json!("run");
            page.evaluate_with_arguments(&copy, &json!([cfg]), SHORT, &cancel)
                .await?;
            let deadline = Instant::now() + SHORT;
            let result = loop {
                let result = page
                    .evaluate_with_arguments(
                        "(cfg)=>window.PUBLIC_copyResult",
                        &json!([{}]),
                        SHORT,
                        &cancel,
                    )
                    .await?;
                if !result.is_null() {
                    break result;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Exact copy fixture did not settle"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            let clicks = page
                .evaluate_with_arguments(
                    "(cfg)=>window.PUBLIC_copyClicks",
                    &json!([{}]),
                    SHORT,
                    &cancel,
                )
                .await?;
            assert_eq!(
                clicks,
                usize::from(expected),
                "Wrong copy action clicked: {result}"
            );
            if expected {
                assert_eq!(result["markdown"], "PUBLIC exact answer");
            } else {
                assert_eq!(result["status"], "ambiguous-copy-control");
            }
        }
    }
    page.evaluate_with_arguments(r#"(cfg)=>{document.querySelector('[data-turn-key]')?.remove();const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-turn');turn.innerHTML='<div data-message-author-role="assistant" data-message-id="PUBLIC-assistant">PUBLIC answer</div>';document.querySelector('main').append(turn);return true;}"#,&json!([{}]),SHORT,&cancel).await?;
    let historical = include_str!("../browser-scripts/historical-copy-markdown.js");
    let mut cfg = json!({"receipt":{"conversation":{"origin":"https://chatgpt.com","conversationId":null}},"meta":{"messageId":"PUBLIC-assistant","turnId":"PUBLIC-turn"},"captureToken":uuid::Uuid::new_v4().to_string(),"timeoutMs":300,"action":"prepare"});
    let prepared = page
        .evaluate_with_arguments(historical, &json!([cfg]), SHORT, &cancel)
        .await?;
    assert_eq!(prepared["prepared"], true);
    cfg["action"] = json!("run");
    let expired = page
        .evaluate_with_arguments(historical, &json!([cfg]), SHORT, &cancel)
        .await?;
    assert_eq!(expired["status"], "missing-button", "{expired}");
    // A Copy still reading 'Copied' from an earlier capture ignores clicks until it resets.
    page.evaluate_with_arguments(r#"(cfg)=>{document.querySelector('[data-turn-key]')?.remove();const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-turn');turn.innerHTML='<div data-content-search-unit-key="PUBLIC:2:assistant" data-chatgpt-search-message-ids="PUBLIC-assistant">PUBLIC answer</div><div class="turn-action-controls"><button aria-label="Copied"></button></div>';const button=turn.querySelector('button');button.onclick=()=>{if(button.getAttribute('aria-label')==='Copy'){button.setAttribute('aria-label','Copied');navigator.clipboard.writeText('PUBLIC reset answer');}};setTimeout(()=>button.setAttribute('aria-label','Copy'),300);document.querySelector('main').append(turn);return true;}"#,&json!([{}]),SHORT,&cancel).await?;
    let mut cfg = json!({"receipt":{"conversation":{"origin":"https://chatgpt.com","conversationId":null}},"meta":{"messageId":"PUBLIC-assistant","turnId":"PUBLIC-turn"},"captureToken":uuid::Uuid::new_v4().to_string(),"timeoutMs":2000,"action":"prepare"});
    let prepared = page
        .evaluate_with_arguments(historical, &json!([cfg]), SHORT, &cancel)
        .await?;
    assert_eq!(prepared["prepared"], true);
    cfg["action"] = json!("run");
    let reset = page
        .evaluate_with_arguments(historical, &json!([cfg]), SHORT, &cancel)
        .await?;
    assert_eq!(reset["markdown"], "PUBLIC reset answer", "{reset}");
    prompt.arm_send(&mut reservation, SHORT, &cancel).await?;
    page.evaluate_with_arguments("(cfg)=>{document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');return true;}", &json!([{}]), SHORT, &cancel).await?;
    reservation.dispatch_started(SHORT, &cancel).await?;
    assert!(
        prompt
            .send_once(&mut reservation, SHORT, &cancel)
            .await
            .is_err()
    );
    let sends = page
        .evaluate_with_arguments("(cfg)=>window.PUBLIC_sends", &json!([{}]), SHORT, &cancel)
        .await?;
    assert_eq!(sends, 0, "Mode flip reached the app's submit handler");
    // Preparation failures still remove only this upload's preflight records and quarantine.
    for setup in [
        "document.querySelector('input').remove();",
        "const form=document.createElement('form');form.innerHTML='<input type=file>';document.body.prepend(form);",
        "const input=document.querySelector('input');Object.defineProperty(input,'type',{get(){document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');return 'file';}});",
    ] {
        mount(page, ACTIVE, "/", "").await?;
        page.evaluate_with_arguments(
            &format!(
                "(cfg)=>{{sessionStorage.removeItem('oracle:target-claim');{setup}return true;}}"
            ),
            &json!([{}]),
            SHORT,
            &cancel,
        )
        .await?;
        let mut failed = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        let mut upload = browser_dom::UploadFile::open(file)?;
        let nav = browser_dom::navigation("https://chatgpt.com/")?;
        assert!(
            browser_dom::upload(&mut failed, &mut upload, &nav, SHORT, &cancel)
                .await
                .is_err()
        );
        let retained=page.evaluate_with_arguments("(cfg)=>({guards:Object.keys(window.__oracleFileInputGuards??{}).length,evidence:window.__oracleAttachmentEvidence?.size??0,state:JSON.parse(sessionStorage.getItem('oracle:target-claim')).submission.state,files:Array.from(document.querySelectorAll('input[type=file]')).reduce((n,input)=>n+input.files.length,0)})",&json!([{}]),SHORT,&cancel).await?;
        assert_eq!(
            retained,
            json!({"guards":0,"evidence":0,"state":"quarantined","files":0})
        );
    }
    Ok(())
}

async fn generic_upload_input_cases(page: &Cdp, file: &std::path::Path) -> Result<()> {
    let cancel = CancellationToken::new();
    let html = ACTIVE.replace(
        "<input type='file' id='PUBLIC-file'>",
        "<input type='file' accept='image/*' id='PUBLIC-camera'><input type='file' accept='image/*,video/*' multiple><input type='file' accept='image/*' multiple><input type='file' accept='' multiple id='PUBLIC-file'>",
    );
    for case in [
        "typed",
        "bare",
        "bare-wrong-name",
        "bare-ambiguous",
        "nested-controls",
        "nested-wrong-name",
        "nested-ambiguous",
        "nested-reset-foreign",
        "nested-replaced-form",
    ] {
        let setup = "(cfg)=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_uploadInputs=[];window.PUBLIC_removeClicks=0;document.querySelector('form').addEventListener('change',event=>{if(event.target.type!=='file')return;window.PUBLIC_uploadInputs.push(event.target.id);if(event.target.id!=='PUBLIC-file')return;for(const file of event.target.files){const chip=document.createElement('div');chip.dataset.testid='attachment-chip';chip.textContent=file.name;const remove=document.createElement('button');remove.type='button';remove.setAttribute('aria-label','Remove file '+file.name);remove.textContent='Remove';remove.onclick=()=>{window.PUBLIC_removeClicks++;chip.remove();};chip.append(remove);document.querySelector('form').append(chip);}});}";
        let setup = if case != "typed" {
            setup
                .replace("chip.dataset.testid='attachment-chip';", "")
                .replace("'Remove file '", "'Remove '")
        } else {
            setup.into()
        };
        let setup = match case {
            "bare-wrong-name" | "nested-wrong-name" => setup.replace("'Remove '+file.name", "'Remove PUBLIC-other.txt'"),
            "bare-ambiguous" | "nested-ambiguous" => setup.replace("chip.append(remove);", "const extra=remove.cloneNode(true);extra.setAttribute('aria-label','Remove PUBLIC-other.txt');chip.append(extra);chip.append(remove);"),
            _ => setup,
        };
        let nested = case.starts_with("nested-");
        let case_html = if nested {
            // The input remains in the same form, outside the editor/send wrapper.
            // Its first attachment makes that smaller wrapper newly match the old heuristic.
            ACTIVE
                .replace("<input type='file' id='PUBLIC-file'>", "")
                .replace(
                    "<form>",
                    "<form><input type='file' id='PUBLIC-file'><div id='PUBLIC-nested'>",
                )
                .replace("</form>", "</div></form>")
        } else {
            html.clone()
        };
        let setup = if nested {
            setup.replace(
                "document.querySelector('form').append(chip);",
                "document.querySelector('#PUBLIC-nested').append(chip);",
            )
        } else {
            setup
        };
        let setup = if case == "nested-replaced-form" {
            setup.replace("document.querySelector('#PUBLIC-nested').append(chip);", "document.querySelector('#PUBLIC-nested').append(chip);const form=document.querySelector('form');form.replaceWith(form.cloneNode(true));")
        } else {
            setup
        };
        mount(page, &case_html, "/", &setup).await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        let mut upload = browser_dom::UploadFile::open(file)?;
        let manifest = browser_dom::upload(
            &mut reservation,
            &mut upload,
            &browser_dom::navigation("https://chatgpt.com/")?,
            if matches!(
                case,
                "typed" | "bare" | "nested-controls" | "nested-reset-foreign"
            ) {
                SHORT
            } else {
                Duration::from_millis(300)
            },
            &cancel,
        )
        .await;
        if !matches!(
            case,
            "typed" | "bare" | "nested-controls" | "nested-reset-foreign"
        ) {
            anyhow::ensure!(
                manifest.is_err(),
                "Unrelated or ambiguous bare attachment control was accepted: {case}"
            );
            anyhow::ensure!(
                reservation.settled_before_dispatch() == (case != "nested-replaced-form"),
                "Rejected bare attachment guard/evidence cleanup did not settle: {case}"
            );
            page.evaluate(
                "sessionStorage.removeItem('oracle:target-claim');true",
                SHORT,
                &cancel,
            )
            .await?;
            continue;
        }
        let manifest = manifest.with_context(|| format!("PUBLIC generic upload case {case}"))?;
        assert_eq!(manifest["name"], upload.name);
        let selected = page
            .evaluate("window.PUBLIC_uploadInputs", SHORT, &cancel)
            .await?;
        assert_eq!(selected, json!(["PUBLIC-file"]));
        assert_eq!(
            reservation.state(),
            crate::browser_renderer::DispatchState::NotStarted
        );
        let nav = browser_dom::navigation("https://chatgpt.com/")?;
        if case == "nested-controls" {
            let prepared = PreparedPrompt::stage(
                &mut reservation,
                "PUBLIC nested attachment preparation",
                &nav,
                &[browser_dom::receipt_expectation(&manifest, false)?],
                SHORT,
                &cancel,
            )
            .await?;
            prepared.arm_send(&mut reservation, SHORT, &cancel).await?;
            reservation
                .evaluate_mutation(
                    "()=>{window.__oracleAttachmentDispatchGuard.cleanup();return true;}",
                    json!({}),
                    SHORT,
                    &cancel,
                )
                .await?;
            prepared.cleanup_witness(&reservation, false, SHORT).await?;
            prepared.cleanup(&mut reservation, SHORT).await?;
        }
        if case == "nested-reset-foreign" {
            reservation.evaluate_mutation(
                "()=>{const extra=document.createElement('button');extra.type='button';extra.setAttribute('aria-label','Remove PUBLIC-foreign.txt');extra.textContent='Foreign';document.querySelector('form').append(extra);return true;}",
                json!({}), SHORT, &cancel,
            ).await?;
        }
        let reset =
            browser_dom::reset_uploads(&mut reservation, &[manifest], &nav, SHORT, &cancel).await;
        if case == "nested-reset-foreign" {
            anyhow::ensure!(
                reset.is_err(),
                "Reset accepted an unowned bare removal control"
            );
            assert_eq!(
                page.evaluate("window.PUBLIC_removeClicks", SHORT, &cancel)
                    .await?,
                0
            );
            page.evaluate(
                "sessionStorage.removeItem('oracle:target-claim');true",
                SHORT,
                &cancel,
            )
            .await?;
            continue;
        }
        reset?;
        assert_eq!(page.evaluate("({clicks:window.PUBLIC_removeClicks,files:document.querySelector('#PUBLIC-file').files.length,evidence:window.__oracleAttachmentEvidence.size})", SHORT, &cancel).await?, json!({"clicks":1,"files":0,"evidence":0}));
        reservation.release_clean(SHORT).await?;
        page.evaluate(
            "sessionStorage.removeItem('oracle:target-claim');true",
            SHORT,
            &cancel,
        )
        .await?;
    }
    Ok(())
}

/// Reference clearStaleChatGptConversationCookies on installed Chrome: only the unused
/// synthetic conversation cookie of this disposable profile is deleted.
async fn stale_conversation_cookie_case(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    for name in ["conv_key_PUBLIC-stale", "conv_key_PUBLIC-kept"] {
        let set = page
            .call(
                "Network.setCookie",
                json!({"name":name,"value":"PUBLIC-cookie-value","url":"https://chatgpt.com/","secure":true}),
                SHORT,
                &cancel,
            )
            .await?;
        anyhow::ensure!(set["success"] == true, "Synthetic cookie was not stored");
    }
    let lines = crate::browser_auth::clear_stale_conversation_cookies(
        &page.browser_scope(),
        page,
        &[Some("PUBLIC-kept".into())],
        SHORT,
        &cancel,
    )
    .await;
    anyhow::ensure!(
        lines == ["[cookies] Cleared 1 stale ChatGPT conversation cookie."],
        "{lines:?}"
    );
    let names = |reply: Value| -> Vec<String> {
        reply["cookies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|cookie| cookie["name"].as_str().map(str::to_owned))
            .collect()
    };
    let remaining = names(
        page.call(
            "Network.getCookies",
            json!({"urls":["https://chatgpt.com/"]}),
            SHORT,
            &cancel,
        )
        .await?,
    );
    anyhow::ensure!(remaining == ["conv_key_PUBLIC-kept"], "{remaining:?}");
    page.call(
        "Network.deleteCookies",
        json!({"name":"conv_key_PUBLIC-kept","url":"https://chatgpt.com/"}),
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

/// Reference activateComposerPlus: the composer mounts its file input only after one trusted
/// '+' activation, and the upload then uses that input exactly once.
/// Remote hosts receive bytes through the page. Two chunks prove ordered reassembly, and the
/// counted native setter proves no local path was handed to the browser.
async fn data_transfer_upload_case(page: &Cdp, dir: &std::path::Path) -> Result<()> {
    let cancel = CancellationToken::new();
    let path = dir.join("PUBLIC-transfer.txt");
    let content = "PUBLIC transfer bytes ".repeat(40_000);
    std::fs::write(&path, &content)?;
    let setup = "(cfg)=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_setterCalls=0;window.PUBLIC_received=null;const descriptor=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'files');window.PUBLIC_filesDescriptor=descriptor;Object.defineProperty(HTMLInputElement.prototype,'files',{configurable:true,get(){return descriptor.get.call(this);},set(value){window.PUBLIC_setterCalls++;descriptor.set.call(this,value);}});const form=document.querySelector('form');form.addEventListener('change',event=>{if(event.target.type!=='file')return;const file=event.target.files[0];file.text().then(text=>{window.PUBLIC_received={name:file.name,type:file.type,size:file.size,text};});const chip=document.createElement('div');chip.dataset.testid='attachment-chip';chip.textContent=file.name;const remove=document.createElement('button');remove.type='button';remove.setAttribute('aria-label','Remove file '+file.name);remove.textContent='Remove';chip.append(remove);form.append(chip);});}";
    let restore = "Object.defineProperty(HTMLInputElement.prototype,'files',window.PUBLIC_filesDescriptor);sessionStorage.removeItem('oracle:target-claim');true";
    let result = async {
        mount(page, ACTIVE, "/", setup).await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        let mut upload = browser_dom::UploadFile::open(&path)?;
        upload.data_transfer = true;
        let nav = browser_dom::navigation("https://chatgpt.com/")?;
        let manifest = browser_dom::upload(&mut reservation, &mut upload, &nav, SHORT, &cancel)
            .await
            .context("PUBLIC data-transfer upload")?;
        assert_eq!(manifest["bytes"], content.len());
        let deadline = tokio::time::Instant::now() + SHORT;
        let received = loop {
            let received = page.evaluate("window.PUBLIC_received", SHORT, &cancel).await?;
            if !received.is_null() {
                break received;
            }
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "Transferred file was not readable");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            received,
            json!({"name":"PUBLIC-transfer.txt","type":"text/plain","size":content.len(),"text":content})
        );
        assert_eq!(page.evaluate("window.PUBLIC_setterCalls", SHORT, &cancel).await?, 1);
        assert_eq!(
            reservation.state(),
            crate::browser_renderer::DispatchState::NotStarted
        );
        reservation.release_clean(SHORT).await?;
        page.evaluate("sessionStorage.removeItem('oracle:target-claim');true", SHORT, &cancel)
            .await?;
        // The reference cap is refused before any page work.
        let large = dir.join("PUBLIC-large.bin");
        std::fs::File::create(&large)?.set_len(20 * 1024 * 1024 + 1)?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        let mut upload = browser_dom::UploadFile::open(&large)?;
        upload.data_transfer = true;
        let refused = browser_dom::upload(&mut reservation, &mut upload, &nav, SHORT, &cancel)
            .await
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "Attachment PUBLIC-large.bin is too large for data transfer (20971521 bytes). Maximum size is 20971520 bytes."
        );
        assert_eq!(
            reservation.state(),
            crate::browser_renderer::DispatchState::NotStarted
        );
        assert_eq!(page.evaluate("window.PUBLIC_setterCalls", SHORT, &cancel).await?, 1);
        // A clean release proves the refusal left no upload work or quarantine behind.
        reservation.release_clean(SHORT).await?;
        Ok(())
    }
    .await;
    page.evaluate(restore, SHORT, &cancel).await?;
    result
}
async fn plus_menu_upload_case(page: &Cdp, file: &std::path::Path) -> Result<()> {
    let cancel = CancellationToken::new();
    let html = ACTIVE.replace(
        "<input type='file' id='PUBLIC-file'>",
        "<button type='button' id='composer-plus-btn' aria-label='Add files and more'>+</button>",
    );
    let setup = "(cfg)=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_uploadInputs=[];window.PUBLIC_plusClicks=0;const form=document.querySelector('form');document.querySelector('#composer-plus-btn').addEventListener('click',()=>{window.PUBLIC_plusClicks++;if(!document.querySelector('#PUBLIC-file')){const input=document.createElement('input');input.type='file';input.id='PUBLIC-file';form.append(input);}});form.addEventListener('change',event=>{if(event.target.type!=='file')return;window.PUBLIC_uploadInputs.push(event.target.id);for(const file of event.target.files){const chip=document.createElement('div');chip.dataset.testid='attachment-chip';chip.textContent=file.name;const remove=document.createElement('button');remove.type='button';remove.setAttribute('aria-label','Remove file '+file.name);remove.textContent='Remove';chip.append(remove);form.append(chip);}});}";
    mount(page, &html, "/", setup).await?;
    let mut reservation = RendererReservation::acquire(
        page.clone(),
        uuid::Uuid::new_v4().to_string(),
        SHORT,
        &cancel,
    )
    .await?;
    let mut upload = browser_dom::UploadFile::open(file)?;
    let manifest = browser_dom::upload(
        &mut reservation,
        &mut upload,
        &browser_dom::navigation("https://chatgpt.com/")?,
        SHORT,
        &cancel,
    )
    .await
    .context("PUBLIC composer plus upload")?;
    assert_eq!(manifest["name"], upload.name);
    assert_eq!(
        page.evaluate(
            "({plus:window.PUBLIC_plusClicks,inputs:window.PUBLIC_uploadInputs})",
            SHORT,
            &cancel
        )
        .await?,
        json!({"plus":1,"inputs":["PUBLIC-file"]})
    );
    assert_eq!(
        reservation.state(),
        crate::browser_renderer::DispatchState::NotStarted
    );
    reservation.release_clean(SHORT).await?;
    page.evaluate(
        "sessionStorage.removeItem('oracle:target-claim');true",
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

/// The ChatGPT composer and tools menu captured live on 27 October 2026 (labels, roles,
/// attributes and nesting kept; Tailwind classes trimmed except the row content class).
/// Each menu button holds a row div whose class contains 'menu-item' and whose text is
/// the button's label, and selecting Web search appends an inline pill and one space.
const SEARCH_COMPOSER: &str = "<main><header data-app-shell-titlebar='true'><button aria-label='Turn off temporary chat'>Temporary</button></header><form data-chatgpt-composer='' data-composer-placement='home'><div contenteditable='true' role='textbox' aria-label='Ask ChatGPT' aria-multiline='true' class='ProseMirror' data-composer-markdown=''><p><br class='ProseMirror-trailingBreak'></p></div><button type='button' aria-label='Add files and more' aria-expanded='false' data-composer-navigation-target='add-context' data-state='closed'>+</button><button type='button' id='radix-PUBLIC' aria-label='Select ChatGPT model' aria-haspopup='menu' aria-expanded='false' data-state='closed'>Thinking effort</button><button type='submit' data-testid='send-button' aria-label='Send'>Send</button></form></main>";
const SEARCH_MENU_SETUP: &str = r#"(cfg)=>{
  sessionStorage.removeItem('oracle:target-claim');
  window.PUBLIC_searchClicks=0;
  const plus=document.querySelector('form button[aria-label="Add files and more"]');
  const row=(label,description)=>`<button class="text-default outline-hidden cursor-interaction flex w-full shrink-0 items-center" type="button" data-list-navigation-item="true"><div data-menu-row-content="true" class="flex w-full min-w-0 items-center gap-[var(--spacing-menu-item-content,calc(var(--spacing)*1.5))]"><span data-leading-icon-slot="menu" class="inline-flex flex-none items-center justify-center"></span><span class="flex-1 min-w-0 truncate"><span class="flex min-w-0 items-center gap-menu-row-content"><span class="min-w-0 truncate shrink-0">${label}</span><span class="min-w-0 flex-1 truncate text-sm text-codex-description">${description}</span></span></span></div></button>`;
  plus.addEventListener('click',()=>{
    if(document.querySelector('[class*="ComposerTopMenuShell"]'))return;
    plus.setAttribute('data-state','open');plus.setAttribute('aria-expanded','true');
    const host=document.createElement('div');
    host.innerHTML=`<div><div data-composer-overlay-floating-ui="true" class="ComposerTopMenuShell-ZaKxCC max-h-[inherit]"><div class="flex flex-col overflow-hidden"><div class="vertical-scroll-fade-mask flex w-full flex-1 flex-col overflow-y-auto" data-mention-list-scroll-area=""><div data-mention-section-id="chatgpt-primary"><div data-mention-section-items="true">${row('Add photos &amp; files','Upload from computer')}${row('Add from library','Browse and search your files')}${row('Web search','Find real-time news and info')}</div></div><div data-mention-section-id="plugins"><div data-mention-section-items="true">${row('Data','Answer questions with data')}</div></div></div></div></div></div>`;
    document.body.append(host);
    const search=Array.from(host.querySelectorAll('button')).find(node=>node.textContent==='Web searchFind real-time news and info');
    search.addEventListener('click',()=>{
      window.PUBLIC_searchClicks++;
      host.remove();plus.setAttribute('data-state','closed');plus.setAttribute('aria-expanded','false');
      const pill=document.createElement('span');
      for(const [name,value] of [['data-inline-selection-pill',''],['data-system-hint-type','search'],['data-system-hint-label','Web search'],['data-prompt-link-href','chatgpt-system-hint://search'],['data-prompt-link-label','Web search'],['class','Mention-hIgCkC'],['data-appearance','inline-mention'],['contenteditable','false']])pill.setAttribute(name,value);
      pill.innerHTML='<span class="IconContainer-K62H88" contenteditable="false"></span><span class="Label-arpLwJ">Web search</span>';
      document.querySelector('form [contenteditable="true"] p').append(pill,document.createTextNode(' '));
    });
  });
}"#;

async fn web_search_captured_menu_case(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    mount(page, SEARCH_COMPOSER, "/", SEARCH_MENU_SETUP).await?;
    let mut reservation = RendererReservation::acquire(
        page.clone(),
        uuid::Uuid::new_v4().to_string(),
        SHORT,
        &cancel,
    )
    .await?;
    let mut prompt = PreparedPrompt::stage(
        &mut reservation,
        "PUBLIC web search prompt",
        &browser_dom::navigation("https://chatgpt.com/")?,
        &[],
        SHORT,
        &cancel,
    )
    .await?;
    let selected = prompt
        .activate_search(&mut reservation, SHORT, &cancel)
        .await
        .context("Captured Web search menu")?;
    assert!(selected, "Web search was reported as already selected");
    assert_eq!(
        page.evaluate("({clicks:window.PUBLIC_searchClicks,pills:document.querySelectorAll('form [data-inline-selection-pill][data-system-hint-type=\"search\"]').length,menus:document.querySelectorAll('[class*=\"ComposerTopMenuShell\"]').length})", SHORT, &cancel)
            .await?,
        json!({"clicks":1,"pills":1,"menus":0})
    );
    // The send guard re-reads the staged prompt with the pill and its space present.
    prompt.arm_send(&mut reservation, SHORT, &cancel).await?;
    reservation
        .evaluate_mutation(
            "()=>{window.__oracleAttachmentDispatchGuard.cleanup();return true;}",
            json!({}),
            SHORT,
            &cancel,
        )
        .await?;
    assert_eq!(
        reservation.state(),
        crate::browser_renderer::DispatchState::NotStarted
    );
    prompt.cleanup_witness(&reservation, false, SHORT).await?;
    prompt.cleanup(&mut reservation, SHORT).await?;
    reservation.release_clean(SHORT).await?;
    page.evaluate(
        "sessionStorage.removeItem('oracle:target-claim');true",
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

/// The unified ChatGPT picker as captured on 27 October 2026 (classes removed): a simple view
/// with the Power slider and an inert model view whose radios keep their layout. Selecting
/// GPT-6's top Power step, labelled Pro, also checks the Pro radio. Updates happen in place,
/// as React's do, so retained-node checks see one control.
const PRO_PICKER_SETUP: &str = r#"(cfg)=>{
  sessionStorage.removeItem('oracle:target-claim');
  const state=window.PUBLIC_picker=Object.assign({model:'GPT-6',level:2,max:4},cfg);
  const names=['Light','Medium','High','Extra High','Pro'];
  const trigger=document.createElement('button');
  trigger.type='button';
  trigger.id='PUBLIC-trigger';
  for(const [name,value] of Object.entries({'aria-label':'Select ChatGPT model','aria-haspopup':'menu','aria-expanded':'false','data-state':'closed','data-codex-intelligence-trigger':'true'}))trigger.setAttribute(name,value);
  trigger.innerHTML='<span>Thinking effort</span><span aria-hidden="true"></span>';
  document.querySelector('form').append(trigger);
  const radio=(label)=>`<div role="menuitemradio" aria-checked="false" tabindex="-1" data-orientation="vertical" data-radix-collection-item=""${label==='Pro'&&state.max<4?' aria-disabled="true" data-disabled=""':''}><div data-menu-row-content="true"><span>${label}</span></div></div>`;
  let wrapper=null;
  const close=()=>{wrapper?.remove();wrapper=null;trigger.setAttribute('aria-expanded','false');trigger.setAttribute('data-state','closed');};
  const update=()=>{
    const name=names[state.level];
    trigger.lastChild.textContent=name;
    if(!wrapper)return;
    wrapper.querySelector('[role=status]').textContent=`${name}, ${state.level+1} of ${state.max+1}.`;
    wrapper.querySelector('[data-effort-only]').textContent=name;
    wrapper.querySelector('[role=slider]').setAttribute('aria-valuenow',String(state.level));
    for(const row of wrapper.querySelectorAll('[role=menuitemradio]')){
      const checked=row.textContent===state.model;
      row.setAttribute('aria-checked',String(checked));
      if(checked)row.setAttribute('data-model-selected','true');else row.removeAttribute('data-model-selected');
    }
  };
  const view=(simple)=>{
    wrapper.querySelector('[data-model-picker-view]').setAttribute('data-model-picker-view',simple?'simple':'advanced');
    for(const [node,active] of [[wrapper.querySelector('#PUBLIC-simple'),simple],[wrapper.querySelector('#PUBLIC-models'),!simple]]){
      node.setAttribute('aria-hidden',String(!active));
      node.setAttribute('data-active',String(active));
      if(active)node.removeAttribute('inert');else node.setAttribute('inert','');
      for(const inner of node.querySelectorAll('[data-active]'))inner.setAttribute('data-active',String(active));
    }
    const power=wrapper.querySelector('[data-reasoning-slider]');
    if(simple){power.removeAttribute('aria-disabled');power.removeAttribute('data-disabled');}
    else{power.setAttribute('aria-disabled','true');power.setAttribute('data-disabled','');}
  };
  const open=()=>{
    wrapper=document.createElement('div');
    wrapper.setAttribute('data-radix-popper-content-wrapper','');
    wrapper.innerHTML=`<style>[role=menu] [role=menuitem],[role=menu] [role=menuitemradio],[role=slider]{display:block;min-width:40px;min-height:10px}</style><div data-side="bottom" data-align="center" role="menu" aria-orientation="vertical" data-state="open" data-radix-menu-content="" dir="ltr" id="PUBLIC-menu" aria-labelledby="PUBLIC-trigger" tabindex="-1" data-orientation="vertical"><div data-reduced-motion="false" data-transitions-ready="true" data-model-picker-view="simple"><div id="PUBLIC-simple" aria-hidden="false" data-active="true"><div><div data-active="true"><div data-explicit-model="false" data-inline-model="false" data-effort-header="true" data-ultra-warning-visible="false"><div role="menuitem" aria-label="Select model" data-interactive="true" data-model-picker-view-toggle="true" tabindex="0" data-orientation="vertical" data-radix-collection-item=""><div data-menu-row-content="true"><span><span><span data-power-disabled="false" data-accent="false" data-effort-only="true" data-maximum="false"></span></span></span></div></div></div></div><div data-active="true"><div><span aria-live="polite" id="PUBLIC-status" role="status"></span><span id="PUBLIC-hint">Use Left and Right arrow keys to adjust power</span><div role="menuitem" aria-describedby="PUBLIC-status PUBLIC-hint" aria-keyshortcuts="ArrowLeft ArrowRight" aria-label="Power" data-reasoning-slider="true" tabindex="-1" data-orientation="vertical" data-radix-collection-item=""><div data-menu-row-content="true"><span><div data-keyboard-focused="false" data-model-picker-power-slider=""><span dir="ltr" data-orientation="horizontal" aria-disabled="false" data-locked="false"><span><span role="slider" aria-valuemin="0" aria-valuemax="${state.max}" aria-orientation="horizontal" data-orientation="horizontal" tabindex="-1" aria-hidden="true" data-radix-collection-item="" aria-valuenow="0"></span></span></span></div></span></div></div></div></div></div></div><div id="PUBLIC-models" aria-hidden="true" data-active="false" inert=""><div><div data-active="false"><div>${radio('GPT-6')}${radio('GPT-5.6 Sol')}${radio('Pro')}</div></div></div></div></div></div>`;
    document.body.append(wrapper);
    trigger.setAttribute('aria-expanded','true');
    trigger.setAttribute('data-state','open');
    update();
    wrapper.querySelector('[data-model-picker-view-toggle]').addEventListener('click',()=>view(false));
    for(const row of wrapper.querySelectorAll('[role=menuitemradio]'))row.addEventListener('click',()=>{
      if(row.getAttribute('aria-disabled')==='true')return;
      state.model=row.textContent;
      if(state.model==='Pro')state.level=4;
      else if(state.level===4)state.level=3;
      update();
      close();
    });
    wrapper.querySelector('[data-reasoning-slider]').addEventListener('keydown',(event)=>{
      const step={ArrowRight:1,ArrowLeft:-1}[event.key];
      if(!step||state.model==='GPT-5.6 Sol')return;
      state.level=Math.max(0,Math.min(state.max,state.level+step));
      state.model=state.level===4?'Pro':'GPT-6';
      update();
    });
  };
  update();
  trigger.addEventListener('click',()=>wrapper?close():open());
  document.addEventListener('keydown',(event)=>{if(event.key==='Escape')close();});
}"#;

/// Mapping A (27 October 2026): Pro is GPT-6's top Power step, which also checks the Pro radio.
async fn pro_model_picker_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    let picker = async |initial: Value| -> Result<RendererReservation> {
        mount(page, ACTIVE, "/", "()=>{}").await?;
        page.evaluate_with_arguments(
            &format!("(cfg)=>{{ ({PRO_PICKER_SETUP})(cfg); return true; }}"),
            &json!([initial]),
            SHORT,
            &cancel,
        )
        .await?;
        RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await
    };
    let state = async || {
        page.evaluate(
            "({model:window.PUBLIC_picker.model,level:window.PUBLIC_picker.level,open:Boolean(document.querySelector('[role=menu]'))})",
            SHORT,
            &cancel,
        )
        .await
    };
    // Pro available: GPT-6 High steps up to Pro, which checks the Pro radio.
    // Pro locked: the four-step slider has no Pro, so GPT-6 Extra High.
    for (max, effective, model, level) in
        [(4, "gpt-6-pro", "Pro", 4), (3, "gpt-6-astra", "GPT-6", 3)]
    {
        let mut reservation = picker(json!({"level":2,"max":max})).await?;
        let (_, thinking, chosen) = reservation
            .select_pair("select", "GPT-6", SHORT, &cancel)
            .await
            .with_context(|| format!("Select pair with a {}-step slider", max + 1))?;
        anyhow::ensure!(chosen == effective, "{max}: {chosen}");
        let thinking = serde_json::to_value(&thinking)?;
        anyhow::ensure!(thinking["observedModelLabel"] == model, "{thinking}");
        let after = state().await?;
        anyhow::ensure!(
            after == json!({"model":model,"level":level,"open":false}),
            "{max}: {after}"
        );
        reservation.release_clean(SHORT).await?;
    }
    // The Pro radio counts only whilst the slider reads Pro, and only for a Pro target.
    for (model, level, target, status) in [
        ("Pro", 4, "pro", "ready"),
        ("Pro", 4, "extra-high", "selection_unknown"),
        ("Pro", 3, "pro", "selection_unknown"),
        ("GPT-5.6 Sol", 4, "pro", "selection_unknown"),
    ] {
        let mut reservation = picker(json!({"model":model,"level":level})).await?;
        let observed = reservation
            .inspect_current_effort(target, SHORT, &cancel)
            .await?;
        anyhow::ensure!(
            observed["status"] == status,
            "{model} at {level} for {target}: {observed}"
        );
        reservation.release_clean(SHORT).await?;
    }
    page.evaluate(
        "sessionStorage.removeItem('oracle:target-claim');true",
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

/// ChatGPT's October 2026 answer renderer (observed 27 October 2026): the inline file link is a
/// `role=link` span with no URL, and the file card names its file only through the card's
/// 'Open preview of <name>' button beside a bare 'Download file' button.
async fn file_card_candidate_cases(page: &Cdp) -> Result<()> {
    let card = |name: &str| {
        format!(
            r#"<span><button type="button" aria-busy="false" aria-label="Open preview of {name}"></button><span><span><span title="{name}">{name}</span><span><span>Document</span></span></span><span><span><button type="button" aria-label="Download file"></button></span></span></span></span>"#
        )
    };
    let files_beside = async |cards: &str, other_turn: &str| -> Result<Value> {
        let html = format!(
            r#"<div><div data-content-search-unit-key="PUBLIC:2:assistant" id="PUBLIC-unit"><p><span role="link" tabindex="0" aria-label="Open Download PUBLIC-L5d.txt"><span>Download</span> <span>PUBLIC-L5d.txt</span></span></p><div>{cards}</div></div><div data-content-search-unit-key="PUBLIC:4:assistant">{other_turn}</div></div>"#
        );
        mount(page, &html, "/c/PUBLIC-files?temporary-chat=true", "").await?;
        page.evaluate(
            &format!(
                "(() => {{ {} ; return serializeFiles(document.getElementById('PUBLIC-unit'), 3).map(file => ({{filename: file.filename, url: file.url, candidateId: file.candidateId}})); }})()",
                include_str!("../browser-scripts/artifact-prelude.js")
            ),
            SHORT,
            &CancellationToken::new(),
        )
        .await
    };
    let files = async |cards: &str| files_beside(cards, "").await;
    // One card: its Download file button is the only candidate, named by the card.
    let one = files(&card("PUBLIC-L5d.txt")).await?;
    anyhow::ensure!(
        one.as_array().map(Vec::len) == Some(1)
            && one[0]["filename"] == "PUBLIC-L5d.txt"
            && one[0]["url"] == "browser-download"
            && one[0]["candidateId"]
                .as_str()
                .is_some_and(|id| id.starts_with("3:")),
        "single file card: {one}"
    );
    // Two cards: each button takes its own card's name.
    let two = files(&(card("PUBLIC-a.txt") + &card("PUBLIC-b.csv"))).await?;
    anyhow::ensure!(
        two.as_array().map(Vec::len) == Some(2)
            && two[0]["filename"] == "PUBLIC-a.txt"
            && two[1]["filename"] == "PUBLIC-b.csv",
        "two file cards: {two}"
    );
    // A Download file button whose nearest card holds two previews has no single name.
    let ambiguous = files(r#"<span><button aria-label="Open preview of PUBLIC-a.txt"></button><button aria-label="Open preview of PUBLIC-b.txt"></button><span><button aria-label="Download file"></button></span></span>"#).await?;
    anyhow::ensure!(ambiguous == json!([]), "ambiguous card: {ambiguous}");
    // A card name that is a path is refused.
    let path = files(&card("PUBLIC/escape.txt")).await?;
    anyhow::ensure!(path == json!([]), "path name: {path}");
    // An orphan Download file button never borrows a sibling card's or another answer's name.
    let orphan = files(
        &(card("PUBLIC-a.txt") + r#"<span><button aria-label="Download file"></button></span>"#),
    )
    .await?;
    anyhow::ensure!(
        orphan.as_array().map(Vec::len) == Some(1) && orphan[0]["filename"] == "PUBLIC-a.txt",
        "orphan beside a card: {orphan}"
    );
    let foreign = files_beside(
        r#"<span><button aria-label="Download file"></button></span>"#,
        &card("PUBLIC-other.txt"),
    )
    .await?;
    anyhow::ensure!(
        foreign == json!([]),
        "orphan beside another answer's card: {foreign}"
    );
    Ok(())
}

/// ChatGPT's October 2026 renderer can show the finished-turn controls while a text block still
/// holds raw streamed text (observed 27 October 2026: a capture held only '[Download PUBLIC-L').
async fn unrendered_completion_cases(page: &Cdp) -> Result<()> {
    // Each finished-turn proof: the legacy Copy, a file card, and the October action bar's Copy
    // reading 'Copied' straight after a click, outside the answer unit.
    let complete = async |paragraph: &str, proof: &str| -> Result<Value> {
        let (card, bar) = match proof {
            "card" => (
                r#"<span><button aria-busy="false" aria-label="Open preview of PUBLIC.txt"></button><button aria-label="Download file"></button></span>"#,
                "",
            ),
            "copied" => (
                "",
                r#"<div class="turn-action-controls"><button aria-label="Copied"></button></div>"#,
            ),
            _ => (
                r#"<button data-testid="copy-turn-action-button">Copy</button>"#,
                "",
            ),
        };
        let html = format!(
            r#"<main><div data-turn-key="PUBLIC-turn"><div data-content-search-unit-key="PUBLIC:0:user">PUBLIC prompt</div><div data-content-search-unit-key="PUBLIC:2:assistant" data-chatgpt-search-message-ids="PUBLIC-assistant"><div data-chatgpt-selection-message-id="PUBLIC-assistant"><div data-dil-message-id="PUBLIC-assistant">{paragraph}</div></div>{card}</div>{bar}</div></main>"#
        );
        mount(page, &html, "/c/PUBLIC-render?temporary-chat=true", "").await?;
        // Live completion, then harvest's inspection of the same page.
        page.evaluate(
            &format!(
                "(() => {{ {}\n{}\n return [hasTurnCompletion({{messageId: 'PUBLIC-assistant'}}, -1, false), ({})({{expectedConversationId: 'PUBLIC-render', expectedOrigin: location.origin}}).completionVisible]; }})()",
                include_str!("../browser-scripts/ownership-prelude.js"),
                include_str!("../browser-scripts/result-prelude.js"),
                include_str!("../browser-scripts/historical-inspection.js")
            ),
            SHORT,
            &CancellationToken::new(),
        )
        .await
    };
    let rendered = r#"<p data-d-text-flow="" data-d-component="text"><span data-d-text-flow="word" data-d-stream-word="">Download</span> <span data-d-text-flow="word" data-d-stream-word="">PUBLIC.txt</span></p><p data-d-text-flow="" data-d-component="text"><code data-d-component="code"><span data-d-text-flow="word">PUBLIC-ok</span></code></p>"#;
    let raw = r#"<p data-d-text-flow="" data-d-component="text">[Download PUBLIC-L</p>"#;
    for proof in ["copy", "card", "copied"] {
        let done = complete(rendered, proof).await?;
        anyhow::ensure!(
            done == json!([true, true]),
            "rendered answer ({proof}): {done}"
        );
        let pending = complete(raw, proof).await?;
        anyhow::ensure!(
            pending == json!([false, false]),
            "raw streamed text ({proof}): {pending}"
        );
    }
    Ok(())
}

async fn deferred_editor_render_case(page: &Cdp) -> Result<()> {
    // October 2026: straight after insertText, ChatGPT's editor shows a blank line as a bare <br>
    // paragraph, which reads as an extra newline until it re-renders with a trailing break.
    let cancel = CancellationToken::new();
    let html = ACTIVE.replace(
        "<form><textarea id='prompt-textarea' name='prompt-textarea'></textarea>",
        "<form data-chatgpt-composer><div contenteditable='true' role='textbox'></div>",
    );
    mount(page, &html, "/", r#"()=>{sessionStorage.removeItem('oracle:target-claim');const editor=document.querySelector('[role="textbox"]');let timer;editor.addEventListener('input',()=>{if(!editor.textContent.trim())return;const render=br=>'PUBLIC brief.\n\nPUBLIC question.'.split('\n').map(line=>`<p>${line||br}</p>`).join('');editor.innerHTML=render('<br>');clearTimeout(timer);timer=setTimeout(()=>{editor.innerHTML=render('<br class="ProseMirror-trailingBreak">');},100);});}"#).await?;
    let mut reservation =
        RendererReservation::acquire(page.clone(), "PUBLIC-deferred".into(), SHORT, &cancel)
            .await?;
    let (_, nav) = browser_dom::capture_navigation(&reservation, SHORT, &cancel).await?;
    let prepared = PreparedPrompt::stage(
        &mut reservation,
        "PUBLIC brief.\n\nPUBLIC question.",
        &nav,
        &[],
        SHORT,
        &cancel,
    )
    .await
    .context("Blank-line prompt was refused before the editor re-rendered")?;
    prepared.cleanup_witness(&reservation, false, SHORT).await?;
    prepared.cleanup(&mut reservation, SHORT).await
}

async fn upload_reset_privacy_cases(page: &Cdp, file: &std::path::Path) -> Result<()> {
    let cancel = CancellationToken::new();
    let other = tempfile::tempdir()?;
    let second = other.path().join("PUBLIC-second.txt");
    std::fs::write(&second, "PUBLIC second upload")?;
    for case in ["inactive", "unknown", "navigation"] {
        mount(page, ACTIVE, "/", "()=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_removeClicks=0;document.querySelector('form').addEventListener('change',event=>{if(event.target.type!=='file')return;for(const file of event.target.files){const chip=document.createElement('div');chip.dataset.testid='attachment-chip';chip.textContent=file.name;const remove=document.createElement('button');remove.type='button';remove.setAttribute('aria-label','Remove file '+file.name);remove.textContent='Remove';remove.onclick=()=>{window.PUBLIC_removeClicks++;chip.remove();if(window.PUBLIC_removeClicks===1){if(window.PUBLIC_resetCase==='inactive')document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');if(window.PUBLIC_resetCase==='unknown')document.querySelector('header').remove();if(window.PUBLIC_resetCase==='navigation')history.replaceState(null,'','/?PUBLIC-changed=1');}};chip.append(remove);document.querySelector('form').append(chip);}});}").await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        let nav = browser_dom::navigation("https://chatgpt.com/")?;
        let mut manifests = Vec::new();
        for path in [file, second.as_path()] {
            let mut upload = browser_dom::UploadFile::open(path)?;
            manifests.push(
                browser_dom::upload(&mut reservation, &mut upload, &nav, SHORT, &cancel).await?,
            );
        }
        page.evaluate_with_arguments(
            "(cfg)=>{window.PUBLIC_resetCase=cfg.case;return true;}",
            &json!([{"case":case}]),
            SHORT,
            &cancel,
        )
        .await?;
        assert!(
            browser_dom::reset_uploads(&mut reservation, &manifests, &nav, SHORT, &cancel)
                .await
                .is_err(),
            "Reset accepted {case}"
        );
        let observed = page.evaluate("({clicks:window.PUBLIC_removeClicks,files:document.querySelector('#PUBLIC-file').files.length,remaining:document.querySelectorAll('[data-testid=attachment-chip]').length})", SHORT, &cancel).await?;
        assert_eq!(
            observed,
            json!({"clicks":1,"files":1,"remaining":1}),
            "Reset crossed {case}: {observed}"
        );
        page.evaluate(
            "sessionStorage.removeItem('oracle:target-claim');true",
            SHORT,
            &cancel,
        )
        .await?;
    }
    Ok(())
}

async fn fixture_uploads(
    page: &Cdp,
    reservation: &RendererReservation,
    names: &[&str],
) -> Result<Vec<Value>> {
    let manifests = page.evaluate_with_arguments(r#"(cfg)=>{
        const form=document.querySelector('form'),input=form.querySelector('input[type=file]');
        const transfer=new DataTransfer();
        if(cfg.names.length)transfer.items.add(new File(['PUBLIC fixture bytes'],cfg.names.at(-1)));
        input.files=transfer.files;
        const files=Object.freeze(Array.from(input.files));
        const receipts=window.__oracleAttachmentEvidence=new Map();
        return cfg.names.map((name,i)=>{
            const chip=document.createElement('div');chip.dataset.testid='attachment-chip';chip.textContent=name;
            const control=document.createElement('button');control.type='button';control.setAttribute('aria-label','Remove file '+name);control.textContent='Remove';control.onclick=()=>chip.remove();chip.append(control);form.append(chip);
            const id='PUBLIC-upload-'+i;
            receipts.set(id,{name:name.toLowerCase().replace(/\s+/g,' ').trim(),owner:cfg.owner,document,composer:form,control,label:control.getAttribute('aria-label'),input,inputFiles:files});
            return {name,rendererReceiptId:id};
        });
    }"#, &json!([{"owner":reservation.owner(),"names":names}]), SHORT, &CancellationToken::new()).await?;
    manifests
        .as_array()
        .context("PUBLIC upload manifests missing")?
        .iter()
        .map(|manifest| browser_dom::receipt_expectation(manifest, false))
        .collect()
}

async fn attachment_identity_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    for case in [
        "intact-duplicates",
        "replaced",
        "missing-duplicate",
        "reused-control",
        "extra",
        "zero-extra",
        "decoy-extra",
        "extra-tile",
        "changed-input",
    ] {
        mount(page,ACTIVE,"/","()=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_sends=0;document.querySelector('form').onsubmit=e=>{e.preventDefault();window.PUBLIC_sends++;};}").await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        let names: &[&str] = if matches!(case, "zero-extra" | "decoy-extra") {
            &[]
        } else if matches!(
            case,
            "intact-duplicates" | "missing-duplicate" | "reused-control"
        ) {
            &["PUBLIC-shared.txt", "PUBLIC-shared.txt"]
        } else {
            &["PUBLIC.txt"]
        };
        let expectations = fixture_uploads(page, &reservation, names).await?;
        let nav = browser_dom::navigation("https://chatgpt.com/")?;
        let prepared = PreparedPrompt::stage(
            &mut reservation,
            "PUBLIC attachment identity",
            &nav,
            &expectations,
            SHORT,
            &cancel,
        )
        .await?;
        prepared.arm_send(&mut reservation, SHORT, &cancel).await?;
        if case == "intact-duplicates" {
            reservation
                .evaluate_mutation(
                    "()=>{window.__oracleAttachmentDispatchGuard.cleanup();return true;}",
                    json!({}),
                    SHORT,
                    &cancel,
                )
                .await?;
            prepared.cleanup_witness(&reservation, false, SHORT).await?;
            prepared.cleanup(&mut reservation, SHORT).await?;
            let manifests:Vec<_>=expectations.iter().map(|item|json!({"name":item["name"],"rendererReceiptId":item["rendererReceiptId"]})).collect();
            browser_dom::reset_uploads(&mut reservation, &manifests, &nav, SHORT, &cancel).await?;
            assert_eq!(
                page.evaluate("window.__oracleAttachmentEvidence.size", SHORT, &cancel)
                    .await?,
                0
            );
            reservation.release_clean(SHORT).await?;
        } else {
            page.evaluate_with_arguments(r#"(cfg)=>{
                const controls=Array.from(document.querySelectorAll('[aria-label^="Remove file"]'));
                if(cfg.case==='replaced')controls[0].replaceWith(controls[0].cloneNode(true));
                if(cfg.case==='missing-duplicate')controls[0].parentElement.remove();
                if(cfg.case==='reused-control'){const receipts=Array.from(window.__oracleAttachmentEvidence.values());receipts[1].control=receipts[0].control;}
                if(['extra','zero-extra','decoy-extra','extra-tile'].includes(cfg.case)){const tile=document.createElement('div');tile.dataset.testid='attachment-chip';tile.textContent='PUBLIC-unexpected.png';if(cfg.case!=='extra-tile')tile.innerHTML+='<button type="button" aria-label="Remove file PUBLIC-unexpected.png">Remove</button>';document.querySelector('form').append(tile);}
                if(cfg.case==='decoy-extra'){const decoy=document.createElement('form');decoy.innerHTML='<textarea id="prompt-textarea"></textarea><button type="submit" data-testid="send-button">Send</button>';document.body.prepend(decoy);}
                if(cfg.case==='changed-input'){const transfer=new DataTransfer();transfer.items.add(new File(['PUBLIC changed'],'PUBLIC.txt'));document.querySelector('input[type=file]').files=transfer.files;}
                return true;
            }"#,&json!([{"case":case}]),SHORT,&cancel).await?;
            reservation.dispatch_started(SHORT, &cancel).await?;
            assert!(
                prepared
                    .send_once(&mut reservation, SHORT, &cancel)
                    .await
                    .is_err(),
                "Incorrect attachment set sent: {case}"
            );
            assert_eq!(
                page.evaluate("window.PUBLIC_sends", SHORT, &cancel).await?,
                0,
                "{case}"
            );
            assert_ne!(
                page.evaluate(
                    "window.__oracleSubmissionWitness?.read?.().state",
                    SHORT,
                    &cancel
                )
                .await?,
                "captured"
            );
            prepared.cleanup(&mut reservation, SHORT).await?;
            prepared.cleanup_witness(&reservation, false, SHORT).await?;
        }
        page.evaluate(
            "sessionStorage.removeItem('oracle:target-claim');true",
            SHORT,
            &cancel,
        )
        .await?;
    }
    Ok(())
}

async fn failed_upload_settlement_cases(page: &Cdp, file: &std::path::Path) -> Result<()> {
    let cancel = CancellationToken::new();
    for dropped in [false, true] {
        // No attachment-ready UI is mounted, so every command can finish whilst
        // the readiness condition stays false. The form never submits.
        mount(page, ACTIVE, "/", "()=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_sends=0;document.querySelector('form').addEventListener('submit',event=>{event.preventDefault();window.PUBLIC_sends++;});}").await?;
        let mut owner = RendererReservation::acquire(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        owner.seal_selection();
        let mut upload = browser_dom::UploadFile::open(file)?;
        let nav = browser_dom::navigation("https://chatgpt.com/")?;
        if dropped {
            anyhow::ensure!(
                tokio::time::timeout(
                    Duration::from_millis(500),
                    browser_dom::upload(&mut owner, &mut upload, &nav, SHORT, &cancel)
                )
                .await
                .is_err(),
                "Upload future finished before the drop fixture boundary"
            );
            anyhow::ensure!(
                page.evaluate(
                    "document.querySelector('input[type=file]').files.length",
                    SHORT,
                    &cancel
                )
                .await?
                    == 1,
                "Dropped upload did not reach the acknowledged file-input command"
            );
        } else {
            anyhow::ensure!(
                browser_dom::upload(
                    &mut owner,
                    &mut upload,
                    &nav,
                    Duration::from_millis(300),
                    &cancel
                )
                .await
                .is_err(),
                "Missing attachment readiness unexpectedly succeeded"
            );
        }
        owner.quarantine(SHORT).await?;
        anyhow::ensure!(
            owner.settled_before_dispatch() == !dropped,
            "Upload settlement did not distinguish acknowledged cleanup from a dropped future"
        );
        anyhow::ensure!(
            page.evaluate("window.PUBLIC_sends", SHORT, &cancel).await? == 0,
            "Failed upload reached submit"
        );
        // These are exclusively this fixture's records. A dropped upload retains
        // them for recovery; remove them before the next synthetic case.
        page.evaluate("for(const record of Object.values(window.__oracleFileInputGuards??{}))record.guard.cleanup();delete window.__oracleFileInputGuards;delete window.__oracleAttachmentEvidence;sessionStorage.removeItem('oracle:target-claim');true", SHORT, &cancel).await?;
    }
    Ok(())
}

/// Reference targetClaim: quarantine disposes delivery guards that no longer protect the page.
async fn quarantine_guard_disposal_case(page: &Cdp) -> Result<()> {
    const GUARDS: &str = "()=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_disposed=[];\
        const guard=(name,fields)=>({...fields,validate(){},cleanup(){window.PUBLIC_disposed.push(name);}});\
        window.__oracleFileInputGuards={rolled:{guard:guard('rolled',{rollbackComplete:true})},unrolled:{guard:guard('unrolled',{rollbackComplete:false})}};\
        window.__oracleAttachmentPromptGuards={prompt:guard('prompt',{})};\
        window.__oracleAttachmentPlusGuards={idle:guard('idle',{sawKeyDown:false}),pressed:guard('pressed',{sawKeyDown:true})};\
        window.__oracleAttachmentPlusNodes={idle:document.body,pressed:document.body,orphan:document.body};\
        window.__oracleAttachmentDispatchGuard=guard('dispatch',{sawKeyDown:false,sawPress:false});}";
    const STATE: &str = "({disposed:window.PUBLIC_disposed.slice().sort(),files:Object.keys(window.__oracleFileInputGuards),\
        prompts:Object.keys(window.__oracleAttachmentPromptGuards),plus:Object.keys(window.__oracleAttachmentPlusGuards),\
        nodes:Object.keys(window.__oracleAttachmentPlusNodes)})";
    let cancel = CancellationToken::new();
    mount(page, ACTIVE, "/", GUARDS).await?;
    let mut owner =
        RendererReservation::acquire(page.clone(), "PUBLIC-quarantine".into(), SHORT, &cancel)
            .await?;
    owner.quarantine(SHORT).await?;
    let state = page.evaluate(STATE, SHORT, &cancel).await?;
    page.evaluate(
        "delete window.__oracleFileInputGuards;delete window.__oracleAttachmentPromptGuards;delete window.__oracleAttachmentPlusGuards;delete window.__oracleAttachmentPlusNodes;delete window.__oracleAttachmentDispatchGuard;sessionStorage.removeItem('oracle:target-claim');true",
        SHORT,
        &cancel,
    )
    .await?;
    anyhow::ensure!(
        state
            == json!({"disposed":["dispatch","idle","prompt","rolled"],"files":["rolled","unrolled"],
                "prompts":["prompt"],"plus":["idle","pressed"],"nodes":["idle","pressed"]}),
        "Quarantine kept stale listeners, disposed protecting ones or unregistered owner guards: {state}"
    );
    Ok(())
}

async fn initial_readiness_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    mount(page, ACTIVE, "/", "()=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_delayedForm=document.querySelector('form');window.PUBLIC_delayedForm.remove();}").await?;
    let header_first =
        RendererReservation::acquire(page.clone(), "PUBLIC-header-first".into(), SHORT, &cancel)
            .await?;
    let expected = browser_dom::navigation("https://chatgpt.com/")?;
    anyhow::ensure!(
        browser_dom::capture_navigation(&header_first, SHORT, &cancel)
            .await
            .is_err(),
        "Temporary header without a mounted composer was accepted"
    );
    anyhow::ensure!(
        browser_dom::wait_initial_navigation(
            &header_first,
            &expected,
            Duration::from_millis(150),
            &cancel
        )
        .await
        .is_err(),
        "Missing composer passed the readiness deadline"
    );
    let attach = async {
        tokio::time::sleep(Duration::from_millis(75)).await;
        page.evaluate_with_arguments(
            "()=>{document.querySelector('main').append(window.PUBLIC_delayedForm);return true;}",
            &json!([]),
            SHORT,
            &cancel,
        )
        .await
    };
    let (captured, attached) = tokio::join!(
        browser_dom::wait_initial_navigation(&header_first, &expected, SHORT, &cancel),
        attach
    );
    attached?;
    anyhow::ensure!(
        captured?.1 == expected
            && header_first.state() == crate::browser_renderer::DispatchState::NotStarted,
        "Delayed composer was not admitted without dispatch"
    );
    for change in ["hidden", "disabled", "readOnly", "work"] {
        page.evaluate_with_arguments("(change)=>{const editor=document.querySelector('#prompt-textarea');editor.hidden=change==='hidden';editor.disabled=change==='disabled';editor.readOnly=change==='readOnly';editor.placeholder=change==='work'?'Work on anything':'';return true;}", &json!([change]), SHORT, &cancel).await?;
        anyhow::ensure!(
            browser_dom::wait_initial_navigation(
                &header_first,
                &expected,
                Duration::from_millis(150),
                &cancel
            )
            .await
            .is_err(),
            "Unavailable or Work composer was admitted: {change}"
        );
    }
    let inactive = ACTIVE.replace("Turn off temporary chat", "Turn on temporary chat");
    mount(
        page,
        &inactive,
        "/",
        "()=>{sessionStorage.removeItem('oracle:target-claim');}",
    )
    .await?;
    let reservation =
        RendererReservation::acquire(page.clone(), "PUBLIC-readiness".into(), SHORT, &cancel)
            .await?;
    let expected = browser_dom::navigation("https://chatgpt.com/")?;
    anyhow::ensure!(
        browser_dom::capture_navigation(&reservation, SHORT, &cancel)
            .await
            .is_err(),
        "One-shot capture did not reproduce the unmounted Temporary state"
    );
    let activate = async {
        tokio::time::sleep(Duration::from_millis(75)).await;
        page.evaluate_with_arguments("()=>{document.querySelector('header button').setAttribute('aria-label','Turn off temporary chat');return true;}",
            &json!([]), SHORT, &cancel).await
    };
    let (captured, activated) = tokio::join!(
        browser_dom::wait_initial_navigation(&reservation, &expected, SHORT, &cancel),
        activate
    );
    activated?;
    anyhow::ensure!(
        captured?.1 == expected,
        "Delayed Temporary UI was not admitted"
    );
    page.evaluate_with_arguments("()=>{document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');return true;}",
        &json!([]), SHORT, &cancel).await?;
    anyhow::ensure!(
        browser_dom::wait_initial_navigation(
            &reservation,
            &expected,
            Duration::from_millis(150),
            &cancel
        )
        .await
        .is_err(),
        "Unverified Temporary mode passed its readiness deadline"
    );
    page.evaluate_with_arguments(
        "()=>{history.replaceState(null,'','/c/PUBLIC-other');return true;}",
        &json!([]),
        SHORT,
        &cancel,
    )
    .await?;
    anyhow::ensure!(
        browser_dom::wait_initial_navigation(&reservation, &expected, SHORT, &cancel)
            .await
            .is_err(),
        "Changed initial navigation was admitted"
    );
    anyhow::ensure!(
        reservation.state() == crate::browser_renderer::DispatchState::NotStarted
            && page
                .evaluate(
                    "document.querySelector('#prompt-textarea').value",
                    SHORT,
                    &cancel
                )
                .await?
                == "",
        "Readiness observation entered or submitted payload"
    );
    page.evaluate_with_arguments(
        "()=>{sessionStorage.removeItem('oracle:target-claim');return true;}",
        &json!([]),
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

async fn retirement_marker_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    let owner = json!({"claimId":"PUBLIC-claim","reservationId":"PUBLIC-reservation"});
    let base = json!({"id":"PUBLIC-claim","retiring":true,"retirementId":"PUBLIC-retirement",
        "submission":{"id":"PUBLIC-reservation","state":"quarantined","dispatch":"acknowledged"},
        "inspection":{"token":"PUBLIC-inspection","rendererId":"PUBLIC-renderer","parentReservationId":"PUBLIC-reservation","state":"quarantined"}});
    for case in [
        "exact",
        "releasing-clean",
        "no-children",
        "foreign-marker",
        "foreign-claim",
        "foreign-submission",
        "active-submission",
        "foreign-inspection",
        "active-inspection",
        "unmarked-children",
        "completed-unmarked-children",
        "completed-active-submission",
        "completed-started-submission",
        "unmarked-clean",
        "changed-conversation",
    ] {
        let mut claim = base.clone();
        match case {
            "releasing-clean" => {
                claim["submission"]["state"] = json!("releasing");
                claim["inspection"]["state"] = json!("clean");
            }
            "no-children" | "unmarked-clean" => {
                claim.as_object_mut().unwrap().remove("submission");
                claim.as_object_mut().unwrap().remove("inspection");
            }
            "foreign-marker" => claim["retirementId"] = json!("PUBLIC-other"),
            "foreign-claim" => claim["id"] = json!("PUBLIC-other"),
            "foreign-submission" => claim["submission"]["id"] = json!("PUBLIC-other"),
            "active-submission"
            | "completed-active-submission"
            | "completed-started-submission" => {
                claim["submission"]["state"] = json!("active");
                if case == "completed-started-submission" {
                    claim["submission"]["dispatch"] = json!("started");
                }
            }
            "foreign-inspection" => {
                claim["inspection"]["parentReservationId"] = json!("PUBLIC-other")
            }
            "active-inspection" => claim["inspection"]["state"] = json!("active"),
            _ => {}
        }
        if case.starts_with("unmarked-") || case.starts_with("completed-") {
            claim["retiring"] = json!(false);
            claim.as_object_mut().unwrap().remove("retirementId");
        }
        mount(
            page,
            ACTIVE,
            if case == "changed-conversation" {
                "/c/PUBLIC-other"
            } else {
                "/c/PUBLIC-id"
            },
            "()=>{const root={return:null,stateNode:{}};root.stateNode.current=root;document.querySelector('#view').__reactFiber$PUBLIC={return:root,memoizedProps:{scrollStateConversationId:'PUBLIC-id',isChatConversation:true,isTemporaryChat:true,isWorkConversation:false,isSharedConversation:false}};}",
        )
        .await?;
        page.evaluate_with_arguments("(cfg)=>{sessionStorage.setItem('oracle:target-claim',JSON.stringify(cfg.claim));return true;}",&json!([{"claim":claim}]),SHORT,&cancel).await?;
        let expected = matches!(
            case,
            "exact"
                | "releasing-clean"
                | "no-children"
                | "unmarked-clean"
                | "completed-unmarked-children"
                | "completed-active-submission"
                | "completed-started-submission"
        );
        let expression = if case.starts_with("completed-") {
            crate::browser_retirement::completed_retirement_mark_expression()
        } else {
            crate::browser_retirement::retirement_mark_expression()
        };
        let observed=page.evaluate_with_arguments(&expression,&json!([{"claimId":"PUBLIC-claim","owner":owner,"conversationId":"PUBLIC-id","retirementId":"PUBLIC-retirement"}]),SHORT,&cancel).await?;
        anyhow::ensure!(
            observed == json!(expected),
            "Retirement marker case {case}: {observed}, expected {expected}"
        );
        // Research uses the same original-source gate before inspecting its report frames.
        page.evaluate_with_arguments(r#"()=>{const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-user-turn');turn.innerHTML='<div><div data-message-author-role="user" data-message-id="PUBLIC-user">PUBLIC research source</div></div><iframe name="deep-research" style="width:400px;height:300px"></iframe>';turn.querySelector('[data-message-author-role]').parentElement.__reactProps$PUBLIC={children:[null,null,{props:{children:[null,{props:{item:{type:'user-message',messageId:'PUBLIC-user',message:'PUBLIC research source'}}}]}}]};document.querySelector('main').append(turn);return true;}"#,&json!([]),SHORT,&cancel).await?;
        let session = json!({"browser":{"runtime":{"submissionOwner":owner,"userCommitReceipt":{"messageId":"PUBLIC-user","conversation":{"origin":"https://chatgpt.com","conversationId":"PUBLIC-id"}}}}});
        let record =
            json!({"nativeOwnerIdentity":{},"creator":{"target":{"claimId":"PUBLIC-claim"}}});
        let mut cfg = crate::browser_retirement::research_retirement_config(
            &record,
            &session,
            Some("PUBLIC-retirement"),
        );
        cfg["sourceOnly"] = json!(true);
        let research = page
            .evaluate_with_arguments(
                &crate::browser_runner::result_expression(),
                &json!([cfg]),
                SHORT,
                &cancel,
            )
            .await;
        anyhow::ensure!(
            research.is_ok() == expected,
            "Research source retirement gate accepted foreign ownership or refused its exact owner: {case}: {research:?}"
        );
        if let Ok(source) = research {
            anyhow::ensure!(
                source["lastUserOriginalSourceText"] == "PUBLIC research source"
                    && source["scopedResearchIframeCount"] == 1,
                "Research source/frame evidence was lost"
            );
        }
        let after = page
            .evaluate_with_arguments(
                "()=>JSON.parse(sessionStorage.getItem('oracle:target-claim'))",
                &json!([]),
                SHORT,
                &cancel,
            )
            .await?;
        if expected {
            claim["retiring"] = json!(true);
            claim["retirementId"] = json!("PUBLIC-retirement");
            if case.starts_with("completed-") && claim["submission"].is_object() {
                claim["submission"]["state"] = json!("quarantined");
                if case == "completed-started-submission" {
                    claim["submission"]["dispatch"] = json!("ambiguous");
                }
            }
        }
        anyhow::ensure!(
            after == claim,
            "Retirement marker changed unrelated ownership: {case}"
        );
        if !expected && !matches!(case, "unmarked-children" | "active-submission") {
            anyhow::ensure!(page.evaluate_with_arguments(
                &crate::browser_retirement::completed_retirement_mark_expression(),
                &json!([{"claimId":"PUBLIC-claim","owner":owner,"conversationId":"PUBLIC-id","retirementId":"PUBLIC-retirement"}]),
                SHORT, &cancel).await? == false,
                "Completed initial retirement accepted foreign or active ownership: {case}");
        }
    }
    for kind in ["claimed", "predispatch"] {
        for case in [
            "new",
            "retry",
            "foreign-marker",
            "foreign-claim",
            "active",
            "submitted",
        ] {
            mount(page, ACTIVE, "/c/PUBLIC-id", "()=>{}").await?;
            let mut claim = json!({"id":"PUBLIC-claim","retiring":case!="new","retirementId":"PUBLIC-retirement"});
            if kind == "predispatch" {
                claim["submission"] = json!({"id":"PUBLIC-reservation","state":"quarantined","dispatch":"not-started"});
                claim["inspection"] = json!({"token":"PUBLIC-inspect","rendererId":"PUBLIC-renderer","parentReservationId":"PUBLIC-reservation","state":"clean"});
            }
            match case {
                "foreign-marker" => claim["retirementId"] = json!("PUBLIC-other"),
                "foreign-claim" => claim["id"] = json!("PUBLIC-other"),
                "active" => {
                    claim["submission"] =
                        json!({"id":"PUBLIC-reservation","state":"active","dispatch":"not-started"})
                }
                "submitted" => {
                    claim["submission"] = json!({"id":"PUBLIC-reservation","state":"quarantined","dispatch":"acknowledged"})
                }
                _ => {}
            }
            page.evaluate_with_arguments("(cfg)=>{sessionStorage.setItem('oracle:target-claim',JSON.stringify(cfg.claim));return true;}",
                &json!([{"claim":claim}]), SHORT, &cancel).await?;
            let expression = if kind == "claimed" {
                crate::browser_retirement::claimed_expression("mark")
            } else {
                crate::browser_runner::pre_dispatch_mark_expression()
            };
            let args = json!([{"action":"mark","claimId":"PUBLIC-claim","owner":owner,"retirementId":"PUBLIC-retirement","conversationId":"PUBLIC-id",
                "navigation":{"origin":"https://chatgpt.com","conversationId":"PUBLIC-id"}}]);
            let observed = page
                .evaluate_with_arguments(&expression, &args, SHORT, &cancel)
                .await?;
            anyhow::ensure!(
                observed == json!(matches!(case, "new" | "retry")),
                "{kind} retirement {case} failed: {observed}"
            );
        }
    }
    page.evaluate_with_arguments(
        "()=>{sessionStorage.removeItem('oracle:target-claim');return true;}",
        &json!([]),
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

async fn witness_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    const MESSAGE: &str = "11111111-2222-4333-8444-555555555555";
    for case in [
        "immediate",
        "delayed",
        "source",
        "identity",
        "privacy",
        "privacy-view",
        "duplicate",
        "probe-conflict",
        "replaced-fetch",
        "unicode",
        "request-body",
        "query-endpoint",
        "credential-endpoint",
        "multimodal-image",
        "multimodal-reordered",
        "multimodal-two-images",
        "multimodal-unexpected-image",
        "multimodal-excess-image",
        "multimodal-missing-text",
        "multimodal-duplicate-text",
        "multimodal-unknown-part",
        "multimodal-unknown-field",
        "multimodal-empty-pointer",
        "multimodal-invalid-width",
        "multimodal-invalid-height",
        "multimodal-invalid-size",
        "multimodal-no-image",
        "multimodal-wrong-source",
        "search-inline",
        "search-suffix",
        "search-suffix-spaced",
        "search-plain",
        "search-unhinted",
        "search-unselected",
        "expired",
    ] {
        let checked: Result<()> = async {
        mount(page, ACTIVE, "/", "").await?;
        let prompt = if case == "unicode" {
            "PUBLIC e\u{301} prompt"
        } else {
            "PUBLIC witness prompt"
        };
        page.evaluate_with_arguments(r#"(cfg)=>{
            sessionStorage.removeItem('oracle:target-claim');
            window.PUBLIC_nativeFetch ??= window.fetch;
            window.PUBLIC_fetchCalls=0;
            window.PUBLIC_fetchReceiver={public:true};
            window.PUBLIC_transport=function(){window.PUBLIC_fetchCalls++;window.PUBLIC_actualReceiver=this;window.PUBLIC_actualInput=arguments[0];return Promise.resolve(new Response('PUBLIC response'));};
            window.fetch=window.PUBLIC_transport;
            document.querySelector('form').onsubmit=e=>{
                e.preventDefault();
                const payload={action:'next',history_and_training_disabled:cfg.case!=='privacy',messages:[{id:cfg.case==='identity'?'PUBLIC unqualified':cfg.messageId,author:{role:'user'},content:{content_type:'text',parts:[cfg.case==='source'?'PUBLIC wrong source':cfg.case==='unicode'?cfg.prompt.normalize('NFC'):cfg.prompt]}}]};
                if(cfg.case.startsWith('multimodal-')){
                    const image={content_type:'image_asset_pointer',asset_pointer:'file-service://PUBLIC-image',width:1,height:1,size_bytes:68};
                    let parts=[image,cfg.prompt];
                    if(cfg.case==='multimodal-reordered')parts=[cfg.prompt,image];
                    if(cfg.case==='multimodal-two-images'||cfg.case==='multimodal-excess-image')parts=[image,{...image,asset_pointer:'file-service://PUBLIC-second'},cfg.prompt];
                    if(cfg.case==='multimodal-missing-text')parts=[image];
                    if(cfg.case==='multimodal-duplicate-text')parts.push(cfg.prompt);
                    if(cfg.case==='multimodal-unknown-part')parts.unshift({content_type:'audio_asset_pointer',asset_pointer:'PUBLIC-audio'});
                    if(cfg.case==='multimodal-unknown-field')image.unqualified='PUBLIC';
                    if(cfg.case==='multimodal-empty-pointer')image.asset_pointer='';
                    if(cfg.case==='multimodal-invalid-width')image.width=0;
                    if(cfg.case==='multimodal-invalid-height')image.height=0.5;
                    if(cfg.case==='multimodal-invalid-size')image.size_bytes='68';
                    if(cfg.case==='multimodal-no-image')parts=[cfg.prompt];
                    if(cfg.case==='multimodal-wrong-source')parts=[image,'PUBLIC wrong source'];
                    payload.messages[0].content={content_type:'multimodal_text',parts};
                }
                if(cfg.case.startsWith('search-')){
                    payload.messages[0].content.parts=[cfg.case==='search-plain'?cfg.prompt:cfg.case==='search-suffix'?cfg.prompt+'@Web search':cfg.case==='search-suffix-spaced'?cfg.prompt+' @Web search':'@Web search '+cfg.prompt];
                    if(cfg.case!=='search-unhinted'){payload.system_hints=['search'];payload.messages[0].metadata={system_hints:['search']};}
                }
                window.PUBLIC_pending=()=>{
                    if(cfg.case==='privacy-view')document.querySelector('header button').setAttribute('aria-label','Turn on temporary chat');
                    const body=JSON.stringify(payload);
                    if(cfg.case==='request-body'){
                        const request=new Request('https://chatgpt.com/backend-api/f/conversation',{method:'POST',body});
                        window.PUBLIC_request=request;
                        window.fetch.call(window.PUBLIC_fetchReceiver,request);
                    }else{
                        const endpoint=cfg.case==='query-endpoint'?'/backend-api/f/conversation?unqualified=1':cfg.case==='credential-endpoint'?'https://PUBLIC@chatgpt.com/backend-api/f/conversation':'/backend-api/f/conversation';
                        window.fetch.call(window.PUBLIC_fetchReceiver,endpoint,{method:'POST',body});
                        if(cfg.case==='duplicate')window.fetch('/backend-api/f/conversation',{method:'POST',body});
                    }
                };
                if(cfg.case==='immediate'||cfg.case==='unicode'||cfg.case==='probe-conflict')window.PUBLIC_pending();
            };
            return true;
        }"#,&json!([{"case":case,"prompt":prompt,"messageId":MESSAGE}]),SHORT,&cancel).await?;
        let mut reservation = RendererReservation::acquire(
            page.clone(),
            "PUBLIC-witness-claim".into(),
            SHORT,
            &cancel,
        )
        .await?;
        let image_names: &[&str] = if case == "multimodal-two-images" {
            &["PUBLIC.png", "PUBLIC-second.png"]
        } else if case.starts_with("multimodal-") && case != "multimodal-unexpected-image" {
            &["PUBLIC.png"]
        } else {
            &[]
        };
        let expectations = fixture_uploads(page, &reservation, image_names).await?;
        let (_, nav) = browser_dom::capture_navigation(&reservation, SHORT, &cancel).await?;
        let mut prepared = PreparedPrompt::stage(
            &mut reservation,
            prompt,
            &nav,
            &expectations,
            SHORT,
            &cancel,
        )
        .await?;
        if case.starts_with("search-") && case != "search-unselected" {
            prepared.expect_web_search();
        }
        prepared.arm_send(&mut reservation, SHORT, &cancel).await?;
        // A same-source background call before admission is not creation evidence.
        page.evaluate_with_arguments("(cfg)=>{window.fetch('/backend-api/f/conversation',{method:'POST',body:JSON.stringify({action:'next',history_and_training_disabled:true,messages:[{id:cfg.id,author:{role:'user'},content:{content_type:'text',parts:[cfg.prompt]}}]})});return window.__oracleSubmissionWitness.read().state;}",&json!([{"id":MESSAGE,"prompt":prompt}]),SHORT,&cancel).await?;
        let pending = page
            .evaluate_with_arguments(
                "()=>window.__oracleSubmissionWitness.read()",
                &json!([]),
                SHORT,
                &cancel,
            )
            .await?;
        assert_eq!(
            pending["state"], "pending",
            "Unadmitted request captured: {case}"
        );
        reservation.dispatch_started(SHORT, &cancel).await?;
        prepared.send_once(&mut reservation, SHORT, &cancel).await?;
        prepared.cleanup(&mut reservation, SHORT).await?;
        if !matches!(case, "immediate" | "unicode" | "probe-conflict") {
            page.evaluate_with_arguments("(cfg)=>{if(cfg.case==='expired'){window.PUBLIC_realNow=Date.now;Date.now=()=>window.PUBLIC_realNow()+120001;}window.PUBLIC_pending();if(window.PUBLIC_realNow){Date.now=window.PUBLIC_realNow;delete window.PUBLIC_realNow;}return true;}",&json!([{"case":case}]),SHORT,&cancel).await?;
        }
        if case == "probe-conflict" {
            // Invalidate the collector while the original-source probe runs. The
            // receipt must read the final state in this same renderer task.
            page.evaluate_with_arguments(r#"(cfg)=>{
                const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-user-turn');
                turn.innerHTML='<div><div data-message-author-role="user" data-message-id="'+cfg.id+'">PUBLIC rendered source</div></div>';
                const user=turn.querySelector('[data-message-author-role]');
                user.parentElement.__reactProps$PUBLIC={children:[null,null,{props:{children:[null,{props:{item:{type:'user-message',messageId:cfg.id,message:cfg.prompt}}}]}}]};
                const original=user.getAttribute;let checked=false;
                user.getAttribute=function(name){if(!checked&&name==='data-message-id'){checked=true;window.PUBLIC_pending();}return original.call(this,name);};
                document.querySelector('main').append(turn);return true;
            }"#,&json!([{"id":MESSAGE,"prompt":prompt}]),SHORT,&cancel).await?;
            let receipt=page.evaluate_with_arguments(&crate::browser_commit::receipt_probe(),
                &json!([{"owner":reservation.owner(),"ids":[],"prompt":prompt,"intent":prepared.submission_intent(),"saved":null}]),SHORT,&cancel).await?;
            assert_eq!(
                receipt["probe"]["messages"][0]["matches"], true,
                "Source fixture was not complete: {receipt}"
            );
            assert_eq!(
                receipt["submission"]["state"], "invalid",
                "Stale witness survived source inspection: {receipt}"
            );
        }
            let observed=page.evaluate_with_arguments("()=>({state:window.__oracleSubmissionWitness.read?.()??window.__oracleSubmissionWitness.result,calls:window.PUBLIC_fetchCalls,receiver:window.PUBLIC_actualReceiver===window.PUBLIC_fetchReceiver,requestUntouched:window.PUBLIC_request?.bodyUsed===false})",&json!([]),SHORT,&cancel).await?;
        let valid = matches!(
            case,
            "immediate"
                | "delayed"
                | "replaced-fetch"
                | "unicode"
                | "multimodal-image"
                | "multimodal-reordered"
                | "multimodal-two-images"
                | "search-inline"
                | "search-suffix"
        );
        if case == "privacy" || case == "privacy-view" {
            assert_eq!(
                observed["state"]["failureDetail"],
                if case == "privacy" {
                    "history-disabled-not-true"
                } else {
                    "temporary-refused"
                }
            );
        }
        anyhow::ensure!(
            (observed["state"]["state"] == "captured") == valid,
            "{case}: {observed}"
        );
        if matches!(case, "search-inline" | "search-suffix") {
            // The committed user turn stores the pill token where the witness saw it; only a
            // verified Web search intent lets the original-source probe accept it.
            let source = if case == "search-inline" {
                format!("@Web search {prompt}")
            } else {
                format!("{prompt}@Web search")
            };
            page.evaluate_with_arguments(r#"(cfg)=>{
                const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-user-turn');
                turn.innerHTML='<div><div data-message-author-role="user" data-message-id="'+cfg.id+'">PUBLIC rendered source</div></div>';
                turn.querySelector('[data-message-author-role]').parentElement.__reactProps$PUBLIC={children:[null,null,{props:{children:[null,{props:{item:{type:'user-message',messageId:cfg.id,message:cfg.source}}}]}}]};
                document.querySelector('main').append(turn);return true;
            }"#,&json!([{"id":MESSAGE,"source":source}]),SHORT,&cancel).await?;
            for web_search in [true, false] {
                let mut intent = json!(prepared.submission_intent());
                intent["webSearch"] = json!(web_search);
                let receipt=page.evaluate_with_arguments(&crate::browser_commit::receipt_probe(),
                    &json!([{"owner":reservation.owner(),"ids":[],"prompt":prompt,"intent":intent,"saved":null}]),SHORT,&cancel).await?;
                anyhow::ensure!(
                    receipt["probe"]["messages"][0]["matches"] == web_search,
                    "{case} source with Web search {web_search}: {receipt}"
                );
            }
        }
        assert_eq!(
            observed["calls"],
            if matches!(case, "duplicate" | "probe-conflict") {
                3
            } else {
                2
            },
            "Transport changed: {case}"
        );
        if !matches!(case, "duplicate" | "probe-conflict") {
            assert_eq!(observed["receiver"], true, "Fetch receiver changed: {case}");
        }
        if case == "request-body" {
            assert_eq!(observed["requestUntouched"], true);
        }
        if valid {
            assert_eq!(observed["state"]["witness"]["messageId"], MESSAGE);
            assert_eq!(
                observed["state"]["witness"]["owner"],
                json!(reservation.owner())
            );
            assert!(observed["state"]["witness"].get("prompt").is_none());
            prepared.cleanup_witness(&reservation, true, SHORT).await?;
            let retained = page
                .evaluate_with_arguments(
                    "()=>({detached:typeof window.__oracleSubmissionWitness.read==='undefined'&&typeof window.__oracleSubmissionWitness.stop==='undefined'&&typeof window.__oracleSubmissionWitness.cleanup==='undefined',...window.__oracleSubmissionWitness.result})",
                    &json!([]),
                    SHORT,
                    &cancel,
                )
                .await?;
            assert_eq!(
                retained["detached"], true,
                "Collector closures were retained"
            );
            assert_eq!(
                retained["witness"], observed["state"]["witness"],
                "Lost settled recovery witness"
            );
        }
        if case == "replaced-fetch" {
            page.evaluate_with_arguments("()=>{window.PUBLIC_foreignFetch=()=>Promise.resolve(new Response());window.fetch=window.PUBLIC_foreignFetch;return true;}",&json!([]),SHORT,&cancel).await?;
        }
        prepared.cleanup_witness(&reservation, false, SHORT).await?;
        let cleaned=page.evaluate_with_arguments("(cfg)=>{const result={absent:!window.__oracleSubmissionWitness,restored:window.fetch===(cfg.case==='replaced-fetch'?window.PUBLIC_foreignFetch:window.PUBLIC_transport)};window.fetch=window.PUBLIC_nativeFetch;sessionStorage.removeItem('oracle:target-claim');delete window.PUBLIC_request;return result;}",&json!([{"case":case}]),SHORT,&cancel).await?;
        assert_eq!(cleaned, json!({"absent":true,"restored":true}), "{case}");
        Ok(())
        }.await;
        checked.with_context(|| format!("Witness case {case}"))?;
    }
    Ok(())
}

async fn unknown_privacy_send_case(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    mount(page, ACTIVE, "/", "()=>{sessionStorage.removeItem('oracle:target-claim');window.PUBLIC_sends=0;document.querySelector('form').onsubmit=e=>{e.preventDefault();window.PUBLIC_sends++;};}").await?;
    let mut reservation =
        RendererReservation::acquire(page.clone(), "PUBLIC-unknown-claim".into(), SHORT, &cancel)
            .await?;
    let (_, nav) = browser_dom::capture_navigation(&reservation, SHORT, &cancel).await?;
    let prepared = PreparedPrompt::stage(
        &mut reservation,
        "PUBLIC unknown privacy",
        &nav,
        &[],
        SHORT,
        &cancel,
    )
    .await?;
    prepared.arm_send(&mut reservation, SHORT, &cancel).await?;
    page.evaluate_with_arguments(
        "()=>{document.querySelector('header').remove();return true;}",
        &json!([]),
        SHORT,
        &cancel,
    )
    .await?;
    assert!(require(page, UNKNOWN, &cancel).await.is_err());
    reservation.dispatch_started(SHORT, &cancel).await?;
    assert!(
        prepared
            .send_once(&mut reservation, SHORT, &cancel)
            .await
            .is_err()
    );
    assert_eq!(
        page.evaluate("window.PUBLIC_sends", SHORT, &cancel).await?,
        0,
        "Unknown privacy allowed Send"
    );
    prepared.cleanup(&mut reservation, SHORT).await?;
    prepared.cleanup_witness(&reservation, false, SHORT).await?;
    page.evaluate(
        "(()=>{sessionStorage.removeItem('oracle:target-claim');return true;})()",
        SHORT,
        &cancel,
    )
    .await?;
    Ok(())
}

async fn witness_closure_release_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    for case in [
        "normal",
        "privacy-throws",
        "stop-setter-throws",
        "cleanup-setter-throws",
    ] {
        let throwing = case == "privacy-throws";
        let setter_throws = case.ends_with("setter-throws");
        mount(page, ACTIVE, "/", "").await?;
        let install = format!(
            r#"()=>{{
            window.PUBLIC_savedFetch=window.fetch;
            window.PUBLIC_transportCalls=0;window.PUBLIC_privacyCalls=0;
            window.PUBLIC_throwAtStop=false;
            window.fetch=function(){{window.PUBLIC_transportCalls++;return Promise.resolve(new Response('PUBLIC'));}};
            const cfg={{owner:{{claimId:'PUBLIC-closure-claim',reservationId:'PUBLIC-closure-reservation'}},prompt:'PUBLIC closure prompt',witnessId:'PUBLIC-witness',dispatchGuardId:'PUBLIC-guard',promptSha256:'PUBLIC-digest',expectations:[]}};
            window.PUBLIC_configWeak=new WeakRef(cfg);
            ({collector})(cfg,{{admitted:true}},()=>{{}},()=>{{window.PUBLIC_privacyCalls++;if(window.PUBLIC_throwAtStop)throw Error('PUBLIC');return true;}});
            return true;
        }}"#,
            collector = include_str!("../browser-scripts/submission-witness.js")
        );
        page.evaluate_with_arguments(&install, &json!([]), SHORT, &cancel)
            .await?;
        // Retain a real delegating wrapper, not an unrelated replacement stub.
        let stopped=page.evaluate_with_arguments("(cfg)=>{const delegate=window.fetch;window.PUBLIC_wrapper=function(){return Reflect.apply(delegate,this,arguments);};if(cfg.setterThrows)Object.defineProperty(window,'fetch',{configurable:true,get:()=>delegate,set:()=>{throw Error('PUBLIC restore refused');}});else window.fetch=window.PUBLIC_wrapper;window.PUBLIC_throwAtStop=cfg.throwing;try{return {ok:cfg.cleanup?window.__oracleSubmissionWitness.cleanup():window.__oracleSubmissionWitness.stop()};}catch{return {failed:true};}}",&json!([{"throwing":throwing,"setterThrows":setter_throws,"cleanup":case=="cleanup-setter-throws"}]),SHORT,&cancel).await?;
        assert_eq!(
            stopped["failed"] == true,
            setter_throws,
            "{case}: {stopped}"
        );
        page.call("HeapProfiler.collectGarbage", json!({}), SHORT, &cancel)
            .await?;
        let observed=page.evaluate_with_arguments("(cfg)=>{const r=window.__oracleSubmissionWitness;const probes=window.PUBLIC_privacyCalls;window.fetch('/PUBLIC-after-close');return {configReleased:window.PUBLIC_configWeak.deref()===undefined,detached:cfg.setterThrows||!r.read&&!r.stop&&!r.cleanup,wrapperPreserved:cfg.setterThrows||window.fetch===window.PUBLIC_wrapper,forwarded:window.PUBLIC_transportCalls===1,noProbe:window.PUBLIC_privacyCalls===probes,result:r.result};}",&json!([{"setterThrows":setter_throws}]),SHORT,&cancel).await?;
        for key in [
            "configReleased",
            "detached",
            "wrapperPreserved",
            "forwarded",
            "noProbe",
        ] {
            assert_eq!(observed[key], true, "Collector {case}, {key}: {observed}");
        }
        if throwing {
            assert_eq!(observed["result"]["state"], "invalid");
            assert_eq!(
                observed["result"]["failureDetail"],
                "privacy-probe-unavailable"
            );
            assert_eq!(observed["result"]["failurePhase"], "stop");
        }
        page.evaluate_with_arguments("()=>{Object.defineProperty(window,'fetch',{configurable:true,writable:true,value:window.PUBLIC_savedFetch});delete window.__oracleSubmissionWitness;delete window.PUBLIC_wrapper;return true;}",&json!([]),SHORT,&cancel).await?;
    }
    Ok(())
}

async fn witness_route_transition_cases(page: &Cdp) -> Result<()> {
    use crate::browser_commit::{self, CommitObservation};
    const MESSAGE: &str = "11111111-2222-4333-8444-555555555555";
    const PROMPT: &str = "PUBLIC route transition prompt";
    let cancel = CancellationToken::new();
    for case in [
        "fetch-before-route",
        "fetch-before-route-without-header",
        "retained-before-route-without-header",
        "fetch-after-route",
        "retained-gap",
        "retained-before-route",
        "quarantined-gap",
        "quarantined-before-route",
        "detached-competing-route",
        "quarantined-detached-competing-route",
        "provisional-route",
        "provisional-fetch",
        "quarantined-provisional-route",
        "server-to-provisional",
        "unqualified-client-route",
        "retained-changed-route",
        "no-fetch",
        "wrong-message",
        "ordinary-view",
        "changed-route",
    ] {
        mount(page, ACTIVE, "/", "").await?;
        page.evaluate_with_arguments(r#"(cfg)=>{
            sessionStorage.removeItem('oracle:target-claim');
            window.PUBLIC_nativeFetch=window.fetch;
            window.PUBLIC_sendCount=0;window.PUBLIC_fetchCount=0;
            window.fetch=()=>{window.PUBLIC_fetchCount++;return Promise.resolve(new Response('PUBLIC'));};
            window.PUBLIC_request=()=>window.fetch('/backend-api/f/conversation',{method:'POST',body:JSON.stringify({action:'next',history_and_training_disabled:true,messages:[{id:cfg.messageId,author:{role:'user'},content:{content_type:'text',parts:[cfg.prompt]}}]})});
            document.querySelector('form').onsubmit=e=>{e.preventDefault();window.PUBLIC_sendCount++;if(cfg.case.endsWith('without-header'))document.querySelector('header').remove();if(!['fetch-after-route','provisional-fetch','no-fetch'].includes(cfg.case))window.PUBLIC_request();};
            return true;
        }"#,&json!([{"case":case,"messageId":MESSAGE,"prompt":PROMPT}]),SHORT,&cancel).await?;
        let mut reservation =
            RendererReservation::acquire(page.clone(), "PUBLIC-route-claim".into(), SHORT, &cancel)
                .await?;
        let (_, navigation) = browser_dom::capture_navigation(&reservation, SHORT, &cancel).await?;
        let prepared =
            PreparedPrompt::stage(&mut reservation, PROMPT, &navigation, &[], SHORT, &cancel)
                .await?;
        let mut before =
            browser_commit::capture_snapshot(page, reservation.owner(), PROMPT, SHORT, &cancel)
                .await?;
        prepared.arm_send(&mut reservation, SHORT, &cancel).await?;
        before.bind_submission(prepared.submission_intent())?;
        reservation.dispatch_started(SHORT, &cancel).await?;
        prepared.send_once(&mut reservation, SHORT, &cancel).await?;
        prepared.cleanup(&mut reservation, SHORT).await?;
        if case.starts_with("quarantined-") {
            reservation.quarantine(SHORT).await?;
        }
        if (case.ends_with("before-route") && case != "fetch-before-route")
            || case == "retained-before-route-without-header"
            || case.ends_with("detached-competing-route")
        {
            prepared.cleanup_witness(&reservation, true, SHORT).await?;
        }
        page.evaluate_with_arguments(
            r#"(cfg)=>{
            const provisional=['provisional-route','provisional-fetch','quarantined-provisional-route'].includes(cfg.case);
            history.pushState({},'', provisional?'/c/local-chatgpt%3A11111111-2222-4333-8444-555555555555':cfg.case==='unqualified-client-route'?'/c/local-other%3A11111111-2222-4333-8444-555555555555':'/c/PUBLIC-created');
            document.querySelector('header')?.remove();
            if(['fetch-after-route','provisional-fetch'].includes(cfg.case))window.PUBLIC_request();
            return true;
        }"#,
            &json!([{"case":case}]),
            SHORT,
            &cancel,
        )
        .await?;
        assert!(
            require(page, UNKNOWN, &cancel).await.is_err(),
            "Unmounted view proved privacy"
        );
        if case.ends_with("detached-competing-route") {
            page.evaluate_with_arguments("()=>{const record=window.__oracleSubmissionWitness;window.PUBLIC_witnessWrites=0;Object.defineProperty(window,'__oracleSubmissionWitness',{configurable:true,get:()=>record,set:()=>{window.PUBLIC_witnessWrites++;}});return true;}",&json!([]),SHORT,&cancel).await?;
        }
        let mut latch = None;
        let gap = browser_commit::read_receipt(
            page,
            reservation.owner(),
            &before,
            PROMPT,
            SHORT,
            &cancel,
            &mut latch,
        )
        .await
        .with_context(|| format!("{case}: reading unmounted route"))?;
        assert!(
            if case == "unqualified-client-route" {
                matches!(gap, CommitObservation::Ambiguous(_))
            } else {
                matches!(gap, CommitObservation::Pending)
            },
            "{case}: route gap was not pending: {gap:?}"
        );
        if case.ends_with("detached-competing-route") {
            let writes=page.evaluate_with_arguments("()=>{const record=window.__oracleSubmissionWitness;const unchanged=window.PUBLIC_witnessWrites===0&&record.conversationId===null;Object.defineProperty(window,'__oracleSubmissionWitness',{configurable:true,writable:true,value:record});return unchanged;}",&json!([]),SHORT,&cancel).await?;
            assert_eq!(writes, true, "{case}: recovery changed detached evidence");
        }
        if matches!(
            case,
            "retained-gap" | "retained-changed-route" | "quarantined-gap"
        ) {
            prepared.cleanup_witness(&reservation, true, SHORT).await?;
            let detached = page.evaluate_with_arguments(
                "()=>typeof window.__oracleSubmissionWitness.read==='undefined' && window.__oracleSubmissionWitness.result.state==='pending'",
                &json!([]), SHORT, &cancel,
            ).await?;
            assert_eq!(detached, true, "{case}: retained a collector closure");
        }
        let mount_source = r#"(cfg)=>{
            if(['provisional-route','provisional-fetch','quarantined-provisional-route','unqualified-client-route'].includes(cfg.case))history.pushState({},'', '/c/PUBLIC-created');
            if(cfg.case==='server-to-provisional')history.pushState({},'', '/c/local-chatgpt%3A11111111-2222-4333-8444-555555555555');
            if(['changed-route','retained-changed-route'].includes(cfg.case)||cfg.case.endsWith('detached-competing-route'))history.pushState({},'', '/c/PUBLIC-other');
            const root={return:null,stateNode:{}};root.stateNode.current=root;
            document.querySelector('#view').__reactFiber$PUBLIC={return:root,memoizedProps:{scrollStateConversationId:location.pathname.split('/').pop(),isChatConversation:true,isTemporaryChat:cfg.case!=='ordinary-view',isWorkConversation:false,isSharedConversation:false}};
            const id=cfg.case==='wrong-message'?'99999999-2222-4333-8444-555555555555':cfg.messageId;
            const turn=document.createElement('article');turn.setAttribute('data-turn-key','PUBLIC-user-turn');
            turn.innerHTML='<div><div data-message-author-role="user" data-message-id="'+id+'">PUBLIC rendered source</div></div>';
            turn.querySelector('[data-message-author-role]').parentElement.__reactProps$PUBLIC={children:[null,null,{props:{children:[null,{props:{item:{type:'user-message',messageId:id,message:cfg.prompt}}}]}}]};
            document.querySelector('main').append(turn);return true;
        }"#;
        page.evaluate_with_arguments(
            mount_source,
            &json!([{"case":case,"messageId":MESSAGE,"prompt":PROMPT}]),
            SHORT,
            &cancel,
        )
        .await?;
        let settled = browser_commit::read_receipt(
            page,
            reservation.owner(),
            &before,
            PROMPT,
            SHORT,
            &cancel,
            &mut latch,
        )
        .await?;
        if matches!(
            case,
            "fetch-before-route"
                | "fetch-before-route-without-header"
                | "retained-before-route-without-header"
                | "fetch-after-route"
                | "retained-gap"
                | "retained-before-route"
                | "quarantined-gap"
                | "quarantined-before-route"
                | "detached-competing-route"
                | "quarantined-detached-competing-route"
                | "provisional-route"
                | "provisional-fetch"
                | "quarantined-provisional-route"
        ) {
            let CommitObservation::Committed(receipt) = settled else {
                anyhow::bail!("{case}: mounted exact source did not commit: {settled:?}");
            };
            assert_eq!(receipt.message_id, MESSAGE);
            assert_eq!(
                receipt.conversation.conversation_id,
                if case.ends_with("detached-competing-route") {
                    "PUBLIC-other"
                } else {
                    "PUBLIC-created"
                }
            );
            before.seal_witness(&receipt)?;
        } else if case == "no-fetch" {
            assert!(
                matches!(settled, CommitObservation::Pending),
                "Accepted source without a provider request: {settled:?}"
            );
        } else {
            assert!(
                matches!(settled, CommitObservation::Ambiguous(_)),
                "{case}: unsafe receipt: {settled:?}"
            );
        }
        let counts = page
            .evaluate_with_arguments(
                "()=>({send:window.PUBLIC_sendCount,fetch:window.PUBLIC_fetchCount})",
                &json!([]),
                SHORT,
                &cancel,
            )
            .await?;
        assert_eq!(
            counts,
            json!({"send":1,"fetch":if case=="no-fetch" {0} else {1}}),
            "{case}: replayed dispatch"
        );
        prepared.cleanup_witness(&reservation, false, SHORT).await?;
        if case == "fetch-after-route" {
            // Round-trip the durable snapshot and use a fresh latch, as a new
            // Rust recovery process does. Collector methods have been removed.
            let saved = serde_json::from_value(serde_json::to_value(&before)?)?;
            let original = browser_commit::read_receipt(
                page,
                reservation.owner(),
                &saved,
                PROMPT,
                SHORT,
                &cancel,
                &mut None,
            )
            .await?;
            assert!(
                matches!(original, CommitObservation::Committed(_)),
                "Original document lost saved receipt: {original:?}"
            );
            page.call("Page.reload", json!({}), SHORT, &cancel).await?;
            let deadline = tokio::time::Instant::now() + SHORT;
            loop {
                let loaded = page
                    .evaluate_with_arguments(
                        "()=>document.readyState==='complete'&&!window.__oracleSubmissionDocument",
                        &json!([]),
                        SHORT,
                        &cancel,
                    )
                    .await?;
                if loaded == true {
                    break;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "Replacement document did not load"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // The same target/sessionStorage claim, route, UUID and complete
            // source are insufficient when the original Document has gone.
            mount(page, ACTIVE, "/c/PUBLIC-created", "").await?;
            page.evaluate_with_arguments(
                mount_source,
                &json!([{"case":"replacement","messageId":MESSAGE,"prompt":PROMPT}]),
                SHORT,
                &cancel,
            )
            .await?;
            require(page, SHORT, &cancel).await?;
            let replaced = browser_commit::read_receipt(
                page,
                reservation.owner(),
                &saved,
                PROMPT,
                SHORT,
                &cancel,
                &mut None,
            )
            .await?;
            assert!(
                matches!(replaced, CommitObservation::Ambiguous(_)),
                "Replacement document re-authorised a saved witness: {replaced:?}"
            );
        }
        page.evaluate_with_arguments(
            "()=>{if(window.PUBLIC_nativeFetch)window.fetch=window.PUBLIC_nativeFetch;delete window.PUBLIC_request;sessionStorage.removeItem('oracle:target-claim');return true;}",
            &json!([]), SHORT, &cancel,
        ).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Requires installed Chrome; executes intercepted public DOM fixtures in a disposable profile"]
async fn native_temporary_chat_predicate_and_retained_send_guard() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let environment = [
        "PATH",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
        "SystemRoot",
    ]
    .into_iter()
    .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
    .collect();
    let executable = crate::browser_detect::chrome(&environment)
        .context("Install Chrome to run native privacy fixtures")?;
    let profile = ProfileRegistry::open(&temp.path().join("PUBLIC-profile"))?;
    let cancel = CancellationToken::new();
    let mut chrome = ChromeConnection::launch_or_reuse(
        profile,
        ChromeLaunch {
            executable,
            cwd: temp.path().into(),
            environment,
            debug_port: None,
            timeout: Duration::from_secs(15),
            keep_browser: false,
            headless: true,
            hide_window: true,
            keep_window_behind: false,
        },
        &cancel,
    )
    .await?;
    let result = async {
        crate::browser_runner::native_grok_runner_cases(&chrome.browser,&chrome.endpoint)
            .await.context("Native Grok owned runner lifecycle")?;
        let rows=chrome.browser.call("Target.getTargets",json!({}),SHORT,&cancel).await?;
        let id=rows["targetInfos"].as_array().context("No targets")?.iter().find(|row|row["type"]=="page").and_then(|row|row["targetId"].as_str()).context("No startup page")?;
        let attached=chrome.browser.call("Target.attachToTarget",json!({"targetId":id,"flatten":true}),SHORT,&cancel).await?;
        let page=chrome.browser.with_session(attached["sessionId"].as_str().context("No page session")?.into());
        let mut events=chrome.browser.events();
        page.call("Fetch.enable",json!({"patterns":[{"urlPattern":"*"}]}),SHORT,&cancel).await?;
        let browser=chrome.browser.clone();
        let interceptor=tokio::spawn(async move {
            while let Ok(event)=events.recv().await {
                if event["method"]!="Fetch.requestPaused" {continue;}
                let session=event["sessionId"].as_str().context("Unscoped request")?;
                let body="<!doctype html><meta http-equiv='Content-Security-Policy' content=\"default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'none'; form-action 'none'\"><style>button,textarea,input,#view{min-width:40px;min-height:24px}</style><body></body>";
                browser.with_session(session.into()).call("Fetch.fulfillRequest",json!({"requestId":event["params"]["requestId"],"responseCode":200,"responseHeaders":[{"name":"Content-Type","value":"text/html"}],"body":STANDARD.encode(body)}),SHORT,&CancellationToken::new()).await?;
            }
            Ok::<_,anyhow::Error>(())
        });
        let checked=std::panic::AssertUnwindSafe(async {
            for url in ["https://PUBLIC.invalid/","https://grok.com/","https://chatgpt.com/?temporary-chat=true"] {
            let origin=reqwest::Url::parse(url)?.origin().ascii_serialization();
            page.call("Page.navigate",json!({"url":url}),SHORT,&cancel).await?;
            let deadline=tokio::time::Instant::now()+SHORT;
            loop {
                let loaded=page.evaluate_with_arguments("(cfg)=>({origin:location.origin,ready:document.readyState,body:Boolean(document.body)})",&json!([{}]),SHORT,&cancel).await?;
                if loaded["origin"]==origin&&loaded["ready"]=="complete"&&loaded["body"]==true {break;}
                anyhow::ensure!(tokio::time::Instant::now()<deadline,"Fixture navigation timed out");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if origin!="https://chatgpt.com" {
                mount(&page,ACTIVE,"/","").await?;
                anyhow::ensure!(require(&page,SHORT,&cancel).await.is_err(),"Non-ChatGPT origin proved Temporary Chat");
            }
            if origin=="https://grok.com" {
                crate::browser_grok::tests::native_cases(&page, &chrome.endpoint).await.context("Native Grok Private/witness regression")?;
            }
            }
            let file=temp.path().join("PUBLIC-context.txt");
            std::fs::write(&file,"PUBLIC synthetic attachment")?;
            // Only a hang guard: the cases took 32 s on a 2-worker Linux host (26 October).
            tokio::time::timeout(Duration::from_secs(120),cases(&page,&file)).await.context("Privacy fixture deadline reached")?
        }).catch_unwind().await.unwrap_or_else(|_| Err(anyhow::anyhow!("PUBLIC privacy fixture panicked")));
        interceptor.abort();
        let _=interceptor.await;
        checked
    }.await;
    // This profile and every page in this browser were created by this fixture.
    let cleanup = async {
        let rows = chrome
            .browser
            .call(
                "Target.getTargets",
                json!({}),
                SHORT,
                &CancellationToken::new(),
            )
            .await?;
        for row in rows["targetInfos"]
            .as_array()
            .context("Missing fixture target list")?
        {
            let target = row["targetId"]
                .as_str()
                .context("Fixture target has no identity")?;
            match chrome
                .browser
                .close_owned_target(target, SHORT, &CancellationToken::new())
                .await
            {
                Ok(closed) => anyhow::ensure!(
                    closed["success"] == true,
                    "Fixture target close was not acknowledged: {}",
                    row["type"]
                ),
                Err(error) => {
                    // Chrome may retire another fixture target after the page closes.
                    // Absence is sufficient cleanup evidence; a present target is not.
                    let remaining = chrome
                        .browser
                        .call(
                            "Target.getTargets",
                            json!({}),
                            SHORT,
                            &CancellationToken::new(),
                        )
                        .await?;
                    anyhow::ensure!(
                        remaining["targetInfos"]
                            .as_array()
                            .context("Missing fixture targets after close")?
                            .iter()
                            .all(|row| row["targetId"] != target),
                        "Fixture target remains after close refusal: {error:#}"
                    );
                }
            }
        }
        let deadline = tokio::time::Instant::now() + SHORT;
        loop {
            match chrome.terminate_owned(Duration::from_secs(1)).await {
                Ok(()) => break Ok(()),
                Err(error) if tokio::time::Instant::now() >= deadline => {
                    let remaining = chrome
                        .browser
                        .call(
                            "Target.getTargets",
                            json!({}),
                            SHORT,
                            &CancellationToken::new(),
                        )
                        .await?;
                    let kinds: Vec<_> = remaining["targetInfos"]
                        .as_array()
                        .context("Missing remaining fixture targets")?
                        .iter()
                        .map(|row| row["type"].clone())
                        .collect();
                    break Err(error.context(format!("Remaining fixture target kinds: {kinds:?}")));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }
    .await;
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Fixture cleanup also failed: {cleanup:#}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

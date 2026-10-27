use super::*;
use crate::{artifacts::sha256, browser_renderer::RendererReservation};

const SHORT: Duration = Duration::from_secs(5);
const PROMPT: &str = "PUBLIC Grok request";
const CLIENT: &str = "00000000-0000-4000-8000-000000000001";
const USER: &str = "00000000-0000-4000-8000-000000000002";
const RESPONSE: &str = "00000000-0000-4000-8000-000000000003";
const DISPATCH: &str = "00000000-0000-4000-8000-000000000004";

#[test]
fn live_witness_extends_only_the_same_saved_commitment() {
    let completed = json!({"version":1,"provider":"grok","kind":"grok-websocket-v1","state":"completed",
        "claimId":CLIENT,"reservationId":USER,"dispatchId":DISPATCH,"clientMessageId":CLIENT,
        "userMessageId":format!("response-{USER}"),"responseId":RESPONSE,
        "assistantMessageId":format!("response-{RESPONSE}"),"promptSha256":sha256(PROMPT.as_bytes()),
        "mode":"Auto","privateChat":true});
    let mut committed = completed.clone();
    committed["state"] = json!("committed");
    committed["responseId"] = Value::Null;
    committed["assistantMessageId"] = Value::Null;
    let mut created = committed.clone();
    created["responseId"] = json!(RESPONSE);
    for (saved, observed) in [
        (&committed, &committed),
        (&committed, &created),
        (&committed, &completed),
        (&created, &completed),
        (&completed, &completed),
    ] {
        assert!(extends(saved, observed), "{saved} {observed}");
    }
    for (saved, observed) in [(&completed, &committed), (&created, &committed)] {
        assert!(!extends(saved, observed), "{saved} {observed}");
    }
    let other = "00000000-0000-4000-8000-000000000099";
    for (key, changed) in [
        ("version", json!(2)),
        ("provider", json!("chatgpt")),
        ("kind", json!("new-user-message")),
        ("claimId", json!(other)),
        ("reservationId", json!(other)),
        ("userMessageId", json!(format!("response-{other}"))),
        ("clientMessageId", json!(other)),
        ("dispatchId", json!(other)),
        ("promptSha256", json!(sha256(b"PUBLIC other"))),
        ("mode", json!("Fast")),
        ("privateChat", json!(false)),
        ("responseId", json!(other)),
        ("state", json!("invalid")),
        ("PUBLIC-extra", json!(true)),
    ] {
        let mut observed = completed.clone();
        observed[key] = changed;
        assert!(!extends(&created, &observed), "{key}");
    }
    let mut answer = completed.clone();
    answer["assistantMessageId"] = json!(format!("response-{other}"));
    assert!(!extends(&completed, &answer));
    let mut missing = completed.clone();
    missing.as_object_mut().unwrap().remove("mode");
    assert!(!extends(&committed, &missing));
}

#[test]
fn list_lines_match_the_page_rule() {
    // The expected values are the output of these exact grok-dom.js lines under Node.
    for source in [
        r"  const marker = /^(?:[-*+]|\d{1,9}[.)])\s+/;",
        r"  const lines = value => value.replace(/\r\n/g, '\n').split('\n').map(line => line.trim()).filter(Boolean);",
    ] {
        assert!(DOM.contains(source), "{source}");
    }
    for (text, listed, lines) in [
        (
            "PUBLIC brief.\n\nEVIDENCE:\n- First item\n- Second item\n\nQUESTION:\n1. Is it sound?\n2. What next?",
            Some(
                "PUBLIC brief.\nEVIDENCE:\nFirst item\nSecond item\nQUESTION:\nIs it sound?\nWhat next?",
            ),
            "PUBLIC brief.\nEVIDENCE:\n- First item\n- Second item\nQUESTION:\n1. Is it sound?\n2. What next?",
        ),
        (
            "PUBLIC brief.\n\nEVIDENCE:\n\nFirst item\nSecond item\n\nQUESTION:\n\nIs it sound?\nWhat next?",
            None,
            "PUBLIC brief.\nEVIDENCE:\nFirst item\nSecond item\nQUESTION:\nIs it sound?\nWhat next?",
        ),
        ("", None, ""),
        ("plain\r\nlines\rhere", None, "plain\nlines\rhere"),
        ("a\r\r\nb", None, "a\nb"),
        (
            "123456789. nine\n1234567890. ten\n1)x\n2) y\n-x\n+ z\n* w\n-",
            Some("nine\n1234567890. ten\n1)x\ny\n-x\nz\nw\n-"),
            "123456789. nine\n1234567890. ten\n1)x\n2) y\n-x\n+ z\n* w\n-",
        ),
        (
            "# heading\n> quote\n  - indented\n\t-\ttab\n- - double",
            Some("# heading\n> quote\nindented\ntab\n- double"),
            "# heading\n> quote\n- indented\n-\ttab\n- - double",
        ),
        (
            "- First **item**\n- Second [link](https://example.test/a)",
            Some("First **item**\nSecond [link](https://example.test/a)"),
            "- First **item**\n- Second [link](https://example.test/a)",
        ),
        (
            "\u{feff}- bom\u{85}line\n\u{3000}- wide\u{2028}split\n\u{a0} 1. nbsp",
            Some("bom\u{85}line\nwide\u{2028}split\nnbsp"),
            "- bom\u{85}line\n- wide\u{2028}split\n1. nbsp",
        ),
        (
            "\u{661}. arabic\n\u{ff11}. wide\n-\u{a0}nbsp marker",
            Some("\u{661}. arabic\n\u{ff11}. wide\nnbsp marker"),
            "\u{661}. arabic\n\u{ff11}. wide\n-\u{a0}nbsp marker",
        ),
        (
            "x\u{85}\n\u{180e}x\n\u{200b}x",
            None,
            "x\u{85}\n\u{180e}x\n\u{200b}x",
        ),
        (
            "x\u{2028}\n\u{b}x\u{c}\n\u{1680}x\u{200a}\nx\u{202f}\u{205f}",
            None,
            "x\nx\nx\nx",
        ),
        (
            "-\u{85}x\n1.\u{2029}y\n+\u{feff}z",
            Some("-\u{85}x\ny\nz"),
            "-\u{85}x\n1.\u{2029}y\n+\u{feff}z",
        ),
    ] {
        assert_eq!(listed_prompt(text).as_deref(), listed, "{text:?}");
        assert_eq!(turn_lines(text), lines, "{text:?}");
    }
}

#[test]
fn grok_receipts_require_their_separate_private_message_identity() {
    let owner = SubmissionOwner {
        claim_id: uuid::Uuid::new_v4().to_string(),
        reservation_id: uuid::Uuid::new_v4().to_string(),
    };
    let value = json!({"version":1,"provider":"grok","kind":"grok-websocket-v1","state":"completed",
        "claimId":owner.claim_id,"reservationId":owner.reservation_id,"dispatchId":DISPATCH,
        "clientMessageId":CLIENT,"userMessageId":format!("response-{USER}"),"responseId":RESPONSE,
        "assistantMessageId":format!("response-{RESPONSE}"),"promptSha256":sha256(PROMPT.as_bytes()),"mode":"Auto","privateChat":true});
    require_receipt(&value, &owner).unwrap();
    let intent = Intent::new(PROMPT, "Auto", DISPATCH).unwrap();
    intent.require(&value, &owner).unwrap();
    let observed = json!({"state":"completed","provider":"grok","kind":"grok-private-answer",
        "privateChat":true,"mode":"Auto","wireReceipt":value,"userMessageId":value["userMessageId"],
        "assistantMessageId":value["assistantMessageId"],"completionConfirmed":true,"stopExists":false,
        "strongThinkingActive":false,"userText":PROMPT,"text":"PUBLIC complete answer",
        "providerText":"PUBLIC complete answer","images":[],"files":[]});
    intent.require_answer(&value, &owner, &observed).unwrap();
    // A prompt without list-marker lines has no rendered-list form.
    let mut unlisted = observed.clone();
    unlisted["promptMatch"] = json!("rendered-list");
    assert!(intent.require_answer(&value, &owner, &unlisted).is_err());
    let listed = "PUBLIC brief:\n- one item\n- two [link](https://example.test/a)";
    let list_intent = Intent::new(listed, "Auto", DISPATCH).unwrap();
    let mut list_value = value.clone();
    list_value["promptSha256"] = json!(sha256(listed.as_bytes()));
    let mut list_observed = observed.clone();
    list_observed["wireReceipt"] = list_value.clone();
    let rendered = "PUBLIC brief:\none item\ntwo [link](https://example.test/a)";
    for (kind, source, accepted) in [
        (None, listed, true),
        (
            Some(json!("rendered-list")),
            "PUBLIC brief:\n\none item\ntwo [link](https://example.test/a)\n",
            true,
        ),
        // Rendered inline Markdown, a lost destination, joined lines and changed words fail closed.
        (
            Some(json!("rendered-list")),
            "PUBLIC brief:\none item\ntwo link",
            false,
        ),
        (
            Some(json!("rendered-list")),
            "PUBLIC brief: one item\ntwo [link](https://example.test/a)",
            false,
        ),
        (
            Some(json!("rendered-list")),
            "PUBLIC brief:\none item\ntwo [link](https://example.test/b)",
            false,
        ),
        (None, rendered, false),
        // grok-dom.js never names an exact match; other or malformed kinds fail closed.
        (Some(json!("exact")), listed, false),
        (Some(json!(null)), listed, false),
        (Some(json!("rendered-markdown")), rendered, false),
    ] {
        let mut changed_value = list_observed.clone();
        if let Some(kind) = &kind {
            changed_value["promptMatch"] = kind.clone();
        }
        changed_value["userText"] = json!(source);
        assert_eq!(
            list_intent
                .require_answer(&list_value, &owner, &changed_value)
                .is_ok(),
            accepted,
            "{kind:?} {source:?}"
        );
    }
    for (key, changed) in [
        ("wireReceipt", json!(null)),
        ("privateChat", json!(false)),
        ("mode", json!("Fast")),
        (
            "userMessageId",
            json!("response-00000000-0000-4000-8000-000000000099"),
        ),
        ("assistantMessageId", value["userMessageId"].clone()),
        ("completionConfirmed", json!(false)),
        ("stopExists", json!(true)),
        ("strongThinkingActive", json!(true)),
        ("userText", json!("PUBLIC altered source")),
        ("text", json!("")),
        ("providerText", json!("PUBLIC partial")),
        ("images", json!([{"src":"PUBLIC"}])),
        ("files", json!([{"name":"PUBLIC.txt"}])),
        ("images", json!(null)),
        ("files", json!(null)),
    ] {
        let mut changed_value = observed.clone();
        changed_value[key] = changed;
        assert!(
            intent
                .require_answer(&value, &owner, &changed_value)
                .is_err(),
            "answer {key}"
        );
    }
    for (key, changed) in [
        ("dispatchId", json!(CLIENT)),
        ("promptSha256", json!("0".repeat(64))),
        ("mode", json!("Fast")),
    ] {
        let mut other = value.clone();
        other[key] = changed;
        assert!(intent.require(&other, &owner).is_err(), "{key}");
    }
    for (key, changed) in [
        ("provider", json!("chatgpt")),
        ("kind", json!("new-user-message")),
        ("claimId", json!(uuid::Uuid::new_v4().to_string())),
        ("reservationId", json!(uuid::Uuid::new_v4().to_string())),
        ("dispatchId", json!("unknown")),
        ("clientMessageId", json!(null)),
        ("userMessageId", json!(USER)),
        ("assistantMessageId", value["userMessageId"].clone()),
        ("responseId", json!(null)),
        ("promptSha256", json!("F".repeat(64))),
        ("privateChat", json!(false)),
        ("mode", json!("Expert")),
        ("state", json!("pending")),
    ] {
        let mut changed_value = value.clone();
        changed_value[key] = changed;
        assert!(require_receipt(&changed_value, &owner).is_err(), "{key}");
    }
}

async fn entry_mount(page: &Cdp, html: &str, setup: &str) -> Result<()> {
    page.evaluate_with_arguments(
        "(cfg)=>{window.__oracleGrokWitness?.stop();delete window.__oracleGrokWitness;sessionStorage.removeItem('oracle:target-claim');document.body.innerHTML=cfg.html;window.PUBLIC_activations=0;window.PUBLIC_entry_claim=null;return true;}",
        &json!([{"html":html}]), SHORT, &CancellationToken::new(),
    ).await?;
    if !setup.is_empty() {
        page.evaluate(setup, SHORT, &CancellationToken::new())
            .await?;
    }
    Ok(())
}
async fn entry_cases(page: &Cdp) -> Result<()> {
    let ordinary = "<div class='query-bar'><textarea></textarea><a aria-label='Switch to Private Chat'>Private</a></div>";
    let active = ordinary.replace("Switch to Private Chat", "Switch to Default Chat");
    let cancel = CancellationToken::new();
    for delayed in [false, true] {
        entry_mount(page, ordinary, &format!("document.querySelector('a').onclick=()=>{{PUBLIC_activations++;PUBLIC_entry_claim=sessionStorage.getItem('oracle:target-claim');{} }};true", if delayed {
            "setTimeout(()=>{document.body.innerHTML=\"<div class='query-bar bg-surface-incognito'><textarea></textarea><a aria-label='Switch to Default Chat'>Private</a></div>\";},30);"
        } else { "document.querySelector('a').setAttribute('aria-label','Switch to Default Chat');" })).await?;
        let mut reservation = RendererReservation::acquire_initial_grok(
            page.clone(),
            uuid::Uuid::new_v4().to_string(),
            SHORT,
            &cancel,
        )
        .await?;
        assert_eq!(
            page.evaluate(
                "PUBLIC_activations===1&&PUBLIC_entry_claim===null",
                SHORT,
                &cancel
            )
            .await?,
            true
        );
        require(page, SHORT, &cancel).await?;
        reservation.release_clean(SHORT).await?;
    }
    entry_mount(page, &active, "").await?;
    enter_private(page, SHORT, &cancel).await?;
    assert_eq!(
        page.evaluate("PUBLIC_activations", SHORT, &cancel).await?,
        0
    );
    for html in [
        ordinary.replace(
            "</div>",
            "<a aria-label='Switch to Private Chat'>Duplicate</a></div>",
        ),
        ordinary.replace("<a aria-label", "<a aria-disabled='true' aria-label"),
        ordinary.replace("<textarea>", "<textarea readonly>"),
        ordinary.replace(
            "<textarea></textarea>",
            "<textarea></textarea><textarea></textarea>",
        ),
        ordinary.replace(
            "<textarea></textarea>",
            "<textarea>PUBLIC retained prompt</textarea>",
        ),
        ordinary.replace(
            "</div>",
            "<button aria-label='Open attachment'>PUBLIC.txt</button></div>",
        ),
        format!(
            "{ordinary}<article id='response-PUBLIC'><div data-testid='user-message'>PUBLIC retained source</div></article>"
        ),
        format!(
            "{ordinary}<article data-message-author-role='user'>PUBLIC retained source</article>"
        ),
        format!("{active}<article data-role='assistant'>PUBLIC retained answer</article>"),
        format!("<article data-message-author-role='assistant'>{ordinary}</article>"),
        ordinary.replace(
            "</div>",
            "<a aria-label='Switch to Default Chat'>Contradictory</a></div>",
        ),
    ] {
        entry_mount(page, &html, "").await?;
        assert!(
            enter_private(page, Duration::from_millis(100), &cancel)
                .await
                .is_err(),
            "Accepted initial Grok fixture: {html}"
        );
        assert_eq!(
            page.evaluate(
                "sessionStorage.getItem('oracle:target-claim')===null&&PUBLIC_activations===0",
                SHORT,
                &cancel
            )
            .await?,
            true
        );
    }
    entry_mount(
        page,
        &active,
        "sessionStorage.setItem('oracle:target-claim',JSON.stringify({id:'PUBLIC foreign'}));true",
    )
    .await?;
    assert!(enter_private(page, SHORT, &cancel).await.is_err());
    assert_eq!(
        page.evaluate(
            "JSON.parse(sessionStorage.getItem('oracle:target-claim')).id",
            SHORT,
            &cancel
        )
        .await?,
        "PUBLIC foreign"
    );
    entry_mount(
        page,
        ordinary,
        "document.querySelector('a').onclick=()=>PUBLIC_activations++;true",
    )
    .await?;
    assert!(
        enter_private(page, Duration::from_millis(150), &cancel)
            .await
            .is_err()
    );
    assert_eq!(
        page.evaluate(
            "PUBLIC_activations===1&&sessionStorage.getItem('oracle:target-claim')===null",
            SHORT,
            &cancel
        )
        .await?,
        true
    );
    entry_mount(page, ordinary, "").await?;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(enter_private(page, SHORT, &cancelled).await.is_err());
    assert_eq!(
        page.evaluate("PUBLIC_activations", SHORT, &cancel).await?,
        0
    );
    Ok(())
}

async fn setup(page: &Cdp, mode: &str) -> Result<RendererReservation> {
    setup_with_lifetime(page, mode, Duration::from_secs(60)).await
}
async fn setup_with_lifetime(
    page: &Cdp,
    mode: &str,
    lifetime: Duration,
) -> Result<RendererReservation> {
    let mut reservation = composer(page, mode).await?;
    reservation
        .dispatch_started(SHORT, &CancellationToken::new())
        .await?;
    reservation
        .install_grok_witness(
            PROMPT,
            mode,
            DISPATCH,
            lifetime,
            SHORT,
            &CancellationToken::new(),
        )
        .await?;
    Ok(reservation)
}
async fn composer(page: &Cdp, mode: &str) -> Result<RendererReservation> {
    page.evaluate_with_arguments("(cfg)=>{window.__oracleGrokWitness?.stop();delete window.__oracleGrokWitness;sessionStorage.removeItem('oracle:target-claim');document.body.innerHTML='<div class=\"query-bar bg-surface-incognito\"><textarea></textarea><a aria-label=\"Switch to Default Chat\">Private</a><button aria-haspopup=\"menu\">'+cfg.mode+'</button><button aria-label=\"Send message\">Send</button></div>';class PublicSocket extends EventTarget {constructor(){super();this.url='wss://grok.com/PUBLIC-fixture';}send(raw){window.PUBLIC_sent.push(raw);}emit(value){this.dispatchEvent(new MessageEvent('message',{data:typeof value==='string'?value:JSON.stringify(value)}));}};window.WebSocket=PublicSocket;window.PUBLIC_sent=[];window.PUBLIC_socket=new WebSocket();document.querySelector('textarea').value='PUBLIC Grok request';document.querySelector('button[aria-label=\"Send message\"]').onclick=()=>PUBLIC_socket.send(JSON.stringify({type:'response.create',item:{type:'message',role:'user',x_grok:{client_message_id:{uuid:'00000000-0000-4000-8000-000000000001'},input_chunks:[{text:{text:document.querySelector('textarea').value}}]}}}));return true;}", &json!([{"mode":mode}]),SHORT,&CancellationToken::new()).await?;
    RendererReservation::acquire_grok(
        page.clone(),
        uuid::Uuid::new_v4().to_string(),
        SHORT,
        &CancellationToken::new(),
    )
    .await
}

async fn mount_answer(page: &Cdp) -> Result<()> {
    page.evaluate_with_arguments("(cfg)=>{for(const [role,id,value] of [['user',cfg.user,cfg.prompt],['assistant',cfg.assistant,'PUBLIC verified answer']]){const root=document.createElement('article');root.id=id;root.dataset.role=role;const content=document.createElement('div');content.dataset.testid='message-content';content.textContent=value;root.append(content);if(role==='assistant'){for(const label of ['Copy','Regenerate']){const button=document.createElement('button');button.setAttribute('aria-label',label);button.textContent=label;root.append(button);}}document.body.append(root);}return true;}",
        &json!([{"user":format!("response-{USER}"),"assistant":format!("response-{RESPONSE}"),"prompt":PROMPT}]),SHORT,&CancellationToken::new()).await?;
    Ok(())
}

async fn mode_menu(page: &Cdp, auto: bool, ambiguous: bool) -> Result<()> {
    page.evaluate_with_arguments("(cfg)=>{window.PUBLIC_auto=cfg.auto;window.PUBLIC_ambiguous=cfg.ambiguous;const trigger=document.querySelector('button[aria-haspopup]');trigger.onclick=()=>{trigger.setAttribute('aria-expanded','true');const menu=document.createElement('div');menu.setAttribute('role','menu');for(const name of (PUBLIC_ambiguous?['Auto','Auto','Fast']:['Auto','Fast'])){const choice=document.createElement('button');choice.setAttribute('role','menuitemradio');choice.textContent=name;if(name==='Auto'&&!PUBLIC_auto)choice.disabled=true;choice.onclick=()=>{trigger.textContent=name;trigger.setAttribute('aria-expanded','false');menu.remove();};menu.append(choice);}document.body.append(menu);};return true;}", &json!([{"auto":auto,"ambiguous":ambiguous}]),SHORT,&CancellationToken::new()).await?;
    Ok(())
}

async fn dom_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    // Match the native runner's focus emulation. A background disposable page
    // can otherwise report an active textarea without delivering focus events.
    page.call(
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled":true}),
        SHORT,
        &cancel,
    )
    .await?;
    for (auto, expected, rich) in [
        (true, "Auto", false),
        (false, "Fast", false),
        (true, "Auto", true),
    ] {
        let mut prepared = composer(page, "Expert").await?;
        if rich {
            page.evaluate("document.querySelector('textarea').outerHTML='<div contenteditable=\"true\" role=\"textbox\"></div>';true",SHORT,&cancel).await?;
        }
        mode_menu(page, auto, false).await?;
        let prompt = "PUBLIC <literal>& source\nUnicode café 日本語";
        assert_eq!(
            prepared.prepare_grok_prompt(prompt, SHORT, &cancel).await?,
            expected
        );
        assert_eq!(
            page.evaluate("{const editor=document.querySelector('textarea,[contenteditable]');editor instanceof HTMLTextAreaElement?editor.value:editor.innerText}", SHORT, &cancel)
                .await?,
            prompt
        );
        assert_eq!(
            page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
            0
        );
        prepared.release_clean(SHORT).await?;
    }
    for fault in ["ambiguous-mode", "privacy-on-focus", "owner-on-input"] {
        let mut prepared = composer(page, "Auto").await?;
        mode_menu(page, true, fault == "ambiguous-mode").await?;
        if fault == "privacy-on-focus" {
            page.evaluate("document.querySelector('textarea').onfocus=()=>{document.querySelector('.query-bar').classList.remove('bg-surface-incognito');document.querySelector('a').remove();};true",SHORT,&cancel).await?;
        } else if fault == "owner-on-input" {
            page.evaluate("document.querySelector('textarea').oninput=()=>sessionStorage.removeItem('oracle:target-claim');true",SHORT,&cancel).await?;
        }
        let preparation = prepared
            .prepare_grok_prompt("PUBLIC changed prompt", SHORT, &cancel)
            .await;
        assert!(preparation.is_err(), "{fault}: {preparation:?}; state={:?}", page.evaluate("JSON.stringify({privateControl:!!document.querySelector('a[aria-label=\"Switch to Default Chat\"]'),activeTag:document.activeElement?.tagName,hasFocus:document.hasFocus(),visibility:document.visibilityState,value:document.querySelector('textarea')?.value})",SHORT,&cancel).await?);
        assert_eq!(
            page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
            0,
            "{fault}"
        );
        if fault != "owner-on-input" {
            assert_eq!(
                page.evaluate("document.querySelector('textarea').value", SHORT, &cancel)
                    .await?,
                PROMPT,
                "{fault}"
            );
        }
    }
    for fault in [
        "none",
        "user-source",
        "assistant-id",
        "duplicate-user",
        "image",
        "file",
        "copy-pending",
        "streaming-pending",
        "private",
        "mode",
        "content-siblings",
        "role-siblings",
        "hidden-image",
        "outside-image",
        "download-button",
    ] {
        let mut reservation = setup(page, "Auto").await?;
        send(&mut reservation).await?;
        emit(page, &[user()]).await?;
        let committed = reservation
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?;
        reservation
            .acknowledge_grok(&committed, SHORT, &cancel)
            .await?;
        assert_eq!(
            reservation
                .observe_grok_answer(PROMPT, SHORT, &cancel)
                .await?["state"],
            "pending"
        );
        emit(page, &frames()[1..]).await?;
        assert_eq!(
            reservation
                .observe_grok_answer(PROMPT, SHORT, &cancel)
                .await?["state"],
            "pending"
        );
        mount_answer(page).await?;
        let expression = match fault {
            "none" => "true",
            "user-source" => {
                "document.querySelector('[data-role=user] [data-testid]').textContent='PUBLIC changed source';true"
            }
            "assistant-id" => {
                "document.querySelector('[data-role=assistant]').id='response-00000000-0000-4000-8000-000000000099';true"
            }
            "duplicate-user" => {
                "document.body.append(document.querySelector('[data-role=user]').cloneNode(true));true"
            }
            "image" => {
                "{const image=document.createElement('img');image.src='data:image/png;base64,iVBORw0KGgo=';image.width=20;image.height=20;document.querySelector('[data-role=assistant] [data-testid]').append(image);}true"
            }
            "file" => {
                "{const link=document.createElement('a');link.setAttribute('download','PUBLIC.txt');link.href='data:text/plain,PUBLIC';link.textContent='PUBLIC.txt';document.querySelector('[data-role=assistant] [data-testid]').append(link);}true"
            }
            "content-siblings" => {
                "{const sibling=document.createElement('div');sibling.className='markdown';sibling.textContent='PUBLIC omitted content';document.querySelector('[data-role=assistant]').append(sibling);}true"
            }
            "role-siblings" => {
                "{const sibling=document.createElement('div');sibling.dataset.role='assistant';sibling.textContent='PUBLIC omitted role content';document.querySelector('[data-role=assistant]').append(sibling);}true"
            }
            "hidden-image" => {
                "{const image=document.createElement('img');image.hidden=true;document.querySelector('[data-role=assistant]').append(image);}true"
            }
            "outside-image" => {
                "{const image=document.createElement('img');image.width=20;image.height=20;document.querySelector('[data-role=assistant]').append(image);}true"
            }
            "download-button" => {
                "{const button=document.createElement('button');button.setAttribute('aria-label','Download PUBLIC file');button.textContent='PUBLIC.txt';document.querySelector('[data-role=assistant]').append(button);}true"
            }
            "copy-pending" => "document.querySelector('button[aria-label=Copy]').remove();true",
            "streaming-pending" => {
                "document.querySelector('[data-role=assistant]').dataset.state='streaming';true"
            }
            "private" => {
                "document.querySelector('.query-bar').classList.remove('bg-surface-incognito');document.querySelector('a').remove();true"
            }
            "mode" => "document.querySelector('button[aria-haspopup]').textContent='Fast';true",
            _ => unreachable!(),
        };
        page.evaluate(expression, SHORT, &cancel).await?;
        let result = reservation
            .observe_grok_answer(PROMPT, SHORT, &cancel)
            .await;
        match fault {
            "none" => assert_eq!(result?["text"], "PUBLIC verified answer"),
            "copy-pending" | "streaming-pending" => assert_eq!(result?["state"], "pending"),
            _ => assert!(result.is_err(), "{fault}"),
        }
        reservation
            .grok_witness(DISPATCH, true, SHORT, &cancel)
            .await?;
        assert_eq!(
            page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
            1,
            "{fault}"
        );
    }
    Ok(())
}

async fn send(reservation: &mut RendererReservation) -> Result<()> {
    reservation
        .send_grok_once(PROMPT, SHORT, &CancellationToken::new())
        .await
}

async fn completed_answer(page: &Cdp) -> Result<(RendererReservation, Value, Value)> {
    let cancel = CancellationToken::new();
    let mut reservation = setup(page, "Auto").await?;
    send(&mut reservation).await?;
    emit(page, &[user()]).await?;
    let committed = reservation
        .grok_witness(DISPATCH, false, SHORT, &cancel)
        .await?;
    reservation
        .acknowledge_grok(&committed, SHORT, &cancel)
        .await?;
    emit(page, &frames()[1..]).await?;
    mount_answer(page).await?;
    let observed = reservation
        .observe_grok_answer(PROMPT, SHORT, &cancel)
        .await?;
    Ok((reservation, committed, observed))
}

async fn retirement_cases(page: &Cdp) -> Result<()> {
    let cancel = CancellationToken::new();
    let mut unfinished = setup(page, "Auto").await?;
    send(&mut unfinished).await?;
    emit(page, &[user()]).await?;
    let receipt = unfinished
        .grok_witness(DISPATCH, false, SHORT, &cancel)
        .await?;
    unfinished
        .acknowledge_grok(&receipt, SHORT, &cancel)
        .await?;
    assert!(unfinished.release_clean(SHORT).await.is_err());
    assert_eq!(
        page.evaluate(
            "JSON.parse(sessionStorage.getItem('oracle:target-claim')).submission.state",
            SHORT,
            &cancel
        )
        .await?,
        "quarantined"
    );
    assert_eq!(
        page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
        1
    );
    for case in [
        "active",
        "sealed",
        "clean",
        "claim",
        "reservation",
        "inspection",
        "marker",
        "receipt",
        "missing-witness",
        "incomplete",
        "privacy",
        "source-after",
        "answer-after",
        "mode-after",
        "owner-after",
        "asset-after",
    ] {
        let (mut reservation, committed, observed) = completed_answer(page).await?;
        let marker = uuid::Uuid::new_v4().to_string();
        let mut cfg = json!({"owner":reservation.owner(),"retirementId":marker,"committedReceipt":committed,"receipt":observed["wireReceipt"],
            "action":"observe-retirement","mode":"Auto","prompt":PROMPT});
        if case == "sealed" {
            reservation
                .grok_witness(DISPATCH, true, SHORT, &cancel)
                .await?;
        } else if case == "clean" {
            reservation.release_clean(SHORT).await?;
        }
        match case {
            "claim" => cfg["owner"]["claimId"] = json!(uuid::Uuid::new_v4().to_string()),
            "reservation" => {
                cfg["owner"]["reservationId"] = json!(uuid::Uuid::new_v4().to_string())
            }
            "receipt" => {
                cfg["committedReceipt"]["clientMessageId"] = json!(uuid::Uuid::new_v4().to_string())
            }
            "incomplete" => cfg["receipt"]["state"] = json!("committed"),
            "inspection" => {
                page.evaluate("{const key='oracle:target-claim',claim=JSON.parse(sessionStorage.getItem(key));claim.inspection={token:'PUBLIC',parentReservationId:claim.submission.id,rendererId:'PUBLIC',state:'quarantined'};sessionStorage.setItem(key,JSON.stringify(claim));}true", SHORT, &cancel).await?;
            }
            "marker" => {
                page.evaluate("{const key='oracle:target-claim',claim=JSON.parse(sessionStorage.getItem(key));claim.retiring=true;claim.retirementId='PUBLIC-foreign';sessionStorage.setItem(key,JSON.stringify(claim));}true", SHORT, &cancel).await?;
            }
            "missing-witness" => {
                page.evaluate(
                    "window.__oracleGrokWitness.stop();delete window.__oracleGrokWitness;true",
                    SHORT,
                    &cancel,
                )
                .await?;
            }
            "privacy" => {
                page.evaluate("document.querySelector('.query-bar').classList.remove('bg-surface-incognito');document.querySelector('a').remove();true",SHORT,&cancel).await?;
            }
            _ => {}
        }
        let marked = page
            .evaluate_with_arguments(&retirement_mark(), &json!([cfg.clone()]), SHORT, &cancel)
            .await;
        if matches!(
            case,
            "active"
                | "sealed"
                | "clean"
                | "source-after"
                | "answer-after"
                | "mode-after"
                | "owner-after"
                | "asset-after"
        ) {
            assert_eq!(marked?, true, "{case}");
            // Marking seals the witness and releases its timer/listeners before
            // the DOM read. It cannot be used as live ACK authority afterwards.
            assert!(
                page.evaluate("window.__oracleGrokWitness.acknowledge()", SHORT, &cancel)
                    .await
                    .is_err(),
                "{case}"
            );
            let changed = match case {
                "source-after" => {
                    "document.querySelector('[data-role=user] [data-testid]').textContent='PUBLIC changed source';true"
                }
                "answer-after" => {
                    "document.querySelector('[data-role=assistant] [data-testid]').textContent='PUBLIC changed answer';true"
                }
                "mode-after" => {
                    "document.querySelector('button[aria-haspopup]').textContent='Fast';true"
                }
                "owner-after" => "sessionStorage.removeItem('oracle:target-claim');true",
                "asset-after" => {
                    "document.querySelector('[data-role=assistant]').append(document.createElement('img'));true"
                }
                _ => "true",
            };
            page.evaluate(changed, SHORT, &cancel).await?;
            let read = page
                .evaluate_with_arguments(&guard(DOM), &json!([cfg.clone()]), SHORT, &cancel)
                .await;
            if matches!(case, "active" | "sealed" | "clean") {
                assert_eq!(read?, observed, "{case}");
                assert_eq!(
                    page.evaluate_with_arguments(&retirement_mark(), &json!([cfg]), SHORT, &cancel)
                        .await?,
                    true,
                    "Repeated {case}"
                );
            } else if case == "answer-after" {
                assert_ne!(read?["text"], observed["text"]);
            } else {
                assert!(read.is_err(), "{case}");
            }
        } else {
            assert!(!marked.is_ok_and(|value| value == true), "{case}");
        }
        assert_eq!(
            page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
            1,
            "{case}"
        );
    }
    Ok(())
}

async fn retirement_close_cases(
    page: &Cdp,
    endpoint: &crate::browser_chrome::AttachmentEndpoint,
) -> Result<()> {
    use crate::{
        browser_disposition::{TargetCreation, TargetJournal},
        process::ProcessIdentity,
        sessions::SessionStore,
    };
    use anyhow::{Context, ensure};
    let browser = page.browser_scope();
    let cancel = CancellationToken::new();
    let baseline = browser
        .call("Target.getTargets", json!({}), SHORT, &cancel)
        .await?;
    // Pages only: Chrome's own service-worker targets come and go.
    let ids = |rows: &Value| {
        rows["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["type"] == "page")
            .map(|row| row["targetId"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    for clean in [false, true] {
        let created = browser
            .call(
                "Target.createTarget",
                json!({"url":"about:blank"}),
                SHORT,
                &cancel,
            )
            .await?;
        let target = created["targetId"]
            .as_str()
            .context("Missing Grok fixture target")?;
        let owned = browser.attach_target(target, SHORT, &cancel).await?;
        owned
            .call(
                "Fetch.enable",
                json!({"patterns":[{"urlPattern":"*"}]}),
                SHORT,
                &cancel,
            )
            .await?;
        owned
            .call(
                "Page.navigate",
                json!({"url":"https://grok.com/"}),
                SHORT,
                &cancel,
            )
            .await?;
        let deadline = tokio::time::Instant::now() + SHORT;
        loop {
            if owned.evaluate("location.origin==='https://grok.com'&&document.readyState==='complete'&&!!document.body", SHORT, &cancel).await? == true {break;}
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Grok retirement fixture navigation deadline"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (mut reservation, committed, observed) = completed_answer(&owned).await?;
        let home = tempfile::tempdir()?;
        let store = SessionStore::new(home.path().join(".oracle"));
        let session = store.create(
            json!({"prompt":PROMPT,"model":"gpt-6-astra","engine":"browser"}),
            home.path(),
            None,
        )?;
        let id = session["id"]
            .as_str()
            .context("Missing Grok fixture session")?;
        let journal = TargetJournal::open(&store)?;
        let identity = ProcessIdentity::current()?;
        let creator = journal.record_creation(&TargetCreation::allocated(target), &json!({"host":endpoint.host,"port":endpoint.port,
            "browserWSEndpoint":endpoint.browser_ws_endpoint,"targetId":target,"claimId":reservation.owner().claim_id}), &identity, Some(id), None)?;
        crate::browser_disposition::completed_dispatch_fixture(&store, id, &creator);
        let capture = crate::artifacts::capture(
            &store,
            id,
            "captured-public.md",
            observed["text"].as_str().unwrap().as_bytes(),
            &cancel,
        )?;
        let dispatch_id = store.require(id)?["dispatch"]["reservationId"].clone();
        store.update(id, |_| Ok(Some(json!({"status":"completed","completedAt":"PUBLIC completion","recoveryRequired":true,
            "dispatch":{"state":"completed","reservationId":dispatch_id,"submissionOwner":reservation.owner()},
            "browser":{"provider":"grok","capturedAnswer":capture,"runtime":{"provider":"grok","controllerPid":identity.pid,
            "controllerProcessStartedAtMs":identity.started_at_ms,"submissionOwner":reservation.owner(),"grokCommitReceipt":committed,
            "submittedPromptHash":sha256(PROMPT.as_bytes()),"submittedPromptHashFormat":"grok-request-v1",
            "effectiveModel":"grok-web-auto","modelSelection":{"verified":true,"source":"grok-mode-picker","selectedMode":"Auto","resolvedLabel":"Grok Auto"},
            "acceptedObservation":observed,"targetDispositionPending":true,"ownedRecoveryTarget":{"targetId":target,"claimId":creator["target"]["claimId"],"creator":creator}}}}))))?;
        let commit = journal.session_commit(&creator, &store.require(id)?)?;
        journal.commit(&creator, &commit)?;
        journal.finalise(&commit)?;
        ensure!(
            journal.is_current_completed_owner(&journal.read(&creator["allocationId"])?),
            "Grok original controller lost completed authority"
        );
        if clean {
            reservation.release_clean(SHORT).await?;
        } else {
            owned.evaluate("document.querySelector('[data-role=assistant] [data-testid]').textContent='PUBLIC changed answer';true",SHORT,&cancel).await?;
            let retained = crate::browser_retirement::retire_committed(
                &store, &journal, &browser, endpoint, &creator,
            )
            .await?;
            ensure!(
                retained["decision"] == "retained" && retained["absence"] == "unknown",
                "Changed Grok answer obtained close authority"
            );
            ensure!(
                browser
                    .call(
                        "Target.getTargetInfo",
                        json!({"targetId":target}),
                        SHORT,
                        &cancel
                    )
                    .await?["targetInfo"]["targetId"]
                    == target,
                "Changed Grok target closed"
            );
            owned.evaluate("document.querySelector('[data-role=assistant] [data-testid]').textContent='PUBLIC verified answer';true",SHORT,&cancel).await?;
        }
        let disposed = crate::browser_retirement::retire_committed(
            &store, &journal, &browser, endpoint, &creator,
        )
        .await?;
        ensure!(
            disposed["decision"] == "retired" && disposed["absence"] == "confirmed-absent",
            "Exact completed Grok target was retained: {disposed}"
        );
        crate::browser_runner::settle_completed_retirement(
            &store,
            &journal,
            &creator,
            &disposed,
            std::time::Instant::now() + SHORT,
        )?;
        let saved = store.require(id)?;
        ensure!(
            saved["recoveryRequired"] == false
                && saved["browser"]["runtime"]["targetDispositionPending"] == false,
            "Grok cleanup failed to settle"
        );
        ensure!(
            ids(&browser
                .call("Target.getTargets", json!({}), SHORT, &cancel)
                .await?)
                == ids(&baseline),
            "Grok close changed the other fixture targets"
        );
        assert!(
            journal.read(&creator["allocationId"])?["commit"]
                .get("conversationId")
                .is_none()
        );
    }
    Ok(())
}

fn user() -> Value {
    json!({"type":"conversation.item.added","item":{"id":{"uuid":USER},"type":"message","role":"user","x_grok":{"client_message_id":{"uuid":CLIENT},"input_chunks":[{"text":{"text":PROMPT}}]}}})
}
fn frames() -> Vec<Value> {
    vec![
        user(),
        json!({"type":"response.created","response":{"id":{"uuid":RESPONSE},"status":"in_progress"}}),
        json!({"type":"response.output_item.added","response_id":{"uuid":RESPONSE},"output_index":0,"item":{"id":{"uuid":RESPONSE},"role":"assistant","x_grok":{"parent_response_id":{"uuid":USER}}}}),
        json!({"type":"response.output_item.done","response_id":{"uuid":RESPONSE},"output_index":0,"item":{"id":{"uuid":RESPONSE},"role":"assistant","status":"completed"}}),
        json!({"type":"response.done","response":{"id":{"uuid":RESPONSE},"status":"completed"}}),
    ]
}
async fn emit(page: &Cdp, values: &[Value]) -> Result<()> {
    page.evaluate_with_arguments(
        "(cfg)=>{for(const value of cfg.frames)window.PUBLIC_socket.emit(value);return true;}",
        &json!([{"frames":values}]),
        SHORT,
        &CancellationToken::new(),
    )
    .await?;
    Ok(())
}

/// Runs in the existing disposable, intercepted native privacy fixture. These
/// exercise production JS in Chrome, without an account or provider request.
pub(crate) async fn native_cases(
    page: &Cdp,
    endpoint: &crate::browser_chrome::AttachmentEndpoint,
) -> Result<()> {
    entry_cases(page).await?;
    dom_cases(page).await?;
    retirement_cases(page).await?;
    retirement_close_cases(page, endpoint).await?;
    let cancel = CancellationToken::new();
    for mode in ["Auto", "Fast"] {
        let mut reservation = setup(page, mode).await?;
        send(&mut reservation).await?;
        emit(page, &[user()]).await?;
        let committed = reservation
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?;
        require_receipt(&committed, reservation.owner())?;
        assert_eq!(committed["state"], "committed");
        reservation
            .acknowledge_grok(&committed, SHORT, &cancel)
            .await?;
        emit(page, &frames()[1..]).await?;
        let completed = reservation
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?;
        require_receipt(&completed, reservation.owner())?;
        assert_eq!(completed["state"], "completed");
        let stopped = reservation
            .grok_witness(DISPATCH, true, SHORT, &cancel)
            .await?;
        assert_eq!(stopped, completed);
        reservation.release_clean(SHORT).await?;
    }
    for case in [
        "wrong-client",
        "wrong-prompt",
        "wrong-parent",
        "wrong-response",
        "wrong-assistant",
        "early-complete",
        "incomplete",
        "duplicate-user",
        "duplicate-send",
        "oversized",
        "opaque",
        "private-lost",
        "mode-changed",
        "owner-changed",
        "socket-close",
        "chunk-response",
        "part-item",
        "text-index",
        "content-after-done",
    ] {
        let mut reservation = setup(page, "Auto").await?;
        send(&mut reservation).await?;
        let mut values = frames();
        match case {
            "wrong-client" => {
                values[0]["item"]["x_grok"]["client_message_id"]["uuid"] = json!(DISPATCH)
            }
            "wrong-prompt" => {
                values[0]["item"]["x_grok"]["input_chunks"][0]["text"]["text"] =
                    json!("PUBLIC other")
            }
            "wrong-parent" => {
                values[2]["item"]["x_grok"]["parent_response_id"]["uuid"] = json!(DISPATCH)
            }
            "wrong-response" => values[4]["response"]["id"]["uuid"] = json!(DISPATCH),
            "wrong-assistant" => values[3]["item"]["id"]["uuid"] = json!(DISPATCH),
            "early-complete" => values = vec![values[4].clone()],
            "incomplete" => values[3]["item"]["status"] = json!("in_progress"),
            "duplicate-user" => values.insert(1, user()),
            "oversized" => values = vec![json!("x".repeat(1048577))],
            "opaque" => {
                page.evaluate("window.PUBLIC_socket.dispatchEvent(new MessageEvent('message',{data:new Blob(['PUBLIC'])}));true",SHORT,&cancel).await?;
            }
            "duplicate-send" => {
                assert!(
                    page.evaluate_with_arguments(
                        "(cfg)=>{PUBLIC_socket.send(PUBLIC_sent[0]);return true;}",
                        &json!([{}]),
                        SHORT,
                        &cancel
                    )
                    .await
                    .is_err()
                );
            }
            "private-lost" => {
                page.evaluate("document.querySelector('.query-bar').classList.remove('bg-surface-incognito');document.querySelector('a').remove();true",SHORT,&cancel).await?;
            }
            "mode-changed" => {
                page.evaluate(
                    "document.querySelector('button').innerText='Expert';true",
                    SHORT,
                    &cancel,
                )
                .await?;
            }
            "owner-changed" => {
                page.evaluate(
                    "sessionStorage.removeItem('oracle:target-claim');true",
                    SHORT,
                    &cancel,
                )
                .await?;
            }
            "socket-close" => {
                page.evaluate(
                    "PUBLIC_socket.dispatchEvent(new Event('close'));true",
                    SHORT,
                    &cancel,
                )
                .await?;
            }
            "chunk-response"=>values.insert(3,json!({"type":"response.chunk","response_id":{"uuid":DISPATCH},"item_id":{"uuid":RESPONSE}})),
            "part-item"=>values.insert(3,json!({"type":"response.content_part.added","response_id":{"uuid":RESPONSE},"item_id":{"uuid":DISPATCH},"output_index":0,"content_index":0})),
            "text-index"=>values.insert(3,json!({"type":"response.output_text.done","response_id":{"uuid":RESPONSE},"item_id":{"uuid":RESPONSE},"output_index":1,"content_index":0})),
            "content-after-done"=>values.push(json!({"type":"response.chunk","response_id":{"uuid":RESPONSE},"item_id":{"uuid":RESPONSE}})),
            _ => unreachable!(),
        }
        emit(page, &values).await?;
        let observed = reservation
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?;
        assert_eq!(observed["state"], "invalid", "{case}: {observed}");
        assert!(
            require_receipt(&observed, reservation.owner()).is_err(),
            "{case}"
        );
        assert_eq!(
            reservation
                .grok_witness(DISPATCH, true, SHORT, &cancel)
                .await?["state"],
            "invalid"
        );
    }
    // A different socket and forged caller receipt cannot supply the mapping.
    let mut reservation = setup(page, "Auto").await?;
    send(&mut reservation).await?;
    page.evaluate_with_arguments("(cfg)=>{const other=new WebSocket();for(const value of cfg.frames)other.emit(value);return true;}",&json!([{"frames":frames()}]),SHORT,&cancel).await?;
    assert_eq!(
        reservation
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?["state"],
        "pending"
    );
    emit(page, &[user()]).await?;
    let mut forged = reservation
        .grok_witness(DISPATCH, false, SHORT, &cancel)
        .await?;
    forged["dispatchId"] = json!(CLIENT);
    assert!(
        reservation
            .acknowledge_grok(&forged, SHORT, &cancel)
            .await
            .is_err()
    );
    reservation
        .grok_witness(DISPATCH, true, SHORT, &cancel)
        .await?;
    for changed in ["mode", "owner"] {
        let mut reservation = setup(page, "Auto").await?;
        send(&mut reservation).await?;
        emit(page, &[user()]).await?;
        let receipt = reservation
            .grok_witness(DISPATCH, true, SHORT, &cancel)
            .await?;
        page.evaluate(
            if changed == "mode" {
                "document.querySelector('button').innerText='Fast';true"
            } else {
                "sessionStorage.removeItem('oracle:target-claim');true"
            },
            SHORT,
            &cancel,
        )
        .await?;
        assert!(
            reservation
                .acknowledge_grok(&receipt, SHORT, &cancel)
                .await
                .is_err(),
            "stopped {changed}"
        );
    }
    for invalidated in ["privacy", "unreadable"] {
        let mut reservation = setup(page, "Auto").await?;
        if invalidated == "privacy" {
            page.evaluate("document.querySelector('.query-bar').classList.remove('bg-surface-incognito');document.querySelector('a').setAttribute('aria-label','PUBLIC');true",SHORT,&cancel).await?;
            assert_eq!(
                reservation
                    .grok_witness(DISPATCH, false, SHORT, &cancel)
                    .await?["state"],
                "invalid"
            );
            page.evaluate("document.querySelector('.query-bar').classList.add('bg-surface-incognito');document.querySelector('a').setAttribute('aria-label','Switch to Default Chat');true",SHORT,&cancel).await?;
        } else {
            assert!(
                page.evaluate("PUBLIC_socket.send('{');true", SHORT, &cancel)
                    .await
                    .is_err()
            );
        }
        assert!(
            send(&mut reservation).await.is_err(),
            "sticky {invalidated}"
        );
        assert_eq!(
            page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
            0,
            "sticky {invalidated}"
        );
        reservation
            .grok_witness(DISPATCH, true, SHORT, &cancel)
            .await?;
    }
    // Explicit stop releases the input object even if a foreign wrapper retains
    // the delegating observer. No payload or authority survives in that closure.
    let mut reservation = setup(page, "Auto").await?;
    reservation
        .grok_witness(DISPATCH, true, SHORT, &cancel)
        .await?;
    page.evaluate("delete window.__oracleGrokWitness;true", SHORT, &cancel)
        .await?;
    reservation.evaluate_mutation(&format!("(cfg,privateChat)=>{{window.PUBLIC_cfgWeak=new WeakRef(cfg);return ({WITNESS})(cfg,privateChat);}}"),
        json!({"prompt":PROMPT,"promptSha256":sha256(PROMPT.as_bytes()),"mode":"Auto","dispatchId":DISPATCH,"lifetimeMs":60000}),SHORT,&cancel).await?;
    page.evaluate("{const observed=WebSocket.prototype.send;WebSocket.prototype.send=function(raw){return observed.call(this,raw);};}true",SHORT,&cancel).await?;
    reservation
        .grok_witness(DISPATCH, true, SHORT, &cancel)
        .await?;
    page.call("HeapProfiler.collectGarbage", json!({}), SHORT, &cancel)
        .await?;
    assert_eq!(
        page.evaluate("PUBLIC_cfgWeak.deref()===undefined", SHORT, &cancel)
            .await?,
        true
    );
    page.evaluate(
        "PUBLIC_socket.send('PUBLIC after stopped observer');true",
        SHORT,
        &cancel,
    )
    .await?;
    assert_eq!(
        page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
        1
    );
    // The expiry timer detaches and releases prompt data after caller loss too.
    page.evaluate("delete window.__oracleGrokWitness;true", SHORT, &cancel)
        .await?;
    reservation.evaluate_mutation(WITNESS,json!({"prompt":PROMPT,"promptSha256":sha256(PROMPT.as_bytes()),"mode":"Auto","dispatchId":DISPATCH,"lifetimeMs":1}),SHORT,&cancel).await?;
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(
        reservation
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?["state"],
        "invalid"
    );
    assert!(send(&mut reservation).await.is_err());
    assert_eq!(
        page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
        1,
        "Expired witness must refuse the click and leave only the earlier post-stop fixture send"
    );
    // An asynchronous click handler must not escape observation by writing only
    // after expiry. The remaining guard retains no prompt or document data.
    let mut delayed = setup_with_lifetime(page, "Auto", Duration::from_millis(1000)).await?;
    page.evaluate("{const clicked=document.querySelector('button[aria-label=\"Send message\"]').onclick;window.PUBLIC_clicked=0;window.PUBLIC_lateBlocked=false;document.querySelector('button[aria-label=\"Send message\"]').onclick=()=>{PUBLIC_clicked++;setTimeout(()=>{try{clicked();}catch{PUBLIC_lateBlocked=true;}},1500);};}true",SHORT,&cancel).await?;
    send(&mut delayed).await?;
    assert_eq!(page.evaluate("PUBLIC_clicked", SHORT, &cancel).await?, 1);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(
        page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
        0
    );
    assert_eq!(
        page.evaluate("PUBLIC_lateBlocked", SHORT, &cancel).await?,
        true
    );
    assert_eq!(
        delayed
            .grok_witness(DISPATCH, false, SHORT, &cancel)
            .await?["state"],
        "invalid"
    );
    // Rejection consumes the arm but never removes the outstanding click's
    // no-replay barrier. A later provider retry stays blocked after cleanup.
    let mut rejected = setup(page, "Auto").await?;
    page.evaluate("document.querySelector('button[aria-label=\"Send message\"]').onclick=()=>{try{PUBLIC_socket.send(JSON.stringify({type:'response.create',item:{role:'user',x_grok:{client_message_id:{uuid:'00000000-0000-4000-8000-000000000001'},input_chunks:[{text:{text:'PUBLIC wrong prompt'}}]}}}));}catch{}};true",SHORT,&cancel).await?;
    send(&mut rejected).await?;
    assert_eq!(
        rejected
            .grok_witness(DISPATCH, true, SHORT, &cancel)
            .await?["state"],
        "invalid"
    );
    assert!(page.evaluate_with_arguments("(cfg)=>{PUBLIC_socket.send(JSON.stringify({type:'response.create',item:{role:'user',x_grok:{client_message_id:{uuid:cfg.client},input_chunks:[{text:{text:cfg.prompt}}]}}}));return true;}",&json!([{"client":CLIENT,"prompt":PROMPT}]),SHORT,&cancel).await.is_err());
    assert_eq!(
        page.evaluate("PUBLIC_sent.length", SHORT, &cancel).await?,
        0
    );
    // Quoted controls in an answer never prove Private mode.
    page.evaluate("document.body.innerHTML='<textarea></textarea><div id=\"response-PUBLIC\"><a aria-label=\"Switch to Default Chat\">Private</a><div class=\"query-bar bg-surface-incognito\"></div></div>';true",SHORT,&cancel).await?;
    assert!(require(page, SHORT, &cancel).await.is_err());
    Ok(())
}

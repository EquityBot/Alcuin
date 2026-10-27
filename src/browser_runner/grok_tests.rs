use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::sync::atomic::{AtomicUsize, Ordering};

const PROMPT: &str = "PUBLIC Grok owned runner request";
const ANSWER: &str = "PUBLIC original Grok runner answer";
const LIST_PROMPT: &str = "PUBLIC brief.\n\nEVIDENCE:\n- First item\n- Second item\n\nQUESTION:\n1. Is it sound?\n2. What next?";
const INLINE_PROMPT: &str =
    "PUBLIC brief.\n\n- First **item**\n- Second [link](https://example.test/a)";
// Grok's renderings of the submitted user turn for each Markdown case.
fn markdown_turn(case: &str) -> (&'static str, &'static str) {
    const LIST: &str = "<p>PUBLIC brief.</p><p>EVIDENCE:</p><ul><li>First item</li><li>Second item</li></ul><p>QUESTION:</p><ol><li>Is it sound?</li><li>What next?</li></ol>";
    match case {
        "listed" => (LIST_PROMPT, LIST),
        // Only the last word differs from `listed`.
        "differs" => (
            LIST_PROMPT,
            "<p>PUBLIC brief.</p><p>EVIDENCE:</p><ul><li>First item</li><li>Second item</li></ul><p>QUESTION:</p><ol><li>Is it sound?</li><li>What else?</li></ol>",
        ),
        // The same lines without list items.
        "unlisted" => (
            LIST_PROMPT,
            "<p>PUBLIC brief.</p><p>EVIDENCE:</p><p>First item</p><p>Second item</p><p>QUESTION:</p><p>Is it sound?</p><p>What next?</p>",
        ),
        // INLINE_PROMPT shown as its exact text passes.
        "inline-exact" => (INLINE_PROMPT, ""),
        "inline" => (
            INLINE_PROMPT,
            "<p>PUBLIC brief.</p><ul><li>First <strong>item</strong></li><li>Second <a href='https://example.test/a'>link</a></li></ul>",
        ),
        _ => (PROMPT, ""),
    }
}

// `stagger` sends only the user frame at Send, then `response.created` just after the
// worker's first committed witness read and the rest after the renderer acknowledgement,
// so the acknowledgement sees a stream that advanced past the worker's read.
// A non-empty `user_html` replaces the user turn's text with rendered HTML.
fn document(auto: bool, private_loss: &str, stagger: bool, user_html: &str) -> String {
    let config = json!({"auto":auto,"privateLoss":private_loss,"stagger":stagger,"answer":ANSWER,"userHtml":user_html});
    format!(
        r#"<!doctype html><meta http-equiv='Content-Security-Policy' content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'none'; form-action 'none'"><style>button,textarea,a{{min-width:40px;min-height:24px;display:inline-block}}</style>
<div class='query-bar'><textarea></textarea><a aria-label='Switch to Private Chat'>Private</a><button aria-haspopup='menu'>Expert</button><button aria-label='Send message'>Send</button></div>
<script>
const cfg={config};
window.addEventListener('error',event=>{{window.PUBLIC_fixtureError=event.message;}});
window.PUBLIC_witnessTimers=new Set();const nativeSetTimeout=window.setTimeout.bind(window),nativeClearTimeout=window.clearTimeout.bind(window);
window.setTimeout=(fn,ms,...args)=>{{const id=nativeSetTimeout(fn,ms,...args);if(ms>2000)PUBLIC_witnessTimers.add(id);return id;}};
window.clearTimeout=id=>{{PUBLIC_witnessTimers.delete(id);return nativeClearTimeout(id);}};
const query=document.querySelector('.query-bar'),editor=query.querySelector('textarea'),trigger=query.querySelector('button[aria-haspopup]');
query.querySelector('a').onclick=()=>{{query.classList.add('bg-surface-incognito');query.querySelector('a').setAttribute('aria-label','Switch to Default Chat');}};
trigger.onclick=()=>{{trigger.setAttribute('aria-expanded','true');const menu=document.createElement('div');menu.setAttribute('role','menu');for(const name of ['Auto','Fast']){{const choice=document.createElement('button');choice.setAttribute('role','menuitemradio');choice.textContent=name;if(name==='Auto'&&!cfg.auto)choice.disabled=true;choice.onclick=()=>{{trigger.textContent=name;trigger.setAttribute('aria-expanded','false');menu.remove();}};menu.append(choice);}}document.body.append(menu);}};
if(cfg.privateLoss==='focus')editor.addEventListener('focus',()=>{{query.classList.remove('bg-surface-incognito');query.querySelector('a').setAttribute('aria-label','Switch to Private Chat');}});
class PublicSocket extends EventTarget{{constructor(){{super();this.url='wss://grok.com/PUBLIC-owned-runner';}}emit(value){{this.dispatchEvent(new MessageEvent('message',{{data:JSON.stringify(value)}}));}}send(raw){{
PUBLICGrokRunnerSend('send');const sent=JSON.parse(raw),client=sent.item.x_grok.client_message_id,prompt=sent.item.x_grok.input_chunks[0].text.text,user='00000000-0000-4000-8000-000000000002',response='00000000-0000-4000-8000-000000000003';
const frames=[
{{type:'conversation.item.added',item:{{id:{{uuid:user}},type:'message',role:'user',x_grok:{{client_message_id:client,input_chunks:[{{text:{{text:prompt}}}}]}}}}}},
{{type:'response.created',response:{{id:{{uuid:response}},status:'in_progress'}}}},
{{type:'response.output_item.added',response_id:{{uuid:response}},output_index:0,item:{{id:{{uuid:response}},role:'assistant',x_grok:{{parent_response_id:{{uuid:user}}}}}}}},
{{type:'response.output_item.done',response_id:{{uuid:response}},output_index:0,item:{{id:{{uuid:response}},role:'assistant',status:'completed'}}}},
{{type:'response.done',response:{{id:{{uuid:response}},status:'completed'}}}}
];
const finish=()=>{{for(const frame of frames.slice(2))this.emit(frame);
for(const [role,id,text] of [['user',user,prompt],['assistant',response,cfg.answer]]){{const article=document.createElement('article');article.id='response-'+id;article.dataset.role=role;const content=document.createElement('div');content.dataset.testid='message-content';if(role==='user'&&cfg.userHtml)content.innerHTML=cfg.userHtml;else{{content.style.whiteSpace='pre-wrap';content.textContent=text;}}article.append(content);if(role==='assistant')for(const label of ['Copy','Regenerate']){{const button=document.createElement('button');button.setAttribute('aria-label',label);button.textContent=label;article.append(button);}}document.body.append(article);}}
if(cfg.privateLoss==='after-send'){{query.classList.remove('bg-surface-incognito');query.querySelector('a').setAttribute('aria-label','Switch to Private Chat');}}
}};
this.emit(frames[0]);
if(!cfg.stagger){{this.emit(frames[1]);finish();return;}}
const original=window.__oracleGrokWitness;let staged=false,acknowledged=false;
Object.defineProperty(window,'__oracleGrokWitness',{{value:Object.freeze({{...original,
read:()=>{{const value=original.read();if(!staged&&value.state==='committed'){{staged=true;setTimeout(()=>this.emit(frames[1]),0);}}return value;}},
acknowledge:()=>{{const value=original.acknowledge();if(!acknowledged){{acknowledged=true;PUBLICGrokAck(value.state+':'+Boolean(value.responseId));setTimeout(finish,0);}}return value;}}}}),writable:false,configurable:true}});
}}}}
window.WebSocket=PublicSocket;
query.querySelector('button[aria-label="Send message"]').onclick=()=>new WebSocket().send(JSON.stringify({{type:'response.create',item:{{type:'message',role:'user',x_grok:{{client_message_id:{{uuid:'00000000-0000-4000-8000-000000000001'}},input_chunks:[{{text:{{text:editor.value}}}}]}}}}}}));
</script>"#
    )
}

pub(crate) async fn native_cases(browser: &Cdp, endpoint: &AttachmentEndpoint) -> Result<()> {
    let limits = RunnerLimits {
        command: Duration::from_secs(5),
        admission: Duration::from_secs(5),
        commit: Duration::from_secs(3),
        answer: Duration::from_secs(3),
        poll: Duration::from_millis(20),
        ..RunnerLimits::default()
    };
    let cancel = CancellationToken::new();
    let baseline = browser
        .call("Target.getTargets", json!({}), limits.command, &cancel)
        .await?;
    for (auto, private_loss, stagger, markdown) in [
        (true, "", false, ""),
        (false, "", false, ""),
        (true, "", true, ""),
        (true, "", false, "listed"),
        (true, "", false, "differs"),
        (true, "", false, "unlisted"),
        (true, "", false, "inline"),
        (true, "", false, "inline-exact"),
        (true, "focus", false, ""),
        (true, "after-send", false, ""),
    ] {
        let (prompt, user_html) = markdown_turn(markdown);
        let refused = matches!(markdown, "differs" | "unlisted" | "inline");
        let home = tempfile::tempdir()?;
        let store = SessionStore::new(home.path().join(".oracle"));
        let output = home.path().join("PUBLIC-answer.md");
        let environment = crate::config::Environment::from([
            (
                "ORACLE_HOME_DIR".into(),
                store.home().to_string_lossy().into(),
            ),
            (
                (if cfg!(windows) { "USERPROFILE" } else { "HOME" }).into(),
                home.path().to_string_lossy().into(),
            ),
        ]);
        let resolved = crate::execution::resolve(
            &json!({"prompt":prompt,"engine":"browser","model":"gpt-6-pro","writeOutputPath":output}),
            &json!({"browser":{"manualLogin":true,"cookieSync":false}}),
            &environment,
            home.path(),
        )?;
        let parent = prepare(&store, &resolved, home.path())?;
        let parent_id = parent["id"].as_str().context("No public fixture parent")?;
        let original_options = parent["options"].clone();
        let identity = ProcessIdentity::current()?;
        start_owner(&store, parent_id, &identity)?;
        let mut coordinator =
            crate::browser_attempt::Parent::begin(&store, parent_id, &identity, &cancel)?;
        let link = coordinator.reserve_grok_fixture(&environment, &cancel)?;
        start_owner(&store, &link.child_id, &identity)?;
        let child = store.require(&link.child_id)?;
        let profile = ProfileRegistry::open(Path::new(
            child["browser"]["config"]["manualLoginProfileDir"]
                .as_str()
                .unwrap(),
        ))?;
        let lease = Arc::new(
            profile
                .acquire_with_limit(&link.child_id, 1, limits.admission, &cancel)
                .await?,
        );
        let heartbeat = lease.heartbeat();
        let mut target = TargetAcquisition::default()
            .acquire(browser.clone(), lease, &cancel, limits.command)
            .await?;
        let target_id = target.id().to_owned();
        let page = target.page().clone();
        let session = page
            .session_id()
            .context("No owned fixture session")?
            .to_owned();
        let sends = Arc::new(AtomicUsize::new(0));
        let sent = sends.clone();
        let acks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let acked = acks.clone();
        let documents = Arc::new(AtomicUsize::new(0));
        let seen = documents.clone();
        let mut events = browser.events();
        page.call("Runtime.enable", json!({}), limits.command, &cancel)
            .await?;
        for name in ["PUBLICGrokRunnerSend", "PUBLICGrokAck"] {
            page.call(
                "Runtime.addBinding",
                json!({ "name": name }),
                limits.command,
                &cancel,
            )
            .await?;
        }
        page.call(
            "Fetch.enable",
            json!({"patterns":[{"urlPattern":"*"}]}),
            limits.command,
            &cancel,
        )
        .await?;
        let intercepted = page.clone();
        let html = document(auto, private_loss, stagger, user_html);
        let interceptor = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if event["sessionId"] != session {
                    continue;
                }
                if event["method"] == "Runtime.bindingCalled"
                    && event["params"]["name"] == "PUBLICGrokRunnerSend"
                {
                    sent.fetch_add(1, Ordering::SeqCst);
                }
                if event["method"] == "Runtime.bindingCalled"
                    && event["params"]["name"] == "PUBLICGrokAck"
                {
                    acked
                        .lock()
                        .unwrap()
                        .push(event["params"]["payload"].clone());
                }
                if event["method"] != "Fetch.requestPaused" {
                    continue;
                }
                anyhow::ensure!(
                    event["params"]["request"]["url"] == "https://grok.com/",
                    "Unexpected fixture network request"
                );
                seen.fetch_add(1, Ordering::SeqCst);
                intercepted.call("Fetch.fulfillRequest",json!({"requestId":event["params"]["requestId"],"responseCode":200,
                    "responseHeaders":[{"name":"Content-Type","value":"text/html"}],"body":STANDARD.encode(&html)}),Duration::from_secs(5),&CancellationToken::new()).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let checked=async {
            let bringup=DirectoryLock::acquire_async(&profile.path().join("oracle-browser-bringup.lock"),limits.command,&cancel).await?;
            let witness=ExecutionWitness::capture(&store,&link.child_id,&identity)?;
            let committed=request(&store,&link.child_id)?;
            let delivered=run_owned(&store,&link.child_id,&witness,&committed,&mut target,endpoint,bringup,&limits,&cancel,&CaptureBoundary::ParentAttempt,None,None).await;
            if delivered.is_err() {
                let reason=page.evaluate("window.__oracleGrokWitness?.read()?.reason??'no installed witness'",limits.command,&cancel).await;
                let fixture_error=page.evaluate("window.PUBLIC_fixtureError??null",limits.command,&cancel).await;
                eprintln!("PUBLIC Grok runner {private_loss:?} failure witness reason: {reason:?}; fixture error: {fixture_error:?}");
            }
            if !private_loss.is_empty() {
                let expected=usize::from(private_loss=="after-send");
                anyhow::ensure!(delivered.is_err() && sends.load(Ordering::SeqCst)==expected,"Changed Private mode repeated or unexpectedly issued physical Send");
                anyhow::ensure!(store.require(&link.child_id)?["capturedAnswer"].is_null(),"Refused mode produced capture");
                anyhow::ensure!(page.evaluate("PUBLIC_witnessTimers.size",limits.command,&cancel).await?==0,"Failed provider retained its witness expiry timer");
                if private_loss=="after-send" {
                    anyhow::ensure!(page.evaluate("Boolean(window.__oracleGrokWitness)",limits.command,&cancel).await?==true,"Post-Send case did not install the original witness");
                }
                return Ok::<_,anyhow::Error>(());
            }
            if refused {
                // CDP redacts the page's refusal but not that the page threw. The "listed" and
                // "inline-exact" cases pass on the same fixtures except for the user turn, and the
                // refused run had acknowledged its commitment and received the whole answer, so
                // the refusal is the user-turn comparison, not a stall.
                let dispatch=store.require(&link.child_id)?["dispatch"]["state"].clone();
                let answered=page.evaluate("Boolean(document.getElementById('response-00000000-0000-4000-8000-000000000003'))",limits.command,&cancel).await?;
                let error=delivered.as_ref().err().map(|error| format!("{error:#}")).unwrap_or_default();
                anyhow::ensure!(error.contains("Browser expression failed") && !error.contains("deadline")
                    && sends.load(Ordering::SeqCst)==1 && dispatch=="acknowledged" && answered==true,
                    "{markdown} Markdown user turn was not refused after its one answered Send: {:?} {} {dispatch} {answered}",
                    delivered.as_ref().err(),sends.load(Ordering::SeqCst));
                anyhow::ensure!(store.require(&link.child_id)?["capturedAnswer"].is_null(),"{markdown} Markdown user turn produced capture");
                anyhow::ensure!(page.evaluate("PUBLIC_witnessTimers.size",limits.command,&cancel).await?==0,"Failed provider retained its witness expiry timer");
                eprintln!("PUBLIC native Grok runner refused the {markdown} Markdown user turn after one Send");
                return Ok(());
            }
            let child_capture=delivered?;
            let expected=(markdown=="listed").then(|| json!("rendered-list"));
            anyhow::ensure!(child_capture["browser"]["runtime"]["acceptedObservation"].get("promptMatch")==expected.as_ref(),
                "Grok user turn match is not {expected:?}");
            if stagger {
                // The worker saved the bare committed read; the renderer then acknowledged it
                // against a witness that already had its response identity.
                let receipt=&child_capture["browser"]["runtime"]["grokCommitReceipt"];
                anyhow::ensure!(receipt["state"]=="committed" && receipt["responseId"].is_null()
                    && *acks.lock().unwrap()==[json!("committed:true")],
                    "Staggered case did not acknowledge an advanced witness: {receipt} {:?}",acks.lock().unwrap());
            }
            anyhow::ensure!(child_capture["status"]=="running" && child_capture["dispatch"]["state"]=="acknowledged"
                && child_capture["browser"]["runtime"]["targetDispositionPending"]==true,"Provider completed before parent acknowledgement");
            let selected=if auto {"Auto"}else{"Fast"};
            anyhow::ensure!(child_capture["browser"]["runtime"]["modelSelection"]["selectedMode"]==selected
                && child_capture["browser"]["runtime"].get("conversationId").is_none(),"Provider invented GPT identity");
            anyhow::ensure!(browser.call("Target.getTargetInfo",json!({"targetId":target_id}),limits.command,&cancel).await?["targetInfo"]["targetId"]==target_id,
                "Provider target closed before adoption");
            coordinator.adopt(&link.child_id,&cancel)?;
            coordinator.complete(&cancel)?;
            crate::browser_attempt::settle_selected_saved(&store,parent_id,&cancel).await?;
            let parent=store.require(parent_id)?;
            anyhow::ensure!(parent["status"]=="completed" && parent["recoveryRequired"]==false && parent["options"]==original_options,
                "Public parent completion or original options changed");
            anyhow::ensure!(String::from_utf8(captured(&store,parent_id)?)?==ANSWER && fs::read_to_string(&output)?.contains(ANSWER),
                "Public capture/output lost original provider answer");
            anyhow::ensure!(sends.load(Ordering::SeqCst)==1 && documents.load(Ordering::SeqCst)==1,"Owned provider repeated navigation or Send");
            anyhow::ensure!(target_ids(&browser.call("Target.getTargets",json!({}),limits.command,&cancel).await?)==target_ids(&baseline),
                "Selected Grok cleanup retained its target or closed another fixture target");
            eprintln!("PUBLIC native Grok runner {selected}{}{}: one Send, parent output durable, exact target absent",
                if stagger {" staggered"} else {""}, if markdown.is_empty() {String::new()} else {format!(" {markdown}")});
            Ok(())
        }.await;
        interceptor.abort();
        let _ = interceptor.await;
        drop(heartbeat);
        // This fixture allocated the target. Its error teardown never runs in production.
        if !private_loss.is_empty() || refused || checked.is_err() {
            let _ = browser
                .close_owned_target(&target_id, limits.command, &CancellationToken::new())
                .await;
        }
        checked?;
    }
    for (hold, damage) in [
        ("user", ""),
        ("answer", ""),
        ("delivered", ""),
        ("captured", ""),
        ("receipt", ""),
        ("answer", "witness"),
        ("answer", "private"),
    ] {
        recovery_case(browser, endpoint, hold, damage)
            .await
            .with_context(|| format!("PUBLIC Grok recovery case {hold:?} {damage:?}"))?;
    }
    Ok(())
}

// Frames before `hold` reach the original witness at Send; the rest, and the DOM
// turn, arrive only when the recovering process calls PUBLIC_release().
fn recovery_document(hold: &str) -> String {
    r#"<!doctype html><meta http-equiv='Content-Security-Policy' content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'none'; form-action 'none'"><style>button,textarea,a{min-width:40px;min-height:24px;display:inline-block}</style>
<div class='query-bar'><textarea></textarea><a aria-label='Switch to Private Chat'>Private</a><button aria-haspopup='menu'>Expert</button><button aria-label='Send message'>Send</button></div>
<script>
const cfg=__CONFIG__;
window.PUBLIC_sendCount=0;
const query=document.querySelector('.query-bar'),editor=query.querySelector('textarea'),trigger=query.querySelector('button[aria-haspopup]');
query.querySelector('a').onclick=()=>{query.classList.add('bg-surface-incognito');query.querySelector('a').setAttribute('aria-label','Switch to Default Chat');};
trigger.onclick=()=>{trigger.setAttribute('aria-expanded','true');const menu=document.createElement('div');menu.setAttribute('role','menu');for(const name of ['Auto','Fast']){const choice=document.createElement('button');choice.setAttribute('role','menuitemradio');choice.textContent=name;choice.onclick=()=>{trigger.textContent=name;trigger.setAttribute('aria-expanded','false');menu.remove();};menu.append(choice);}document.body.append(menu);};
class PublicSocket extends EventTarget{constructor(){super();this.url='wss://grok.com/PUBLIC-recovery';}emit(value){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(value)}));}send(raw){
PUBLIC_sendCount++;const sent=JSON.parse(raw),client=sent.item.x_grok.client_message_id,prompt=sent.item.x_grok.input_chunks[0].text.text,user='00000000-0000-4000-8000-000000000012',response='00000000-0000-4000-8000-000000000013';
const frames=[
{type:'conversation.item.added',item:{id:{uuid:user},type:'message',role:'user',x_grok:{client_message_id:client,input_chunks:[{text:{text:prompt}}]}}},
{type:'response.created',response:{id:{uuid:response},status:'in_progress'}},
{type:'response.output_item.added',response_id:{uuid:response},output_index:0,item:{id:{uuid:response},role:'assistant',x_grok:{parent_response_id:{uuid:user}}}},
{type:'response.output_item.done',response_id:{uuid:response},output_index:0,item:{id:{uuid:response},role:'assistant',status:'completed'}},
{type:'response.done',response:{id:{uuid:response},status:'completed'}}
];
const now=cfg.hold==='user'?0:2;
for(const frame of frames.slice(0,now))this.emit(frame);
window.PUBLIC_release=()=>{for(const frame of frames.slice(now))this.emit(frame);
for(const [role,id,text] of [['user',user,prompt],['assistant',response,cfg.answer]]){const article=document.createElement('article');article.id='response-'+id;article.dataset.role=role;const content=document.createElement('div');content.dataset.testid='message-content';content.textContent=text;article.append(content);if(role==='assistant')for(const label of ['Copy','Regenerate']){const button=document.createElement('button');button.setAttribute('aria-label',label);button.textContent=label;article.append(button);}document.body.append(article);}
delete window.PUBLIC_release;return true;};
if(['delivered','captured'].includes(cfg.hold))PUBLIC_release();
}}
window.WebSocket=PublicSocket;
query.querySelector('button[aria-label="Send message"]').onclick=()=>new WebSocket().send(JSON.stringify({type:'response.create',item:{type:'message',role:'user',x_grok:{client_message_id:{uuid:'00000000-0000-4000-8000-000000000011'},input_chunks:[{text:{text:editor.value}}]}}}));
</script>"#
        .replace("__CONFIG__", &json!({"hold":hold,"answer":ANSWER}).to_string())
}

// Pages only: Chrome starts and stops its own service workers and UI targets meanwhile.
fn target_ids(rows: &Value) -> std::collections::BTreeSet<String> {
    rows["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["type"] == "page")
        .filter_map(|row| row["targetId"].as_str().map(str::to_owned))
        .collect()
}

/// A genuine worker crash after the single Send, then public parent recovery from this
/// process against the same live Chrome target, renderer claim and witness.
async fn recovery_case(
    browser: &Cdp,
    endpoint: &AttachmentEndpoint,
    hold: &str,
    damage: &str,
) -> Result<()> {
    let limits = RunnerLimits {
        command: Duration::from_secs(5),
        admission: Duration::from_secs(5),
        commit: Duration::from_secs(3),
        answer: Duration::from_secs(10),
        poll: Duration::from_millis(20),
        ..RunnerLimits::default()
    };
    let cancel = CancellationToken::new();
    let baseline = target_ids(
        &browser
            .call("Target.getTargets", json!({}), limits.command, &cancel)
            .await?,
    );
    let root = tempfile::tempdir()?;
    // Windows resolves the worker's startup lock folder from its synthetic USERPROFILE.
    fs::create_dir_all(root.path().join("AppData").join("Local"))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "--ignored",
            "--exact",
            "browser_runner::grok::tests::grok_recovery_worker_helper",
            "--nocapture",
        ])
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("INDUSTRIOUS_GROK_RECOVERY", root.path())
        .env("INDUSTRIOUS_GROK_RECOVERY_HOLD", hold)
        .env("INDUSTRIOUS_GROK_RECOVERY_HOST", &endpoint.host)
        .env("INDUSTRIOUS_GROK_RECOVERY_PORT", endpoint.port.to_string())
        .env(
            "INDUSTRIOUS_GROK_RECOVERY_WS",
            &endpoint.browser_ws_endpoint,
        );
    for key in ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || command.output()),
    )
    .await
    .context("PUBLIC Grok worker did not crash before its deadline")???;
    anyhow::ensure!(
        output.status.success(),
        "PUBLIC Grok worker failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value = serde_json::from_slice(&fs::read(root.path().join("ready.json"))?)?;
    let identity: ProcessIdentity = serde_json::from_value(manifest["identity"].clone())?;
    anyhow::ensure!(
        identity.liveness() == crate::process::Liveness::Dead,
        "PUBLIC Grok worker is not positively dead"
    );
    let store = SessionStore::new(
        Path::new(manifest["home"].as_str().context("No worker home")?).join(".oracle"),
    );
    let parent_id = manifest["parentId"].as_str().context("No parent")?;
    let child_id = manifest["childId"].as_str().context("No child")?;
    let target_id = manifest["targetId"].as_str().context("No target")?;
    let attached = browser
        .call(
            "Target.attachToTarget",
            json!({"targetId":target_id,"flatten":true}),
            limits.command,
            &cancel,
        )
        .await?;
    let page = browser.with_session(
        attached["sessionId"]
            .as_str()
            .context("No recovery observer session")?
            .into(),
    );
    let checked = async {
        anyhow::ensure!(
            page.evaluate("PUBLIC_sendCount", limits.command, &cancel)
                .await?
                == 1,
            "PUBLIC worker did not issue exactly one Send"
        );
        let crashed = store.stored_for_update(child_id)?;
        // These holds stream the whole answer at Send, so nothing is left to release.
        let streamed = matches!(hold, "delivered" | "captured");
        let expected = if matches!(hold, "user" | "receipt") {
            "started"
        } else {
            "acknowledged"
        };
        anyhow::ensure!(
            crashed["browser"]["runtime"]["grokCommitReceipt"].is_null() == (hold == "user")
                && crashed["dispatch"]["state"] == expected
                && crashed["capturedAnswer"].is_null() == !streamed
                && store.stored_for_update(parent_id)?["browser"]["consultation"]["switch"]
                    ["outcome"]
                    .is_null(),
            "PUBLIC worker crashed at an unexpected point"
        );
        match damage {
            "witness" => {
                page.evaluate("delete window.__oracleGrokWitness", limits.command, &cancel)
                    .await?;
            }
            "private" => {
                page.evaluate("document.querySelector('.query-bar').classList.remove('bg-surface-incognito'),document.querySelector('.query-bar a').setAttribute('aria-label','Switch to Private Chat'),true",limits.command,&cancel).await?;
            }
            _ => {}
        }
        // Record every renderer claim write during recovery, to prove the witness was sealed
        // (submission clean or released) before retirement began.
        let initial = page
            .evaluate(
                "sessionStorage.getItem('oracle:target-claim')",
                limits.command,
                &cancel,
            )
            .await?;
        page.call("Runtime.enable", json!({}), limits.command, &cancel)
            .await?;
        page.call(
            "Runtime.addBinding",
            json!({"name":"PUBLIC_claim"}),
            limits.command,
            &cancel,
        )
        .await?;
        page.evaluate("(()=>{const set=Storage.prototype.setItem;Storage.prototype.setItem=function(key,value){if(this===sessionStorage&&key==='oracle:target-claim')PUBLIC_claim(String(value));return set.call(this,key,value);};return true;})()",limits.command,&cancel).await?;
        let session = page
            .session_id()
            .context("No recovery observer session")?
            .to_owned();
        let mut events = browser.events();
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let written = writes.clone();
        let collector = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if event["sessionId"] == session
                    && event["method"] == "Runtime.bindingCalled"
                    && event["params"]["name"] == "PUBLIC_claim"
                {
                    written
                        .lock()
                        .unwrap()
                        .push(event["params"]["payload"].clone());
                }
            }
        });
        if hold == "receipt" {
            // The stream completes before recovery, past the receipt the worker saved.
            anyhow::ensure!(
                page.evaluate("PUBLIC_release()", limits.command, &cancel)
                    .await?
                    == true,
                "PUBLIC answer was not released before recovery"
            );
        }
        let releaser = page.clone();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            releaser
                .evaluate(
                    "window.PUBLIC_release?.()??false",
                    Duration::from_secs(5),
                    &CancellationToken::new(),
                )
                .await
        });
        let profile = ProfileRegistry::open_existing(Path::new(
            manifest["profileDir"].as_str().context("No profile")?,
        ))?;
        let recovered = recover_attached(
            &store,
            parent_id,
            profile,
            browser.clone(),
            endpoint.clone(),
            &limits,
            &cancel,
        )
        .await;
        let released = release.await?;
        // Bindings arrive asynchronously; retirement has finished by now.
        tokio::time::sleep(Duration::from_millis(200)).await;
        collector.abort();
        let writes = writes.lock().unwrap().clone();
        let parent = store.require(parent_id)?;
        let child = store.require(child_id)?;
        let targets = target_ids(
            &browser
                .call("Target.getTargets", json!({}), limits.command, &cancel)
                .await?,
        );
        if !damage.is_empty() {
            let after = store.stored_for_update(child_id)?;
            // The Private guard's throw reaches Rust redacted by the CDP client.
            let cause = if damage == "witness" {
                "live witness or dispatch changed"
            } else {
                "Browser expression failed"
            };
            anyhow::ensure!(
                recovered
                    .as_ref()
                    .is_err_and(|error| format!("{error:#}").contains(cause))
                    && parent["status"] != "completed"
                    && child["capturedAnswer"].is_null()
                    && after["dispatch"] == crashed["dispatch"]
                    && after["browser"]["runtime"]["grokCommitReceipt"]
                        == crashed["browser"]["runtime"]["grokCommitReceipt"]
                    && targets.contains(target_id)
                    && page.evaluate("PUBLIC_sendCount", limits.command, &cancel).await? == 1,
                "Damaged Grok evidence was recovered, replayed, changed or closed: {recovered:?}"
            );
            eprintln!(
                "PUBLIC Grok recovery {damage} refused: {:#}",
                recovered.unwrap_err()
            );
            return Ok(());
        }
        let recovered = recovered?;
        anyhow::ensure!(
            released? == !(streamed || hold == "receipt"),
            "PUBLIC answer release did not match its crash point"
        );
        anyhow::ensure!(
            recovered["status"] == "completed"
                && parent["status"] == "completed"
                && child["dispatch"]["state"] == "completed"
                && child["browser"]["runtime"]["grokCommitReceipt"]["dispatchId"]
                    == child["dispatch"]["providerDispatchId"]
                && child["browser"]["runtime"]["acceptedObservation"]["wireReceipt"]["state"]
                    == "completed",
            "Recovered Grok attempt is incomplete"
        );
        anyhow::ensure!(
            String::from_utf8(captured(&store, parent_id)?)? == ANSWER
                && fs::read_to_string(manifest["output"].as_str().context("No output")?)?
                    .contains(ANSWER),
            "Recovered capture or output lost the original Grok answer"
        );
        // Chromium can list a closed target briefly after its retirement is recorded.
        let mut targets = targets;
        let settle = Instant::now() + Duration::from_secs(5);
        while targets != baseline && Instant::now() < settle {
            tokio::time::sleep(Duration::from_millis(50)).await;
            targets = target_ids(
                &browser
                    .call("Target.getTargets", json!({}), limits.command, &cancel)
                    .await?,
            );
        }
        anyhow::ensure!(
            child["browser"]["runtime"]["targetRetiredAt"].is_string() && targets == baseline,
            "Recovered Grok target was retained or another target changed"
        );
        let claims = std::iter::once(&initial)
            .chain(&writes)
            .map(|raw| serde_json::from_str::<Value>(raw.as_str().unwrap_or("null")))
            .collect::<Result<Vec<_>, _>>()?;
        let retiring = claims
            .iter()
            .position(|claim| claim["retiring"] == true)
            .context("PUBLIC recovery recorded no retirement claim write")?;
        let before = &claims[retiring.checked_sub(1).context("Retirement preceded recovery")?];
        anyhow::ensure!(
            before["submission"].is_null() || before["submission"]["state"] == "clean",
            "Recovered Grok witness was not sealed before retirement: {before}"
        );
        eprintln!(
            "PUBLIC Grok recovery {hold}: one original Send, durable parent output, exact target absent"
        );
        Ok(())
    }
    .await;
    if checked.is_err() || !damage.is_empty() {
        // This fixture allocated the target. Its error teardown never runs in production.
        let _ = browser
            .close_owned_target(target_id, limits.command, &CancellationToken::new())
            .await;
    }
    checked
}

#[tokio::test]
#[ignore = "Subprocess fixture, invoked only by the native Grok recovery cases"]
async fn grok_recovery_worker_helper() {
    let Some(root) = std::env::var_os("INDUSTRIOUS_GROK_RECOVERY") else {
        return;
    };
    if let Err(error) = recovery_worker(Path::new(&root)).await {
        eprintln!("PUBLIC Grok recovery worker failed: {error:#}");
        std::process::exit(3);
    }
}

async fn recovery_worker(root: &Path) -> Result<()> {
    let hold = std::env::var("INDUSTRIOUS_GROK_RECOVERY_HOLD")?;
    let endpoint = AttachmentEndpoint {
        host: std::env::var("INDUSTRIOUS_GROK_RECOVERY_HOST")?,
        port: std::env::var("INDUSTRIOUS_GROK_RECOVERY_PORT")?.parse()?,
        browser_ws_endpoint: std::env::var("INDUSTRIOUS_GROK_RECOVERY_WS")?,
    };
    // The answer deadline outlives the crash point; the witness lifetime covers recovery.
    let limits = RunnerLimits {
        command: Duration::from_secs(5),
        admission: Duration::from_secs(5),
        commit: Duration::from_secs(30),
        answer: Duration::from_secs(30),
        poll: Duration::from_millis(20),
        ..RunnerLimits::default()
    };
    let cancel = CancellationToken::new();
    let browser = endpoint.connect(limits.command, &cancel).await?;
    let home = root.join("home");
    fs::create_dir_all(&home)?;
    let store = SessionStore::new(home.join(".oracle"));
    let output = home.join("PUBLIC-answer.md");
    let environment = crate::config::Environment::from([
        (
            "ORACLE_HOME_DIR".into(),
            store.home().to_string_lossy().into(),
        ),
        (
            (if cfg!(windows) { "USERPROFILE" } else { "HOME" }).into(),
            home.to_string_lossy().into(),
        ),
    ]);
    let resolved = crate::execution::resolve(
        &json!({"prompt":PROMPT,"engine":"browser","model":"gpt-6-pro","writeOutputPath":output,"browserFallback":"grok"}),
        &json!({"browser":{"manualLogin":true,"cookieSync":false}}),
        &environment,
        &home,
    )?;
    let parent = prepare(&store, &resolved, &home)?;
    let parent_id = parent["id"].as_str().context("No public fixture parent")?;
    let identity = ProcessIdentity::current()?;
    start_owner(&store, parent_id, &identity)?;
    let mut coordinator =
        crate::browser_attempt::Parent::begin(&store, parent_id, &identity, &cancel)?;
    let first = coordinator.reserve_first(&cancel)?;
    // The ChatGPT attempt is inert evidence whose bounded recovery expired, as in a real switch.
    store.update(&first.child_id, |meta| {
        let mut runtime = meta["browser"]["runtime"].clone();
        if !runtime.is_object() {
            runtime = json!({});
        }
        runtime["chromeTargetId"] = json!("PUBLIC-chatgpt-target");
        runtime["providerReadRecovery"] = json!({"version":1,"state":"expired",
            "readOnly":true,"targetId":"PUBLIC-chatgpt-target"});
        Ok(Some(json!({"status":"error","recoveryRequired":true,
            "dispatch":{"state":"uncertain","reservationId":"PUBLIC"},
            "browser":merge_object(&meta["browser"],&json!({"runtime":runtime}))})))
    })?;
    let failure = anyhow::Error::new(crate::browser_provider::Failure::AnswerDeadline)
        .context("Bounded original-target observation expired");
    let link = coordinator
        .switch_to_grok(&first, &failure, &environment, &cancel)?
        .context("PUBLIC provider switch did not start")?;
    start_owner(&store, &link.child_id, &identity)?;
    let child = store.require(&link.child_id)?;
    let profile = ProfileRegistry::open(Path::new(
        child["browser"]["config"]["manualLoginProfileDir"]
            .as_str()
            .context("No Grok profile")?,
    ))?;
    let lease = Arc::new(
        profile
            .acquire_with_limit(&link.child_id, 1, limits.admission, &cancel)
            .await?,
    );
    let _heartbeat = lease.heartbeat();
    let mut target = TargetAcquisition::default()
        .acquire(browser.clone(), lease.clone(), &cancel, limits.command)
        .await?;
    // Production admission records the endpoint that recovery later requires.
    lease.update(&json!({"chromeHost":endpoint.host,"chromePort":endpoint.port}))?;
    let target_id = target.id().to_owned();
    let page = target.page().clone();
    let session = page
        .session_id()
        .context("No owned fixture session")?
        .to_owned();
    let mut events = browser.events();
    page.call(
        "Fetch.enable",
        json!({"patterns":[{"urlPattern":"*"}]}),
        limits.command,
        &cancel,
    )
    .await?;
    let intercepted = page.clone();
    let html = recovery_document(&hold);
    let _interceptor = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if event["sessionId"] != session || event["method"] != "Fetch.requestPaused" {
                continue;
            }
            anyhow::ensure!(
                event["params"]["request"]["url"] == "https://grok.com/",
                "Unexpected fixture network request"
            );
            intercepted.call("Fetch.fulfillRequest",json!({"requestId":event["params"]["requestId"],"responseCode":200,
                "responseHeaders":[{"name":"Content-Type","value":"text/html"}],"body":STANDARD.encode(&html)}),Duration::from_secs(5),&CancellationToken::new()).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let bringup = DirectoryLock::acquire_async(
        &profile.path().join("oracle-browser-bringup.lock"),
        limits.command,
        &cancel,
    )
    .await?;
    let witness = ExecutionWitness::capture(&store, &link.child_id, &identity)?;
    let committed = request(&store, &link.child_id)?;
    let crash_state = if hold == "user" {
        "started"
    } else {
        "acknowledged"
    };
    let crash_point = async {
        loop {
            let state = store.stored_for_update(&link.child_id)?["dispatch"]["state"].clone();
            let sent = page
                .evaluate("window.PUBLIC_sendCount??0", limits.command, &cancel)
                .await?
                == 1;
            if sent && state == crash_state {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let manifest = json!({"parentId":parent_id,"childId":link.child_id,"targetId":target_id,
        "profileDir":profile.path(),"home":home,"output":output,"identity":identity});
    // A genuine crash: no destructor, witness stop, quarantine or cleanup runs.
    let ready = root.join("ready.json");
    let crash = move || -> ! {
        if let Err(error) = crate::storage::atomic_json(&ready, &manifest) {
            eprintln!("PUBLIC Grok recovery manifest failed: {error:#}");
            std::process::exit(3);
        }
        std::process::exit(0)
    };
    match hold.as_str() {
        // Exits after the child capture, before the worker's own clean release.
        "captured" => {
            let crash = crash.clone();
            TEST_BEFORE_ATTEMPT_RELEASE.set(Some(Box::new(move || crash())));
        }
        // Exits after the committed receipt is saved, before the renderer acknowledges it.
        "receipt" => {
            let crash = crash.clone();
            TEST_BEFORE_GROK_ACKNOWLEDGE.set(Some(Box::new(move || crash())));
        }
        // Exits after the child capture and clean release, before parent adoption.
        "delivered" => {}
        _ => {
            tokio::select! {
                delivered = run_owned(&store,&link.child_id,&witness,&committed,&mut target,&endpoint,bringup,&limits,&cancel,&CaptureBoundary::ParentAttempt,None,None) => {
                    anyhow::bail!("PUBLIC worker finished before its crash point: {:?}", delivered.err());
                }
                ready = crash_point => ready?,
            }
            crash()
        }
    }
    let delivered = run_owned(
        &store,
        &link.child_id,
        &witness,
        &committed,
        &mut target,
        &endpoint,
        bringup,
        &limits,
        &cancel,
        &CaptureBoundary::ParentAttempt,
        None,
        None,
    )
    .await;
    anyhow::ensure!(
        hold == "delivered",
        "PUBLIC worker passed its crash point: {:?}",
        delivered.err()
    );
    delivered?;
    crash()
}

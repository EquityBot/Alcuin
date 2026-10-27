//! Session inspection is separate from durable completion and native target retirement.
//! Reference: src/cli/browserTabs.ts and src/browser/sessionHarvest.ts.
use crate::{
    browser::{conversation_id, normalise_host},
    browser_chrome::{AttachmentEndpoint, read_devtools_endpoint},
    browser_harvest::{self, HarvestIntegrity},
    browser_output,
    browser_tabs::{self, HarvestCompletionGate},
    policy,
    sessions::SessionStore,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}
pub fn implicit_harvest(meta: &Value) -> Option<&Value> {
    let harvest = meta["browser"].get("harvest")?;
    if let Some(integrity) = harvest.get("integrity").filter(|v| !v.is_null())
        && (integrity["status"] != "matched" || integrity["explicitTarget"] == true)
    {
        return None;
    }
    Some(harvest)
}
pub fn session_tab_ref(meta: &Value) -> &str {
    let runtime = &meta["browser"]["runtime"];
    let harvest = implicit_harvest(meta).unwrap_or(&Value::Null);
    [
        &harvest["url"],
        &runtime["tabUrl"],
        &meta["browser"]["archive"]["conversationUrl"],
        &harvest["conversationId"],
        &runtime["conversationId"],
        &harvest["targetId"],
        &runtime["chromeTargetId"],
    ]
    .into_iter()
    .find_map(text)
    .unwrap_or("current")
}
/// Saved stable conversation URL, else the reference buildConversationUrl rebuild from the
/// recorded conversation ID and configured base, shared with follow-ups.
pub fn recovery_url(meta: &Value) -> Option<String> {
    crate::followup::browser_resume(meta)
}
pub fn recovery_profile(meta: &Value) -> Result<&Path> {
    let config = &meta["browser"]["config"];
    ensure!(
        config["manualLogin"] == true,
        "Cannot recover conversation: session was not run with a manual-login browser profile."
    );
    text(&meta["browser"]["runtime"]["userDataDir"]).or_else(||text(&meta["browser"]["runtime"]["profileDir"])).or_else(||text(&config["manualLoginProfileDir"]))
        .filter(|s|!policy::trim(s).is_empty()).map(Path::new)
        .context("Cannot recover conversation: session metadata has no recorded manual-login profile directory.")
}
pub fn session_matches_tab(meta: &Value, tab: &Value) -> bool {
    let runtime = &meta["browser"]["runtime"];
    let harvest = &meta["browser"]["harvest"];
    if [
        &runtime["chromeHost"],
        &meta["browser"]["config"]["remoteChrome"]["host"],
    ]
    .into_iter()
    .filter_map(text)
    .any(|host| tab["host"].as_str().is_some_and(|actual| actual != host))
    {
        return false;
    }
    let conversation = text(&tab["conversationId"])
        .map(str::to_owned)
        .or_else(|| tab["url"].as_str().and_then(conversation_id));
    [
        ("chromeTargetId", "targetId", runtime),
        ("targetId", "targetId", harvest),
        ("tabUrl", "url", runtime),
        ("url", "url", harvest),
    ]
    .into_iter()
    .any(|(saved, observed, record)| {
        text(&record[saved]).is_some_and(|s| tab[observed].as_str() == Some(s))
    }) || conversation.is_some_and(|id| {
        [&runtime["conversationId"], &harvest["conversationId"]]
            .into_iter()
            .filter_map(text)
            .any(|saved| saved == id)
    })
}
pub async fn session_endpoint(
    meta: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Option<AttachmentEndpoint>> {
    let runtime = &meta["browser"]["runtime"];
    let remote = &meta["browser"]["config"]["remoteChrome"];
    let Some(host) = text(&runtime["chromeHost"]).or_else(|| text(&remote["host"])) else {
        return Ok(None);
    };
    let port = runtime
        .get("chromePort")
        .filter(|v| !v.is_null())
        .unwrap_or(&remote["port"]);
    let Some(port) = port
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .filter(|n| *n > 0)
    else {
        return Ok(None);
    };
    let saved =
        text(&runtime["chromeBrowserWSEndpoint"]).or_else(|| text(&runtime["browserWSEndpoint"]));
    let Some(saved) = saved else {
        return inspect_endpoint(host, port, None, timeout, cancel)
            .await
            .map(Some);
    };
    let mut endpoint = AttachmentEndpoint {
        host: host.into(),
        port,
        browser_ws_endpoint: saved.into(),
    };
    endpoint.validate()?;
    if let Some(profile) =
        text(&runtime["chromeProfileRoot"]).or_else(|| text(&runtime["userDataDir"]))
        && let Ok(Some(active)) = read_devtools_endpoint(Path::new(profile))
        && normalise_host(&active.host) == normalise_host(host)
    {
        endpoint.port = active.port;
        let mut url = reqwest::Url::parse(saved)?;
        url.set_port(Some(active.port))
            .map_err(|_| anyhow::anyhow!("Invalid refreshed Chrome port"))?;
        url.set_path(reqwest::Url::parse(&active.browser_ws_endpoint)?.path());
        endpoint.browser_ws_endpoint = url.into();
    } else if let Ok(active) = inspect_endpoint(
        host,
        port,
        Some(saved),
        timeout.min(Duration::from_secs(1)),
        cancel,
    )
    .await
    {
        let mut url = reqwest::Url::parse(saved)?;
        url.set_path(reqwest::Url::parse(&active.browser_ws_endpoint)?.path());
        endpoint.browser_ws_endpoint = url.into();
    }
    ensure!(
        !cancel.is_cancelled(),
        "Session endpoint inspection cancelled"
    );
    endpoint.validate()?;
    Ok(Some(endpoint))
}
async fn inspect_endpoint(
    host: &str,
    port: u16,
    saved: Option<&str>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<AttachmentEndpoint> {
    let base = crate::browser_chrome::endpoint_base(host, port)?;
    let version = crate::cdp::endpoint_json(&base, "/json/version", timeout, cancel).await?;
    let advertised = reqwest::Url::parse(
        version["webSocketDebuggerUrl"]
            .as_str()
            .context("Chrome has no advertised browser socket")?,
    )?;
    let mut socket = reqwest::Url::parse(saved.unwrap_or(&base))?;
    if saved.is_none() {
        socket
            .set_scheme("ws")
            .map_err(|_| anyhow::anyhow!("Invalid historical Chrome socket scheme"))?;
    }
    socket.set_path(advertised.path());
    let endpoint = AttachmentEndpoint {
        host: host.into(),
        port,
        browser_ws_endpoint: socket.into(),
    };
    endpoint.validate()?;
    Ok(endpoint)
}
#[derive(Debug)]
pub struct HarvestSnapshot {
    pub endpoint: AttachmentEndpoint,
    pub tab: Value,
    pub integrity: HarvestIntegrity,
}
pub async fn tabs_status(store: &SessionStore, cancel: &CancellationToken) -> Result<String> {
    let metas = store.list(true).unwrap_or_default();
    let timeout = Duration::from_secs(2);
    let mut endpoints = vec![];
    if let Ok(endpoint) = AttachmentEndpoint::inspect("127.0.0.1", 9222, timeout, cancel).await {
        endpoints.push(endpoint);
    }
    for endpoint in saved_endpoints(&metas, timeout, cancel).await? {
        if !endpoints.contains(&endpoint) {
            endpoints.push(endpoint);
        }
    }
    let mut output = String::new();
    for endpoint in endpoints {
        let tabs = browser_tabs::collect(&endpoint, timeout, cancel).await;
        ensure!(!cancel.is_cancelled(), "Browser tab listing cancelled");
        if let Ok(tabs) = tabs
            && !tabs.is_empty()
        {
            output.push_str(&format_tabs(&endpoint, &tabs, &metas));
        }
    }
    if output.is_empty() {
        output.push_str("No live ChatGPT tabs found on known Chrome DevTools endpoints.\n");
    }
    Ok(output)
}
async fn saved_endpoints(
    metas: &[Value],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<AttachmentEndpoint>> {
    let mut seen = HashSet::new();
    let mut endpoints = vec![];
    for meta in metas {
        ensure!(!cancel.is_cancelled(), "Browser tab listing cancelled");
        let runtime = &meta["browser"]["runtime"];
        let remote = &meta["browser"]["config"]["remoteChrome"];
        let socket = text(&runtime["chromeBrowserWSEndpoint"])
            .or_else(|| text(&runtime["browserWSEndpoint"]));
        let basis = json!([
            text(&runtime["chromeHost"]).or_else(|| text(&remote["host"])),
            runtime
                .get("chromePort")
                .filter(|v| !v.is_null())
                .unwrap_or(&remote["port"]),
            socket,
            socket
                .and_then(|_| text(&runtime["chromeProfileRoot"])
                    .or_else(|| text(&runtime["userDataDir"])))
        ])
        .to_string();
        if !seen.insert(basis) {
            continue;
        }
        if let Ok(Some(endpoint)) = session_endpoint(meta, timeout, cancel).await
            && !endpoints.contains(&endpoint)
        {
            endpoints.push(endpoint);
        }
    }
    ensure!(!cancel.is_cancelled(), "Browser tab listing cancelled");
    Ok(endpoints)
}
fn format_tabs(endpoint: &AttachmentEndpoint, tabs: &[Value], metas: &[Value]) -> String {
    let mut sessions: Vec<_> = metas
        .iter()
        .filter(|meta| meta["mode"] == "browser")
        .collect();
    sessions.sort_by(|left, right| {
        right["createdAt"]
            .as_str()
            .unwrap_or("")
            .cmp(left["createdAt"].as_str().unwrap_or(""))
    });
    let mut output = format!("Browser Tabs {}:{}\n", endpoint.host, endpoint.port);
    for tab in tabs {
        let model = text(&tab["currentModelLabel"]).unwrap_or("(unknown)");
        output.push_str(&format!(
            "- {} {} model={model} turns={} stop={} send={}\n  title={}\n  url={}\n",
            tab["targetId"].as_str().unwrap_or(""),
            tab["state"].as_str().unwrap_or("detached"),
            tab["assistantCount"].as_f64().unwrap_or(0.0),
            if tab["stopExists"] == true {
                "yes"
            } else {
                "no"
            },
            if tab["sendExists"] == true {
                "yes"
            } else {
                "no"
            },
            text(&tab["title"]).unwrap_or("(untitled)"),
            tab["url"].as_str().unwrap_or("")
        ));
        if let Some(linked) = sessions.iter().find(|meta| session_matches_tab(meta, tab)) {
            output.push_str(&format!(
                "  session={}\n",
                linked["id"].as_str().unwrap_or("")
            ));
        }
        if let Some(snippet) = text(&tab["lastAssistantSnippet"]) {
            let units: Vec<_> = snippet.encode_utf16().collect();
            let snippet = if units.len() > 120 {
                format!(
                    "{}…",
                    policy::trim_end(&String::from_utf16_lossy(&units[..119]))
                )
            } else {
                snippet.into()
            };
            output.push_str(&format!("  last={snippet}\n"));
        }
    }
    output
}
pub async fn harvest_target(
    store: &SessionStore,
    id: &str,
    endpoint: &AttachmentEndpoint,
    reference: &str,
    explicit: bool,
    cancel: &CancellationToken,
) -> Result<HarvestSnapshot> {
    harvest_target_within(
        store,
        id,
        endpoint,
        reference,
        explicit,
        cancel,
        Duration::MAX,
    )
    .await
}
/// Reference harvest freshness refusal, with its whole-second timeout rounded up.
fn stale_harvest(timeout: Duration) -> String {
    format!(
        "Latest ChatGPT turn did not contain an completed assistant answer paired with this session prompt after {}s; refusing to harvest stale output.",
        timeout.as_millis().div_ceil(1000)
    )
}
pub(crate) async fn harvest_target_within(
    store: &SessionStore,
    id: &str,
    endpoint: &AttachmentEndpoint,
    reference: &str,
    explicit: bool,
    cancel: &CancellationToken,
    budget: Duration,
) -> Result<HarvestSnapshot> {
    let meta = store.require(id)?;
    // Check explicit null before connecting to any browser.
    browser_tabs::ready_for_session(&meta, &Value::Null, explicit)?;
    let pinned = crate::browser_attempt::pinned_target(&meta, Some(reference))?;
    if let Some((saved, _)) = &pinned {
        ensure!(
            saved == endpoint,
            "Provider harvest endpoint differs from its original socket"
        );
    }
    let timeout = Duration::from_millis(
        meta["browser"]["config"]["inputTimeoutMs"]
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(60_000),
    )
    .min(budget);
    let started = Instant::now();
    let deadline = started + timeout;
    let operation_cancel = cancel.child_token();
    let operation = async {
        let mut gate = HarvestCompletionGate::default();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .with_context(|| stale_harvest(timeout))?;
            let target = browser_tabs::resolve_target(
                endpoint,
                Some(reference),
                pinned.is_some() || !explicit,
                remaining,
                &operation_cancel,
            )
            .await?;
            ensure!(
                pinned.is_none() || target["targetId"] == reference,
                "Original provider harvest target changed"
            );
            let mut tab =
                browser_tabs::inspect(endpoint, &target, true, remaining, &operation_cancel)
                    .await?;
            gate.observe(
                &mut tab,
                started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                3,
                1200,
            );
            if browser_tabs::ready_for_session(&meta, &tab, explicit)? {
                ensure!(
                    !operation_cancel.is_cancelled(),
                    "Browser harvest cancelled"
                );
                let integrity = browser_harvest::persist(store, id, &tab, explicit)?;
                ensure!(
                    !operation_cancel.is_cancelled(),
                    "Browser harvest cancelled"
                );
                return Ok(HarvestSnapshot {
                    endpoint: endpoint.clone(),
                    tab,
                    integrity,
                });
            }
            tokio::select! {biased; _=operation_cancel.cancelled()=>bail!("Browser harvest cancelled"),_=tokio::time::sleep(Duration::from_millis(250))=>{}}
        }
    };
    tokio::pin!(operation);
    tokio::select! {biased;
        result=&mut operation=>result,
        _=tokio::time::sleep_until(deadline)=>{
            operation_cancel.cancel();
            operation.await.context("Browser harvest timed out after acknowledged observation cleanup").and_then(|_|bail!("Browser harvest timed out"))
        }
    }
}
pub fn write_output(
    store: &SessionStore,
    cwd: &Path,
    destination: &str,
    answer: &str,
    cancel: &CancellationToken,
) -> Result<Option<PathBuf>> {
    if matches!(destination, "-" | "/dev/stdout") {
        return Ok(None);
    }
    let resolved = crate::prompts::absolute(cwd, destination);
    browser_output::save_inspection(store, &resolved, answer.as_bytes(), cancel)?;
    Ok(Some(resolved))
}

pub fn harvest_summary(id: &str, snapshot: &HarvestSnapshot) -> String {
    let tab = &snapshot.tab;
    let mut summary = format!(
        "Session: {id}\nTarget: {}\nState: {}\nModel: {}\nURL: {}\nAssistant turns: {}\nCapture identity: {}{}\n",
        tab["targetId"].as_str().unwrap_or(""),
        tab["state"].as_str().unwrap_or("detached"),
        text(&tab["currentModelLabel"]).unwrap_or("(unknown)"),
        tab["url"].as_str().unwrap_or(""),
        tab["assistantCount"].as_f64().unwrap_or(0.0),
        snapshot.integrity.status,
        if snapshot.integrity.explicit_target {
            " (explicit target)"
        } else {
            ""
        }
    );
    if snapshot.integrity.status == "mismatch" {
        summary.push_str("Explicit harvest target differs from the saved capture; original artifacts are unchanged.\n");
    }
    summary.push_str(&format!(
        "Signals: stop={} send={}\n",
        if tab["stopExists"] == true {
            "yes"
        } else {
            "no"
        },
        if tab["sendExists"] == true {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(user) = text(&tab["lastUserSnippet"]) {
        summary.push_str(&format!("Last user: {user}\n"));
    }
    summary.push_str("---\n");
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_or_unverified_harvest_never_redirects_implicit_recovery() {
        let mut meta = json!({"browser":{"runtime":{"tabUrl":"https://chatgpt.com/c/public-runtime","conversationId":"public-runtime","chromeTargetId":"PUBLIC-runtime"},"archive":{"conversationUrl":"https://chatgpt.com/c/public-archive"},"harvest":{"url":"https://chatgpt.com/c/public-harvest","integrity":{"status":"matched","explicitTarget":false}}}});
        assert_eq!(
            session_tab_ref(&meta),
            "https://chatgpt.com/c/public-harvest"
        );
        assert_eq!(
            recovery_url(&meta).as_deref(),
            Some("https://chatgpt.com/c/public-harvest")
        );
        for integrity in [
            json!({"status":"matched","explicitTarget":true}),
            json!({"status":"mismatch"}),
            json!({"status":"unverified"}),
        ] {
            meta["browser"]["harvest"]["integrity"] = integrity;
            assert_eq!(
                session_tab_ref(&meta),
                "https://chatgpt.com/c/public-runtime"
            );
            assert_eq!(
                recovery_url(&meta).as_deref(),
                Some("https://chatgpt.com/c/public-runtime")
            );
        }
        meta["browser"]["runtime"]["tabUrl"] = json!("https://foreign.example/c/public");
        assert_eq!(
            recovery_url(&meta).as_deref(),
            Some("https://chatgpt.com/c/public-archive")
        );
        let mut legacy = json!({"browser":{"config":{"url":"https://chatgpt.com/"},"runtime":{"conversationId":"public-id"}}});
        assert_eq!(
            recovery_url(&legacy).as_deref(),
            Some("https://chatgpt.com/c/public-id")
        );
        legacy["browser"]["config"]["url"] = json!("https://foreign.example/");
        assert_eq!(recovery_url(&legacy), None);
    }
    #[test]
    fn profile_reopening_requires_recorded_manual_login_and_does_not_choose_cookie_sources() {
        let mut meta = json!({"browser":{"config":{"manualLogin":false,"manualLoginProfileDir":"PUBLIC profile","copyProfileSource":"PUBLIC unrelated profile"}}});
        assert!(recovery_profile(&meta).is_err());
        meta["browser"]["config"]["manualLogin"] = json!(true);
        assert_eq!(
            recovery_profile(&meta).unwrap(),
            Path::new("PUBLIC profile")
        );
        meta["browser"]["runtime"] = json!({"userDataDir":"PUBLIC recorded profile"});
        assert_eq!(
            recovery_profile(&meta).unwrap(),
            Path::new("PUBLIC recorded profile")
        );
        meta["browser"]["runtime"]["userDataDir"] = json!(" ");
        assert!(recovery_profile(&meta).is_err());
    }
    #[test]
    fn native_reopening_uses_its_recorded_profile_directory() {
        let mut meta = json!({"browser":{"config":{"manualLogin":true},"runtime":{"profileDir":"PUBLIC native profile"}}});
        assert_eq!(
            recovery_profile(&meta).unwrap(),
            Path::new("PUBLIC native profile")
        );
        meta["browser"]["config"]["manualLogin"] = json!(false);
        assert!(recovery_profile(&meta).is_err());
    }
    #[test]
    fn session_links_are_display_only_and_require_the_recorded_host() {
        let meta = json!({"status":"error","browser":{"runtime":{"chromeHost":"PUBLIC-host","chromeTargetId":"PUBLIC-target","conversationId":"public-a"}}});
        assert!(session_matches_tab(
            &meta,
            &json!({"host":"PUBLIC-host","targetId":"PUBLIC-target"})
        ));
        assert!(session_matches_tab(
            &meta,
            &json!({"host":"PUBLIC-host","url":"https://chatgpt.com/c/public-a"})
        ));
        assert!(!session_matches_tab(
            &meta,
            &json!({"host":"PUBLIC-foreign","targetId":"PUBLIC-target"})
        ));
        assert!(!session_matches_tab(
            &meta,
            &json!({"host":"PUBLIC-host","targetId":"PUBLIC-foreign"})
        ));
        assert_eq!(meta["status"], "error");
    }
    #[test]
    fn browser_tab_status_links_the_newest_matching_browser_session() {
        let endpoint = AttachmentEndpoint {
            host: "PUBLIC-host".into(),
            port: 9222,
            browser_ws_endpoint: "ws://PUBLIC-host:9222/devtools/browser/PUBLIC".into(),
        };
        let tab = json!({"host":"PUBLIC-host","targetId":"PUBLIC-target","url":"https://chatgpt.com/c/public-a","state":"running","assistantCount":1,"stopExists":false,"sendExists":true,"lastAssistantSnippet":"PUBLIC answer"});
        let metas = [
            json!({"id":"old","mode":"browser","createdAt":"2026-10-15","browser":{"runtime":{"chromeTargetId":"PUBLIC-target"}}}),
            json!({"id":"new","mode":"browser","createdAt":"2026-10-16","browser":{"runtime":{"chromeTargetId":"PUBLIC-target"}}}),
            json!({"id":"api","mode":"api","createdAt":"2026-10-17","browser":{"runtime":{"chromeTargetId":"PUBLIC-target"}}}),
        ];
        assert_eq!(
            format_tabs(&endpoint, &[tab], &metas),
            "Browser Tabs PUBLIC-host:9222\n- PUBLIC-target running model=(unknown) turns=1 stop=no send=yes\n  title=(untitled)\n  url=https://chatgpt.com/c/public-a\n  session=new\n  last=PUBLIC answer\n"
        );
    }
    #[test]
    fn inspection_write_uses_command_destination_and_preserves_original_output_requirements() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("store"));
        let meta=store.create(json!({"prompt":"PUBLIC inspection write","model":"gpt-6-astra","writeOutputPath":"PUBLIC-original.md"}),home.path(),None).unwrap();
        let id = meta["id"].as_str().unwrap();
        let before = store.require(id).unwrap();
        let path = write_output(
            &store,
            home.path(),
            "PUBLIC-inspection.md",
            "  PUBLIC answer\r\n ",
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"  PUBLIC answer\r\n ");
        assert_eq!(store.require(id).unwrap(), before);
        assert!(
            write_output(
                &store,
                home.path(),
                "-",
                "PUBLIC stdout",
                &CancellationToken::new()
            )
            .unwrap()
            .is_none()
        );
    }
    #[tokio::test]
    async fn repeated_forwarded_endpoints_deduplicate_before_io_and_preserve_saved_authority() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.fetch_add(1, Ordering::SeqCst);
                let mut request = vec![0; 4096];
                let _ = stream.read(&mut request).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                let body = json!({"webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/PUBLIC-new"}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let mut metas = vec![
            json!({"browser":{"runtime":{"chromeHost":"127.0.0.1","chromePort":port,"chromeBrowserWSEndpoint":format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-old")}}});
            100
        ];
        metas.extend(vec![
            json!({"browser":{"runtime":{"chromeHost":"127.0.0.1","chromePort":port}}});
            100
        ]);
        for (group, expected_requests) in [(&metas[..100], 1), (&metas[100..], 2), (&metas[..], 4)]
        {
            let found = tokio::time::timeout(
                Duration::from_millis(500),
                saved_endpoints(group, Duration::from_secs(2), &CancellationToken::new()),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(count.load(Ordering::SeqCst), expected_requests);
            assert_eq!(found[0].port, port);
            assert_eq!(
                found[0].browser_ws_endpoint,
                format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-new")
            );
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
    #[tokio::test]
    async fn null_submission_refuses_before_connecting_to_a_browser() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta = store
            .create(
                json!({"prompt":"PUBLIC null harvest","model":"gpt-6-astra"}),
                home.path(),
                None,
            )
            .unwrap();
        let id = meta["id"].as_str().unwrap();
        store
            .update(id, |_| {
                Ok(Some(
                    json!({"browser":{"runtime":{"submittedPromptHash":null}}}),
                ))
            })
            .unwrap();
        let endpoint = AttachmentEndpoint {
            host: "127.0.0.1".into(),
            port: 1,
            browser_ws_endpoint: "ws://127.0.0.1:1/devtools/browser/PUBLIC-unreachable".into(),
        };
        assert!(
            harvest_target(
                &store,
                id,
                &endpoint,
                "PUBLIC-target",
                false,
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("no confirmed submitted")
        );
    }
}
#[cfg(test)]
mod stale_harvest_tests {
    #[test]
    fn stale_harvest_refusal_uses_the_reference_wording() {
        assert_eq!(
            super::stale_harvest(std::time::Duration::from_millis(60_001)),
            "Latest ChatGPT turn did not contain an completed assistant answer paired with this session prompt after 61s; refusing to harvest stale output."
        );
    }
}

//! Export an exact labelled native reply without submitting a prompt or launching Chrome.
use crate::{
    artifacts, browser,
    browser_chrome::{AttachmentEndpoint, read_devtools_endpoint},
    browser_disposition::TargetJournal,
    browser_profile::ProfileRegistry,
    browser_tabs::{self, HarvestCompletionGate},
    browser_target::TargetAcquisition,
    cdp::Cdp,
    config::Environment,
    native_fs::{NativeEntry, NativeIdentity, read_stamp},
    policy,
    process::ProcessIdentity,
    sessions::{SessionStore, timestamp},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{io::Read, path::Path, sync::Arc, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const LIMIT: u64 = 32_768;
const COMMAND: Duration = Duration::from_secs(10);
const MISMATCH: &str =
    "The completed reply does not match the saved request; no answer was written.";
pub(crate) const TIMEOUT: &str = "Browser inspection or recovery exceeded its timeout.";

#[derive(Debug, PartialEq, Eq)]
struct Request {
    chat_id: String,
    label: String,
    turn_id: Option<String>,
}
impl Request {
    fn parse(value: &Value) -> Result<Self> {
        ensure!(value.is_object(), "Invalid native reply request.");
        let chat = value["chatId"].as_str();
        let label = value["label"].as_str();
        let valid = chat.is_some_and(|s| {
            (1..=100).contains(&s.len())
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }) && label.is_some_and(|s| {
            (12..=128).contains(&s.len())
                && s.as_bytes()[0].is_ascii_alphanumeric()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }) && value.get("turnId").is_none_or(|v| {
            v.as_str()
                .is_some_and(|s| !policy::trim(s).is_empty() && s.encode_utf16().count() <= 100)
        });
        ensure!(
            valid,
            "Request requires chatId, a unique label of 12 to 128 letters, digits, dots, underscores or hyphens, and an optional turnId."
        );
        Ok(Self {
            chat_id: chat.unwrap().into(),
            label: label.unwrap().into(),
            turn_id: value
                .get("turnId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

fn read_request(path: &Path) -> Result<Request> {
    let mut entry = NativeEntry::open(path, false)
        .map_err(|_| anyhow::anyhow!("Native reply request could not be read."))?;
    let size = entry
        .handle
        .metadata()
        .map_err(|_| anyhow::anyhow!("Native reply request could not be read."))?
        .len();
    ensure!(size <= LIMIT, "Native reply request exceeds 32 KiB.");
    let json = (|| {
        let stamp = read_stamp(&entry.handle)?;
        let mut bytes = Vec::new();
        (&mut entry.handle)
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= LIMIT, "Request grew beyond its limit");
        entry.require_matches(path)?;
        ensure!(
            read_stamp(&entry.handle)? == stamp,
            "Native request changed while reading"
        );
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        Ok::<Value, anyhow::Error>(serde_json::from_slice(bytes)?)
    })()
    .map_err(|_| anyhow::anyhow!("Native reply request could not be read as JSON."))?;
    Request::parse(&json)
}

fn matches_url(raw: &str, chat_id: &str) -> bool {
    browser::provider_url(raw).is_some_and(|url| {
        url.host_str() == Some("chatgpt.com")
            && browser::conversation_id(url.as_str()).as_deref() == Some(chat_id)
    })
}

fn answer_placeholder(text: &str) -> bool {
    let text = text
        .to_lowercase()
        .split(policy::javascript_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if matches!(
        text.as_str(),
        "answer now" | "chatgpt said:" | "chatgpt said"
    ) {
        return true;
    }
    if text.encode_utf16().count() > 60 {
        return false;
    }
    let mut rest = text.as_str();
    let mut owner = false;
    let mut gate = false;
    while !rest.is_empty() {
        let Some(label) = [
            "chatgpt said:",
            "chatgpt said",
            "file upload request",
            "pro thinking",
            "answer now",
        ]
        .into_iter()
        .find(|label| rest.starts_with(label)) else {
            return false;
        };
        if matches!(label, "answer now" | "file upload request") {
            gate = true;
        } else {
            owner = true;
        }
        rest = rest[label.len()..]
            .trim_start_matches(|c| policy::javascript_whitespace(c) || ":.,;|·•-".contains(c));
    }
    owner && gate
}

fn ready(tab: &Value) -> bool {
    let text = tab["lastAssistantText"]
        .as_str()
        .or_else(|| tab["lastAssistantMarkdown"].as_str())
        .or_else(|| tab["lastAssistantSnippet"].as_str())
        .unwrap_or("");
    tab["assistantCount"].as_f64().is_some_and(|n| n > 0.0)
        && tab["assistantFollowsLatestUser"] == true
        && !policy::trim(text).is_empty()
        && !browser::image_only_ui_text(text)
        && !answer_placeholder(text)
}

fn validate_reply<'a>(request: &Request, tab: &'a Value) -> Result<&'a str> {
    // The raw bubble preserves the structured first line that rendered summaries flatten.
    let user = tab["lastUserTextRaw"]
        .as_str()
        .or_else(|| tab["lastUserText"].as_str())
        .unwrap_or("")
        .replace("\r\n", "\n");
    let label_matches = if request.turn_id.is_some() {
        user.contains(&request.label)
    } else {
        user.split('\n').next() == Some(format!("ORACLE_REQUEST_LABEL: {}", request.label).as_str())
            && user.matches("ORACLE_REQUEST_LABEL:").count() == 1
    };
    ensure!(
        tab["url"]
            .as_str()
            .is_some_and(|url| matches_url(url, &request.chat_id))
            && tab["conversationId"] == request.chat_id
            && tab["authenticated"] == true
            && tab["stopExists"] == false
            && tab["state"] == "completed"
            && tab["completionConfirmed"] == true
            && ready(tab)
            && label_matches
            && request
                .turn_id
                .as_ref()
                .is_none_or(|id| tab["lastUserMessageId"] == *id),
        MISMATCH
    );
    let answer = tab["lastAssistantMarkdown"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| tab["lastAssistantText"].as_str())
        .unwrap_or("");
    ensure!(
        !policy::trim(answer).is_empty(),
        "The reply is empty; no answer was written."
    );
    Ok(answer)
}

fn check(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Native reply recovery cancelled");
    Ok(())
}

async fn harvest(
    endpoint: &AttachmentEndpoint,
    target: &Value,
    created: bool,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let mut gate = HarvestCompletionGate::default();
    let mut awaiting_new = created;
    let operation_cancel = cancel.child_token();
    let operation = async {
        loop {
            check(&operation_cancel)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), TIMEOUT);
            let inspected = browser_tabs::inspect(
                endpoint,
                target,
                true,
                remaining.min(COMMAND),
                &operation_cancel,
            )
            .await;
            check(&operation_cancel)?;
            match inspected {
                Ok(mut tab) => {
                    awaiting_new = false;
                    gate.observe(&mut tab, started.elapsed().as_millis() as u64, 3, 1_000);
                    if tab["completionConfirmed"] == true && ready(&tab) {
                        return Ok(tab);
                    }
                }
                Err(error)
                    if awaiting_new
                        && error.to_string()
                            == "ChatGPT conversation changed or inspection failed; refusing mixed capture." =>
                    {}
                Err(_) => bail!("Unable to read the saved conversation."),
            }
            tokio::select! { biased; _ = operation_cancel.cancelled() => check(&operation_cancel)?, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
        }
    };
    tokio::pin!(operation);
    tokio::select! { biased;
        result = &mut operation => result,
        _ = cancel.cancelled() => { operation_cancel.cancel(); let _ = operation.await; check(cancel)?; unreachable!() },
        _ = tokio::time::sleep_until(deadline) => { operation_cancel.cancel(); match operation.await { Err(error) => Err(error.context(TIMEOUT)), Ok(_) => bail!(TIMEOUT) } },
    }
}

fn output_receipt(answer: &str, destination: &Path, cancel: &CancellationToken) -> Result<Value> {
    let result = (|| {
        // Existing parent only: publishing a native answer never creates a requested directory tree.
        let parent = destination
            .parent()
            .context("Answer output has no parent")?;
        let directory = NativeEntry::open(parent, true)?;
        let saved = artifacts::publish(answer.as_bytes(), destination, false, cancel)?;
        directory.require_matches(parent)?;
        let linked = NativeEntry::open(destination, false)?;
        ensure!(
            linked.identity == saved.native_identity,
            "Native output identity changed"
        );
        let (dev, ino) = NativeIdentity::legacy_stat(&linked.handle)?;
        ensure!(ino != "0", "Native output has no stable identity");
        linked.require_matches(destination)?;
        if let Some(error) = saved.cleanup_error {
            eprintln!("{error}");
        }
        Ok(
            json!({"file":destination,"bytes":saved.size_bytes,"sha256":saved.sha256,"dev":dev,"ino":ino}),
        )
    })();
    result.map_err(|error: anyhow::Error| {
        if error.chain().any(|e| {
            e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
        }) {
            anyhow::anyhow!("The answer output already exists; no answer was written.")
        } else {
            anyhow::anyhow!("Unable to create the answer output.")
        }
    })
}

async fn claim(page: &Cdp, claim_id: &Value, cancel: &CancellationToken) -> Result<()> {
    let expression = format!(
        "(cfg)=>{{ /* alcuin-dom:native-reply-claim */ {} if(window!==window.top)return false;try{{const claim=readOracleTargetClaim();if(claim&&claim.id!==cfg.claimId||claim?.retiring||claim?.submission||claim?.inspection||!oracleOwnershipId(cfg.claimId))return false;writeOracleTargetClaim({{...claim,id:cfg.claimId,retiring:false}});return true;}}catch{{return false;}}}}",
        include_str!("../browser-scripts/ownership-prelude.js")
    );
    ensure!(
        page.evaluate_with_arguments(
            &expression,
            &json!([{"claimId":claim_id}]),
            Duration::from_secs(3),
            cancel
        )
        .await?
            == true,
        "Native recovery ownership is unverified"
    );
    Ok(())
}

pub(crate) async fn recover(
    store: &SessionStore,
    environment: &Environment,
    cwd: &Path,
    request: &Path,
    output: &Path,
    cancel: &CancellationToken,
) -> Result<usize> {
    let request_path = crate::prompts::absolute(cwd, &request.to_string_lossy());
    let output_path = crate::prompts::absolute(cwd, &output.to_string_lossy());
    ensure!(
        request_path.to_string_lossy().to_lowercase()
            != output_path.to_string_lossy().to_lowercase(),
        "Request and answer paths must differ."
    );
    let request = read_request(&request_path)?;
    check(cancel)?;
    let explicit = environment
        .get("ORACLE_BROWSER_PROFILE_DIR")
        .map(String::as_str)
        .filter(|dir| !policy::trim(dir).is_empty());
    let home = environment.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    ensure!(
        explicit.is_some() || home.is_some(),
        "Unable to inspect Alcuin's Chrome endpoint."
    );
    let profile = crate::config::manual_login_profile_dir(
        [explicit],
        environment,
        home.map(String::as_str).unwrap_or_default(),
    )?;
    let endpoint = match read_devtools_endpoint(&profile) {
        Ok(endpoint) => endpoint,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            None
        }
        Err(_) => bail!("Unable to inspect Alcuin's Chrome endpoint."),
    }
    .context("Open Alcuin's manual-login Chrome before recovering a native reply.")?;
    // While this reply is connected, its marker on the Chrome profile refuses idle
    // cleanup, as a borrowed run's does. Its own admission lease lives elsewhere.
    let marker = match ProfileRegistry::open_existing(&profile) {
        Ok(registry) => tokio::task::spawn_blocking(move || {
            registry.mark_attachment(std::time::Instant::now() + Duration::from_secs(5))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|marker| marker),
        Err(error) => Err(error),
    };
    if let Err(error) = &marker {
        eprintln!("Native reply attachment marker was not recorded: {error:#}");
    }
    let browser = endpoint
        .connect(COMMAND, cancel)
        .await
        .map_err(|_| anyhow::anyhow!("Unable to inspect Alcuin's Chrome tabs."))?;
    let result =
        recover_connected(store, &endpoint, &browser, &request, &output_path, cancel).await;
    let cleanup = browser
        .disconnect_and_wait(Duration::from_millis(500))
        .await;
    drop(marker);
    if result.is_ok() {
        // A retired reply tab may have been the last page holding a hinted idle launch open.
        crate::browser_runner::settle_profile_idle(store, &profile, None, true).await;
    }
    match (result, cleanup) {
        (Ok(count), Ok(())) => Ok(count),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => {
            Err(error.context("Native reply saved; browser disconnection failed"))
        }
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Browser disconnection also failed: {cleanup:#}")))
        }
    }
}

async fn recover_connected(
    store: &SessionStore,
    endpoint: &AttachmentEndpoint,
    browser: &Cdp,
    request: &Request,
    output_path: &Path,
    cancel: &CancellationToken,
) -> Result<usize> {
    let tabs = browser_tabs::targets(browser, COMMAND, cancel)
        .await
        .map_err(|_| anyhow::anyhow!("Unable to inspect Alcuin's Chrome tabs."))?;
    let matching: Vec<_> = tabs
        .into_iter()
        .filter(|tab| {
            tab["url"]
                .as_str()
                .is_some_and(|url| matches_url(url, &request.chat_id))
        })
        .collect();
    ensure!(
        matching.len() <= 1,
        "More than one tab matches the request; no answer was written."
    );
    let journal = matching
        .is_empty()
        .then(|| TargetJournal::open(store))
        .transpose()?;
    let mut owned = None;
    let mut creator = None;
    let mut heartbeat = None;
    let target = if let Some(tab) = matching.into_iter().next() {
        tab
    } else {
        let journal = journal
            .as_ref()
            .context("Missing native creation journal")?;
        let admission = store
            .home()
            .join("native-reply-admission")
            .join(artifacts::sha256(endpoint.browser_ws_endpoint.as_bytes()));
        let registry = ProfileRegistry::open(&admission)?;
        let admission_id = format!("native-reply-{}", uuid::Uuid::new_v4());
        let lease = Arc::new(registry.acquire(&admission_id, COMMAND, cancel).await?);
        heartbeat = Some(lease.heartbeat());
        lease.update(&json!({"chromeHost":endpoint.host,"chromePort":endpoint.port}))?;
        let allocation = TargetAcquisition::default();
        let target = allocation
            .acquire(browser.clone(), lease.clone(), cancel, COMMAND)
            .await?;
        let creation = target
            .creation()
            .context("Native reply target has no actual creation evidence")?;
        lease.update(&json!({"targetDispositionAllocationId":creation.allocation_id()}))?;
        let identity = json!({"host":endpoint.host,"port":endpoint.port,"targetId":target.id(),"browserWSEndpoint":endpoint.browser_ws_endpoint,"claimId":uuid::Uuid::new_v4().to_string()});
        creator = Some(journal.record_creation(creation, &identity, &ProcessIdentity::current()?, None, Some(json!({"profileDir":crate::native_fs::canonical_wire_path(&admission)?,"id":lease.id(),"admissionId":admission_id})))?);
        target.require(COMMAND, cancel).await?;
        target
            .page()
            .call("Page.enable", json!({}), COMMAND, cancel)
            .await?;
        let url = format!("https://chatgpt.com/c/{}", request.chat_id);
        let navigation = target
            .page()
            .call("Page.navigate", json!({"url":url}), COMMAND, cancel)
            .await?;
        ensure!(
            navigation.get("errorText").is_none(),
            "Unable to open the saved conversation in a separate Chrome tab."
        );
        let observed = json!({"targetId":target.id(),"url":url});
        owned = Some(target);
        observed
    };
    let capture = harvest(
        endpoint,
        &target,
        owned.is_some(),
        cancel,
        Duration::from_secs(30),
    )
    .await?;
    let answer = validate_reply(request, &capture)?;
    if let Some(owner) = &creator {
        let journal = journal
            .as_ref()
            .context("Missing native creation journal")?;
        let target = owned
            .as_ref()
            .context("Native creator lost its allocation")?;
        if claim(target.page(), &owner["target"]["claimId"], cancel)
            .await
            .is_err()
        {
            check(cancel)?;
            journal.record_disposition(owner, "unknown", "creator-unverified")?;
            creator = None;
            eprintln!(
                "Native recovery target ownership is unverified; its target will be retained."
            );
        }
    }
    let output = output_receipt(answer, output_path, cancel)?;
    check(cancel)?;
    if let Some(creator) = creator {
        let journal = journal
            .as_ref()
            .context("Missing native creation journal")?;
        let finalise = async {
            let user_id = capture["lastUserMessageId"].as_str().filter(|s| !s.is_empty()).context("Native completion identity is incomplete.")?;
            let assistant_id = capture["lastAssistantMessageId"].as_str().filter(|s| !s.is_empty()).or_else(|| capture["lastAssistantTurnId"].as_str()).filter(|s| !s.is_empty()).context("Native completion identity is incomplete.")?;
            let commit = journal.commit(&creator, &json!({"version":1,"kind":"native","allocationId":creator["allocationId"],"claimId":creator["target"]["claimId"],"conversationId":request.chat_id,"userMessageId":user_id,"assistantMessageId":assistant_id,"userText":capture["lastUserText"],"output":output,"committedAt":timestamp()}))?;
            journal.finalise(&commit)?;
            let disposed = crate::browser_retirement::retire_committed(store, journal, browser, endpoint, &creator).await?;
            if disposed["absence"] != "confirmed-absent" { eprintln!("Native reply saved; browser target retained: {}", disposed["reason"].as_str().unwrap_or("unconfirmed")); }
            Ok::<(), anyhow::Error>(())
        }.await;
        if finalise.is_err() {
            let _ = journal.record_disposition(&creator, "unknown", "native-commit-unconfirmed");
            eprintln!(
                "Native reply saved; browser target retained because its commitment is unconfirmed."
            );
        }
    }
    drop(heartbeat);
    Ok(answer.encode_utf16().count())
}

#[cfg(test)]
#[path = "native_reply_tests.rs"]
mod tests;

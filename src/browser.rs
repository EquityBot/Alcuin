//! Browser evidence gates. Production browser dispatch remains disabled.
use crate::{
    artifacts::sha256,
    policy::{self, ModelEvidence, ThinkingEvidence},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub fn normalise_host(host: &str) -> String {
    let host = host
        .trim()
        .to_lowercase()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    if host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        "loopback".into()
    } else {
        host
    }
}
pub fn stable_conversation_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
pub fn provider_url(raw: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url)
}
pub fn conversation_id(value: &str) -> Option<String> {
    let url = provider_url(value)?;
    let segments: Vec<_> = url
        .path()
        .trim_end_matches('/')
        .split('/')
        .skip(1)
        .collect();
    // Maintained recovery also accepts project and custom-GPT conversation routes.
    let index = segments.iter().position(|s| *s == "c")?;
    if index + 2 != segments.len() || !stable_conversation_id(segments[index + 1]) {
        return None;
    }
    if index != 0
        && !(segments[0] == "g"
            && index >= 2
            && segments[1].starts_with("g-")
            && stable_conversation_id(segments[1])
            && (index == 2 || index == 3 && segments[2] == "project"))
    {
        return None;
    }
    Some(segments[index + 1].into())
}
pub(crate) fn provider_identity(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 256
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
    })
}
pub(crate) fn assistant_identity(observed: &Value) -> Option<&Value> {
    let identity = observed
        .get("lastAssistantMessageId")
        .filter(|id| !id.is_null() && id.as_str() != Some(""))
        .unwrap_or(&observed["lastAssistantTurnId"]);
    provider_identity(identity).then_some(identity)
}
pub fn prompt_fingerprint(source: &str, message_id: &str) -> String {
    sha256(
        serde_json::to_string(&json!([
            message_id,
            source.replace("\r\n", "\n").replace('\r', "\n")
        ]))
        .expect("serialisable prompt fingerprint")
        .as_bytes(),
    )
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryTarget {
    pub host: String,
    pub port: u16,
    pub target_id: String,
    #[serde(rename = "browserWSEndpoint", alias = "browserWsEndpoint")]
    pub browser_ws_endpoint: Option<String>,
    pub claim_id: String,
    pub conversation_id: String,
}
pub fn exact_owned_target(metadata: &Value, target: &RecoveryTarget) -> bool {
    let runtime = &metadata["browser"]["runtime"];
    let owned = &runtime["ownedRecoveryTarget"];
    let Some(conversation) = runtime["tabUrl"].as_str().and_then(conversation_id) else {
        return false;
    };
    if runtime
        .get("conversationId")
        .is_some_and(|v| v.as_str() != Some(&conversation))
        || owned
            .get("conversationId")
            .is_some_and(|v| v.as_str() != Some(&conversation))
    {
        return false;
    }
    stable_conversation_id(&target.conversation_id)
        && provider_identity(&json!(target.target_id))
        && provider_identity(&json!(target.claim_id))
        && target.port != 0
        && owned["claimId"] == target.claim_id
        && owned["targetId"] == target.target_id
        && owned["port"].as_u64() == Some(u64::from(target.port))
        && owned["host"]
            .as_str()
            .is_some_and(|h| normalise_host(h) == normalise_host(&target.host))
        && owned["browserWSEndpoint"].as_str() == target.browser_ws_endpoint.as_deref()
        && conversation == target.conversation_id
}
pub(crate) fn require_original_user_source(metadata: &Value, observed: &Value) -> Result<()> {
    let runtime = &metadata["browser"]["runtime"];
    let expected_id = runtime["conversationId"]
        .as_str()
        .or_else(|| runtime["ownedRecoveryTarget"]["conversationId"].as_str())
        .context_pair()?;
    anyhow::ensure!(
        stable_conversation_id(expected_id),
        "Invalid saved provider conversation identity"
    );
    let actual_url = observed["currentUrl"]
        .as_str()
        .or_else(|| observed["url"].as_str())
        .context_pair()?;
    let actual = conversation_id(actual_url).context_pair()?;
    let origin = provider_url(actual_url)
        .context_pair()?
        .origin()
        .ascii_serialization();
    anyhow::ensure!(
        actual == expected_id,
        "Provider origin or conversation identity changed"
    );
    if let Some(url) = runtime["tabUrl"].as_str() {
        anyhow::ensure!(
            conversation_id(url).as_deref() == Some(expected_id)
                && provider_url(url)
                    .is_some_and(|url| url.origin().ascii_serialization() == origin),
            "Saved provider origin or conversation identity is invalid"
        );
    }
    if let Some(expected_origin) = observed.get("origin") {
        anyhow::ensure!(
            expected_origin.as_str() == Some(&origin),
            "Provider origin is invalid"
        );
    }
    if let Some(owned) = runtime["ownedRecoveryTarget"].get("conversationId") {
        anyhow::ensure!(
            owned == expected_id,
            "Owned provider conversation identity conflicts with saved runtime"
        );
    }
    if observed["conversationId"] != expected_id
        || !provider_identity(&observed["lastUserMessageId"])
    {
        bail!("Saved request and provider message/turn identity do not match.");
    }
    let message_id = observed["lastUserMessageId"].as_str().context_pair()?;
    let source = if runtime["submittedPromptHashFormat"] == "original-source-v1" {
        if observed["lastUserOriginalSourceMessageId"] != message_id {
            bail!("Original source belongs to a different provider message.");
        }
        observed["lastUserOriginalSourceText"]
            .as_str()
            .context_pair()?
    } else if runtime.get("submittedPromptHashFormat").is_some() {
        bail!("Unsupported saved prompt fingerprint format.");
    } else {
        observed["lastUserTextRaw"].as_str().context_pair()?
    };
    if runtime["submittedPromptHash"] != prompt_fingerprint(source, message_id) {
        bail!("Complete original prompt source does not match the committed request.");
    }
    Ok(())
}
pub fn require_original_source_pair(metadata: &Value, observed: &Value) -> Result<()> {
    let assistant_identity = assistant_identity(observed).context_pair()?;
    require_original_user_source(metadata, observed)?;
    if observed["assistantFollowsLatestUser"] != true
        || &observed["lastUserMessageId"] == assistant_identity
    {
        bail!("Saved request and provider message/turn identity do not match.");
    }
    let user = observed["lastUserTurnIndex"].as_u64().context_pair()?;
    let assistant = observed["lastAssistantTurnIndex"].as_u64().context_pair()?;
    let shared_exchange = assistant == user
        && observed["sameTurnExchange"] == true
        && provider_identity(&observed["lastUserTurnId"])
        && observed["lastUserTurnId"] == observed["lastAssistantTurnId"];
    // Current maintained markup puts user and assistant units in one exchange. Equal indices
    // require positive DOM order, exact shared exchange identity and the complete source above.
    if assistant < user
        || assistant == user && !shared_exchange
        || !provider_identity(&observed["lastAssistantTurnId"])
    {
        bail!("Assistant reply is not paired with the committed provider turn.");
    }
    Ok(())
}
trait PairContext<T> {
    fn context_pair(self) -> Result<T>;
}
impl<T> PairContext<T> for Option<T> {
    fn context_pair(self) -> Result<T> {
        self.ok_or_else(|| {
            anyhow::anyhow!("Provider source or turn evidence is unavailable; refusing result.")
        })
    }
}
pub fn accept_answer(metadata: &Value, observed: &Value, result: &Value) -> Result<()> {
    if observed["kind"] == "deep-research-report" {
        crate::browser_research::require_report(metadata, observed)?;
    } else {
        require_original_source_pair(metadata, observed)?;
    }
    let model = serde_json::from_value::<ModelEvidence>(result["modelSelection"].clone()).ok();
    let thinking =
        serde_json::from_value::<ThinkingEvidence>(result["thinkingSelection"].clone()).ok();
    policy::assert_browser_completion(
        model.as_ref(),
        thinking.as_ref(),
        result["effectiveModel"].as_str(),
    )?;
    anyhow::ensure!(
        observed["completionConfirmed"] == true
            && observed["stopExists"] == false
            && observed["strongThinkingActive"] == false,
        "Positive provider completion is required before answer publication."
    );
    // Reference: an unloaded notice is answerless even beside a generated image.
    if observed["unloaded"] == true || observed["text"].as_str().is_some_and(response_load_label) {
        return Err(crate::browser_provider::Failure::ResponseLoadFailed.into());
    }
    if observed["withheld"] == true {
        let notice = observed["text"]
            .as_str()
            .unwrap_or("")
            .split(crate::policy::javascript_whitespace)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let notice = notice.encode_utf16().take(300).collect::<Vec<_>>();
        return Err(
            crate::browser_provider::Failure::ContentWithheld(String::from_utf16_lossy(&notice))
                .into(),
        );
    }
    Ok(())
}

fn response_load_label(text: &str) -> bool {
    let normalised = text
        .to_lowercase()
        .split(crate::policy::javascript_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let label = normalised.strip_suffix(" retry").unwrap_or(&normalised);
    let label = label.strip_suffix(['.', '!']).unwrap_or(label);
    matches!(
        label,
        "this response couldn't load" | "this response couldn’t load"
    )
}
pub fn image_only_ui_text(text: &str) -> bool {
    let normalised = text
        .to_lowercase()
        .split(crate::policy::javascript_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if matches!(
        normalised.as_str(),
        "" | "edit" | "stopped thinking" | "stopped thinking edit"
    ) {
        return true;
    }
    let text = normalised
        .strip_prefix("reasoning ")
        .or_else(|| normalised.strip_prefix("pro thinking "))
        .unwrap_or(&normalised);
    let Some(duration) = text
        .strip_prefix("thought for ")
        .and_then(|s| s.strip_suffix(" edit"))
    else {
        return false;
    };
    let end = duration
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(duration.len());
    let number = &duration[..end];
    let numeric = match number.split_once('.') {
        Some((whole, fraction)) => {
            !whole.is_empty()
                && !fraction.is_empty()
                && whole.bytes().all(|b| b.is_ascii_digit())
                && fraction.bytes().all(|b| b.is_ascii_digit())
        }
        None => !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
    };
    numeric
        && matches!(
            duration[end..].trim_start(),
            "s" | "sec"
                | "secs"
                | "second"
                | "seconds"
                | "m"
                | "min"
                | "mins"
                | "minute"
                | "minutes"
                | "h"
                | "hr"
                | "hrs"
                | "hour"
                | "hours"
        )
}
pub fn normalise_image_answer(text: &str, image_count: usize) -> String {
    if image_count > 0 && image_only_ui_text(text) {
        if image_count == 1 {
            "Generated image.".into()
        } else {
            format!("Generated {image_count} images.")
        }
    } else {
        text.into()
    }
}
#[cfg(test)]
mod image_text_tests {
    use super::*;
    #[test]
    fn owned_images_replace_only_reference_ui_chrome() {
        for text in [
            "",
            " Edit ",
            "Stopped thinking",
            "Stopped thinking\nEdit",
            "Reasoning thought for 1.5 seconds Edit",
            "Pro thinking thought for 3min Edit",
            "thought for 0 h edit",
            "\u{feff}edit\u{a0}",
        ] {
            assert!(image_only_ui_text(text), "{text:?}");
            assert_eq!(normalise_image_answer(text, 1), "Generated image.");
            assert_eq!(normalise_image_answer(text, 2), "Generated 2 images.");
            assert_eq!(normalise_image_answer(text, 0), text);
        }
        for text in [
            "PUBLIC drawing explanation",
            "Edit this image",
            "thought for .5 sec edit",
            "thought for 5. sec edit",
            "thought for 5 seconds",
            "thought for 5 seconds edit extra",
            "thought for 5e2 seconds edit",
            "\u{85}edit",
        ] {
            assert!(!image_only_ui_text(text), "{text:?}");
            assert_eq!(normalise_image_answer(text, 2), text);
        }
    }
}
#[derive(Clone, Debug)]
pub struct TerminalSample {
    pub now_ms: u64,
    pub length: usize,
    pub content_key: String,
    pub stop_visible: bool,
    pub bar_visible: bool,
    pub strong_thinking_active: bool,
}
#[derive(Clone, Debug, Default)]
pub struct TerminalGate {
    last_key: String,
    last_change_ms: u64,
    bar_stable_cycles: u32,
    seen: bool,
}
impl TerminalGate {
    pub fn observe(
        &mut self,
        sample: &TerminalSample,
        confirm_cycles: u32,
        min_stable_ms: u64,
    ) -> bool {
        let changed = !self.seen || sample.content_key != self.last_key;
        if changed {
            self.last_change_ms = sample.now_ms;
        }
        self.bar_stable_cycles = if sample.bar_visible
            && !sample.stop_visible
            && !sample.strong_thinking_active
            && !changed
        {
            self.bar_stable_cycles + 1
        } else {
            0
        };
        self.last_key = sample.content_key.clone();
        self.seen = true;
        !sample.stop_visible
            && sample.length > 0
            && sample.bar_visible
            && !sample.strong_thinking_active
            && self.bar_stable_cycles >= confirm_cycles
            && sample.now_ms.saturating_sub(self.last_change_ms) >= min_stable_ms
    }
}
/// What a DevTools client disconnect meant, read from the reference liveness probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectCause {
    CdpClientDisconnect,
    TargetClosed,
    LivenessUnknown,
    ChromeClosed,
}
impl DisconnectCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CdpClientDisconnect => "cdp-client-disconnect",
            Self::TargetClosed => "target-closed",
            Self::LivenessUnknown => "liveness-unknown",
            Self::ChromeClosed => "chrome-closed",
        }
    }
    /// Reference connectionLostUserMessage.
    pub fn user_message(self, remote: bool) -> String {
        let place = if remote { "Remote Chrome" } else { "Chrome" };
        match self {
            Self::CdpClientDisconnect => format!("{place} DevTools client disconnected before Industrious finished; the browser target appears still alive."),
            Self::TargetClosed => format!("{place} closed the ChatGPT tab before Industrious finished; Chrome itself is still running."),
            Self::LivenessUnknown => format!("{place} DevTools client disconnected before Industrious finished and Chrome's state could not be confirmed; the window may still be open."),
            Self::ChromeClosed if remote => "Remote Chrome connection lost before Industrious finished.".into(),
            Self::ChromeClosed => "Chrome window closed before Industrious finished. Please keep it open until completion.".into(),
        }
    }
}
#[cfg(test)]
tokio::task_local! {
    /// Fake discovery base for tests. Unset, the probe stays off every real DevTools port.
    pub(crate) static TEST_DISCOVERY: String;
}
/// Reference probeChromeTargetLiveness and classifyChromeDisconnect: two bounded `/json/version`
/// attempts, then the exact target in `/json/list`. It reads only DevTools discovery endpoints.
pub async fn probe_disconnect(
    host: &str,
    port: u16,
    target_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> DisconnectCause {
    let timeout = std::time::Duration::from_millis(1500);
    let base = crate::browser_chrome::endpoint_base(host, port);
    #[cfg(test)]
    let base = base.and_then(|_| {
        TEST_DISCOVERY
            .try_with(Clone::clone)
            .map_err(|_| anyhow::anyhow!("Test discovery is unset"))
    });
    let Ok(base) = base else {
        return DisconnectCause::ChromeClosed;
    };
    let mut reachable = false;
    for attempt in 0..2 {
        if crate::cdp::endpoint_json(&base, "/json/version", timeout, cancel)
            .await
            .is_ok()
        {
            reachable = true;
            break;
        }
        if attempt == 0 {
            tokio::select! {_=cancel.cancelled()=>break,_=tokio::time::sleep(std::time::Duration::from_millis(500))=>{}}
        }
    }
    if !reachable {
        return DisconnectCause::ChromeClosed;
    }
    match crate::cdp::endpoint_json(&base, "/json/list", timeout, cancel).await {
        Ok(targets) => {
            let found = targets.as_array().is_some_and(|targets| {
                targets.iter().any(|target| {
                    target["targetId"].as_str().or(target["id"].as_str()) == Some(target_id)
                })
            });
            if found {
                DisconnectCause::CdpClientDisconnect
            } else {
                DisconnectCause::TargetClosed
            }
        }
        // The endpoint answered; an unreadable target list fails closed.
        Err(_) => DisconnectCause::LivenessUnknown,
    }
}
#[cfg(test)]
mod disconnect_tests {
    use super::DisconnectCause::*;
    #[test]
    fn remote_disconnects_use_the_reference_wording() {
        for (cause, remote, message) in [
            (
                CdpClientDisconnect,
                true,
                "Remote Chrome DevTools client disconnected before Industrious finished; the browser target appears still alive.",
            ),
            (
                TargetClosed,
                true,
                "Remote Chrome closed the ChatGPT tab before Industrious finished; Chrome itself is still running.",
            ),
            (
                LivenessUnknown,
                true,
                "Remote Chrome DevTools client disconnected before Industrious finished and Chrome's state could not be confirmed; the window may still be open.",
            ),
            (
                LivenessUnknown,
                false,
                "Chrome DevTools client disconnected before Industrious finished and Chrome's state could not be confirmed; the window may still be open.",
            ),
            (
                ChromeClosed,
                true,
                "Remote Chrome connection lost before Industrious finished.",
            ),
        ] {
            assert_eq!(cause.user_message(remote), message);
        }
    }
}

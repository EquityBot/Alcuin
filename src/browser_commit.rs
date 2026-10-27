//! Read-only original-source commitment. These helpers do not acquire targets or send prompts.
use crate::{
    artifacts::sha256,
    browser::{conversation_id, prompt_fingerprint, provider_url, stable_conversation_id},
    cdp::Cdp,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use tokio_util::sync::CancellationToken;
const PROBE: &str = include_str!("../browser-scripts/owned_commit.js");
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionOwner {
    pub claim_id: String,
    pub reservation_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    id: String,
    turn_id: String,
    index: u64,
    #[serde(default)]
    matches: Option<bool>,
    #[serde(default)]
    fingerprint_text: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Probe {
    current_url: String,
    work_selected: bool,
    mode_unverified: bool,
    invalid: bool,
    turn_ids: Vec<String>,
    messages: Vec<Message>,
    canonical_prompt: String,
    #[serde(default)]
    submission_witness: Option<SubmissionWitness>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmissionIntent {
    pub witness_id: String,
    pub dispatch_guard_id: String,
    pub prompt_sha256: String,
    /// Set only after Alcuin verified its own Web search selection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub web_search: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionWitness {
    version: u8,
    owner: SubmissionOwner,
    witness_id: String,
    dispatch_guard_id: String,
    message_id: String,
    prompt_sha256: String,
    source: String,
    request_path: String,
    history_disabled: bool,
}
const WITNESS_READ: &str = r#"(cfg) => {
    const record=window.__oracleSubmissionWitness;
    if(!record || record.witnessId!==cfg.intent.witnessId || record.dispatchGuardId!==cfg.intent.dispatchGuardId ||
        record.owner.claimId!==cfg.owner.claimId || record.owner.reservationId!==cfg.owner.reservationId) return {state:'unavailable'};
    if(typeof record.read==='function') return record.read();
    const current=location.pathname.match(/^\/c\/([a-zA-Z0-9-]+)$/)?.[1] ?? null;
    if(record.document!==document || window!==window.top || location.origin!=='https://chatgpt.com' ||
        record.conversationId && current!==record.conversationId) return {state:'invalid'};
    if(record.result.state==='invalid')return record.result;
    if(oracleTemporaryChat()!==true)return {state:'pending'};
    if(record.result.state==='pending' && record.candidate)
        return {state:'captured',witness:record.candidate};
    return record.result;
}"#;
/// DOM source and the final live witness state are read in one renderer task. A
/// conflicting request during source inspection must invalidate this observation.
pub(crate) fn receipt_probe() -> String {
    crate::browser_privacy::with_predicate(&format!(
        "(cfg) => {{ /* alcuin-dom:submission-receipt */
            const readSubmission=()=>{{
                const privacy=oracleTemporaryChat();
                if(privacy===false)return {{state:'invalid'}};
                if(cfg.saved){{
                    const proof=window.__oracleSubmissionDocument;
                    if(!proof || proof.document!==document || window!==window.top ||
                        proof.owner.claimId!==cfg.owner.claimId || proof.owner.reservationId!==cfg.owner.reservationId ||
                        proof.witnessId!==cfg.intent?.witnessId || proof.dispatchGuardId!==cfg.intent?.dispatchGuardId)
                        return {{state:'invalid'}};
                    return privacy===true ? {{state:'captured',witness:cfg.saved}} : {{state:'pending'}};
                }}
                if(cfg.intent)return ({WITNESS_READ})(cfg);
                return {{state:privacy===true?'none':'pending'}};
            }};
            if(oracleTemporaryChat()!==true)return {{probe:null,submission:readSubmission()}};
            const probe=({PROBE})(cfg.owner,cfg.ids,cfg.prompt,cfg.intent?.webSearch===true);
            return {{probe,submission:readSubmission()}};
        }}"
    ))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Navigation {
    origin: String,
    conversation_id: Option<String>,
    landing_path: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitSnapshot {
    owner: SubmissionOwner,
    navigation: Navigation,
    message_ids: Vec<String>,
    turn_ids: Vec<String>,
    canonical_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submission_intent: Option<SubmissionIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submission_witness: Option<SubmissionWitness>,
}
impl CommitSnapshot {
    pub(crate) fn require_receipt(&self, receipt: &CommitReceipt) -> Result<()> {
        anyhow::ensure!(
            self.owner == receipt.owner
                && receipt.version == 1
                && receipt.kind == "new-user-message"
                && id(&receipt.message_id)
                && id(&receipt.turn_id)
                && receipt.message_fingerprint_format == "original-source-v1"
                && receipt.prompt_text_fingerprint == sha256(self.canonical_prompt.as_bytes())
                && receipt.conversation.origin == self.navigation.origin
                && self
                    .navigation
                    .conversation_id
                    .as_ref()
                    .is_none_or(|id| *id == receipt.conversation.conversation_id)
                && !self.message_ids.contains(&receipt.message_id)
                && !self.turn_ids.contains(&receipt.turn_id)
                && receipt.turns_count > self.turn_ids.len(),
            "Recovered receipt differs from the original pre-send snapshot"
        );
        if self.navigation.conversation_id.is_none() || self.submission_intent.is_some() {
            let intent = self
                .submission_intent
                .as_ref()
                .context("Original submission intent missing")?;
            let witness = receipt
                .submission_witness
                .as_ref()
                .context("Recovered submission witness missing")?;
            anyhow::ensure!(
                witness.version == 1
                    && witness.owner == self.owner
                    && witness.witness_id == intent.witness_id
                    && witness.dispatch_guard_id == intent.dispatch_guard_id
                    && witness.prompt_sha256 == intent.prompt_sha256
                    && witness.message_id == receipt.message_id
                    && witness.source == "chatgpt-fetch-v1"
                    && witness.request_path == "/backend-api/f/conversation"
                    && witness.history_disabled
                    && self
                        .submission_witness
                        .as_ref()
                        .is_none_or(|saved| saved == witness),
                "Recovered receipt differs from the original outbound submission witness"
            );
        }
        Ok(())
    }
    pub(crate) fn bind_submission(&mut self, intent: SubmissionIntent) -> Result<()> {
        anyhow::ensure!(
            self.submission_intent.is_none()
                && id(&intent.witness_id)
                && id(&intent.dispatch_guard_id)
                && intent.prompt_sha256.len() == 64
                && intent.prompt_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Submission witness intent cannot be replaced"
        );
        self.submission_intent = Some(intent);
        Ok(())
    }
    pub(crate) fn seal_witness(&mut self, receipt: &CommitReceipt) -> Result<()> {
        anyhow::ensure!(
            self.owner == receipt.owner && self.submission_witness.is_none(),
            "Submission witness owner changed"
        );
        self.submission_witness = receipt.submission_witness.clone();
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitReceipt {
    #[serde(flatten)]
    pub owner: SubmissionOwner,
    pub version: u8,
    pub kind: String,
    pub message_id: String,
    pub turn_id: String,
    pub message_fingerprint: String,
    pub message_fingerprint_format: String,
    pub prompt_text_fingerprint: String,
    pub conversation: Conversation,
    pub turns_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_witness: Option<SubmissionWitness>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub origin: String,
    pub conversation_id: String,
}
#[derive(Debug)]
pub enum CommitObservation {
    Pending,
    Ambiguous(&'static str),
    Committed(Box<CommitReceipt>),
}
fn id(value: &str) -> bool {
    !value.is_empty() && value.chars().map(char::len_utf16).sum::<usize>() <= 256
}
/// JavaScript `\s`, which the reference probe's link pattern uses.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}
fn unescape_punctuation(text: &[char]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < text.len() {
        if text[index] == '\\' && text.get(index + 1).is_some_and(char::is_ascii_punctuation) {
            index += 1;
        }
        out.push(text[index]);
        index += 1;
    }
    out
}
/// End of an `https?:` link part starting at `start` and closed by `close`, else None.
fn link_part(value: &[char], start: usize, excluded: &[char], close: char) -> Option<usize> {
    let mut index = start;
    for c in ['h', 't', 't', 'p'] {
        (value.get(index) == Some(&c)).then_some(())?;
        index += 1;
    }
    if value.get(index) == Some(&'s') {
        index += 1;
    }
    (value.get(index) == Some(&':')).then_some(())?;
    index += 1;
    // Every backslash starts a two-character escape, so the greedy scan is the only parse.
    loop {
        match value.get(index) {
            Some('\\') if value.get(index + 1).is_some_and(|c| !js_space(*c)) => index += 2,
            Some(c) if *c != '\\' && !excluded.contains(c) && !js_space(*c) => index += 1,
            _ => break,
        }
    }
    (value.get(index) == Some(&close)).then_some(index)
}
fn autolink_end(value: &[char], mut index: usize) -> bool {
    loop {
        match value.get(index).copied() {
            None => return true,
            Some(c) if js_space(c) || ")]}'\"".contains(c) => return true,
            Some('\\') if value.get(index + 1).is_some_and(char::is_ascii_punctuation) => {
                index += 1;
            }
            Some('.' | ',' | ';' | ':' | '!' | '?') => index += 1,
            _ => return false,
        }
    }
}
fn complete_literal_link(text: &str) -> bool {
    text.match_indices('[').any(|(index, _)| {
        if text[..index].ends_with('!') {
            return false;
        }
        let rest = &text[index + 1..];
        let Some(label_end) = rest.find(['[', ']', '\r', '\n']) else {
            return false;
        };
        if label_end == 0 || !rest[label_end..].starts_with("](") {
            return false;
        }
        let tail = &rest[label_end + 2..];
        tail.find(|c| js_space(c) || c == '(' || c == ')')
            .is_some_and(|end| end > 0 && tail[end..].starts_with(')'))
    })
}
/// Rust check of the reference `markdownSourceText` (46c757d): ChatGPT stores a message with an
/// autolinked URL or link syntax as escaped Markdown. The source decodes only with link evidence
/// and no unescaped Markdown syntax outside a restored URL.
/// shortcut: a backslash typed before punctuation in a link-bearing prompt is indistinguishable
/// from an added escape; settling that needs the provider's encoding mode, which is not exposed.
fn markdown_source_text(value: &str) -> Option<String> {
    let value = value.chars().collect::<Vec<_>>();
    let mut text = String::new();
    let mut evidence = 0;
    let mut escaped_link_opener = false;
    let mut index = 0;
    while index < value.len() {
        let previous = index.checked_sub(1).map(|i| value[i]);
        let link = (value[index] == '[')
            .then(|| link_part(&value, index + 1, &['[', ']'], ']'))
            .flatten()
            .filter(|label| value.get(label + 1) == Some(&'('))
            .and_then(|label| {
                link_part(&value, label + 2, &['[', ']', '(', ')'], ')').map(|end| (label, end))
            });
        if let Some((label, end)) = link {
            let destination = unescape_punctuation(&value[label + 2..end]);
            if unescape_punctuation(&value[index + 1..label]) == destination
                && previous.is_none_or(|c| js_space(c) || "([{\"'".contains(c))
                && autolink_end(&value, end + 1)
                && (destination.starts_with("https://") || destination.starts_with("http://"))
                && reqwest::Url::parse(&destination).is_ok_and(|url| url.has_host())
            {
                text.push_str(&destination);
                evidence += 1;
                index = end + 1;
                continue;
            }
        }
        let next = value.get(index + 1).copied();
        if value[index] == '\\' && next.is_some_and(|c| c.is_ascii_punctuation()) {
            if next == Some('(') && previous == Some(']') {
                escaped_link_opener = true;
            }
            text.extend(next);
            index += 2;
        } else if matches!(value[index], '`' | '*' | '<')
            || value[index] == ']' && next == Some('(')
        {
            return None;
        } else {
            text.push(value[index]);
            index += 1;
        }
    }
    (evidence > 0 || escaped_link_opener && complete_literal_link(&text)).then_some(text)
}
fn navigation(raw: &str) -> Option<Navigation> {
    let url = provider_url(raw)?;
    if url.host_str() != Some("chatgpt.com") {
        return None;
    }
    let conversation = conversation_id(raw);
    let landing = if conversation.is_none() {
        Some(url.path().trim_end_matches('/').to_owned())
    } else {
        None
    };
    Some(Navigation {
        origin: "https://chatgpt.com".into(),
        conversation_id: conversation,
        landing_path: landing.map(|p| if p.is_empty() { "/".into() } else { p }),
    })
}
fn parse(value: Value) -> Result<Probe> {
    let probe: Probe = serde_json::from_value(value)
        .context("Provider commit probe has incomplete or malformed evidence")?;
    let mut turn_ids = BTreeSet::new();
    let mut message_ids = BTreeSet::new();
    anyhow::ensure!(
        probe.turn_ids.iter().all(|s| id(s) && turn_ids.insert(s)),
        "Provider turn identities are invalid or duplicated"
    );
    anyhow::ensure!(
        probe.messages.iter().all(|m| id(&m.id)
            && id(&m.turn_id)
            && m.index <= 9007199254740991
            && probe.turn_ids.get(m.index as usize) == Some(&m.turn_id)
            && message_ids.insert(&m.id)),
        "Provider message and turn identities conflict"
    );
    Ok(probe)
}
fn snapshot(owner: &SubmissionOwner, value: Value) -> Result<CommitSnapshot> {
    anyhow::ensure!(
        id(&owner.claim_id) && id(&owner.reservation_id),
        "Invalid submission owner identity"
    );
    let probe = parse(value)?;
    let navigation =
        navigation(&probe.current_url).context("Provider origin or navigation is unqualified")?;
    anyhow::ensure!(
        !probe.invalid && !probe.work_selected && !probe.mode_unverified,
        "Provider commitment context is ambiguous"
    );
    if let Some(conversation) = &navigation.conversation_id {
        anyhow::ensure!(
            stable_conversation_id(conversation) && !probe.messages.is_empty(),
            "Existing provider conversation has no complete user identity"
        );
    } else {
        let path = navigation.landing_path.as_deref().unwrap_or("");
        let project = path
            .strip_prefix("/g/g-p-")
            .and_then(|p| p.strip_suffix("/project"))
            .is_some_and(stable_conversation_id);
        anyhow::ensure!(
            (path == "/" || project) && probe.turn_ids.is_empty(),
            "Provider landing page is unqualified or has historical turns"
        );
    }
    Ok(CommitSnapshot {
        owner: owner.clone(),
        navigation,
        message_ids: probe.messages.into_iter().map(|m| m.id).collect(),
        turn_ids: probe.turn_ids,
        canonical_prompt: probe.canonical_prompt,
        submission_intent: None,
        submission_witness: None,
    })
}
/// `latch` keeps the first conversation the receipt reaches; a send owns only that one.
fn observe(
    owner: &SubmissionOwner,
    before: &CommitSnapshot,
    value: Value,
    latch: &mut Option<String>,
) -> CommitObservation {
    use CommitObservation::*;
    if *owner != before.owner {
        return Ambiguous("pre-dispatch-identity-unavailable");
    }
    if before.canonical_prompt.is_empty() {
        return Ambiguous("attachment-only-commit-unqualified");
    }
    let Ok(probe) = parse(value) else {
        return Ambiguous("user-identity-unavailable");
    };
    if probe.invalid {
        return Ambiguous("user-identity-unavailable");
    }
    let Some(navigation) = navigation(&probe.current_url) else {
        return Ambiguous("conversation-context-changed");
    };
    if probe.work_selected
        || probe.mode_unverified
        || navigation.origin != before.navigation.origin
        || before
            .navigation
            .conversation_id
            .as_ref()
            .is_some_and(|id| navigation.conversation_id.as_ref() != Some(id))
        || probe.canonical_prompt != before.canonical_prompt
    {
        return Ambiguous("conversation-context-changed");
    }
    let Some(conversation) = navigation.conversation_id else {
        return if navigation.landing_path != before.navigation.landing_path {
            Ambiguous("conversation-context-changed")
        } else {
            Pending
        };
    };
    // Landing snapshots have no historical anchor. Only an admitted outbound client
    // message identity can distinguish creation from another mounted conversation.
    let needs_witness =
        before.navigation.conversation_id.is_none() || before.submission_intent.is_some();
    if needs_witness {
        let (Some(intent), Some(witness)) = (&before.submission_intent, &probe.submission_witness)
        else {
            return Ambiguous("submission-witness-unavailable");
        };
        if witness.version != 1
            || witness.owner != *owner
            || witness.witness_id != intent.witness_id
            || witness.dispatch_guard_id != intent.dispatch_guard_id
            || !id(&witness.message_id)
            || witness.prompt_sha256 != intent.prompt_sha256
            || witness.source != "chatgpt-fetch-v1"
            || witness.request_path != "/backend-api/f/conversation"
            || !witness.history_disabled
        {
            return Ambiguous("submission-witness-invalid");
        }
    }
    if latch.as_ref().is_some_and(|id| *id != conversation) {
        return Ambiguous("conversation-context-changed");
    }
    *latch = Some(conversation.clone());
    let added = probe
        .messages
        .iter()
        .filter(|m| !before.message_ids.contains(&m.id))
        .collect::<Vec<_>>();
    if added.is_empty() {
        return Pending;
    }
    if added.len() != 1 {
        return Ambiguous("multiple-new-user-messages");
    }
    let message = added[0];
    if needs_witness
        && probe
            .submission_witness
            .as_ref()
            .is_none_or(|w| w.message_id != message.id)
    {
        return Ambiguous("submission-message-identity-mismatch");
    }
    let anchor = before.turn_ids.last();
    let anchor_index = anchor.and_then(|anchor| probe.turn_ids.iter().position(|id| id == anchor));
    if anchor.is_some() && anchor_index.is_none()
        || anchor_index.is_some_and(|index| message.index as usize <= index)
        || before.turn_ids.contains(&message.turn_id)
    {
        return Ambiguous("historical-or-unanchored-user-message");
    }
    let web_search = before
        .submission_intent
        .as_ref()
        .is_some_and(|intent| intent.web_search);
    let Some(source) = message.fingerprint_text.as_ref().filter(|source| {
        message.matches == Some(true)
            && (*source == &before.canonical_prompt
                || markdown_source_text(source).as_deref()
                    == Some(before.canonical_prompt.as_str())
                || web_search && web_search_source(source, &before.canonical_prompt))
    }) else {
        return Ambiguous("complete-prompt-mismatch");
    };
    Committed(Box::new(CommitReceipt {
        owner: owner.clone(),
        version: 1,
        kind: "new-user-message".into(),
        message_id: message.id.clone(),
        turn_id: message.turn_id.clone(),
        message_fingerprint: prompt_fingerprint(source, &message.id),
        message_fingerprint_format: "original-source-v1".into(),
        prompt_text_fingerprint: sha256(before.canonical_prompt.as_bytes()),
        conversation: Conversation {
            origin: navigation.origin,
            conversation_id: conversation,
        },
        turns_count: probe.turn_ids.len(),
        submission_witness: probe.submission_witness,
    }))
}
/// With Web search selected, ChatGPT stores the pill as an '@Web search' token at its composer
/// position (observed 27 October 2026), the forms the submission witness accepts.
fn web_search_source(source: &str, prompt: &str) -> bool {
    source.strip_suffix("@Web search") == Some(prompt)
        || source.strip_prefix("@Web search ") == Some(prompt)
}
pub async fn capture_snapshot(
    cdp: &Cdp,
    owner: &SubmissionOwner,
    prompt: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<CommitSnapshot> {
    let value = cdp
        .evaluate_with_arguments(PROBE, &json!([owner, null, prompt]), timeout, cancel)
        .await?;
    snapshot(owner, value)
}
pub async fn read_receipt(
    cdp: &Cdp,
    owner: &SubmissionOwner,
    before: &CommitSnapshot,
    prompt: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    latch: &mut Option<String>,
) -> Result<CommitObservation> {
    if *owner != before.owner {
        bail!("Pre-dispatch submission ownership changed");
    }
    let observed = cdp
        .evaluate_with_arguments(
            &receipt_probe(),
            &json!([{"owner":owner,"ids":before.message_ids,"prompt":prompt,
                "intent":before.submission_intent,"saved":before.submission_witness}]),
            timeout,
            cancel,
        )
        .await?;
    let mut value = observed["probe"].clone();
    match observed["submission"]["state"].as_str() {
        Some("captured") => value["submissionWitness"] = observed["submission"]["witness"].clone(),
        Some("pending") => return Ok(CommitObservation::Pending),
        Some("none") if before.submission_intent.is_none() => {}
        _ => {
            return Ok(CommitObservation::Ambiguous(
                "submission-witness-unavailable",
            ));
        }
    }
    Ok(observe(owner, before, value, latch))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> SubmissionOwner {
        SubmissionOwner {
            claim_id: "controller-public".into(),
            reservation_id: "reservation-public".into(),
        }
    }
    fn probe(messages: Vec<Value>, turns: Vec<&str>, prompt: &str) -> Value {
        json!({"currentUrl":"https://chatgpt.com/c/conversation-public","workSelected":false,"modeUnverified":false,"invalid":false,"turnIds":turns,"messages":messages,"canonicalPrompt":prompt})
    }
    fn message(id: &str, turn: &str, index: u64, source: &str) -> Value {
        json!({"id":id,"turnId":turn,"index":index,"matches":true,"fingerprintText":source})
    }
    fn bound_landing(prompt: &str) -> CommitSnapshot {
        let mut landing = probe(vec![], vec![], prompt);
        landing["currentUrl"] = json!("https://chatgpt.com/");
        let mut before = snapshot(&owner(), landing).unwrap();
        before
            .bind_submission(SubmissionIntent {
                witness_id: "PUBLIC-witness".into(),
                dispatch_guard_id: "PUBLIC-send-guard".into(),
                prompt_sha256: sha256(prompt.as_bytes()),
                web_search: false,
            })
            .unwrap();
        before
    }
    fn with_witness(before: &CommitSnapshot, mut value: Value, message_id: &str) -> Value {
        let intent = before.submission_intent.as_ref().unwrap();
        value["submissionWitness"] = json!({"version":1,"owner":before.owner,
            "witnessId":intent.witness_id,"dispatchGuardId":intent.dispatch_guard_id,
            "promptSha256":intent.prompt_sha256,"messageId":message_id,
            "source":"chatgpt-fetch-v1","requestPath":"/backend-api/f/conversation","historyDisabled":true});
        value
    }
    fn observe_once(
        owner: &SubmissionOwner,
        before: &CommitSnapshot,
        value: Value,
    ) -> CommitObservation {
        observe(owner, before, value, &mut None)
    }
    fn receipt_for(prompt: &str, source: &str) -> CommitObservation {
        let owner = owner();
        let before = snapshot(
            &owner,
            probe(
                vec![message("old-user", "old-turn", 0, "PUBLIC old")],
                vec!["old-turn"],
                prompt,
            ),
        )
        .unwrap();
        observe_once(
            &owner,
            &before,
            probe(
                vec![
                    message("old-user", "old-turn", 0, "PUBLIC old"),
                    message("new-user", "new-turn", 1, source),
                ],
                vec!["old-turn", "new-turn"],
                prompt,
            ),
        )
    }
    // Reference fixtures from tests/browser/ownedUserCommitReceipt.test.ts at 46c757d.
    const BARE_URL_PROMPT: &str = "Review https://example.com/docs/guide.md before release.\n\n1. Run `npm test` and *check* the <output>.\n- Keep C:\\Tools paths.";
    const ESCAPED_BARE_URL: &str = r#"Review [https://example.com/docs/guide.md](https://example.com/docs/guide.md) before release\.

1\. Run \`npm test\` and \*check\* the \<output\>\.
\- Keep C:\\Tools paths\."#;
    #[test]
    fn escaped_markdown_sources_with_link_evidence_commit_with_the_stored_source_fingerprint() {
        let observed_prompt = "Run `gh release edit` on live-evidence/*.json.\n- check <file> in test_workflow.py\n1. emits https://slsa.dev/provenance/v1 (the default)";
        let observed_source = r#"Run \`gh release edit\` on live-evidence/\*.json.
\- check \<file> in test_workflow\.py
1\. emits [https://slsa.dev/provenance/v1](https://slsa.dev/provenance/v1) (the default)"#;
        let link_text_prompt =
            "- change.diff: the change.\nturn [url](url) back into url\n1. Can it match?";
        let link_text_source = r#"\- change.diff: the change.
turn [url]\(url) back into url
1\. Can it match?"#;
        for (prompt, source) in [
            (BARE_URL_PROMPT, ESCAPED_BARE_URL),
            (observed_prompt, observed_source),
            (link_text_prompt, link_text_source),
        ] {
            let CommitObservation::Committed(receipt) = receipt_for(prompt, source) else {
                panic!("Escaped source must commit: {source}");
            };
            assert_eq!(
                receipt.message_fingerprint,
                prompt_fingerprint(source, "new-user")
            );
            assert_eq!(receipt.prompt_text_fingerprint, sha256(prompt.as_bytes()));
        }
    }
    #[test]
    fn escaped_markdown_sources_without_exact_link_evidence_are_refused() {
        let url = "https://example.com/docs/guide.md";
        let mismatches = [
            ESCAPED_BARE_URL.replace("release", "merge"),
            ESCAPED_BARE_URL[..ESCAPED_BARE_URL.len() - 6].to_owned(),
            ESCAPED_BARE_URL.replace(&format!("({url})"), "(https://example.net/docs/guide.md)"),
            ESCAPED_BARE_URL.replace(r"C:\\Tools", "C:Tools"),
            ESCAPED_BARE_URL.replace(r"\*check\*", r"\*check\*!"),
        ];
        for stored in &mismatches {
            assert!(matches!(
                receipt_for(BARE_URL_PROMPT, stored),
                CommitObservation::Ambiguous("complete-prompt-mismatch")
            ));
        }
        for (prompt, stored) in [
            ("Use * literally.", r"Use \* literally\."),
            (
                "`https://a.example`",
                "`[https://a.example](https://a.example)`",
            ),
            (
                "[https://a.example](https://b.example)",
                "[[https://a.example](https://a.example)](https://b.example)",
            ),
            (
                "https://a.example[https://b.example",
                "[https://a.example[https://b.example](https://a.example[https://b.example)",
            ),
            (
                "!https://a.example",
                "![https://a.example](https://a.example)",
            ),
        ] {
            assert!(matches!(
                receipt_for(prompt, stored),
                CommitObservation::Ambiguous("complete-prompt-mismatch")
            ));
        }
    }
    #[test]
    fn the_rust_check_refuses_an_escaped_source_the_probe_matched_wrongly() {
        // The probe's `matches` alone never commits; the Rust decoder must agree.
        assert!(matches!(
            receipt_for("PUBLIC.", r"PUBLIC\."),
            CommitObservation::Ambiguous("complete-prompt-mismatch")
        ));
    }
    #[test]
    fn complete_autolinks_keep_delimiters_and_escaped_literal_links() {
        for (prompt, source) in [
            (
                "See (https://a.example).",
                r"See ([https://a.example](https://a.example))\.",
            ),
            (
                "See https://a.example.",
                r"See [https://a.example](https://a.example)\.",
            ),
            ("Type [label](target).", r"Type \[label\]\(target\)\."),
            (
                "See \"https://a.example.\"",
                r#"See \"[https://a.example](https://a.example)\.\""#,
            ),
        ] {
            assert!(matches!(
                receipt_for(prompt, source),
                CommitObservation::Committed(_)
            ));
        }
        assert_eq!(
            markdown_source_text(&format!("{} ]\\( now", "[".repeat(50_000))),
            None
        );
        assert_eq!(
            markdown_source_text(&format!(
                "[https://a.example](https://a.example){}x",
                "\\".repeat(50_000)
            )),
            None
        );
    }
    #[test]
    fn incomplete_links_and_partial_autolink_tokens_never_authorise_decoding() {
        for (prompt, source) in [
            ("Type ]( now", r"Type ]\( now"),
            ("Type [label]( now", r"Type [label]\( now"),
            ("Type [label](target now", r"Type [label]\(target now"),
            ("Visit https:", "Visit [https:](https:)"),
            (
                "Visit https:a.example",
                "Visit [https:a.example](https:a.example)",
            ),
            (
                "Visit https://a.example/docs",
                "Visit [https://a.example](https://a.example)/docs",
            ),
            (
                "Visit https://a.example?q=1",
                "Visit [https://a.example](https://a.example)?q=1",
            ),
            (
                "Visit prefixhttps://a.example",
                "Visit prefix[https://a.example](https://a.example)",
            ),
            (
                "Visit https://a.example.evil",
                "Visit [https://a.example](https://a.example).evil",
            ),
        ] {
            assert!(
                matches!(
                    receipt_for(prompt, source),
                    CommitObservation::Ambiguous("complete-prompt-mismatch")
                ),
                "Unexpected decoding: {source:?}"
            );
            // Exact source equality remains authoritative, including deliberately typed syntax.
            assert!(matches!(
                receipt_for(source, source),
                CommitObservation::Committed(_)
            ));
        }
    }
    #[test]
    fn landing_matching_historical_source_without_creation_witness_is_ambiguous() {
        let owner = owner();
        let mut landing = probe(vec![], vec![], "PUBLIC prompt");
        landing["currentUrl"] = json!("https://chatgpt.com/");
        let before = snapshot(&owner, landing).unwrap();
        assert!(matches!(
            observe_once(
                &owner,
                &before,
                probe(
                    vec![message(
                        "historical-user",
                        "historical-turn",
                        0,
                        "PUBLIC prompt"
                    )],
                    vec!["historical-turn"],
                    "PUBLIC prompt"
                )
            ),
            CommitObservation::Ambiguous("submission-witness-unavailable")
        ));
    }
    #[test]
    fn a_landing_page_send_owns_only_the_first_conversation_it_reaches() {
        let owner = owner();
        let before = bound_landing("PUBLIC");
        let mut latch = None;
        let reached = |conversation: &str| {
            let mut value = probe(vec![], vec![], "PUBLIC");
            value["currentUrl"] = json!(format!("https://chatgpt.com/c/{conversation}"));
            with_witness(&before, value, "new-user")
        };
        assert!(matches!(
            observe(&owner, &before, reached("first-public"), &mut latch),
            CommitObservation::Pending
        ));
        assert_eq!(latch.as_deref(), Some("first-public"));
        assert!(matches!(
            observe(&owner, &before, reached("second-public"), &mut latch),
            CommitObservation::Ambiguous("conversation-context-changed")
        ));
        assert_eq!(latch.as_deref(), Some("first-public"));
    }
    #[test]
    fn verified_web_search_accepts_only_the_witnessed_pill_token_forms() {
        let owner = owner();
        let prompt = "PUBLIC prompt";
        let commit = |web_search: bool, source: &str| {
            let mut before = bound_landing(prompt);
            before.submission_intent.as_mut().unwrap().web_search = web_search;
            let value = with_witness(
                &before,
                probe(
                    vec![message("new-user", "new-turn", 0, source)],
                    vec!["new-turn"],
                    prompt,
                ),
                "new-user",
            );
            observe_once(&owner, &before, value)
        };
        for source in ["PUBLIC prompt@Web search", "@Web search PUBLIC prompt"] {
            assert!(matches!(
                commit(true, source),
                CommitObservation::Committed(_)
            ));
            assert!(matches!(
                commit(false, source),
                CommitObservation::Ambiguous("complete-prompt-mismatch")
            ));
        }
        for source in [
            "PUBLIC prompt @Web search",
            "PUBLIC prompt
Web search",
            "@Web searchPUBLIC prompt",
            "PUBLIC prompt@Web search@Web search",
            "PUBLIC promp@Web search",
        ] {
            assert!(matches!(
                commit(true, source),
                CommitObservation::Ambiguous("complete-prompt-mismatch")
            ));
        }
        assert!(matches!(
            commit(true, prompt),
            CommitObservation::Committed(_)
        ));
    }
    #[test]
    fn creation_witness_requires_exact_message_source_owner_and_prepared_intent() {
        let owner = owner();
        let before = bound_landing("PUBLIC prompt");
        let value = with_witness(
            &before,
            probe(
                vec![message("new-user", "new-turn", 0, "PUBLIC prompt")],
                vec!["new-turn"],
                "PUBLIC prompt",
            ),
            "new-user",
        );
        let CommitObservation::Committed(receipt) = observe_once(&owner, &before, value.clone())
        else {
            panic!("Qualified creation must commit");
        };
        let mut sealed = before.clone();
        sealed.seal_witness(&receipt).unwrap();
        let restored: CommitSnapshot =
            serde_json::from_value(serde_json::to_value(sealed).unwrap()).unwrap();
        assert_eq!(restored.submission_witness.unwrap().message_id, "new-user");
        for (field, replacement) in [
            ("messageId", json!("historical-user")),
            (
                "owner",
                json!({"claimId":"foreign","reservationId":"reservation-public"}),
            ),
            ("witnessId", json!("foreign")),
            ("dispatchGuardId", json!("foreign")),
            ("promptSha256", json!(sha256(b"foreign"))),
            ("version", json!(2)),
            ("source", json!("navigation")),
            ("requestPath", json!("/unqualified")),
            ("historyDisabled", json!(false)),
        ] {
            let mut changed = value.clone();
            changed["submissionWitness"][field] = replacement;
            assert!(
                matches!(
                    observe_once(&owner, &before, changed),
                    CommitObservation::Ambiguous(_)
                ),
                "Accepted conflicting {field}"
            );
        }
        let mut changed = value.clone();
        changed["messages"][0]["fingerprintText"] = json!("PUBLIC wrong source");
        assert!(matches!(
            observe_once(&owner, &before, changed),
            CommitObservation::Ambiguous("complete-prompt-mismatch")
        ));
        let mut historical = value;
        historical["messages"][0]["id"] = json!("historical-user");
        assert!(matches!(
            observe_once(&owner, &before, historical),
            CommitObservation::Ambiguous("submission-message-identity-mismatch")
        ));
    }
    /// Differential check against the reference decoder: `ALCUIN_DECODER_CASES` names a JSON array
    /// of `[input, output-or-null]` pairs produced by the reference `markdownSourceText`.
    #[test]
    #[ignore = "needs reference decoder cases"]
    fn markdown_source_text_matches_reference_cases() {
        let path = std::env::var("ALCUIN_DECODER_CASES").unwrap();
        let cases: Vec<(String, Option<String>)> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for (input, output) in &cases {
            assert_eq!(&markdown_source_text(input), output, "{input:?}");
        }
        assert!(cases.len() > 1000);
    }
    #[test]
    fn collapsed_original_source_is_committed_in_full_only_for_one_new_owned_message() {
        let owner = owner();
        let expected = format!(
            "{}\n```rust\nfn public() {{}}\n```\nEnd",
            "PUBLIC long source ".repeat(2800)
        );
        let before = snapshot(
            &owner,
            probe(
                vec![message("old-user", "old-turn", 0, "PUBLIC old")],
                vec!["old-turn"],
                &expected,
            ),
        )
        .unwrap();
        let after = probe(
            vec![
                message("old-user", "old-turn", 0, "PUBLIC old"),
                message("new-user", "new-turn", 1, &expected),
            ],
            vec!["old-turn", "new-turn"],
            &expected,
        );
        let CommitObservation::Committed(receipt) = observe_once(&owner, &before, after.clone())
        else {
            panic!("Complete native source must commit");
        };
        assert_eq!(
            receipt.message_fingerprint,
            prompt_fingerprint(&expected, "new-user")
        );
        assert_eq!(receipt.message_fingerprint_format, "original-source-v1");
        assert_eq!(receipt.prompt_text_fingerprint, sha256(expected.as_bytes()));
        let mut truncated = after.clone();
        truncated["messages"][1]["fingerprintText"] = json!(&expected[..10240]);
        assert!(matches!(
            observe_once(&owner, &before, truncated),
            CommitObservation::Ambiguous("complete-prompt-mismatch")
        ));
        let mut foreign = after;
        foreign["currentUrl"] = json!("https://foreign.example/c/conversation-public");
        assert!(matches!(
            observe_once(&owner, &before, foreign),
            CommitObservation::Ambiguous("conversation-context-changed")
        ));
    }
    #[test]
    fn historical_echo_multiple_messages_and_unmounted_anchor_are_ambiguous() {
        let owner = owner();
        let before = snapshot(
            &owner,
            probe(
                vec![message("old-user", "old-turn", 0, "PUBLIC")],
                vec!["old-turn"],
                "PUBLIC",
            ),
        )
        .unwrap();
        assert!(matches!(
            observe_once(
                &owner,
                &before,
                probe(
                    vec![message("old-user", "old-turn", 0, "PUBLIC")],
                    vec!["old-turn"],
                    "PUBLIC"
                )
            ),
            CommitObservation::Pending
        ));
        assert!(matches!(
            observe_once(
                &owner,
                &before,
                probe(
                    vec![message("new-user", "new-turn", 0, "PUBLIC")],
                    vec!["new-turn"],
                    "PUBLIC"
                )
            ),
            CommitObservation::Ambiguous("historical-or-unanchored-user-message")
        ));
        assert!(matches!(
            observe_once(
                &owner,
                &before,
                probe(
                    vec![
                        message("new-user", "new-turn", 1, "PUBLIC"),
                        message("second-user", "second-turn", 2, "PUBLIC")
                    ],
                    vec!["old-turn", "new-turn", "second-turn"],
                    "PUBLIC"
                )
            ),
            CommitObservation::Ambiguous("multiple-new-user-messages")
        ));
    }
    #[test]
    fn malformed_original_source_and_provider_identity_never_fall_back_to_rendered_text() {
        let owner = owner();
        let before = snapshot(
            &owner,
            probe(
                vec![message("old-user", "old-turn", 0, "PUBLIC")],
                vec!["old-turn"],
                "PUBLIC",
            ),
        )
        .unwrap();
        for value in [json!(null), json!(true), json!("fragment")] {
            let mut after = probe(
                vec![message("new-user", "new-turn", 1, "PUBLIC")],
                vec!["old-turn", "new-turn"],
                "PUBLIC",
            );
            after["messages"][0]["fingerprintText"] = value;
            assert!(!matches!(
                observe_once(&owner, &before, after),
                CommitObservation::Committed(_)
            ));
        }
        assert!(
            parse(probe(
                vec![message("new-user", "wrong-turn", 0, "PUBLIC")],
                vec!["new-turn"],
                "PUBLIC"
            ))
            .is_err()
        );
        assert!(parse(probe(vec![], vec!["reused", "reused"], "PUBLIC")).is_err());
        let mut foreign = owner.clone();
        foreign.claim_id = "foreign-owner".into();
        assert!(matches!(
            observe_once(&foreign, &before, probe(vec![], vec![], "PUBLIC")),
            CommitObservation::Ambiguous("pre-dispatch-identity-unavailable")
        ));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };
    #[tokio::test]
    async fn raw_cdp_snapshot_and_full_source_receipt_use_only_read_evaluation() {
        let (client, server) = tokio::io::duplex(4096);
        let cdp = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_secs(1),
        );
        let mut peer = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let owner = SubmissionOwner {
            claim_id: "public-claim".into(),
            reservation_id: "public-reservation".into(),
        };
        let prompt = "PUBLIC full source\n```rust\nprintln!(\"quoted </script> source\");\n```\n";
        let worker = tokio::spawn(async move {
            for turn in 0..2 {
                let frame = peer.next().await.unwrap().unwrap();
                let request: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "Runtime.evaluate");
                let source = request["params"]["expression"].as_str().unwrap();
                assert!(source.contains("readUserOriginalSource"));
                assert!(source.contains("Object.getOwnPropertyDescriptor"));
                assert!(source.contains("oracle:target-claim"));
                assert!(source.contains(&serde_json::to_string(prompt).unwrap()));
                let mut value = if turn == 0 {
                    json!({"currentUrl":"https://chatgpt.com/","workSelected":false,"modeUnverified":false,"invalid":false,"turnIds":[],"messages":[],"canonicalPrompt":prompt})
                } else {
                    json!({"currentUrl":"https://chatgpt.com/c/public-conversation","workSelected":false,"modeUnverified":false,"invalid":false,"turnIds":["public-turn"],"messages":[{"id":"public-user","turnId":"public-turn","index":0,"matches":true,"fingerprintText":prompt}],"canonicalPrompt":prompt})
                };
                if turn == 1 {
                    assert!(source.contains("alcuin-dom:submission-receipt"));
                    value = json!({"probe":value,"submission":{"state":"captured","witness":{
                        "version":1,"owner":{"claimId":"public-claim","reservationId":"public-reservation"},
                        "witnessId":"public-witness","dispatchGuardId":"public-dispatch",
                        "messageId":"public-user","promptSha256":sha256(prompt.as_bytes()),
                        "source":"chatgpt-fetch-v1","requestPath":"/backend-api/f/conversation","historyDisabled":true}}});
                }
                peer.send(Message::Text(
                    json!({"id":request["id"],"result":{"result":{"value":value}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        let cancel = CancellationToken::new();
        let mut snapshot = capture_snapshot(&cdp, &owner, prompt, Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        snapshot
            .bind_submission(SubmissionIntent {
                witness_id: "public-witness".into(),
                dispatch_guard_id: "public-dispatch".into(),
                prompt_sha256: sha256(prompt.as_bytes()),
                web_search: false,
            })
            .unwrap();
        let CommitObservation::Committed(receipt) = read_receipt(
            &cdp,
            &owner,
            &snapshot,
            prompt,
            Duration::from_secs(1),
            &cancel,
            &mut None,
        )
        .await
        .unwrap() else {
            panic!("Complete synthetic source must commit");
        };
        assert_eq!(
            receipt.message_fingerprint,
            prompt_fingerprint(prompt, "public-user")
        );
        worker.await.unwrap();
        cdp.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
    }
}

//! Sequential browser prompt normalisation and the final conversation transcript.
use crate::{
    artifacts::{self, CaptureReceipt},
    policy,
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;

pub(crate) fn prompts(options: &Value) -> Result<Vec<String>> {
    let Some(values) = options.get("browserFollowUps").filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    values
        .as_array()
        .context("Browser follow-ups must be an array of prompts")?
        .iter()
        .map(|value| {
            Ok(policy::trim(value.as_str().context("Browser follow-up must be text")?).to_owned())
        })
        .filter(|value: &Result<String>| !matches!(value, Ok(text) if text.is_empty()))
        .collect()
}

pub(crate) fn transcript(initial: &str, followups: &[(String, String)]) -> String {
    if followups.is_empty() {
        return initial.to_owned();
    }
    fn answer(text: &str) -> &str {
        let text = policy::trim(text);
        if text.is_empty() {
            "_No text captured._"
        } else {
            text
        }
    }
    let mut text = format!("## Initial response\n\n### Answer\n\n{}", answer(initial));
    for (index, (prompt, response)) in followups.iter().enumerate() {
        text.push_str(&format!(
            "\n\n## Follow-up {}\n\n### Prompt\n\n{}\n\n### Answer\n\n{}",
            index + 1,
            policy::trim(prompt),
            answer(response)
        ));
    }
    text
}

pub(crate) fn index(metadata: &Value) -> Result<usize> {
    metadata["dispatch"]
        .get("turnIndex")
        .map_or(Ok(0), |value| {
            value
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .context("Sequential turn index changed")
        })
}
pub(crate) fn saved(metadata: &Value) -> Result<&[Value]> {
    metadata["browser"]["runtime"]
        .get("completedBrowserTurns")
        .map_or(Ok(&[][..]), |v| {
            v.as_array()
                .map(Vec::as_slice)
                .context("Sequential turn history changed")
        })
}
pub(crate) fn verify_turn(
    store: &SessionStore,
    metadata: &Value,
    row: &Value,
    index: usize,
) -> Result<String> {
    if !row["report"].is_null() {
        ensure!(
            row["report"]["completed"] == true
                && row["report"]["researchStarted"] == false
                && row["report"]["textSha256"] == row["sourceSha256"]
                && row["capture"]["sha256"] == row["sourceSha256"]
                && row["assistantMessageId"].is_null()
                && row["assistantTurnId"].is_null(),
            "Saved research turn differs from its exact report capture"
        );
    }
    ensure!(
        row["index"] == index
            && row["receipt"]["conversation"]["conversationId"]
                == metadata["browser"]["runtime"]["conversationId"],
        "Saved browser turn belongs to another sequence"
    );
    if index < self::index(metadata)? {
        let prior = &metadata["dispatch"]["history"][index];
        ensure!(
            prior["userMessageId"] == row["receipt"]["messageId"]
                && prior["assistantMessageId"] == row["assistantMessageId"]
                && prior["assistantTurnId"] == row["assistantTurnId"]
                && prior["report"] == row["report"]
                && prior["answerSha256"] == row["sourceSha256"]
                && prior["capture"] == row["capture"],
            "Saved browser turn differs from its completed dispatch"
        );
    } else {
        ensure!(
            row["receipt"] == metadata["browser"]["runtime"]["userCommitReceipt"]
                && metadata["dispatch"]["turnCapture"] == row["capture"],
            "Saved browser turn differs from its committed source"
        );
    }
    let receipt: CaptureReceipt = serde_json::from_value(row["capture"].clone())?;
    ensure!(
        metadata["id"] == receipt.session_id,
        "Captured browser turn belongs to another session"
    );
    String::from_utf8(artifacts::verify_capture(store, &receipt)?)
        .context("Saved browser turn is not UTF-8")
}
pub(crate) fn saved_transcript(store: &SessionStore, metadata: &Value) -> Result<String> {
    let prompts = prompts(&metadata["options"])?;
    let rows = saved(metadata)?;
    ensure!(
        !prompts.is_empty() && rows.len() == prompts.len() + 1 && index(metadata)? == prompts.len(),
        "Sequential browser transcript is incomplete; retained turns cannot establish the full request"
    );
    let initial = verify_turn(store, metadata, &rows[0], 0)?;
    let followups = prompts
        .into_iter()
        .enumerate()
        .map(|(i, prompt)| Ok((prompt, verify_turn(store, metadata, &rows[i + 1], i + 1)?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(transcript(&initial, &followups))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalisation_preserves_order_and_rejects_malformed_prompts() {
        assert_eq!(prompts(&json!({})).unwrap(), Vec::<String>::new());
        assert_eq!(
            prompts(&json!({"browserFollowUps":["  next ","\u{feff}"," final\n"]})).unwrap(),
            ["next", "final"]
        );
        assert!(prompts(&json!({"browserFollowUps":["next", 3]})).is_err());
        assert!(prompts(&json!({"browserFollowUps":"next"})).is_err());
    }

    #[test]
    fn transcript_matches_single_and_sequential_turn_contracts() {
        assert_eq!(transcript(" initial\n", &[]), " initial\n");
        assert_eq!(
            transcript(" initial\n", &[(" next ".into(), " final\n".into())]),
            "## Initial response\n\n### Answer\n\ninitial\n\n## Follow-up 1\n\n### Prompt\n\nnext\n\n### Answer\n\nfinal"
        );
    }
}

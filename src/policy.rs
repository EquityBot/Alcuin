use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_API_MODEL: &str = "gpt-6-astra";

pub(crate) fn javascript_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

pub(crate) fn trim(value: &str) -> &str {
    value.trim_matches(javascript_whitespace)
}
pub(crate) fn trim_end(value: &str) -> &str {
    value.trim_end_matches(javascript_whitespace)
}

fn replace_runs(value: &str, separator: impl Fn(char) -> bool, replacement: char) -> String {
    let mut normalised = String::new();
    let mut in_separator = false;
    for character in value.chars() {
        if separator(character) {
            if !in_separator {
                normalised.push(replacement);
            }
            in_separator = true;
        } else {
            normalised.push(character);
            in_separator = false;
        }
    }
    normalised
}

/// New requests are restricted; historical stored requests remain readable.
pub fn assert_model_allowed(model: Option<&str>) -> Result<()> {
    let Some(model) = model else { return Ok(()) };
    let model = trim(model).to_lowercase();
    if model.is_empty() || model == "latest" {
        return Ok(());
    }
    let model = model.strip_prefix("openai/").unwrap_or(&model);
    let model = model.replace(['_', ' '], "-");
    if !matches!(
        model.as_str(),
        "gpt6" | "gpt-6" | "gpt6-pro" | "gpt-6-pro" | "gpt6-astra" | "gpt-6-astra"
    ) {
        bail!(
            "Industrious only permits Latest (GPT-6) for new dispatch. Pro is first; verified Extra High fallback remains on GPT-6."
        );
    }
    Ok(())
}

pub fn require_api_pro(mode: Option<&str>) -> Result<&'static str> {
    if mode.is_some_and(|mode| !mode.is_empty() && mode != "pro") {
        bail!("Industrious API dispatch requires Pro reasoning mode.");
    }
    Ok("pro")
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingTime {
    Light,
    Standard,
    Extended,
    ExtraHigh,
    Pro,
    Heavy,
}

impl ThinkingTime {
    pub fn parse(value: &str) -> Option<Self> {
        let value = replace_runs(
            &trim(value).to_lowercase(),
            |character| character == '_' || javascript_whitespace(character),
            '-',
        );
        match value.as_str() {
            "light" => Some(Self::Light),
            "standard" => Some(Self::Standard),
            "extended" => Some(Self::Extended),
            "extra-high" | "extrahigh" | "xhigh" => Some(Self::ExtraHigh),
            "pro" => Some(Self::Pro),
            // The maintained input contract normalises these aliases.
            "heavy" => Some(Self::Heavy),
            "high" => Some(Self::Extended),
            "instant" | "low" => Some(Self::Light),
            "medium" => Some(Self::Standard),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEvidence {
    pub verified: bool,
    pub source: String,
    pub requested_model: Option<String>,
    pub resolved_label: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingEvidence {
    pub verified: bool,
    pub strict_fail_closed: bool,
    pub source: String,
    pub requested_level: ThinkingTime,
    pub resolved_label: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn current_label(label: Option<&str>) -> bool {
    let Some(label) = label else { return false };
    let label = trim(label).to_lowercase().replace(['_', ' '], "-");
    matches!(label.as_str(), "latest" | "gpt6" | "gpt-6")
}

pub fn assert_browser_completion(
    model: Option<&ModelEvidence>,
    thinking: Option<&ThinkingEvidence>,
    effective_model: Option<&str>,
) -> Result<()> {
    let accepted = model.zip(thinking).is_some_and(|(model, thinking)| {
        let expected = match thinking.requested_level {
            ThinkingTime::Pro => Some(("pro", "gpt-6-pro")),
            ThinkingTime::ExtraHigh => Some(("extra high", "gpt-6-astra")),
            _ => None,
        };
        let label = thinking.resolved_label.as_deref().map(|label| {
            replace_runs(
                &trim(label).to_lowercase(),
                |character| matches!(character, '-' | '_'),
                ' ',
            )
        });
        model.verified
            && model.source == "chatgpt-model-picker"
            && current_label(model.requested_model.as_deref())
            && current_label(model.resolved_label.as_deref())
            && thinking.verified
            && thinking.strict_fail_closed
            && thinking.source == "chatgpt-thinking-picker"
            && expected.is_some_and(|(expected_label, expected_model)| {
                label.as_deref() == Some(expected_label)
                    && effective_model.is_none_or(|model| model == expected_model)
            })
    });
    if !accepted {
        bail!(
            "Industrious browser completion requires a verified Latest (GPT-6) Pro or Extra High pair. Preserve the uncertain result for recovery; do not replay or retire it."
        );
    }
    Ok(())
}

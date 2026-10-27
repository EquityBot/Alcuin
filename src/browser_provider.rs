//! Explicit provider consent and qualified browser failure boundaries.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Minted at the original observation. Dispatch, recovery and cancellation still
/// need independent verification before the coordinator can switch providers.
#[derive(Debug)]
pub(crate) enum Failure {
    AnswerDeadline,
    CommitDeadline,
    /// Contradictory source/identity evidence. It must never authorise a provider switch.
    CommitAmbiguous(String),
    ResponseLoadFailed,
    /// ChatGPT finished the turn with a notice instead of an answer (reference chatgpt-content-withheld).
    ContentWithheld(String),
    /// The submitted turn shows a Retry control (reference chatgpt-ui-warning).
    RetryOffered,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnswerDeadline => output.write_str("Owned answer deadline expired; recover the retained target without sending again"),
            Self::CommitDeadline => output.write_str("New user message acknowledgement is uncertain; no automatic replay"),
            Self::CommitAmbiguous(reason) => write!(output,"Complete new user message commitment is ambiguous ({reason}); no automatic replay"),
            Self::ResponseLoadFailed => output.write_str("ChatGPT could not load the answer to the submitted turn in this tab; the reply may still be in the conversation. Reattach to read it again; do not resubmit."),
            Self::ContentWithheld(notice) => write!(output,"ChatGPT finished the submitted assistant turn without showing an answer: {notice}"),
            Self::RetryOffered => output.write_str("ChatGPT could not generate the submitted assistant turn and offered Retry."),
        }
    }
}
impl std::error::Error for Failure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FallbackProvider {
    None,
    Grok,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConsentSource {
    Request,
    PersonalConfig,
    Default,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Consent {
    pub provider: FallbackProvider,
    pub source: ConsentSource,
}
impl Consent {
    pub(crate) fn resolve(input: &Value, browser: &Value) -> Result<Self> {
        let (value, source) = if let Some(value) = input.get("browserFallback") {
            (value, ConsentSource::Request)
        } else if let Some(value) = browser.get("fallbackProvider") {
            (value, ConsentSource::PersonalConfig)
        } else {
            return Ok(Self {
                provider: FallbackProvider::None,
                source: ConsentSource::Default,
            });
        };
        let provider = match value.as_str() {
            Some("none") => FallbackProvider::None,
            Some("grok") => FallbackProvider::Grok,
            _ => bail!("Browser fallback must be none or grok"),
        };
        Ok(Self { provider, source })
    }
    pub(crate) fn save(self, browser: &mut Value) {
        browser["fallbackProvider"] = json!(self.provider);
        browser["fallbackConsentSource"] = json!(self.source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_none_wins_and_project_configuration_never_grants_provider_consent() {
        assert_eq!(
            Consent::resolve(
                &json!({"browserFallback":"none"}),
                &json!({"fallbackProvider":"grok"})
            )
            .unwrap(),
            Consent {
                provider: FallbackProvider::None,
                source: ConsentSource::Request
            }
        );
        assert_eq!(
            Consent::resolve(&json!({}), &json!({"fallbackProvider":"grok"})).unwrap(),
            Consent {
                provider: FallbackProvider::Grok,
                source: ConsentSource::PersonalConfig
            }
        );
        let project = crate::config::sanitise_project(
            &json!({"browser":{"fallbackProvider":"grok","fallbackConsentSource":"request"}}),
        );
        assert_eq!(
            Consent::resolve(&json!({}), &project["browser"]).unwrap(),
            Consent {
                provider: FallbackProvider::None,
                source: ConsentSource::Default
            }
        );
        for invalid in [
            Value::Null,
            json!(true),
            json!("Grok"),
            json!("unknown"),
            json!(["grok"]),
        ] {
            assert!(
                Consent::resolve(
                    &json!({"browserFallback":invalid}),
                    &json!({"fallbackProvider":"grok"})
                )
                .is_err()
            );
        }
    }
}

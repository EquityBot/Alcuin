//! Maintained follow-up references and saved browser selections.
use crate::{
    browser,
    config::Environment,
    policy::{self, ThinkingTime},
    sessions::SessionStore,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub enum Followup {
    Api {
        response_id: String,
        session_id: Option<String>,
    },
    Browser {
        session_id: String,
        model: String,
        config: Value,
    },
}
fn response_id(response: &Value) -> Option<&str> {
    response
        .get("responseId")
        .filter(|value| !value.is_null())
        .unwrap_or(&response["id"])
        .as_str()
        .filter(|id| id.starts_with("resp_"))
}
pub fn metadata_response(parent: &Value, model: Option<&str>) -> Result<Option<String>> {
    if let Some(id) = response_id(&parent["response"]) {
        return Ok(Some(id.into()));
    }
    let runs = parent["models"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if runs.is_empty() {
        return Ok(None);
    }
    let chosen = if let Some(model) = model {
        runs.iter().find(|run| run["model"] == model)
    } else if runs.len() == 1 {
        runs.first()
    } else {
        None
    };
    let Some(chosen) = chosen else {
        let available = runs
            .iter()
            .map(|run| run["model"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(", ");
        let id = parent["id"].as_str().unwrap_or("");
        if let Some(model) = model {
            bail!("Session {id} has no model named {model}. Available: {available}");
        }
        bail!(
            "Session {id} has multiple model runs. Re-run with --followup-model. Available: {available}"
        );
    };
    Ok(response_id(&chosen["response"]).map(str::to_owned))
}
pub fn last_log_response(log: &str) -> Option<String> {
    let mut remaining = log;
    let mut last = None;
    while let Some(index) = remaining.find("resp_") {
        let tail = &remaining[index + 5..];
        let length = tail.bytes().take_while(u8::is_ascii_alphanumeric).count();
        if length > 0 {
            last = Some(format!("resp_{}", &tail[..length]));
        }
        remaining = tail;
    }
    last
}
fn similarity(input: &str, candidate: &str) -> f64 {
    if input == candidate {
        return 1.0;
    }
    if input.starts_with(candidate) || candidate.starts_with(input) {
        return 0.95;
    }
    if input.contains(candidate) || candidate.contains(input) {
        return 0.8;
    }
    let a = input.encode_utf16().collect::<Vec<_>>();
    let b = candidate.encode_utf16().collect::<Vec<_>>();
    let mut previous = (0..=b.len()).collect::<Vec<_>>();
    let mut current = previous.clone();
    for (i, one) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, two) in b.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(one != two));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    1.0 - previous[b.len()] as f64 / a.len().max(b.len()) as f64
}
fn missing_reference(store: &SessionStore, reference: &str) -> anyhow::Error {
    let mut seen = BTreeSet::new();
    let mut suggestions = store
        .list(false)
        .unwrap_or_default()
        .into_iter()
        .filter(|session| !crate::browser_attempt::internal(session))
        .filter_map(|session| session["id"].as_str().map(str::to_owned))
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .map(|id| {
            let score = similarity(&reference.to_lowercase(), &id.to_lowercase());
            (id, score)
        })
        .filter(|(_, score)| *score >= 0.45)
        .collect::<Vec<_>>();
    suggestions.sort_by(|a, b| b.1.total_cmp(&a.1));
    let suggestions = suggestions
        .into_iter()
        .take(3)
        .map(|(id, _)| format!("\"{id}\""))
        .collect::<Vec<_>>();
    let suggestion = if suggestions.is_empty() {
        String::new()
    } else {
        format!(" Did you mean: {}?", suggestions.join(", "))
    };
    anyhow::anyhow!(
        "No session found with ID {reference}.{suggestion} Run \"alcuin status --hours 72 --limit 20\" to list recent sessions."
    )
}
pub fn resolve(store: &SessionStore, reference: &str, model: Option<&str>) -> Result<Followup> {
    let reference = policy::trim(reference);
    ensure!(
        !reference.is_empty(),
        "--followup requires a session id or response id."
    );
    if reference.starts_with("resp_") {
        return Ok(Followup::Api {
            response_id: reference.into(),
            session_id: None,
        });
    }
    let parent = store
        .read(reference)?
        .ok_or_else(|| missing_reference(store, reference))?;
    if let Some(config) = browser_parent(reference, &parent)? {
        return Ok(Followup::Browser {
            session_id: parent["id"]
                .as_str()
                .context("Browser parent has no session identity")?
                .into(),
            model: parent["options"]["model"]
                .as_str()
                .or(parent["model"].as_str())
                .filter(|model| model.starts_with("gpt-"))
                .unwrap_or(policy::DEFAULT_API_MODEL)
                .into(),
            config,
        });
    }
    let response_id=metadata_response(&parent,model)?.or_else(||last_log_response(&store.log(reference).unwrap_or_default())).with_context(||format!("Session {reference} does not contain a stored response id. Ensure the original run produced a Responses API response id (background/store helps)."))?;
    Ok(Followup::Api {
        response_id,
        session_id: Some(
            parent["id"]
                .as_str()
                .context("API parent has no session identity")?
                .into(),
        ),
    })
}
pub fn require_api(engine: &str, model: &str, options: &Value, env: &Environment) -> Result<()> {
    ensure!(engine == "api", "--followup requires --engine api.");
    ensure!(
        !model.starts_with("gemini") && !model.starts_with("claude"),
        "--followup is only supported for OpenAI Responses API runs. Model {model} uses a provider client without previous_response_id support."
    );
    let configured =
        |value: Option<&str>| value.is_some_and(|value| !policy::trim(value).is_empty());
    let base = configured(options["baseUrl"].as_str())
        || configured(env.get("OPENAI_BASE_URL").map(String::as_str));
    let azure = configured(options["azure"]["endpoint"].as_str());
    ensure!(
        !base || azure,
        "--followup is only supported for the default OpenAI Responses API or Azure OpenAI Responses. Custom --base-url providers are not supported."
    );
    Ok(())
}
pub fn attachment_timeout(raw: &str) -> Option<f64> {
    let raw = policy::trim(raw).to_lowercase();
    if !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return raw.parse::<f64>().ok().filter(|value| value.is_finite());
    }
    let raw = raw
        .chars()
        .filter(|character| !policy::javascript_whitespace(*character))
        .collect::<String>();
    let mut tail = raw.as_str();
    let mut total = 0.0;
    let mut pieces = 0;
    while !tail.is_empty() {
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let number = tail[..digits].parse::<f64>().ok()?;
        tail = &tail[digits..];
        let (unit, multiplier) = if tail.starts_with("ms") {
            ("ms", 1.0)
        } else if tail.starts_with('s') {
            ("s", 1000.0)
        } else if tail.starts_with('m') {
            ("m", 60_000.0)
        } else if tail.starts_with('h') {
            ("h", 3_600_000.0)
        } else {
            return None;
        };
        tail = &tail[unit.len()..];
        total += number * multiplier;
        pieces += 1;
    }
    (total.is_finite() && (total > 0.0 || pieces == 1)).then_some(total)
}
fn recoverable_url(candidate: &Value) -> Option<String> {
    candidate
        .as_str()
        .filter(|url| browser::conversation_id(url).is_some())
        .map(str::to_owned)
}
pub fn browser_resume(parent: &Value) -> Option<String> {
    let harvest = &parent["browser"]["harvest"];
    let integrity = &harvest["integrity"];
    if (integrity.is_null()
        || integrity["status"] == "matched" && integrity["explicitTarget"] != true)
        && let Some(url) = recoverable_url(&harvest["url"])
    {
        return Some(url);
    }
    for candidate in [
        &parent["browser"]["runtime"]["tabUrl"],
        &parent["browser"]["archive"]["conversationUrl"],
    ] {
        if let Some(url) = recoverable_url(candidate) {
            return Some(url);
        }
    }
    let id = policy::trim(parent["browser"]["runtime"]["conversationId"].as_str()?);
    if id.is_empty() {
        return None;
    }
    let base = parent["browser"]["config"]["url"]
        .as_str()
        .unwrap_or("https://chatgpt.com/");
    let base = reqwest::Url::parse(base).ok()?;
    let url = format!(
        "{}{}/c/{id}",
        base.origin().ascii_serialization(),
        base.path().trim_end_matches('/')
    );
    recoverable_url(&json!(url))
}
fn expects_effort(model: Option<&str>) -> bool {
    let model = model.map(policy::trim).unwrap_or("").to_lowercase();
    matches!(
        model.as_str(),
        "gpt-5-pro" | "gpt-5.1-pro" | "gpt-5.2-pro" | "gpt-5.4-pro"
    ) || matches!(
        model.replace(['_', ' '], "-").as_str(),
        "gpt6-pro" | "gpt-6-pro"
    ) || matches!(model.as_str(), "gpt-5.6-sol" | "gpt-5.5-pro")
}
pub fn browser_parent(reference: &str, parent: &Value) -> Result<Option<Value>> {
    let browser_config = parent["browser"]
        .get("config")
        .filter(|config| !config.is_null())
        .or_else(|| parent["options"].get("browserConfig"));
    let mode = parent
        .get("mode")
        .filter(|mode| !mode.is_null())
        .unwrap_or(&parent["options"]["mode"]);
    if mode != "browser"
        && parent["browser"]["runtime"].is_null()
        && browser_config.is_none_or(Value::is_null)
    {
        return Ok(None);
    }
    let url=browser_resume(parent).with_context(||format!("Session {reference} is a browser session but does not contain a ChatGPT conversation URL. Run \"alcuin status --hours 72 --limit 20\" to list recent sessions."))?;
    let mut config = browser_config
        .filter(|config| config.is_object())
        .with_context(|| {
            format!("Session {reference} is missing its stored browser configuration.")
        })?
        .clone();
    if config["modelStrategy"].as_str().unwrap_or("select") == "select" {
        let model = config["desiredModel"]
            .as_str()
            .map(policy::trim)
            .filter(|model| !model.is_empty())
            .or_else(|| {
                (parent["browser"]["modelSelection"]["verified"] == true)
                    .then(|| {
                        parent["browser"]["modelSelection"]["requestedModel"]
                            .as_str()
                            .map(policy::trim)
                            .filter(|model| !model.is_empty())
                    })
                    .flatten()
            });
        let thinking = config
            .get("thinkingTime")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| {
                (parent["browser"]["thinkingSelection"]["verified"] == true)
                    .then(|| {
                        parent["browser"]["thinkingSelection"]["requestedLevel"]
                            .as_str()
                            .and_then(ThinkingTime::parse)
                            .map(|level| json!(level))
                    })
                    .flatten()
            });
        let requested = config["requestedModel"]
            .as_str()
            .or(parent["options"]["model"].as_str())
            .or(parent["model"].as_str());
        ensure!(
            model.is_some()
                && (!(config["requireVerifiedSelection"] == true || expects_effort(requested))
                    || thinking.as_ref().is_some_and(|thinking| thinking
                        .as_str()
                        .is_some_and(|level| !level.is_empty()))),
            "Session {reference} is missing its stored browser model or thinking effort. Refusing to replace the saved selection with current defaults."
        );
        config["desiredModel"] = json!(model.unwrap());
        if let Some(thinking) = thinking {
            config["thinkingTime"] = thinking;
        }
    }
    config["browserTabRef"] = Value::Null;
    config["resumeConversationUrl"] = json!(url);
    config["researchMode"] = json!("off");
    config["archiveConversations"] = json!("never");
    config["automaticModelPriority"] = json!(false);
    Ok(Some(config))
}

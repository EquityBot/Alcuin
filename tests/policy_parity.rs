use alcuin::policy::{
    ModelEvidence, ThinkingEvidence, ThinkingTime, assert_browser_completion, assert_model_allowed,
    require_api_pro,
};
use serde_json::Value;

fn fixture(contents: &str) -> Vec<Value> {
    serde_json::from_str(contents).expect("valid immutable reference fixture")
}

#[test]
fn admission_matches_reference_including_retired_models_and_unicode() {
    for case in fixture(include_str!("../fixtures/model-admission.json")) {
        let actual = assert_model_allowed(case["input"].as_str());
        assert_eq!(
            actual.is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{}",
            case
        );
        if let Err(error) = actual {
            assert_eq!(error.to_string(), case["error"].as_str().unwrap());
        }
    }
}

#[test]
fn api_reasoning_admission_matches_reference() {
    for case in fixture(include_str!("../fixtures/api-mode.json")) {
        let actual = require_api_pro(case["input"].as_str());
        assert_eq!(
            actual.is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{}",
            case
        );
        if let Err(error) = actual {
            assert_eq!(error.to_string(), case["error"].as_str().unwrap());
        }
    }
}

#[test]
fn thinking_aliases_match_reference_without_collapsing_heavy_into_pro() {
    let cases = fixture(include_str!("../fixtures/thinking-time.json"))
        .into_iter()
        .chain(fixture(include_str!(
            "../fixtures/thinking-time-edges.json"
        )));
    for case in cases {
        let actual = case["input"].as_str().and_then(ThinkingTime::parse);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            case["output"],
            "{}",
            case
        );
    }
}

#[test]
fn result_acceptance_requires_the_reference_model_and_tier_evidence() {
    for case in fixture(include_str!("../fixtures/browser-completion.json")) {
        let input = &case["input"];
        let model = serde_json::from_value::<ModelEvidence>(input["modelSelection"].clone()).ok();
        let thinking =
            serde_json::from_value::<ThinkingEvidence>(input["thinkingSelection"].clone()).ok();
        let actual = assert_browser_completion(
            model.as_ref(),
            thinking.as_ref(),
            input["effectiveModel"].as_str(),
        );
        assert_eq!(
            actual.is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{}",
            case
        );
        if let Err(error) = actual {
            assert_eq!(error.to_string(), case["error"].as_str().unwrap());
        }
    }
}

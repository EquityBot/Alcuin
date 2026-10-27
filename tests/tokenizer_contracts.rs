use industrious::tokenizer::Tokenizer;
use serde_json::Value;

#[test]
fn token_counts_and_complete_request_estimates_match_the_maintained_reference() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/tokenizer-reference.json")).unwrap();
    let tokenizer = Tokenizer::new().unwrap();
    for case in fixture["strings"].as_array().unwrap() {
        assert_eq!(
            tokenizer.count(case["input"].as_str().unwrap()) as u64,
            case["tokens"].as_u64().unwrap(),
            "String reference differs: {case}"
        );
    }
    for case in fixture["requests"].as_array().unwrap() {
        assert_eq!(
            tokenizer.estimate(&case["body"], 200).unwrap() as u64,
            case["tokens"].as_u64().unwrap(),
            "Request estimate differs: {case}"
        );
    }
}

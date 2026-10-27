use alcuin::process::ProcessIdentity;
use serde_json::json;

#[test]
fn fractional_start_timestamps_round_trip_without_changing_lock_identity() {
    for offset in 0..20_000_u64 {
        let native_start = 133_988_232_770_019_713 + offset;
        let identity = ProcessIdentity {
            pid: 1234,
            started_at_ms: native_start as f64 / 10000.0 - 11644473600000.0,
            native_start,
        };
        let bytes = serde_json::to_vec(&identity).unwrap();
        let restored: ProcessIdentity = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, identity, "offset={offset}");
    }
}

#[test]
fn windows_start_ticks_survive_a_legacy_json_reader_as_decimal_text() {
    let identity = ProcessIdentity {
        pid: 1234,
        started_at_ms: 1_791_869_094_000.0,
        native_start: 133_988_232_770_019_713,
    };
    let value = serde_json::to_value(&identity).unwrap();
    assert_eq!(value["nativeStart"], "133988232770019713");
    let round_trip: ProcessIdentity =
        serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
    assert_eq!(round_trip, identity);
}

#[test]
fn ambiguous_numbers_and_malformed_native_identities_grant_no_ownership() {
    for native in [
        json!(133_988_232_770_019_712u64),
        json!(""),
        json!("+1"),
        json!("1.5"),
        json!("18446744073709551616"),
        json!(-1),
    ] {
        assert!(
            serde_json::from_value::<ProcessIdentity>(
                json!({"pid":1234,"startedAtMs":1.0,"nativeStart":native})
            )
            .is_err()
        );
    }
    let legacy: ProcessIdentity =
        serde_json::from_value(json!({"pid":1234,"startedAtMs":1.0,"nativeStart":1000})).unwrap();
    assert_eq!(legacy.native_start, 1000);
}

#![no_main]
//! The canonical JSON that remote result digests are taken over, on any text.

use alcuin::remote_json::stringify;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    if let Ok(once) = stringify(text) {
        // The canonical form is its own canonical form, so a digest of it is stable.
        let once = String::from_utf8(once).expect("canonical JSON is UTF-8");
        assert_eq!(
            stringify(&once).ok().as_deref(),
            Some(once.as_bytes()),
            "{text:?}"
        );
    }
});

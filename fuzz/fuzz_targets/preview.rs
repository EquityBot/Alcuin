#![no_main]
//! The notification preview made from an answer's text.

use alcuin::notify::sanitize_preview;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let preview = sanitize_preview(text);
    // One line of at most 200 UTF-16 units, the ellipsis included.
    assert!(preview.encode_utf16().count() <= 200, "{preview:?}");
    assert!(
        preview.chars().all(|c| c == ' ' || !c.is_whitespace()),
        "{preview:?}"
    );
});

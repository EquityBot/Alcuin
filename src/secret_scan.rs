//! Refuses credentials before dispatch. Matching runs on bytes, so line length and binary content
//! do not matter, and only names a rule and a line: no matched byte leaves this module.
use regex::bytes::{Regex, RegexSet};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{self, Read},
    sync::LazyLock,
};

/// The most bytes one run reads to scan its files; a larger selection is refused unread.
pub const BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
const WINDOW: usize = 1024 * 1024;
/// Longer than the longest possible match (190 bytes) in UTF-32, so a match that a window boundary
/// cuts lies whole in the next window.
const OVERLAP: usize = 4096;
const SHOWN: usize = 5;
/// Every prefix comes from the vendor's documentation, or the standard for key blocks. Bounded, so
/// `OVERLAP` holds, and a body shorter than a real one only needs enough to pass placeholders.
const RULES: &[(&str, &str)] = &[
    (
        "private key block",
        r"-----BEGIN (?:[A-Z0-9]{1,12} ){0,3}PRIVATE KEY(?: BLOCK)?-----(?:\\[rn]|[\r\n \t]){1,8}(?:[A-Za-z0-9+/=]{32}|Proc-Type:|Version:|Comment:|Hash:|MessageID:|Charset:)",
    ),
    // The dot, dash and underscore cover the stateless `ghs_APPID_JWT` installation tokens.
    ("GitHub token", r"\bgh[pousr]_[A-Za-z0-9_.-]{36}"),
    (
        "GitHub fine-grained token",
        r"\bgithub_pat_[A-Za-z0-9_]{36}",
    ),
    ("GitLab token", r"\bglpat-[A-Za-z0-9_.-]{20}"),
    // An access key ID alone is not a secret, so a 40-character value must follow it.
    (
        "AWS access key pair",
        r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b(?s:.{0,128}?)[^A-Za-z0-9/+=][A-Za-z0-9/+]{40}(?:[^A-Za-z0-9/+=]|\z)",
    ),
    ("OpenAI admin key", r"\bsk-admin-[A-Za-z0-9_-]{40}"),
    (
        "Anthropic key",
        r"\bsk-ant-(?:api|admin)[0-9a-z]*-[A-Za-z0-9_-]{40}",
    ),
    ("xAI key", r"\bxai-[A-Za-z0-9]{80}"),
    ("Google API key", r"\bAIza[A-Za-z0-9_-]{35}"),
    (
        "Slack token",
        r"\b(?:xoxe\.)?(?:xox[bp]|xapp|xwfp)-[0-9]+-[A-Za-z0-9-]{20}",
    ),
    ("Stripe live key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{24}"),
    ("npm token", r"\bnpm_[A-Za-z0-9]{36}"),
    ("PyPI token", r"\bpypi-[A-Za-z0-9_-]{85}"),
];
static ANY: LazyLock<Regex> = LazyLock::new(|| {
    let rules: Vec<_> = RULES.iter().map(|rule| format!("(?:{})", rule.1)).collect();
    Regex::new(&format!("(?-u){}", rules.join("|"))).expect("credential patterns")
});
static EACH: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new(RULES.iter().map(|rule| format!("(?-u){}", rule.1))).expect("credential patterns")
});

/// Holds no matched bytes, so a report cannot show any part of a value, its tail or its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub line: u64,
    pub rule: &'static str,
}
#[derive(Debug)]
pub struct Scan {
    pub findings: BTreeSet<Finding>,
    pub sha256: String,
    pub bytes: u64,
}
fn newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|byte| **byte == b'\n').count() as u64
}
fn search(window: &[u8], line: u64, findings: &mut BTreeSet<Finding>) {
    let mut start = 0;
    while let Some(hit) = ANY.find_at(window, start) {
        // Vendor samples are marked EXAMPLE. Resume inside one, as it may precede a real value.
        if hit.as_bytes().windows(7).any(|part| part == b"EXAMPLE") {
            start = hit.start() + 1;
            continue;
        }
        start = hit.end();
        let rule = EACH.matches(hit.as_bytes()).iter().next();
        findings.insert(Finding {
            line: line + newlines(&window[..hit.start()]),
            rule: rule.map_or("credential pattern", |rule| RULES[rule].0),
        });
    }
}
/// Scans everything `reader` yields. Every pattern is ASCII, so a window holding a NUL gets a
/// second pass without NULs, which reveals UTF-16 and UTF-32 text in either byte order. The first
/// pass stays because removing a NUL can erase the `\b` before a token.
pub fn scan(mut reader: impl Read) -> io::Result<Scan> {
    let mut buffer = vec![0; OVERLAP + WINDOW];
    let (mut held, mut bytes, mut line) = (0, 0, 1);
    let mut hash = Sha256::new();
    let mut findings = BTreeSet::new();
    loop {
        let count = reader.read(&mut buffer[held..])?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[held..held + count]);
        bytes += count as u64;
        let filled = held + count;
        let window = &buffer[..filled];
        search(window, line, &mut findings);
        if window.contains(&0) {
            let visible: Vec<u8> = window.iter().copied().filter(|byte| *byte != 0).collect();
            search(&visible, line, &mut findings);
        }
        held = filled.min(OVERLAP);
        line += newlines(&window[..filled - held]);
        buffer.copy_within(filled - held..filled, 0);
    }
    Ok(Scan {
        findings,
        sha256: hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        bytes,
    })
}
/// One report line per finding, in the fixed shape every refusal uses.
pub fn lines(source: &str, findings: &BTreeSet<Finding>) -> Vec<String> {
    let mut lines: Vec<_> = findings
        .iter()
        .take(SHOWN)
        .map(|finding| format!("{source}:{}: {}", finding.line, finding.rule))
        .collect();
    if findings.len() > SHOWN {
        lines.push(format!("{source}: and more"));
    }
    lines
}
/// Refuses prompt text that matches a pattern. No override exists for it.
pub fn refuse_text<'a>(texts: impl IntoIterator<Item = (&'a str, &'a str)>) -> anyhow::Result<()> {
    let mut refused = vec![];
    for (source, text) in texts {
        refused.extend(lines(source, &scan(text.as_bytes())?.findings));
    }
    anyhow::ensure!(
        refused.is_empty(),
        "Refusing prompt text that matches a credential pattern:\n- {}\nNothing was sent. Remove the value from the prompt.",
        refused.join("\n- ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built at run time from split literals, so the repository holds no contiguous credential.
    fn body(length: usize, seed: u8) -> String {
        (0..length)
            .map(|index| (b'A' + ((index as u8).wrapping_mul(7).wrapping_add(seed)) % 26) as char)
            .collect()
    }
    fn fixtures() -> Vec<(&'static str, String)> {
        let dash = "-".repeat(5);
        let key_id = ["AK", "IA"].concat() + &body(16, 1);
        vec![
            (
                "private key block",
                format!("{dash}BEGIN OPENSSH PRIVATE KEY{dash}\n{}\n", body(70, 2)),
            ),
            (
                "private key block",
                format!("{dash}BEGIN RSA PRIVATE KEY{dash}\\n{}", body(64, 3)),
            ),
            (
                "private key block",
                format!("{dash}BEGIN PGP PRIVATE KEY BLOCK{dash}\n\n{}", body(64, 4)),
            ),
            (
                "private key block",
                format!("{dash}BEGIN RSA PRIVATE KEY{dash}\nProc-Type: 4,ENCRYPTED\n"),
            ),
            ("GitHub token", ["gh", "p_"].concat() + &body(36, 5)),
            (
                "GitHub token",
                format!(
                    "{}12345_ey{}.{}",
                    ["gh", "s_"].concat(),
                    body(36, 6),
                    body(9, 7)
                ),
            ),
            (
                "GitHub fine-grained token",
                ["github", "_pat_"].concat() + &body(82, 8),
            ),
            ("GitLab token", ["gl", "pat-"].concat() + &body(20, 9)),
            (
                "AWS access key pair",
                format!("{key_id}\naws_secret_access_key = {}\n", body(40, 10)),
            ),
            (
                "OpenAI admin key",
                ["sk-", "admin-"].concat() + &body(60, 11),
            ),
            (
                "Anthropic key",
                ["sk-", "ant-api03-"].concat() + &body(95, 12),
            ),
            (
                "Anthropic key",
                ["sk-", "ant-admin01-"].concat() + &body(95, 13),
            ),
            ("xAI key", ["x", "ai-"].concat() + &body(80, 14)),
            ("Google API key", ["AI", "za"].concat() + &body(35, 15)),
            (
                "Slack token",
                format!("{}111-222-333-{}", ["xo", "xp-"].concat(), body(32, 16)),
            ),
            (
                "Slack token",
                format!("{}1-A1-22-{}", ["xoxe.", "xapp-"].concat(), body(32, 17)),
            ),
            ("Stripe live key", ["rk_", "live_"].concat() + &body(24, 18)),
            ("npm token", ["np", "m_"].concat() + &body(36, 19)),
            ("PyPI token", ["py", "pi-"].concat() + &body(85, 20)),
        ]
    }
    fn found(bytes: &[u8]) -> Vec<&'static str> {
        let scan = scan(bytes).unwrap();
        scan.findings.iter().map(|finding| finding.rule).collect()
    }

    #[test]
    fn every_rule_has_a_fixture_and_fixtures_fit_the_overlap() {
        for (rule, _) in RULES {
            assert!(fixtures().iter().any(|(name, _)| name == rule), "{rule}");
        }
        for (rule, fixture) in fixtures() {
            assert_eq!(found(fixture.as_bytes()), [rule], "{fixture:?}");
            assert!(fixture.len() * 4 < OVERLAP, "{rule}");
        }
    }
    /// A reader that returns one exact chunk per call, so a test fixes the first window boundary.
    struct Chunks<'a>(&'a [u8], usize);
    impl Read for Chunks<'_> {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            let count = self.0.len().min(self.1).min(target.len());
            target[..count].copy_from_slice(&self.0[..count]);
            self.0 = &self.0[count..];
            Ok(count)
        }
    }
    #[test]
    fn a_match_cut_by_a_window_boundary_on_one_long_line_is_found() {
        for (rule, fixture) in fixtures() {
            for offset in (0..=fixture.len()).step_by(5) {
                let mut bytes = vec![b' '; OVERLAP + WINDOW - offset];
                bytes.extend_from_slice(fixture.as_bytes());
                bytes.extend_from_slice(&[b' '; 64]);
                assert_eq!(found(&bytes), [rule], "{rule} at {offset}");
            }
            let mut split = vec![b' '; 100];
            split.extend_from_slice(fixture.as_bytes());
            let chunks = Chunks(&split, 100 + fixture.len() / 2);
            let scan = scan(chunks).unwrap();
            assert!(scan.findings.iter().any(|f| f.rule == rule), "{rule}");
        }
    }
    #[test]
    fn utf16_and_utf32_text_is_found_in_either_byte_order_with_or_without_a_bom() {
        let widen = |text: &str, width: usize, big: bool, bom: bool| {
            let mut bytes = if bom { vec![0xFE, 0xFF] } else { vec![] };
            for byte in text.bytes() {
                let mut unit = vec![0; width];
                unit[if big { width - 1 } else { 0 }] = byte;
                bytes.extend(unit);
            }
            bytes
        };
        for (rule, fixture) in fixtures() {
            for (width, big, bom) in [
                (2, false, true),
                (2, true, true),
                (2, false, false),
                (2, true, false),
                (4, false, false),
                (4, true, false),
            ] {
                let wide = widen(&format!(" {fixture} "), width, big, bom);
                let mut bytes = vec![b' '; OVERLAP + WINDOW - wide.len() / 2];
                bytes.extend(&wide);
                assert!(found(&bytes).contains(&rule), "{rule} {width} {big} {bom}");
            }
        }
    }
    #[test]
    fn names_the_first_line_of_each_match_and_digests_the_whole_input() {
        let token = ["np", "m_"].concat() + &body(36, 21);
        let text = format!("one\ntwo\n{token}\n");
        let scan = scan(text.as_bytes()).unwrap();
        assert_eq!(scan.findings.first().unwrap().line, 3);
        assert_eq!(scan.bytes, text.len() as u64);
        assert_eq!(scan.sha256, crate::artifacts::sha256(text.as_bytes()));
        let spaced = format!("{}{token}", "\n".repeat(WINDOW + OVERLAP + 9));
        assert_eq!(
            super::scan(spaced.as_bytes())
                .unwrap()
                .findings
                .first()
                .unwrap()
                .line,
            (WINDOW + OVERLAP + 10) as u64
        );
    }
    #[test]
    fn mentions_samples_and_placeholders_pass() {
        let dash = "-".repeat(5);
        let key_id = ["AK", "IA"].concat() + &body(16, 22);
        let sample = ["AK", "IA"].concat() + "IOSFODNN7" + "EXAMPLE";
        let sample_secret = format!("{}/K7MDENG/{}EXAMPLEKEY", body(11, 23), body(10, 24));
        let safe = [
            // A key block's header alone, in code and in documentation.
            format!("const HEADER: &str = \"{dash}BEGIN PRIVATE KEY{dash}\";"),
            format!("{dash}BEGIN RSA PRIVATE KEY{dash}\n...\n{dash}END RSA PRIVATE KEY{dash}"),
            // Public keys and certificates hold no secret.
            format!("{dash}BEGIN PUBLIC KEY{dash}\n{}\n", body(64, 25)),
            format!("{dash}BEGIN CERTIFICATE{dash}\n{}\n", body(64, 26)),
            // An access key ID alone, and with a value of the wrong shape after it.
            format!("role {key_id} in the audit log"),
            format!("{key_id} {}", body(39, 27)),
            format!("{key_id} {}", body(41, 28)),
            // AWS's documented pair.
            format!("{sample}\n{sample_secret}\n"),
            // Prefixes too short to be keys, and vendor placeholders.
            ["gh", "p_"].concat() + "xxxx",
            ["sk-", "admin-"].concat() + "1234abcd",
            ["sk-", "ant-api03-"].concat() + "R2D...igAA",
            ["x", "ai-"].concat() + "...oYZ4",
            ["xo", "xb-"].concat() + "not-a-real-token-this-will-not-work",
            ["pk_", "live_"].concat() + &body(24, 29),
            ["sk_", "test_"].concat() + &body(24, 30),
            ["py", "pi-"].concat() + "NMIfyYncKcRAL" + "EXAMPLE...",
            ["AI", "za"].concat() + "...",
            // Inside a longer word, and an ordinary 40-character hash.
            format!("ask{}", ["-admin-"].concat() + &body(60, 31)),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709".into(),
        ];
        for (case, text) in safe.into_iter().enumerate() {
            assert_eq!(found(text.as_bytes()), Vec::<&str>::new(), "case {case}");
        }
    }
    #[test]
    fn a_sample_does_not_hide_a_real_value_inside_its_match() {
        let sample = ["AK", "IA"].concat() + "IOSFODNN7" + "EXAMPLE";
        let token = ["gh", "p_"].concat() + &body(36, 32);
        let text = format!("{sample} {token} {} ", body(40, 34));
        assert_eq!(found(text.as_bytes()), ["GitHub token"]);
    }
    #[test]
    fn a_report_is_capped_and_holds_only_source_line_and_rule() {
        let token = ["np", "m_"].concat() + &body(36, 33);
        let scan = scan(format!("{token}\n").repeat(SHOWN + 1).as_bytes()).unwrap();
        assert_eq!(
            lines("a.txt", &scan.findings).join("\n"),
            "a.txt:1: npm token\na.txt:2: npm token\na.txt:3: npm token\na.txt:4: npm token\na.txt:5: npm token\na.txt: and more"
        );
    }
}

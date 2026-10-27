use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use tiktoken_rs::CoreBPE;

pub fn estimate_request(body: &Value) -> Result<usize> {
    tokenizer()?.estimate(body, 200)
}
pub fn estimate_chat(system: &str, user: &str) -> Result<usize> {
    count_chat(&[("system", system), ("user", user)])
}
pub fn count_chat(messages: &[(&str, &str)]) -> Result<usize> {
    let tokenizer = tokenizer()?;
    Ok(messages
        .iter()
        .map(|(role, content)| 3 + tokenizer.count(role) + tokenizer.count(content))
        .sum::<usize>()
        + 3)
}
pub fn count_text(text: &str) -> Result<usize> {
    Ok(tokenizer()?.count(text))
}
fn tokenizer() -> Result<&'static Tokenizer> {
    static TOKENIZER: std::sync::OnceLock<Result<Tokenizer, String>> = std::sync::OnceLock::new();
    let tokenizer = TOKENIZER.get_or_init(|| Tokenizer::new().map_err(|error| error.to_string()));
    match tokenizer {
        Ok(tokenizer) => Ok(tokenizer),
        Err(error) => anyhow::bail!("Tokenizer initialisation failed: {error}"),
    }
}

// Match gpt-tokenizer 4.0.0's o200k_base split pattern and JavaScript whitespace.
const PATTERN: &str = r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+(?:'(?:[sS]|[dD]|[mM]|[tT]|[lL][lL]|[vV][eE]|[rR][eE]))?|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*(?:'(?:[sS]|[dD]|[mM]|[tT]|[lL][lL]|[vV][eE]|[rR][eE]))?|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n/]*|\s*[\r\n]+|\s+(?!\S)|\s+";
const WHITE: &str = r"[\t\n\x0B\x0C\r\x20\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const NON_WHITE: &str = r"[^\t\n\x0B\x0C\r\x20\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const SPECIALS: [&str; 8] = [
    "<|endoftext|>",
    "<|fim_prefix|>",
    "<|fim_middle|>",
    "<|fim_suffix|>",
    "<|im_start|>",
    "<|im_end|>",
    "<|im_sep|>",
    "<|endofprompt|>",
];

pub struct Tokenizer {
    core: CoreBPE,
    specials: Vec<Vec<u16>>,
}

impl Tokenizer {
    pub fn new() -> Result<Self> {
        let ranks = include_str!("../data/o200k_base.tiktoken");
        let parsed = ranks
            .lines()
            .map(|line| {
                let (encoded, rank) = line
                    .split_once(' ')
                    .context("Invalid checked tokenizer rank")?;
                Ok((STANDARD.decode(encoded)?, rank.parse()?))
            })
            .collect::<Result<Vec<_>>>()?;
        // The maintained byte lookup decodes leading BOM bytes before lookup;
        // that prevents BOM-prefixed ranks from being reached during merges.
        let encoder = parsed
            .into_iter()
            .filter(|(bytes, _)| !bytes.starts_with(&[239, 187, 191]))
            .collect();
        let special_tokens = SPECIALS
            .iter()
            .enumerate()
            .map(|(index, token)| (token.to_string(), 199999 + index as u32))
            .collect();
        let pattern = PATTERN
            .replace(
                r"[^\s\p{L}\p{N}]",
                &format!(r"[^{}\p{{L}}\p{{N}}]", &WHITE[1..WHITE.len() - 1]),
            )
            .replace(r"\s", WHITE)
            .replace(r"\S", NON_WHITE);
        let core = CoreBPE::new(encoder, special_tokens, &pattern)?;
        Ok(Self {
            core,
            specials: SPECIALS
                .iter()
                .map(|token| token.encode_utf16().collect())
                .collect(),
        })
    }

    pub fn count(&self, text: &str) -> usize {
        if !text.contains("<|") {
            return self.core.encode_ordinary(text).len();
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut start: usize = 0;
        let mut count = 0;
        loop {
            let found = self
                .specials
                .iter()
                .find(|special| {
                    units.get(start..start.saturating_add(special.len()))
                        == Some(special.as_slice())
                })
                .map(|special| (start, special.len()));
            // Preserve the maintained countNative/findNextSpecialToken offset
            // convention and sticky special-token match in UTF-16 for parity.
            let end = found.map(|(index, _)| index + start).unwrap_or(units.len());
            let chunk =
                String::from_utf16_lossy(&units[start.min(units.len())..end.min(units.len())]);
            count += self.core.encode_ordinary(&chunk).len();
            if let Some((_, length)) = found {
                count += 1;
                start = end + length;
            } else {
                return count;
            }
        }
    }

    pub fn estimate(&self, body: &Value, buffer: usize) -> Result<usize> {
        let mut parts = vec![];
        if let Some(instructions) = body["instructions"].as_str().filter(|s| !s.is_empty()) {
            parts.push(instructions.to_owned());
        }
        for turn in body["input"].as_array().into_iter().flatten() {
            for content in turn["content"].as_array().into_iter().flatten() {
                if let Some(text) = content["text"].as_str() {
                    parts.push(text.to_owned());
                }
            }
        }
        if body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
        {
            parts.push(serde_json::to_string(&body["tools"])?);
        }
        if !body["reasoning"].is_null() {
            parts.push(serde_json::to_string(&body["reasoning"])?);
        }
        if body["background"] == true {
            parts.push("background:true".into());
        }
        if body["store"] == true {
            parts.push("store:true".into());
        }
        let search_buffer = if body["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["type"] == "web_search_preview")
        }) {
            4000
        } else {
            0
        };
        Ok(self.count(&parts.join("\n")) + buffer + search_buffer)
    }
}

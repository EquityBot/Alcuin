//! JSON.stringify bytes for maintained remote delivery digests, including UTF-16 surrogates.
use anyhow::{Result, bail, ensure};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::{Value, value::RawValue};
use std::{collections::HashMap, fmt};

#[derive(PartialEq, Eq, Hash)]
struct Text(Vec<u8>); // serde_json's byte visitor retains lone surrogates as WTF-8.
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Bytes;
        impl<'de> Visitor<'de> for Bytes {
            type Value = Text;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON string")
            }
            fn visit_bytes<E: serde::de::Error>(
                self,
                bytes: &[u8],
            ) -> std::result::Result<Text, E> {
                Ok(Text(bytes.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(
                self,
                bytes: Vec<u8>,
            ) -> std::result::Result<Text, E> {
                Ok(Text(bytes))
            }
        }
        deserializer.deserialize_byte_buf(Bytes)
    }
}
struct Object<'a>(Vec<(Text, &'a RawValue)>);
impl<'de> Deserialize<'de> for Object<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Entries;
        impl<'de> Visitor<'de> for Entries {
            type Value = Object<'de>;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Object<'de>, A::Error> {
                let mut entries = vec![];
                let mut positions = HashMap::new();
                while let Some((key, value)) = map.next_entry::<Text, &'de RawValue>()? {
                    if let Some(&index) = positions.get(&key.0) {
                        entries[index] = (key, value);
                    } else {
                        positions.insert(key.0.clone(), entries.len());
                        entries.push((key, value));
                    }
                }
                Ok(Object(entries))
            }
        }
        deserializer.deserialize_map(Entries)
    }
}
fn array_index(key: &[u8]) -> Option<u32> {
    if key.is_empty()
        || key.len() > 10
        || key.len() > 1 && key[0] == b'0'
        || !key.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let index = std::str::from_utf8(key).ok()?.parse::<u32>().ok()?;
    (index < u32::MAX).then_some(index)
}
fn quote(bytes: &[u8], output: &mut Vec<u8>) {
    const HEX: &[u8] = b"0123456789abcdef";
    output.push(b'"');
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b'"' => output.extend_from_slice(b"\\\""),
            b'\\' => output.extend_from_slice(b"\\\\"),
            8 => output.extend_from_slice(b"\\b"),
            9 => output.extend_from_slice(b"\\t"),
            10 => output.extend_from_slice(b"\\n"),
            12 => output.extend_from_slice(b"\\f"),
            13 => output.extend_from_slice(b"\\r"),
            0..=31 => output.extend_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[(byte >> 4) as usize],
                HEX[(byte & 15) as usize],
            ]),
            0xed if at + 2 < bytes.len() && bytes[at + 1] >= 0xa0 => {
                let unit = ((byte as u16 & 15) << 12)
                    | ((bytes[at + 1] as u16 & 63) << 6)
                    | (bytes[at + 2] as u16 & 63);
                output.extend_from_slice(&[
                    b'\\',
                    b'u',
                    HEX[(unit >> 12) as usize],
                    HEX[((unit >> 8) & 15) as usize],
                    HEX[((unit >> 4) & 15) as usize],
                    HEX[(unit & 15) as usize],
                ]);
                at += 2;
            }
            _ => output.push(byte),
        }
        at += 1;
    }
    output.push(b'"');
}
fn emit(raw: &RawValue, depth: usize, output: &mut Vec<u8>) -> Result<()> {
    ensure!(depth <= 128, "Remote JSON exceeds the nesting bound");
    let text = raw.get();
    match text.as_bytes()[0] {
        b'"' => quote(&serde_json::from_str::<Text>(text)?.0, output),
        b'[' => {
            output.push(b'[');
            for (index, value) in serde_json::from_str::<Vec<&RawValue>>(text)?
                .iter()
                .enumerate()
            {
                if index > 0 {
                    output.push(b',');
                }
                emit(value, depth + 1, output)?;
            }
            output.push(b']');
        }
        b'{' => {
            let object: Object<'_> = serde_json::from_str(text)?;
            let mut ordered: Vec<_> = object.0.iter().enumerate().collect();
            ordered.sort_by_key(|(position, (key, _))| match array_index(&key.0) {
                Some(index) => (0, index as usize),
                None => (1, *position),
            });
            output.push(b'{');
            for (index, (_, (key, value))) in ordered.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                quote(&key.0, output);
                output.push(b':');
                emit(value, depth + 1, output)?;
            }
            output.push(b'}');
        }
        b'n' | b't' | b'f' => output.extend_from_slice(text.as_bytes()),
        b'-' | b'0'..=b'9' => {
            let number = text.parse::<f64>()?;
            if number.is_finite() {
                output.extend_from_slice(ryu_js::Buffer::new().format_finite(number).as_bytes());
            } else {
                output.extend_from_slice(b"null");
            }
        }
        _ => bail!("Invalid remote JSON value"),
    }
    Ok(())
}
pub fn stringify(input: &str) -> Result<Vec<u8>> {
    let raw: &RawValue = serde_json::from_str(input)?;
    let mut output = Vec::with_capacity(input.len());
    emit(raw, 0, &mut output)?;
    Ok(output)
}
pub fn result_digest(run_id: &str, result: &Value, artifacts: &Value) -> Result<String> {
    ensure!(artifacts.is_array(), "Remote descriptors must be an array");
    let raw = serde_json::to_string(
        &serde_json::json!({"runId":run_id,"result":result,"artifacts":artifacts}),
    )?;
    Ok(crate::artifacts::sha256(&stringify(&raw)?))
}
pub(crate) fn field<'a>(raw: &'a RawValue, name: &str) -> Result<Option<&'a RawValue>> {
    let Object(entries) = serde_json::from_str(raw.get())?;
    Ok(entries
        .into_iter()
        .find(|(key, _)| key.0 == name.as_bytes())
        .map(|(_, value)| value))
}
fn utf8(bytes: &[u8]) -> String {
    let mut output = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == 0xed && at + 2 < bytes.len() && bytes[at + 1] >= 0xa0 {
            output.extend_from_slice("\u{fffd}".as_bytes());
            at += 3;
        } else {
            output.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(output).expect("JSON text with surrogates replaced is UTF-8")
}
/// Native UTF-8 view for display and file writes. Never hash or forward this projection.
pub(crate) fn native_projection(raw: &RawValue) -> Result<Value> {
    fn project(raw: &RawValue, depth: usize) -> Result<Value> {
        ensure!(
            depth <= 128,
            "Remote JSON nesting exceeds its supported depth"
        );
        let text = raw.get();
        Ok(match text.as_bytes()[0] {
            b'"' => Value::String(utf8(&serde_json::from_str::<Text>(text)?.0)),
            b'[' => Value::Array(
                serde_json::from_str::<Vec<&RawValue>>(text)?
                    .into_iter()
                    .map(|value| project(value, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            b'{' => {
                let Object(entries) = serde_json::from_str(text)?;
                let mut object = serde_json::Map::new();
                for (key, value) in entries {
                    object.insert(utf8(&key.0), project(value, depth + 1)?);
                }
                Value::Object(object)
            }
            b'-' | b'0'..=b'9' => serde_json::from_slice(&stringify(text)?)?,
            _ => serde_json::from_str(text)?,
        })
    }
    project(raw, 0)
}
pub(crate) fn result_digest_raw(
    run_id: &str,
    result: &RawValue,
    artifacts: &[&RawValue],
) -> Result<String> {
    let descriptors = artifacts
        .iter()
        .map(|artifact| {
            field(artifact, "descriptor").map(|value| value.map_or("null", RawValue::get))
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let envelope = format!(
        "{{\"runId\":{},\"result\":{},\"artifacts\":[{}]}}",
        serde_json::to_string(run_id)?,
        result.get(),
        descriptors
    );
    Ok(crate::artifacts::sha256(&stringify(&envelope)?))
}
/// JSON numbers have JavaScript binary64 identity even when Rust stored them as 456.0.
pub(crate) fn deep_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .zip(right.as_f64())
            .is_some_and(|(left, right)| left.to_bits() == right.to_bits()),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| deep_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, left)| right.get(key).is_some_and(|right| deep_equal(left, right)))
        }
        _ => left == right,
    }
}
pub(crate) fn optional_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => deep_equal(left, right),
        (None, None) => true,
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synthetic_native_javascript_corpus_matches_every_byte_and_digest() {
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/remote-json-reference.json")).unwrap();
        let rows = fixture["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 269);
        for row in rows {
            let bytes = stringify(row["input"].as_str().unwrap()).unwrap();
            assert_eq!(
                bytes,
                row["expected"].as_str().unwrap().as_bytes(),
                "{}",
                row["input"]
            );
            assert_eq!(
                crate::artifacts::sha256(&bytes),
                row["sha256"],
                "{}",
                row["input"]
            );
        }
        let raw: &RawValue = serde_json::from_str(
            r#"{"answerText":"\ud800","answerMarkdown":"\udfff","\ud800":"PUBLIC preserved key"}"#,
        )
        .unwrap();
        assert_eq!(
            result_digest_raw("11111111-1111-4111-8111-111111111111", raw, &[]).unwrap(),
            fixture["surrogateResultDigest"]
        );
    }
    #[test]
    fn binary64_number_spelling_matches_javascript_thresholds_and_rounding() {
        for (input, expected) in [
            ("1.0", "1"),
            ("-0", "0"),
            ("1e-7", "1e-7"),
            ("1e-6", "0.000001"),
            ("1e20", "100000000000000000000"),
            ("1e21", "1e+21"),
            ("9007199254740993", "9007199254740992"),
            ("1e400", "null"),
            ("5e-324", "5e-324"),
        ] {
            assert_eq!(
                String::from_utf8(stringify(input).unwrap()).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn object_order_and_duplicate_keys_follow_javascript_property_rules() {
        let input = r#"{"z":1,"10":2,"2":3,"01":4,"4294967295":5,"4294967294":6,"z":7,"\u0032":8}"#;
        assert_eq!(
            stringify(input).unwrap(),
            br#"{"2":8,"10":2,"4294967294":6,"z":7,"01":4,"4294967295":5}"#
        );
    }
    #[test]
    fn unicode_surrogates_keys_and_control_escapes_remain_lossless() {
        let input = r#"{"\ud800":"\udfff","emoji":"\ud83d\ude00","slash":"\/","control":"\u0000\t\r\n","separator":"\u2028"}"#;
        let expected = "{\"\\ud800\":\"\\udfff\",\"emoji\":\"😀\",\"slash\":\"/\",\"control\":\"\\u0000\\t\\r\\n\",\"separator\":\"\u{2028}\"}";
        assert_eq!(stringify(input).unwrap(), expected.as_bytes());
    }
    #[test]
    fn invalid_json_and_excessive_nesting_cannot_produce_delivery_digests() {
        for input in ["NaN", "Infinity", "01", "{\"x\":1,}", "\"\\uZZZZ\""] {
            assert!(stringify(input).is_err());
        }
        assert!(stringify(&format!("{}0{}", "[".repeat(130), "]".repeat(130))).is_err());
    }
}

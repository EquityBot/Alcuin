//! Responses SSE framing and accumulation. A clean EOF returns the latest snapshot;
//! transport failures never turn a partial response into a successful result.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

#[derive(Debug)]
pub(crate) struct Event {
    pub kind: String,
    pub data: String,
}

/// One provider JSON value, framed or not, stays within the non-streamed body limit.
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
#[derive(Default)]
pub(crate) struct SseDecoder {
    line: Vec<u8>,
    after_cr: bool,
    first_line: bool,
    started: bool,
    kind: String,
    data: Vec<String>,
    data_bytes: usize,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>> {
        let mut events = vec![];
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    self.line(&mut events)?;
                    self.after_cr = byte == b'\r';
                }
                _ => {
                    self.line.push(byte);
                    ensure!(
                        self.line.len() + self.data_bytes <= MAX_EVENT_BYTES,
                        "Provider stream event exceeds the 8 MiB limit."
                    );
                }
            }
        }
        Ok(events)
    }
    pub fn finish(&mut self) -> Result<Vec<Event>> {
        let mut events = vec![];
        if !self.line.is_empty() {
            self.line(&mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }
    fn line(&mut self, events: &mut Vec<Event>) -> Result<()> {
        let bytes = std::mem::take(&mut self.line);
        let mut line =
            std::str::from_utf8(&bytes).context("Provider stream contains invalid UTF-8")?;
        if !self.started {
            self.first_line = true;
            self.started = true;
        }
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            self.dispatch(events);
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.kind = value.into(),
                "data" => {
                    self.data_bytes += value.len() + 1;
                    self.data.push(value.into());
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn dispatch(&mut self, events: &mut Vec<Event>) {
        self.data_bytes = 0;
        if !self.data.is_empty() {
            events.push(Event {
                kind: std::mem::take(&mut self.kind),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        } else {
            self.kind.clear();
        }
    }
}

#[derive(Default)]
pub(crate) struct ResponseAccumulator {
    pub snapshot: Option<Value>,
}
impl ResponseAccumulator {
    pub fn event(&mut self, event: &Value) -> Result<Option<String>> {
        let kind = event["type"]
            .as_str()
            .context("Provider event has no type")?;
        if kind == "error" {
            bail!(
                "{}",
                event["message"].as_str().unwrap_or("Provider stream error")
            );
        }
        if kind == "keepalive" {
            return Ok(None);
        }
        if matches!(
            kind,
            "response.created"
                | "response.queued"
                | "response.in_progress"
                | "response.completed"
                | "response.failed"
                | "response.incomplete"
        ) {
            let next = event["response"].clone();
            ensure!(next.is_object(), "Provider lifecycle event has no response");
            if let Some(previous) = &self.snapshot {
                ensure!(
                    previous["id"] == next["id"],
                    "Provider stream response identity changed"
                );
            } else {
                ensure!(
                    kind == "response.created",
                    "Provider stream did not begin with response.created"
                );
            }
            validate_output(&next)?;
            self.snapshot = Some(next);
            self.canonical_text();
            return Ok(None);
        }
        let snapshot = self
            .snapshot
            .as_mut()
            .context("Provider stream did not begin with response.created")?;
        let mut delta = None;
        match kind {
            "response.output_item.added" => {
                let index = index(event, "output_index")?;
                let outputs = snapshot["output"]
                    .as_array_mut()
                    .context("Response has no output array")?;
                ensure!(index == outputs.len(), "Missing output at index {index}");
                let item = &event["item"];
                validate_item(item)?;
                ensure!(
                    !outputs.iter().any(|existing| same_identity(existing, item)),
                    "Duplicate streamed output identity"
                );
                outputs.push(item.clone());
            }
            "response.output_item.done" => {
                let output = output(snapshot, event)?;
                let replacement = &event["item"];
                validate_item(replacement)?;
                ensure!(
                    output["id"] == replacement["id"]
                        && output["type"] == replacement["type"]
                        && output["call_id"] == replacement["call_id"],
                    "Completed output identity changed"
                );
                *output = replacement.clone();
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let output = scoped_output(snapshot, event)?;
                let summary = kind.starts_with("response.reasoning_summary");
                let field = if summary { "summary" } else { "content" };
                let index = index(
                    event,
                    if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?;
                if output.get(field).is_none() && kind.ends_with(".added") {
                    output[field] = json!([]);
                }
                let parts = output[field]
                    .as_array_mut()
                    .context("Output has no content array")?;
                ensure!(
                    event["part"].is_object(),
                    "Provider event has no content part"
                );
                if kind.ends_with(".added") {
                    ensure!(index == parts.len(), "Missing content at index {index}");
                    parts.push(event["part"].clone());
                } else {
                    *parts.get_mut(index).context("Missing completed content")? =
                        event["part"].clone();
                }
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => {
                let output = scoped_output(snapshot, event)?;
                let summary = kind.starts_with("response.reasoning_summary");
                let index = index(
                    event,
                    if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?;
                let part = output[if summary { "summary" } else { "content" }]
                    .as_array_mut()
                    .and_then(|parts| parts.get_mut(index))
                    .context("Missing streamed content")?;
                let expected = if kind.starts_with("response.refusal") {
                    "refusal"
                } else if kind.starts_with("response.output_text") {
                    "output_text"
                } else if summary {
                    "summary_text"
                } else {
                    "reasoning_text"
                };
                ensure!(part["type"] == expected, "Streamed content type changed");
                let field = if expected == "refusal" {
                    "refusal"
                } else {
                    "text"
                };
                if kind.ends_with(".delta") {
                    let text = event["delta"]
                        .as_str()
                        .context("Text delta is not a string")?;
                    let existing = part[field]
                        .as_str()
                        .context("Streamed content is not text")?;
                    part[field] = json!(format!("{existing}{text}"));
                    if expected == "output_text" {
                        delta = Some(text.into());
                    }
                } else {
                    ensure!(event[field].is_string(), "Completed content is not text");
                    part[field] = event[field].clone();
                }
            }
            "response.output_text.annotation.added" => {
                let output = scoped_output(snapshot, event)?;
                let content_index = index(event, "content_index")?;
                let part = output["content"]
                    .as_array_mut()
                    .and_then(|parts| parts.get_mut(content_index))
                    .context("Missing annotation content")?;
                ensure!(
                    part["type"] == "output_text",
                    "Annotation content type changed"
                );
                let annotation_index = index(event, "annotation_index")?;
                let annotations = part["annotations"]
                    .as_array_mut()
                    .context("Missing annotations")?;
                ensure!(
                    annotation_index <= annotations.len(),
                    "Missing annotation index"
                );
                if annotation_index == annotations.len() {
                    annotations.push(event["annotation"].clone());
                } else {
                    annotations[annotation_index] = event["annotation"].clone();
                }
            }
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
            | "response.mcp_call_arguments.delta"
            | "response.mcp_call_arguments.done"
            | "response.code_interpreter_call_code.delta"
            | "response.code_interpreter_call_code.done" => {
                let output = scoped_output(snapshot, event)?;
                let field = if kind.starts_with("response.custom_tool") {
                    "input"
                } else if kind.starts_with("response.code_interpreter") {
                    "code"
                } else {
                    "arguments"
                };
                if kind.ends_with(".delta") {
                    let text = event["delta"]
                        .as_str()
                        .context("Tool delta is not a string")?;
                    output[field] =
                        json!(format!("{}{text}", output[field].as_str().unwrap_or("")));
                } else {
                    ensure!(event[field].is_string(), "Completed tool input is not text");
                    output[field] = event[field].clone();
                }
            }
            _ => {
                // Non-text events remain in the final lifecycle snapshot. Status events
                // update the in-flight item so clean EOF also retains its latest state.
                if let Some(status) = kind.rsplit('.').next().filter(|status| {
                    matches!(
                        *status,
                        "in_progress"
                            | "searching"
                            | "interpreting"
                            | "generating"
                            | "completed"
                            | "failed"
                    )
                }) && event.get("output_index").is_some()
                {
                    let output = output(snapshot, event)?;
                    if let Some(item_id) = event.get("item_id") {
                        ensure!(output["id"] == *item_id, "Provider item identity changed");
                    }
                    output["status"] = json!(status);
                }
            }
        }
        self.canonical_text();
        Ok(delta)
    }
    fn canonical_text(&mut self) {
        if let Some(snapshot) = &mut self.snapshot {
            let text: String = snapshot["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "message")
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .filter(|part| part["type"] == "output_text")
                .filter_map(|part| part["text"].as_str())
                .collect();
            snapshot["output_text"] = json!(text);
        }
    }
    pub fn finish(self) -> Result<Value> {
        self.snapshot
            .context("Stream ended without producing a Response")
    }
}
fn index(event: &Value, field: &str) -> Result<usize> {
    usize::try_from(
        event[field]
            .as_u64()
            .context("Invalid streamed array index")?,
    )
    .context("Streamed array index is too large")
}
fn output<'a>(snapshot: &'a mut Value, event: &Value) -> Result<&'a mut Value> {
    let index = index(event, "output_index")?;
    snapshot["output"]
        .as_array_mut()
        .and_then(|items| items.get_mut(index))
        .context("Missing streamed output")
}
fn scoped_output<'a>(snapshot: &'a mut Value, event: &Value) -> Result<&'a mut Value> {
    let output = output(snapshot, event)?;
    let id = event["item_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("Missing streamed item identity")?;
    ensure!(output["id"] == id, "Provider item identity changed");
    let kind = event["type"].as_str().unwrap_or_default();
    let expected = if kind.starts_with("response.reasoning_") {
        "reasoning"
    } else if kind.starts_with("response.function_call") {
        "function_call"
    } else if kind.starts_with("response.custom_tool") {
        "custom_tool_call"
    } else if kind.starts_with("response.mcp_call") {
        "mcp_call"
    } else if kind.starts_with("response.code_interpreter") {
        "code_interpreter_call"
    } else if kind.starts_with("response.content_part") {
        output["type"].as_str().unwrap_or_default()
    } else {
        "message"
    };
    ensure!(output["type"] == expected, "Provider output type changed");
    Ok(output)
}
fn validate_item(item: &Value) -> Result<()> {
    let kind = item["type"].as_str().context("Missing output item type")?;
    ensure!(
        matches!(kind, "function_call" | "custom_tool_call")
            || item["id"].as_str().is_some_and(|id| !id.is_empty()),
        "Missing output identity"
    );
    if let Some(id) = item.get("id") {
        ensure!(
            id.as_str().is_some_and(|id| !id.is_empty()),
            "Invalid output identity"
        );
    }
    if matches!(
        kind,
        "function_call" | "custom_tool_call" | "shell_call" | "shell_call_output"
    ) {
        ensure!(
            item["call_id"].as_str().is_some_and(|id| !id.is_empty()),
            "Missing tool call identity"
        );
    }
    Ok(())
}
fn same_identity(left: &Value, right: &Value) -> bool {
    (right.get("id").is_some() && left["id"] == right["id"])
        || (right.get("call_id").is_some()
            && left["type"] == right["type"]
            && left["call_id"] == right["call_id"])
}
fn validate_output(response: &Value) -> Result<()> {
    let items = response["output"]
        .as_array()
        .context("Response has no output array")?;
    for (index, item) in items.iter().enumerate() {
        validate_item(item)?;
        ensure!(
            !items[..index]
                .iter()
                .any(|other| same_identity(other, item)),
            "Duplicate response output identity"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_preserves_utf8_multiline_data_and_all_line_endings_at_every_split() {
        let wire = "\u{feff}:comment\r\nevent: response.created\rdata: {\"type\":\r\ndata: \"response.created\",\"text\":\"PUBLIC café 🦀\"}\r\n\r\nevent: tail\ndata: EOF without blank line".as_bytes();
        for split in 0..=wire.len() {
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&wire[..split]).unwrap();
            events.extend(decoder.push(&wire[split..]).unwrap());
            events.extend(decoder.finish().unwrap());
            assert_eq!(events.len(), 2, "split {split}");
            assert_eq!(events[0].kind, "response.created");
            let value: Value = serde_json::from_str(&events[0].data).unwrap();
            assert_eq!(value["text"], "PUBLIC café 🦀");
            assert_eq!(events[1].data, "EOF without blank line");
        }
        let mut decoder = SseDecoder::default();
        let mut count = 0;
        for byte in wire {
            count += decoder.push(&[*byte]).unwrap().len();
        }
        count += decoder.finish().unwrap().len();
        assert_eq!(count, 2);
        assert!(SseDecoder::default().push(b"data: \xff\n").is_err());
    }

    fn created() -> Value {
        json!({"type":"response.created","response":{"id":"resp_public","status":"in_progress","output":[]}})
    }
    fn add(accumulator: &mut ResponseAccumulator, output_index: usize, id: &str) {
        accumulator.event(&json!({"type":"response.output_item.added","output_index":output_index,"item":{"id":id,"type":"message","content":[]}})).unwrap();
        accumulator.event(&json!({"type":"response.content_part.added","item_id":id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})).unwrap();
    }
    #[test]
    fn indexed_deltas_final_replacements_and_clean_in_progress_eof() {
        let mut accumulator = ResponseAccumulator::default();
        accumulator.event(&created()).unwrap();
        add(&mut accumulator, 0, "msg_a");
        add(&mut accumulator, 1, "msg_b");
        for (index, id, text) in [
            (1, "msg_b", "second"),
            (0, "msg_a", "PUBLIC first "),
            (1, "msg_b", " tail"),
        ] {
            assert_eq!(accumulator.event(&json!({"type":"response.output_text.delta","item_id":id,"output_index":index,"content_index":0,"delta":text})).unwrap(),Some(text.into()));
        }
        assert_eq!(
            accumulator.snapshot.as_ref().unwrap()["output_text"],
            "PUBLIC first second tail"
        );
        accumulator.event(&json!({"type":"response.output_text.done","item_id":"msg_a","output_index":0,"content_index":0,"text":"corrected "})).unwrap();
        let result = accumulator.finish().unwrap();
        assert_eq!(result["output_text"], "corrected second tail");
        assert_eq!(result["status"], "in_progress");
    }
    #[test]
    fn stream_refuses_identity_changes_missing_items_and_error_events() {
        for event in [
            json!({"type":"response.output_text.delta","item_id":"other","output_index":0,"content_index":0,"delta":"wrong"}),
            json!({"type":"response.output_text.delta","item_id":"msg_a","output_index":5,"content_index":0,"delta":"wrong"}),
            json!({"type":"response.completed","response":{"id":"resp_other","output":[]}}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_a","type":"message","content":[]}}),
            json!({"type":"error","message":"PUBLIC provider error"}),
        ] {
            let mut accumulator = ResponseAccumulator::default();
            accumulator.event(&created()).unwrap();
            add(&mut accumulator, 0, "msg_a");
            assert!(accumulator.event(&event).is_err(), "{event}");
            assert_eq!(accumulator.snapshot.as_ref().unwrap()["id"], "resp_public");
        }
        assert!(ResponseAccumulator::default().finish().is_err());
    }
}

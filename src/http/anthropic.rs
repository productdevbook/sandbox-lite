//! The Anthropic side of the chat: framing the upstream `text/event-stream` and putting the
//! streamed deltas back together as the content blocks the next request has to echo.

use serde_json::{Value, json};

const MAX_BLOCKS: usize = 256;

/// Splits a `text/event-stream` byte stream into the `data:` payload of each event. Bytes are
/// buffered rather than decoded on arrival: a chunk boundary can fall inside a UTF-8 sequence.
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
    data: String,
}

impl Frames {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data));
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        out
    }
}

/// What the caller should forward to its own client as the stream arrives.
#[derive(Debug, PartialEq)]
pub enum Emit {
    Text(String),
    Tool { name: String, input: Value },
    Error(String),
}

#[derive(Default, Clone)]
struct Build {
    value: Value,
    json: String,
}

/// One assistant turn, rebuilt from its stream events.
#[derive(Default)]
pub struct Reply {
    blocks: Vec<Build>,
    stop_reason: Option<String>,
    error: Option<String>,
    done: bool,
}

impl Reply {
    pub fn event(&mut self, v: &Value) -> Option<Emit> {
        match v["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let block = v["content_block"].clone();
                let slot = self.slot(v)?;
                slot.value = block;
                slot.json.clear();
                None
            }
            "content_block_delta" => {
                let delta = v["delta"].clone();
                let text = if delta["type"] == "text_delta" { delta["text"].as_str().map(str::to_string) } else { None };
                let slot = self.slot(v)?;
                if let Some(part) = delta["partial_json"].as_str() {
                    slot.json.push_str(part);
                } else if let (Some(fields), Some(block)) = (delta.as_object(), slot.value.as_object_mut()) {
                    // every other delta appends to the block field of the same name: text, thinking, signature
                    for (k, value) in fields.iter().filter(|(k, _)| *k != "type") {
                        let Some(part) = value.as_str() else { continue };
                        let grown = format!("{}{part}", block.get(k).and_then(Value::as_str).unwrap_or(""));
                        block.insert(k.clone(), Value::String(grown));
                    }
                }
                text.map(Emit::Text)
            }
            "content_block_stop" => {
                let slot = self.slot(v)?;
                if slot.value["type"] == "tool_use" {
                    let input = serde_json::from_str(&slot.json).unwrap_or_else(|_| json!({}));
                    if let Some(block) = slot.value.as_object_mut() {
                        block.insert("input".into(), input);
                    }
                    return Some(Emit::Tool {
                        name: slot.value["name"].as_str().unwrap_or("").to_string(),
                        input: slot.value["input"].clone(),
                    });
                }
                None
            }
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                None
            }
            "message_stop" => {
                self.done = true;
                None
            }
            "error" => {
                let message = v["error"]["message"].as_str().unwrap_or("upstream stream error").to_string();
                self.error = Some(message.clone());
                Some(Emit::Error(message))
            }
            _ => None,
        }
    }

    fn slot(&mut self, v: &Value) -> Option<&mut Build> {
        let i = v["index"].as_u64().unwrap_or(0) as usize;
        if i >= MAX_BLOCKS {
            return None;
        }
        if self.blocks.len() <= i {
            self.blocks.resize(i + 1, Build::default());
        }
        Some(&mut self.blocks[i])
    }

    /// The assistant turn as the API returned it, to be sent back as the next `assistant` message.
    pub fn content(&self) -> Vec<Value> {
        self.blocks.iter().map(|b| b.value.clone()).filter(|v| !v.is_null()).collect()
    }

    pub fn text(&self) -> String {
        let parts: Vec<&str> = self.blocks.iter().filter(|b| b.value["type"] == "text").filter_map(|b| b.value["text"].as_str()).collect();
        parts.join("\n")
    }

    /// `(id, name, input)` of every tool the turn asked for, in order.
    pub fn tool_calls(&self) -> Vec<(String, String, Value)> {
        self.blocks
            .iter()
            .filter(|b| b.value["type"] == "tool_use")
            .map(|b| {
                (
                    b.value["id"].as_str().unwrap_or("").to_string(),
                    b.value["name"].as_str().unwrap_or("").to_string(),
                    b.value["input"].clone(),
                )
            })
            .collect()
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn done(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::{Emit, Frames, Reply};
    use serde_json::{Value, json};

    /// One recorded turn: a sentence, a `write_file` call whose input arrives in four fragments,
    /// and the trailing message events. Line endings are CRLF, as they come off the wire.
    const RECORDED: &str = concat!(
        "event: message_start\r\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude\"}}\r\n",
        "\r\n",
        "event: content_block_start\r\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\r\n",
        "\r\n",
        ": ping\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Making \"}}\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"the change.\"}}\r\n",
        "\r\n",
        "event: content_block_stop\r\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\r\n",
        "\r\n",
        "event: content_block_start\r\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\",\"input\":{}}}\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"src/pages/\"}}\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"index.astro\\\",\"}}\r\n",
        "\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"content\\\":\\\"hi\\\"}\"}}\r\n",
        "\r\n",
        "event: content_block_stop\r\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\r\n",
        "\r\n",
        "event: message_delta\r\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\r\n",
        "\r\n",
        "event: message_stop\r\n",
        "data: {\"type\":\"message_stop\"}\r\n",
        "\r\n",
    );

    /// Feeds the recording through the parser in `size`-byte chunks, as reqwest would.
    fn replay(text: &str, size: usize) -> (Reply, Vec<Emit>) {
        let mut frames = Frames::default();
        let mut reply = Reply::default();
        let mut emitted = Vec::new();
        for chunk in text.as_bytes().chunks(size) {
            for data in frames.push(chunk) {
                let v: Value = serde_json::from_str(&data).expect("every data payload is JSON");
                emitted.extend(reply.event(&v));
            }
        }
        (reply, emitted)
    }

    #[test]
    fn stream_events_rebuild_the_assistant_content_blocks() {
        for size in [1, 7, 64, 4096] {
            let (reply, emitted) = replay(RECORDED, size);
            assert_eq!(
                reply.content(),
                vec![
                    json!({"type":"text","text":"Making the change."}),
                    json!({"type":"tool_use","id":"toolu_1","name":"write_file","input":{"path":"src/pages/index.astro","content":"hi"}}),
                ],
                "chunk size {size}"
            );
            assert_eq!(reply.text(), "Making the change.");
            assert_eq!(reply.stop_reason(), Some("tool_use"));
            assert!(reply.done() && reply.error().is_none());
            assert_eq!(reply.tool_calls().len(), 1);
            assert_eq!(reply.tool_calls()[0].1, "write_file");
            assert_eq!(
                emitted,
                vec![
                    Emit::Text("Making ".into()),
                    Emit::Text("the change.".into()),
                    Emit::Tool { name: "write_file".into(), input: json!({"path":"src/pages/index.astro","content":"hi"}) },
                ],
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn a_tool_call_with_no_input_deltas_becomes_an_empty_object() {
        let mut reply = Reply::default();
        reply.event(&json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"list_files"}}));
        let emit = reply.event(&json!({"type":"content_block_stop","index":0}));
        assert_eq!(emit, Some(Emit::Tool { name: "list_files".into(), input: json!({}) }));
        assert_eq!(reply.content(), vec![json!({"type":"tool_use","id":"t","name":"list_files","input":{}})]);
    }

    #[test]
    fn unknown_block_kinds_keep_their_fields() {
        let mut reply = Reply::default();
        reply.event(&json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}));
        reply.event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}));
        reply.event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}));
        reply.event(&json!({"type":"content_block_stop","index":0}));
        assert_eq!(reply.content(), vec![json!({"type":"thinking","thinking":"hm","signature":"sig"})]);
        assert_eq!(reply.text(), "");
    }

    #[test]
    fn an_error_event_is_reported_and_not_mistaken_for_a_reply() {
        let mut reply = Reply::default();
        let emit = reply.event(&json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}));
        assert_eq!(emit, Some(Emit::Error("Overloaded".into())));
        assert_eq!(reply.error(), Some("Overloaded"));
        assert!(!reply.done());
    }

    #[test]
    fn absurd_block_indexes_are_ignored() {
        let mut reply = Reply::default();
        reply.event(&json!({"type":"content_block_start","index":9_999_999,"content_block":{"type":"text","text":"x"}}));
        assert!(reply.content().is_empty());
    }

    #[test]
    fn frames_join_multiple_data_lines_and_skip_comments() {
        let mut frames = Frames::default();
        assert!(frames.push(b": keep-alive\n\n").is_empty());
        assert_eq!(frames.push(b"event: x\ndata: a\ndata: b\n\n"), vec!["a\nb"]);
        assert!(frames.push(b"data: par").is_empty());
        assert_eq!(frames.push(b"tial\n\n"), vec!["partial"]);
    }
}

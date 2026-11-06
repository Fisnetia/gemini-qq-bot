//! Salvage pipeline for agent output that the strict parser cannot use.
//!
//! The strict path ([`crate::agent::contract::validate`]) is deliberately
//! all-or-nothing: one unusable byte and the whole turn is lost, because the
//! probe and the other callers depend on that exact semantics. This module is the
//! *bypass* that runs only after the strict path has already failed, and it never
//! relaxes what that path accepts.
//!
//! Everything here is deterministic — no model calls, no clock, no randomness —
//! so every layer is unit-testable in isolation:
//!
//! * **L1 span** ([`spans`], [`envelope_candidates`]) — find every top-level
//!   `{…}` span with a string-aware scanner, so prose around the answer, Markdown
//!   fences, log lines and several JSON documents in one buffer stop being fatal.
//! * **L2 repair** ([`repair_json`]) — drop trailing commas, escape bare control
//!   characters inside strings, and close a document truncated mid-way. String
//!   membership is decided by a state machine, never by a regex.
//! * **L4 text** ([`plain_text_reply`]) — a plain-text answer that is obviously a
//!   reply and obviously not a mangled contract.
//!
//! L3 (section-by-section validation) lives in [`crate::agent::contract`] because
//! it needs the same per-item validators the strict path uses.

use serde_json::Value;

/// Which layer produced a usable answer. `None` means the strict path passed and
/// nothing had to be recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SalvageLayer {
    /// Strict validation passed byte for byte.
    #[default]
    None,
    /// A JSON document was cut out of a larger buffer.
    Span,
    /// A JSON document had to be repaired before it parsed.
    Repair,
    /// The document parsed but individual sections were dropped.
    Partial,
    /// Nothing JSON-like was usable; the whole output is the reply.
    Text,
}

impl SalvageLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            SalvageLayer::None => "none",
            SalvageLayer::Span => "span",
            SalvageLayer::Repair => "repair",
            SalvageLayer::Partial => "partial",
            SalvageLayer::Text => "text",
        }
    }
}

/// What one salvage pass had to give up, carried to the audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalvageReport {
    pub layer: SalvageLayer,
    /// `(section, reason, count)` — one entry per distinct drop, in encounter order.
    pub dropped: Vec<(&'static str, &'static str, usize)>,
    /// Values that were clamped or truncated instead of dropped.
    pub clamped: usize,
}

impl SalvageReport {
    pub fn new(layer: SalvageLayer) -> Self {
        Self {
            layer,
            ..Self::default()
        }
    }

    /// Record one dropped item. Identical `(section, reason)` pairs accumulate.
    pub fn note(&mut self, section: &'static str, reason: &'static str) {
        match self
            .dropped
            .iter_mut()
            .find(|(s, r, _)| *s == section && *r == reason)
        {
            Some(entry) => entry.2 += 1,
            None => self.dropped.push((section, reason, 1)),
        }
    }

    pub fn dropped_total(&self) -> usize {
        self.dropped.iter().map(|(_, _, count)| *count).sum()
    }

    /// True when nothing was recovered and nothing was lost (the strict path ran).
    pub fn is_clean(&self) -> bool {
        self.layer == SalvageLayer::None
    }

    /// Compact form for a single audit row: `span.d2.c0`.
    pub fn audit_suffix(&self) -> String {
        format!(
            "{}.d{}.c{}",
            self.layer.as_str(),
            self.dropped_total(),
            self.clamped
        )
    }

    /// Detail for the log line: `reply_parts/forbidden×2, memory_ops/unknown_ref×1`.
    pub fn detail(&self) -> String {
        if self.dropped.is_empty() && self.clamped == 0 {
            return self.layer.as_str().to_string();
        }
        let mut parts: Vec<String> = self
            .dropped
            .iter()
            .map(|(section, reason, count)| format!("{section}/{reason}×{count}"))
            .collect();
        if self.clamped > 0 {
            parts.push(format!("clamped×{}", self.clamped));
        }
        format!("{}: {}", self.layer.as_str(), parts.join(", "))
    }
}

/// One parsed candidate document plus the layer that produced it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub value: Value,
    pub layer: SalvageLayer,
}

/// Enumerate every top-level `{…}` span in `raw`.
///
/// The scanner tracks string membership (including `\"` escapes), so braces,
/// brackets and quotes *inside* strings never affect the depth. Nested objects
/// stay inside their parent span; only the outermost pairs are returned. A span
/// left open at the end of the buffer is returned as its own (truncated) span so
/// the repair layer can close it.
pub fn spans(raw: &str) -> Vec<&str> {
    let bytes = raw.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }

    // An unterminated object is still worth repairing.
    if depth > 0 {
        out.push(&raw[start..]);
    }
    out
}

/// Unwrap one of the four envelope shapes the agent CLI is known to produce.
///
/// Returns `None` when the value is not recognisable as an envelope at all — that
/// distinction is what keeps a stray `{"status":"ok"}` from being mistaken for a
/// contract.
pub fn unwrap_envelope(value: &Value) -> Option<Value> {
    if value.get("should_reply").is_some() {
        return Some(value.clone());
    }
    if let Some(result) = value.get("result")
        && result.is_object()
    {
        return Some(result.clone());
    }
    for key in ["output", "text", "response"] {
        let Some(text) = value.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_str::<Value>(text.trim())
            && parsed.is_object()
        {
            return Some(parsed);
        }
    }
    None
}

/// Candidates recognised as one of the known envelopes (span, then repair).
///
/// This is what the extraction layer uses: an unknown top-level object stays
/// unknown instead of being guessed into a contract.
pub fn envelope_candidates(raw: &str) -> Vec<Candidate> {
    collect(raw, false)
}

/// Candidates for section-by-section validation, including documents that were
/// not recognisable as an envelope (the caller inspects them field by field, so
/// no guess is made here either).
pub fn candidates(raw: &str) -> Vec<Candidate> {
    collect(raw, true)
}

fn collect(raw: &str, allow_identity: bool) -> Vec<Candidate> {
    for repair in [false, true] {
        let layer = if repair {
            SalvageLayer::Repair
        } else {
            SalvageLayer::Span
        };
        let mut found: Vec<Candidate> = Vec::new();
        for value in parse_texts(raw, repair) {
            let inner = match unwrap_envelope(&value) {
                Some(inner) => Some(inner),
                None if allow_identity && value.is_object() => Some(value.clone()),
                None => None,
            };
            if let Some(inner) = inner
                && inner.is_object()
            {
                found.push(Candidate {
                    value: inner,
                    layer,
                });
            }
        }
        if !found.is_empty() {
            // A document that actually answers is worth more than one that merely
            // parses: prefer anything carrying `should_reply`.
            found.sort_by_key(|candidate| candidate.value.get("should_reply").is_none());
            return found;
        }
    }
    Vec::new()
}

fn parse_texts(raw: &str, repair: bool) -> Vec<Value> {
    let trimmed = raw.trim();
    let mut out: Vec<Value> = Vec::new();
    let mut push = |text: &str| {
        if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
            out.push(value);
        }
    };

    // The whole buffer first: a document the strict path rejected on *semantics*
    // still parses, and L3 can do something with it.
    if !repair {
        push(trimmed);
    } else if let Some(fixed) = repair_json(trimmed) {
        push(&fixed);
    }

    for span in spans(trimmed) {
        if repair {
            if let Some(fixed) = repair_json(span) {
                push(&fixed);
            }
        } else {
            push(span);
        }
    }
    out
}

/// Deterministically repair a JSON document so it parses again.
///
/// Layers apply in order and each one is a state machine over string membership:
/// bare control characters inside strings get escaped, trailing commas before a
/// `}` or `]` are dropped, and a truncated document is closed by dropping the
/// incomplete tail element and appending the missing closers in stack order.
pub fn repair_json(span: &str) -> Option<String> {
    let span = span.trim();
    if span.is_empty() {
        return None;
    }
    if serde_json::from_str::<Value>(span).is_ok() {
        return Some(span.to_string());
    }
    let escaped = escape_bare_controls(span);
    let cleaned = drop_trailing_commas(&escaped);
    if serde_json::from_str::<Value>(&cleaned).is_ok() {
        return Some(cleaned);
    }
    close_truncated(&cleaned)
}

/// Escape raw control characters that appear *inside* a JSON string. A model
/// that emits a real newline inside a string produces invalid JSON; the intent is
/// unambiguous, so it is escaped rather than dropped.
fn escape_bare_controls(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if in_string {
            if escaped {
                out.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => {
                    out.push(ch);
                    escaped = true;
                }
                '"' => {
                    out.push(ch);
                    in_string = false;
                }
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Drop commas that are followed (modulo whitespace) by a closer.
fn drop_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Close a document cut off mid-way: drop the incomplete tail element and append
/// the missing closers in stack order. The longest prefix that parses wins, which
/// keeps as much of the answer as possible.
///
/// A string the document was cut inside of is never closed in place: its
/// content is definitionally incomplete (a half-generated reply, a streaming
/// snapshot), and presenting the fragment as a finished value shipped a
/// truncated sentence to a group on 2026-10-27. Boundary cuts only ever land
/// outside strings, so the fragment falls away with the tail element.
fn close_truncated(input: &str) -> Option<String> {
    let base = input.trim_end().to_string();
    for cut in boundary_positions(&base).iter().rev() {
        let prefix = &base[..*cut];
        let Some(closers) = unclosed_closers(prefix) else {
            continue;
        };
        let candidate = format!("{prefix}{closers}");
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}

/// Byte offsets where a token just ended (outside strings), plus the buffer end.
/// Cutting only here means a value is never split in half.
fn boundary_positions(input: &str) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                out.push(i + ch.len_utf8());
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '}' | ']' => out.push(i + 1),
            c if c.is_ascii_alphanumeric() || c == '.' => out.push(i + 1),
            _ => {}
        }
    }
    if !out.contains(&input.len()) {
        out.push(input.len());
    }
    out
}

/// The closers needed to finish `prefix`, in reverse stack order.
/// `None` when `prefix` is unbalanced in a way that no closer can fix.
fn unclosed_closers(prefix: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in prefix.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.pop() != Some(ch) => return None,
            _ => {}
        }
    }
    if in_string {
        return None;
    }
    stack.reverse();
    Some(stack.into_iter().collect())
}

/// Shortest and longest plain-text answer accepted by the text layer.
pub const TEXT_MIN_CHARS: usize = 2;
/// A plain answer longer than this is not a reply any more.
pub const TEXT_MAX_CHARS: usize = 2_000;

/// Provider-side refusal and error notices (a Gemini policy block is the
/// known specimen, 2026-10-27: the notice arrived in place of a generation
/// and the text layer relayed it to the group verbatim). Machinery, never
/// prose: a turn whose model was refused ends silent. Signatures are
/// multi-word and distinctive; only the plain-text fallback is blocked,
/// because a contract-shaped reply carries its own vouching.
pub fn looks_like_provider_notice(text: &str) -> bool {
    const SIGNATURES: [&str; 6] = [
        "could not be submitted",
        "Prohibited Use policy",
        "Generative AI Prohibited",
        "sensitive words that violate",
        "ai.google.dev/gemini-api",
        "prompt was blocked",
    ];
    SIGNATURES.iter().any(|sig| text.contains(sig))
}

/// Last-resort fallback: the output is a plain-text reply, not a mangled
/// contract. Every condition must hold, so a broken document is never mistaken
/// for prose — losing a reply is cheaper than sending a debug dump to the group.
pub fn plain_text_reply(raw: &str) -> Option<String> {
    let text = raw.trim();
    let chars = text.chars().count();
    if chars < TEXT_MIN_CHARS {
        return None;
    }
    // Over-long prose is clamped, not refused: the section salvage clamps
    // reply parts the same way, and a model that answered in a long wall of
    // plain text at a deep step has still answered. The screens below run
    // on the clamped text.
    let text: &str = if chars > TEXT_MAX_CHARS {
        let boundary = text
            .char_indices()
            .nth(TEXT_MAX_CHARS)
            .map(|(at, _)| at)
            .unwrap_or(text.len());
        &text[..boundary]
    } else {
        text
    };
    let chars = text.chars().count();
    // A provider refusal/error notice is not the bot talking.
    if looks_like_provider_notice(text) {
        return None;
    }
    // Anything that opens like JSON is a contract attempt, not an answer.
    if text.starts_with('{') || text.starts_with('[') {
        return None;
    }
    // Tool calls and fenced code are machinery, not prose.
    if text.contains("call_mcp_tool") || text.contains("```") {
        return None;
    }
    // The same markup the reply path must never relay.
    if crate::agent::contract::contains_forbidden_markup(text) {
        return None;
    }
    // Contract vocabulary means a mangled document, not an answer.
    const CONTRACT_MARKERS: [&str; 8] = [
        "should_reply",
        "reply_parts",
        "reply_text",
        "memory_ops",
        "sticker_ops",
        "nicknames",
        "quote_ref",
        "mood_update",
    ];
    if CONTRACT_MARKERS.iter().any(|marker| text.contains(marker)) {
        return None;
    }
    // A wall of JSON-ish punctuation with no sentence structure is a dump.
    let structural = text
        .chars()
        .filter(|ch| matches!(ch, '{' | '}' | '[' | ']' | '"' | ':' | ','))
        .count();
    if structural * 3 > chars {
        return None;
    }
    if !text.chars().any(|ch| ch.is_alphanumeric() || is_cjk(ch)) {
        return None;
    }
    Some(text.to_string())
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_notice_is_never_a_plain_text_reply() {
        // The exact string the bot relayed to a group on 2026-10-27 when the
        // provider refused the prompt.
        let notice = "The prompt could not be submitted. The prompt contains sensitive words that violate Google's [Generative AI Prohibited Use policy](https://policies.google.com/terms/generative-ai/use-policy). Try rephrasing the prompt. If you think this was an error, [send feedback](https://ai.google.dev/gemini-api/docs/troubleshooting).";
        assert!(looks_like_provider_notice(notice));
        assert_eq!(plain_text_reply(notice), None);
        // Ordinary prose still passes the text layer.
        assert!(plain_text_reply("哈哈哈这也太好笑了吧").is_some());
    }

    #[test]
    fn spans_find_every_top_level_object() {
        let raw = "log line\n{\"a\":1}\nnoise\n{\"b\":{\"c\":2}}\ntrailing";
        let found = spans(raw);
        assert_eq!(found, vec!["{\"a\":1}", "{\"b\":{\"c\":2}}"]);
    }

    #[test]
    fn spans_ignore_braces_inside_strings() {
        let raw = r#"prose {"should_reply":true,"reply_parts":["a } b { c \" d"]} tail"#;
        let found = spans(raw);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("\"]}"));
        let value: Value = serde_json::from_str(found[0]).expect("span parses");
        assert_eq!(value["reply_parts"][0], "a } b { c \" d");
    }

    #[test]
    fn spans_keep_unterminated_tail() {
        let raw = r#"noise {"should_reply":true"#;
        assert_eq!(spans(raw), vec![r#"{"should_reply":true"#]);
    }

    #[test]
    fn repair_drops_trailing_comma() {
        let fixed =
            repair_json(r#"{"should_reply":true,"reply_parts":["hi"],}"#).expect("repaired");
        let value: Value = serde_json::from_str(&fixed).expect("parses");
        assert_eq!(value["should_reply"], true);
    }

    #[test]
    fn repair_escapes_bare_control_characters_inside_strings() {
        let raw = "{\"should_reply\":true,\"reply_parts\":[\"line1\nline2\"]}";
        assert!(serde_json::from_str::<Value>(raw).is_err());
        let fixed = repair_json(raw).expect("repaired");
        let value: Value = serde_json::from_str(&fixed).expect("parses");
        assert_eq!(value["reply_parts"][0], "line1\nline2");
    }

    #[test]
    fn repair_never_resurrects_a_truncated_string() {
        // Cut inside the only reply part: the fragment must not survive as a
        // finished reply (2026-10-27 live incident: "…字多" shipped, then the
        // completed line followed). Repair may keep the document, never the
        // fragment.
        let raw = r#"{"should_reply":true,"reply_parts":["hello wor"#;
        let fixed = repair_json(raw).expect("repaired");
        let value: Value = serde_json::from_str(&fixed).expect("parses");
        let parts = value["reply_parts"].as_array().cloned().unwrap_or_default();
        assert!(
            parts.iter().all(|p| p.as_str() != Some("hello wor")),
            "fragment survived repair: {fixed}"
        );
        // A complete part before the cut still survives; only the tail goes.
        let raw = r#"{"should_reply":true,"reply_parts":["done.","hello wor"#;
        let fixed = repair_json(raw).expect("repaired");
        let value: Value = serde_json::from_str(&fixed).expect("parses");
        assert_eq!(value["reply_parts"], serde_json::json!(["done."]));
    }

    #[test]
    fn repair_drops_a_truncated_tail_element() {
        let raw = r#"{"should_reply":true,"reply_parts":["ok"],"mood_update":{"label":"cal"#;
        let fixed = repair_json(raw).expect("repaired");
        let value: Value = serde_json::from_str(&fixed).expect("parses");
        assert_eq!(value["reply_parts"][0], "ok");
        // Either the label survived (string closed) or the whole element went: both
        // are valid repairs, what matters is that the reply did not.
        assert!(value["should_reply"] == true);
    }

    #[test]
    fn repair_gives_up_on_garbage() {
        assert!(repair_json("not json at all").is_none());
        assert!(repair_json("").is_none());
    }

    #[test]
    fn candidates_prefer_a_document_that_answers() {
        let raw = r#"{"status":"ok"} prose {"should_reply":true,"reply_parts":["hi"]}"#;
        let found = candidates(raw);
        assert_eq!(found.len(), 2);
        // The object carrying `should_reply` is the document that answers.
        assert_eq!(found[0].value["should_reply"], true);
        assert_eq!(found[0].layer, SalvageLayer::Span);
        assert_eq!(found[1].value["status"], "ok");
    }

    #[test]
    fn candidates_unwrap_the_four_envelopes() {
        let bare = candidates(r#"{"should_reply":true,"reply_parts":["a"]}"#);
        assert_eq!(bare[0].value["reply_parts"][0], "a");

        let result = candidates(r#"{"result":{"should_reply":true,"reply_parts":["b"]}}"#);
        assert_eq!(result[0].value["reply_parts"][0], "b");

        let text = candidates(r#"{"text":"{\"should_reply\":true,\"reply_parts\":[\"c\"]}"}"#);
        assert_eq!(text[0].value["reply_parts"][0], "c");
    }

    #[test]
    fn envelope_candidates_do_not_guess_at_unknown_objects() {
        assert!(envelope_candidates(r#"{"status":"ok","whatever":1}"#).is_empty());
        assert!(envelope_candidates("hello there").is_empty());
        assert_eq!(
            envelope_candidates(r#"{"should_reply":true}"#)[0].layer,
            SalvageLayer::Span
        );
    }

    #[test]
    fn candidates_fall_back_to_repair() {
        let raw = "thinking…\n{\"should_reply\":true,\"reply_parts\":[\"hi\"],}\n";
        let found = candidates(raw);
        assert_eq!(found[0].layer, SalvageLayer::Repair);
        assert_eq!(found[0].value["reply_parts"][0], "hi");
    }

    #[test]
    fn plain_text_is_accepted() {
        assert_eq!(
            plain_text_reply("  你好呀，今天过得怎么样？ ").as_deref(),
            Some("你好呀，今天过得怎么样？")
        );
        assert_eq!(plain_text_reply("ok").as_deref(), Some("ok"));
    }

    #[test]
    fn machinery_is_not_a_plain_reply() {
        assert!(plain_text_reply("").is_none());
        assert!(plain_text_reply("a").is_none());
        // Over-long prose is clamped to TEXT_MAX_CHARS, not refused
        // (2026-10-27: same clamp philosophy as the section salvage).
        let long_prose = "这是一个很长的回答。".repeat(300);
        let clamped = plain_text_reply(&long_prose).expect("long prose clamps");
        assert_eq!(clamped.chars().count(), TEXT_MAX_CHARS);
        assert!(plain_text_reply("```json\n{}\n```").is_none());
        assert!(plain_text_reply("I will call_mcp_tool now").is_none());
        assert!(plain_text_reply("{\"should_reply\":true}").is_none());
        assert!(plain_text_reply("[1,2,3]").is_none());
        assert!(plain_text_reply("好的[CQ:at,qq=1]").is_none());
        assert!(plain_text_reply("should_reply 应该是 true").is_none());
        assert!(plain_text_reply(r#""a":1,"b":2"#).is_none());
    }

    #[test]
    fn report_aggregates_and_renders() {
        let mut report = SalvageReport::new(SalvageLayer::Partial);
        report.note("memory_ops", "unknown_ref");
        report.note("memory_ops", "unknown_ref");
        report.note("reply_parts", "forbidden");
        report.clamped = 2;
        assert_eq!(report.dropped_total(), 3);
        assert_eq!(report.audit_suffix(), "partial.d3.c2");
        assert_eq!(
            report.detail(),
            "partial: memory_ops/unknown_ref×2, reply_parts/forbidden×1, clamped×2"
        );
        assert!(!report.is_clean());
        assert!(SalvageReport::default().is_clean());
    }
}

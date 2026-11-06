//! Raw OneBot v11 wire structs.
//!
//! These mirror the wire format exactly and are deliberately isolated from the
//! business types in `pipeline::normalize` so that a wire change never ripples
//! into the agent contract.

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_json::Value;

/// OneBot v11 sends QQ ids as JSON numbers. SnowLuma does that. A `String` field
/// rejects the whole event, so ids are accepted as either a string or a number.
fn string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(Value::Number(number)) => Ok(Some(number.to_string())),
        Some(other) => Err(de::Error::custom(format!(
            "id must be a string or number, got {other}"
        ))),
    }
}

/// A single message segment as sent by the OneBot implementation.
///
/// Unknown segment types are preserved with `data = None` so normalization can
/// decide to ignore them instead of failing the whole message.
#[derive(Debug, Clone, Deserialize)]
pub struct RawMessageSegment {
    #[serde(rename = "type")]
    pub seg_type: String,
    #[serde(default)]
    pub data: Option<SegmentData>,
}

/// Segment payload. Only the fields we actually consume are typed; everything
/// else is ignored by serde.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SegmentData {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "file_size")]
    pub file_size: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    /// QQ's own sticker flag on image segments: `1` is an animated sticker, `0` a
    /// normal picture. SnowLuma sends it as a JSON number, so it is widened here.
    #[serde(default, rename = "sub_type", deserialize_with = "string_or_number")]
    pub sub_type: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub qq: Option<String>,
    /// Target message id on `reply` segments: the message being quoted.
    #[serde(default, deserialize_with = "string_or_number")]
    pub id: Option<String>,
}

/// A raw event envelope. `post_type` decides which branch is meaningful.
#[derive(Debug, Clone, Deserialize)]
pub struct RawEvent {
    #[serde(default)]
    pub time: Option<i64>,
    #[serde(default, rename = "self_id", deserialize_with = "string_or_number")]
    pub self_id: Option<String>,
    #[serde(default, rename = "post_type")]
    pub post_type: Option<String>,
    #[serde(default, rename = "message_type")]
    pub message_type: Option<String>,
    #[serde(default, rename = "sub_type")]
    pub sub_type: Option<String>,
    #[serde(default, rename = "message_id", deserialize_with = "string_or_number")]
    pub message_id: Option<String>,
    #[serde(default, rename = "group_id", deserialize_with = "string_or_number")]
    pub group_id: Option<String>,
    #[serde(default, rename = "user_id", deserialize_with = "string_or_number")]
    pub user_id: Option<String>,
    /// Poke target on `notify`/`poke` notices: who got poked.
    #[serde(default, rename = "target_id", deserialize_with = "string_or_number")]
    pub target_id: Option<String>,
    #[serde(default)]
    pub message: Option<RawMessage>,
    #[serde(default)]
    pub raw_message: Option<String>,
    #[serde(default)]
    pub sender: Option<RawSender>,
    #[serde(default)]
    pub notice_type: Option<String>,
}

/// `message` is either a string or an array of segments depending on the
/// implementation and the configured message format.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawMessage {
    Text(String),
    Segments(Vec<RawMessageSegment>),
}

/// Sender info. Only a display name is consumed, and it is treated as hostile
/// data: it never reaches the agent verbatim.
#[derive(Debug, Clone, Deserialize)]
pub struct RawSender {
    #[serde(default, rename = "user_id", deserialize_with = "string_or_number")]
    pub user_id: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub card: Option<String>,
}

/// OneBot action response envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct RawActionResponse {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub retcode: Option<i64>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub echo: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub wording: Option<String>,
}

/// A client-to-server frame can be either an event or an action response.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum IncomingFrame {
    Event(RawEvent),
    Response(RawActionResponse),
    Unknown(serde_json::Value),
}

impl IncomingFrame {
    /// Classify by the presence of stable discriminator fields rather than by
    /// serde ordering, so a malformed event never masquerades as a response.
    pub fn classify(value: &serde_json::Value) -> FrameKind {
        if value.get("echo").is_some() {
            return FrameKind::Response;
        }
        if value.get("post_type").is_some() {
            return FrameKind::Event;
        }
        if value.get("retcode").is_some() && value.get("status").is_some() {
            return FrameKind::Response;
        }
        FrameKind::Unknown
    }
}

/// What a frame turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Event,
    Response,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_array_message_format() {
        let raw = r#"{
            "post_type":"message","message_type":"group","sub_type":"normal",
            "message_id":"12","group_id":"100","user_id":"555",
            "self_id":"999","time":1700000000,
            "message":[
                {"type":"text","data":{"text":"hello "}},
                {"type":"at","data":{"qq":"999"}},
                {"type":"image","data":{"file":"a.png","url":"https://media.example.com/a.png"}}
            ],
            "sender":{"user_id":"555","nickname":"nick"}
        }"#;
        let ev: RawEvent = serde_json::from_str(raw).expect("解析事件");
        assert_eq!(ev.post_type.as_deref(), Some("message"));
        match ev.message.expect("消息体") {
            RawMessage::Segments(segs) => {
                assert_eq!(segs.len(), 3);
                assert_eq!(segs[2].seg_type, "image");
                let data = segs[2].data.clone().expect("段数据");
                assert_eq!(data.url.as_deref(), Some("https://media.example.com/a.png"));
            }
            RawMessage::Text(_) => panic!("期待数组格式"),
        }
    }

    #[test]
    fn parses_string_message_format() {
        let raw = r#"{"post_type":"message","message_type":"group","message":"hi"}"#;
        let ev: RawEvent = serde_json::from_str(raw).expect("解析事件");
        match ev.message.expect("消息体") {
            RawMessage::Text(t) => assert_eq!(t, "hi"),
            RawMessage::Segments(_) => panic!("期待字符串格式"),
        }
    }

    #[test]
    fn unknown_segment_type_is_kept_without_data() {
        let raw = r#"{"type":"mystery"}"#;
        let seg: RawMessageSegment = serde_json::from_str(raw).expect("解析段");
        assert_eq!(seg.seg_type, "mystery");
        assert!(seg.data.is_none());
    }

    #[test]
    fn classifies_frames() {
        let event = serde_json::json!({"post_type":"message"});
        let response = serde_json::json!({"status":"ok","retcode":0,"echo":"1"});
        let unknown = serde_json::json!({"foo":1});
        assert_eq!(IncomingFrame::classify(&event), FrameKind::Event);
        assert_eq!(IncomingFrame::classify(&response), FrameKind::Response);
        assert_eq!(IncomingFrame::classify(&unknown), FrameKind::Unknown);
    }

    #[test]
    fn parses_action_response() {
        let raw = r#"{"status":"failed","retcode":100,"echo":"e1","message":"bad"}"#;
        let resp: RawActionResponse = serde_json::from_str(raw).expect("解析响应");
        assert_eq!(resp.retcode, Some(100));
        assert_eq!(resp.echo.as_deref(), Some("e1"));
    }

    #[test]
    fn numeric_ids_from_snowluma_parse_as_decimal_strings() {
        let raw = r#"{
            "post_type":"message","message_type":"group",
            "message_id":802604290,"group_id":900000002,"user_id":2000000002,
            "self_id":2000000001,"time":1792897563,
            "message":[
                {"type":"text","data":{"text":"自我介绍一下"}},
                {"type":"at","data":{"qq":2000000001}},
                {"type":"text","data":{"text":" "}}
            ],
            "sender":{"user_id":2000000002,"nickname":"tester","card":""}
        }"#;
        let event: RawEvent = serde_json::from_str(raw).expect("数字 ID 也应能解析");
        assert_eq!(event.message_id.as_deref(), Some("802604290"));
        assert_eq!(event.group_id.as_deref(), Some("900000002"));
        assert_eq!(event.user_id.as_deref(), Some("2000000002"));
        assert_eq!(event.self_id.as_deref(), Some("2000000001"));
        let sender = event.sender.expect("发送者");
        assert_eq!(sender.user_id.as_deref(), Some("2000000002"));
        let RawMessage::Segments(segments) = event.message.expect("消息体") else {
            panic!("应为段数组");
        };
        assert_eq!(
            segments[1]
                .data
                .as_ref()
                .and_then(|d| d.qq.clone())
                .as_deref(),
            Some("2000000001")
        );
    }
}

//! Owner private chat, carried on the group pipeline.
//!
//! The whole bot is keyed by group: batching, sessions, memory and the outbox.
//! Rather than fork those paths, the owner's private chat is given a synthetic
//! group id at the transport edge. Inbound frames are rewritten into group
//! messages; outbound sends are routed back to `send_private_msg`.
//!
//! Only the owner is rewritten. Any other private message is dropped as before,
//! so a stranger who adds the bot as a friend cannot spend the quota.

use serde_json::{Value, json};

use crate::onebot::event::RawEvent;

/// No real QQ group id has 18 digits, so this prefix cannot collide with one.
const PRIVATE_GROUP_PREFIX: &str = "990000";
const PRIVATE_GROUP_WIDTH: usize = 12;

/// The synthetic group id for a private chat with `user_id`.
pub fn private_group_id(user_id: &str) -> String {
    format!("{PRIVATE_GROUP_PREFIX}{user_id:0>PRIVATE_GROUP_WIDTH$}")
}

/// The QQ user behind a synthetic group id, or `None` for a real group.
pub fn private_user_id(group_id: &str) -> Option<&str> {
    let tail = group_id.strip_prefix(PRIVATE_GROUP_PREFIX)?;
    if tail.len() != PRIVATE_GROUP_WIDTH || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let user = tail.trim_start_matches('0');
    (!user.is_empty()).then_some(user)
}

/// Rewrite the owner's friend message into a message of their synthetic group.
pub fn rewrite_owner_private(event: RawEvent, owner_qq_id: &str) -> RawEvent {
    let is_owner_friend_message = event.post_type.as_deref() == Some("message")
        && event.message_type.as_deref() == Some("private")
        && matches!(event.sub_type.as_deref(), None | Some("friend"))
        && event.user_id.as_deref() == Some(owner_qq_id)
        && !owner_qq_id.is_empty();
    if !is_owner_friend_message {
        return event;
    }
    RawEvent {
        message_type: Some("group".to_string()),
        group_id: Some(private_group_id(owner_qq_id)),
        ..event
    }
}

/// The OneBot action and params for a send built by `SendGroupMsg`.
pub fn route_send(group_id: &str, mut params: Value) -> (&'static str, Value) {
    let Some(user_id) = private_user_id(group_id) else {
        return ("send_group_msg", params);
    };
    if let Some(map) = params.as_object_mut() {
        map.remove("group_id");
        map.insert("user_id".to_string(), json!(user_id));
    }
    ("send_private_msg", params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(raw: &str) -> RawEvent {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn synthetic_id_round_trips() {
        let group = private_group_id("123456789");
        assert_eq!(group, "990000000123456789");
        assert_eq!(private_user_id(&group), Some("123456789"));
    }

    #[test]
    fn real_groups_are_not_private() {
        assert_eq!(private_user_id("100"), None);
        assert_eq!(private_user_id("990000"), None);
        assert_eq!(private_user_id("990000000000000000"), None);
        assert_eq!(private_user_id("99000012345678901x"), None);
    }

    #[test]
    fn owner_friend_message_becomes_a_group_message() {
        let out = rewrite_owner_private(
            event(
                r#"{"post_type":"message","message_type":"private","sub_type":"friend",
                    "message_id":"7","user_id":123456789,"message":"hi"}"#,
            ),
            "123456789",
        );
        assert_eq!(out.message_type.as_deref(), Some("group"));
        assert_eq!(out.group_id.as_deref(), Some("990000000123456789"));
    }

    #[test]
    fn other_private_messages_stay_private() {
        let stranger = rewrite_owner_private(
            event(
                r#"{"post_type":"message","message_type":"private","sub_type":"friend",
                    "message_id":"7","user_id":555,"message":"hi"}"#,
            ),
            "123456789",
        );
        assert_eq!(stranger.message_type.as_deref(), Some("private"));
        assert_eq!(stranger.group_id, None);

        let temporary = rewrite_owner_private(
            event(
                r#"{"post_type":"message","message_type":"private","sub_type":"group",
                    "message_id":"7","user_id":123456789,"message":"hi"}"#,
            ),
            "123456789",
        );
        assert_eq!(temporary.message_type.as_deref(), Some("private"));
    }

    #[test]
    fn group_sends_are_untouched() {
        let (action, params) = route_send("100", json!({"group_id":"100","message":[]}));
        assert_eq!(action, "send_group_msg");
        assert_eq!(params["group_id"], "100");
    }

    #[test]
    fn private_sends_name_the_user_only() {
        let (action, params) = route_send(
            "990000000123456789",
            json!({"group_id":"990000000123456789","message":[]}),
        );
        assert_eq!(action, "send_private_msg");
        assert_eq!(params["user_id"], "123456789");
        assert!(params.get("group_id").is_none());
    }
}

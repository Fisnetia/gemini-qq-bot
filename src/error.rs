//! Error types.
//!
//! Error classes are stored in the database (`agent_calls.error_class`,
//! `memory_compaction_jobs.error_class`) and surfaced on the admin page, so the
//! strings are part of the operational contract and must stay stable.

use std::fmt;

/// Stable, low-cardinality error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Nothing has been attempted yet.
    None,
    /// The web socket to SnowLuma is down.
    OnebotDisconnected,
    /// A OneBot action returned a non-zero retcode.
    OnebotActionFailed,
    /// The US proxy or its US node is unhealthy.
    ProxyUnavailable,
    /// The Agy child process exceeded its deadline.
    AgentTimeout,
    /// The Agy process failed without a parsable result.
    AgentFailed,
    /// The Agy result could not be parsed into the expected contract.
    AgentBadOutput,
    /// The upstream model refused the submitted prompt itself (content
    /// filter): the prompt never ran, so the round is not a contract
    /// failure and the session is not at fault.
    ContentFiltered,
    /// The Agy call timed out while the session may already have advanced.
    AgentResultUnknown,
    /// The Aux compactor call failed.
    AuxFailed,
    /// The Aux compactor answered but the payload was rejected.
    AuxBadOutput,
    /// Media download was rejected or failed.
    MediaFailed,
    /// The feature is not configured or explicitly disabled.
    Unavailable,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::None => "none",
            ErrorClass::OnebotDisconnected => "onebot_disconnected",
            ErrorClass::OnebotActionFailed => "onebot_action_failed",
            ErrorClass::ProxyUnavailable => "proxy_unavailable",
            ErrorClass::AgentTimeout => "agent_timeout",
            ErrorClass::AgentFailed => "agent_failed",
            ErrorClass::AgentBadOutput => "agent_bad_output",
            ErrorClass::ContentFiltered => "content_filtered",
            ErrorClass::AgentResultUnknown => "agent_result_unknown",
            ErrorClass::AuxFailed => "aux_failed",
            ErrorClass::AuxBadOutput => "aux_bad_output",
            ErrorClass::MediaFailed => "media_failed",
            ErrorClass::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Top level error type for the bot.
#[derive(Debug)]
pub enum BotError {
    /// Configuration is missing or invalid.
    Config(String),
    /// Persistence failure.
    Store(sqlx::Error),
    /// I/O failure.
    Io(std::io::Error),
    /// A generic message.
    Message(String),
    /// A write was refused because the target memory row is retired
    /// (`archived_at` set). Carries the `memory_ref` so the caller can log the exact
    /// row instead of just "an update failed".
    MemoryArchived(String),
}

impl fmt::Display for BotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BotError::Config(m) => write!(f, "配置错误: {m}"),
            BotError::Store(e) => write!(f, "存储错误: {e}"),
            BotError::Io(e) => write!(f, "IO 错误: {e}"),
            BotError::Message(m) => f.write_str(m),
            BotError::MemoryArchived(r) => write!(f, "记忆已归档，拒绝更新: {r}"),
        }
    }
}

impl std::error::Error for BotError {}

impl From<sqlx::Error> for BotError {
    fn from(e: sqlx::Error) -> Self {
        BotError::Store(e)
    }
}

impl From<std::io::Error> for BotError {
    fn from(e: std::io::Error) -> Self {
        BotError::Io(e)
    }
}

impl From<reqwest::Error> for BotError {
    fn from(e: reqwest::Error) -> Self {
        BotError::Message(format!("HTTP 请求失败: {e}"))
    }
}

/// Convenience result alias.
pub type BotResult<T> = Result<T, BotError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The strings below are written into `agent_calls.error_class` and shown on the
    /// admin page, so a rename is a data migration, not a refactor.
    #[test]
    fn every_error_class_serialises_to_its_stable_slug() {
        let table = [
            (ErrorClass::None, "none"),
            (ErrorClass::OnebotDisconnected, "onebot_disconnected"),
            (ErrorClass::OnebotActionFailed, "onebot_action_failed"),
            (ErrorClass::ProxyUnavailable, "proxy_unavailable"),
            (ErrorClass::AgentTimeout, "agent_timeout"),
            (ErrorClass::AgentFailed, "agent_failed"),
            (ErrorClass::AgentBadOutput, "agent_bad_output"),
            (ErrorClass::ContentFiltered, "content_filtered"),
            (ErrorClass::AgentResultUnknown, "agent_result_unknown"),
            (ErrorClass::AuxFailed, "aux_failed"),
            (ErrorClass::AuxBadOutput, "aux_bad_output"),
            (ErrorClass::MediaFailed, "media_failed"),
            (ErrorClass::Unavailable, "unavailable"),
        ];
        let mut slugs = Vec::new();
        for (class, slug) in table {
            assert_eq!(class.as_str(), slug);
            // Display must agree with as_str: the audit trail is built from Display.
            assert_eq!(class.to_string(), slug);
            // Low cardinality means no uppercase, no spaces, no colons.
            assert!(
                slug.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "非规范 slug: {slug}"
            );
            slugs.push(class.as_str());
        }
        slugs.sort_unstable();
        let before = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), before, "两个错误类共用了同一个 slug");
    }

    #[test]
    fn config_error_carries_the_field_it_rejected() {
        let e = BotError::Config("SNOWLUMA_BOT_ID 必须是非空字符串".into());
        assert_eq!(e.to_string(), "配置错误: SNOWLUMA_BOT_ID 必须是非空字符串");
    }

    #[test]
    fn store_and_io_errors_are_prefixed() {
        let io = BotError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "缺失"));
        assert!(io.to_string().starts_with("IO 错误: "), "got: {io}");
        assert!(io.to_string().contains("缺失"));
    }

    #[test]
    fn an_archived_memory_names_the_exact_row() {
        let e = BotError::MemoryArchived("mem_500_12".into());
        assert_eq!(e.to_string(), "记忆已归档，拒绝更新: mem_500_12");
    }

    #[test]
    fn a_plain_message_is_never_reworded() {
        let e = BotError::Message("队列已满".into());
        assert_eq!(e.to_string(), "队列已满");
        // No prefix means no accidental double classification on the admin page.
        assert!(!e.to_string().contains(":"));
    }

    #[test]
    fn every_variant_is_usable_as_an_error_object() {
        // `Box<dyn Error>` is what `?` produces at the boundary, so the trait
        // impl has to hold for every variant.
        let errors: Vec<BotError> = vec![
            BotError::Config("c".into()),
            BotError::Message("m".into()),
            BotError::MemoryArchived("r".into()),
            BotError::Io(std::io::Error::other("io")),
        ];
        for e in errors {
            let boxed: Box<dyn std::error::Error> = Box::new(e);
            assert!(!boxed.to_string().is_empty());
        }
    }

    #[test]
    fn bot_result_holds_the_bare_value_on_the_ok_path() {
        let ok: BotResult<u8> = Ok(7);
        assert_eq!(ok.expect("应成功"), 7);
        let err: BotResult<u8> = Err(BotError::Message("停".into()));
        assert!(err.is_err());
    }
}

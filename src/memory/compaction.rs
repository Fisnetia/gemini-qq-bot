//! Memory compaction: turn a group's reviewed memories into a derived summary.
//!
//! Boundaries that shape this module:
//!
//! * Input is **only** the current group's reviewed `memories` rows. Raw pending
//!   events, chat logs and other groups' data are never read.
//! * The outgoing payload contains no QQ UIN, `group_id`, `message_id`, Agy session
//!   id or other group's content. Subjects appear as group-local aliases.
//! * The model may only return summary text; Rust validates the JSON envelope, the
//!   character budget and the alias/revision references. A bad or timed out response
//!   records an error class and **keeps the previous summary**.
//! * Scheduling: trigger at >50 memories or >8,000 characters, 24 h cooldown per
//!   group, global concurrency 1, at most 10 runs per day including manual ones.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::ErrorClass;
use crate::store::{CompactionJob, JobState, MemoryRecord, SqliteStore};

/// Trigger thresholds.
#[derive(Debug, Clone, Copy)]
pub struct CompactionThresholds {
    /// Compact when the group holds more than this many memories.
    pub max_memories: i64,
    /// …or more than this many characters across all memory content.
    pub max_chars: i64,
    /// Per-group cooldown between successful runs.
    pub cooldown_seconds: i64,
    /// Global daily cap, manual runs included.
    pub daily_limit: i64,
}

impl Default for CompactionThresholds {
    fn default() -> Self {
        Self {
            max_memories: 50,
            max_chars: 8_000,
            cooldown_seconds: 24 * 60 * 60,
            daily_limit: 10,
        }
    }
}

/// Bounds on the generated summary.
pub const MIN_SUMMARY_CHARS: usize = 20;
pub const MAX_SUMMARY_CHARS: usize = 1_200;

/// One compaction request, already scoped to a single group.
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub bot_id: String,
    pub group_id: String,
    /// Memories to summarise; already restricted to this group.
    pub memories: Vec<MemoryRecord>,
    /// Current memory revision, recorded as `source_revision`.
    pub source_revision: i64,
    /// Alias mapping used in the prompt so no raw ids leave Rust.
    pub aliases: Vec<(String, String)>,
}

/// Why a compaction did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Below both thresholds.
    BelowThreshold,
    /// Inside the 24 h cooldown.
    Cooldown,
    /// Daily cap reached.
    DailyLimit,
    /// Another group is compacting (global concurrency 1).
    Busy,
    /// No memories at all.
    NoMemories,
    /// The store refused the job.
    Rejected,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::BelowThreshold => "below_threshold",
            SkipReason::Cooldown => "cooldown",
            SkipReason::DailyLimit => "daily_limit",
            SkipReason::Busy => "busy",
            SkipReason::NoMemories => "no_memories",
            SkipReason::Rejected => "rejected",
        }
    }
}

/// Result of asking for a compaction.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionDecision {
    /// A job was created and is queued.
    Queued(CompactionJob),
    /// Not now, with the reason recorded.
    Skipped(SkipReason),
}

/// The model's response, after validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedSummary {
    pub summary_text: String,
    pub source_revision: i64,
}

/// Validation failure of a compactor response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryError {
    /// Not JSON, or not the expected object.
    Malformed,
    /// Too short to be a useful summary.
    TooShort,
    /// Exceeded the character budget.
    TooLong,
    /// Referenced a revision other than the one sent.
    BadRevision,
    /// Mentioned something that looks like a raw identifier.
    LeakedIdentifier,
}

impl SummaryError {
    pub fn as_str(self) -> &'static str {
        match self {
            SummaryError::Malformed => "malformed",
            SummaryError::TooShort => "too_short",
            SummaryError::TooLong => "too_long",
            SummaryError::BadRevision => "bad_revision",
            SummaryError::LeakedIdentifier => "leaked_identifier",
        }
    }

    /// Map onto the stable error classes stored on the job row.
    pub fn error_class(self) -> ErrorClass {
        match self {
            SummaryError::Malformed | SummaryError::BadRevision => ErrorClass::AuxBadOutput,
            SummaryError::TooShort | SummaryError::TooLong | SummaryError::LeakedIdentifier => {
                ErrorClass::AuxBadOutput
            }
        }
    }
}

/// Validate the compactor's JSON response.
///
/// `expected_revision` is the revision that was sent; a mismatch means the model
/// answered for a different snapshot and the result must not overwrite the summary.
pub fn validate_summary(
    raw: &str,
    expected_revision: i64,
) -> Result<ValidatedSummary, SummaryError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| SummaryError::Malformed)?;
    let obj = value.as_object().ok_or(SummaryError::Malformed)?;

    let text = obj
        .get("summary")
        .or_else(|| obj.get("summary_text"))
        .and_then(|v| v.as_str())
        .ok_or(SummaryError::Malformed)?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(SummaryError::Malformed);
    }

    let revision = match obj.get("source_revision") {
        None => expected_revision,
        Some(v) => v.as_i64().ok_or(SummaryError::Malformed)?,
    };
    if revision != expected_revision {
        return Err(SummaryError::BadRevision);
    }

    let chars = text.chars().count();
    if chars < MIN_SUMMARY_CHARS {
        return Err(SummaryError::TooShort);
    }
    if chars > MAX_SUMMARY_CHARS {
        return Err(SummaryError::TooLong);
    }
    if leaks_identifier(&text) {
        return Err(SummaryError::LeakedIdentifier);
    }
    Ok(ValidatedSummary {
        summary_text: text,
        source_revision: revision,
    })
}

/// Conservative detector for raw identifiers or cross-boundary references that must
/// never appear in a derived summary: bare long digit runs (QQ numbers, message ids),
/// explicit id keys, and CQ codes.
pub fn leaks_identifier(text: &str) -> bool {
    if text.contains("[CQ:") || text.contains("group_id") || text.contains("message_id") {
        return true;
    }
    // A run of 5+ digits is never a legitimate part of a summary sentence.
    let mut run = 0usize;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            run += 1;
            if run >= 5 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Render the compactor prompt. Contains only aliased memory content of one group.
pub fn render_prompt(request: &CompactionRequest) -> String {
    let mut out = String::with_capacity(2_048);
    out.push_str(
        "你在为一个聊天群维护一份记忆摘要。下面只给出该群已审核的记忆条目，\
每条带一个群内别名。请把它们压缩成一段客观、简洁的中文摘要。\n\n要求：\n",
    );
    out.push_str(&format!(
        "1. 摘要长度在 {MIN_SUMMARY_CHARS} 到 {MAX_SUMMARY_CHARS} 字之间。\n"
    ));
    out.push_str("2. 只使用给出的信息，不得推测、不得编造新的成员或事实。\n");
    out.push_str("3. 不得出现任何数字编号、身份证号、聊天记录原文或外部链接。\n");
    out.push_str("4. 不得提及别的群、别的会话或系统配置。\n");
    out.push_str("5. 只输出 JSON，不要 Markdown：{\"summary\":\"…\",\"source_revision\":N}\n\n");
    out.push_str(&format!(
        "source_revision = {}\n\n",
        request.source_revision
    ));
    out.push_str("记忆条目：\n");
    for (index, memory) in request.memories.iter().enumerate() {
        let subject = memory
            .subject_user_id
            .as_ref()
            .and_then(|uid| {
                request
                    .aliases
                    .iter()
                    .find(|(user_id, _)| user_id == uid)
                    .map(|(_, alias)| alias.clone())
            })
            .unwrap_or_else(|| "群".to_string());
        out.push_str(&format!(
            "{}. [{}] ({}) {}\n",
            index + 1,
            subject,
            memory.kind,
            memory.content.trim()
        ));
    }
    out
}

/// The compactor's model interface. The real implementation is the Aux adapter;
/// tests use a fake.
#[async_trait::async_trait]
pub trait SummaryModel: Send + Sync + 'static {
    /// Return the raw JSON string produced by the model.
    async fn summarise(&self, prompt: String) -> Result<String, ErrorClass>;
}

/// Scheduler that decides when a compaction may run.
pub struct CompactionScheduler {
    store: Arc<SqliteStore>,
    thresholds: CompactionThresholds,
    /// Guards the global "concurrency 1" rule.
    busy: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for CompactionScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionScheduler")
            .field("thresholds", &self.thresholds)
            .finish_non_exhaustive()
    }
}

impl CompactionScheduler {
    pub fn new(store: Arc<SqliteStore>, thresholds: CompactionThresholds) -> Self {
        Self {
            store,
            thresholds,
            busy: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Whether the group currently exceeds a threshold.
    pub async fn above_threshold(&self, bot_id: &str, group_id: &str) -> bool {
        match self.store.memory_stats(bot_id, group_id).await {
            Ok((count, chars)) => {
                count > self.thresholds.max_memories || chars > self.thresholds.max_chars
            }
            Err(_) => false,
        }
    }

    /// Decide whether a compaction may be queued right now, and queue it if so.
    ///
    /// `requested_by` is `owner` for the QQ command path and `panel` for the admin
    /// page; manual runs count against the daily limit exactly like automatic ones.
    pub async fn maybe_queue(
        &self,
        bot_id: &str,
        group_id: &str,
        requested_by: crate::store::ActorKind,
        now: i64,
        force: bool,
    ) -> CompactionDecision {
        let (count, _chars) = match self.store.memory_stats(bot_id, group_id).await {
            Ok(stats) => stats,
            Err(_) => return CompactionDecision::Skipped(SkipReason::Rejected),
        };
        if count == 0 {
            return CompactionDecision::Skipped(SkipReason::NoMemories);
        }
        if !force && !self.above_threshold(bot_id, group_id).await {
            return CompactionDecision::Skipped(SkipReason::BelowThreshold);
        }
        // Per-group 24 h cooldown.
        match self.store.last_success_at(bot_id, group_id).await {
            Ok(Some(last)) if now - last < self.thresholds.cooldown_seconds && !force => {
                return CompactionDecision::Skipped(SkipReason::Cooldown);
            }
            Ok(_) => {}
            Err(_) => return CompactionDecision::Skipped(SkipReason::Rejected),
        }
        // Global daily cap, manual runs included.
        let day_start = now - (now % 86_400);
        match self.store.jobs_since(day_start).await {
            Ok(n) if n >= self.thresholds.daily_limit => {
                return CompactionDecision::Skipped(SkipReason::DailyLimit);
            }
            Ok(_) => {}
            Err(_) => return CompactionDecision::Skipped(SkipReason::Rejected),
        }
        let revision = match self.store.memory_revision(bot_id, group_id).await {
            Ok(r) => r,
            Err(_) => return CompactionDecision::Skipped(SkipReason::Rejected),
        };
        match self
            .store
            .create_job(bot_id, group_id, revision, requested_by.as_str(), now)
            .await
        {
            Ok(job) => CompactionDecision::Queued(job),
            Err(_) => CompactionDecision::Skipped(SkipReason::Rejected),
        }
    }

    /// Try to take the single global execution slot.
    pub fn try_lock(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        Arc::clone(&self.busy).try_lock_owned().ok()
    }

    /// Run one job end to end.
    ///
    /// On success the summary is written and the job marked `succeeded`; on any error
    /// the previous summary is left untouched and the job records the error class.
    pub async fn run_job(
        &self,
        job: &CompactionJob,
        model: Arc<dyn SummaryModel>,
        aliases: Vec<(String, String)>,
        now: i64,
    ) -> Result<ValidatedSummary, SummaryError> {
        let memories = match self
            .store
            .list_memories(&job.bot_id, &job.group_id, 500)
            .await
        {
            Ok(m) => m,
            Err(_) => {
                let _ = self
                    .store
                    .finish_job(&job.id, JobState::Failed, Some("store"), now)
                    .await;
                return Err(SummaryError::Malformed);
            }
        };
        let request = CompactionRequest {
            bot_id: job.bot_id.clone(),
            group_id: job.group_id.clone(),
            memories,
            source_revision: job.source_revision,
            aliases,
        };
        let prompt = render_prompt(&request);

        let _ = self.store.mark_job_running(&job.id, now).await;
        let raw = match model.summarise(prompt).await {
            Ok(raw) => raw,
            Err(class) => {
                let _ = self
                    .store
                    .finish_job(&job.id, JobState::Failed, Some(class.as_str()), now)
                    .await;
                return Err(SummaryError::Malformed);
            }
        };
        match validate_summary(&raw, job.source_revision) {
            Ok(validated) => {
                let _ = self
                    .store
                    .put_summary(
                        &job.bot_id,
                        &job.group_id,
                        &validated.summary_text,
                        validated.source_revision,
                        crate::config::DEFAULT_AUX_MODEL,
                        now,
                    )
                    .await;
                let _ = self
                    .store
                    .finish_job(&job.id, JobState::Succeeded, None, now)
                    .await;
                Ok(validated)
            }
            Err(e) => {
                let _ = self
                    .store
                    .finish_job(
                        &job.id,
                        JobState::Failed,
                        Some(e.error_class().as_str()),
                        now,
                    )
                    .await;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CompactionRequest {
        CompactionRequest {
            bot_id: "1".into(),
            group_id: "100".into(),
            memories: vec![
                MemoryRecord {
                    memory_ref: "mem_a".into(),
                    bot_id: "1".into(),
                    group_id: "100".into(),
                    subject_user_id: Some("555".into()),
                    content: "喜欢手冲咖啡".into(),
                    kind: "preference".into(),
                    source_message_ids: "[]".into(),
                    confidence: 0.9,
                    revision: 1,
                    origin: "agy".into(),
                    archived_at: None,
                    created_at: 10,
                    updated_at: 10,
                },
                MemoryRecord {
                    memory_ref: "mem_b".into(),
                    bot_id: "1".into(),
                    group_id: "100".into(),
                    subject_user_id: None,
                    content: "群里有三个人常聊茶叶".into(),
                    kind: "topic".into(),
                    source_message_ids: "[]".into(),
                    confidence: 0.7,
                    revision: 1,
                    origin: "agy".into(),
                    archived_at: None,
                    created_at: 10,
                    updated_at: 10,
                },
            ],
            source_revision: 7,
            aliases: vec![("555".into(), "u1".into())],
        }
    }

    #[test]
    fn thresholds_match_the_design() {
        let t = CompactionThresholds::default();
        assert_eq!(t.max_memories, 50);
        assert_eq!(t.max_chars, 8_000);
        assert_eq!(t.cooldown_seconds, 86_400);
        assert_eq!(t.daily_limit, 10);
    }

    #[test]
    fn prompt_uses_aliases_and_no_raw_ids() {
        let prompt = render_prompt(&request());
        assert!(prompt.contains("[u1] (preference) 喜欢手冲咖啡"));
        assert!(prompt.contains("[群] (topic)"));
        // The raw user id must not appear.
        assert!(!prompt.contains("555"));
        assert!(!prompt.contains("mem_a"));
        assert!(prompt.contains("source_revision = 7"));
        assert!(!leaks_identifier(&prompt) || prompt.contains("source_revision"));
    }

    #[test]
    fn valid_summary_is_accepted() {
        let raw = r#"{"summary":"本群常聊咖啡与茶叶，成员偏好温和的日常话题，没有争议性讨论。","source_revision":7}"#;
        let v = validate_summary(raw, 7).expect("应通过");
        assert_eq!(v.source_revision, 7);
        assert!(v.summary_text.starts_with("本群常聊"));
    }

    #[test]
    fn validate_summary_rejects_each_error_class() {
        let long = format!(
            r#"{{"summary":"{}","source_revision":7}}"#,
            "很长".repeat(MAX_SUMMARY_CHARS)
        );
        let cases = [
            (
                r#"{"summary":"本群常聊咖啡与茶叶，成员偏好温和的日常话题，没有争议性讨论。","source_revision":6}"#,
                SummaryError::BadRevision,
            ),
            (
                r#"{"summary":"太短","source_revision":7}"#,
                SummaryError::TooShort,
            ),
            (long.as_str(), SummaryError::TooLong),
            (
                r#"{"summary":"成员 123456789 喜欢咖啡并且经常在群里分享新的豆子和冲煮技巧。","source_revision":7}"#,
                SummaryError::LeakedIdentifier,
            ),
            (
                r#"{"summary":"成员喜欢咖啡并且经常分享[CQ:at,qq=all]这类标记，需要清理掉。","source_revision":7}"#,
                SummaryError::LeakedIdentifier,
            ),
            ("not json", SummaryError::Malformed),
            (r#"{"nothing":1}"#, SummaryError::Malformed),
            (r#"["a"]"#, SummaryError::Malformed),
        ];
        for (raw, err) in cases {
            assert_eq!(validate_summary(raw, 7).unwrap_err(), err, "{raw}");
        }
    }

    #[test]
    fn identifier_detection_allows_normal_numbers() {
        assert!(!leaks_identifier("群里有 3 个人常聊咖啡"));
        assert!(!leaks_identifier("每天聊 20 分钟"));
        assert!(leaks_identifier("编号 12345"));
    }

    #[test]
    fn error_classes_are_stable_strings() {
        for err in [
            SummaryError::Malformed,
            SummaryError::TooShort,
            SummaryError::TooLong,
            SummaryError::BadRevision,
            SummaryError::LeakedIdentifier,
        ] {
            assert_eq!(err.error_class(), ErrorClass::AuxBadOutput);
            assert!(!err.as_str().is_empty());
        }
    }
}

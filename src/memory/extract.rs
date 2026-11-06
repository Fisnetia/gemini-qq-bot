//! Fold-time memory extraction (`origin = 'hy3'`).
//!
//! The fold request already carries one window of raw chat to the cheap model. Asking
//! for a second round trip just to harvest durable facts would double the cost of the
//! most frequent background job, so the same request also asks for a list of memory
//! candidates and the same response is parsed twice: [`crate::memory::chunker`] keeps
//! the `summary` field, this module reads the `memories` field.
//!
//! Extraction is strictly additive and strictly optional:
//!
//! * it may only **create** rows, never update or delete one — a fold must never be a
//!   back door around the agent contract's write rules,
//! * a missing or malformed candidate block costs nothing but the extraction,
//! * a candidate that came in over the wire is a proposal, not a fact: the kind, the
//!   length, the confidence ceiling and every `source_refs` entry are re-checked here,
//! * a candidate that near-duplicates an existing active memory is dropped, even when
//!   the existing row was written by the agent. Skipping is the whole action: the
//!   agent's row is left byte-identical.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::contract::{MAX_MEMORY_CONTENT_CHARS, MEMORY_KINDS};
use crate::memory::retrieval::{bigrams, overlap_score};
use crate::store::{GroupMessageRecord, SqliteStore};

/// Maximum candidates honoured from one fold response.
///
/// A fold covers at most [`crate::memory::chunker::MAX_FOLD_MESSAGES`] messages, so a
/// handful of durable facts is the realistic ceiling; anything beyond that is the model
/// padding the array.
pub const MAX_CANDIDATES: usize = 6;

/// Hard ceiling on extracted confidence.
///
/// Extraction reads a raw chat log, not a reviewed statement, so it must never claim
/// more certainty than the agent contract can: a candidate is capped below what a
/// deliberate per-turn write may assert.
pub const MAX_EXTRACTED_CONFIDENCE: f64 = 0.8;

/// Content bigram similarity at or above which a candidate counts as a duplicate.
/// 0.85 missed paraphrases of the same claim ("青蛙长文是希腊奶写的" versus
/// "希腊奶蛙写青蛙长文"). 0.55 still requires most of the bigrams to agree.
pub const NEAR_DUPLICATE_THRESHOLD: f64 = 0.55;

/// How a response's candidate block parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateParseStatus {
    /// The response had no `memories` key at all.
    Absent,
    /// The key existed but was not a readable array (or the whole body was not JSON).
    Broken,
    /// A readable array, possibly empty.
    Parsed,
}

impl CandidateParseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateParseStatus::Absent => "absent",
            CandidateParseStatus::Broken => "broken",
            CandidateParseStatus::Parsed => "parsed",
        }
    }
}

/// One candidate exactly as the model proposed it, before server-side checks.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCandidate {
    pub kind: String,
    /// Group-local alias of the subject (`u3`), or empty for a group-level fact.
    pub subject: String,
    pub content: String,
    pub confidence: f64,
    /// 1-based line numbers into the folded window.
    pub source_refs: Vec<String>,
}

/// A candidate that survived validation, with refs already mapped to row ids.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCandidate {
    pub kind: String,
    pub subject: String,
    pub content: String,
    pub confidence: f64,
    pub source_ids: Vec<i64>,
}

/// What one extraction pass did. Purely diagnostic: nothing downstream branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtractionReport {
    pub status: Option<CandidateParseStatus>,
    pub created: usize,
    pub duplicates: usize,
    pub rejected: usize,
}

impl ExtractionReport {
    fn new(status: CandidateParseStatus) -> Self {
        Self {
            status: Some(status),
            ..Self::default()
        }
    }

    /// Audit action summarising this pass, in the same shape as `memory.fold.*`.
    pub fn audit_action(&self, group_id: &str) -> String {
        format!(
            "memory.extract.g{group_id}.{}.n{}.dup{}.rej{}",
            self.status.unwrap_or(CandidateParseStatus::Absent).as_str(),
            self.created,
            self.duplicates,
            self.rejected
        )
    }
}

/// The JSON object inside a possibly fenced model reply.
///
/// The fold reply carries both a summary and a candidate array, and a model that fences
/// its JSON is normal, so the slicing lives in one place instead of being copied.
pub fn json_body(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed
    } else {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        &trimmed[start..=end]
    };
    serde_json::from_str(candidate).ok()
}

/// Read the `memories` array out of a fold response.
///
/// Never returns an error: a reply without candidates is a legitimate reply, and a
/// garbled one must not take the fold down with it.
pub fn parse_candidates(raw: &str) -> (CandidateParseStatus, Vec<MemoryCandidate>) {
    let Some(body) = json_body(raw) else {
        return (CandidateParseStatus::Broken, Vec::new());
    };
    let Some(block) = body.get("memories") else {
        return (CandidateParseStatus::Absent, Vec::new());
    };
    let Some(items) = block.as_array() else {
        return (CandidateParseStatus::Broken, Vec::new());
    };
    let mut candidates = Vec::new();
    for item in items.iter().take(MAX_CANDIDATES) {
        if let Some(candidate) = candidate_from_json(item) {
            candidates.push(candidate);
        }
    }
    (CandidateParseStatus::Parsed, candidates)
}

fn candidate_from_json(item: &Value) -> Option<MemoryCandidate> {
    let kind = item.get("kind")?.as_str()?.trim().to_lowercase();
    let content = item.get("content")?.as_str()?.trim().to_string();
    if kind.is_empty() || content.is_empty() {
        return None;
    }
    let subject = item
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    // A missing confidence is read as the floor rather than rejected: the ranking
    // already multiplies by it, so an absent value simply ranks last.
    let confidence = item
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.5);
    let source_refs = item
        .get("source_refs")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .filter_map(|r| match r {
                    Value::Number(n) => n.as_i64().map(|n| n.to_string()),
                    Value::String(s) => Some(s.trim().to_string()),
                    _ => None,
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    Some(MemoryCandidate {
        kind,
        subject,
        content,
        confidence,
        source_refs,
    })
}

/// Normalise a subject for equality checks: case and spacing carry no meaning here.
pub fn normalize_subject(subject: &str) -> String {
    subject
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Re-check one candidate against the server-side rules.
///
/// `window` is the folded message window; `refs` are resolved against its position, so
/// a candidate can only ever cite a message this fold actually covered.
pub fn validate_candidate(
    candidate: &MemoryCandidate,
    window: &[GroupMessageRecord],
) -> Option<ValidatedCandidate> {
    if !MEMORY_KINDS.contains(&candidate.kind.as_str()) {
        return None;
    }
    if candidate.content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
        return None;
    }
    // At least one citation: an extracted fact nobody can trace is unverifiable, and
    // the whole point of the ref is that a later reader can check it.
    if candidate.source_refs.is_empty() {
        return None;
    }
    let mut source_ids = Vec::new();
    for raw_ref in &candidate.source_refs {
        let index = raw_ref
            .strip_prefix('m')
            .unwrap_or(raw_ref)
            .parse::<usize>()
            .ok()?;
        // 1-based line numbers; 0 and anything past the window are out of scope.
        let message = window.get(index.checked_sub(1)?)?;
        if !source_ids.contains(&message.id) {
            source_ids.push(message.id);
        }
    }
    if crate::memory::retrieval::is_operational_memory(&candidate.content) {
        return None;
    }
    let confidence = if candidate.confidence.is_finite() {
        candidate.confidence.clamp(0.0, MAX_EXTRACTED_CONFIDENCE)
    } else {
        MAX_EXTRACTED_CONFIDENCE
    };
    Some(ValidatedCandidate {
        kind: candidate.kind.clone(),
        subject: candidate.subject.clone(),
        content: candidate.content.clone(),
        confidence,
        source_ids,
    })
}

/// True when `content` is the same fact as an existing memory's content.
pub fn is_near_duplicate(content: &str, existing: &str) -> bool {
    overlap_score(&bigrams(content), &bigrams(existing)) >= NEAR_DUPLICATE_THRESHOLD
}

/// Runs extraction for one folded window.
#[derive(Debug, Clone)]
pub struct Extractor {
    store: Arc<SqliteStore>,
    /// Writer tag for the rows this pass creates: which aux backend ran
    /// the fold (`origin::HY3` in gateway mode, `origin::AGY` in agy mode).
    origin: &'static str,
}

impl Extractor {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self {
            store,
            origin: crate::store::origin::HY3,
        }
    }

    pub fn with_origin(mut self, origin: &'static str) -> Self {
        self.origin = origin;
        self
    }

    /// Extract and store candidates from one fold response.
    ///
    /// Returns a report instead of a `Result`: a fold has already succeeded by the time
    /// this runs, and a store hiccup here must not turn it into a failed fold that
    /// retries the same window forever.
    pub async fn run(
        &self,
        bot_id: &str,
        group_id: &str,
        window: &[GroupMessageRecord],
        raw: &str,
        aliases: &[(String, String)],
        now: i64,
    ) -> ExtractionReport {
        let (status, candidates) = parse_candidates(raw);
        let mut report = ExtractionReport::new(status);
        if status != CandidateParseStatus::Parsed {
            return report;
        }

        // Active rows only: a retired memory must not block a fresh extraction, and
        // this is also the snapshot the duplicate check is run against.
        let existing = match self.store.list_memories(bot_id, group_id, 500).await {
            Ok(rows) => rows,
            Err(_) => {
                report.rejected += candidates.len();
                return report;
            }
        };
        let mut taken: Vec<(String, Option<String>, String)> = existing
            .into_iter()
            .map(|m| {
                (
                    m.kind.clone(),
                    m.subject_user_id.as_deref().map(normalize_subject),
                    m.content.clone(),
                )
            })
            .collect();

        for candidate in &candidates {
            let Some(valid) = validate_candidate(candidate, window) else {
                report.rejected += 1;
                continue;
            };
            // The assistant is a first-class subject: an item about it binds
            // to the bot's own id, never to a member and never to nobody.
            let subject_user_id = if normalize_subject(&valid.subject) == "bot" {
                Some(bot_id.to_string())
            } else {
                resolve_subject(&valid.subject, aliases)
                    .or_else(|| derive_subject_from_content(&valid.content, window, bot_id))
            };
            // ID binding is not optional for person-scoped kinds: a preference
            // or a relation with no resolved subject is an unattributable
            // claim, and unattributable claims are how jokes about "member"
            // became long-term facts. The placeholder word itself is likewise
            // banned from content — it means the extractor lost the person.
            let person_scoped = matches!(valid.kind.as_str(), "preference" | "relation");
            if (person_scoped && subject_user_id.is_none()) || valid.content.contains("member") {
                report.rejected += 1;
                continue;
            }
            let key_subject = subject_user_id.as_deref().map(normalize_subject);
            let duplicate = taken.iter().any(|(kind, subject, content)| {
                kind == &valid.kind
                    && subject == &key_subject
                    && is_near_duplicate(&valid.content, content)
            });
            if duplicate {
                report.duplicates += 1;
                continue;
            }

            let source_ids =
                serde_json::to_string(&valid.source_ids).unwrap_or_else(|_| "[]".into());
            let inserted = self
                .store
                .insert_memory_with_origin(
                    bot_id,
                    group_id,
                    subject_user_id.as_deref(),
                    &valid.content,
                    &valid.kind,
                    &source_ids,
                    valid.confidence,
                    self.origin,
                    now,
                )
                .await;
            match inserted {
                Ok(_) => {
                    report.created += 1;
                    // Guard against two identical candidates in one response.
                    taken.push((valid.kind.clone(), key_subject, valid.content.clone()));
                }
                Err(_) => report.rejected += 1,
            }
        }
        report
    }
}

/// Map a candidate's `subject` alias onto the group's real member id.
///
/// An alias this group does not know is treated as "about the group", never as a raw
/// user id: the model must not be able to name a stranger.
fn resolve_subject(subject: &str, aliases: &[(String, String)]) -> Option<String> {
    let wanted = normalize_subject(subject);
    if wanted.is_empty() {
        return None;
    }
    aliases
        .iter()
        .find(|(alias, _)| normalize_subject(alias) == wanted)
        .map(|(_, user_id)| user_id.clone())
}

/// Recover a subject anchor the extractor failed to name.
///
/// The extractor writes memory content in natural language — it will happily
/// produce a fact whose text names the owner while leaving `subject` empty,
/// because the roster it sees is bare aliases and the person it means may
/// not have spoken in the window at all. An unanchored person-fact is nearly
/// useless at recall time (it cannot be attributed or weighted), so before
/// giving up, derive the anchor from the content itself: a leading display
/// name resolves to that speaker, and an explicit owner reference resolves
/// to the window's owner. Anything else stays unanchored — a group-level
/// fact is a legitimate thing to be.
fn derive_subject_from_content(
    content: &str,
    window: &[GroupMessageRecord],
    bot_id: &str,
) -> Option<String> {
    let head: String = content.chars().take(8).collect();
    for message in window {
        if message.sender_id == bot_id {
            continue;
        }
        let Some(name) = message.display_name.as_deref().map(str::trim) else {
            continue;
        };
        // Display names may carry an ASCII prefix (a name like "DS..." whose
        // distinctive tail is what the content actually cites).
        let core =
            name.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if core.chars().count() >= 2 && head.contains(core) {
            return Some(message.sender_id.clone());
        }
    }
    if content.contains("\u{4e3b}\u{4eba}")
        && let Some(owner) = window.iter().find(|m| m.speaker_role == "owner")
    {
        return Some(owner.sender_id.clone());
    }
    None
}

/// Audit action name for a pass, exposed so the chunker stays out of the format.
pub fn audit_action(group_id: &str, report: &ExtractionReport) -> String {
    report.audit_action(group_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: i64, sender: &str, text: &str) -> GroupMessageRecord {
        GroupMessageRecord {
            id,
            bot_id: "9999".into(),
            group_id: "500".into(),
            message_id: format!("msg{id}"),
            sender_id: sender.into(),
            display_name: Some("tester".into()),
            speaker_role: "member".into(),
            text: text.into(),
            has_image: false,
            sticker_hash: None,
            sub_type: None,
            image_tags: None,
            created_at: id,
            archived: false,
        }
    }

    fn window() -> Vec<GroupMessageRecord> {
        vec![
            record(11, "42", "以后每周五晚上我都来打本"),
            record(12, "43", "那就定周五九点"),
            record(13, "42", "行"),
        ]
    }

    fn aliases() -> Vec<(String, String)> {
        vec![
            ("u1".to_string(), "42".to_string()),
            ("u2".to_string(), "43".to_string()),
        ]
    }

    fn candidate(kind: &str, subject: &str, content: &str, conf: f64) -> MemoryCandidate {
        MemoryCandidate {
            kind: kind.into(),
            subject: subject.into(),
            content: content.into(),
            confidence: conf,
            source_refs: vec!["m1".into()],
        }
    }

    // --- parser: valid / broken / absent -------------------------------------------

    #[test]
    fn a_reply_without_a_candidate_block_is_absent_not_an_error() {
        let raw = r#"{"summary": "他们约好了周五打本的事情，还顺带定了具体时间。"}"#;
        let (status, candidates) = parse_candidates(raw);
        assert_eq!(status, CandidateParseStatus::Absent);
        assert!(candidates.is_empty());
    }

    #[test]
    fn a_garbled_candidate_block_is_broken_and_yields_nothing() {
        // Missing closing bracket inside the array: the whole body fails to parse.
        let raw = r#"{"summary": "摘要", "memories": [{"kind": "fact""#;
        let (status, candidates) = parse_candidates(raw);
        assert_eq!(status, CandidateParseStatus::Broken);
        assert!(candidates.is_empty());

        // A non-array `memories` is broken too, but the summary is untouched.
        let raw = r#"{"summary": "摘要", "memories": "fact"}"#;
        assert_eq!(parse_candidates(raw).0, CandidateParseStatus::Broken);
    }

    #[test]
    fn a_valid_candidate_block_parses_every_field() {
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "preference", "subject": "u1", "content": "每周五晚上打本",
                 "confidence": 0.7, "source_refs": ["m1", "m2"]},
                {"kind": "event", "subject": "", "content": "周五九点集合",
                 "confidence": 0.6, "source_refs": [3]}
            ]
        })
        .to_string();
        let (status, candidates) = parse_candidates(&raw);
        assert_eq!(status, CandidateParseStatus::Parsed);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].kind, "preference");
        assert_eq!(candidates[0].source_refs, vec!["m1", "m2"]);
        // Numeric refs are accepted as well as `mN` strings.
        assert_eq!(candidates[1].source_refs, vec!["3"]);
        assert_eq!(candidates[1].subject, "");
    }

    #[test]
    fn a_fenced_reply_still_parses() {
        let raw = "```json\n{\"summary\": \"x\", \"memories\": [{\"kind\": \"fact\", \"content\": \"住在杭州\", \"source_refs\": [\"m1\"]}]}\n```";
        let (status, candidates) = parse_candidates(raw);
        assert_eq!(status, CandidateParseStatus::Parsed);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn junk_entries_inside_the_array_are_dropped_one_by_one() {
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "fact"},                                  // no content
                {"content": "缺少 kind"},                           // no kind
                {"kind": "fact", "content": "住在杭州", "source_refs": ["m1"]}
            ]
        })
        .to_string();
        let (status, candidates) = parse_candidates(&raw);
        assert_eq!(status, CandidateParseStatus::Parsed);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].content, "住在杭州");
    }

    #[test]
    fn the_candidate_cap_is_applied() {
        let items: Vec<Value> = (0..20)
            .map(|i| {
                serde_json::json!({"kind": "fact", "content": format!("事实{i}"), "source_refs": ["m1"]})
            })
            .collect();
        let raw = serde_json::json!({"summary": "摘要", "memories": items}).to_string();
        assert_eq!(parse_candidates(&raw).1.len(), MAX_CANDIDATES);
    }

    // --- validation ----------------------------------------------------------------

    #[test]
    fn validate_candidate_rejects_each_invalid_shape() {
        let w = window();
        let mut past_end = candidate("fact", "", "每周五晚上打本", 0.5);
        past_end.source_refs = vec!["m9".into()];
        assert!(
            validate_candidate(&past_end, &w).is_none(),
            "越过窗口末尾必须拒绝"
        );

        let mut zero = candidate("fact", "", "每周五晚上打本", 0.5);
        zero.source_refs = vec!["m0".into()];
        assert!(validate_candidate(&zero, &w).is_none(), "m0 不是合法行号");

        let mut none = candidate("fact", "", "每周五晚上打本", 0.5);
        none.source_refs.clear();
        assert!(validate_candidate(&none, &w).is_none(), "无引用必须拒绝");

        let mut junk = candidate("fact", "", "每周五晚上打本", 0.5);
        junk.source_refs = vec!["m1".into(), "不是行号".into()];
        assert!(
            validate_candidate(&junk, &w).is_none(),
            "部分越界也整条拒绝"
        );

        for bad in ["gossip", "", "FACT!"] {
            assert!(
                validate_candidate(&candidate(bad, "", "内容在这里", 0.5), &w).is_none(),
                "{bad} 不在既有集合内，必须拒绝"
            );
        }

        let long = "字".repeat(MAX_MEMORY_CONTENT_CHARS + 1);
        assert!(validate_candidate(&candidate("fact", "", &long, 0.5), &w).is_none());
        let edge = "字".repeat(MAX_MEMORY_CONTENT_CHARS);
        assert!(validate_candidate(&candidate("fact", "", &edge, 0.5), &w).is_some());

        let good = candidate("fact", "", "住在杭州", 0.5);
        let gossip = candidate("gossip", "", "八卦", 0.5);
        assert!(validate_candidate(&good, &w).is_some());
        assert!(validate_candidate(&gossip, &w).is_none());
    }

    #[test]
    fn confidence_is_clamped_to_the_extraction_ceiling() {
        let w = window();
        let high = validate_candidate(&candidate("fact", "", "住在杭州", 5.0), &w).expect("应保留");
        assert_eq!(high.confidence, MAX_EXTRACTED_CONFIDENCE);
        let negative =
            validate_candidate(&candidate("fact", "", "住在杭州", -2.0), &w).expect("应保留");
        assert_eq!(negative.confidence, 0.0);
        let nan =
            validate_candidate(&candidate("fact", "", "住在杭州", f64::NAN), &w).expect("应保留");
        assert_eq!(nan.confidence, MAX_EXTRACTED_CONFIDENCE);
        // A normal value passes through untouched.
        let normal =
            validate_candidate(&candidate("fact", "", "住在杭州", 0.55), &w).expect("应保留");
        assert!((normal.confidence - 0.55).abs() < 1e-9);
    }

    #[test]
    fn source_refs_map_back_to_real_row_ids() {
        let w = window();
        let mut c = candidate("event", "", "周五九点集合", 0.5);
        c.source_refs = vec!["m1".into(), "m2".into(), "m1".into()];
        let valid = validate_candidate(&c, &w).expect("应通过");
        // Position 1 and 2 of the window, deduplicated.
        assert_eq!(valid.source_ids, vec![11, 12]);
    }

    #[test]
    fn near_duplicate_detection_uses_the_bigram_overlap() {
        assert!(is_near_duplicate(
            "每周五晚上一起打本",
            "每周五晚上一起打本"
        ));
        assert!(is_near_duplicate(
            "每周五晚上一起打本吧",
            "每周五晚上一起打本"
        ));
        assert!(!is_near_duplicate(
            "每周五晚上一起打本",
            "住在杭州喜欢喝咖啡"
        ));
    }

    #[test]
    fn subjects_are_normalised_before_comparison() {
        assert_eq!(normalize_subject(" U1 "), "u1");
        assert_eq!(normalize_subject("小林"), "小林");
    }

    #[test]
    fn subject_aliases_resolve_and_unknown_ones_become_group_level() {
        let aliases = aliases();
        assert_eq!(resolve_subject("u1", &aliases).as_deref(), Some("42"));
        assert_eq!(resolve_subject(" U2 ", &aliases).as_deref(), Some("43"));
        // A raw id is not an alias: the model must not be able to name a stranger.
        assert_eq!(resolve_subject("42", &aliases), None);
        assert_eq!(resolve_subject("", &aliases), None);
    }

    // --- extraction against a real store -------------------------------------------

    async fn extract_store() -> (Arc<SqliteStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("bot.sqlite3");
        let pool = crate::store::migrations::open_and_migrate(&path)
            .await
            .expect("迁移");
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group("9999", "500", 0).await.expect("建群");
        (store, dir)
    }

    #[tokio::test]
    async fn a_healthy_block_creates_rows_with_hy3_provenance() {
        let (store, _dir) = extract_store().await;
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "preference", "subject": "u1", "content": "每周五晚上一起打本",
                 "confidence": 0.7, "source_refs": ["m1"]},
                {"kind": "event", "subject": "", "content": "周五九点集合",
                 "confidence": 0.9, "source_refs": ["m2"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window(), &raw, &aliases(), 100)
            .await;
        assert_eq!(report.status, Some(CandidateParseStatus::Parsed));
        assert_eq!(report.created, 2);
        assert_eq!(report.rejected, 0);

        let rows = store.list_memories("9999", "500", 50).await.expect("查询");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.origin == crate::store::origin::HY3));
        // The subject alias became the real member id.
        assert!(
            rows.iter()
                .any(|r| r.subject_user_id.as_deref() == Some("42"))
        );
        // Confidence came back clamped, not as the model sent it.
        let event = rows.iter().find(|r| r.kind == "event").expect("事件");
        assert!((event.confidence - MAX_EXTRACTED_CONFIDENCE).abs() < 1e-9);
        // Refs are stored as real message ids, not line labels.
        assert!(event.source_message_ids.contains("12"));
        assert!(!event.source_message_ids.contains("\"m2\""));
    }

    #[tokio::test]
    async fn unbound_person_facts_and_placeholder_content_are_rejected() {
        let (store, _dir) = extract_store().await;
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                // A preference with no subject: unattributable, must not land.
                {"kind": "preference", "subject": "", "content": "不吃香菜",
                 "confidence": 0.7, "source_refs": ["m1"]},
                // The placeholder pathology: the extractor lost the person.
                {"kind": "fact", "subject": "", "content": "member偏好用杂鱼群友等称呼",
                 "confidence": 0.7, "source_refs": ["m1"]},
                // About the assistant: binds to the bot's own id.
                {"kind": "fact", "subject": "bot", "content": "被群友起名叫夜鹭",
                 "confidence": 0.7, "source_refs": ["m2"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window(), &raw, &aliases(), 100)
            .await;
        assert_eq!(report.created, 1);
        assert_eq!(report.rejected, 2);
        let rows = store.list_memories("9999", "500", 50).await.expect("查询");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_user_id.as_deref(), Some("9999"));
    }

    #[tokio::test]
    async fn a_missing_block_writes_nothing_and_reports_absent() {
        let (store, _dir) = extract_store().await;
        let report = Extractor::new(Arc::clone(&store))
            .run(
                "9999",
                "500",
                &window(),
                r#"{"summary": "摘要"}"#,
                &aliases(),
                100,
            )
            .await;
        assert_eq!(report.status, Some(CandidateParseStatus::Absent));
        assert_eq!(report.created, 0);
        assert!(
            store
                .list_memories("9999", "500", 50)
                .await
                .expect("查询")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_out_of_scope_ref_rejects_only_that_candidate() {
        let (store, _dir) = extract_store().await;
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "fact", "content": "越界的引用", "source_refs": ["m99"]},
                {"kind": "fact", "content": "合法的引用", "source_refs": ["m1"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window(), &raw, &aliases(), 100)
            .await;
        assert_eq!(report.created, 1);
        assert_eq!(report.rejected, 1);
        let rows = store.list_memories("9999", "500", 50).await.expect("查询");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "合法的引用");
    }

    #[tokio::test]
    async fn a_near_duplicate_of_an_agy_row_is_skipped_and_left_untouched() {
        let (store, _dir) = extract_store().await;
        let original = store
            .insert_memory(
                "9999",
                "500",
                Some("42"),
                "每周五晚上一起打本",
                "preference",
                "[11]",
                0.9,
                50,
            )
            .await
            .expect("原记忆");
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "preference", "subject": "u1", "content": "每周五晚上一起打本吧",
                 "confidence": 0.7, "source_refs": ["m1"]},
                {"kind": "preference", "subject": "u1", "content": "其实更喜欢在周末刷副本",
                 "confidence": 0.7, "source_refs": ["m1"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window(), &raw, &aliases(), 100)
            .await;
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.created, 1);

        let rows = store.list_memories("9999", "500", 50).await.expect("查询");
        assert_eq!(rows.len(), 2, "近重只跳过，不得新增也不得改旧行");
        // The agy row is byte-identical: same content, confidence and revision.
        let kept = rows
            .iter()
            .find(|r| r.memory_ref == original)
            .expect("旧行");
        assert_eq!(kept.content, "每周五晚上一起打本");
        assert_eq!(kept.origin, crate::store::origin::AGY);
        assert_eq!(kept.revision, 1);
        assert!((kept.confidence - 0.9).abs() < 1e-9);
        assert_eq!(kept.updated_at, 50);
    }

    #[tokio::test]
    async fn a_near_duplicate_of_a_hy3_row_is_also_skipped() {
        let (store, _dir) = extract_store().await;
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "event", "content": "周五九点集合", "source_refs": ["m2"]}
            ]
        })
        .to_string();
        let extractor = Extractor::new(Arc::clone(&store));
        let first = extractor
            .run("9999", "500", &window(), &raw, &aliases(), 100)
            .await;
        assert_eq!(first.created, 1);
        // A second fold over the same window proposes the same fact again.
        let second = extractor
            .run("9999", "500", &window(), &raw, &aliases(), 200)
            .await;
        assert_eq!(second.created, 0);
        assert_eq!(second.duplicates, 1);
        assert_eq!(
            store
                .list_memories("9999", "500", 50)
                .await
                .expect("查询")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn extraction_never_touches_a_retired_row() {
        let (store, _dir) = extract_store().await;
        let gone = store
            .insert_memory("9999", "500", None, "周五九点集合", "event", "[]", 0.6, 50)
            .await
            .expect("原记忆");
        sqlx::query("UPDATE memories SET archived_at = ? WHERE memory_ref = ?")
            .bind(51_i64)
            .bind(&gone)
            .execute(&store.pool)
            .await
            .expect("归档");
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "event", "content": "周五九点集合", "source_refs": ["m2"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window(), &raw, &aliases(), 100)
            .await;
        // A retired row is not a duplicate source: only active rows are compared.
        assert_eq!(report.created, 1);
        assert_eq!(report.duplicates, 0);
        // The retired row itself is untouched.
        let retired = store
            .memory_by_ref("9999", "500", &gone)
            .await
            .expect("查询")
            .expect("存在");
        assert!(retired.is_archived());
        assert_eq!(retired.revision, 1);
    }

    #[tokio::test]
    async fn the_audit_action_names_the_counts() {
        let report = ExtractionReport {
            status: Some(CandidateParseStatus::Parsed),
            created: 2,
            duplicates: 1,
            rejected: 0,
        };
        assert_eq!(
            report.audit_action("500"),
            "memory.extract.g500.parsed.n2.dup1.rej0"
        );
        assert_eq!(
            audit_action("500", &report),
            "memory.extract.g500.parsed.n2.dup1.rej0"
        );
    }
    #[tokio::test]
    async fn an_unanchored_owner_fact_is_anchored_to_the_owner() {
        let (store, _dir) = extract_store().await;
        let mut owner_msg = record(21, "42", "在的");
        owner_msg.speaker_role = "owner".into();
        let window = vec![owner_msg, record(22, "43", "173左右吧，76.5左右胯高我记得")];
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "fact", "subject": "", "content": "主人身高约173cm，胯高约76.5cm",
                 "confidence": 0.7, "source_refs": ["m2"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window, &raw, &aliases(), 100)
            .await;
        assert_eq!(report.created, 1);
        let rows = store.list_memories("9999", "500", 50).await.expect("查询");
        assert_eq!(rows[0].subject_user_id.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn an_unanchored_fact_is_anchored_to_a_named_speaker() {
        let (store, _dir) = extract_store().await;
        let mut ds_msg = record(22, "43", "我住在硬盘里");
        ds_msg.display_name = Some("DS大肥鱼".into());
        let window = vec![record(21, "42", "路过"), ds_msg];
        let raw = serde_json::json!({
            "summary": "摘要",
            "memories": [
                {"kind": "fact", "subject": "", "content": "大肥鱼自述住在硬盘里没有空调，害怕断电",
                 "confidence": 0.7, "source_refs": ["m2"]}
            ]
        })
        .to_string();
        let report = Extractor::new(Arc::clone(&store))
            .run("9999", "500", &window, &raw, &aliases(), 100)
            .await;
        assert_eq!(report.created, 1);
        let rows = store.list_memories("9999", "500", 50).await.expect("查询");
        assert_eq!(rows[0].subject_user_id.as_deref(), Some("43"));
    }
}

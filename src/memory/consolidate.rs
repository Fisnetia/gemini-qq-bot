//! Second-pass memory consolidation: merge within a subject cluster and forget the
//! rows the merge replaced (`origin = 'hy3'`, soft archive only).
//!
//! Extraction ([`crate::memory::extract`]) can only ever add rows, so a group that has
//! been chatty for months accumulates restatements of the same fact — "周五晚上打本"
//! said five times in five ways. Retrieval spends its budget on those duplicates and
//! the agent sees five near-synonyms where one canonical line would do. This module is
//! the periodic pass that folds a cluster back down.
//!
//! Two directions matter and they are deliberately opposite:
//!
//! * **Fail-safe towards forgetting** (this module): a malformed reply, a timeout, or
//!   a cluster the server cannot re-validate means the whole round is skipped. Never
//!   archive on a guess — a memory lost here is lost silently, and the chat log it came
//!   from may already be trimmed.
//! * **Fail-open towards remembering** ([`crate::memory::digest`]): a missing read-side
//!   digest just falls back to the raw list.
//!
//! Archiving is one-way for writers: [`crate::store::SqliteStore::archive_memory`] sets
//! `archived_at` and nothing else, so the row stays readable for the panel and for
//! audit while dropping out of retrieval. The merged replacement carries the union of
//! the sources' message refs, so provenance survives the fold.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::contract::{MAX_MEMORY_CONTENT_CHARS, MEMORY_KINDS};
use crate::memory::compaction::{SummaryModel, leaks_identifier};
use crate::memory::extract::{
    MAX_EXTRACTED_CONFIDENCE, is_near_duplicate, json_body, normalize_subject,
};
use crate::memory::retrieval::is_operational_memory;
use crate::store::{MemoryRecord, SqliteStore, origin};

/// Active rows a group may hold before a consolidation pass is worth a model call.
///
/// Below this the retrieval budget is not yet fighting duplicates, and every pass costs
/// real tokens on every group, every cycle.
pub const ACTIVE_MEMORY_LIMIT: usize = 40;

/// Most entries handed to one pass. A request that covered every cluster in a huge
/// group would blow the prompt budget and the model's attention; the tail is picked up
/// next cycle.
pub const MAX_CONSOLIDATE_ENTRIES: usize = 40;

/// Most merges honoured from one reply.
pub const MAX_MERGES: usize = 8;

/// A cluster must hold at least this many rows before merging is meaningful.
pub const MIN_CLUSTER_SIZE: usize = 2;

/// Hard wall-clock cap for one consolidation call. It runs inside the background fold
/// loop, never on a turn's critical path, so a generous cap is affordable — but not
/// unbounded: the loop must still finish.
///
/// The cap matches the aux HTTP client's own ceiling. Production evidence
/// (2026-11-03/04): at 30s every pass died at exactly 30.0s — the gateway queues
/// these background calls behind interactive traffic, and the old cap sat below
/// the queue's drain time, so consolidation never landed at all.
pub const CONSOLIDATE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a row the agent endorsed stays out of reach of the forget pass.
///
/// The agent only writes when a human corrected it ([`crate::agent::prompt`] rule 2), so
/// its recent rows are the most deliberate statements in the store. Seven days keeps a
/// fresh correction from being merged away by the next cycle.
pub const PROTECT_WINDOW_SECONDS: i64 = 7 * 86_400;

/// How one pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationStatus {
    /// Active rows are at or below [`ACTIVE_MEMORY_LIMIT`]; nothing was attempted.
    BelowThreshold,
    /// Over the limit, but no subject has two rows to merge.
    Unclustered,
    /// The model call failed or timed out: nothing archived.
    Failed,
    /// The reply was unreadable: nothing archived.
    Malformed,
    /// At least one merge was considered; see the counters for what landed.
    Applied,
}

impl ConsolidationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConsolidationStatus::BelowThreshold => "below",
            ConsolidationStatus::Unclustered => "flat",
            ConsolidationStatus::Failed => "failed",
            ConsolidationStatus::Malformed => "malformed",
            ConsolidationStatus::Applied => "applied",
        }
    }
}

/// What one pass did. Diagnostic only: nothing downstream branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConsolidationReport {
    pub status: Option<ConsolidationStatus>,
    /// Merge units the server accepted and applied.
    pub merged: usize,
    /// Rows created (one per applied merge).
    pub created: usize,
    /// Rows retired.
    pub archived: usize,
    /// Merge units skipped because they named a protected row.
    pub protected: usize,
    /// Merge units rejected by re-validation, plus failed archive writes.
    pub rejected: usize,
    /// Why a `Failed` pass failed: "timeout", "call" or "store". Diagnostic
    /// only, like the counters — it exists so the audit trail can tell a
    /// gateway that never answered from a store that refused a read (the
    /// bare `failed` bucket hid the 30s timeout kills for weeks).
    pub failure: Option<&'static str>,
}

impl ConsolidationReport {
    fn new(status: ConsolidationStatus) -> Self {
        Self {
            status: Some(status),
            ..Self::default()
        }
    }

    fn failed(kind: &'static str) -> Self {
        Self {
            status: Some(ConsolidationStatus::Failed),
            failure: Some(kind),
            ..Self::default()
        }
    }

    /// Audit action summarising this pass, in the same shape as `memory.extract.*`.
    pub fn audit_action(&self, group_id: &str) -> String {
        let status = match self.status.unwrap_or(ConsolidationStatus::Applied) {
            ConsolidationStatus::Failed => match self.failure {
                Some(kind) => format!("failed.{kind}"),
                None => "failed".to_string(),
            },
            other => other.as_str().to_string(),
        };
        format!(
            "memory.consolidate.g{group_id}.{status}.n{}.arch{}.prot{}.rej{}",
            self.created, self.archived, self.protected, self.rejected
        )
    }
}

/// One merge proposal exactly as the model gave it, before re-validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedProposal {
    /// Group-local alias (`u3`) or empty for a group-level cluster.
    pub subject: String,
    pub kind: String,
    pub content: String,
    /// The `memory_ref` keys this merge claims to replace.
    pub archive: Vec<String>,
}

/// Read the `merges` array out of a consolidation reply.
///
/// `None` means the whole body was unreadable (not JSON, or no `merges` key) — the
/// caller must then leave every row alone. An empty array is a legitimate "nothing to
/// merge" answer.
pub fn parse_merges(raw: &str) -> Option<Vec<MergedProposal>> {
    let body = json_body(raw)?;
    let items = body.get("merges")?.as_array()?;
    let mut out = Vec::new();
    for item in items.iter().take(MAX_MERGES) {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if content.is_empty() {
            continue;
        }
        let kind = item
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let subject = item
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let archive = item
            .get("archive")
            .and_then(|v| v.as_array())
            .map(|refs| {
                refs.iter()
                    .filter_map(|r| r.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        out.push(MergedProposal {
            subject,
            kind,
            content,
            archive,
        });
    }
    Some(out)
}

/// Render the consolidation request.
///
/// Entries are grouped by subject so the model sees exactly the clusters the server
/// will hold it to; the keys are shown because they come back in the reply and are
/// re-checked against the same set.
pub fn render_prompt(clusters: &[(String, Vec<&MemoryRecord>)]) -> String {
    let mut out = String::new();
    out.push_str(
        "你在整理一个 QQ 群的长期记忆库。下面按「关于谁」分好了组，每组是同一个人的同一批记忆条目，\
很可能有重复、过时或互相矛盾的表述。\n\n\
请你判断每组是否值得合并：\n\
- 只有确实在说同一件事的条目才合并；说法不同但事实不同的，分开保留（不要召唤进结果里）。\n\
- 合并后的 `content` 用一句第三人称中文陈述，不超过 200 字，保留所有仍然成立的关键细节\
（时间、偏好、称呼、约定），删掉重复与已被推翻的旧说法。\n\
- `kind` 从 fact / preference / topic / relation / event 里选一个最贴合的。\n\
- `archive` 列出被这次合并取代的条目键，只能是本组里出现过的键，至少 2 个。\n\
- 不要输出群号、QQ 号、消息 id、CQ 码，正文里也不要出现条目键。\n\
- 没有值得合并的组就不输出它；全部都不值得合并就输出空数组。\n\n\
只输出这一个 JSON 对象，不要任何解释或代码块以外的文字：\n\
{\"merges\":[{\"subject\":\"u1\",\"kind\":\"preference\",\"content\":\"……\",\"archive\":[\"mem_xxx\",\"mem_yyy\"]}]}\n\n",
    );
    for (label, rows) in clusters {
        out.push_str(&format!("# 分组 {label}（{} 条）\n", rows.len()));
        for row in rows {
            out.push_str(&format!(
                "- {} ({}, c{:.2}) {}\n",
                row.memory_ref, row.kind, row.confidence, row.content
            ));
        }
        out.push('\n');
    }
    out
}

/// Cluster active rows by subject.
///
/// Only clusters with at least [`MIN_CLUSTER_SIZE`] rows are returned — a singleton has
/// nothing to merge. Clusters are ordered largest first, then by most recent activity,
/// and the total number of rows handed to the model is capped at
/// [`MAX_CONSOLIDATE_ENTRIES`].
pub fn cluster(rows: &[MemoryRecord]) -> Vec<(String, Vec<&MemoryRecord>)> {
    let mut grouped: BTreeMap<String, Vec<&MemoryRecord>> = BTreeMap::new();
    for row in rows.iter().filter(|row| !row.is_archived()) {
        let key = row
            .subject_user_id
            .as_deref()
            .map(normalize_subject)
            .unwrap_or_default();
        grouped.entry(key).or_default().push(row);
    }
    let mut clusters: Vec<(String, Vec<&MemoryRecord>)> = grouped
        .into_iter()
        .filter(|(_, rows)| rows.len() >= MIN_CLUSTER_SIZE)
        .collect();
    clusters.sort_by(|a, b| {
        let a_recent = a.1.iter().map(|r| r.updated_at).max().unwrap_or(0);
        let b_recent = b.1.iter().map(|r| r.updated_at).max().unwrap_or(0);
        b.1.len().cmp(&a.1.len()).then(b_recent.cmp(&a_recent))
    });
    let mut budget = MAX_CONSOLIDATE_ENTRIES;
    let mut kept = Vec::new();
    for (key, rows) in clusters {
        if budget == 0 {
            break;
        }
        let take = rows.len().min(budget);
        budget -= take;
        kept.push((key, rows.into_iter().take(take).collect()));
    }
    kept
}

/// Display label for a cluster key: the group-local alias when one is known.
///
/// `aliases` is `(alias, user_id)`, the same shape the fold loop and
/// [`crate::memory::extract`] use.
fn cluster_label(key: &str, aliases: &[(String, String)]) -> String {
    if key.is_empty() {
        return "群".to_string();
    }
    aliases
        .iter()
        .find(|(_, user_id)| normalize_subject(user_id) == key)
        .map(|(alias, _)| alias.clone())
        .unwrap_or_else(|| "群成员".to_string())
}

fn endorsed(origin_name: &str) -> bool {
    origin_name == origin::AGY || origin_name == origin::PANEL
}

/// True when a row is inside the protection window.
///
/// Rows a person last wrote are protected: the agent (`agy`) or the panel.
/// An `hy3` row is a machine guess, so age buys it nothing.
pub fn is_protected(row: &MemoryRecord, now: i64) -> bool {
    if !endorsed(&row.origin) {
        return false;
    }
    let touched = row.updated_at.max(row.created_at);
    now.saturating_sub(touched) <= PROTECT_WINDOW_SECONDS
}

/// Rows to archive before the model pass: hy3 runtime bookkeeping, and the
/// loser of a near-duplicate pair. A protected agy row is never the loser.
/// Both protected means the pair stays.
pub fn paraphrase_losers(rows: &[MemoryRecord], now: i64) -> BTreeSet<String> {
    let mut doomed = BTreeSet::new();
    for row in rows.iter().filter(|row| row.archived_at.is_none()) {
        if !endorsed(&row.origin) && is_operational_memory(&row.content) {
            doomed.insert(row.memory_ref.clone());
        }
    }
    let live: Vec<&MemoryRecord> = rows
        .iter()
        .filter(|row| row.archived_at.is_none() && !doomed.contains(&row.memory_ref))
        .collect();
    for i in 0..live.len() {
        for j in (i + 1)..live.len() {
            // Different people can say similar things. Only restatements of the
            // same subject (including two unattributed rows) are the same claim.
            if live[i].subject_user_id != live[j].subject_user_id {
                continue;
            }
            if !is_near_duplicate(&live[i].content, &live[j].content) {
                continue;
            }
            if let Some(loser) = paraphrase_loser(live[i], live[j], now) {
                doomed.insert(loser.to_string());
            }
        }
    }
    doomed
}

fn paraphrase_loser<'a>(a: &'a MemoryRecord, b: &'a MemoryRecord, now: i64) -> Option<&'a str> {
    let a_prot = is_protected(a, now);
    let b_prot = is_protected(b, now);
    if a_prot && b_prot {
        return None;
    }
    if a_prot {
        return Some(b.memory_ref.as_str());
    }
    if b_prot {
        return Some(a.memory_ref.as_str());
    }
    let a_kept = endorsed(&a.origin);
    let b_kept = endorsed(&b.origin);
    if a_kept != b_kept {
        return Some(if a_kept {
            b.memory_ref.as_str()
        } else {
            a.memory_ref.as_str()
        });
    }
    if a.updated_at == b.updated_at {
        return Some(if a.memory_ref > b.memory_ref {
            a.memory_ref.as_str()
        } else {
            b.memory_ref.as_str()
        });
    }
    Some(if a.updated_at < b.updated_at {
        a.memory_ref.as_str()
    } else {
        b.memory_ref.as_str()
    })
}

/// The message ids a row cites, as a sorted set.
fn source_ids(row: &MemoryRecord) -> BTreeSet<i64> {
    serde_json::from_str::<Vec<i64>>(&row.source_message_ids)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// Performs one consolidation pass for one group.
#[derive(Debug, Clone)]
pub struct Consolidator {
    store: Arc<SqliteStore>,
    timeout: Duration,
}

impl Consolidator {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self {
            store,
            timeout: CONSOLIDATE_TIMEOUT,
        }
    }

    /// Shorten the wall-clock cap. Only tests need this; production keeps
    /// [`CONSOLIDATE_TIMEOUT`].
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Merge and forget, or leave the store untouched.
    ///
    /// Every failure mode returns a report instead of an error: this runs in a
    /// background loop, and a store hiccup must not turn into a retry storm over the
    /// same rows.
    pub async fn run_once(
        &self,
        bot_id: &str,
        group_id: &str,
        model: &dyn SummaryModel,
        aliases: &[(String, String)],
        now: i64,
    ) -> ConsolidationReport {
        let mut active = match self.store.list_memories(bot_id, group_id, 500).await {
            Ok(rows) => rows,
            Err(_) => return ConsolidationReport::failed("store"),
        };
        // Paraphrases and runtime bookkeeping are archived before the model
        // threshold. A group of 31 rows never reached that call, so the same
        // claim sat in the store three times.
        let doomed = paraphrase_losers(&active, now);
        for memory_ref in &doomed {
            let _ = self
                .store
                .archive_memory(bot_id, group_id, memory_ref, "paraphrase", now)
                .await;
        }
        if !doomed.is_empty() {
            active.retain(|row| !doomed.contains(&row.memory_ref));
        }
        if active.len() <= ACTIVE_MEMORY_LIMIT {
            return ConsolidationReport::new(ConsolidationStatus::BelowThreshold);
        }
        let clusters = cluster(&active);
        if clusters.is_empty() {
            return ConsolidationReport::new(ConsolidationStatus::Unclustered);
        }
        let labelled: Vec<(String, Vec<&MemoryRecord>)> = clusters
            .iter()
            .map(|(key, rows)| (cluster_label(key, aliases), rows.clone()))
            .collect();
        let prompt = render_prompt(&labelled);

        // A timeout or transport failure is a "no opinion" answer, not an empty one:
        // treat it exactly like a malformed reply and forget nothing.
        let raw = match tokio::time::timeout(self.timeout, model.summarise(prompt)).await {
            Ok(Ok(raw)) => raw,
            Ok(Err(_)) => return ConsolidationReport::failed("call"),
            Err(_) => return ConsolidationReport::failed("timeout"),
        };
        let Some(proposals) = parse_merges(&raw) else {
            return ConsolidationReport::new(ConsolidationStatus::Malformed);
        };

        let mut report = ConsolidationReport::new(ConsolidationStatus::Applied);
        let task_id = format!("t{now:x}");
        for proposal in &proposals {
            if self
                .apply_merge(
                    bot_id,
                    group_id,
                    &clusters,
                    proposal,
                    &task_id,
                    now,
                    &mut report,
                )
                .await
                .is_none()
            {
                // Re-validation refused this unit; the counter is already bumped.
                continue;
            }
        }
        report
    }

    /// Apply one merge unit, or return `None` when it was refused whole.
    ///
    /// Refusal is all-or-nothing: the replacement row and the retirements go together,
    /// so a unit can never archive a row and then fail to keep its content.
    #[allow(clippy::too_many_arguments)]
    async fn apply_merge(
        &self,
        bot_id: &str,
        group_id: &str,
        clusters: &[(String, Vec<&MemoryRecord>)],
        proposal: &MergedProposal,
        task_id: &str,
        now: i64,
        report: &mut ConsolidationReport,
    ) -> Option<()> {
        let cluster = clusters
            .iter()
            .find(|(label, _)| normalize_subject(label) == normalize_subject(&proposal.subject))
            .or_else(|| {
                // A cluster label may be "群成员" for an alias we could not resolve; fall
                // back to locating the cluster that owns every cited key.
                clusters.iter().find(|(_, rows)| {
                    proposal
                        .archive
                        .iter()
                        .all(|r| rows.iter().any(|row| &row.memory_ref == r))
                })
            })?;
        let rows = &cluster.1;

        if !MEMORY_KINDS.contains(&proposal.kind.as_str()) {
            report.rejected += 1;
            return None;
        }
        if proposal.content.chars().count() > MAX_MEMORY_CONTENT_CHARS
            || leaks_identifier(&proposal.content)
        {
            report.rejected += 1;
            return None;
        }
        // Every cited key must live in this cluster and the unit must actually fold
        // something: a one-row "merge" would only churn the revision, and a key from
        // another subject means the model lost track of the partition.
        let mut replaced: Vec<&MemoryRecord> = Vec::new();
        for key in &proposal.archive {
            let Some(row) = rows.iter().find(|row| &row.memory_ref == key) else {
                report.rejected += 1;
                return None;
            };
            if !replaced.iter().any(|r| r.memory_ref == row.memory_ref) {
                replaced.push(row);
            }
        }
        if replaced.len() < MIN_CLUSTER_SIZE {
            report.rejected += 1;
            return None;
        }
        // Protection is checked per unit, not per row: retiring half a cluster while
        // leaving the protected half active would keep both the new row and the fact it
        // is supposed to replace.
        if replaced.iter().any(|row| is_protected(row, now)) {
            report.protected += 1;
            return None;
        }

        let confidence = replaced
            .iter()
            .map(|row| row.confidence)
            .fold(f64::INFINITY, f64::min)
            .min(MAX_EXTRACTED_CONFIDENCE);
        let confidence = if confidence.is_finite() {
            confidence
        } else {
            MAX_EXTRACTED_CONFIDENCE
        };
        let ids: BTreeSet<i64> = replaced.iter().flat_map(|row| source_ids(row)).collect();
        let source_ids = serde_json::to_string(&ids.into_iter().collect::<Vec<i64>>())
            .unwrap_or_else(|_| "[]".into());

        let created = self
            .store
            .insert_memory_with_origin(
                bot_id,
                group_id,
                replaced
                    .first()
                    .and_then(|row| row.subject_user_id.as_deref()),
                &proposal.content,
                &proposal.kind,
                &source_ids,
                confidence,
                origin::HY3,
                now,
            )
            .await;
        if created.is_err() {
            report.rejected += 1;
            return None;
        }
        report.merged += 1;
        report.created += 1;

        for row in replaced {
            match self
                .store
                .archive_memory(bot_id, group_id, &row.memory_ref, task_id, now)
                .await
            {
                Ok(true) => report.archived += 1,
                // Already archived by someone else: not a failure, just nothing to do.
                Ok(false) => {}
                Err(_) => report.rejected += 1,
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorClass;
    use crate::memory::compaction::SummaryModel;

    fn row(
        n: usize,
        subject: Option<&str>,
        content: &str,
        conf: f64,
        origin: &str,
        at: i64,
    ) -> MemoryRecord {
        MemoryRecord {
            memory_ref: format!("mem_{n}"),
            bot_id: "bot".into(),
            group_id: "100".into(),
            subject_user_id: subject.map(|s| s.to_string()),
            content: content.into(),
            kind: "fact".into(),
            source_message_ids: format!("[{}]", n),
            confidence: conf,
            revision: 1,
            origin: origin.into(),
            archived_at: None,
            created_at: at,
            updated_at: at,
        }
    }

    struct Stub {
        reply: Result<String, ErrorClass>,
        delay: Option<Duration>,
    }

    #[async_trait::async_trait]
    impl SummaryModel for Stub {
        async fn summarise(&self, _prompt: String) -> Result<String, ErrorClass> {
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            self.reply.clone()
        }
    }

    fn ok(reply: &str) -> Stub {
        Stub {
            reply: Ok(reply.into()),
            delay: None,
        }
    }

    async fn seeded_store(
        rows: &[(Option<String>, String, &'static str, i64)],
    ) -> (tempfile::TempDir, Arc<SqliteStore>) {
        let dir = tempfile::tempdir().expect("tmp");
        let pool = crate::store::migrations::open_temp(dir.path())
            .await
            .expect("迁移");
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group("bot", "100", 1).await.expect("建组");
        store.ensure_member("bot", "100", "7", 1).await.expect("人");
        for (subject, content, origin, at) in rows {
            store
                .insert_memory_with_origin(
                    "bot",
                    "100",
                    subject.as_deref(),
                    content,
                    "fact",
                    "[1]",
                    0.6,
                    origin,
                    *at,
                )
                .await
                .expect("写记忆");
        }
        (dir, store)
    }

    /// Fill a group past the threshold with singleton rows on distinct subjects, so the
    /// cluster under test is the only one with something to merge and the entry budget is
    /// not spent on filler.
    fn filler() -> Vec<(Option<String>, String, &'static str, i64)> {
        (0..ACTIVE_MEMORY_LIMIT)
            .map(|n| {
                (
                    Some(format!("f{n}")),
                    format!("填满阈值用的无关事实{n}"),
                    origin_static_hy3(),
                    n as i64,
                )
            })
            .collect()
    }

    fn origin_static_hy3() -> &'static str {
        crate::store::origin::HY3
    }

    fn m(
        subject: Option<&str>,
        content: &str,
        origin: &'static str,
        at: i64,
    ) -> (Option<String>, String, &'static str, i64) {
        (
            subject.map(|s| s.to_string()),
            content.to_string(),
            origin,
            at,
        )
    }

    #[tokio::test]
    async fn a_merge_creates_the_canonical_row_and_archives_its_sources() {
        let mut rows = filler();
        rows.push(m(Some("7"), "喜欢喝咖啡", origin_static_hy3(), 100));
        rows.push(m(Some("7"), "每天一杯咖啡", origin_static_hy3(), 200));
        let (_dir, store) = seeded_store(&rows).await;
        let before = store.list_memories("bot", "100", 500).await.expect("读");
        let keys: Vec<String> = before
            .iter()
            .filter(|r| r.subject_user_id.as_deref() == Some("7"))
            .map(|r| r.memory_ref.clone())
            .collect();
        assert_eq!(keys.len(), 2);
        let reply = serde_json::json!({
            "merges": [{
                "subject": "u1",
                "kind": "preference",
                "content": "他爱喝咖啡，每天一杯。",
                "archive": [keys[0], keys[1]],
            }]
        })
        .to_string();
        let consolidator = Consolidator::new(Arc::clone(&store));
        let aliases = vec![("u1".to_string(), "7".to_string())];
        let report = consolidator
            .run_once("bot", "100", &ok(&reply), &aliases, 1_000)
            .await;
        assert_eq!(report.status, Some(ConsolidationStatus::Applied));
        assert_eq!(report.created, 1);
        assert_eq!(report.archived, 2);

        let active = store.list_memories("bot", "100", 500).await.expect("读");
        assert!(
            active.iter().all(|r| !keys.contains(&r.memory_ref)),
            "被合并的行必须退出召回"
        );
        let merged = active
            .iter()
            .find(|r| r.content.contains("每天一杯"))
            .expect("规范条目应在");
        assert_eq!(merged.origin, origin::HY3);
        assert_eq!(merged.confidence, 0.6, "取来源最小值");
        assert_eq!(merged.source_message_ids, "[1]", "source_refs 取并集后去重");

        let audits: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events")
            .fetch_all(store.pool())
            .await
            .expect("读审计");
        assert!(
            audits.iter().any(|a| a.starts_with("memory.archive.task")),
            "缺归档审计"
        );
    }

    #[tokio::test]
    async fn a_recent_agent_row_blocks_its_whole_merge_unit() {
        let mut rows = filler();
        rows.push(m(Some("7"), "喜欢喝咖啡", origin::AGY, 900));
        rows.push(m(Some("7"), "每天一杯咖啡", origin_static_hy3(), 100));
        let (_dir, store) = seeded_store(&rows).await;
        // The agent row was touched at 900; a pass at 1000 is inside the window.
        let keys: Vec<String> = store
            .list_memories("bot", "100", 500)
            .await
            .expect("读")
            .iter()
            .filter(|r| r.subject_user_id.as_deref() == Some("7"))
            .map(|r| r.memory_ref.clone())
            .collect();
        let reply = serde_json::json!({
            "merges": [{
                "subject": "u1",
                "kind": "preference",
                "content": "他爱喝咖啡。",
                "archive": [keys[0], keys[1]],
            }]
        })
        .to_string();
        let consolidator = Consolidator::new(Arc::clone(&store));
        let aliases = vec![("u1".to_string(), "7".to_string())];
        let report = consolidator
            .run_once("bot", "100", &ok(&reply), &aliases, 1_000)
            .await;
        assert_eq!(report.protected, 1);
        assert_eq!(report.archived, 0);
        assert_eq!(report.created, 0, "保护组不得留下新条目");
    }

    #[tokio::test]
    async fn a_malformed_reply_archives_nothing() {
        let mut rows = filler();
        rows.push(m(Some("7"), "喜欢喝咖啡", origin_static_hy3(), 100));
        rows.push(m(Some("7"), "每天一杯咖啡", origin_static_hy3(), 200));
        let (_dir, store) = seeded_store(&rows).await;
        let before = store
            .list_memories("bot", "100", 500)
            .await
            .expect("读")
            .len();
        let consolidator = Consolidator::new(Arc::clone(&store));
        let report = consolidator
            .run_once("bot", "100", &ok("我觉得都挺好的"), &[], 1_000)
            .await;
        assert_eq!(report.status, Some(ConsolidationStatus::Malformed));
        assert_eq!(report.archived, 0);
        let after = store
            .list_memories("bot", "100", 500)
            .await
            .expect("读")
            .len();
        assert_eq!(before, after, "畸形回复不得改动任何行");
    }

    #[tokio::test]
    async fn a_timeout_archives_nothing() {
        let mut rows = filler();
        rows.push(m(Some("7"), "喜欢喝咖啡", origin_static_hy3(), 100));
        rows.push(m(Some("7"), "每天一杯咖啡", origin_static_hy3(), 200));
        let (_dir, store) = seeded_store(&rows).await;
        let slow = Stub {
            reply: Ok("{\"merges\":[]}".into()),
            delay: Some(Duration::from_millis(60)),
        };
        let consolidator =
            Consolidator::new(Arc::clone(&store)).with_timeout(Duration::from_millis(20));
        let report = consolidator.run_once("bot", "100", &slow, &[], 1_000).await;
        assert_eq!(report.status, Some(ConsolidationStatus::Failed));
        assert_eq!(report.failure, Some("timeout"));
        assert_eq!(
            report.audit_action("100"),
            "memory.consolidate.g100.failed.timeout.n0.arch0.prot0.rej0"
        );
        assert_eq!(report.archived, 0);
    }

    #[test]
    fn failed_audit_action_names_the_failure_kind() {
        assert_eq!(
            ConsolidationReport::failed("call").audit_action("100"),
            "memory.consolidate.g100.failed.call.n0.arch0.prot0.rej0"
        );
        assert_eq!(
            ConsolidationReport::failed("store").audit_action("100"),
            "memory.consolidate.g100.failed.store.n0.arch0.prot0.rej0"
        );
    }

    #[tokio::test]
    async fn below_the_threshold_makes_no_model_call() {
        let (_dir, store) =
            seeded_store(&[m(Some("7"), "喜欢喝咖啡", origin_static_hy3(), 1)]).await;
        let boom = Stub {
            reply: Err(ErrorClass::AuxFailed),
            delay: None,
        };
        let consolidator = Consolidator::new(Arc::clone(&store));
        let report = consolidator.run_once("bot", "100", &boom, &[], 1_000).await;
        assert_eq!(report.status, Some(ConsolidationStatus::BelowThreshold));
    }

    #[test]
    fn below_the_limit_is_not_worth_a_call() {
        let rows: Vec<MemoryRecord> = (0..ACTIVE_MEMORY_LIMIT)
            .map(|n| row(n, Some("u1"), "同一件事", 0.5, origin::HY3, 100))
            .collect();
        assert!(cluster(&rows).len() == 1);
        // The threshold itself is exclusive: exactly ACTIVE_MEMORY_LIMIT is below.
        assert!(rows.len() <= ACTIVE_MEMORY_LIMIT);
    }

    #[test]
    fn archived_rows_do_not_cluster() {
        let mut a = row(1, Some("u1"), "喜欢喝咖啡", 0.5, origin::HY3, 100);
        a.archived_at = Some(100);
        let b = row(2, Some("u1"), "每天一杯咖啡", 0.5, origin::HY3, 100);
        let rows = [a, b];
        let clusters = cluster(&rows);
        assert!(clusters.is_empty(), "one live row is not a cluster");
    }

    #[test]
    fn clusters_are_grouped_by_subject_and_sized_first() {
        let rows = vec![
            row(1, Some("u1"), "a", 0.5, origin::HY3, 10),
            row(2, Some("u1"), "b", 0.5, origin::HY3, 20),
            row(3, Some("u2"), "c", 0.5, origin::HY3, 30),
            row(4, None, "d", 0.5, origin::HY3, 40),
            row(5, None, "e", 0.5, origin::HY3, 50),
            row(6, None, "f", 0.5, origin::HY3, 60),
        ];
        let clusters = cluster(&rows);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].0, "", "group-level cluster is the largest");
        assert_eq!(clusters[0].1.len(), 3);
        assert_eq!(clusters[1].0, "u1");
    }

    #[test]
    fn the_entry_budget_truncates_instead_of_dropping_the_pass() {
        let mut rows = Vec::new();
        for n in 0..60 {
            rows.push(row(n, Some("u1"), "同一件事", 0.5, origin::HY3, n as i64));
        }
        let clusters = cluster(&rows);
        let total: usize = clusters.iter().map(|(_, r)| r.len()).sum();
        assert_eq!(total, MAX_CONSOLIDATE_ENTRIES);
    }

    #[test]
    fn protection_covers_only_recent_agent_rows() {
        let now = 1_000_000;
        let recent_agy = row(1, Some("u1"), "x", 0.5, origin::AGY, now - 10);
        let old_agy = row(
            2,
            Some("u1"),
            "x",
            0.5,
            origin::AGY,
            now - PROTECT_WINDOW_SECONDS - 1,
        );
        let recent_hy3 = row(3, Some("u1"), "x", 0.5, origin::HY3, now - 10);
        assert!(is_protected(&recent_agy, now));
        assert!(!is_protected(&old_agy, now));
        assert!(
            !is_protected(&recent_hy3, now),
            "a machine guess is not a correction"
        );
    }

    #[test]
    fn a_fenced_reply_is_read() {
        let raw = "```json\n{\"merges\":[{\"subject\":\"u1\",\"kind\":\"fact\",\"content\":\"他每周五打本\",\"archive\":[\"mem_1\",\"mem_2\"]}]}\n```";
        let merges = parse_merges(raw).expect("应能解析");
        assert_eq!(merges.len(), 1);
        assert_eq!(merges[0].archive, vec!["mem_1", "mem_2"]);
    }

    #[test]
    fn a_body_without_the_key_is_malformed_not_empty() {
        assert!(parse_merges("{\"summary\":\"什么都没说\"}").is_none());
        assert!(parse_merges("not json at all").is_none());
        assert_eq!(parse_merges("{\"merges\":[]}"), Some(Vec::new()));
    }

    #[test]
    fn the_prompt_shows_keys_and_hides_identifiers() {
        let rows = vec![
            row(1, Some("u1"), "喜欢喝咖啡", 0.5, origin::HY3, 10),
            row(2, Some("u1"), "每天一杯咖啡", 0.5, origin::HY3, 20),
        ];
        let clusters = cluster(&rows);
        let labelled = vec![("u1".to_string(), clusters[0].1.clone())];
        let prompt = render_prompt(&labelled);
        assert!(prompt.contains("mem_1"));
        assert!(prompt.contains("# 分组 u1（2 条）"));
        assert!(prompt.contains("\"merges\""));
    }

    #[test]
    fn audit_action_names_the_outcome_and_counters() {
        let mut report = ConsolidationReport::new(ConsolidationStatus::Applied);
        report.merged = 1;
        report.created = 1;
        report.archived = 2;
        assert_eq!(
            report.audit_action("100"),
            "memory.consolidate.g100.applied.n1.arch2.prot0.rej0"
        );
    }
}

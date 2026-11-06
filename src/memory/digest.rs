//! Per-turn memory digest: a derived, never-persisted view of the retrieved context.
//!
//! Retrieval hands the turn a list of scored rows. Injecting the rows raw makes the
//! prompt grow with the budget and spends most of that budget on entries the turn does
//! not end up needing. This module digests the same rows — plus whatever slice of the
//! compaction summary is topically relevant — into one short brief, computed fresh on
//! every turn.
//!
//! Three properties are load-bearing:
//!
//! * the digest is **derived state**: it is never written to the store, never becomes a
//!   memory, and is dropped with the turn that produced it;
//! * it is **optional**: any timeout, gateway error or malformed output falls back to the
//!   raw entry list the caller would have injected anyway. A digest must never stall a
//!   turn, so the extra wait is capped by [`DIGEST_WAIT`] and runs *inside* the settle
//!   window the pacing gate already opened;
//! * it still carries **store keys** (the `refs` array), because corrections are issued
//!   as `memory_ops.update` against a `memory_ref`. A brief with no keys would silently
//!   disable the agent's whole memory-write duty, so the keys are kept out of the prose
//!   and carried in a separate field.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::memory::compaction::{SummaryModel, leaks_identifier};
use crate::memory::extract::json_body;
use crate::memory::retrieval::{
    self, ContextMemory, MemoryContext, RetrievalInput, bigrams, overlap_score,
};
use crate::store::SqliteStore;

/// Hard character ceiling for the digest text.
pub const MAX_DIGEST_CHARS: usize = 1_200;
/// Hard ceiling for the number of expansion keywords.
pub const MAX_EXPANSION_TERMS: usize = 6;
/// Ceiling on the characters of summary the digest prompt may quote.
const MAX_SUMMARY_SOURCE_CHARS: usize = 1_200;
/// Ceiling on characters of one memory line inside the digest prompt.
const MAX_LINE_CHARS: usize = 160;
/// How long the digest may take before the turn falls back to the raw entry list.
///
/// This runs concurrently with the pacing gate, so the added latency is
/// `max(0, digest_time - gate_time)`, never a serial 8 seconds.
pub const DIGEST_WAIT: Duration = Duration::from_secs(8);
/// A digest older than this is stale: the batch it was built for has long gone.
const SLOT_TTL: Duration = Duration::from_secs(90);

/// One digest, ready to replace the raw entry list in the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    /// The prose brief.
    pub brief: String,
    /// Store keys of the entries the brief covers, in prompt order.
    pub refs: Vec<String>,
    /// Extra retrieval keywords the model suggested.
    pub expansion_terms: Vec<String>,
}

impl Digest {
    /// The block injected in place of the raw entry list.
    ///
    /// The brief is prose and carries no keys, so the keys the model kept are appended as
    /// a compact list: issuing a correction is `memory_ops.update` against a
    /// `memory_ref`, and a brief with no keys would silently disable that duty.
    pub fn render(&self) -> String {
        let mut out = self.brief.clone();
        if !self.refs.is_empty() {
            out.push_str("\n可更正的条目（update 时用这些键）：");
            out.push_str(&self.refs.join("，"));
        }
        out
    }
}

/// A digest request: what the model is allowed to see, already group-scoped.
#[derive(Debug, Clone)]
pub struct DigestRequest {
    /// Retrieved entries, in the order retrieval ranked them.
    pub entries: Vec<ContextMemory>,
    /// Compacted summary of this group, when one exists.
    pub summary: Option<String>,
    /// Plain text of the batch, for the "relevance" instruction.
    pub query_text: String,
}

impl DigestRequest {
    /// A digest is only worth a model call when there is something to digest.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Render the digest prompt. Contains aliased memory content plus store keys, which are
/// the only identifiers the agent is allowed to echo back.
pub fn render_prompt(request: &DigestRequest) -> String {
    let mut out = String::new();
    out.push_str(
        "下面是某个 QQ 群聊机器人关于这个群的长期记忆条目（每行开头是内部键），以及这批新消息。请把它消化成一段给机器人自己看的「前情摘要」。\n",
    );
    out.push_str("要求：\n");
    out.push_str("- 只保留和这批消息可能相关的内容；明显无关的条目直接丢掉。\n");
    out.push_str("- 用第三人称陈述事实，不要编造条目里没有的东西，不要评价。\n");
    out.push_str(&format!("- 不超过 {} 字。\n", MAX_DIGEST_CHARS));
    out.push_str(&format!(
        "- 另外给出最多 {} 个补充检索词（关键词，不是句子），用于补查记忆库里可能漏掉的条目。\n",
        MAX_EXPANSION_TERMS
    ));
    out.push_str("- 正文里不要出现任何键或数字标识；把摘要用到的条目的键原样放进 refs 数组。\n");
    out.push_str("- 不要输出群号、QQ 号、消息 id 或 CQ 码。\n");
    out.push_str(
        "- 只输出一个 JSON 对象，不要解释：{\"digest\":\"...\",\"refs\":[\"...\"],\"terms\":[\"...\"]}\n\n",
    );

    if let Some(summary) = &request.summary
        && !summary.trim().is_empty()
    {
        let clipped: String = summary
            .trim()
            .chars()
            .take(MAX_SUMMARY_SOURCE_CHARS)
            .collect();
        out.push_str("# 本群已有的压缩摘要（背景，可以只取相关部分）\n");
        out.push_str(&clipped);
        out.push_str("\n\n");
    }

    out.push_str("# 召回到的记忆条目\n");
    for entry in request.entries.iter() {
        let subject = match &entry.subject_ref {
            Some(s) => s.as_str(),
            None => "群",
        };
        let content: String = entry.content.trim().chars().take(MAX_LINE_CHARS).collect();
        out.push_str(&format!(
            "- {} [{subject}] ({}) {content}\n",
            entry.memory_ref, entry.kind
        ));
    }

    out.push_str("\n# 这批新消息\n");
    let query: String = request
        .query_text
        .trim()
        .chars()
        .take(MAX_SUMMARY_SOURCE_CHARS)
        .collect();
    out.push_str(&query);
    out.push('\n');
    out
}

/// Parse and validate the model's reply.
///
/// `known` is the set of store keys handed to the model; any key outside it is dropped
/// rather than trusted, so a hallucinated `memory_ref` can never reach the contract.
pub fn parse_digest(raw: &str, known: &[ContextMemory]) -> Result<Digest, DigestFail> {
    let value = json_body(raw).ok_or(DigestFail::Parse)?;
    let brief = value
        .get("digest")
        .or_else(|| value.get("summary"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if brief.is_empty() {
        return Err(DigestFail::Parse);
    }
    let brief: String = brief.chars().take(MAX_DIGEST_CHARS).collect();
    // A brief that echoes internal identifiers is worse than no brief: the agent could
    // copy them into a reply, and the fallback path exists precisely to avoid that.
    if leaks_identifier(&brief) {
        return Err(DigestFail::Leak);
    }
    let mut refs: Vec<String> = Vec::new();
    if let Some(items) = value.get("refs").and_then(|v| v.as_array()) {
        for item in items {
            let Some(key) = item.as_str() else { continue };
            let key = key.trim();
            if known.iter().any(|e| e.memory_ref == key) && !refs.iter().any(|r| r == key) {
                refs.push(key.to_string());
            }
        }
    }
    let terms = value
        .get("terms")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s.chars().count() <= 24)
                .filter(|s| !leaks_identifier(s))
                .take(MAX_EXPANSION_TERMS)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // A brief that names no key is still usable: the caller backfills the key
    // list from the very entries this digest was built from, so the agent's
    // correction duty survives. The old one-vote veto here discarded every
    // digest whose model omitted the refs array (2026-10-27: fallback rate
    // 100%, zero digests ever used).
    Ok(Digest {
        brief,
        refs,
        expansion_terms: terms,
    })
}

/// Why a digest did not happen, kept distinct because a 100% fallback rate hid
/// behind an undifferentiated `None` for a full day (2026-10-27): the audit
/// suffix is the only place this surfaces, so it has to carry the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFail {
    /// Nothing to digest.
    Empty,
    /// The summary model call itself failed.
    Model,
    /// The model answered, but not in the expected JSON shape.
    Parse,
    /// The brief would have leaked an internal identifier into the prompt.
    Leak,
    /// The wait budget ran out before the model finished.
    Timeout,
}

impl DigestFail {
    pub fn as_str(self) -> &'static str {
        match self {
            DigestFail::Empty => "empty",
            DigestFail::Model => "model",
            DigestFail::Parse => "parse",
            DigestFail::Leak => "leak",
            DigestFail::Timeout => "timeout",
        }
    }
}

/// Ask the model for a digest. Any failure is reported by kind and the caller
/// falls back to the raw entry list.
pub async fn digest(
    model: &dyn SummaryModel,
    request: &DigestRequest,
) -> Result<Digest, DigestFail> {
    if request.is_empty() {
        return Err(DigestFail::Empty);
    }
    let raw = model
        .summarise(render_prompt(request))
        .await
        .map_err(|_| DigestFail::Model)?;
    parse_digest(&raw, &request.entries)
}

/// Compute the extra recall the expansion terms are worth.
///
/// Deliberately arithmetic: the terms become additional query text for one more lexical
/// pass over the memories already in hand, so no second model call is needed.
pub fn expansion_query(terms: &[String]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    Some(terms.join(" "))
}

/// Score one entry against an expansion query, reusing the lexical overlap measure.
pub fn expansion_score(terms: &[String], content: &str) -> f64 {
    let Some(query) = expansion_query(terms) else {
        return 0.0;
    };
    let query = bigrams(&query);
    let memory: BTreeSet<String> = bigrams(content);
    overlap_score(&query, &memory)
}

/// Audit action for one turn's digest outcome.
///
/// Kept as an audit action (not a counter table) because the turn audit stream already
/// carries per-turn decisions and the admin panel reads it. Fallbacks carry the
/// reason the prepared digest was not there — an unreasoned fallback counter is
/// how a 100% failure rate stayed invisible for a day.
pub fn audit_action(digest_used: bool, entries: usize, reason: Option<&str>) -> String {
    if digest_used {
        format!("memory.digest.used.n{entries}")
    } else {
        match reason {
            Some(reason) => format!("memory.digest.fallback.{reason}.n{entries}"),
            None => format!("memory.digest.fallback.n{entries}"),
        }
    }
}

/// One prepared digest, waiting for the turn that owns its batch to pick it up.
#[derive(Debug, Clone)]
pub struct DigestSlot {
    pub digest: Digest,
    /// Entries the expansion pass added on top of the first pass.
    pub extra: Vec<ContextMemory>,
    /// The batch text the digest was built from. A slot is only handed to a turn whose
    /// batch produced the same text, so a digest can never be recycled onto an unrelated
    /// (later) burst of the same group.
    pub query: String,
    pub made_at: Instant,
}

/// Builds digests for the intake tick and hands them to the turn that follows.
///
/// The digest is computed while the pacing gate is deciding whether the burst is
/// finished, so its cost hides inside a wait that already exists. It is stored by group
/// id and picked up once by the next turn for that group; a slot nobody claims expires.
/// Budget for a background digest: the gate window no longer bounds it (see
/// [`DigestEngine::prepare_background`]), but a digest is a per-burst view, so
/// anything slower than this is not worth having either.
pub const DIGEST_BACKGROUND_BUDGET: Duration = Duration::from_secs(25);

pub struct DigestEngine {
    store: Arc<SqliteStore>,
    bot_id: String,
    model: Arc<dyn SummaryModel>,
    budget: usize,
    slots: Mutex<HashMap<String, DigestSlot>>,
    /// Last preparation outcome per group, for the fallback audit's reason.
    outcomes: Mutex<HashMap<String, (Instant, &'static str)>>,
    wait: Duration,
}

impl DigestEngine {
    pub fn new(
        store: Arc<SqliteStore>,
        bot_id: String,
        model: Arc<dyn SummaryModel>,
        budget: usize,
    ) -> Self {
        Self {
            store,
            bot_id,
            model,
            budget,
            slots: Mutex::new(HashMap::new()),
            outcomes: Mutex::new(HashMap::new()),
            wait: DIGEST_WAIT,
        }
    }

    fn record_outcome(&self, group_id: &str, reason: &'static str) {
        if let Ok(mut outcomes) = self.outcomes.lock() {
            outcomes.insert(group_id.to_string(), (Instant::now(), reason));
        }
    }

    /// The most recent preparation outcome for this group, when fresh enough to
    /// explain a fallback (an outcome from an hour ago explains nothing).
    pub fn last_outcome(&self, group_id: &str) -> Option<&'static str> {
        let outcomes = self.outcomes.lock().ok()?;
        let (at, reason) = outcomes.get(group_id)?;
        (at.elapsed() <= Duration::from_secs(300)).then_some(*reason)
    }

    /// Test hook: shorten the wait budget.
    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// The wait budget this engine applies to one digest call.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    /// Retrieve, digest, and stash the result for the next turn of this group.
    ///
    /// Returns the digest for the caller's audit, or `None` when there was nothing to
    /// digest or the model failed; either way the turn falls back to the raw list.
    pub async fn prepare(
        &self,
        group_id: &str,
        messages: &[crate::onebot::normalize::IncomingMessage],
    ) -> Option<Digest> {
        self.prepare_within(group_id, messages, Instant::now() + self.wait)
            .await
    }

    /// Launch a digest for this burst in the background and return immediately.
    ///
    /// The original design hid the digest inside the pacing gate's window,
    /// which capped it at the gate's remaining time — while the summary model
    /// routinely needs longer than that to finish a reply at all (2026-10-27:
    /// fallback rate 100%, no digest ever landed). The computation is cheap
    /// and per-burst; letting it finish on its own budget and stashing the
    /// slot for the next turn costs nothing but a task.
    pub fn prepare_background(
        self: &Arc<Self>,
        group_id: &str,
        messages: Vec<crate::onebot::normalize::IncomingMessage>,
    ) {
        let engine = Arc::clone(self);
        let group_id = group_id.to_string();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                engine
                    .prepare_within(
                        &group_id,
                        &messages,
                        Instant::now() + DIGEST_BACKGROUND_BUDGET,
                    )
                    .await;
            });
        }
    }

    /// Same as [`Self::prepare`], but the wait stops at `deadline`. The deadline
    /// is authoritative: callers that want the short in-gate budget pass a
    /// short one, the background launcher passes [`DIGEST_BACKGROUND_BUDGET`].
    pub async fn prepare_within(
        &self,
        group_id: &str,
        messages: &[crate::onebot::normalize::IncomingMessage],
        deadline: Instant,
    ) -> Option<Digest> {
        let budget = deadline.saturating_duration_since(Instant::now());
        if budget.is_zero() {
            self.record_outcome(group_id, DigestFail::Timeout.as_str());
            return None;
        }
        let context = self.retrieve(group_id, messages, None).await;
        if context.empty {
            self.record_outcome(group_id, DigestFail::Empty.as_str());
            self.clear(group_id);
            return None;
        }
        let entries = context.entries.clone();
        let summary = self
            .store
            .get_summary(&self.bot_id, group_id)
            .await
            .ok()
            .flatten()
            .map(|s| s.summary_text);
        let request = DigestRequest {
            entries: entries.clone(),
            summary,
            query_text: query_text(messages),
        };
        let mut digest =
            match tokio::time::timeout(budget, digest(self.model.as_ref(), &request)).await {
                Ok(Ok(digest)) => digest,
                Ok(Err(fail)) => {
                    self.record_outcome(group_id, fail.as_str());
                    return None;
                }
                Err(_) => {
                    self.record_outcome(group_id, DigestFail::Timeout.as_str());
                    return None;
                }
            };
        // A model that omitted refs does not disarm the correction duty: the
        // key list is backfilled from the entries this digest was built from.
        if digest.refs.is_empty() {
            digest.refs = entries.iter().map(|e| e.memory_ref.clone()).collect();
        }
        self.record_outcome(group_id, "ok");
        // The expansion terms are only worth a second *lexical* pass, never a second
        // model call: they widen recall over rows already in memory.
        let extra = self
            .expand(group_id, messages, &entries, &digest.expansion_terms)
            .await;

        let slot = DigestSlot {
            digest: digest.clone(),
            extra,
            query: request.query_text.clone(),
            made_at: Instant::now(),
        };
        if let Ok(mut slots) = self.slots.lock() {
            slots.insert(group_id.to_string(), slot);
        }
        Some(digest)
    }

    /// Take the prepared digest for this group, if one is fresh.
    ///
    /// Matching used to require the batch text to be identical to the one the
    /// digest was built from — but a burst that grows by one message between
    /// the gate and the turn (the normal case in a live group) changed the
    /// text and orphaned every finished digest. The slot is a per-group,
    /// one-shot, TTL-bound view of the group's recalled memories against its
    /// recent talk; one burst of drift is what it was always for.
    pub fn take(&self, group_id: &str, _query: &str) -> Option<DigestSlot> {
        let mut slots = self.slots.lock().ok()?;
        let fresh = slots
            .get(group_id)
            .map(|slot| slot.made_at.elapsed() <= SLOT_TTL)
            .unwrap_or(false);
        if !fresh {
            slots.remove(group_id);
            return None;
        }
        slots.remove(group_id)
    }

    /// Drop a prepared digest that will not be used (e.g. the batch went inactive).
    pub fn clear(&self, group_id: &str) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.remove(group_id);
        }
    }

    /// One retrieval over the rows in hand.
    async fn retrieve(
        &self,
        group_id: &str,
        messages: &[crate::onebot::normalize::IncomingMessage],
        expansion: Option<&str>,
    ) -> MemoryContext {
        let memories = self
            .store
            .list_memories(&self.bot_id, group_id, 500)
            .await
            .unwrap_or_default();
        let members = self
            .store
            .list_members(&self.bot_id, group_id)
            .await
            .unwrap_or_default();
        let aliases: Vec<(String, String)> = members
            .iter()
            .map(|m| (m.user_id.clone(), m.speaker_ref.clone()))
            .collect();
        let mut query = query_text(messages);
        if let Some(extra) = expansion {
            query.push('\n');
            query.push_str(extra);
        }
        let mut speaker_refs: Vec<String> = Vec::new();
        for m in messages {
            if !speaker_refs.contains(&m.speaker_ref) {
                speaker_refs.push(m.speaker_ref.clone());
            }
        }
        retrieval::build(&RetrievalInput {
            memories: &memories,
            query_text: &query,
            speaker_refs: &speaker_refs,
            subject_aliases: &aliases,
            budget: self.budget,
        })
    }

    /// Second lexical pass with the expansion terms appended, returning only rows the
    /// first pass missed.
    async fn expand(
        &self,
        group_id: &str,
        messages: &[crate::onebot::normalize::IncomingMessage],
        first: &[ContextMemory],
        terms: &[String],
    ) -> Vec<ContextMemory> {
        let Some(query) = expansion_query(terms) else {
            return Vec::new();
        };
        let widened = self.retrieve(group_id, messages, Some(&query)).await;
        widened
            .entries
            .into_iter()
            .filter(|e| !first.iter().any(|f| f.memory_ref == e.memory_ref))
            .collect()
    }
}

/// The batch text retrieval is scored against.
fn query_text(messages: &[crate::onebot::normalize::IncomingMessage]) -> String {
    messages
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ref_: &str, content: &str) -> ContextMemory {
        ContextMemory {
            memory_ref: ref_.into(),
            subject_ref: Some("u1".into()),
            content: content.into(),
            kind: "fact".into(),
            score: 1.0,
            recent: false,
        }
    }

    fn entries() -> Vec<ContextMemory> {
        vec![entry("mem_aa", "喜欢咖啡"), entry("mem_bb", "周五联机")]
    }

    fn request() -> DigestRequest {
        DigestRequest {
            entries: entries(),
            summary: Some("本群常聊吃喝与游戏。".into()),
            query_text: "今晚喝咖啡吗".into(),
        }
    }

    #[test]
    fn prompt_carries_keys_for_refs_but_does_not_ask_for_them_in_the_brief() {
        let prompt = render_prompt(&request());
        assert!(prompt.contains("mem_aa"));
        assert!(prompt.contains("[u1]"));
        assert!(prompt.contains("喜欢咖啡"));
        assert!(prompt.contains("今晚喝咖啡吗"));
        assert!(prompt.contains("本群常聊吃喝与游戏"));
        assert!(prompt.contains("refs"));
    }

    #[test]
    fn empty_entries_are_not_worth_a_call() {
        let mut r = request();
        r.entries.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn a_valid_reply_parses_brief_refs_and_terms() {
        let raw = r#"{"digest":"群里在聊喝咖啡，主人偏好红茶。","refs":["mem_aa"],"terms":["红茶","饮品"]}"#;
        let d = parse_digest(raw, &entries()).expect("应解析");
        assert!(d.brief.contains("红茶"));
        assert_eq!(d.refs, vec!["mem_aa"]);
        assert_eq!(d.expansion_terms, vec!["红茶", "饮品"]);
    }

    #[test]
    fn unknown_or_duplicate_refs_are_dropped() {
        let raw = r#"{"digest":"正文","refs":["mem_zz","mem_aa","mem_aa","群"],"terms":[]}"#;
        let d = parse_digest(raw, &entries()).expect("应解析");
        assert_eq!(d.refs, vec!["mem_aa"]);
    }

    #[test]
    fn fenced_json_is_still_read() {
        let raw = "```json\n{\"digest\":\"摘要正文\",\"refs\":[\"mem_aa\"],\"terms\":[]}\n```";
        assert_eq!(
            parse_digest(raw, &entries()).expect("应解析").brief,
            "摘要正文"
        );
    }

    #[test]
    fn a_brief_without_keys_still_parses() {
        // The refs veto used to discard these wholesale; the caller backfills
        // the key list from the source entries instead (see prepare_within).
        let raw = r#"{"digest":"正文但没给键","terms":[]}"#;
        let d = parse_digest(raw, &entries()).expect("无键也应解析");
        assert_eq!(d.brief, "正文但没给键");
        assert!(d.refs.is_empty());
        assert!(parse_digest(raw, &[]).is_ok());
    }

    #[test]
    fn render_appends_the_keys_it_kept() {
        let raw = r#"{"digest":"正文","refs":["mem_aa","mem_bb"],"terms":[]}"#;
        let d = parse_digest(raw, &entries()).expect("应解析");
        let rendered = d.render();
        assert!(rendered.starts_with("正文"));
        assert!(rendered.contains("mem_aa") && rendered.contains("mem_bb"));
    }

    #[test]
    fn malformed_or_identifier_leaking_output_falls_back() {
        assert_eq!(
            parse_digest("hello there", &entries()),
            Err(DigestFail::Parse)
        );
        assert_eq!(
            parse_digest(r#"{"digest":""}"#, &entries()),
            Err(DigestFail::Parse)
        );
        assert_eq!(
            parse_digest(
                r#"{"digest":"群号 123456789 说的","refs":["mem_aa"]}"#,
                &entries()
            ),
            Err(DigestFail::Leak)
        );
        assert_eq!(
            parse_digest(
                r#"{"digest":"[CQ:at,qq=1] 说的","refs":["mem_aa"]}"#,
                &entries()
            ),
            Err(DigestFail::Leak)
        );
    }

    #[test]
    fn terms_are_capped_and_filtered() {
        let raw =
            r#"{"digest":"正文","refs":["mem_aa"],"terms":["a","b","c","d","e","f","g","h"]}"#;
        assert_eq!(
            parse_digest(raw, &entries())
                .expect("应解析")
                .expansion_terms
                .len(),
            6
        );
        let raw = r#"{"digest":"正文","refs":["mem_aa"],"terms":["ok","1234567"]}"#;
        assert_eq!(
            parse_digest(raw, &entries())
                .expect("应解析")
                .expansion_terms,
            vec!["ok"]
        );
    }

    #[test]
    fn digest_is_truncated_not_rejected_when_long() {
        let long = "长".repeat(MAX_DIGEST_CHARS + 500);
        let raw = serde_json::json!({ "digest": long, "refs": ["mem_aa"] }).to_string();
        let d = parse_digest(&raw, &entries()).expect("应解析");
        assert_eq!(d.brief.chars().count(), MAX_DIGEST_CHARS);
    }

    #[test]
    fn expansion_terms_score_overlapping_content_higher() {
        assert!(expansion_score(&["咖啡".into()], "喜欢咖啡") > 0.0);
        assert_eq!(expansion_score(&["咖啡".into()], "周五联机打游戏"), 0.0);
        assert_eq!(expansion_score(&[], "喜欢咖啡"), 0.0);
    }

    #[test]
    fn audit_action_distinguishes_used_from_fallback() {
        assert_eq!(audit_action(true, 3, None), "memory.digest.used.n3");
        assert_eq!(audit_action(false, 3, None), "memory.digest.fallback.n3");
        assert_eq!(
            audit_action(false, 3, Some("timeout")),
            "memory.digest.fallback.timeout.n3"
        );
    }

    struct StubModel {
        reply: Result<String, crate::error::ErrorClass>,
        delay: Option<Duration>,
    }

    #[async_trait::async_trait]
    impl SummaryModel for StubModel {
        async fn summarise(&self, _prompt: String) -> Result<String, crate::error::ErrorClass> {
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            self.reply.clone()
        }
    }

    fn message(text: &str) -> crate::onebot::normalize::IncomingMessage {
        crate::onebot::normalize::IncomingMessage {
            bot_id: "bot".into(),
            group_id: "100".into(),
            message_id: "900".into(),
            sender_id: "7".into(),
            speaker_ref: "u1".into(),
            speaker_role: crate::onebot::normalize::SpeakerRole::Member,
            display_name: String::new(),
            known_alias: None,
            received_at: 1,
            segments: Vec::new(),
            text: text.into(),
            mentions_bot: false,
            quote_of: None,
        }
    }

    async fn seeded_store() -> (tempfile::TempDir, Arc<SqliteStore>) {
        let dir = tempfile::tempdir().expect("tmp");
        let pool = crate::store::migrations::open_temp(dir.path())
            .await
            .expect("迁移");
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group("bot", "100", 1).await.expect("建组");
        store.ensure_member("bot", "100", "7", 1).await.expect("人");
        store
            .insert_memory_with_origin(
                "bot",
                "100",
                Some("7"),
                "喜欢喝咖啡",
                "fact",
                "",
                0.8,
                crate::store::origin::HY3,
                1,
            )
            .await
            .expect("写记忆");
        (dir, store)
    }

    fn engine(store: Arc<SqliteStore>, reply: &str, delay: Option<Duration>) -> DigestEngine {
        DigestEngine::new(
            store,
            "bot".into(),
            Arc::new(StubModel {
                reply: Ok(reply.to_string()),
                delay,
            }),
            crate::memory::retrieval::DEFAULT_BUDGET,
        )
    }

    #[tokio::test]
    async fn a_prepared_digest_is_handed_to_the_matching_batch_once() {
        let (_dir, store) = seeded_store().await;
        let key = store
            .list_memories("bot", "100", 10)
            .await
            .expect("读")
            .remove(0)
            .memory_ref;
        let raw = serde_json::json!({
            "digest": "主人爱喝咖啡。",
            "refs": [key],
            "terms": ["咖啡"]
        })
        .to_string();
        let engine = engine(Arc::clone(&store), &raw, None);
        let messages = vec![message("今晚喝咖啡吗")];
        let digest = engine.prepare("100", &messages).await.expect("应出摘要");
        assert!(digest.brief.contains("咖啡"));
        // Matching batch: handed over, and consumed exactly once.
        let slot = engine.take("100", "今晚喝咖啡吗").expect("应取到");
        assert_eq!(slot.digest.brief, digest.brief);
        assert!(engine.take("100", "今晚喝咖啡吗").is_none());
    }

    #[tokio::test]
    async fn a_digest_serves_the_groups_next_turn_even_as_the_burst_grows() {
        // The burst grew between the gate and the turn, changing its text:
        // the group's fresh digest is still the right view for its next turn.
        let (_dir, store) = seeded_store().await;
        let key = store
            .list_memories("bot", "100", 10)
            .await
            .expect("读")
            .remove(0)
            .memory_ref;
        let raw = serde_json::json!({"digest": "主人爱喝咖啡。", "refs": [key]}).to_string();
        let engine = engine(Arc::clone(&store), &raw, None);
        engine.prepare("100", &[message("今晚喝咖啡吗")]).await;
        let slot = engine
            .take("100", "今晚喝咖啡吗 对了再加一份糖")
            .expect("长大了的批次也应取到");
        assert!(slot.digest.brief.contains("咖啡"));
    }

    #[tokio::test]
    async fn a_slow_model_misses_its_window_and_the_turn_falls_back() {
        let (_dir, store) = seeded_store().await;
        let engine = DigestEngine::new(
            Arc::clone(&store),
            "bot".into(),
            Arc::new(StubModel {
                reply: Ok("{\"digest\":\"慢的\",\"refs\":[\"mem_aa\"]}".into()),
                delay: Some(Duration::from_millis(400)),
            }),
            crate::memory::retrieval::DEFAULT_BUDGET,
        )
        .with_wait(Duration::from_millis(20));
        let messages = vec![message("今晚喝咖啡吗")];
        assert!(engine.prepare("100", &messages).await.is_none());
        assert!(engine.take("100", "今晚喝咖啡吗").is_none());
    }

    #[tokio::test]
    async fn an_empty_memory_store_is_not_worth_a_model_call() {
        let dir = tempfile::tempdir().expect("tmp");
        let pool = crate::store::migrations::open_temp(dir.path())
            .await
            .expect("迁移");
        let store = Arc::new(SqliteStore::new(pool));
        store.ensure_group("bot", "100", 1).await.expect("建组");
        let engine = engine(Arc::clone(&store), "{\"digest\":\"x\"}", None);
        assert!(engine.prepare("100", &[message("你好")]).await.is_none());
    }
}

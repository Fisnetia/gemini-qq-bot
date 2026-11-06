//! Memory context retrieval for one group.
//!
//! Runs entirely inside Rust/SQLite: the agent is never given a database handle.
//! Older detail is reachable on demand through the `memory_search` MCP tool. Each
//! turn the caller asks for the memories of the **current** `(bot_id, group_id)`
//! only; nothing is ever borrowed from another group to fill the budget.
//!
//! Ranking is deterministic so the same inputs always produce the same prompt:
//!
//! 1. character-bigram Dice overlap between the batch text and the memory content,
//! 2. a +0.25 bonus when the memory's subject is a speaker in this batch **and**
//!    the lexical score is already non-zero,
//! 3. a confidence multiplier `0.5 + 0.5 * confidence` so a half-trusted row
//!    cannot outrank a well-supported one with the same overlap,
//! 4. a small kind weight (`preference` 1.1, `fact` 1.05, everything else 1.0),
//! 5. stable tie-break on (`score` desc, `updated_at` desc, `memory_ref` asc).
//!
//! Pass 1 takes up to 12 memories with score > 0. Pass 2 fills up to 5 leftover
//! rows by true recency (`updated_at` desc), inside a 1,200 character budget. A row
//! that does not fit is skipped and the next one is tried: a single oversized memory
//! must not evict every shorter memory behind it.
//!
//! Retired rows (`archived_at` set) never surface, whether or not the caller already
//! filtered them: retrieval is the last gate before the prompt.
//!
//! Injected `memory_ref` values are the store keys (`mem_{hex}`), never sequential
//! aliases. When nothing matches, the block says so explicitly instead of being empty.

use std::collections::BTreeSet;

use crate::store::MemoryRecord;

/// Default character budget for the whole memory block.
///
/// Kept deliberately small: the summary is no longer injected and older detail is
/// reachable on demand through the `memory_search` MCP tool, so a large always-on blob
/// only spends input tokens the agent may never need.
pub const DEFAULT_BUDGET: usize = 1_200;
/// How many memories one turn may see. Five is the working set; twelve was
/// enough for the model to recite stale rows instead of the live thread.
pub const MAX_RELEVANT: usize = 5;
/// Unrelated leftovers used to fill the prompt just because they were new.
/// A miss now renders nothing.
pub const MAX_RECENT: usize = 0;

/// Confidence enters the score as `0.5 + 0.5 * confidence`, i.e. in `0.5..=1.0`.
///
/// The floor is deliberate: confidence is a hint about trustworthiness, not a
/// veto, so a zero-confidence row still competes at half weight instead of
/// vanishing (the prompt already shows the content, not the score).
pub fn confidence_multiplier(confidence: f64) -> f64 {
    let c = if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.5
    };
    0.5 + 0.5 * c
}

/// Kind weight: durable preferences matter more in conversation than trivia,
/// and facts more than open ends. Anything unknown (or a future kind) is neutral.
pub fn kind_weight(kind: &str) -> f64 {
    match kind {
        "preference" => 1.1,
        "fact" => 1.05,
        _ => 1.0,
    }
}

/// One memory line as shown to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMemory {
    /// Store key (`mem_{hex}`). The model must echo this on update.
    pub memory_ref: String,
    /// `uN` alias of the subject, when the subject is a known member of this group.
    pub subject_ref: Option<String>,
    pub content: String,
    pub kind: String,
    /// Ranking score, exposed for tests and the admin view.
    pub score: f64,
    /// True when the entry came from the "fresh" pass.
    pub recent: bool,
}

/// The rendered memory context for one turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryContext {
    pub entries: Vec<ContextMemory>,
    /// True when no memory was available.
    pub empty: bool,
    /// Characters consumed.
    pub chars: usize,
}

impl MemoryContext {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            empty: true,
            chars: 0,
        }
    }

    /// Render the block injected into the prompt.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.entries.is_empty() {
            return out;
        }
        out.push_str("相关记忆条目：\n");
        for e in &self.entries {
            let subject = match &e.subject_ref {
                Some(s) => format!("[{s}]"),
                None => "[群]".to_string(),
            };
            out.push_str(&format!(
                "- {} {} ({}) {}\n",
                e.memory_ref, subject, e.kind, e.content
            ));
        }
        out
    }

    /// Store keys handed to the model, for output validation.
    pub fn memory_refs(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.memory_ref.clone()).collect()
    }
}

/// Inputs for one retrieval pass. Already scoped to a single group by the caller.
#[derive(Debug, Clone)]
pub struct RetrievalInput<'a> {
    /// Memories of the current group only.
    pub memories: &'a [MemoryRecord],
    /// Concatenated plain text of the batch, used for overlap scoring.
    pub query_text: &'a str,
    /// Speaker aliases present in this batch, used for the subject bonus.
    pub speaker_refs: &'a [String],
    /// Mapping from `user_id` to this group's `speaker_ref`, so subject weighting is
    /// possible without exposing raw ids.
    pub subject_aliases: &'a [(String, String)],
    pub budget: usize,
}

/// Character bigrams of a string, ignoring whitespace. Bigrams give a language
/// agnostic similarity measure that works for Chinese without a tokenizer.
pub fn bigrams(text: &str) -> BTreeSet<String> {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = BTreeSet::new();
    if chars.len() < 2 {
        if let Some(c) = chars.first() {
            out.insert(c.to_string());
        }
        return out;
    }
    for pair in chars.windows(2) {
        out.insert(pair.iter().collect());
    }
    out
}

/// Dice-style overlap between two bigram sets, in `0.0..=1.0`.
pub fn overlap_score(query: &BTreeSet<String>, memory: &BTreeSet<String>) -> f64 {
    if query.is_empty() || memory.is_empty() {
        return 0.0;
    }
    let shared = query.intersection(memory).count();
    if shared == 0 {
        return 0.0;
    }
    // Normalising by the memory size keeps long memories from dominating.
    (2.0 * shared as f64) / ((query.len() + memory.len()) as f64)
}

/// Convenience wrapper that carries the configured budget.
#[derive(Debug, Clone, Copy)]
pub struct MemoryContextBuilder {
    pub budget: usize,
}

impl Default for MemoryContextBuilder {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
        }
    }
}

impl MemoryContextBuilder {
    pub fn new(budget: usize) -> Self {
        Self {
            budget: if budget == 0 { DEFAULT_BUDGET } else { budget },
        }
    }

    /// Build the context, applying the configured budget.
    pub fn build(&self, mut input: RetrievalInput<'_>) -> MemoryContext {
        input.budget = self.budget;
        build(&input)
    }
}

/// Build the context for one turn.
pub fn build(input: &RetrievalInput<'_>) -> MemoryContext {
    let budget = if input.budget == 0 {
        DEFAULT_BUDGET
    } else {
        input.budget
    };
    let query = bigrams(input.query_text);
    let speakers: BTreeSet<&str> = input.speaker_refs.iter().map(|s| s.as_str()).collect();

    // Ranking pass. Retired rows are dropped here as well as in the store query:
    // this function is also reachable with a hand-built slice (tests, replays).
    let mut scored: Vec<(f64, &MemoryRecord, bool)> = Vec::new();
    for memory in input.memories.iter().filter(|m| !m.is_archived()) {
        if is_operational_memory(&memory.content) {
            continue;
        }
        let mem_grams = bigrams(&memory.content);
        let lexical = overlap_score(&query, &mem_grams);
        let mut score = lexical;
        let mut subject_matches = false;
        if let Some(subject_user) = &memory.subject_user_id
            && let Some((_, alias)) = input
                .subject_aliases
                .iter()
                .find(|(user_id, _)| user_id == subject_user)
            && speakers.contains(alias.as_str())
        {
            // The person in the room comes first, even when this batch's words
            // do not overlap the stored sentence.
            score += 0.25;
            subject_matches = true;
        }
        score *= confidence_multiplier(memory.confidence) * kind_weight(&memory.kind);
        scored.push((score, memory, subject_matches));
    }
    // Stable ordering: score desc, then updated_at desc, then memory_ref asc.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
            .then_with(|| a.1.memory_ref.cmp(&b.1.memory_ref))
    });

    let mut context = MemoryContext::default();
    let mut used = 0usize;
    let mut taken: BTreeSet<String> = BTreeSet::new();

    // Pass 1: relevant memories with a non-zero score. The store key is shown
    // as-is so an update can find the row; sequential mem_1 aliases cannot.
    for (score, memory, _) in &scored {
        if context.entries.len() >= MAX_RELEVANT {
            break;
        }
        if *score <= 0.0 {
            continue;
        }
        let cost = memory.content.chars().count() + 24;
        if used + cost > budget {
            // Skip, don't break: the next entry may well be shorter.
            continue;
        }
        used += cost;
        taken.insert(memory.memory_ref.clone());
        context.entries.push(ContextMemory {
            memory_ref: memory.memory_ref.clone(),
            subject_ref: subject_ref_for(memory, input),
            content: memory.content.clone(),
            kind: memory.kind.clone(),
            score: *score,
            recent: false,
        });
    }

    // Pass 2: leftover rows by true recency, not leftover score order.
    let mut leftover: Vec<&MemoryRecord> = scored
        .iter()
        .filter(|(_, memory, _)| !taken.contains(&memory.memory_ref))
        .map(|(_, memory, _)| *memory)
        .collect();
    leftover.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.memory_ref.cmp(&b.memory_ref))
    });
    // The cap is on injected rows, not on candidates examined: a leftover that does
    // not fit must not consume one of the five slots.
    let mut recent_added = 0usize;
    for memory in leftover {
        if recent_added >= MAX_RECENT {
            break;
        }
        let cost = memory.content.chars().count() + 24;
        if used + cost > budget {
            continue;
        }
        used += cost;
        taken.insert(memory.memory_ref.clone());
        recent_added += 1;
        context.entries.push(ContextMemory {
            memory_ref: memory.memory_ref.clone(),
            subject_ref: subject_ref_for(memory, input),
            content: memory.content.clone(),
            kind: memory.kind.clone(),
            score: 0.0,
            recent: true,
        });
    }

    context.empty = context.entries.is_empty();
    context.chars = if context.empty { 0 } else { used };
    context
}

/// Runtime bookkeeping that hy3 stored as a fact. It is not about anyone in
/// the group, and it must not enter a prompt or a new row.
pub fn is_operational_memory(content: &str) -> bool {
    let lower = content.to_lowercase();
    const MARKERS: &[&str] = &["token", "上下文窗口", "上下文消耗", "记忆系统", "showluma"];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn subject_ref_for(memory: &MemoryRecord, input: &RetrievalInput<'_>) -> Option<String> {
    let subject = memory.subject_user_id.as_ref()?;
    input
        .subject_aliases
        .iter()
        .find(|(user_id, _)| user_id == subject)
        .map(|(_, alias)| alias.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(ref_id: &str, content: &str, subject: Option<&str>, updated: i64) -> MemoryRecord {
        MemoryRecord {
            memory_ref: ref_id.into(),
            bot_id: "1".into(),
            group_id: "100".into(),
            subject_user_id: subject.map(|s| s.to_string()),
            content: content.into(),
            kind: "fact".into(),
            source_message_ids: "[]".into(),
            confidence: 0.8,
            revision: 1,
            origin: "agy".into(),
            archived_at: None,
            created_at: updated,
            updated_at: updated,
        }
    }

    fn input<'a>(
        memories: &'a [MemoryRecord],
        query: &'a str,
        speakers: &'a [String],
        aliases: &'a [(String, String)],
    ) -> RetrievalInput<'a> {
        RetrievalInput {
            memories,
            query_text: query,
            speaker_refs: speakers,
            subject_aliases: aliases,
            budget: DEFAULT_BUDGET,
        }
    }

    #[test]
    fn empty_group_says_nothing_was_found() {
        let memories: Vec<MemoryRecord> = Vec::new();
        let ctx = build(&input(&memories, "随便聊聊", &[], &[]));
        assert!(ctx.empty);
        assert_eq!(ctx.render(), "");
        assert!(ctx.memory_refs().is_empty());
    }

    #[test]
    fn relevant_memories_rank_above_unrelated_ones() {
        let memories = vec![
            memory("mem_a", "喜欢喝手冲咖啡", None, 100),
            memory("mem_b", "住在杭州", None, 100),
        ];
        let ctx = build(&input(&memories, "今天咖啡怎么样", &[], &[]));
        assert_eq!(ctx.memory_refs(), vec!["mem_a"]);
        assert_eq!(ctx.entries[0].content, "喜欢喝手冲咖啡");
        assert!(!ctx.entries[0].recent);
    }

    #[test]
    fn speaker_subject_memory_gets_a_bonus() {
        let memories = vec![
            memory("mem_a", "喜欢喝咖啡", Some("555"), 100),
            memory("mem_b", "喜欢喝咖啡", Some("666"), 100),
        ];
        let speakers = vec!["u2".to_string()];
        let aliases = vec![
            ("555".to_string(), "u1".to_string()),
            ("666".to_string(), "u2".to_string()),
        ];
        let ctx = build(&input(&memories, "咖啡", &speakers, &aliases));
        // Same lexical overlap; the speaker's own row gets the +0.25 and leads.
        assert_eq!(ctx.entries[0].memory_ref, "mem_b");
        assert_eq!(ctx.entries[0].subject_ref.as_deref(), Some("u2"));
        assert!(!ctx.entries[0].recent);
    }

    #[test]
    fn a_speakers_memory_enters_without_lexical_overlap() {
        let memories = vec![
            memory("mem_a", "养了一只猫", Some("555"), 100),
            memory("mem_b", "在学吉他", Some("666"), 200),
        ];
        let speakers = vec!["u2".to_string()];
        let aliases = vec![
            ("555".to_string(), "u1".to_string()),
            ("666".to_string(), "u2".to_string()),
        ];
        let ctx = build(&input(&memories, "zzz", &speakers, &aliases));
        assert_eq!(ctx.memory_refs(), vec!["mem_b"]);
        assert!(!ctx.entries[0].recent);
    }

    #[test]
    fn recent_memories_fill_remaining_slots_without_duplicates() {
        let memories = vec![
            memory("mem_a", "聊咖啡", None, 300),
            memory("mem_b", "聊茶叶", None, 200),
            memory("mem_c", "完全无关", None, 100),
        ];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        let contents: Vec<&str> = ctx.entries.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(contents, vec!["聊咖啡"]);
        assert!(!ctx.entries[0].recent);
    }

    #[test]
    fn relevant_count_is_capped_at_twelve() {
        let memories: Vec<MemoryRecord> = (0..20)
            .map(|i| memory(&format!("mem_{i}"), "咖啡", None, 100 + i))
            .collect();
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        let relevant = ctx.entries.iter().filter(|e| !e.recent).count();
        assert_eq!(relevant, MAX_RELEVANT, "相关记忆上限 5，实际 {relevant}");
    }

    #[test]
    fn unrelated_rows_are_not_injected() {
        let memories: Vec<MemoryRecord> = (0..20)
            .map(|i| memory(&format!("mem_{i}"), &format!("无关内容{i}"), None, 100 + i))
            .collect();
        let ctx = build(&input(&memories, "zzz", &[], &[]));
        assert!(ctx.empty);
        assert!(ctx.entries.is_empty());
    }

    #[test]
    fn total_budget_is_respected() {
        let memories: Vec<MemoryRecord> = (0..30)
            .map(|i| memory(&format!("mem_{i}"), &"很".repeat(400), None, 100 + i))
            .collect();
        let ctx = build(&input(&memories, "很很", &[], &[]));
        assert!(ctx.chars <= DEFAULT_BUDGET, "预算 1200，实际 {}", ctx.chars);
        assert!(!ctx.entries.is_empty());
    }

    #[test]
    fn the_group_summary_is_no_longer_injected() {
        // The rolling summary used to be prepended to every prompt. It is stale by
        // construction (it lags the memory revision) and duplicates what the chunk
        // layer plus `memory_search` now provide, so it must not appear at all.
        let memories = vec![memory("mem_a", "喜欢咖啡", None, 100)];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        let rendered = ctx.render();
        assert!(!rendered.contains("本群记忆摘要"));
        assert!(!rendered.contains("未找到与本批消息相关的记忆条目"));
        assert!(rendered.starts_with("相关记忆条目："));
    }

    #[test]
    fn ranking_is_deterministic() {
        let memories = vec![
            memory("mem_b", "咖啡", None, 100),
            memory("mem_a", "咖啡", None, 100),
        ];
        let first = build(&input(&memories, "咖啡", &[], &[]));
        let second = build(&input(&memories, "咖啡", &[], &[]));
        assert_eq!(first, second);
        // Equal score and timestamp: memory_ref ascending decides.
        assert_eq!(first.entries[0].memory_ref, "mem_a");
        assert_eq!(first.entries[1].memory_ref, "mem_b");
    }

    #[test]
    fn bigrams_work_for_chinese_and_short_strings() {
        assert_eq!(bigrams("咖").len(), 1);
        assert!(bigrams("咖啡").contains("咖啡"));
        assert_eq!(overlap_score(&bigrams("咖啡"), &bigrams("咖啡")), 1.0);
        assert_eq!(overlap_score(&bigrams("咖啡"), &bigrams("茶")), 0.0);
    }

    #[test]
    fn subject_ref_is_none_when_the_member_is_unknown_to_this_group() {
        let memories = vec![memory("mem_a", "咖啡", Some("999"), 100)];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        assert!(ctx.entries[0].subject_ref.is_none());
        assert!(ctx.render().contains("[群]"));
    }

    #[test]
    fn confidence_multiplier_spans_half_to_one() {
        assert_eq!(confidence_multiplier(0.0), 0.5);
        assert_eq!(confidence_multiplier(1.0), 1.0);
        assert_eq!(confidence_multiplier(0.5), 0.75);
        // Out-of-range and non-finite inputs are clamped, never panicking.
        assert_eq!(confidence_multiplier(-3.0), 0.5);
        assert_eq!(confidence_multiplier(9.0), 1.0);
        assert_eq!(confidence_multiplier(f64::NAN), 0.75);
    }

    #[test]
    fn kind_weight_only_boosts_preference_and_fact() {
        assert_eq!(kind_weight("preference"), 1.1);
        assert_eq!(kind_weight("fact"), 1.05);
        for neutral in ["topic", "relation", "event", "something_new"] {
            assert_eq!(kind_weight(neutral), 1.0, "{neutral} 应保持中性权重");
        }
    }

    #[test]
    fn higher_confidence_wins_a_tie_on_overlap() {
        let mut low = memory("mem_low", "喜欢喝咖啡", None, 100);
        low.confidence = 0.2;
        let mut high = memory("mem_high", "喜欢喝咖啡", None, 100);
        high.confidence = 0.95;
        let memories = vec![low, high];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        // Identical text, identical timestamps: confidence alone decides.
        assert_eq!(ctx.entries[0].memory_ref, "mem_high");
        assert!(ctx.entries[0].score > ctx.entries[1].score);
    }

    #[test]
    fn confidence_cannot_reverse_a_clear_relevance_gap() {
        let mut weak = memory("mem_weak", "今天咖啡怎么样要不要一起喝咖啡", None, 100);
        weak.confidence = 0.1;
        let mut strong = memory("mem_strong", "空调坏了", None, 100);
        strong.confidence = 1.0;
        let memories = vec![weak, strong];
        // The multiplier is floored at 0.5 precisely so it nudges instead of vetoing.
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        assert_eq!(ctx.entries[0].memory_ref, "mem_weak");
    }

    #[test]
    fn preference_outranks_topic_at_equal_overlap_and_confidence() {
        let mut pref = memory("mem_pref", "喜欢咖啡", None, 100);
        pref.kind = "preference".into();
        let mut topic = memory("mem_topic", "喜欢咖啡", None, 100);
        topic.kind = "topic".into();
        let memories = vec![topic, pref];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        assert_eq!(ctx.entries[0].memory_ref, "mem_pref");
    }

    #[test]
    fn an_oversized_memory_is_skipped_instead_of_ending_the_pass() {
        // A 1,000-character row eats almost the whole 1,200 budget. The old code
        // broke out of the loop there, losing the two short rows behind it.
        let memories = vec![
            memory("mem_huge", &"咖".repeat(1_000), None, 300),
            memory("mem_mid", "咖啡", None, 200),
            memory("mem_small", "咖啡", None, 100),
        ];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        let refs: Vec<&str> = ctx.entries.iter().map(|e| e.memory_ref.as_str()).collect();
        assert!(
            refs.contains(&"mem_mid"),
            "应当跳过超大条目继续往后看：{refs:?}"
        );
        assert!(
            refs.contains(&"mem_small"),
            "应当跳过超大条目继续往后看：{refs:?}"
        );
        assert!(ctx.chars <= DEFAULT_BUDGET);
    }

    #[test]
    fn unrelated_leftovers_stay_out_even_when_they_are_new() {
        let memories = vec![
            memory("mem_huge", &"无".repeat(1_100), None, 999),
            memory("mem_a", "无关甲", None, 200),
            memory("mem_b", "无关乙", None, 100),
        ];
        let ctx = build(&input(&memories, "zzz", &[], &[]));
        assert!(ctx.empty);
    }

    #[test]
    fn archived_rows_never_enter_the_context() {
        let mut gone = memory("mem_gone", "喜欢喝咖啡", None, 300);
        gone.archived_at = Some(400);
        let live = memory("mem_live", "喜欢喝咖啡", None, 100);
        let memories = vec![gone, live];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        // Not in Pass 1 (relevance) and not in Pass 2 (recency) either.
        assert_eq!(ctx.memory_refs(), vec!["mem_live"]);
        assert!(!ctx.render().contains("mem_gone"));
    }

    #[test]
    fn an_all_archived_group_reports_nothing_found() {
        let mut gone = memory("mem_gone", "喜欢喝咖啡", None, 300);
        gone.archived_at = Some(400);
        let memories = vec![gone];
        let ctx = build(&input(&memories, "咖啡", &[], &[]));
        assert!(ctx.empty);
        assert_eq!(ctx.render(), "");
    }
}

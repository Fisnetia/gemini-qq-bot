//! Send-time gates for a finished turn: batch freshness, quote-anchor
//! resolution and echo checks, and the replay (near-duplicate) guard.
//! Moved out of turn.rs verbatim (std-split-turn); logic unchanged.

/// Send-time freshness gate: a reply is dropped when the batch's newest
/// message is older than this at the moment of sending. Agent turns on a
/// long resumed conversation routinely take one to two minutes, so this age
/// arm is only a backstop against truly ancient batches; the moved-on arm
/// below is the primary anti-misplacement guard. (At 90s a quiet group whose
/// agent merely thought for two minutes lost every reply: age 188, new 0.)
pub const SEND_GATE_MAX_AGE_SECONDS: i64 = 300;
/// …or when more than this many newer group messages arrived after the
/// batch's newest message (the conversation has clearly moved on).
pub const SEND_GATE_MAX_NEWER_MESSAGES: i64 = 8;
/// A quote anchor counts as scrolled away when more than this many group
/// messages are newer than it; a comment on it must then carry a quote.
pub const QUOTE_ANCHOR_MAX_NEWER: i64 = 5;

pub fn send_gate_drop(batch_age_seconds: i64, newer_messages: i64) -> bool {
    batch_age_seconds > SEND_GATE_MAX_AGE_SECONDS || newer_messages > SEND_GATE_MAX_NEWER_MESSAGES
}

/// One batch message as the quote-anchor analysis sees it.
#[derive(Debug, Clone)]
pub struct AnchorItem {
    pub message_ref: String,
    pub message_id: String,
    pub sender_id: String,
    pub image_count: usize,
    pub quote_of: Option<String>,
    pub received_at: Option<i64>,
}

/// The message a batch's replies most likely target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchAnchor {
    /// The batch collectively quotes one earlier message (not in the batch).
    Quoted { target_id: String, via_ref: String },
    /// One speaker's image post inside the batch (the first such message).
    Image {
        message_id: String,
        message_ref: String,
        received_at: Option<i64>,
    },
}

/// Anchor detection shared by the prompt hint and the send-time fallback.
/// A quoted target only counts when the quoting messages are the batch's
/// focus (small batch, or at least half the messages quote the same
/// target); images only count when every image message shares one sender
/// (a single post drip-fed in several shots).
pub fn batch_anchor(items: &[AnchorItem]) -> Option<BatchAnchor> {
    let mut targets: Vec<&str> = items.iter().filter_map(|i| i.quote_of.as_deref()).collect();
    let quoting = targets.len();
    if quoting > 0 {
        targets.sort_unstable();
        targets.dedup();
        if targets.len() == 1 && (items.len() <= 3 || quoting * 2 >= items.len()) {
            let via = items
                .iter()
                .find(|i| i.quote_of.as_deref() == Some(targets[0]))
                .expect("引用者必存在");
            return Some(BatchAnchor::Quoted {
                target_id: targets[0].to_string(),
                via_ref: via.message_ref.clone(),
            });
        }
    }
    let image_items: Vec<&AnchorItem> = items.iter().filter(|i| i.image_count > 0).collect();
    if let Some(first) = image_items.first()
        && image_items.iter().all(|i| i.sender_id == first.sender_id)
    {
        return Some(BatchAnchor::Image {
            message_id: first.message_id.clone(),
            message_ref: first.message_ref.clone(),
            received_at: first.received_at,
        });
    }
    None
}

/// How a turn's reply is checked against the group's message flow at send time.
///
/// A resident turn may have delivered several batches into one logical turn; each
/// of them has to be covered, oldest first, or the reply is dropped as stale
/// rather than landing out of order.
/// Seconds between the bot's previous send in a group and a resident
/// batch's first message below which a quote is presumed to ride on lagging
/// content: at busy moments her text keeps answering the previous thread
/// while the quote machinery pins the fresh trigger (2026-10-27 incident:
/// gaps of 17–37s on every misquote). A quote dropped this way costs a
/// plain send; a wrong quote costs the group's trust in the feature.
pub const QUOTE_HOT_THREAD_SECONDS: i64 = 45;

/// Normalize a message text or a quote echo for the evidence check:
/// whitespace and sentence punctuation carry no addressing information.
pub fn normalize_echo_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_whitespace()
                && !"，。！？、；：·…—「」『』“”‘’（）()【】[]<>,.!?;:\"'".contains(*c)
        })
        .collect()
}

/// The echo evidence check for one resolved quote target: the model's
/// copied snippet must literally appear in the target message's text.
/// An image-only target (no quotable text) skips the check — the ref is
/// the only addressing a picture has.
pub fn quote_echo_check(echo: Option<&str>, target_text: &str) -> Result<(), &'static str> {
    let target_norm = normalize_echo_text(target_text);
    if target_norm.is_empty() {
        return Ok(());
    }
    let Some(echo) = echo else {
        return Err("echo_missing");
    };
    let echo_norm = normalize_echo_text(echo);
    if echo_norm.chars().count() < 2 {
        return Err("echo_missing");
    }
    if target_norm.contains(&echo_norm) {
        Ok(())
    } else {
        Err("echo_mismatch")
    }
}

/// Does this segment open by addressing the owner? She calls exactly one
/// person 主人; when the words go to him, a quote pinned on anyone else's
/// message is a misquote no matter how fresh that message is.
pub fn segment_addresses_owner(text: &str) -> bool {
    let head: String = text.trim_start().chars().take(8).collect();
    head.contains("主人")
}

/// May a resolved quote target actually be attached? Returns the drop
/// reason when it may not. Checks, strongest first:
///
/// * `seeded` — the turn ran on a seed brief (fresh conversation): its
///   content answers the brief's digest of the past, so any batch-local
///   ref it named is decoration, not addressing (the 14:43 seed echo and
///   the 15:15 fresh-session misquote both happened on seeded turns).
/// * `addresses_owner && !target_is_owner` — the words address the owner
///   but the quote would pin someone else's message.
/// * `not_latest` — the target is neither the newest covered message nor
///   from the newest message's sender (multi-thread batches).
/// * `hot_thread` — resident only: the bot's previous send landed less
///   than [`QUOTE_HOT_THREAD_SECONDS`] before this batch's first message,
///   so the content is presumed to still answer that thread.
pub fn quote_drop_reason(
    target_sender: &str,
    target_is_newest: bool,
    target_is_owner: bool,
    newest_sender: &str,
    resident: bool,
    prev_sent_gap: Option<i64>,
    addresses_owner: bool,
    seeded: bool,
) -> Option<&'static str> {
    if seeded {
        return Some("cold_seed");
    }
    if addresses_owner && !target_is_owner {
        return Some("addressee_mismatch");
    }
    if !target_is_newest && target_sender != newest_sender {
        return Some("not_latest");
    }
    if resident && prev_sent_gap.is_some_and(|gap| gap < QUOTE_HOT_THREAD_SECONDS) {
        return Some("hot_thread");
    }
    None
}

/// How far back the replay guard compares a reply against the bot's own
/// sends (seconds). The seed-echo incident repeated lines from 23 minutes
/// earlier; an hour of margin costs nothing, repetition that old is never
/// a fresh answer either way — half an hour is the compromise.
pub const REPLAY_WINDOW_SECONDS: i64 = 1800;
/// A segment shorter than this (after normalization) is a reaction, not a
/// line; repeating it is natural and exempt from the replay guard.
pub const REPLAY_MIN_CHARS: usize = 8;
/// Character-bigram Dice at or above this counts as the same line.
pub const REPLAY_DICE_THRESHOLD: f64 = 0.90;
/// Containment with at least this length ratio also counts as the same
/// line (a re-sent line with a clause added or dropped).
pub const REPLAY_CONTAINMENT_RATIO: f64 = 0.85;

/// Comparison form for the replay guard: whitespace removed, and a short
/// trailing parenthesized group (a kaomoji tail like `(=｀ω´=)`) stripped —
/// she swaps those freely on what is otherwise the same line.
pub fn normalize_for_replay(text: &str) -> String {
    let mut s: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if s.ends_with(')') || s.ends_with('）') {
        let open = s.rfind(['(', '（']);
        if let Some(idx) = open {
            let tail_chars = s[idx..].chars().count();
            if tail_chars <= 12 {
                s.truncate(idx);
            }
        }
    }
    s
}

pub(crate) fn bigram_dice(a: &[char], b: &[char]) -> f64 {
    if a.len() < 2 || b.len() < 2 {
        return if a == b { 1.0 } else { 0.0 };
    }
    let mut counts: std::collections::HashMap<(char, char), i64> = std::collections::HashMap::new();
    for w in a.windows(2) {
        *counts.entry((w[0], w[1])).or_default() += 1;
    }
    let mut hits = 0i64;
    for w in b.windows(2) {
        if let Some(c) = counts.get_mut(&(w[0], w[1]))
            && *c > 0
        {
            *c -= 1;
            hits += 1;
        }
    }
    (2 * hits) as f64 / ((a.len() - 1) + (b.len() - 1)) as f64
}

/// Whether a normalized line ends the way a finished sentence does.
/// Kaomoji tails are already stripped by [`normalize_for_replay`], so the
/// last char here is sentence content.
pub(crate) fn ends_sentence_final(s: &str) -> bool {
    matches!(
        s.chars().last(),
        Some(
            '。' | '．'
                | '.'
                | '！'
                | '!'
                | '？'
                | '?'
                | '～'
                | '~'
                | '…'
                | '」'
                | '』'
                | '）'
                | ')'
        )
    )
}

/// Similarity of two normalized lines in [0, 1]: exact equality and
/// containment are scored directly, the rest is character-bigram Dice.
pub fn replay_similarity(candidate_norm: &str, prior_norm: &str) -> f64 {
    if candidate_norm.is_empty() || prior_norm.is_empty() {
        return 0.0;
    }
    if candidate_norm == prior_norm {
        return 1.0;
    }
    let cand: Vec<char> = candidate_norm.chars().collect();
    let prior: Vec<char> = prior_norm.chars().collect();
    let (short, long) = if cand.len() <= prior.len() {
        (&cand, &prior)
    } else {
        (&prior, &cand)
    };
    // A strict prefix that stops mid-sentence is not a shorter take on the
    // line, it is the same line cut off (2026-10-27: a half-generated reply
    // was delivered, then the completed line followed a minute later — Dice
    // scored the pair 0.76 and the length-ratio gate below waved it through).
    // Sentence-final punctuation is what separates a truncation from a
    // naturally short reply, which stays exempt.
    if short.len() as f64 / long.len() as f64 >= 0.5 {
        let short_s: String = short.iter().collect();
        let long_s: String = long.iter().collect();
        if long_s.starts_with(&short_s) && !ends_sentence_final(&short_s) {
            return 1.0;
        }
    }
    if short.len() as f64 / long.len() as f64 >= REPLAY_CONTAINMENT_RATIO {
        let short_s: String = short.iter().collect();
        let long_s: String = long.iter().collect();
        if long_s.contains(&short_s) {
            return 0.95;
        }
    }
    bigram_dice(&cand, &prior)
}

/// The best replay similarity of `candidate` against normalized priors, if
/// it clears the block threshold. Short candidates are exempt.
pub fn replay_match(candidate: &str, priors_norm: &[String]) -> Option<f64> {
    let norm = normalize_for_replay(candidate);
    if norm.chars().count() < REPLAY_MIN_CHARS {
        return None;
    }
    let mut best = 0.0f64;
    for prior in priors_norm {
        if prior.chars().count() < REPLAY_MIN_CHARS {
            continue;
        }
        best = best.max(replay_similarity(&norm, prior));
    }
    (best >= REPLAY_DICE_THRESHOLD).then_some(best)
}

/// Short preview of a replay-blocked segment for the audit trail: the
/// guard's verdicts were previously unauditable beyond the similarity
/// bucket, so a block (2026-10-27 21:17) could not be reviewed after the
/// fact. Whitespace and separators are flattened so the preview stays a
/// single audit-action token.
pub fn replay_preview(text: &str) -> String {
    text.chars()
        .take(48)
        .map(|c| {
            if c.is_whitespace() || c == '.' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Audit bucket for a blocked replay similarity (`sim90`/`sim95`/`sim100`).
pub fn replay_sim_bucket(sim: f64) -> u32 {
    if sim >= 0.99 {
        100
    } else if sim >= 0.95 {
        95
    } else {
        90
    }
}

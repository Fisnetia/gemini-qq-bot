//! Conversation continuity for a turn: the rotation seed brief and the
//! previous-batch annex with its p-number quote refs. Moved out of
//! turn.rs verbatim (std-split-turn); logic unchanged.

use crate::onebot::normalize::{IncomingMessage, Segment, SpeakerRole};
use crate::store::SqliteStore;

/// How many raw group-log messages a rotation seed replays.
pub const SEED_WINDOW_MESSAGES: i64 = 15;
/// How many active facts a rotation seed carries. Subject-bound rows win.
pub const SEED_FACT_LIMIT: i64 = 5;
/// One window line. 80 dropped the point of a message; 400 still stops a paste
/// from eating the brief.
pub const SEED_LINE_CHARS: usize = 400;
/// Hard character cap for the whole seed brief. The window is rendered before
/// the archive, so the cap drops old facts before it drops the live thread.
pub const SEED_MAX_CHARS: usize = 2_000;

/// One message the model may quote that is *not* part of the current batch:
/// either a message of the seed window (`h1`, `h2`, ... in window order) or a
/// message of the previous batch on a resident continuation turn (`p1`, `p2`,
/// ... in batch order). Either way the ref resolves to the real message, so
/// the turn can verify a history quote's echo and sender like a batch quote's.
///
/// Both families live in the same struct because the delivery side treats them
/// identically; only the ref spelling keeps them apart, and that spelling is
/// what keeps them from colliding with the batch's own `m` numbers (the
/// failure this whole channel exists to prevent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTarget {
    pub history_ref: String,
    pub message_id: String,
    pub sender_id: String,
    /// `owner` / `member` (a window's own bot lines are never numbered).
    pub speaker_role: String,
    pub text: String,
    pub has_image: bool,
}

/// Ref prefix for the previous batch's annex on a continuation turn.
///
/// Deliberately not `m` (that is this batch's space) and not `h` (that is the
/// seed window); a third letter is the only thing that makes "which number
/// space does this ref belong to" decidable by looking at the ref alone.
///
/// The numbers after the prefix do not restart at 1 on every turn: they keep
/// counting up for as long as the conversation lives, and a batch that never
/// became anyone's annex leaves a gap. That is the whole point — a turn's
/// transcript still shows the annex *two* turns back, and if `p1` were reused
/// the model's `p1` from that older prompt would resolve to a different
/// message of the newer one, which is the same silent misquoting this channel
/// exists to prevent, just one turn later. With one monotonic sequence a stale
/// `p` ref always misses and is dropped, never re-aimed.
pub const ANNEX_REF_PREFIX: &str = "p";
/// Characters of a message kept in the annex table. The table is a legend, not
/// a transcript: the full text of that batch is already in the session's own
/// multi-turn context, and re-sending it would pay for it twice.
pub const ANNEX_LINE_CHARS: usize = 80;

/// The previous batch of a resident conversation, kept between turns so the
/// next turn's model can still point at those messages.
///
/// The session's transcript keeps the previous turn's prompt verbatim, so the
/// model still *sees* those `mN` refs — but every new turn renumbers its own
/// batch from `m1`, which means a stale `mN` either resolves to nothing or,
/// far worse, to a different message that happens to share the number. This
/// cache is what lets the turn hand out a second, non-colliding name for the
/// same messages instead of silently misquoting.
///
/// It lives in process memory rather than the store: it is strictly a
/// per-session aid, it must die with the session, and reconstructing it would
/// mean re-reading a batch that is already consumed.
#[derive(Debug, Clone)]
pub struct PreviousBatch {
    /// The conversation these messages were delivered into. A turn may only
    /// use the annex when its own conversation id still equals this — after a
    /// rotation, a retire, or a rebuild into a fresh session the old refs are
    /// gone, and reviving them would leak one session's numbering into
    /// another.
    pub conversation_id: String,
    /// The rendered legend (`- [p1] 老王：xxx`), built once when the batch
    /// finished so a continuation turn only copies it into the prompt.
    pub table: String,
    /// The previous batch's quotable messages, in batch order.
    pub targets: Vec<HistoryTarget>,
    /// The next annex number this conversation may hand out. The successor's
    /// annex starts here, which is what keeps a stale ref from ever being
    /// re-issued to a different message.
    pub next_ref: usize,
    /// What the bot herself answered on the turn this batch opened, noted
    /// when that turn's reply was sent. The annex renders it as an
    /// unnumbered line: her own lines are never quote *targets* (see
    /// `assemble_annex`), but without the line the next turn cannot tell
    /// a correction of her last answer ("没叫你") from a fresh remark —
    /// her own replies appear in no prompt otherwise.
    pub own_reply: Option<String>,
}

impl PreviousBatch {
    /// Build the cache entry for a finished batch, or `None` when it held
    /// nothing quotable.
    ///
    /// `base` is the first annex number to use; the successor must continue
    /// from [`Self::next_ref`], not from 1.
    pub fn for_batch(
        conversation_id: &str,
        base: usize,
        messages: &[IncomingMessage],
    ) -> Option<Self> {
        let (table, targets) = assemble_annex(messages, base)?;
        let next_ref = base + targets.len();
        Some(Self {
            conversation_id: conversation_id.to_string(),
            table,
            targets,
            next_ref,
            own_reply: None,
        })
    }
}

/// Build the annex legend for the previous batch.
///
/// One line per message: ref, speaker, truncated text. Deliberately a legend
/// and not a transcript — the session already holds these messages in full a
/// few steps up, so the only new information is the renaming. Bot lines are
/// skipped for the same reason the seed window skips them: the model cannot
/// usefully quote itself, and numbering them would invite exactly that.
///
/// `base` is the number the first quotable message takes; see
/// [`ANNEX_REF_PREFIX`] for why the sequence never restarts.
///
/// Returns the rendered table and the targets to register, or `None` when the
/// batch held nothing quotable.
pub fn assemble_annex(
    messages: &[IncomingMessage],
    base: usize,
) -> Option<(String, Vec<HistoryTarget>)> {
    let mut out = String::new();
    let mut targets: Vec<HistoryTarget> = Vec::new();
    for message in messages {
        if message.speaker_role == SpeakerRole::Bot {
            continue;
        }
        let who = {
            let name = message.display_name.trim();
            if name.is_empty() {
                "群友".to_string()
            } else {
                name.to_string()
            }
        };
        let mut text = message.text.trim().to_string();
        if text.is_empty()
            && message
                .segments
                .iter()
                .any(|s| matches!(s, Segment::Image { .. }))
        {
            text = "[图片]".to_string();
        }
        let short: String = text.chars().take(ANNEX_LINE_CHARS).collect();
        let history_ref = format!("{ANNEX_REF_PREFIX}{}", base + targets.len());
        out.push_str(&format!("- [{history_ref}] {who}：{short}\n"));
        targets.push(HistoryTarget {
            history_ref,
            message_id: message.message_id.clone(),
            sender_id: message.sender_id.clone(),
            speaker_role: match message.speaker_role {
                SpeakerRole::Owner => "owner",
                SpeakerRole::Bot => "bot",
                SpeakerRole::Member => "member",
            }
            .to_string(),
            text: message.text.clone(),
            has_image: message
                .segments
                .iter()
                .any(|s| matches!(s, Segment::Image { .. })),
        });
    }
    if targets.is_empty() {
        return None;
    }
    Some((out.trim_end().to_string(), targets))
}

/// Assemble the rotation seed brief from its ingredients. All empty means
/// the group has no past worth seeding (a brand-new group), and no section
/// is rendered at all. Human window messages are numbered (`[h1]`, ...) so
/// a fresh conversation can quote the recent past it is answering instead
/// of pinning the quote on the current batch; the returned targets resolve
/// those refs.
///
/// The brief opens with a continuity line and the handover note: the seam
/// between the outgoing conversation and this one must be invisible to the
/// group, so the fresh session is told plainly that nothing has ended, and
/// is handed the live threads (open questions, promises, running bits)
/// before the older archive material.
pub fn assemble_seed_brief(
    handover: Option<&str>,
    chunk_summary: Option<&str>,
    facts: &[crate::store::MemoryRecord],
    window: &[crate::store::GroupMessageRecord],
    subject_names: &std::collections::HashMap<String, String>,
) -> Option<(String, Vec<HistoryTarget>)> {
    if handover.is_none() && chunk_summary.is_none() && facts.is_empty() && window.is_empty() {
        return None;
    }
    let mut history_targets: Vec<HistoryTarget> = Vec::new();
    let mut out = String::new();
    out.push_str(
        "（这是同一场对话的继续，只是你换了一个新会话：不要重新打招呼，不要复述这份简报，像没断过一样自然接话。）\n",
    );
    if let Some(note) = handover {
        out.push_str("## 接力纸条（上一个你留给现在的你）\n");
        out.push_str(note.trim());
        out.push('\n');
    }
    if !window.is_empty() {
        out.push_str("## 最近的群聊原文（接话题用，旧→新；[hN] 是该条的引用编号，回复某段是答它时在那段的 quote_ref 里填它）\n");
        for message in window {
            let who = if message.speaker_role == "bot" {
                "你".to_string()
            } else {
                message
                    .display_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "群友".to_string())
            };
            let mut text = message.text.trim().to_string();
            if text.is_empty() && message.has_image {
                text = "[图片]".to_string();
            }
            let text: String = text.chars().take(SEED_LINE_CHARS).collect();
            if message.speaker_role == "bot" {
                out.push_str(&format!("- {who}：{text}\n"));
            } else {
                let history_ref = format!("h{}", history_targets.len() + 1);
                out.push_str(&format!("- [{history_ref}] {who}：{text}\n"));
                history_targets.push(HistoryTarget {
                    history_ref,
                    message_id: message.message_id.clone(),
                    sender_id: message.sender_id.clone(),
                    speaker_role: message.speaker_role.clone(),
                    text: message.text.clone(),
                    has_image: message.has_image,
                });
            }
        }
    }
    if let Some(summary) = chunk_summary {
        out.push_str("## 已归档对话摘要（最近几段，旧→新）\n");
        out.push_str(summary.trim());
        out.push('\n');
    }
    if !facts.is_empty() {
        out.push_str("## 当前活跃事实\n");
        for fact in facts {
            // A fact whose subject is a bare id in the store reads as
            // unattributed prose in the seed; naming the subject (when the
            // content does not already open with the name) is what keeps a
            // fresh session from pinning one person's fact on another.
            let name = fact
                .subject_user_id
                .as_ref()
                .and_then(|id| subject_names.get(id));
            match name {
                Some(name) if !fact.content.trim_start().starts_with(name.as_str()) => {
                    out.push_str(&format!("- ({}) 关于{name}：{}\n", fact.kind, fact.content));
                }
                _ => {
                    out.push_str(&format!("- ({}) {}\n", fact.kind, fact.content));
                }
            }
        }
    }
    let mut out = out.trim_end().to_string();
    if out.chars().count() > SEED_MAX_CHARS {
        out = out.chars().take(SEED_MAX_CHARS).collect();
    }
    Some((out, history_targets))
}

/// Fetch and assemble the seed brief for one group. Errors degrade to no
/// seed: a fresh conversation with the ordinary context still works.
///
/// Two archived-summary sources exist and they cover different spans:
/// `memory_chunks` holds one summary per folded window (newest last), while
/// `group_memory_summaries` holds the panel's whole-store compaction of the memory table.
/// The fold is preferred because it summarises the most recent raw chat; the compaction
/// summary is the fallback for a group whose log was never folded but whose memories were
/// compacted. Only one is rendered, so the seed budget keeps room for the facts and window
/// sections below.
///
/// Seed quarantine (migration 0020): if an upstream content filter rejected
/// a prompt on this group's resident session, the seed that session was born
/// with is quarantined by hash. Assembly starts at the group's recorded
/// degradation tier — 1 drops the newest folded segment, 2 is a facts-only
/// minimal seed — and degrades further within the call whenever the
/// assembled bytes still hash to the rejected seed: a rejected seed is
/// never resubmitted verbatim. The spawn actually used is recorded so a
/// later rejection can quarantine it in turn.
pub async fn seed_brief_for(
    store: &SqliteStore,
    bot_id: &str,
    group_id: &str,
) -> Option<(String, Vec<HistoryTarget>)> {
    let quarantine = store
        .get_seed_quarantine(bot_id, group_id)
        .await
        .ok()
        .flatten();
    let start_tier = quarantine.as_ref().map(|q| q.tier).unwrap_or(0);
    let rejected_hash = quarantine.and_then(|q| q.rejected_hash);
    let mut chunk_rows = store.recent_chunks(bot_id, group_id, 3).await.ok()?;
    // The store returns newest first; reversed, the brief reads old to new
    // and the newest folded segment is the LAST element.
    chunk_rows.reverse();
    let chunk_segments: Vec<String> = chunk_rows
        .into_iter()
        .map(|chunk| chunk.summary)
        .filter(|summary| !summary.trim().is_empty())
        .collect();
    let full_chunk = if chunk_segments.is_empty() {
        store
            .get_summary(bot_id, group_id)
            .await
            .ok()
            .flatten()
            .map(|row| row.summary_text)
            .filter(|summary| !summary.trim().is_empty())
    } else {
        Some(chunk_segments.join("\n"))
    };
    // Tier 1 drops the newest folded segment. When that leaves nothing —
    // one segment, or only the compaction fallback, which is the same kind
    // of folded narrative — tier 1 has no folded narrative at all.
    let trimmed_chunk = if chunk_segments.len() > 1 {
        Some(chunk_segments[..chunk_segments.len() - 1].join("\n"))
    } else {
        None
    };
    let handover = store
        .get_handover(bot_id, group_id)
        .await
        .ok()
        .flatten()
        .map(|row| row.note_text)
        .filter(|note| !note.trim().is_empty());
    let mut facts = store.list_memories(bot_id, group_id, 200).await.ok()?;
    facts.retain(|fact| !crate::memory::retrieval::is_operational_memory(&fact.content));
    facts.sort_by(|a, b| {
        b.subject_user_id
            .is_some()
            .cmp(&a.subject_user_id.is_some())
            .then(b.updated_at.cmp(&a.updated_at))
    });
    facts.truncate(SEED_FACT_LIMIT as usize);
    let window = store
        .recent_group_messages(bot_id, group_id, SEED_WINDOW_MESSAGES)
        .await
        .ok()?;
    // Subject names for the facts section: the window is the freshest
    // display-name source there is, and the bot's own facts get its own
    // label rather than a bare id.
    let mut subject_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    subject_names.insert(bot_id.to_string(), "你（Gemini 本人）".to_string());
    for message in &window {
        if let Some(name) = message
            .display_name
            .as_ref()
            .filter(|name| !name.trim().is_empty())
        {
            subject_names.insert(message.sender_id.clone(), name.clone());
        }
    }
    for tier in start_tier..=2 {
        let (handover_t, chunk_t, window_t): (
            Option<&str>,
            Option<&str>,
            &[crate::store::GroupMessageRecord],
        ) = match tier {
            0 => (handover.as_deref(), full_chunk.as_deref(), &window),
            1 => (handover.as_deref(), trimmed_chunk.as_deref(), &window),
            _ => (None, None, &[]),
        };
        let Some((brief, targets)) =
            assemble_seed_brief(handover_t, chunk_t, &facts, window_t, &subject_names)
        else {
            // Nothing to seed with at this tier (a group whose only past is
            // a handover note, at tier 2): a seedless birth, and nothing to
            // quarantine.
            return None;
        };
        let hash = seed_fingerprint(&brief);
        if rejected_hash.as_deref() == Some(hash.as_str()) {
            continue;
        }
        let _ = store
            .record_seed_spawn(bot_id, group_id, &hash, tier, crate::now_unix())
            .await;
        return Some((brief, targets));
    }
    // Every tier assembled to the rejected bytes (a facts-only seed whose
    // facts have not moved): the only seed that cannot be refused for the
    // old reason is no seed.
    None
}

/// Fingerprint of an assembled seed brief, for the seed quarantine
/// (migration 0020). Only ever compared for equality against hashes this
/// same build produced, so a plain sha256 hex is all it needs to be.
fn seed_fingerprint(brief: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(brief.as_bytes());
    format!("{:x}", hasher.finalize())
}

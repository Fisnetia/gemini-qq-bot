//! The one door a tagged picture walks through on its way into the sticker cache.
//!
//! Both dedup gates and the write live here so no caller can reimplement a half of
//! them. Before this module the persist step had its own exact-hash check inline, and
//! every future entry point would have had to copy it; the gates are the kind of rule
//! that must have exactly one implementation or they quietly disagree with each other.
//!
//! Order is deliberate:
//!
//! 1. **Exact hash.** Same bytes, same stash entry. Cheap, no decode.
//! 2. **Perceptual hash.** QQ recompresses every upload, so the same sticker arrives
//!    with different bytes. An a-hash hit is only a *candidate*: it narrows the shelf to
//!    a handful of visually similar rows, and each of those is then checked against the
//!    description the tagger wrote for this picture. Six of sixty-four bits is coarse
//!    enough that two unrelated pictures have collided in production, and a merge is
//!    irreversible, so the hash proposes and the description disposes. The tag is
//!    already in hand here — it is this function's input — so the check costs no extra
//!    model call, only a string comparison.
//! 3. **Semantic.** Two different files can be the same joke: the same reaction face
//!    re-cropped, re-captioned, exported at another quality. Same visible text plus a
//!    near-verbatim description means the shelf already has it.
//!
//! A row that survives all three is written once, unjudged (`kept = 0`): the tagger
//! describes, she decides. Nothing here sets `kept`.

use crate::app::preflight::ImageTag;
use crate::memory::stickers::{
    self, PHASH_SAME_MAX_DISTANCE, SEMANTIC_DEDUP_MIN_SCORE, StickerDraft, is_semantic_duplicate,
    phash_async, phash_distance,
};
use crate::store::SqliteStore;

/// How many of a group's existing rows each gate compares against.
///
/// The gates are about "did this group already see this one", not "does the library
/// contain it somewhere", so a recent window is the right scope — and it keeps the cost
/// flat as the cache grows.
const GATE_CANDIDATE_LIMIT: i64 = 500;

/// What happened to one picture. Returned instead of `()` so tests and callers can tell
/// "already had it" from "stored it" without reading the database back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Not library material (a photo, a screenshot), or nothing to store.
    Skipped,
    /// A new row was written.
    Inserted(String),
    /// Same bytes as an existing row; only the sighting counter moved.
    AlreadyKnown(String),
    /// Perceptual-hash hit: same picture, different encoding.
    SamePicture(String),
    /// Semantic hit: same visible text, same description.
    SameMeaning(String),
    /// Sensitive material is recorded nowhere.
    Refused,
}

impl IngestOutcome {
    /// The row this picture resolved to, if any.
    pub fn hash(&self) -> Option<&str> {
        match self {
            IngestOutcome::Skipped | IngestOutcome::Refused => None,
            IngestOutcome::Inserted(hash)
            | IngestOutcome::AlreadyKnown(hash)
            | IngestOutcome::SamePicture(hash)
            | IngestOutcome::SameMeaning(hash) => Some(hash.as_str()),
        }
    }
}

/// Everything the gate needs about one picture.
pub struct TaggedPicture<'a> {
    /// The group that posted it. Provenance only — the cache itself is bot-wide.
    pub group_id: &'a str,
    /// The message the picture arrived on, for the audit trail.
    pub message_ref: &'a str,
    /// sha256 of the bytes as received.
    pub hash: &'a str,
    /// Raw image bytes, as downloaded.
    pub bytes: &'a [u8],
    pub tag: &'a ImageTag,
    pub now: i64,
}

/// Path the sticker file is stored under.
fn sticker_path(sticker_dir: &std::path::Path, hash: &str) -> std::path::PathBuf {
    sticker_dir.join(format!("{hash}.jpg"))
}

/// Run the gates, then either merge into the row that already describes this picture or
/// write a new one.
pub async fn ingest(
    store: &SqliteStore,
    bot_id: &str,
    sticker_dir: &std::path::Path,
    picture: TaggedPicture<'_>,
) -> IngestOutcome {
    let draft = StickerDraft {
        hash: picture.hash.to_string(),
        kind: picture.tag.kind.clone(),
        is_self: picture.tag.self_ref.clone(),
        ocr: picture.tag.ocr.clone(),
        description: picture.tag.description.clone(),
        emotion: picture.tag.emotion.clone(),
        nsfw: picture.tag.nsfw,
    };
    if !draft.is_library_material() || draft.hash.is_empty() {
        return IngestOutcome::Skipped;
    }
    // Sensitive material gets neither a file nor a row. It is not a dedup decision, it
    // is a "this never becomes shelf material" decision, so it precedes the gates.
    if draft.nsfw {
        audit(
            store,
            "sticker.ingest.nsfw",
            bot_id,
            picture.group_id,
            picture.now,
        )
        .await;
        return IngestOutcome::Refused;
    }

    // Gate 0: identical bytes. Also the cheapest, so it runs before any decode.
    if let Ok(Some(existing)) = store.sticker_by_hash(bot_id, &draft.hash).await {
        let _ = store
            .bump_sticker_seen(bot_id, &existing.hash, None, picture.now)
            .await;
        return IngestOutcome::AlreadyKnown(existing.hash);
    }

    // Gate 1: the perceptual hash narrows to candidates, the description decides.
    if let Some(fresh) = phash_async(picture.bytes.to_vec()).await
        && let Some(hit) =
            phash_verified_duplicate(store, bot_id, picture.group_id, fresh, &draft, picture.now)
                .await
    {
        let _ = store
            .bump_sticker_seen(bot_id, &hit.hash, None, picture.now)
            .await;
        let action = if hit.verified {
            "sticker.dedup.phash".to_string()
        } else {
            format!("sticker.dedup.phash.unverified.h{}", short(&draft.hash))
        };
        audit(store, &action, bot_id, picture.group_id, picture.now).await;
        return IngestOutcome::SamePicture(hit.hash);
    }

    // Gate 2: same visible text, near-identical description.
    if let Some(existing) = semantic_duplicate(store, bot_id, picture.group_id, &draft).await {
        // The longer description wins: the tagger that saw more detail keeps its work.
        let _ = store
            .bump_sticker_seen(
                bot_id,
                &existing,
                Some(draft.description.as_str()),
                picture.now,
            )
            .await;
        audit(
            store,
            "sticker.dedup.semantic",
            bot_id,
            picture.group_id,
            picture.now,
        )
        .await;
        return IngestOutcome::SameMeaning(existing);
    }

    let path = sticker_path(sticker_dir, &draft.hash);
    if !sticker_dir.is_dir() {
        let _ = tokio::fs::create_dir_all(sticker_dir).await;
    }
    if tokio::fs::write(&path, picture.bytes).await.is_err() {
        return IngestOutcome::Skipped;
    }
    let file_path = path.to_string_lossy().to_string();
    let blob = draft.search_blob();
    // The row carries its own perceptual hash so the next copy is caught by gate 1.
    let stored_phash = phash_async(picture.bytes.to_vec())
        .await
        .map(|hash| hash as i64);
    let _ = store
        .upsert_sticker(
            &draft.hash,
            bot_id,
            picture.group_id,
            &file_path,
            &draft.kind,
            &draft.is_self,
            &draft.ocr,
            &draft.description,
            &draft.emotion,
            &blob,
            stored_phash,
            draft.nsfw,
            true,
            picture.now,
        )
        .await;
    audit(
        store,
        &format!("sticker.ingest.new.h{}", short(&draft.hash)),
        bot_id,
        picture.group_id,
        picture.now,
    )
    .await;
    IngestOutcome::Inserted(draft.hash)
}

/// One row a perceptual-hash hit resolved to, and whether the description backed it up.
struct PhashHit {
    hash: String,
    /// `false` when neither side carried a description, so the merge rested on the hash
    /// alone. Kept apart from a verified merge so the two can be counted separately.
    verified: bool,
}

/// The nearest existing row within [`PHASH_SAME_MAX_DISTANCE`] whose description does not
/// contradict the hash.
///
/// Candidates are walked nearest-first rather than the nearest one simply taken: the
/// closest picture by light/dark structure is not necessarily the same joke, and the
/// second closest can still be the repost we are actually holding. A candidate whose
/// description rules it out is recorded in the audit trail before the walk continues —
/// a silent rejection is indistinguishable from a gate that never ran, and merging is
/// irreversible, so the trail is the only place this decision stays reversible in
/// analysis.
async fn phash_verified_duplicate(
    store: &SqliteStore,
    bot_id: &str,
    group_id: &str,
    fresh: u64,
    draft: &StickerDraft,
    now: i64,
) -> Option<PhashHit> {
    // Stored as a signed 64-bit integer because SQLite has no unsigned type; the bit
    // pattern survives the round trip, so the distance is taken on the raw bits.
    let candidates = store
        .sticker_phash_candidates(bot_id, group_id, GATE_CANDIDATE_LIMIT)
        .await
        .ok()?;
    let mut within: Vec<(String, u32, String)> = candidates
        .into_iter()
        .map(|(hash, stored, description)| {
            (hash, phash_distance(fresh, stored as u64), description)
        })
        .filter(|(_, distance, _)| *distance <= PHASH_SAME_MAX_DISTANCE)
        .collect();
    within.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    for (hash, _, description) in within {
        // An empty description on either side is no evidence against the hash: a legacy
        // row tagged before descriptions existed still deserves its merge. Only two
        // descriptions that are actually present and disagree overrule a candidate.
        let both_described = !description.trim().is_empty() && !draft.description.trim().is_empty();
        if both_described
            && stickers::description_similarity(&description, &draft.description)
                < SEMANTIC_DEDUP_MIN_SCORE
        {
            audit(
                store,
                &format!(
                    "sticker.dedup.phash_overruled.h{}.over{}",
                    short(&draft.hash),
                    short(&hash)
                ),
                bot_id,
                group_id,
                now,
            )
            .await;
            continue;
        }
        return Some(PhashHit {
            hash,
            verified: both_described,
        });
    }
    None
}

/// An existing row this picture is semantically identical to, if any.
async fn semantic_duplicate(
    store: &SqliteStore,
    bot_id: &str,
    group_id: &str,
    draft: &StickerDraft,
) -> Option<String> {
    if draft.ocr.trim().is_empty() {
        return None;
    }
    let candidates = store
        .sticker_semantic_candidates(bot_id, group_id, GATE_CANDIDATE_LIMIT)
        .await
        .ok()?;
    candidates
        .into_iter()
        .filter(|(_, ocr, description)| {
            is_semantic_duplicate(ocr, &draft.ocr, description, &draft.description)
        })
        .map(|(hash, _, description)| {
            let score = stickers::description_similarity(&description, &draft.description);
            (hash, score)
        })
        .filter(|(_, score)| *score >= SEMANTIC_DEDUP_MIN_SCORE)
        .max_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.0.cmp(&a.0))
        })
        .map(|(hash, _)| hash)
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(8)]
}

/// Audit is best-effort everywhere else in the codebase; a missing trail must not fail
/// an ingest.
async fn audit(store: &SqliteStore, action: &str, bot_id: &str, group_id: &str, now: i64) {
    let _ = store
        .audit("system", action, Some(bot_id), Some(group_id), now)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{KEPT_YES, migrations};

    fn picture(size: u32) -> image::RgbaImage {
        // Three bright blobs on a dark field: an 8×8 downsample keeps this structure
        // under re-encoding, which is what an a-hash is for.
        image::RgbaImage::from_fn(size, size, |x, y| {
            let fx = x as f32 / size as f32;
            let fy = y as f32 / size as f32;
            let lit = [(0.25, 0.25), (0.7, 0.35), (0.5, 0.75)]
                .iter()
                .any(|(cx, cy)| ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() < 0.16);
            if lit {
                image::Rgba([240, 240, 240, 255])
            } else {
                image::Rgba([16, 16, 16, 255])
            }
        })
    }

    fn encode(image: &image::RgbaImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut std::io::Cursor::new(&mut out), format)
            .expect("编码");
        out
    }

    fn other_picture(size: u32) -> image::RgbaImage {
        // Inverted: every one of the 64 light/dark decisions flips.
        image::RgbaImage::from_fn(size, size, |x, y| {
            let fx = x as f32 / size as f32;
            let fy = y as f32 / size as f32;
            let lit = ((fx - 0.2).powi(2) + (fy - 0.8).powi(2)).sqrt() > 0.3;
            if lit {
                image::Rgba([250, 250, 250, 255])
            } else {
                image::Rgba([8, 8, 8, 255])
            }
        })
    }

    fn tag(ocr: &str, description: &str) -> ImageTag {
        ImageTag {
            index: 1,
            kind: "sticker".into(),
            ocr: ocr.into(),
            description: description.into(),
            emotion: "无语".into(),
            self_ref: "other".into(),
            nsfw: false,
        }
    }

    async fn store(dir: &std::path::Path) -> SqliteStore {
        let pool = migrations::open_and_migrate(&dir.join("bot.sqlite3"))
            .await
            .expect("建库");
        SqliteStore::new(pool)
    }

    async fn ingest_bytes(
        store: &SqliteStore,
        dir: &std::path::Path,
        bytes: &[u8],
        tag: &ImageTag,
    ) -> IngestOutcome {
        ingest(
            store,
            "1",
            &dir.join("stickers"),
            TaggedPicture {
                group_id: "100",
                message_ref: "m1",
                hash: &stickers::hash_bytes(bytes),
                bytes,
                tag,
                now: 10,
            },
        )
        .await
    }

    #[tokio::test]
    async fn a_reencoded_copy_of_a_known_picture_is_not_a_new_row() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let original = encode(&picture(64), image::ImageFormat::Png);
        let first = ingest_bytes(&store, dir.path(), &original, &tag("", "")).await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        // Same picture, different size and different encoding: QQ's recompression.
        let recoded = encode(&picture(48), image::ImageFormat::Jpeg);
        assert_ne!(
            stickers::hash_bytes(&original),
            stickers::hash_bytes(&recoded)
        );
        let second = ingest_bytes(&store, dir.path(), &recoded, &tag("", "")).await;
        assert!(
            matches!(second, IngestOutcome::SamePicture(_)),
            "{second:?}"
        );
        assert_eq!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .len(),
            1,
            "同一张图只该有一行"
        );
        let row = store
            .sticker_by_hash("1", first.hash().unwrap())
            .await
            .expect("读")
            .expect("行存在");
        assert_eq!(row.seen_count, 2, "重复喂必须 bump seen_count");
        assert!(row.last_seen_at.is_some(), "重复喂必须记 last_seen_at");
    }

    #[tokio::test]
    async fn a_different_picture_still_gets_its_own_row() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", ""),
        )
        .await;
        let second = ingest_bytes(
            &store,
            dir.path(),
            &encode(&other_picture(64), image::ImageFormat::Png),
            &tag("", ""),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");
        assert!(matches!(second, IngestOutcome::Inserted(_)), "{second:?}");
        assert_ne!(first.hash(), second.hash());
    }

    #[tokio::test]
    async fn the_same_joke_in_another_file_merges_instead_of_duplicating() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let long = "白猫趴在键盘上，画风简笔，表情无奈，适合被说摸鱼时发";
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("不许摸鱼", long),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        // A different file (so gate 1 cannot catch it) with the same caption and a
        // paraphrased description.
        let shorter = "白猫趴在键盘上，画风简笔，表情无奈，适合被说摸鱼时发。";
        let second = ingest_bytes(
            &store,
            dir.path(),
            &encode(&other_picture(64), image::ImageFormat::Png),
            &tag("不许摸鱼", shorter),
        )
        .await;
        assert!(
            matches!(second, IngestOutcome::SameMeaning(_)),
            "{second:?}"
        );
        assert_eq!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_different_caption_is_not_a_duplicate() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let text = "白猫趴在键盘上，画风简笔，表情无奈，适合被说摸鱼时发";
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("不许摸鱼", text),
        )
        .await;
        // Same description, different visible text: a different picture of the same
        // joke. Merging these would collapse two distinct stickers into one.
        let second = ingest_bytes(
            &store,
            dir.path(),
            &encode(&other_picture(64), image::ImageFormat::Png),
            &tag("今天也要加油", text),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");
        assert!(matches!(second, IngestOutcome::Inserted(_)), "{second:?}");
    }

    #[tokio::test]
    async fn a_photo_never_enters_the_cache() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let mut photo = tag("", "一张街景照片");
        photo.kind = "photo".into();
        let outcome = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &photo,
        )
        .await;
        assert_eq!(outcome, IngestOutcome::Skipped);
        assert!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sensitive_material_is_not_stored() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let mut nsfw = tag("", "一张露骨图");
        nsfw.nsfw = true;
        let outcome = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &nsfw,
        )
        .await;
        assert_eq!(outcome, IngestOutcome::Refused);
        assert!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_fresh_row_is_unjudged_and_kept_out_of_her_library() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let outcome = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("不许摸鱼", "白猫趴在键盘上"),
        )
        .await;
        let hash = outcome.hash().expect("插入").to_string();
        let row = store
            .sticker_by_hash("1", &hash)
            .await
            .expect("读")
            .expect("行存在");
        // The tagger describes; she decides. Auto-collection must never self-promote.
        assert_eq!(row.kept, crate::store::KEPT_UNJUDGED);
        assert_ne!(row.kept, KEPT_YES);
        assert!(row.enabled);
        assert_eq!(row.seen_count, 1);
        assert!(row.phash.is_some(), "新行必须带上自己的 phash");
        assert!(row.search_blob.contains("不许摸鱼"));
        assert!(store.list_library("1", 10).await.expect("读").is_empty());
    }

    /// Every audit action recorded so far, oldest first.
    async fn audit_actions(store: &SqliteStore) -> Vec<String> {
        store
            .recent_audit(100)
            .await
            .expect("审计")
            .into_iter()
            .map(|event| event.action)
            .rev()
            .collect()
    }

    /// The same blobs as [`picture`] with a dark strip laid down the left edge: a
    /// visibly different drawing that the a-hash still calls a candidate (Hamming 5 at
    /// `strip = 0.25`). `strip` widens the band, which is how the second test builds a
    /// decoy nearer to itself than the row it is really a copy of.
    fn rival_picture(size: u32, strip: f32) -> image::RgbaImage {
        image::RgbaImage::from_fn(size, size, |x, y| {
            let fx = x as f32 / size as f32;
            let fy = y as f32 / size as f32;
            if fx < strip {
                return image::Rgba([8, 8, 8, 255]);
            }
            let lit = [(0.25, 0.25), (0.7, 0.35), (0.5, 0.75)]
                .iter()
                .any(|(cx, cy)| ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() < 0.16);
            if lit {
                image::Rgba([240, 240, 240, 255])
            } else {
                image::Rgba([16, 16, 16, 255])
            }
        })
    }

    #[tokio::test]
    async fn a_phash_hit_with_a_contradicting_description_is_overruled() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", "白猫趴在键盘上，画风简笔，表情无奈"),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        // Same light/dark structure, so the hash will offer the first row as a
        // candidate; a different drawing, which only the description can tell.
        let rival = encode(&rival_picture(64, 0.25), image::ImageFormat::Png);
        let distance = stickers::phash_distance(
            stickers::phash(&encode(&picture(64), image::ImageFormat::Png)).expect("解码"),
            stickers::phash(&rival).expect("解码"),
        );
        assert!(
            distance <= PHASH_SAME_MAX_DISTANCE,
            "造图前提：汉明距 {distance} 必须落在假阳性区间"
        );

        let second = ingest_bytes(
            &store,
            dir.path(),
            &rival,
            &tag("", "黑色柴犬张嘴大笑，写实风格，背景是海边"),
        )
        .await;
        assert!(matches!(second, IngestOutcome::Inserted(_)), "{second:?}");
        assert_ne!(first.hash(), second.hash());
        assert_eq!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .len(),
            2,
            "被推翻的候选不得合并"
        );

        let actions = audit_actions(&store).await;
        assert!(
            actions.iter().any(|action| {
                action.starts_with("sticker.dedup.phash_overruled.h")
                    && action.contains(&format!(".over{}", short(first.hash().unwrap())))
            }),
            "被推翻必须留痕，实际 {actions:?}"
        );
        assert!(
            !actions.iter().any(|action| action == "sticker.dedup.phash"),
            "没有复核通过的合并，不该有 phash 合并审计：{actions:?}"
        );
    }

    #[tokio::test]
    async fn a_phash_hit_with_a_matching_description_still_merges() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let original = "白猫趴在键盘上，画风简笔，表情无奈，适合被说摸鱼时发";
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", original),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        // The recompressed repost of the very same picture, described the same way.
        let second = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(48), image::ImageFormat::Jpeg),
            &tag("", original),
        )
        .await;
        assert!(
            matches!(second, IngestOutcome::SamePicture(ref hash) if hash == first.hash().unwrap()),
            "复核通过必须合并回候选行：{second:?}"
        );
        assert_eq!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .len(),
            1
        );

        let actions = audit_actions(&store).await;
        assert!(
            actions.iter().any(|action| action == "sticker.dedup.phash"),
            "复核通过的合并沿用 sticker.dedup.phash：{actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|action| action.starts_with("sticker.dedup.phash.unverified")),
            "复核通过的合并不该带 unverified：{actions:?}"
        );
    }

    #[tokio::test]
    async fn a_phash_hit_without_descriptions_merges_but_is_marked_unverified() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", ""),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        let recoded = encode(&picture(48), image::ImageFormat::Jpeg);
        let second = ingest_bytes(&store, dir.path(), &recoded, &tag("", "")).await;
        assert!(
            matches!(second, IngestOutcome::SamePicture(ref hash) if hash == first.hash().unwrap()),
            "描述缺失维持合并：{second:?}"
        );
        assert_eq!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .len(),
            1
        );

        let actions = audit_actions(&store).await;
        assert!(
            actions.iter().any(|action| action
                == &format!(
                    "sticker.dedup.phash.unverified.h{}",
                    short(&stickers::hash_bytes(&recoded))
                )),
            "描述缺失的合并必须与复核通过可区分：{actions:?}"
        );
        assert!(
            !actions.iter().any(|action| action == "sticker.dedup.phash"),
            "缺描述的合并不得冒充复核通过：{actions:?}"
        );
    }

    #[tokio::test]
    async fn a_one_sided_description_still_merges_but_is_unverified() {
        // Either side missing is no evidence against the hash — a legacy row, or a
        // tagger that left the caption empty. Both directions must land the same way:
        // merge, and say in the trail that nothing verified it.
        let dir = tempfile::tempdir().expect("tmp");
        let store_a = store(dir.path()).await;
        let original = "白猫趴在键盘上，画风简笔，表情无奈";

        let first = ingest_bytes(
            &store_a,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", original),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        // Candidate described, incoming caption empty.
        let recoded = encode(&picture(48), image::ImageFormat::Jpeg);
        let second = ingest_bytes(&store_a, dir.path(), &recoded, &tag("", "")).await;
        assert!(
            matches!(second, IngestOutcome::SamePicture(ref hash) if hash == first.hash().unwrap()),
            "候选有描述、新图无描述：维持合并 {second:?}"
        );
        let actions = audit_actions(&store_a).await;
        assert!(
            actions.iter().any(|action| action
                == &format!(
                    "sticker.dedup.phash.unverified.h{}",
                    short(&stickers::hash_bytes(&recoded))
                )),
            "单边缺描述必须记 unverified：{actions:?}"
        );

        // Candidate empty, incoming caption present.
        let dir2 = tempfile::tempdir().expect("tmp");
        let store_b = store(dir2.path()).await;
        let third = ingest_bytes(
            &store_b,
            dir2.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", ""),
        )
        .await;
        assert!(matches!(third, IngestOutcome::Inserted(_)), "{third:?}");
        let recoded2 = encode(&picture(48), image::ImageFormat::Jpeg);
        let fourth = ingest_bytes(&store_b, dir2.path(), &recoded2, &tag("", original)).await;
        assert!(
            matches!(fourth, IngestOutcome::SamePicture(ref hash) if hash == third.hash().unwrap()),
            "候选无描述、新图有描述：维持合并 {fourth:?}"
        );
        let actions2 = audit_actions(&store_b).await;
        assert!(
            actions2.iter().any(|action| action
                == &format!(
                    "sticker.dedup.phash.unverified.h{}",
                    short(&stickers::hash_bytes(&recoded2))
                )),
            "反方向同样记 unverified：{actions2:?}"
        );
    }

    #[tokio::test]
    async fn an_overruled_nearest_candidate_gives_the_next_one_a_chance() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = store(dir.path()).await;
        let original = "白猫趴在键盘上，画风简笔，表情无奈，适合被说摸鱼时发";
        let first = ingest_bytes(
            &store,
            dir.path(),
            &encode(&picture(64), image::ImageFormat::Png),
            &tag("", original),
        )
        .await;
        assert!(matches!(first, IngestOutcome::Inserted(_)), "{first:?}");

        // A different picture that happens to land inside the distance: the nearest
        // neighbour by hash, and the one the description must throw out.
        let rival = encode(&rival_picture(64, 0.25), image::ImageFormat::Png);
        let second = ingest_bytes(
            &store,
            dir.path(),
            &rival,
            &tag("", "黑色柴犬张嘴大笑，写实风格，背景是海边"),
        )
        .await;
        assert!(matches!(second, IngestOutcome::Inserted(_)), "{second:?}");

        // A near-copy of the *rival* arrives, described as the first picture. The rival
        // is the nearer hit and the description must throw it out; the walk then has to
        // reach the first row, which is farther away but actually describes this file.
        let fresh = encode(&rival_picture(64, 0.27), image::ImageFormat::Png);
        let fresh_phash = stickers::phash(&fresh).expect("解码");
        let rival_distance =
            stickers::phash_distance(fresh_phash, stickers::phash(&rival).expect("解码"));
        let first_distance = stickers::phash_distance(
            fresh_phash,
            stickers::phash(&encode(&picture(64), image::ImageFormat::Png)).expect("解码"),
        );
        assert!(
            rival_distance <= PHASH_SAME_MAX_DISTANCE && first_distance <= PHASH_SAME_MAX_DISTANCE,
            "造图前提：两个候选都须落在候选区间（rival {rival_distance} / 原图 {first_distance}）"
        );
        assert!(
            rival_distance < first_distance,
            "造图前提：被推翻的候选必须更近（rival {rival_distance} vs 原图 {first_distance}），\
             否则这条用例钉不住「逐个复核」"
        );

        let third = ingest_bytes(&store, dir.path(), &fresh, &tag("", original)).await;
        assert!(
            matches!(third, IngestOutcome::SamePicture(ref hash) if hash == first.hash().unwrap()),
            "最近邻被推翻时次近邻仍须有机会：{third:?}"
        );
        assert_eq!(
            store
                .list_stickers_page("1", None, 10, 0)
                .await
                .expect("读")
                .len(),
            2,
            "只有前两行：第三张必须合并回第一行"
        );
        let actions = audit_actions(&store).await;
        assert!(
            actions.iter().any(|action| action
                == &format!(
                    "sticker.dedup.phash_overruled.h{}.over{}",
                    short(&stickers::hash_bytes(&fresh)),
                    short(second.hash().unwrap())
                )),
            "更近的假阳性必须被记成推翻：{actions:?}"
        );
        assert!(
            actions.iter().any(|action| action == "sticker.dedup.phash"),
            "次近邻复核通过后仍要留 phash 合并审计：{actions:?}"
        );
    }
}

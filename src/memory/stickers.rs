//! Sticker library: hashing, perceptual dedup, search blob construction, and weighted
//! retrieval.
//!
//! The preflight tagger decides *what* a picture is; this module decides *whether it
//! is worth keeping* and *how to find it again*. Four rules drive the design:
//!
//! 1. A sticker is identified by the sha256 of its bytes. A repost costs zero tagging
//!    tokens because the row already exists.
//! 2. A re-encoded repost is not a different picture. QQ recompresses every upload, so
//!    the sha256 of two copies of the same sticker differs. The perceptual hash
//!    ([`phash`]) is the identity that survives that.
//! 3. Retrieval scores fields with different weights instead of one flat blob score.
//!    `ocr` is what the picture literally says, `emotion` is its tone, and
//!    `description` is everything else — the tagger writes one short caption in
//!    place of the old `scene` / `intent` / `usage_hint` triple.

use sha2::{Digest, Sha256};

/// Fields the tagger produced for one picture, already normalised for storage.
#[derive(Debug, Clone, Default)]
pub struct StickerDraft {
    pub hash: String,
    pub kind: String,
    pub is_self: String,
    pub ocr: String,
    /// One short caption (≤30 chars since 2026-10-27; it was a full paragraph
    /// before, which cost prompt tokens everywhere it was rendered): the subject
    /// and its defining look, the expression or action, the art style if
    /// distinctive. Replaces the old `scene` + `intent` + `usage_hint` triple.
    pub description: String,
    /// Joined with "/" for storage; the panel splits on it.
    pub emotion: String,
    pub nsfw: bool,
}

impl StickerDraft {
    /// Only real stickers enter the library. A photo or a screenshot is conversation
    /// material, not something she would ever send back as a reaction.
    pub fn is_library_material(&self) -> bool {
        self.kind == "sticker"
    }

    /// A sticker that is not hers is still keepable, so `is_self` never gates entry.
    /// It only gates how the panel groups the shelf.
    pub fn search_blob(&self) -> String {
        sticker_search_blob(&self.ocr, &self.emotion, &self.description, "", "", "")
    }
}

/// Build the one text column retrieval keeps around, from a description-first row or a
/// legacy one.
///
/// A row tagged after the description change carries `description` and leaves the three
/// legacy columns empty, so the blob is `description + emotion + ocr`. A row tagged
/// before it has an empty `description` and its signal lives in the legacy columns, so
/// the blob falls back to all five. One function so the two shapes cannot drift.
pub fn sticker_search_blob(
    ocr: &str,
    emotion: &str,
    description: &str,
    scene: &str,
    intent: &str,
    usage_hint: &str,
) -> String {
    if description.trim().is_empty() {
        return build_search_blob(&[ocr, scene, emotion, intent, usage_hint]);
    }
    build_search_blob(&[description, emotion, ocr])
}

/// Lowercase, deduplicate, and join every text field so one column can be scored.
///
/// Repeats are dropped because the same word in `ocr` and `intent` would otherwise
/// double its own weight, which makes scoring depend on how verbose the tagger was.
pub fn build_search_blob(fields: &[&str]) -> String {
    let mut seen: Vec<String> = Vec::new();
    for field in fields {
        for token in field.split(|c: char| c.is_whitespace() || "、，,/|".contains(c)) {
            let token = token.trim().to_lowercase();
            if token.is_empty() {
                continue;
            }
            if !seen.iter().any(|t| t == &token) {
                seen.push(token);
            }
        }
    }
    seen.join(" ")
}

/// sha256 of the raw image bytes, lowercase hex. The exact identity a sticker has.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 64-bit average hash of a decoded picture.
///
/// Algorithm (the classic `a-hash`): decode, convert to 8-bit greyscale, box-downsample
/// to 8×8, take the mean brightness of those 64 samples, and set bit *i* when sample *i*
/// is at or above the mean. Comparing two hashes by Hamming distance therefore asks
/// "does the light/dark structure match", which is invariant to re-encoding, to a
/// re-upload that only changed JPEG quality, and to a resized copy. It is coarse on
/// purpose: a near-identical crop still matches, two different pictures do not.
///
/// `None` when the bytes are not a decodable image, or when the decoder is unavailable;
/// a missing hash never matches anything.
pub fn phash(bytes: &[u8]) -> Option<u64> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let decoded = reader.decode().ok()?;
    Some(phash_of(&decoded))
}

/// The hash of an already-decoded picture. Split out so a caller that has the image in
/// hand does not pay for a second decode.
pub fn phash_of(image: &image::DynamicImage) -> u64 {
    let small = image
        .resize_exact(8, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let samples: Vec<u32> = small.pixels().map(|p| u32::from(p.0[0])).collect();
    let mean = samples.iter().map(|v| f64::from(*v)).sum::<f64>() / (samples.len() as f64);
    let mut hash = 0u64;
    for (index, value) in samples.iter().enumerate() {
        if f64::from(*value) >= mean {
            hash |= 1 << index;
        }
    }
    hash
}

/// Hamming distance between two perceptual hashes: how many of the 64 light/dark
/// decisions disagree.
pub fn phash_distance(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}

/// Distance at or below which two hashes are the same picture.
///
/// Six of sixty-four bits is the usual a-hash cut-off: a re-encode moves a handful of
/// bits around the mean, while two unrelated pictures differ in roughly a third of them.
pub const PHASH_SAME_MAX_DISTANCE: u32 = 6;

/// Async wrapper around [`phash`], for the same reason `resize::prepare_async` exists:
/// decoding is CPU work and must not run on the reactor thread.
pub async fn phash_async(bytes: Vec<u8>) -> Option<u64> {
    tokio::task::spawn_blocking(move || phash(&bytes))
        .await
        .ok()
        .flatten()
}

/// Character-bigram Dice similarity between two descriptions, in `0.0..=1.0`.
///
/// Same measure as [`score_text`] uses per field, applied to a whole paragraph: the
/// point is "did the tagger say the same thing twice", not a ranked query match.
pub fn description_similarity(left: &str, right: &str) -> f64 {
    let grams = bigrams(left);
    overlap(&grams, right)
}

/// Score at or above which two descriptions are treated as the same picture.
pub const SEMANTIC_DEDUP_MIN_SCORE: f64 = 0.8;

/// The semantic gate: same visible text, same description → the same sticker.
///
/// Both halves are required. The `ocr` has to be non-empty and exactly equal (a picture
/// with no text at all says nothing about identity), and the descriptions have to be
/// near-verbatim. An empty `ocr` therefore never merges anything, which is what keeps
/// two different textless reaction faces apart.
pub fn is_semantic_duplicate(
    existing_ocr: &str,
    new_ocr: &str,
    existing_description: &str,
    new_description: &str,
) -> bool {
    let existing_ocr = existing_ocr.trim();
    if existing_ocr.is_empty() || existing_ocr != new_ocr.trim() {
        return false;
    }
    if existing_description.trim().is_empty() || new_description.trim().is_empty() {
        return false;
    }
    description_similarity(existing_description, new_description) >= SEMANTIC_DEDUP_MIN_SCORE
}

/// The text fields retrieval scores over, borrowed from either a fresh draft
/// or a stored row. One shape for both, so there is exactly one scoring
/// implementation: the MCP `sticker_search` and the server-side per-batch
/// preselection both call [`score_text`], and a fix here fixes both.
#[derive(Debug, Clone, Copy, Default)]
pub struct StickerText<'a> {
    pub ocr: &'a str,
    pub scene: &'a str,
    pub emotion: &'a str,
    pub intent: &'a str,
    pub usage_hint: &'a str,
}

/// Fill [`StickerText`] from either row shape.
///
/// A description-first row scores its paragraph in the three legacy slots at once. That
/// is the point, not a loophole: the old triple asked three questions (`scene`, `intent`,
/// `usage_hint`) that a single paragraph now answers, so the paragraph inherits exactly
/// the weight budget those three carried — `W_USAGE + W_INTENT + W_SCENE`. Dropping it
/// into one slot would silently cut the budget from 5.3 to 0.8 and leave every
/// description-tagged row unsearchable, because a coarse Dice ratio over a long paragraph
/// is small by construction. The weights themselves are untouched.
///
/// A legacy row (empty description) keeps its own fields untouched.
pub fn sticker_text<'a>(
    ocr: &'a str,
    emotion: &'a str,
    description: &'a str,
    scene: &'a str,
    intent: &'a str,
    usage_hint: &'a str,
) -> StickerText<'a> {
    if description.trim().is_empty() {
        return StickerText {
            ocr,
            scene,
            emotion,
            intent,
            usage_hint,
        };
    }
    StickerText {
        ocr,
        scene: description,
        emotion,
        intent: description,
        usage_hint: description,
    }
}

impl StickerDraft {
    /// The fields retrieval scores over, in the shared shape.
    pub fn text(&self) -> StickerText<'_> {
        sticker_text(&self.ocr, &self.emotion, &self.description, "", "", "")
    }
}

impl crate::store::StickerRecord {
    /// The fields retrieval scores over, in the shared shape.
    pub fn text(&self) -> StickerText<'_> {
        sticker_text(
            &self.ocr,
            &self.emotion,
            &self.description,
            &self.scene,
            &self.intent,
            &self.usage_hint,
        )
    }
}

/// Minimum score for a sticker to count as a match at all — returned by
/// `sticker_search` or offered by the server-side per-batch preselection.
///
/// Calibrated against the weights below: an exact emotion hit on a short
/// query lands around 0.6, an exact usage phrase well above 1.0, while an
/// incidental scene overlap stays under 0.2. Below this line the honest
/// answer is "no match", never the least-bad row in the library — a search
/// that always returns something teaches the model that searching finds
/// things that are not there.
pub const SEARCH_MIN_SCORE: f64 = 0.35;

/// Score at or above which a match is labelled strong.
pub const STRONG_MATCH_SCORE: f64 = 1.2;

/// Lower bar used only by the server-side per-batch preselection that puts a
/// menu in front of the model. Deliberately below [`SEARCH_MIN_SCORE`]: a
/// search that answers "nothing" is correct, but a menu that answers
/// "nothing" leaves the model with no sticker to pick, which is how a whole
/// day of traffic ends in zero sends. A weak offer costs one line of prompt;
/// a missing offer costs the chance to react at all.
pub const OFFER_MIN_SCORE: f64 = 0.25;

/// Multiplier applied to a candidate this group already saw recently, in
/// both places candidates are ranked: the server-side preselection
/// (`select_sticker_offers`) and `sticker_search`.
///
/// A soft penalty, deliberately not a filter: the sticker stays selectable when it is
/// the only thing that fits, but it no longer wins just for being familiar. The user
/// explicitly rejected a hard "same sticker 30 minutes" cool-down. Strengthened
/// from a half to a quarter on 2026-11-01: at a half, sends still concentrated
/// on a few favourites (top sticker 13% of a week's sends).
pub const RECENT_SEND_DAMPING: f64 = 0.25;

/// Human-readable match tier for a score; `None` below [`SEARCH_MIN_SCORE`].
pub fn match_tier(score: f64) -> Option<&'static str> {
    if score >= STRONG_MATCH_SCORE {
        Some("很贴")
    } else if score >= SEARCH_MIN_SCORE {
        Some("一般")
    } else {
        None
    }
}

/// Field weights — the single canonical set (the MCP copy that used to
/// duplicate these with different values is gone). `ocr` is what the picture
/// literally says, `usage_hint` is the situation, `intent` is the meaning:
/// those carry the signal. `scene` and `emotion` only help.
const W_OCR: f64 = 3.0;
const W_USAGE: f64 = 2.5;
const W_INTENT: f64 = 2.0;
const W_EMOTION: f64 = 2.0;
const W_SCENE: f64 = 0.8;

/// Character bigrams: no tokenizer, no dictionary, works for Chinese.
fn bigrams(text: &str) -> Vec<String> {
    let chars: Vec<char> = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<Vec<_>>();
    if chars.len() < 2 {
        return chars.iter().map(|c| c.to_string()).collect();
    }
    chars.windows(2).map(|w| w.iter().collect()).collect()
}

/// Dice coefficient over character bigrams, in `0.0..=1.0`.
fn overlap(query: &[String], field: &str) -> f64 {
    if query.is_empty() || field.trim().is_empty() {
        return 0.0;
    }
    let target = bigrams(field);
    if target.is_empty() {
        return 0.0;
    }
    let shared = query.iter().filter(|g| target.contains(g)).count();
    (2.0 * shared as f64) / ((query.len() + target.len()) as f64)
}

/// Score one sticker's text fields against a query: weighted char-bigram
/// Dice per field, summed. THE scoring implementation for the whole crate —
/// `sticker_search` and the server-side preselection both call this.
///
/// Deliberately not a filter: a zero score is returned as 0.0 so the caller
/// applies [`SEARCH_MIN_SCORE`] and decides what "nothing relevant" means.
pub fn score_text(query: &str, text: &StickerText<'_>) -> f64 {
    let grams = bigrams(query);
    if grams.is_empty() {
        return 0.0;
    }
    W_OCR * overlap(&grams, text.ocr)
        + W_USAGE * overlap(&grams, text.usage_hint)
        + W_INTENT * overlap(&grams, text.intent)
        + W_EMOTION * overlap(&grams, text.emotion)
        + W_SCENE * overlap(&grams, text.scene)
}

/// Score one draft against a query. Kept as a thin delegate so existing
/// callers and tests read naturally; the work happens in [`score_text`].
pub fn score(query: &str, draft: &StickerDraft) -> f64 {
    score_text(query, &draft.text())
}

/// The answer when nothing matched. Fixed text, so an empty result never leaks the
/// library's size or its contents.
pub const NO_STICKERS: &str = "表情库里没有合适的图。";

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(hash: &str, ocr: &str, description: &str) -> StickerDraft {
        StickerDraft {
            hash: hash.to_string(),
            kind: "sticker".into(),
            is_self: "other".into(),
            ocr: ocr.into(),
            description: description.into(),
            emotion: String::new(),
            nsfw: false,
        }
    }

    fn record(hash: &str, ocr: &str, description: &str) -> crate::store::StickerRecord {
        crate::store::StickerRecord {
            hash: hash.to_string(),
            bot_id: "1".into(),
            group_id: "2".into(),
            file_path: String::new(),
            kind: "sticker".into(),
            is_self: "other".into(),
            ocr: ocr.into(),
            scene: String::new(),
            emotion: String::new(),
            intent: String::new(),
            usage_hint: String::new(),
            description: description.into(),
            search_blob: String::new(),
            phash: None,
            last_seen_at: None,
            seen_count: 1,
            nsfw: false,
            enabled: true,
            kept: 1,
            created_at: 0,
        }
    }

    /// A 64×64 test picture with three light blobs on a dark field, and a jpeg/png
    /// re-encode of the same thing at another size.
    fn picture(size: u32) -> image::RgbaImage {
        image::RgbaImage::from_fn(size, size, |x, y| {
            let fx = f64::from(x) / f64::from(size);
            let fy = f64::from(y) / f64::from(size);
            let blob = (fx - 0.25).powi(2) + (fy - 0.3).powi(2) < 0.05
                || (fx - 0.7).powi(2) + (fy - 0.25).powi(2) < 0.03
                || (fx - 0.5).powi(2) + (fy - 0.75).powi(2) < 0.04;
            let value = if blob { 235 } else { 20 };
            image::Rgba([value, value, value, 255])
        })
    }

    fn encode(image: &image::RgbaImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut std::io::Cursor::new(&mut out), format)
            .expect("编码");
        out
    }

    #[test]
    fn hash_is_stable_lowercase_hex() {
        let a = hash_bytes(b"sticker");
        let b = hash_bytes(b"sticker");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(a, hash_bytes(b"sticker2"));
    }

    #[test]
    fn search_blob_deduplicates_across_fields() {
        let blob = build_search_blob(&["笨蛋", "笨蛋 委屈", "被说笨时"]);
        assert_eq!(blob, "笨蛋 委屈 被说笨时");
    }

    #[test]
    fn a_description_row_blobs_description_emotion_and_ocr() {
        let blob = sticker_search_blob("不许摸鱼", "无奈", "白猫趴在键盘上，画风简笔", "", "", "");
        // The blob is a token bag: punctuation splits, so the sentence does not survive
        // as one string. Order follows the field order — description, emotion, ocr.
        assert_eq!(blob, "白猫趴在键盘上 画风简笔 无奈 不许摸鱼");
    }

    #[test]
    fn a_legacy_row_blobs_its_old_fields() {
        // Rows tagged before the description change hold their signal in the legacy
        // columns; the blob must keep covering them or they become unsearchable.
        let blob = sticker_search_blob("不许摸鱼", "无奈", "", "白猫趴键盘", "吐槽加班", "上班时");
        assert_eq!(blob, "不许摸鱼 白猫趴键盘 无奈 吐槽加班 上班时");
    }

    #[test]
    fn only_stickers_enter_the_library() {
        let mut d = draft("h", "x", "y");
        assert!(d.is_library_material());
        d.kind = "photo".into();
        assert!(!d.is_library_material());
        d.kind = "screenshot".into();
        assert!(!d.is_library_material());
    }

    #[test]
    fn a_description_hit_is_what_makes_a_new_row_searchable() {
        let d = draft(
            "h",
            "",
            "银发猫耳少女抱着枕头倒在床上，眼睛半闭，画风圆润可爱，适合说困了",
        );
        // A description row is meant to be found by the situation, not by one word in
        // it: "抱着枕头困了" shares enough bigrams with the paragraph to clear the
        // search threshold through the three slots the paragraph fills.
        assert!(score("抱着枕头困了", &d) >= SEARCH_MIN_SCORE);
        // A one-word query against a 30-character paragraph cannot clear it — coarse
        // Dice over prose is small by construction. This is the known shape of the
        // trade-off, and the per-batch preselection still sees it via `ocr`/`emotion`.
        assert!(score("困了", &d) > 0.0);
        assert_eq!(match_tier(score("量子力学考试", &d)), None);
    }

    #[test]
    fn a_record_and_a_draft_with_the_same_text_score_identically() {
        // The shared-shape promise: search (records) and preselection must
        // never drift apart, because both go through `score_text`.
        let d = draft("h", "不许摸鱼", "白猫趴在键盘上，画风简笔，适合吐槽加班");
        let stored = record("h", "不许摸鱼", "白猫趴在键盘上，画风简笔，适合吐槽加班");
        assert_eq!(
            score_text("摸鱼", &stored.text()),
            score_text("摸鱼", &d.text())
        );
        assert!(score_text("摸鱼", &stored.text()) > SEARCH_MIN_SCORE);
    }

    #[test]
    fn a_legacy_record_keeps_scoring_on_its_old_fields() {
        let mut stored = record("h", "不许摸鱼", "");
        stored.usage_hint = "上班时表达无奈".into();
        // "摸鱼" is one query bigram against "不许摸鱼" (three) → Dice 0.5, so the ocr
        // slot alone contributes W_OCR * 0.5. The point of the assertion is that no
        // description slot is invented for a legacy row.
        assert_eq!(
            score_text("摸鱼", &stored.text()),
            W_OCR * 0.5 + W_USAGE * 0.0,
            "legacy rows must not gain or lose weight from the description change"
        );
        assert!(score_text("上班时", &stored.text()) > 0.0);
    }

    #[test]
    fn match_tiers_follow_the_threshold_constants() {
        assert_eq!(match_tier(0.0), None);
        assert_eq!(match_tier(SEARCH_MIN_SCORE - 0.01), None);
        assert_eq!(match_tier(SEARCH_MIN_SCORE), Some("一般"));
        assert_eq!(match_tier(STRONG_MATCH_SCORE), Some("很贴"));
        // An unrelated query against a real row stays below the line: this is
        // the B1 promise that search may honestly find nothing.
        let d = draft("h", "不许摸鱼", "白猫趴在键盘上");
        assert_eq!(match_tier(score("量子力学考试", &d)), None);
    }

    #[test]
    fn empty_query_scores_zero() {
        assert_eq!(score("", &draft("h", "有字", "有描述")), 0.0);
    }

    #[test]
    fn the_same_picture_at_another_size_or_encoding_is_within_the_distance() {
        let base = picture(64);
        let reference = phash(&encode(&base, image::ImageFormat::Png)).expect("解码");
        // Same drawing, re-encoded as jpeg and re-saved at another size: the sha256
        // changes, the picture does not.
        for variant in [
            encode(&base, image::ImageFormat::Jpeg),
            encode(
                &image::imageops::resize(&base, 128, 128, image::imageops::FilterType::Triangle),
                image::ImageFormat::Png,
            ),
            encode(
                &image::imageops::resize(&base, 40, 40, image::imageops::FilterType::Triangle),
                image::ImageFormat::Jpeg,
            ),
        ] {
            let other = phash(&variant).expect("解码");
            assert_ne!(
                hash_bytes(&variant),
                hash_bytes(&encode(&base, image::ImageFormat::Png))
            );
            let distance = phash_distance(reference, other);
            assert!(
                distance <= PHASH_SAME_MAX_DISTANCE,
                "重编码后汉明距 {distance} 超过 {}",
                PHASH_SAME_MAX_DISTANCE
            );
        }
    }

    #[test]
    fn a_clearly_different_picture_is_far_away() {
        let dark = picture(64);
        // Same size, inverted layout: light field with dark blobs. Nothing about this
        // is the same picture, and the gate must say so.
        let inverted = image::RgbaImage::from_fn(64, 64, |x, y| {
            let fx = f64::from(x) / 64.0;
            let fy = f64::from(y) / 64.0;
            let blob = (fx - 0.25).powi(2) + (fy - 0.3).powi(2) < 0.05
                || (fx - 0.7).powi(2) + (fy - 0.25).powi(2) < 0.03
                || (fx - 0.5).powi(2) + (fy - 0.75).powi(2) < 0.04;
            let value = if blob { 20 } else { 235 };
            image::Rgba([value, value, value, 255])
        });
        let left = phash(&encode(&dark, image::ImageFormat::Png)).expect("解码");
        let right = phash(&encode(&inverted, image::ImageFormat::Png)).expect("解码");
        assert!(
            phash_distance(left, right) > PHASH_SAME_MAX_DISTANCE,
            "换了一张图必须拉开距离，实际 {}",
            phash_distance(left, right)
        );

        let stripes = image::RgbaImage::from_fn(64, 64, |x, _| {
            let value = if x % 8 < 4 { 250 } else { 5 };
            image::Rgba([value, value, value, 255])
        });
        let other = phash(&encode(&stripes, image::ImageFormat::Png)).expect("解码");
        assert!(phash_distance(left, other) > PHASH_SAME_MAX_DISTANCE);
    }

    #[test]
    fn garbage_has_no_perceptual_hash() {
        assert_eq!(phash(b"not an image at all"), None);
    }

    #[test]
    fn a_paraphrased_description_with_the_same_text_is_a_duplicate() {
        let a = "银发猫耳少女捂着脸，害羞地别过头，画风柔和，适合被夸时";
        let b = "银发猫耳少女捂着脸，害羞地别过头，画风柔和，被夸的时候发";
        assert!(description_similarity(a, b) >= SEMANTIC_DEDUP_MIN_SCORE);
        assert!(is_semantic_duplicate("不许摸鱼", "不许摸鱼", a, b));
    }

    #[test]
    fn a_different_ocr_or_a_different_picture_is_not_a_duplicate() {
        let a = "白猫趴在键盘上，画风简笔";
        let b = "白猫趴在键盘上，画风简笔，多了一只鼠标";
        // Same picture, different visible text: never merge, the text is what differs.
        assert!(!is_semantic_duplicate("不许摸鱼", "努力工作", a, b));
        // No text at all on either side: the gate has nothing to stand on.
        assert!(!is_semantic_duplicate("", "", a, b));
        assert!(!is_semantic_duplicate("   ", "   ", a, b));
        // Same text, genuinely different picture.
        assert!(!is_semantic_duplicate(
            "不许摸鱼",
            "不许摸鱼",
            "白猫趴在键盘上，画风简笔",
            "黑色柴犬张嘴大笑，写实风格，背景是海边"
        ));
    }
}

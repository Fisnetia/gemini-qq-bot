//! Admin panel sticker endpoints.
//! Moved out of routes.rs verbatim (std-split-routes); logic unchanged.

use super::*;

/// Recent audit trail.
/// Detect an image type from its magic bytes. Only the four formats the pipeline can
/// produce are accepted; anything else is treated as missing rather than served.
pub(super) fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    None
}

/// Serve one sticker's bytes so the panel can show a thumbnail.
///
/// The path is an allowlisted hash, never a filesystem path: the row's `file_path` is read
/// from the database and the caller cannot influence it, so this is not a file server.
pub(super) async fn sticker_image(
    State(state): State<Arc<AdminState>>,
    AxumPath(hash): AxumPath<String>,
) -> ApiResult {
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request("bad_hash"));
    }
    let Some(sticker) = state
        .store
        .sticker_by_hash(&state.bot_id, &hash)
        .await
        .map_err(|_| ApiError::internal())?
    else {
        return Err(ApiError::not_found("unknown_sticker"));
    };
    let bytes = tokio::fs::read(&sticker.file_path)
        .await
        .map_err(|_| ApiError::not_found("sticker_image_missing"))?;
    // The file is stored under a `.jpg` name whatever the source format was, so the type
    // is read from the magic bytes instead of from the extension. Serving a PNG as
    // image/jpeg would be refused by the browser under `X-Content-Type-Options: nosniff`.
    let content_type = sniff_image_type(&bytes)
        .ok_or_else(|| ApiError::not_found("sticker_image_not_an_image"))?;
    Ok((
        StatusCode::OK,
        [
            ("Content-Type", content_type),
            // The hash is the content address, so it can never change under this URL.
            ("Cache-Control", "private, max-age=86400"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub(super) struct StickerQuery {
    /// 1 = only what she kept, 0 = unjudged cache, -1 = refused, absent = everything.
    kept: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl StickerQuery {
    fn window(&self) -> (i64, i64) {
        let limit = self.limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX);
        let offset = self.offset.unwrap_or(0).max(0);
        (limit, offset)
    }
}

/// The sticker shelf. Recognition cache and her library share the table and differ
/// only by `kept`, so the same list backs both views.
pub(super) async fn list_stickers(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<StickerQuery>,
) -> ApiResult {
    let (limit, offset) = query.window();
    let stickers = state
        .store
        .list_stickers_page(&state.bot_id, query.kept, limit, offset)
        .await
        .map_err(|_| ApiError::internal())?;
    let library = state
        .store
        .list_library(&state.bot_id, PAGE_MAX)
        .await
        .map_err(|_| ApiError::internal())?;
    let total = state
        .store
        .count_stickers(&state.bot_id, query.kept)
        .await
        .map_err(|_| ApiError::internal())?;
    ok_json(json!({
        "stickers": stickers.into_iter().map(sticker_json).collect::<Vec<_>>(),
        "total": total,
        "library_size": library.len(),
    }))
}

pub(super) fn sticker_json(sticker: crate::store::StickerRecord) -> serde_json::Value {
    json!({
        "hash": sticker.hash,
        "short": sticker.hash.chars().take(8).collect::<String>(),
        "kind": sticker.kind,
        "is_self": sticker.is_self,
        "ocr": sticker.ocr,
        "description": sticker.description,
        "scene": sticker.scene,
        "emotion": sticker.emotion,
        "intent": sticker.intent,
        "usage_hint": sticker.usage_hint,
        "phash": sticker.phash,
        "last_seen_at": sticker.last_seen_at,
        "seen_count": sticker.seen_count,
        "nsfw": sticker.nsfw,
        "enabled": sticker.enabled,
        "kept": sticker.kept,
        "group_id": sticker.group_id,
        "created_at": sticker.created_at,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct StickerPatch {
    /// 1 = her library, 0 = back to cache, -1 = refused.
    kept: Option<i64>,
    enabled: Option<bool>,
}

/// Change one sticker's shelf state from the panel. `hash` is the only identifier, so a
/// typo cannot touch a different row.
pub(super) async fn patch_sticker(
    State(state): State<Arc<AdminState>>,
    AxumPath(hash): AxumPath<String>,
    Json(body): Json<StickerPatch>,
) -> ApiResult {
    if body.kept.is_none() && body.enabled.is_none() {
        return Err(ApiError::bad_request("empty_patch"));
    }
    if let Some(kept) = body.kept {
        if !(-1..=1).contains(&kept) {
            return Err(ApiError::bad_request("bad_kept"));
        }
        let found = state
            .store
            .set_sticker_kept(&state.bot_id, &hash, kept)
            .await
            .map_err(|_| ApiError::internal())?;
        if !found {
            return Err(ApiError::not_found("unknown_sticker"));
        }
    }
    if let Some(enabled) = body.enabled {
        let found = state
            .store
            .set_sticker_enabled(&state.bot_id, &hash, enabled)
            .await
            .map_err(|_| ApiError::internal())?;
        if !found {
            return Err(ApiError::not_found("unknown_sticker"));
        }
    }
    ok_json(json!({ "ok": true }))
}

/// Drop one sticker row. The image file itself is left on disk, so a re-post of the same
/// picture is re-tagged once and then behaves like any other picture.
pub(super) async fn delete_sticker(
    State(state): State<Arc<AdminState>>,
    AxumPath(hash): AxumPath<String>,
) -> ApiResult {
    let path = state
        .store
        .delete_sticker(&state.bot_id, &hash)
        .await
        .map_err(|_| ApiError::internal())?;
    let Some(path) = path else {
        return Err(ApiError::not_found("unknown_sticker"));
    };
    // The row is the source of truth; a missing file is not an error, because the
    // operator may have cleared the directory by hand.
    let file_removed = std::fs::remove_file(&path).is_ok();
    ok_json(json!({ "ok": true, "file_removed": file_removed }))
}

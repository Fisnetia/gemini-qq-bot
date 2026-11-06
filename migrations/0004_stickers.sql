-- Sticker library: everything the preflight model tagged as kind='sticker'.
--
-- The bot never stores an original image twice: hash is the sha256 of the bytes, so a
-- reposted sticker hits the existing row and costs zero tagging tokens. Metadata lives
-- here; the bytes live in runtime/stickers/<hash>.jpg.
--
-- search_blob is a lowercased, deduplicated join of every text field so retrieval can
-- score a single column without pulling in FTS5.

CREATE TABLE IF NOT EXISTS stickers (
    hash        TEXT    PRIMARY KEY,
    bot_id      TEXT    NOT NULL,
    group_id    TEXT    NOT NULL,
    file_path   TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    is_self     TEXT    NOT NULL DEFAULT 'other',
    ocr         TEXT    NOT NULL DEFAULT '',
    scene       TEXT    NOT NULL DEFAULT '',
    emotion     TEXT    NOT NULL DEFAULT '',
    intent      TEXT    NOT NULL DEFAULT '',
    usage_hint  TEXT    NOT NULL DEFAULT '',
    search_blob TEXT    NOT NULL DEFAULT '',
    nsfw        INTEGER NOT NULL DEFAULT 0,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS stickers_group_idx
    ON stickers (bot_id, group_id, enabled);

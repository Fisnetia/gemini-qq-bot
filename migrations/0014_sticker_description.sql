-- A fourth round on the sticker shelf: one description block, and the bookkeeping
-- the dedup gates need.
--
-- `description` is the picture itself in words: what character, what expression,
-- what is happening, any visible text, the art style, and (optionally) when one
-- would send it. It replaces `scene` / `intent` / `usage_hint` as *what the tagger
-- writes*; those three stay in the table because 900+ rows already carry them and
-- nothing is being backfilled (that is a deploy-time job, not a migration's).
--
-- `phash` is a 64-bit average hash of the decoded picture, stored as a signed
-- i64 so SQLite keeps it exact. It catches the same sticker re-encoded by QQ,
-- which changes the sha256 and used to cost a second row and a second tagging
-- pass. NULL means "not computed" (rows from before this round, or a picture we
-- could not decode) and is never treated as a match.
--
-- `seen_count` / `last_seen_at` record reposts: a repeat bumps them instead of
-- inserting a row. New rows start at 1, so `seen_count > 1` means "this exact
-- picture came back".

ALTER TABLE stickers ADD COLUMN phash INTEGER;
ALTER TABLE stickers ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE stickers ADD COLUMN last_seen_at INTEGER;
ALTER TABLE stickers ADD COLUMN seen_count INTEGER NOT NULL DEFAULT 1;

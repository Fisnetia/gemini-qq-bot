-- Outbox rows can now carry a sticker instead of text.
-- `kind` is 'text' for the original behaviour and 'sticker' for an image row;
-- a sticker row stores only the content hash, and the bytes are read from the
-- sticker directory at send time. Storing the hash keeps the database small and
-- avoids duplicating the same picture across every send.

ALTER TABLE outbox ADD COLUMN kind TEXT NOT NULL DEFAULT 'text';
ALTER TABLE outbox ADD COLUMN sticker_hash TEXT;

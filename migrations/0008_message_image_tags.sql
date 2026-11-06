-- Per-message image tagging results.
--
-- Stickers get a row in `stickers` because they are library material she can send.
-- Photos and screenshots are conversation material: they must never enter the shelf,
-- but their description still has to be inspectable (including whether the subject is
-- her), so it is stored here as a JSON array next to the message that carried it.
ALTER TABLE group_messages ADD COLUMN image_tags TEXT;

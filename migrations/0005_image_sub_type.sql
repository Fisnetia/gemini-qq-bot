-- QQ's own image sub_type, kept on the raw log.
--
-- `sub_type` is 1 for an animated sticker and 0 for a normal picture, so a message
-- row can tell the two apart without asking the vision model. Nullable because rows
-- written before this migration have no value and text-only messages never have one.

ALTER TABLE group_messages ADD COLUMN sub_type TEXT;

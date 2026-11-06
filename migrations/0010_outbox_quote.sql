-- The first part of a reply may quote an earlier group message: the target's
-- OneBot message id rides on the outbox row so the send path can prepend a
-- `reply` segment. NULL means no quote (every pre-existing row).
ALTER TABLE outbox ADD COLUMN quote_message_id TEXT;

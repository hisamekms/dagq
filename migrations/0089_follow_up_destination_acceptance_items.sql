-- dagq-schema: compatible
-- The items of the destination goal's acceptance an out_of_scope judgement
-- names as needing the follow_up (ADR-t3325-1): a JSON array of text,
-- written with the judgement's row in its transaction. The runtime only
-- refuses a destination above low that names none; plan review checks them
-- against the destination's acceptance. NULL is a row of an older binary,
-- read as none. A nullable addition only: an older binary ignores it.
ALTER TABLE follow_up_judgements ADD COLUMN destination_acceptance_items TEXT;

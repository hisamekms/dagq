-- dagq-schema: compatible
-- Whose authority an answer carries and whether it is an approval
-- (ADR-t728-3 decisions 2 and 3, task 733): `answer_authority` is `user`
-- (a person at a plain terminal), `delegated` (the inbox at a person's
-- word) or `runtime` (the runtime closing an ask itself), from the type of
-- the actor that answered; `answer_approval` is 1 when the answer approves
-- or refuses what a person decides (an approval kind, or a `propose` /
-- `dismiss` applied to a finding), else 0. `answered_by` keeps its values.
-- Answers written before this migration, and by an older binary, leave
-- both NULL. An addition only: an older binary reads the table by column
-- name and never writes them.
ALTER TABLE asks ADD COLUMN answer_authority TEXT;
ALTER TABLE asks ADD COLUMN answer_approval INTEGER;

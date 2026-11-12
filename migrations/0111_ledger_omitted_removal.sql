-- dagq-schema: compatible
-- Deletion candidates: one saved omitted subject kind, the current ledger_v
-- and the public (C, S] of the record's own event id. Seek rather than walk
-- the retained prefix/suffix or other versions on every builder pass.
-- Single-event settled rows only; event id and key support keyset pages
-- even when two rows record the same event. No row or ledger_v changes.
CREATE INDEX ledger_rows_omitted_removal
  ON ledger_rows(kind, subject_kind, ledger_v, json_extract(records, '$[0].id'), key)
  WHERE final = TRUE AND json_array_length(records) = 1;

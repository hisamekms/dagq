-- dagq-schema: compatible
-- The queue rows a read from a time leaves out by their end
-- (src/infrastructure/runtime_store/ledger/queue_from.rs): a row of a kind
-- read by its range that ended before the time is still read when its start
-- was recorded at the time or after it, which happens only when a clock went
-- ahead and back (its start after its end) or recorded no time. This partial
-- index holds those rows alone, so the seek walks them and not the settled
-- rows of the kind. An index only: ledger rows and ledger_v keep their shape
-- (ADR-t1662-2 decisions 1, 7, 8); older binaries may ignore it.
CREATE INDEX ledger_rows_started_after_end ON ledger_rows(kind, subject_kind, started_at)
  WHERE ended_at IS NOT NULL AND (started_at IS NULL OR started_at > ended_at);

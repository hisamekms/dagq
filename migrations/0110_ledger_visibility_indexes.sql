-- dagq-schema: compatible
-- The two reads of the visibility of a small read of run_events
-- (src/infrastructure/runtime_store/ledger/visibility.rs; promise 7 of
-- docs/plans/ledger-window-earlier-visibility-spike.md, "追試 (iii) 可視性"):
-- each reads the rows it returns and not the settled history.
-- Indexes only: events, ledger rows and ledger_v keep their shape
-- (ADR-t1662-2 decisions 1, 7, 8); older binaries may ignore the indexes.

-- (iii-a) A run with no run row: the unfinished task rows, whose detail.runs
-- name the runs on their way. ledger_rows_by_subject_end leads with
-- subject_kind, which a task row has none of, so it walks every task row;
-- this partial index holds only the unfinished ones.
CREATE INDEX ledger_rows_open_tasks ON ledger_rows(kind)
  WHERE kind = 'task' AND ended_at IS NULL;

-- (iii-b) The rows a fold of another ledger_v wrote, which may hold fewer
-- events than W and both R promise and void them for the reader: none on a
-- store one version has caught up with, legacy rows apart. Partial, so only
-- a read that writes these terms as literals can take it, and the reads
-- that bind kind keep their plans.
CREATE INDEX ledger_rows_by_version ON ledger_rows(ledger_v)
  WHERE legacy = FALSE AND kind IN ('run', 'task', 'session', 'queue');

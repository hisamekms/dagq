-- dagq-schema: compatible
-- A task's claim waits until the supervisor's own build contains the
-- landed commits of every task it depends on (ADR-t1632-1; `add
-- --wait-for-build`, `edit --wait-for-build` / `--no-wait-for-build`).
-- 0 for a task that does not declare it, every task before this migration
-- included. No CHECK: the runtime reads any other value than 0 as declared.
-- An addition with a default only: an older binary never names the column,
-- and its inserts leave it 0.
ALTER TABLE tasks ADD COLUMN wait_for_build INTEGER NOT NULL DEFAULT 0;

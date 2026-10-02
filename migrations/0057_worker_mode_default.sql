-- dagq-schema: compatible
-- Claude's worker runs headless when a task names no mode (ADR-t1340-1,
-- amending ADR-t813-1 decision 7). From now on `add` and `edit` store
-- NULL in tasks.worker_mode when no mode is given and a value only for
-- --interactive / --headless, so the default and a named mode stay
-- apart. Before this, `add` stored the resolved mode, so every Claude task
-- registered without one holds 'interactive', the old default.
--
-- Such a task that no run has claimed yet (draft, submitted, ready) goes
-- back to NULL and follows the new default. Left as it is: a task whose
-- `task_edited` set worker_mode to 'interactive' (`edit --interactive`, or
-- a provider change back to claude, which cannot be told apart from it in
-- the payload, so kept on the side of the old route), a 'headless' task,
-- a Codex task, and a task in_progress, completed or canceled (a live
-- run's route and the record of past ones stay; a run keeps its own
-- worker_mode in task_runs). An older binary reads NULL as Claude
-- interactive, as before.
UPDATE tasks SET worker_mode = NULL
WHERE worker_provider = 'claude'
  AND worker_mode = 'interactive'
  AND status IN ('draft', 'submitted', 'ready')
  AND NOT EXISTS (
    SELECT 1 FROM run_events e
    WHERE e.task_id = tasks.id
      AND e.kind = 'task_edited'
      AND json_extract(e.payload, '$.to.worker_mode') = 'interactive'
  );

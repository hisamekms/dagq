-- dagq-schema: compatible
-- The worker a task asks for (ADR-t813-2 decision 1, ADR-t813-1 decision
-- 7): its provider (`claude` / `codex`) and mode (`interactive` /
-- `headless`), written by `add` / `edit --provider` and `--headless` /
-- `--interactive`. NULL is a task from before this migration, or one an
-- older binary inserted: Claude, interactive. Each run records the mode
-- its task asked for at the claim beside its providers (NULL:
-- interactive). The values are listed in a CHECK like a status, so adding
-- a provider or a mode later is a breaking change (ADR-0073 decision 6).
-- A supervisor records the executables of its providers as it resolved
-- them (JSON; NULL for an older binary's). Nullable additions only: an
-- older binary never names these columns.
ALTER TABLE tasks ADD COLUMN worker_provider TEXT
    CHECK (worker_provider IS NULL OR worker_provider IN ('claude', 'codex'));
ALTER TABLE tasks ADD COLUMN worker_mode TEXT
    CHECK (worker_mode IS NULL OR worker_mode IN ('interactive', 'headless'));
ALTER TABLE task_runs ADD COLUMN worker_mode TEXT
    CHECK (worker_mode IS NULL OR worker_mode IN ('interactive', 'headless'));
ALTER TABLE supervisors ADD COLUMN providers TEXT;

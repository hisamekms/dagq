-- dagq-schema: compatible
-- The spacing of new claims while the load hold is on (ADR-t1479-1): the
-- supervisor's `claim_spacing` in seconds and where it comes from (`flag`,
-- `dagq.toml` (its `[supervisor] claim_spacing`) or `default`), written with
-- `parallel`, `max_waiting` and `runtime_planners`, and its `--max-load` (NULL
-- when the load hold is off), written when it registers or takes its
-- registration back after an exec. `status` reads them for the time of the
-- next claim. NULL in the first two is a supervisor of an older binary. A
-- nullable addition only: an older binary ignores it.
ALTER TABLE supervisors ADD COLUMN claim_spacing INTEGER;
ALTER TABLE supervisors ADD COLUMN claim_spacing_source TEXT;
ALTER TABLE supervisors ADD COLUMN max_load REAL;

-- dagq-schema: compatible
-- The limit on the planners a supervisor's runtime opens at once (ADR-0041
-- decision 12) and where it comes from (task 941): `flag`, `dagq.toml`
-- (its `[supervisor] runtime_planners`) or `default`, written by the
-- supervisor with `parallel` and `max_waiting` when it registers, takes its
-- registration back after an exec, or reads `[supervisor]` again. NULL is a
-- supervisor of an older binary, whose value is not recorded. A nullable
-- addition only: an older binary ignores it.
ALTER TABLE supervisors ADD COLUMN runtime_planners INTEGER;
ALTER TABLE supervisors ADD COLUMN runtime_planners_source TEXT;

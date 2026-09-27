-- dagq-schema: compatible
-- Where a supervisor's `parallel` and `max_waiting` come from (task 698):
-- `flag`, `dagq.toml` (its `[supervisor]`) or `default`, written by the
-- supervisor with the values when it registers, takes its registration back
-- after an exec, or reads `[supervisor]` again. NULL is a supervisor of an
-- older binary, whose values came from its flags or their defaults. A
-- nullable addition only: an older binary ignores it.
ALTER TABLE supervisors ADD COLUMN parallel_source TEXT;
ALTER TABLE supervisors ADD COLUMN max_waiting_source TEXT;

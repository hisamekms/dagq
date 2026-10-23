-- dagq-schema: compatible
-- Where the supervisor's `max_load` comes from: `flag` (its `--max-load`)
-- or `default` (twice the host's logical cores), written with `max_load`.
-- `install --allow-breaking` passes a flag's value (0 when it disabled the
-- hold) to the `up` that starts the drained supervisor again. NULL is a
-- supervisor of an older binary. A nullable addition only: an older binary
-- ignores it.
ALTER TABLE supervisors ADD COLUMN max_load_source TEXT;

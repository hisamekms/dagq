-- dagq-schema: compatible
-- The execution classes' claims (ADR-t1487-2): the supervisor's
-- `max_spikes` (NULL from `default` is no limit) and `class_aging` in
-- seconds, each with where it comes from (`dagq.toml`, its `[supervisor]`,
-- or `default`), written with `parallel` and the other slot limits. `status`
-- reads them. NULL in the sources is a supervisor of an older binary. A
-- nullable addition only: an older binary ignores it.
ALTER TABLE supervisors ADD COLUMN max_spikes INTEGER;
ALTER TABLE supervisors ADD COLUMN max_spikes_source TEXT;
ALTER TABLE supervisors ADD COLUMN class_aging INTEGER;
ALTER TABLE supervisors ADD COLUMN class_aging_source TEXT;

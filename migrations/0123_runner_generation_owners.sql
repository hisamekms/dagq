-- dagq-schema: compatible
-- The holder of a granted generation is immutable authorization evidence.
-- NULL means an old generation's holder was never recorded: fail closed,
-- never infer ownership from an accepted head or another generation.
-- The current lease supplies its own holder; grant preserves that evidence
-- in this column before advancing the generation, without a data backfill.
ALTER TABLE runner_lease_records ADD COLUMN runner_id TEXT;

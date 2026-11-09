-- A run's lease on a runner of the team's version, one row per run for its
-- current generation; the local default deployment writes no row here. The
-- generation, end and state are what every report is judged against
-- (src/domain/runner_lease.rs), and every change is an update conditioned on
-- the generation read in the same write leader's transaction.
CREATE TABLE runner_leases (
    run_id TEXT PRIMARY KEY NOT NULL,
    generation BIGINT NOT NULL,
    runner_id TEXT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    state TEXT NOT NULL
);
-- What the control side recorded per run and generation: the newest head it
-- accepted from that generation's reports, and the head the generation's ref
-- was read back at from GitHub. No foreign keys, as in the runner tables.
CREATE TABLE runner_lease_records (
    run_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    accepted_head TEXT,
    accepted_at_ms BIGINT,
    verified_ref_head TEXT,
    verified_at_ms BIGINT,
    PRIMARY KEY (run_id, generation)
);

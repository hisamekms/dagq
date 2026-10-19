-- dagq-schema: compatible
-- The owners and the resource samples of the sessions (ADR-t1662-3
-- decisions 1, 4, 9 and 10, SessionStepStore).
--
-- session_owners: the one target the control side registered a session
-- to before its executor reports (target_kind and target_id as in
-- session_steps; registered_at in unix ms). The queue service records a
-- session's steps and samples only for the telemetry principal of that
-- target, so no token takes another target's session first.
--
-- session_resources: what an executor measured of its session's
-- resources from inside, where the supervisor cannot measure from
-- outside, one row per `(session_id, seq)` kept once as the steps are.
-- offset_ms and duration_ms place the window from the session's start;
-- the values are in the common units (CPU and throttled time in seconds,
-- the CPU limit in cores, memory, reads and writes in bytes, the CPU
-- pressure a percentage), null where the environment let it measure none.
-- received_at is when it came in (unix ms).
--
-- The retention removes the samples that came in more than 90 days before
-- with the steps, and the owners registered before that whose session has
-- no step and no sample left.
-- Additions only: an older binary ignores the tables.
CREATE TABLE session_owners (
    session_id TEXT PRIMARY KEY,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    registered_at INTEGER NOT NULL
);

CREATE TABLE session_resources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    offset_ms INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    cpu_seconds REAL,
    cpu_limit REAL,
    throttled_seconds REAL,
    memory_max_bytes INTEGER,
    memory_limit_bytes INTEGER,
    read_bytes INTEGER,
    write_bytes INTEGER,
    cpu_pressure REAL,
    load_average REAL,
    node_id TEXT,
    received_at INTEGER NOT NULL,
    UNIQUE (session_id, seq)
);

CREATE INDEX session_resources_target ON session_resources(target_kind, target_id);

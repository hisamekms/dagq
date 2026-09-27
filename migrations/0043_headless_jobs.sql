-- dagq-schema: compatible
-- The processes of the supervisor's headless jobs (task 443): the reviews,
-- the recovery jobs, the plan reviews and the goal reviews.
--
-- headless_jobs: one row per job, written when its process starts with
-- the pid and the process's start as `ps` reads it (`process_start`), so a
-- pid used again by another process is told apart. `ended_at` / `outcome`
-- are set once the job ended, was stopped by its supervisor, or was taken
-- over. A supervisor that finds an unfinished row of a supervisor gone
-- (not registered, or its heartbeat stale) stops the process when it is
-- still the job's, with its descendants, and records
-- `headless_job_stopped`. `kind` names the job (review, recovery,
-- plan_review, goal_review), `label` what else tells it (the recovery
-- job's alert), and `run_id` / `proposal_id` / `goal_id` its subject.
-- Additions only, and no foreign key: an older binary never reads this
-- table, and deletes nothing it would refer to.
CREATE TABLE headless_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (length(kind) > 0),
    label TEXT,
    run_id TEXT,
    proposal_id INTEGER,
    goal_id INTEGER,
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    pid INTEGER NOT NULL,
    process_start TEXT,
    supervisor_token TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    outcome TEXT CHECK (outcome IS NULL OR length(outcome) > 0),
    CHECK ((ended_at IS NULL) = (outcome IS NULL))
);
CREATE INDEX headless_jobs_unfinished ON headless_jobs(id) WHERE ended_at IS NULL;

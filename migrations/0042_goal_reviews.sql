-- dagq-schema: compatible
-- Goal review (ADR-0047 decision 43). The supervisor reviews one open goal
-- whose tasks all ended at a time, queue-wide, with a headless job.
--
-- goal_reviews: one row per job. A supervisor starts one only inside
-- BEGIN IMMEDIATE after finding no unfinished row of a live supervisor, so
-- two supervisors never review at once; a row whose supervisor is gone is
-- finished as `interrupted` by the next one.
-- `fingerprint` is the state of the goal's tasks the job saw (each task's
-- ID and status): the next review of the goal starts only once it
-- differs, or once a person rearms the goal (`goal review ID` sets
-- `rearmed_at` on its latest row). `dir` is the job's directory under the
-- queue's goal-reviews/ (its prompt and output), `verdict` the JSON verdict
-- it printed, `error` why it failed, `ask_id` the approve_goal ask it
-- opened. Additions only, and no foreign key: an older binary never reads
-- this table, and deletes nothing it would refer to.
CREATE TABLE goal_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    goal_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    supervisor_token TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    dir TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT CHECK (outcome IS NULL OR length(outcome) > 0),
    verdict TEXT CHECK (verdict IS NULL OR json_valid(verdict)),
    error TEXT,
    ask_id INTEGER,
    rearmed_at INTEGER,
    CHECK ((finished_at IS NULL) = (outcome IS NULL))
);
CREATE INDEX goal_reviews_by_goal ON goal_reviews(goal_id, id);

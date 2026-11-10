-- The landing marks of a goal, one row per goal and mark; same contract as
-- SQLite's goal_landing_marks migration.
CREATE TABLE goal_landing_marks (
    goal_id BIGINT NOT NULL,
    mark TEXT NOT NULL,
    PRIMARY KEY (goal_id, mark)
);

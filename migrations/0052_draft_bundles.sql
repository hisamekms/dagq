-- dagq-schema: compatible
-- Bundles of drafts (ADR-t807-1): one planner of the runtime's takes the
-- drafts the same piece of work made that wait at once (the follow_ups of
-- one run's receipt, the gaps of one goal review, the tasks one withdrawal
-- returned), instead of one planner per draft.
--
-- draft_bundles: the bundle a planner of the runtime's was opened for, one
-- row per such planner: the drafts' origin and the key that made them one
-- bundle (`key_kind` is `source_run_id`, `goal_review_id`,
-- `reviewed_proposal_id`, or `task_id` for a draft whose material names
-- none, with its value as text). `planners.draft_task_id` keeps the
-- bundle's oldest draft.
--
-- draft_bundle_members: the drafts of the bundle, each with its attempt
-- (which planner of the runtime's this is for that draft) and, once the
-- planner ended, what became of it (`outcome`: submitted, canceled,
-- duplicate, keep_draft, undecided; with the proposal it was submitted
-- into or the task it duplicates). A planner opened before this has no
-- rows. Additions only: an older binary ignores both tables.
CREATE TABLE draft_bundles (
    planner_id INTEGER PRIMARY KEY,
    origin TEXT NOT NULL,
    key_kind TEXT NOT NULL,
    key_value TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX draft_bundles_by_key ON draft_bundles(key_kind, key_value);
CREATE TABLE draft_bundle_members (
    planner_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    outcome TEXT,
    proposal_id INTEGER,
    duplicate_of INTEGER,
    settled_at INTEGER,
    PRIMARY KEY (planner_id, task_id)
);
CREATE INDEX draft_bundle_members_by_task ON draft_bundle_members(task_id);

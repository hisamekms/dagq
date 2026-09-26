-- dagq-schema: compatible
-- Drafts of origin `reopened` (task 418): a ready task plan review
-- reopened into a proposal of its own (ADR-0044 decision 14) that returns
-- to `draft` when that proposal is withdrawn. The runtime opens a planner
-- for it like for a draft in `draft_origins`, whose CHECK names only
-- `follow_up` and `goal_gap`; this table keeps the origin instead, so that
-- an older binary, which does not know the origin, never reads it and
-- only leaves such a draft for a person. `material` is what the planner
-- is shown: the reopen's reason, the withdrawn proposal and the proposal
-- whose plan review reopened the task. A later withdrawal of a reopened
-- task replaces its row. A row here takes precedence over the task's
-- `draft_origins` row as the origin a planner is shown; `draft_origins`
-- keeps the origin the task was registered with. Additions only.
CREATE TABLE draft_reopens (
    task_id INTEGER PRIMARY KEY,
    material TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(material)),
    created_at INTEGER NOT NULL
);

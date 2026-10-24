-- dagq-schema: compatible
-- The replacement plans of replanning operations (ADR-t1521-2 decision 4).
--
-- replan_plans: one row per plan a planner of the operation's request
-- submitted with its proposal (`submit --replacement`), `revision` 1 and
-- up per operation. `plan` is the JSON of
-- domain::replan_plan::ReplacementPlan and `hash` its SHA-256; plan review
-- checks the latest revision, and `approved_at` is when its pass (or a
-- person's ready answer) found it applicable, which leaves the children
-- `submitted` until the trusted runtime applies the replacement.
--
-- Additions only: an older binary ignores the table.
CREATE TABLE replan_plans (
    operation_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    proposal_id INTEGER NOT NULL,
    plan TEXT NOT NULL,
    hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    approved_at INTEGER,
    PRIMARY KEY (operation_id, revision)
);
CREATE INDEX replan_plans_by_proposal ON replan_plans(proposal_id);

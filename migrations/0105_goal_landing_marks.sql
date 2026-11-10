-- dagq-schema: compatible
-- The landing marks of a goal (ADR-t3988-1 decision 4), the names a main's
-- `[landing.approval] goal_marks` condition looks for: one row per goal and
-- mark, a table apart from goals.tags, which a planner's `goal edit`
-- replaces, so that only `goal.landing_mark` (a goal's owner or an
-- administrator, the host's user in the local default) puts one on or
-- takes one off. Each change is written with its goal_landing_marks_changed
-- event, its actor and the marks before and after, in one transaction.
-- `mark` is a name that is not blank (domain::landing_approval::GoalMark),
-- checked by the domain as no CHECK constraint is used. No foreign key to
-- goals: it would fail an older binary's delete of a goal with marks.
-- Additions only: an older binary ignores the table, and no local goal
-- carries a mark.
CREATE TABLE goal_landing_marks (
    goal_id INTEGER NOT NULL,
    mark TEXT NOT NULL,
    PRIMARY KEY (goal_id, mark)
);

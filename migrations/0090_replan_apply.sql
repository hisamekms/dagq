-- dagq-schema: breaking
-- The atomic apply of a replacement (ADR-t1521-2 decision 5): the trusted
-- runtime ends the replaced task as `replaced` and its parked run as
-- `replaced`, statuses an older binary cannot read (a task or a run with a
-- status outside its enum fails the read), so the queue refuses every
-- such binary at open.
--
-- replan_replacements.carry: what the child takes of the saved work, as
-- the applied plan said (`base`, `snapshot_head`, `snapshot_diff`, the
-- values of domain::replan_plan::CarrySource), and carry_order its place
-- among the children that take saved work. The child's claim hands it
-- over from the operation's snapshot. NULL is read as `base`.
ALTER TABLE replan_replacements ADD COLUMN carry TEXT;
ALTER TABLE replan_replacements ADD COLUMN carry_order INTEGER;
--
-- replan_plans.approved_children: the version of each child's conditions
-- (domain::task_replan::ChildConditions, read by
-- infrastructure::replans::child_version) when plan review approved the
-- plan, a JSON object of task ID to version. The apply
-- refuses a plan whose child changed since; NULL (a plan approved before
-- this column) is refused alike.
ALTER TABLE replan_plans ADD COLUMN approved_children TEXT;

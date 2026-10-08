-- dagq-schema: compatible
-- When the runtime asked a planner of the runtime's to exit because only a
-- person's answer to its `planner_question` was left (ADR-t1704-1
-- decision 1), with `planner_answer_wait` on the queue. Null for every
-- other planner and for every planner before this migration. Such a
-- planner gets no answer any more (the answer goes to a new planner once
-- its row is closed) and counts to no limit of planners that ended
-- undecided (decision 5). An addition only: an older binary never names
-- the column.
ALTER TABLE planners ADD COLUMN answer_wait_at INTEGER;

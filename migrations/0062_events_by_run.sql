-- dagq-schema: compatible
-- The events of a run are found by its run_id (goal 103). With only
-- events_by_task, events_by_goal, events_by_kind and events_by_opened_event,
-- `WHERE run_id=?1` walked every row of run_events, and the supervisor's
-- round reads a run's events for each session it watches. The id keeps
-- their order, as the other indexes on run_events do.
CREATE INDEX events_by_run ON run_events(run_id, id);

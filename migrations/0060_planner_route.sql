-- dagq-schema: compatible
-- The route a planner's agent runs on (ADR-t1394-2 decision 1): 'headless'
-- for a planner of the runtime's opened under `[roles.runtime_planner]
-- route = "headless"`, whose agent is one non-interactive call per turn.
-- NULL is interactive, as every planner opened before this: a nullable
-- addition only, which an older binary never names.
ALTER TABLE planners ADD COLUMN route TEXT;

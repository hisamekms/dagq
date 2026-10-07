-- dagq-schema: breaking
-- A request's priority, the origin of goals and tasks, and who set a
-- priority (ADR-t1975-1 decisions 1, 2, 5 and 6).
--
-- plan_requests.priority: the priority the person's words gave the request
-- (`request add --priority`, low=0 … interrupt=4); null without one and for
-- every request before this migration.
--
-- goals/tasks.origin, origin_kind, origin_request_id: where the row comes
-- from, recorded at its creation and never rewritten (`domain::plan_request`
-- Origin, OriginKind, RecordedOrigin). goals.priority_by and
-- tasks.priority_by: who set the goal's priority and the task's own
-- (`human`, `ai`; PriorityBy). A task's is null without an own priority;
-- null beside one (an older binary's insert) reads as `human`.
--
-- Existing rows (decision 6) are filled from the records only, never from
-- the words of a task or goal: the actor of the creation event (the user
-- or the inbox is a person's; a planner by its row: its request, a
-- person's planner, its finding, its draft's origin, the request the
-- proposal it revised is linked to, else `planner`; any other actor the
-- follow_up or goal gap it registered, else `runtime`), then for a row
-- without one, or a planner without a row (who opened it is unknown), the
-- request its proposal is linked to, a follow_up's or goal gap's origin,
-- the finding its proposal remedies; a row none of them decides is
-- `unknown`. A later reopen is not read as where a row came from. A priority is the
-- AI's only when the row is the AI's and the actor that set it last (the
-- latest change, else the creation) is recorded and is neither the user
-- nor the inbox; any other is `human`, so plan review keeps it. No value
-- or membership changes. Breaking: it fills the existing rows, which a
-- compatible migration may not, and an older binary would create goals and
-- tasks without recording their origin.
ALTER TABLE plan_requests ADD COLUMN priority INTEGER;
ALTER TABLE goals ADD COLUMN origin TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE goals ADD COLUMN origin_kind TEXT;
ALTER TABLE goals ADD COLUMN origin_request_id INTEGER;
ALTER TABLE goals ADD COLUMN priority_by TEXT NOT NULL DEFAULT 'human';
ALTER TABLE tasks ADD COLUMN origin TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE tasks ADD COLUMN origin_kind TEXT;
ALTER TABLE tasks ADD COLUMN origin_request_id INTEGER;
ALTER TABLE tasks ADD COLUMN priority_by TEXT;

-- What each row's records say of who created it.
CREATE TEMP TABLE created_v70 AS
SELECT 'task' AS what, t.id AS id, t.proposal_id AS proposal_id,
       e.actor_role AS role,
       CASE WHEN e.actor_id LIKE 'planner:%' THEN CAST(substr(e.actor_id, 9) AS INTEGER) END
           AS planner_id,
       (SELECT o.origin FROM draft_origins o WHERE o.task_id = t.id) AS draft_origin
FROM tasks t
LEFT JOIN run_events e ON e.id = (SELECT min(x.id) FROM run_events x
    WHERE x.task_id = t.id AND x.kind = 'task_created')
UNION ALL
SELECT 'goal', g.id, g.proposal_id, e.actor_role,
       CASE WHEN e.actor_id LIKE 'planner:%' THEN CAST(substr(e.actor_id, 9) AS INTEGER) END,
       NULL
FROM goals g
LEFT JOIN run_events e ON e.id = (SELECT min(x.id) FROM run_events x
    WHERE x.goal_id = g.id AND x.kind = 'goal_created');

CREATE TEMP TABLE planned_v70 AS
SELECT c.*, p.id AS planner_row, p.origin AS planner_origin, p.request_id AS planner_request,
       p.finding_id AS planner_finding,
       coalesce(p.draft_task_id, (SELECT min(m.task_id) FROM draft_bundle_members m
           WHERE m.planner_id = p.id)) AS planner_draft,
       (SELECT min(l.request_id) FROM plan_request_proposals l
           WHERE l.proposal_id = p.proposal_id) AS revised_request,
       (SELECT min(l.request_id) FROM plan_request_proposals l
           WHERE l.proposal_id = c.proposal_id) AS linked_request,
       EXISTS(SELECT 1 FROM findings f WHERE f.proposal_id = c.proposal_id) AS remedies
FROM created_v70 c LEFT JOIN planners p ON p.id = c.planner_id;

CREATE TEMP TABLE origin_v70 AS
SELECT what, id,
  CASE
    WHEN role IN ('user', 'inbox') THEN 'person'
    WHEN role = 'planner' THEN CASE
        WHEN planner_row IS NULL THEN CASE WHEN linked_request IS NOT NULL THEN 'request' END
        WHEN planner_request IS NOT NULL THEN 'request'
        WHEN planner_origin = 'person' THEN 'person'
        WHEN planner_finding IS NOT NULL THEN 'finding'
        WHEN planner_draft IS NOT NULL THEN
            CASE WHEN EXISTS(SELECT 1 FROM draft_reopens r WHERE r.task_id = planner_draft)
                     THEN 'reopened'
                 ELSE CASE (SELECT o.origin FROM draft_origins o WHERE o.task_id = planner_draft)
                     WHEN 'follow_up' THEN 'follow_up' WHEN 'goal_gap' THEN 'goal_gap'
                     WHEN 'reopened' THEN 'reopened' ELSE 'draft' END END
        WHEN revised_request IS NOT NULL THEN 'request'
        WHEN linked_request IS NOT NULL THEN 'request'
        ELSE 'planner' END
    WHEN role IS NOT NULL THEN CASE draft_origin
        WHEN 'follow_up' THEN 'follow_up' WHEN 'goal_gap' THEN 'goal_gap' ELSE 'runtime' END
    WHEN linked_request IS NOT NULL THEN 'request'
    WHEN draft_origin IN ('follow_up', 'goal_gap') THEN draft_origin
    WHEN remedies THEN 'finding'
  END AS kind,
  CASE
    WHEN role = 'planner' AND planner_row IS NULL THEN linked_request
    WHEN role = 'planner' AND planner_request IS NOT NULL THEN planner_request
    WHEN role = 'planner' AND planner_origin IS NOT 'person' AND planner_finding IS NULL
        AND planner_draft IS NULL THEN coalesce(revised_request, linked_request)
    WHEN role IS NULL THEN linked_request
  END AS request_id
FROM planned_v70;

UPDATE goals SET
  origin_kind = (SELECT o.kind FROM origin_v70 o WHERE o.what = 'goal' AND o.id = goals.id),
  origin_request_id = (SELECT o.request_id FROM origin_v70 o
      WHERE o.what = 'goal' AND o.id = goals.id AND o.kind = 'request');
UPDATE tasks SET
  origin_kind = (SELECT o.kind FROM origin_v70 o WHERE o.what = 'task' AND o.id = tasks.id),
  origin_request_id = (SELECT o.request_id FROM origin_v70 o
      WHERE o.what = 'task' AND o.id = tasks.id AND o.kind = 'request');
UPDATE goals SET origin = CASE
    WHEN origin_kind IN ('request', 'person') THEN 'human'
    WHEN origin_kind IS NOT NULL THEN 'ai' ELSE 'unknown' END;
UPDATE tasks SET origin = CASE
    WHEN origin_kind IN ('request', 'person') THEN 'human'
    WHEN origin_kind IS NOT NULL THEN 'ai' ELSE 'unknown' END;

-- Who set each priority last: a goal's latest event that changed it (its
-- creation with a priority, or an edit whose old and new differ), a task's
-- latest `task_priority_changed`, else its creation.
UPDATE goals SET priority_by = CASE
    WHEN origin = 'ai' AND (SELECT e.actor_role FROM run_events e
        WHERE e.goal_id = goals.id AND (
            (e.kind = 'goal_created' AND json_extract(e.payload, '$.goal.priority') IS NOT NULL)
            OR (e.kind = 'goal_updated' AND json_extract(e.payload, '$.old.priority')
                IS NOT json_extract(e.payload, '$.new.priority')))
        ORDER BY e.id DESC LIMIT 1) NOT IN ('user', 'inbox')
    THEN 'ai' ELSE 'human' END;
UPDATE tasks SET priority_by = CASE
    WHEN origin = 'ai' AND coalesce(
        (SELECT e.actor_role FROM run_events e
            WHERE e.task_id = tasks.id AND e.kind = 'task_priority_changed'
            ORDER BY e.id DESC LIMIT 1),
        (SELECT e.actor_role FROM run_events e
            WHERE e.task_id = tasks.id AND e.kind = 'task_created'
            ORDER BY e.id LIMIT 1)) NOT IN ('user', 'inbox')
    THEN 'ai' ELSE 'human' END
WHERE priority IS NOT NULL;

DROP TABLE origin_v70;
DROP TABLE planned_v70;
DROP TABLE created_v70;

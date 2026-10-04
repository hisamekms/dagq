-- dagq-schema: breaking
-- ADR-t1504-2: immutable registration facts and append-only judgements.
ALTER TABLE goals ADD COLUMN acceptance_version INTEGER NOT NULL DEFAULT 1;
CREATE TRIGGER goal_acceptance_version AFTER UPDATE OF acceptance ON goals
WHEN old.acceptance IS NOT new.acceptance BEGIN
 UPDATE goals SET acceptance_version=old.acceptance_version+1 WHERE id=new.id;
END;
CREATE TABLE follow_up_judgements (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 task_id INTEGER NOT NULL REFERENCES tasks(id),
 source_goal_id INTEGER NOT NULL REFERENCES goals(id),
 source_kind TEXT NOT NULL,
 classification TEXT NOT NULL,
 acceptance_items TEXT NOT NULL,
 reason TEXT NOT NULL,
 evidence TEXT NOT NULL,
 destination_goal_id INTEGER REFERENCES goals(id),
 acceptance_version INTEGER NOT NULL,
 corrects INTEGER REFERENCES follow_up_judgements(id),
 actor_role TEXT NOT NULL,
 created_at TEXT NOT NULL
);
CREATE INDEX judgements_by_task ON follow_up_judgements(task_id,id);
CREATE INDEX judgements_by_source ON follow_up_judgements(source_goal_id,task_id,id);
-- Rewind BOTH the source and draft's membership histories. Validate every
-- link against its successor (including the current task row). Missing or
-- conflicting evidence is unknown, never inferred from today's membership.
CREATE TEMP TABLE restored_follow_up AS
WITH RECURSIVE
registrations AS (
 SELECT o.task_id, json_extract(o.material,'$.source_task_id') source_task,
        e.id event_id,e.created_at registered_at,e.payload,
        count(*) OVER(PARTITION BY o.task_id) registrations
 FROM draft_origins o JOIN run_events e
 ON e.kind='follow_up_registered' AND json_extract(e.payload,'$.task_id')=o.task_id
 AND e.task_id=json_extract(o.material,'$.source_task_id')
 AND e.run_id=json_extract(o.material,'$.source_run_id')
 AND json_extract(e.payload,'$.index')=json_extract(o.material,'$.index')
 WHERE o.origin='follow_up'
),
rewind(draft_id,which,task_id,event_id,cursor,goal_id,valid) AS (
 SELECT r.task_id,'source',t.id,r.event_id,9223372036854775807,t.goal_id,1
 FROM registrations r JOIN tasks t ON t.id=r.source_task WHERE r.registrations=1
 UNION ALL
 SELECT r.task_id,'draft',t.id,r.event_id,9223372036854775807,t.goal_id,1
 FROM registrations r JOIN tasks t ON t.id=r.task_id WHERE r.registrations=1
 UNION ALL
 SELECT w.draft_id,w.which,w.task_id,w.event_id,e.id,json_extract(e.payload,'$.from'),
        w.valid AND json_type(e.payload,'$.from') IN ('integer','null')
        AND json_type(e.payload,'$.to') IN ('integer','null')
        AND json_extract(e.payload,'$.to') IS w.goal_id
 FROM rewind w JOIN run_events e ON e.id=(
  SELECT max(id) FROM run_events WHERE task_id=w.task_id AND kind='task_goal_changed'
  AND id>w.event_id AND id<w.cursor)
),
original AS (
 SELECT * FROM rewind w WHERE NOT EXISTS(
 SELECT 1 FROM run_events WHERE task_id=w.task_id AND kind='task_goal_changed'
 AND id>w.event_id AND id<w.cursor)
)
SELECT r.task_id,s.goal_id,
 CASE WHEN s.goal_id IS NULL THEN 'none' WHEN g.closed_at IS NULL OR julianday(g.closed_at)>julianday(r.registered_at) THEN 'open' ELSE 'closed' END registration_state,
 r.event_id,r.registered_at,
 s.valid AND d.valid AND julianday(r.registered_at) IS NOT NULL
 AND (s.goal_id IS NULL OR g.id IS NOT NULL)
 AND (g.closed_at IS NULL OR julianday(g.closed_at) IS NOT NULL)
 AND coalesce(json_extract(r.payload,'$.goal_closed'),0)=
     CASE WHEN g.closed_at IS NOT NULL AND julianday(g.closed_at)<=julianday(r.registered_at) THEN 1 ELSE 0 END
 AND d.goal_id IS CASE WHEN g.closed_at IS NOT NULL AND julianday(g.closed_at)<=julianday(r.registered_at) THEN NULL ELSE s.goal_id END valid
FROM registrations r JOIN original s ON s.draft_id=r.task_id AND s.which='source'
 JOIN original d ON d.draft_id=r.task_id AND d.which='draft'
 LEFT JOIN goals g ON g.id=s.goal_id WHERE r.registrations=1;
UPDATE draft_origins SET material=json_set(material,
 '$.source_goal_id',NULL,'$.source_goal_state','unknown','$.source_goal_provenance','unknown',
 '$.source_goal_recovery_reason','missing_or_ambiguous_registration')
WHERE origin='follow_up';
UPDATE draft_origins SET material=json_set(material,
 '$.source_goal_recovery_reason','inconsistent_history',
 '$.registration_event_id',(SELECT event_id FROM restored_follow_up r WHERE r.task_id=draft_origins.task_id),
 '$.registered_at',(SELECT registered_at FROM restored_follow_up r WHERE r.task_id=draft_origins.task_id))
WHERE origin='follow_up' AND task_id IN (SELECT task_id FROM restored_follow_up WHERE valid IS NOT 1);
UPDATE draft_origins SET material=json_set(material,
 '$.source_goal_id',(SELECT goal_id FROM restored_follow_up r WHERE r.task_id=draft_origins.task_id),
 '$.source_goal_state',(SELECT registration_state FROM restored_follow_up r WHERE r.task_id=draft_origins.task_id),
 '$.source_goal_provenance','restored', '$.source_goal_recovery_reason',NULL,
 '$.registration_event_id',(SELECT event_id FROM restored_follow_up r WHERE r.task_id=draft_origins.task_id),
 '$.registered_at',(SELECT registered_at FROM restored_follow_up r WHERE r.task_id=draft_origins.task_id))
WHERE origin='follow_up' AND task_id IN (SELECT task_id FROM restored_follow_up WHERE valid=1);
DROP TABLE restored_follow_up;
CREATE TRIGGER judgement_no_update BEFORE UPDATE ON follow_up_judgements BEGIN
 SELECT RAISE(ABORT,'follow_up judgements are append-only');
END;
CREATE TRIGGER judgement_no_delete BEFORE DELETE ON follow_up_judgements BEGIN
 SELECT RAISE(ABORT,'follow_up judgements are append-only');
END;

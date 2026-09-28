-- dagq-schema: breaking
-- No CHECK constraints (ADR-t876-1): every table that still had a CHECK
-- loses all of them, so adding a value to a column no longer needs a table
-- rebuild. The rules those CHECKs held are kept by the domain types and the
-- write port (task 877; the table in docs/design/persistence.md), and a row
-- outside them fails its read, except a kind column. NOT NULL, UNIQUE, the
-- primary keys, the foreign keys and DEFAULT stay as they were. SQLite
-- cannot drop a CHECK, so each table is rebuilt like 0029, 0036, 0039 and
-- 0049: a new table without CHECKs, every row copied with its id (and the
-- rowid of a table without an integer key), the AUTOINCREMENT sequence
-- carried over, the old table dropped and the new one renamed, and its
-- indexes created again. The search triggers are dropped first and created
-- again last, since they name the tables being replaced. The migration
-- runner disables foreign keys around this script and runs
-- foreign_key_check before committing. From here on a migration writes no
-- CHECK; scripts/check-migration-numbers.sh refuses one after this file.
-- Breaking, once: every table is rebuilt, and an older binary relies on
-- the CHECKs this drops.
DROP TRIGGER search_task_inserted;
DROP TRIGGER search_task_updated;
DROP TRIGGER search_task_deleted;
DROP TRIGGER search_goal_inserted;
DROP TRIGGER search_goal_updated;
DROP TRIGGER search_goal_deleted;
DROP TRIGGER search_commit_inserted;
DROP TRIGGER search_commit_updated;
DROP TRIGGER search_note_inserted;
DROP TRIGGER search_note_deleted;
DROP TRIGGER search_run_integrated;
DROP TRIGGER search_task_moved;

-- task_dependencies
CREATE TABLE task_dependencies_v50 (
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    predecessor_id INTEGER NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, predecessor_id)
);
INSERT INTO task_dependencies_v50 (rowid, task_id, predecessor_id)
SELECT rowid, task_id, predecessor_id FROM task_dependencies;
DROP TABLE task_dependencies;
ALTER TABLE task_dependencies_v50 RENAME TO task_dependencies;
CREATE INDEX dependencies_by_predecessor ON task_dependencies(predecessor_id);

-- queue_repository
CREATE TABLE queue_repository_v50 (
    singleton INTEGER PRIMARY KEY,
    git_common_dir TEXT NOT NULL
);
INSERT INTO queue_repository_v50 (singleton, git_common_dir)
SELECT singleton, git_common_dir FROM queue_repository;
DROP TABLE queue_repository;
ALTER TABLE queue_repository_v50 RENAME TO queue_repository;

-- run_processes
CREATE TABLE run_processes_v50 (
    run_id TEXT NOT NULL REFERENCES task_runs(id),
    role TEXT NOT NULL,
    pid INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL DEFAULT (unixepoch()),
    exited_at INTEGER,
    exit_code INTEGER,
    PRIMARY KEY (run_id, role)
);
INSERT INTO run_processes_v50 (rowid, run_id, role, pid, heartbeat_at, exited_at, exit_code)
SELECT rowid, run_id, role, pid, heartbeat_at, exited_at, exit_code FROM run_processes;
DROP TABLE run_processes;
ALTER TABLE run_processes_v50 RENAME TO run_processes;

-- supervisors
CREATE TABLE supervisors_v50 (
    token TEXT PRIMARY KEY NOT NULL,
    pid INTEGER NOT NULL,
    parallel INTEGER NOT NULL,
    started_at INTEGER NOT NULL DEFAULT (unixepoch()),
    heartbeat_at INTEGER NOT NULL DEFAULT (unixepoch()),
    mode TEXT,
    workspace_id TEXT,
    binary_version TEXT,
    handoff_accepted INTEGER,
    handoff_binary TEXT,
    handoff_requested_at INTEGER,
    auto_update INTEGER,
    max_waiting INTEGER,
    parallel_source TEXT,
    max_waiting_source TEXT,
    providers TEXT
);
INSERT INTO supervisors_v50 (rowid, token, pid, parallel, started_at, heartbeat_at, mode, workspace_id, binary_version, handoff_accepted, handoff_binary, handoff_requested_at, auto_update, max_waiting, parallel_source, max_waiting_source, providers)
SELECT rowid, token, pid, parallel, started_at, heartbeat_at, mode, workspace_id, binary_version, handoff_accepted, handoff_binary, handoff_requested_at, auto_update, max_waiting, parallel_source, max_waiting_source, providers FROM supervisors;
DROP TABLE supervisors;
ALTER TABLE supervisors_v50 RENAME TO supervisors;

-- goals
CREATE TABLE goals_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    acceptance TEXT NOT NULL DEFAULT '',
    constraints TEXT NOT NULL DEFAULT '',
    doc TEXT,
    closed_at TEXT,
    verdict TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    status TEXT NOT NULL DEFAULT 'open',
    proposal_id INTEGER REFERENCES proposals(id)
);
INSERT INTO goals_v50 (id, title, description, acceptance, constraints, doc, closed_at, verdict, created_at, updated_at, status, proposal_id)
SELECT id, title, description, acceptance, constraints, doc, closed_at, verdict, created_at, updated_at, status, proposal_id FROM goals;
DELETE FROM sqlite_sequence WHERE name = 'goals_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'goals_v50', seq FROM sqlite_sequence WHERE name = 'goals';
DROP TABLE goals;
ALTER TABLE goals_v50 RENAME TO goals;

-- proposals
CREATE TABLE proposals_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    status TEXT NOT NULL,
    owner_origin TEXT NOT NULL,
    owner_workspace_id TEXT,
    submitted_at TEXT NOT NULL,
    revise_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    review_hold TEXT,
    revise_reasons TEXT,
    revised_at INTEGER,
    revise_sent_at INTEGER,
    revise_planner_id INTEGER,
    unresponsive_at INTEGER,
    owner_actor_id TEXT
);
INSERT INTO proposals_v50 (id, status, owner_origin, owner_workspace_id, submitted_at, revise_count, created_at, updated_at, review_hold, revise_reasons, revised_at, revise_sent_at, revise_planner_id, unresponsive_at, owner_actor_id)
SELECT id, status, owner_origin, owner_workspace_id, submitted_at, revise_count, created_at, updated_at, review_hold, revise_reasons, revised_at, revise_sent_at, revise_planner_id, unresponsive_at, owner_actor_id FROM proposals;
DELETE FROM sqlite_sequence WHERE name = 'proposals_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'proposals_v50', seq FROM sqlite_sequence WHERE name = 'proposals';
DROP TABLE proposals;
ALTER TABLE proposals_v50 RENAME TO proposals;
CREATE INDEX proposals_by_status ON proposals(status, submitted_at);

-- tasks
CREATE TABLE tasks_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    acceptance TEXT NOT NULL,
    verification_commands TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    goal_id INTEGER REFERENCES goals(id),
    context TEXT NOT NULL DEFAULT '',
    required_evidence TEXT NOT NULL DEFAULT '[]',
    paths TEXT NOT NULL DEFAULT '[]',
    priority INTEGER NOT NULL DEFAULT 1,
    proposal_id INTEGER REFERENCES proposals(id),
    follow_up_depth INTEGER NOT NULL DEFAULT 0,
    kind TEXT,
    worker_provider TEXT,
    worker_mode TEXT
);
INSERT INTO tasks_v50 (id, title, description, acceptance, verification_commands, status, created_at, updated_at, goal_id, context, required_evidence, paths, priority, proposal_id, follow_up_depth, kind, worker_provider, worker_mode)
SELECT id, title, description, acceptance, verification_commands, status, created_at, updated_at, goal_id, context, required_evidence, paths, priority, proposal_id, follow_up_depth, kind, worker_provider, worker_mode FROM tasks;
DELETE FROM sqlite_sequence WHERE name = 'tasks_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'tasks_v50', seq FROM sqlite_sequence WHERE name = 'tasks';
DROP TABLE tasks;
ALTER TABLE tasks_v50 RENAME TO tasks;
CREATE INDEX tasks_by_goal ON tasks(goal_id);
CREATE INDEX tasks_by_proposal ON tasks(proposal_id);

-- planners
CREATE TABLE planners_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    origin TEXT NOT NULL,
    proposal_id INTEGER REFERENCES proposals(id),
    workspace_id TEXT UNIQUE,
    wrapper_pid INTEGER,
    agent_pid INTEGER,
    heartbeat_at INTEGER,
    exit_code INTEGER,
    exited_at INTEGER,
    closed_at INTEGER,
    error TEXT,
    created_at INTEGER NOT NULL,
    draft_task_id INTEGER REFERENCES tasks(id),
    finding_id INTEGER
);
INSERT INTO planners_v50 (id, origin, proposal_id, workspace_id, wrapper_pid, agent_pid, heartbeat_at, exit_code, exited_at, closed_at, error, created_at, draft_task_id, finding_id)
SELECT id, origin, proposal_id, workspace_id, wrapper_pid, agent_pid, heartbeat_at, exit_code, exited_at, closed_at, error, created_at, draft_task_id, finding_id FROM planners;
DELETE FROM sqlite_sequence WHERE name = 'planners_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'planners_v50', seq FROM sqlite_sequence WHERE name = 'planners';
DROP TABLE planners;
ALTER TABLE planners_v50 RENAME TO planners;
CREATE INDEX planners_by_proposal ON planners(proposal_id);
CREATE INDEX planners_by_draft ON planners(draft_task_id);
CREATE INDEX planners_by_finding ON planners(finding_id);

-- schema_floor
CREATE TABLE schema_floor_v50 (
    singleton INTEGER PRIMARY KEY NOT NULL,
    floor INTEGER NOT NULL
);
INSERT INTO schema_floor_v50 (singleton, floor)
SELECT singleton, floor FROM schema_floor;
DROP TABLE schema_floor;
ALTER TABLE schema_floor_v50 RENAME TO schema_floor;

-- plan_reviews
CREATE TABLE plan_reviews_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    proposal_id INTEGER NOT NULL REFERENCES proposals(id),
    attempt INTEGER NOT NULL,
    supervisor_token TEXT NOT NULL,
    dir TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT,
    verdict TEXT,
    error TEXT
);
INSERT INTO plan_reviews_v50 (id, proposal_id, attempt, supervisor_token, dir, started_at, finished_at, outcome, verdict, error)
SELECT id, proposal_id, attempt, supervisor_token, dir, started_at, finished_at, outcome, verdict, error FROM plan_reviews;
DELETE FROM sqlite_sequence WHERE name = 'plan_reviews_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'plan_reviews_v50', seq FROM sqlite_sequence WHERE name = 'plan_reviews';
DROP TABLE plan_reviews;
ALTER TABLE plan_reviews_v50 RENAME TO plan_reviews;
CREATE UNIQUE INDEX plan_reviews_running ON plan_reviews(finished_at IS NULL)
    WHERE finished_at IS NULL;
CREATE INDEX plan_reviews_by_proposal ON plan_reviews(proposal_id, id);

-- draft_origins
CREATE TABLE draft_origins_v50 (
    task_id INTEGER PRIMARY KEY REFERENCES tasks(id),
    origin TEXT NOT NULL,
    material TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);
INSERT INTO draft_origins_v50 (task_id, origin, material, created_at)
SELECT task_id, origin, material, created_at FROM draft_origins;
DROP TABLE draft_origins;
ALTER TABLE draft_origins_v50 RENAME TO draft_origins;

-- findings
CREATE TABLE findings_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    target TEXT NOT NULL,
    task_id INTEGER REFERENCES tasks(id),
    run_id TEXT,
    goal_id INTEGER REFERENCES goals(id),
    subject TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    impact TEXT NOT NULL DEFAULT 'normal',
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    occurrences INTEGER NOT NULL DEFAULT 1,
    evidence TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'open',
    status_reason TEXT,
    proposal_id INTEGER REFERENCES proposals(id),
    propose_reason TEXT,
    propose_requested_at INTEGER,
    recorded_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO findings_v50 (id, kind, target, task_id, run_id, goal_id, subject, summary, detail, impact, first_seen_at, last_seen_at, occurrences, evidence, status, status_reason, proposal_id, propose_reason, propose_requested_at, recorded_by, updated_at)
SELECT id, kind, target, task_id, run_id, goal_id, subject, summary, detail, impact, first_seen_at, last_seen_at, occurrences, evidence, status, status_reason, proposal_id, propose_reason, propose_requested_at, recorded_by, updated_at FROM findings;
DELETE FROM sqlite_sequence WHERE name = 'findings_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'findings_v50', seq FROM sqlite_sequence WHERE name = 'findings';
DROP TABLE findings;
ALTER TABLE findings_v50 RENAME TO findings;
CREATE UNIQUE INDEX findings_unsettled ON findings(
    kind, target, ifnull(task_id, 0), ifnull(run_id, ''), ifnull(goal_id, 0), subject)
  WHERE status IN ('open', 'proposed');
CREATE INDEX findings_by_status ON findings(status, id);

-- binary_updates
CREATE TABLE binary_updates_v50 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  commit_sha TEXT,
  payload TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO binary_updates_v50 (id, kind, commit_sha, payload, created_at)
SELECT id, kind, commit_sha, payload, created_at FROM binary_updates;
DELETE FROM sqlite_sequence WHERE name = 'binary_updates_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'binary_updates_v50', seq FROM sqlite_sequence WHERE name = 'binary_updates';
DROP TABLE binary_updates;
ALTER TABLE binary_updates_v50 RENAME TO binary_updates;

-- asks
CREATE TABLE asks_v50 (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  task_id INTEGER REFERENCES tasks(id),
  run_id TEXT,
  question TEXT NOT NULL,
  options TEXT NOT NULL DEFAULT '[]',
  answer TEXT,
  asked_by TEXT NOT NULL,
  reason_category TEXT NOT NULL,
  subject TEXT,
  affected TEXT NOT NULL DEFAULT '[]',
  created_at INTEGER NOT NULL DEFAULT (unixepoch()),
  answered_at INTEGER,
  closed_at INTEGER,
  finding_id INTEGER REFERENCES findings(id),
  answered_by TEXT,
  option_index INTEGER,
  answer_authority TEXT,
  answer_approval INTEGER,
  FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO asks_v50 (id, kind, task_id, run_id, question, options, answer, asked_by, reason_category, subject, affected, created_at, answered_at, closed_at, finding_id, answered_by, option_index, answer_authority, answer_approval)
SELECT id, kind, task_id, run_id, question, options, answer, asked_by, reason_category, subject, affected, created_at, answered_at, closed_at, finding_id, answered_by, option_index, answer_authority, answer_approval FROM asks;
DELETE FROM sqlite_sequence WHERE name = 'asks_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'asks_v50', seq FROM sqlite_sequence WHERE name = 'asks';
DROP TABLE asks;
ALTER TABLE asks_v50 RENAME TO asks;
CREATE UNIQUE INDEX asks_open ON asks(ifnull(task_id, 0), ifnull(run_id, ''), kind,
                                      reason_category, ifnull(subject, ''),
                                      ifnull(finding_id, 0))
  WHERE answered_at IS NULL AND closed_at IS NULL;

-- run_events
CREATE TABLE run_events_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER REFERENCES tasks(id),
    goal_id INTEGER REFERENCES goals(id),
    run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    actor_role TEXT,
    actor_id TEXT,
    requested_by TEXT,
    FOREIGN KEY (run_id, task_id) REFERENCES task_runs(id, task_id)
);
INSERT INTO run_events_v50 (id, task_id, goal_id, run_id, kind, payload, created_at, actor_role, actor_id, requested_by)
SELECT id, task_id, goal_id, run_id, kind, payload, created_at, actor_role, actor_id, requested_by FROM run_events;
DELETE FROM sqlite_sequence WHERE name = 'run_events_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'run_events_v50', seq FROM sqlite_sequence WHERE name = 'run_events';
DROP TABLE run_events;
ALTER TABLE run_events_v50 RENAME TO run_events;
CREATE INDEX events_by_task ON run_events(task_id, id);
CREATE INDEX events_by_goal ON run_events(goal_id, id);
CREATE INDEX events_by_kind ON run_events(kind, id);

-- draft_reopens
CREATE TABLE draft_reopens_v50 (
    task_id INTEGER PRIMARY KEY,
    material TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);
INSERT INTO draft_reopens_v50 (task_id, material, created_at)
SELECT task_id, material, created_at FROM draft_reopens;
DROP TABLE draft_reopens;
ALTER TABLE draft_reopens_v50 RENAME TO draft_reopens;

-- goal_reviews
CREATE TABLE goal_reviews_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    goal_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    supervisor_token TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    dir TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    outcome TEXT,
    verdict TEXT,
    error TEXT,
    ask_id INTEGER,
    rearmed_at INTEGER
);
INSERT INTO goal_reviews_v50 (id, goal_id, attempt, supervisor_token, fingerprint, dir, started_at, finished_at, outcome, verdict, error, ask_id, rearmed_at)
SELECT id, goal_id, attempt, supervisor_token, fingerprint, dir, started_at, finished_at, outcome, verdict, error, ask_id, rearmed_at FROM goal_reviews;
DELETE FROM sqlite_sequence WHERE name = 'goal_reviews_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'goal_reviews_v50', seq FROM sqlite_sequence WHERE name = 'goal_reviews';
DROP TABLE goal_reviews;
ALTER TABLE goal_reviews_v50 RENAME TO goal_reviews;
CREATE INDEX goal_reviews_by_goal ON goal_reviews(goal_id, id);

-- headless_jobs
CREATE TABLE headless_jobs_v50 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    label TEXT,
    run_id TEXT,
    proposal_id INTEGER,
    goal_id INTEGER,
    attempt INTEGER NOT NULL,
    pid INTEGER NOT NULL,
    process_start TEXT,
    supervisor_token TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    outcome TEXT
);
INSERT INTO headless_jobs_v50 (id, kind, label, run_id, proposal_id, goal_id, attempt, pid, process_start, supervisor_token, started_at, ended_at, outcome)
SELECT id, kind, label, run_id, proposal_id, goal_id, attempt, pid, process_start, supervisor_token, started_at, ended_at, outcome FROM headless_jobs;
DELETE FROM sqlite_sequence WHERE name = 'headless_jobs_v50';
INSERT INTO sqlite_sequence (name, seq) SELECT 'headless_jobs_v50', seq FROM sqlite_sequence WHERE name = 'headless_jobs';
DROP TABLE headless_jobs;
ALTER TABLE headless_jobs_v50 RENAME TO headless_jobs;
CREATE INDEX headless_jobs_unfinished ON headless_jobs(id) WHERE ended_at IS NULL;

CREATE TRIGGER search_task_inserted AFTER INSERT ON tasks BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4, 'task', new.id, new.id, new.goal_id, NULL, new.status, new.updated_at,
            new.title, new.description, new.acceptance, new.context, '');
END;

CREATE TRIGGER search_task_updated AFTER UPDATE ON tasks BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4;
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4, 'task', new.id, new.id, new.goal_id, NULL, new.status, new.updated_at,
            new.title, new.description, new.acceptance, new.context, '');
END;

CREATE TRIGGER search_task_deleted AFTER DELETE ON tasks BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4;
END;

CREATE TRIGGER search_goal_inserted AFTER INSERT ON goals BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 1, 'goal', new.id, NULL, new.id, NULL,
            CASE WHEN new.closed_at IS NULL THEN new.status ELSE new.verdict END, new.updated_at,
            new.title, new.description, new.acceptance, new.constraints, '');
END;

CREATE TRIGGER search_goal_updated AFTER UPDATE ON goals BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 1;
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 1, 'goal', new.id, NULL, new.id, NULL,
            CASE WHEN new.closed_at IS NULL THEN new.status ELSE new.verdict END, new.updated_at,
            new.title, new.description, new.acceptance, new.constraints, '');
    UPDATE search_index
    SET status = CASE WHEN new.closed_at IS NULL THEN new.status ELSE new.verdict END
    WHERE kind = 'note' AND task_id IS NULL AND goal_id = new.id;
END;

CREATE TRIGGER search_goal_deleted AFTER DELETE ON goals BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 1;
END;

CREATE TRIGGER search_commit_inserted AFTER INSERT ON landed_commits BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 3, 'commit', new.commit_sha, new.task_id,
            (SELECT goal_id FROM tasks WHERE id = new.task_id), new.run_id,
            (SELECT status FROM tasks WHERE id = new.task_id), new.landed_at,
            '', '', '', '', coalesce(new.message, ''));
END;

CREATE TRIGGER search_commit_updated AFTER UPDATE ON landed_commits BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 3;
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 3, 'commit', new.commit_sha, new.task_id,
            (SELECT goal_id FROM tasks WHERE id = new.task_id), new.run_id,
            (SELECT status FROM tasks WHERE id = new.task_id), new.landed_at,
            '', '', '', '', coalesce(new.message, ''));
END;

CREATE TRIGGER search_note_inserted AFTER INSERT ON run_events
WHEN new.kind = 'observation' BEGIN
    INSERT INTO search_index (rowid, kind, ref, task_id, goal_id, run_id, status, updated_at,
                              title, description, acceptance, context, text)
    VALUES (new.id * 4 + 2, 'note', new.id, new.task_id,
            coalesce(new.goal_id, (SELECT goal_id FROM tasks WHERE id = new.task_id)), new.run_id,
            coalesce((SELECT status FROM tasks WHERE id = new.task_id),
                     (SELECT CASE WHEN closed_at IS NULL THEN status ELSE verdict END
                      FROM goals WHERE id = new.goal_id)),
            new.created_at, '', '', '', '', coalesce(CAST(json_extract(new.payload, '$.text') AS TEXT), ''));
END;

CREATE TRIGGER search_note_deleted AFTER DELETE ON run_events
WHEN old.kind = 'observation' BEGIN
    DELETE FROM search_index WHERE rowid = old.id * 4 + 2;
END;

CREATE TRIGGER search_run_integrated AFTER INSERT ON run_events
WHEN new.kind = 'run_integrated' BEGIN
    INSERT OR IGNORE INTO landed_commits (run_id, task_id, commit_sha, message, git_common_dir,
                                          landed_at)
    VALUES (new.run_id, new.task_id, json_extract(new.payload, '$.result_commit'),
            CAST(json_extract(new.payload, '$.message') AS TEXT),
            json_extract(new.payload, '$.git_common_dir'),
            new.created_at);
END;

CREATE TRIGGER search_task_moved AFTER UPDATE OF status, goal_id ON tasks
WHEN old.status IS NOT new.status OR old.goal_id IS NOT new.goal_id BEGIN
    UPDATE search_index SET status = new.status, goal_id = new.goal_id
    WHERE task_id = new.id AND kind = 'commit';
    UPDATE search_index
    SET status = new.status,
        goal_id = coalesce((SELECT e.goal_id FROM run_events e WHERE e.id = search_index.ref),
                           new.goal_id)
    WHERE task_id = new.id AND kind = 'note';
END;


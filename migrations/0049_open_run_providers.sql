-- dagq-schema: breaking
-- task_runs without CHECKs (task 816, ADR-t876-1): it lost the CHECKs
-- `requested_provider = 'claude'` and `actual_provider = 'claude'` it had
-- since 0001, so a Codex run can be written, and with them the CHECKs on
-- status and worker_mode, since a new migration writes no CHECK. The rules
-- stay in the domain and the write port: a run is inserted and saved only
-- from a `TaskRun`, whose status, providers and mode are the typed
-- `RunStatus`, `Provider` and `WorkerMode`, and a row with a value outside
-- them fails its read (fail closed). NOT NULL, the keys and the partial
-- UNIQUE indexes stay. SQLite cannot drop a CHECK, so task_runs is rebuilt
-- as in 0006 with its rowid order and every column (worker_mode of 0048
-- included) preserved, and its indexes are created again; the migration
-- runner disables foreign keys around this script and runs
-- foreign_key_check before committing. Breaking: an older binary cannot
-- read a Codex run.
CREATE TABLE task_runs_v49 (
    id TEXT PRIMARY KEY NOT NULL,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    status TEXT NOT NULL,
    requested_provider TEXT NOT NULL,
    actual_provider TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    branch TEXT,
    worktree_path TEXT,
    workspace_id TEXT,
    receipt_path TEXT,
    log_path TEXT,
    result_commit TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    repo_path TEXT,
    run_dir TEXT,
    supervisor_token TEXT,
    last_error TEXT,
    workspace_closed_at INTEGER,
    worker_mode TEXT,
    UNIQUE (id, task_id)
);
INSERT INTO task_runs_v49 (rowid, id, task_id, status, requested_provider, actual_provider,
    base_commit, branch, worktree_path, workspace_id, receipt_path, log_path, result_commit,
    created_at, repo_path, run_dir, supervisor_token, last_error, workspace_closed_at,
    worker_mode)
SELECT rowid, id, task_id, status, requested_provider, actual_provider,
    base_commit, branch, worktree_path, workspace_id, receipt_path, log_path, result_commit,
    created_at, repo_path, run_dir, supervisor_token, last_error, workspace_closed_at,
    worker_mode
FROM task_runs;
DROP TABLE task_runs;
ALTER TABLE task_runs_v49 RENAME TO task_runs;
CREATE INDEX runs_by_task ON task_runs(task_id);
CREATE UNIQUE INDEX one_unfinished_run_per_task ON task_runs(task_id)
    WHERE status IN ('claimed', 'starting', 'running', 'validating', 'awaiting_integration',
                     'integrating', 'needs_session');
CREATE UNIQUE INDEX one_integrated_run_per_task ON task_runs(task_id)
    WHERE status = 'integrated';
CREATE UNIQUE INDEX one_integrating_run_per_queue ON task_runs((1))
    WHERE status = 'integrating';

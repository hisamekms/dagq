-- dagq-schema: compatible
-- The resources in the common units of every environment (ADR-t1662-1
-- decision 8 and (ii), ADR-t1662-3 decisions 3 and 4).
--
-- node_samples gains the sample in the common units, as the environment's
-- measure of the node made it of the same sample, in the same row: the
-- CPU seconds over the sampling interval, the CPU limit in cores, the most
-- memory used in bytes, the seconds a CPU limit throttled, and the CPU
-- pressure (PSI `some avg10`, a percentage). Null where the environment
-- could not give it, and in the rows before this migration and those
-- imported from a CSV file. node_id is the node's of the
-- `environment_attached` of the runs on it. The columns of the CSV files
-- keep their values: no summary of the host reads these in their place.
--
-- run_resources: the windows of a run's resources the supervisor measured
-- from outside, one row per window. id is the order they were recorded
-- (AUTOINCREMENT: a cursor past it holds). attachment is the id of the
-- run's `environment_attached` the window was measured under; from_ms and
-- until_ms the window on the supervisor's clock (unix ms); the values in
-- the common units as in node_samples, and the load average where the
-- environment has one, null where it could not measure them. is_last is 1
-- for the measure taken after the run left that attachment, which the
-- run's ledger row waits on to settle.
-- Additions only: an older binary ignores the columns and the table.
ALTER TABLE node_samples ADD COLUMN cpu_seconds REAL;
ALTER TABLE node_samples ADD COLUMN cpu_limit REAL;
ALTER TABLE node_samples ADD COLUMN memory_max_bytes INTEGER;
ALTER TABLE node_samples ADD COLUMN throttled_seconds REAL;
ALTER TABLE node_samples ADD COLUMN cpu_pressure REAL;

CREATE TABLE run_resources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    attachment INTEGER NOT NULL,
    node_id TEXT NOT NULL,
    from_ms INTEGER NOT NULL,
    until_ms INTEGER NOT NULL,
    cpu_seconds REAL,
    cpu_limit REAL,
    memory_max_bytes INTEGER,
    throttled_seconds REAL,
    cpu_pressure REAL,
    load_average REAL,
    is_last INTEGER NOT NULL
);

CREATE INDEX run_resources_run ON run_resources(run_id);
CREATE INDEX run_resources_attachment ON run_resources(attachment, is_last);

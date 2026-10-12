-- dagq-schema: compatible
-- The node samples (ADR-t1662-2 decisions 5 and 7, NodeSampleStore): the
-- permanent record of a node's load the supervisor samples every
-- `--host-metrics-interval`, the input the measurement reads in place of
-- the host's CSV files, which become the copy a person reads. Not events:
-- continuous values stay out of run_events.
--
-- id is the order the rows came in (AUTOINCREMENT: never reused after the
-- retention removes rows, so a cursor past it holds). node_id names the
-- node (`host` for the supervisor's host); unix is when the sample was
-- taken, one row per node and second. local_day is the node's local day
-- the sample is on (days since the epoch), the day of the CSV file it is
-- in: the half-interval rule and the retention go by it as by the files.
-- source is `sampled` (the supervisor) or `csv_import` (a row of a CSV
-- file kept from before this table). Each other column is a column of the
-- CSV files of the same name; a value the node could not give is null,
-- and a column added later is null in the rows before it.
--
-- node_sample_imports holds each CSV file imported and its length in
-- bytes as it was read, so an import reads it again only once rows were
-- added after (a job that ended late, a clock gone back); day is its local
-- day (days since the epoch), for the retention.
-- Additions only: an older binary ignores the tables.
CREATE TABLE node_samples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id TEXT NOT NULL,
    unix INTEGER NOT NULL,
    local_day INTEGER NOT NULL,
    source TEXT NOT NULL,
    load1 REAL,
    load5 REAL,
    load15 REAL,
    cpu_total REAL,
    cpu_cargo REAL,
    cpu_rustc REAL,
    cpu_claude REAL,
    cpu_dagq REAL,
    cpu_other REAL,
    rss_cargo_mb REAL,
    rss_rustc_mb REAL,
    rss_claude_mb REAL,
    rss_dagq_mb REAL,
    rss_other_mb REAL,
    mem_total_mb REAL,
    mem_used_mb REAL,
    mem_compressed_mb REAL,
    swap_total_mb REAL,
    swap_used_mb REAL,
    pageouts REAL,
    disk_free_bytes REAL,
    disk_total_bytes REAL,
    disk_free_pct REAL,
    UNIQUE (node_id, unix)
);

CREATE INDEX node_samples_local_day ON node_samples(local_day);

CREATE TABLE node_sample_imports (
    node_id TEXT NOT NULL,
    file TEXT NOT NULL,
    day INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    rows INTEGER NOT NULL,
    imported_at INTEGER NOT NULL,
    PRIMARY KEY (node_id, file)
);

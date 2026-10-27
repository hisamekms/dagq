-- dagq-schema: compatible
-- The build outputs of the runs that keep their worktree while they wait
-- (task 1382), in the node samples as in the host's CSV files: the runs
-- waiting outside the slots and the awaiting_integration runs, each
-- kind's count and the bytes on disk of their worktrees' build
-- directories, and both kinds' bytes together. The supervisor measures
-- them only every so often, so a row it did not measure them for, and
-- every row before this migration, has them null; a byte count is null
-- when a run of its kind could not be measured.
-- Additions only: an older binary names the columns it writes and reads,
-- and leaves these null.
ALTER TABLE node_samples ADD COLUMN waiting_runs REAL;
ALTER TABLE node_samples ADD COLUMN waiting_target_bytes REAL;
ALTER TABLE node_samples ADD COLUMN landing_runs REAL;
ALTER TABLE node_samples ADD COLUMN landing_target_bytes REAL;
ALTER TABLE node_samples ADD COLUMN held_target_bytes REAL;

-- The owner pairs of goals and of tasks without a goal; same contract as
-- SQLite's goal_owner migration.
ALTER TABLE goals ADD COLUMN owner_issuer text;
ALTER TABLE goals ADD COLUMN owner_subject text;
ALTER TABLE tasks ADD COLUMN owner_issuer text;
ALTER TABLE tasks ADD COLUMN owner_subject text;

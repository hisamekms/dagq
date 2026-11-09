-- Declaration is untrusted scheduling data. Only explicit administrator
-- approval changes shared; declaration replacement never updates approval.
ALTER TABLE runners ADD COLUMN declaration TEXT;
ALTER TABLE runners ADD COLUMN shared BIGINT NOT NULL DEFAULT 0;
ALTER TABLE runners ADD COLUMN approved_by TEXT;

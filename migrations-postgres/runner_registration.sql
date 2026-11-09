-- Only credential hashes are retained, including consumed refresh credentials
-- so replay can revoke every credential of that runner.
-- Runner linkage is established in the store's credential transaction; no
-- foreign keys are added, preserving the compatible migration's contract.
CREATE TABLE runners (
    id TEXT PRIMARY KEY NOT NULL,
    owner_issuer TEXT NOT NULL,
    owner_subject TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    revoked_at BIGINT
);
CREATE TABLE runner_codes (
    hash TEXT PRIMARY KEY NOT NULL,
    owner_issuer TEXT NOT NULL,
    owner_subject TEXT NOT NULL,
    expires_at BIGINT NOT NULL,
    used_at BIGINT
);
CREATE TABLE runner_refresh (
    hash TEXT PRIMARY KEY NOT NULL,
    runner_id TEXT NOT NULL,
    expires_at BIGINT NOT NULL,
    used_at BIGINT
);
CREATE TABLE runner_tokens (
    hash TEXT PRIMARY KEY NOT NULL,
    runner_id TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);

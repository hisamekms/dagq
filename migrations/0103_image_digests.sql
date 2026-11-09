-- dagq-schema: compatible
-- The image publisher's tokens: only the value's hash, with the claims of the
-- CI that exchanged it (who handed a digest over), until expires_at.
CREATE TABLE image_publishers (
    hash TEXT PRIMARY KEY NOT NULL,
    id TEXT NOT NULL,
    repository TEXT NOT NULL,
    workflow_ref TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
-- Appended per architecture; an architecture's latest row (highest id) is
-- main's digest now. A pair the architecture recorded before adds no row.
CREATE TABLE image_digests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    arch TEXT NOT NULL,
    tag TEXT NOT NULL,
    digest TEXT NOT NULL,
    repository TEXT NOT NULL,
    workflow_ref TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    published_at INTEGER NOT NULL
);
CREATE INDEX image_digests_arch ON image_digests(arch, id);

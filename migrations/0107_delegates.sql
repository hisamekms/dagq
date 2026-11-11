-- dagq-schema: compatible
-- The people who answer for an owner in the team version (ADR-t3989-1
-- decision 3): one row per owner and delegate, both registered people of
-- `members` by their OIDC (issuer, subject) pair. `owner_issuer` and
-- `owner_subject` name the owner who chose the delegate; `delegate_issuer`
-- and `delegate_subject` the person its asks go to as well; `created_at`
-- the Unix seconds of the add. Only the owner (or the inbox of that owner)
-- adds or removes its own rows, through `delegate.write`, and each change is
-- written with its `delegates_changed` event (the delegates before and
-- after, the actor who changed them) in one transaction. No foreign key to
-- members: a registry row is disabled, never deleted, and the domain checks
-- that a delegate is registered and enabled when it is added or removed. Additions
-- only: an older binary ignores the table, and no local queue has a row.
CREATE TABLE delegates (
    owner_issuer TEXT NOT NULL,
    owner_subject TEXT NOT NULL,
    delegate_issuer TEXT NOT NULL,
    delegate_subject TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (owner_issuer, owner_subject, delegate_issuer, delegate_subject)
);

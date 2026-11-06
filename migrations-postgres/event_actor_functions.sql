-- Transaction-local settings are supplied by the execution adapter.
-- An absent or reset custom setting reads as NULL.
CREATE FUNCTION dagq_actor_role() RETURNS text LANGUAGE sql STABLE AS $$
    SELECT NULLIF(current_setting('dagq.actor_role', true), '')
$$;
CREATE FUNCTION dagq_actor_id() RETURNS text LANGUAGE sql STABLE AS $$
    SELECT NULLIF(current_setting('dagq.actor_id', true), '')
$$;
CREATE FUNCTION dagq_requested_by() RETURNS text LANGUAGE sql STABLE AS $$
    SELECT NULLIF(current_setting('dagq.requested_by', true), '')
$$;

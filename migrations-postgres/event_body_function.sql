-- The body stays in the row: a DB function cannot call the application's body port.
-- The local InlineEventBodies has the same result.
CREATE FUNCTION dagq_event_body(body jsonb) RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
    SELECT body
$$;

-- A mark without a generation is not held. Readers trust this view only in
-- the write leader's current generation. A writer carries a mark from its
-- immediately preceding generation; a gap discards it and refills run rows
-- in bounded parts, with through NULL only after all rows were taken in.
ALTER TABLE ledger_first_executions_ready ADD COLUMN generation BIGINT;
ALTER TABLE ledger_first_executions_ready ADD COLUMN through TEXT;

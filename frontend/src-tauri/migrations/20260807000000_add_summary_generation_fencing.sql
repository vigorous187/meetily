-- Fence asynchronous summary generations so a superseded task cannot overwrite
-- the state or restored result of a newer task for the same meeting.
ALTER TABLE summary_processes
ADD COLUMN generation_id TEXT;

-- How many times a job was claimed. A job whose worker dies running it (e.g.
-- it is killed for running out of memory) goes stale and is claimed again;
-- after a bounded number of claims it is failed instead, so one poison job
-- doesn't take down every replica in turn.
ALTER TABLE jobs ADD COLUMN attempts int NOT NULL DEFAULT 0;

-- Staged uploads are also scanned by workers, which fail those whose upload
-- stopped (heartbeat_at is bumped as data arrives).
DROP INDEX jobs_queued;
CREATE INDEX jobs_active ON jobs (id) WHERE status IN ('staging', 'queued', 'running');

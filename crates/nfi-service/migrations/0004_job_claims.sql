-- A fencing token for each claim of a job. Worker names (host and pid) can
-- repeat, e.g. in containers, so a worker that lost its claim to another
-- (because its heartbeat went stale) is told apart by this token: its
-- heartbeat, progress and result updates match no row.
ALTER TABLE jobs ADD COLUMN claim_id uuid;

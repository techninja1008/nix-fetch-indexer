-- Index the missing_paths -> closures foreign key: deleting a closure (prune)
-- cascades to missing_paths, which otherwise scans the whole table for each
-- deleted closure.
CREATE INDEX missing_paths_closure ON missing_paths (closure_id);

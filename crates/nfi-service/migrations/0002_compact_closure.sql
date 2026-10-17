-- Store a path's release membership and references as arrays on its row.
--
-- A channel's releases share most store paths, so one row per (path,
-- release) grew to tens of millions of rows, and one row per reference to
-- millions more, each with index and foreign-key maintenance. Membership is
-- only used by prune, and references are only ever read whole per path.
ALTER TABLE store_paths
    ADD COLUMN releases int[] NOT NULL DEFAULT '{}',
    ADD COLUMN refs bigint[] NOT NULL DEFAULT '{}';

UPDATE store_paths s
   SET releases = m.releases
  FROM (SELECT path_id, array_agg(release_id ORDER BY release_id) AS releases
          FROM store_path_releases GROUP BY path_id) m
 WHERE m.path_id = s.id;

UPDATE store_paths s
   SET refs = r.refs
  FROM (SELECT path_id, array_agg(ref_id ORDER BY ref_id) AS refs
          FROM store_path_refs GROUP BY path_id) r
 WHERE r.path_id = s.id;

DROP TABLE store_path_releases;
DROP TABLE store_path_refs;

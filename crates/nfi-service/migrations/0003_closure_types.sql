-- Evaluations and closures.
--
-- An evaluation is a release evaluated in one way (its eval type): the
-- `store-paths` pseudo-evaluation (store-paths.xz, from Hydra's
-- evaluation), helper probes (`helpers`), ... Each evaluation produces
-- closures, one per closure type of its eval type; closure type names are
-- namespaced by eval type and may be empty. Store path membership and
-- missing paths refer to closures instead of releases, and eval errors to
-- evaluations.
CREATE TABLE eval_types (
    id    serial PRIMARY KEY,
    name  text NOT NULL UNIQUE
);
INSERT INTO eval_types (name) VALUES ('store-paths');

CREATE TABLE evals (
    id              serial  PRIMARY KEY,
    release_id      int     NOT NULL REFERENCES releases ON DELETE CASCADE,
    type_id         int     NOT NULL REFERENCES eval_types,
    -- Whether the evaluation's errors are known (recorded in eval_errors).
    -- When false, nothing is known about its errors.
    errors_indexed  boolean NOT NULL DEFAULT false,
    UNIQUE (release_id, type_id)
);

CREATE TABLE closure_types (
    id            serial PRIMARY KEY,
    eval_type_id  int    NOT NULL REFERENCES eval_types,
    name          text   NOT NULL DEFAULT '',
    UNIQUE (eval_type_id, name)
);
INSERT INTO closure_types (eval_type_id, name)
SELECT id, '' FROM eval_types WHERE name = 'store-paths';

-- A closure's release is its evaluation's; its type's eval type is the
-- evaluation's type (maintained by ingest).
CREATE TABLE closures (
    id       serial PRIMARY KEY,
    eval_id  int    NOT NULL REFERENCES evals ON DELETE CASCADE,
    type_id  int    NOT NULL REFERENCES closure_types,
    UNIQUE (eval_id, type_id)
);

-- Existing closures are store-paths closures. Their evaluations and the
-- closures themselves take their release's id, so the ids already in
-- store_paths and missing_paths stay valid.
INSERT INTO evals (id, release_id, type_id)
SELECT r.id, r.id, t.id
  FROM releases r
  JOIN release_datasets d ON d.release_id = r.id AND d.dataset = 'closure'
  JOIN eval_types t ON t.name = 'store-paths';
INSERT INTO closures (id, eval_id, type_id)
SELECT e.id, e.id, ct.id
  FROM evals e
  JOIN closure_types ct ON ct.eval_type_id = e.type_id AND ct.name = '';
SELECT setval(pg_get_serial_sequence('evals', 'id'),
              GREATEST((SELECT max(id) FROM evals), (SELECT max(id) FROM releases), 1));
SELECT setval(pg_get_serial_sequence('closures', 'id'),
              GREATEST((SELECT max(id) FROM closures), (SELECT max(id) FROM releases), 1));

ALTER TABLE store_paths RENAME COLUMN releases TO closures;

ALTER TABLE missing_paths DROP CONSTRAINT missing_paths_pkey;
ALTER TABLE missing_paths DROP CONSTRAINT missing_paths_release_id_fkey;
ALTER TABLE missing_paths RENAME COLUMN release_id TO closure_id;
DELETE FROM missing_paths m WHERE NOT EXISTS (SELECT 1 FROM closures c WHERE c.id = m.closure_id);
ALTER TABLE missing_paths
    ADD PRIMARY KEY (hash, closure_id),
    ADD FOREIGN KEY (closure_id) REFERENCES closures ON DELETE CASCADE;

-- Attributes that failed to evaluate, in evaluations with errors_indexed.
-- Evaluation is deterministic, so the attribute and system identify the
-- failure; no message is kept.
CREATE TABLE eval_errors (
    attr     text NOT NULL,
    system   text NOT NULL,
    eval_id  int  NOT NULL REFERENCES evals ON DELETE CASCADE,
    PRIMARY KEY (attr, system, eval_id)
);
CREATE INDEX eval_errors_eval ON eval_errors (eval_id);

-- Minimise's close-miss check looks paths up by name.
CREATE INDEX store_paths_name ON store_paths (name);

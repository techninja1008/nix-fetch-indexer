-- Closure types: a release can have several closures, each collected from a
-- different kind of root (store-paths.xz runtime closures, helper probes,
-- ...). Store path membership and missing paths refer to closures instead
-- of releases.
CREATE TABLE closure_types (
    id    serial PRIMARY KEY,
    name  text NOT NULL UNIQUE
);
INSERT INTO closure_types (name) VALUES ('store-paths');

CREATE TABLE closures (
    id          serial PRIMARY KEY,
    release_id  int    NOT NULL REFERENCES releases ON DELETE CASCADE,
    type_id     int    NOT NULL REFERENCES closure_types,
    UNIQUE (release_id, type_id)
);

-- Existing closures are store-paths closures. They take their release's id,
-- so the ids already in store_paths and missing_paths stay valid.
INSERT INTO closures (id, release_id, type_id)
SELECT r.id, r.id, t.id
  FROM releases r
  JOIN release_datasets d ON d.release_id = r.id AND d.dataset = 'closure'
  JOIN closure_types t ON t.name = 'store-paths';
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

-- Minimise's close-miss check looks paths up by name.
CREATE INDEX store_paths_name ON store_paths (name);

-- Evaluations: a release can be evaluated in several ways (helper probes,
-- ...). Eval types, evaluations and eval errors are to evaluation what
-- closure types, closures and missing paths are to closures.
CREATE TABLE eval_types (
    id    serial PRIMARY KEY,
    name  text NOT NULL UNIQUE
);

CREATE TABLE evals (
    id          serial PRIMARY KEY,
    release_id  int NOT NULL REFERENCES releases ON DELETE CASCADE,
    type_id     int NOT NULL REFERENCES eval_types,
    UNIQUE (release_id, type_id)
);

-- Attributes that failed to evaluate. Evaluation is deterministic, so the
-- attribute and system identify the failure; no message is kept.
CREATE TABLE eval_errors (
    attr     text NOT NULL,
    system   text NOT NULL,
    eval_id  int  NOT NULL REFERENCES evals ON DELETE CASCADE,
    PRIMARY KEY (attr, system, eval_id)
);
CREATE INDEX eval_errors_eval ON eval_errors (eval_id);

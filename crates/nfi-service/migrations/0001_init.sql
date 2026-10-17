CREATE TABLE releases (
    id          serial PRIMARY KEY,
    name        text NOT NULL UNIQUE,
    commit      text NOT NULL UNIQUE,
    channel     text NOT NULL,
    series      text NOT NULL,
    url         text,
    indexed_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX releases_channel ON releases (channel);
CREATE INDEX releases_commit_prefix ON releases (commit text_pattern_ops);

CREATE TABLE release_datasets (
    release_id  int  NOT NULL REFERENCES releases ON DELETE CASCADE,
    dataset     text NOT NULL,
    PRIMARY KEY (release_id, dataset)
);

CREATE TABLE store_paths (
    id         bigserial PRIMARY KEY,
    hash       text NOT NULL UNIQUE,
    name       text NOT NULL,
    nar_size   bigint,
    file_size  bigint,
    deriver    text
);

CREATE TABLE store_path_refs (
    path_id  bigint NOT NULL REFERENCES store_paths ON DELETE CASCADE,
    ref_id   bigint NOT NULL REFERENCES store_paths ON DELETE CASCADE,
    PRIMARY KEY (path_id, ref_id)
);
CREATE INDEX store_path_refs_ref ON store_path_refs (ref_id);

CREATE TABLE store_path_releases (
    path_id     bigint NOT NULL REFERENCES store_paths ON DELETE CASCADE,
    release_id  int    NOT NULL REFERENCES releases ON DELETE CASCADE,
    PRIMARY KEY (path_id, release_id)
);
CREATE INDEX store_path_releases_release ON store_path_releases (release_id);

CREATE TABLE missing_paths (
    hash        text NOT NULL,
    name        text NOT NULL,
    release_id  int  NOT NULL REFERENCES releases ON DELETE CASCADE,
    PRIMARY KEY (hash, release_id)
);

CREATE TABLE packages (
    id            bigserial PRIMARY KEY,
    key           text NOT NULL UNIQUE,  -- hash of the metadata below
    attr          text NOT NULL,
    system        text NOT NULL,
    name          text NOT NULL,
    pname         text,
    version       text,
    description   text,
    homepage      text,
    license       text[] NOT NULL DEFAULT '{}',
    main_program  text,
    outputs       text[] NOT NULL DEFAULT '{}',
    broken        boolean NOT NULL DEFAULT false,
    unfree        boolean NOT NULL DEFAULT false
);
CREATE INDEX packages_attr ON packages (attr, system);

CREATE TABLE package_releases (
    package_id  bigint NOT NULL REFERENCES packages ON DELETE CASCADE,
    release_id  int    NOT NULL REFERENCES releases ON DELETE CASCADE,
    PRIMARY KEY (package_id, release_id)
);
CREATE INDEX package_releases_release ON package_releases (release_id);

CREATE TABLE package_outputs (
    release_id  int  NOT NULL REFERENCES releases ON DELETE CASCADE,
    attr        text NOT NULL,
    system      text NOT NULL,
    output      text NOT NULL,
    path        text NOT NULL,
    drv_path    text,
    PRIMARY KEY (release_id, attr, system, output)
);

CREATE TABLE programs (
    release_id  int  NOT NULL REFERENCES releases ON DELETE CASCADE,
    name        text NOT NULL,
    system      text NOT NULL,
    attr        text NOT NULL,
    PRIMARY KEY (release_id, name, system, attr)
);
CREATE INDEX programs_attr ON programs (attr, system);

CREATE TABLE jobs (
    id            bigserial PRIMARY KEY,
    kind          text NOT NULL,
    status        text NOT NULL DEFAULT 'queued',
    params        jsonb NOT NULL DEFAULT '{}',
    progress      text,
    result        jsonb,
    error         text,
    worker        text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    started_at    timestamptz,
    heartbeat_at  timestamptz,
    finished_at   timestamptz
);
CREATE INDEX jobs_queued ON jobs (id) WHERE status IN ('queued', 'running');

CREATE TABLE job_blob_chunks (
    job_id  bigint NOT NULL REFERENCES jobs ON DELETE CASCADE,
    seq     int    NOT NULL,
    data    bytea  NOT NULL,
    PRIMARY KEY (job_id, seq)
);

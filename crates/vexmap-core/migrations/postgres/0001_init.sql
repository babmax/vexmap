-- Keep in step with migrations/sqlite: same tables, columns and meaning. Every value is TEXT or BIGINT so that one
-- query layer reads both databases.

CREATE TABLE journal_head (
    only_row INTEGER PRIMARY KEY CHECK (only_row = 1),
    seq BIGINT NOT NULL
);
INSERT INTO journal_head (only_row, seq) VALUES (1, 0);

CREATE TABLE journal (
    seq BIGINT PRIMARY KEY,
    at_ms BIGINT NOT NULL,
    region TEXT NOT NULL,
    actor TEXT NOT NULL,
    request_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    object_type TEXT NOT NULL,
    object_id TEXT NOT NULL,
    before_json TEXT,
    after_json TEXT,
    UNIQUE (actor, request_id)
);
CREATE INDEX journal_object ON journal (object_type, object_id, seq);

CREATE TABLE players (
    id TEXT PRIMARY KEY,
    name TEXT,
    role TEXT NOT NULL,
    categories BIGINT NOT NULL,
    updated_seq BIGINT NOT NULL
);
CREATE INDEX players_role ON players (role);

CREATE TABLE feature_flags (
    module TEXT NOT NULL,
    region TEXT NOT NULL,
    enabled BIGINT NOT NULL,
    updated_seq BIGINT NOT NULL,
    PRIMARY KEY (module, region)
);

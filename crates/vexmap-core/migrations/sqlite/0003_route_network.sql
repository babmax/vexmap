-- Keep in step with migrations/postgres. The hand-drawn network that routes are built from (Phase 6). A row's id is the
-- journal seq of the change that created it, so ids are unique without a counter of their own.
CREATE TABLE route_nodes (
    id BIGINT PRIMARY KEY,
    x BIGINT NOT NULL,
    z BIGINT NOT NULL,
    transfer BIGINT NOT NULL,
    updated_seq BIGINT NOT NULL,
    UNIQUE (x, z)
);

CREATE TABLE route_lines (
    id BIGINT PRIMARY KEY,
    a BIGINT NOT NULL REFERENCES route_nodes (id),
    b BIGINT NOT NULL REFERENCES route_nodes (id),
    mode TEXT NOT NULL,
    one_way BIGINT NOT NULL,
    closed BIGINT NOT NULL,
    updated_seq BIGINT NOT NULL
);
CREATE INDEX route_lines_a ON route_lines (a);
CREATE INDEX route_lines_b ON route_lines (b);

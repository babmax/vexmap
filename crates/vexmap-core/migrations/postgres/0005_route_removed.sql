-- Keep in step with migrations/sqlite. Nodes and lines that were rolled back. The row itself is deleted, so unique
-- constraints and every reader of the network stay as they were; this is what tells clients to drop their copy.
CREATE TABLE route_removed (
    object_type TEXT NOT NULL,
    id BIGINT NOT NULL,
    updated_seq BIGINT NOT NULL,
    PRIMARY KEY (object_type, id)
);
CREATE INDEX route_removed_updated ON route_removed (updated_seq);

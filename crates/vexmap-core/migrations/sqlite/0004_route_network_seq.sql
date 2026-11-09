-- Keep in step with migrations/postgres. Clients follow the network by asking what changed after a version.
CREATE INDEX route_nodes_updated ON route_nodes (updated_seq);
CREATE INDEX route_lines_updated ON route_lines (updated_seq);

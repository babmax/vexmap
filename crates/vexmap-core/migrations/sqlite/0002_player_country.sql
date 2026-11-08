-- Keep in step with migrations/postgres. Countries become their own table in Phase 7; until then a slug is enough to
-- decide who sees whom.
ALTER TABLE players ADD COLUMN country TEXT;
CREATE INDEX players_country ON players (country);

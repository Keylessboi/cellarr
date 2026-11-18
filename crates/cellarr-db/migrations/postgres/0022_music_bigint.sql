-- Widen the music tables' integer columns to bigint. Every integer column is
-- read as i64 (see 0001's type mapping), and a Postgres `integer` (int4) does
-- not decode as one, so reading any of these rows failed.
ALTER TABLE music_seq ALTER COLUMN value TYPE BIGINT;
ALTER TABLE artist_meta
    ALTER COLUMN ended TYPE BIGINT,
    ALTER COLUMN metadata_profile_id TYPE BIGINT,
    ALTER COLUMN monitor_new_albums TYPE BIGINT;
ALTER TABLE album_meta ALTER COLUMN any_release_ok TYPE BIGINT;
ALTER TABLE track_meta
    ALTER COLUMN disc TYPE BIGINT,
    ALTER COLUMN position TYPE BIGINT,
    ALTER COLUMN duration_ms TYPE BIGINT;
ALTER TABLE music_history
    ALTER COLUMN album_id TYPE BIGINT,
    ALTER COLUMN artist_id TYPE BIGINT,
    ALTER COLUMN track_id TYPE BIGINT,
    ALTER COLUMN event_type TYPE BIGINT;

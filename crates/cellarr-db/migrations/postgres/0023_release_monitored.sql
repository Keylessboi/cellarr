-- Whether the album's pinned release is monitored. Lidarr keeps a monitored flag per
-- release and an album can have none monitored (then it has no tracks, no statistics
-- and is never wanted). cellarr tracks the pinned release only, so the flag lives here.
ALTER TABLE album_meta ADD COLUMN release_monitored BIGINT NOT NULL DEFAULT 1;

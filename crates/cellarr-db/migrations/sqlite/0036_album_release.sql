-- Every pressing (MusicBrainz release) of an album, not only the one whose track
-- list the album uses. Lidarr-speaking clients list them and pin one: the album's
-- `release_mbid` is the pinned pressing, and `any_release_ok` says whether the
-- pin is a preference (any pressing will do) or a requirement.
--
-- `id` is the integer the Lidarr-compatible API shows; it is kept across refreshes
-- (matched by mbid) and, on a migration, is Lidarr's own id.
CREATE TABLE album_release (
    id                INTEGER PRIMARY KEY NOT NULL,
    album_content_id  TEXT NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    mbid              TEXT NOT NULL,
    title             TEXT NOT NULL,
    status            TEXT,
    release_date      TEXT,
    disambiguation    TEXT,
    -- JSON arrays.
    country           TEXT NOT NULL DEFAULT '[]',
    label             TEXT NOT NULL DEFAULT '[]',
    -- JSON array of {number, name, format}.
    media             TEXT NOT NULL DEFAULT '[]',
    track_count       INTEGER NOT NULL DEFAULT 0,
    duration_ms       INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX idx_album_release_mbid ON album_release(album_content_id, mbid);
CREATE INDEX idx_album_release_album ON album_release(album_content_id);

ALTER TABLE album_meta ADD COLUMN any_release_ok INTEGER NOT NULL DEFAULT 1;

-- Music: the typed identity records behind artist / album / track content nodes.
--
-- The structural tree needs nothing new: an artist, its albums and their tracks
-- are ordinary `content` rows (`kind` artist/album/track), and a track's file is
-- an ordinary `media_file` linked through `content_file`. What music adds is the
-- identity each node carries, and one thing the video types never needed: small
-- **integer ids**.
--
-- The Lidarr-compatible API addresses everything by integer, and its clients
-- persist those integers (a search cursor, a profile id in a config file). So
-- each music record has an `id` that is assigned from a counter, never reused,
-- and preserved verbatim when an existing install is migrated. The content id of
-- a music node is derived from the same number (see `cellarr_core::music::seq_uuid`),
-- which is what keeps the two in step for the code paths that only see a
-- content id.
--
-- Names are stored twice: as written, and normalized (`*_norm`) for matching a
-- release title against the library with an index lookup rather than a scan.

-- One row per sequence, holding the last id handed out. An explicit id (from a
-- migration) raises the counter to at least that value.
CREATE TABLE music_seq (
    name  TEXT PRIMARY KEY NOT NULL,
    value INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE artist_meta (
    id                  INTEGER PRIMARY KEY NOT NULL,
    content_id          TEXT NOT NULL UNIQUE REFERENCES content(id) ON DELETE CASCADE,
    -- MusicBrainz artist id.
    mbid                TEXT NOT NULL,
    name                TEXT NOT NULL,
    name_norm           TEXT NOT NULL,
    sort_name           TEXT,
    disambiguation      TEXT,
    artist_type         TEXT,
    ended               INTEGER NOT NULL DEFAULT 0,
    overview            TEXT,
    -- JSON array of genre names.
    genres              TEXT NOT NULL DEFAULT '[]',
    -- The artist's folder under the library root.
    path                TEXT NOT NULL,
    quality_profile_id  TEXT,
    metadata_profile_id INTEGER NOT NULL DEFAULT 1,
    monitor_new_albums  INTEGER NOT NULL DEFAULT 1,
    added_at            TEXT NOT NULL,
    refreshed_at        TEXT
);
CREATE UNIQUE INDEX idx_artist_meta_mbid ON artist_meta(mbid);
CREATE INDEX idx_artist_meta_name_norm ON artist_meta(name_norm);

-- Alternative spellings of an artist's name (aliases, transliterations), so a
-- release that names the artist differently still matches.
CREATE TABLE artist_alias (
    artist_content_id TEXT NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    alias_norm        TEXT NOT NULL,
    PRIMARY KEY (artist_content_id, alias_norm)
);
CREATE INDEX idx_artist_alias_norm ON artist_alias(alias_norm);

-- An album is a MusicBrainz release group.
CREATE TABLE album_meta (
    id                INTEGER PRIMARY KEY NOT NULL,
    content_id        TEXT NOT NULL UNIQUE REFERENCES content(id) ON DELETE CASCADE,
    artist_content_id TEXT NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    -- MusicBrainz release-group id.
    mbid              TEXT NOT NULL,
    title             TEXT NOT NULL,
    title_norm        TEXT NOT NULL,
    disambiguation    TEXT,
    -- Primary type: Album, EP, Single, Broadcast, Other.
    album_type        TEXT NOT NULL DEFAULT 'Album',
    -- JSON array of secondary types; empty for a studio release.
    secondary_types   TEXT NOT NULL DEFAULT '[]',
    -- First release date, as precise as known (yyyy, yyyy-mm or yyyy-mm-dd).
    release_date      TEXT,
    -- The release whose track list is used, once one has been chosen.
    release_mbid      TEXT,
    added_at          TEXT NOT NULL,
    -- NULL until the track list has been fetched: tracks are materialized
    -- lazily, so an album nobody has asked about costs one row, not fifteen.
    tracks_fetched_at TEXT
);
CREATE UNIQUE INDEX idx_album_meta_mbid ON album_meta(mbid);
CREATE INDEX idx_album_meta_artist ON album_meta(artist_content_id);
CREATE INDEX idx_album_meta_title_norm ON album_meta(title_norm);
CREATE INDEX idx_album_meta_release_date ON album_meta(release_date);

CREATE TABLE track_meta (
    id               INTEGER PRIMARY KEY NOT NULL,
    content_id       TEXT NOT NULL UNIQUE REFERENCES content(id) ON DELETE CASCADE,
    album_content_id TEXT NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    -- MusicBrainz recording id.
    mbid             TEXT,
    title            TEXT NOT NULL,
    disc             INTEGER NOT NULL DEFAULT 1,
    position         INTEGER NOT NULL,
    duration_ms      INTEGER
);
CREATE INDEX idx_track_meta_album ON track_meta(album_content_id);

-- The integer identity of a track's file, and the shortcut from a file to the
-- album and artist it belongs to (so "every file of this artist" is one index
-- lookup rather than a three-level walk).
CREATE TABLE track_file (
    id                INTEGER PRIMARY KEY NOT NULL,
    media_file_id     TEXT NOT NULL UNIQUE REFERENCES media_file(id) ON DELETE CASCADE,
    track_content_id  TEXT NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    album_content_id  TEXT NOT NULL,
    artist_content_id TEXT NOT NULL,
    added_at          TEXT NOT NULL
);
CREATE INDEX idx_track_file_album ON track_file(album_content_id);
CREATE INDEX idx_track_file_artist ON track_file(artist_content_id);
CREATE INDEX idx_track_file_track ON track_file(track_content_id);

-- Which of an artist's release groups are kept (see `MetadataProfile`).
CREATE TABLE metadata_profile (
    id   INTEGER PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    -- JSON-serialized MetadataProfile.
    body TEXT NOT NULL
);

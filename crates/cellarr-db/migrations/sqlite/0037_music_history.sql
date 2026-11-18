-- Music history, in the shape Lidarr-speaking clients read: one row per event,
-- with the release title, the download client's id for the download, the
-- quality and a free-form data object (indexer, client, guid, imported and
-- dropped paths, a failure's message). A migration copies a Lidarr's own rows
-- here with their ids, so a client sees one continuous history across the move;
-- cellarr appends to it from then on.
--
-- `event_type` is Lidarr's numbering: 1 grabbed, 3 trackFileImported,
-- 4 downloadFailed, 5 trackFileDeleted, 6 trackFileRenamed, 7
-- albumImportIncomplete, 8 downloadImported, 9 trackFileRetagged, 10
-- downloadIgnored. `album_id`, `artist_id` and `track_id` are the integer ids the
-- Lidarr-compatible API shows (0 when not applicable).
CREATE TABLE music_history (
    id           INTEGER PRIMARY KEY NOT NULL,
    album_id     INTEGER NOT NULL DEFAULT 0,
    artist_id    INTEGER NOT NULL DEFAULT 0,
    track_id     INTEGER NOT NULL DEFAULT 0,
    source_title TEXT NOT NULL DEFAULT '',
    -- JSON: {"quality": <lidarr id>, "revision": {...}} as Lidarr stores it.
    quality      TEXT NOT NULL DEFAULT '{}',
    date         TEXT NOT NULL,
    event_type   INTEGER NOT NULL,
    data         TEXT NOT NULL DEFAULT '{}',
    download_id  TEXT
);
CREATE INDEX idx_music_history_album ON music_history(album_id, event_type, date);
CREATE INDEX idx_music_history_artist ON music_history(artist_id, date);
CREATE INDEX idx_music_history_download ON music_history(download_id);
CREATE INDEX idx_music_history_date ON music_history(date);

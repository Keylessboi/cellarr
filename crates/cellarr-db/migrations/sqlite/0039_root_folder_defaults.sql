-- What a Lidarr root folder offers a new artist added under it: the defaults the
-- Lidarr-compatible API reports on `/rootfolder` (its quality and metadata
-- profile, monitor options and tags). Keyed by path, since a root folder is a
-- path; the quality profile is a cellarr profile id (the API shows its integer).
CREATE TABLE root_folder_defaults (
    path                     TEXT PRIMARY KEY NOT NULL,
    name                     TEXT NOT NULL DEFAULT '',
    quality_profile_id       TEXT,
    metadata_profile_id      INTEGER NOT NULL DEFAULT 1,
    monitor_option           TEXT NOT NULL DEFAULT 'all',
    new_item_monitor_option  TEXT NOT NULL DEFAULT 'all',
    tags                     TEXT NOT NULL DEFAULT '[]'
);

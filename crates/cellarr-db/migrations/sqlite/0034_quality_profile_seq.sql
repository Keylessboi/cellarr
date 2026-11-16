-- Integer ids for quality profiles.
--
-- cellarr identifies a quality profile by UUID; the Lidarr-compatible API
-- addresses it by small integer, and its clients store that integer (an artist's
-- `qualityProfileId`, a script's config). This table gives each profile that
-- integer the first time it is asked for, keeps it for the profile's life, and
-- lets a migration from an existing Lidarr install claim that install's own ids.
CREATE TABLE quality_profile_seq (
    profile_id TEXT NOT NULL UNIQUE,
    seq        INTEGER PRIMARY KEY NOT NULL
);

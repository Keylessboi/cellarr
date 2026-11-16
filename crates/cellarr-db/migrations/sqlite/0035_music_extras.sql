-- Rich metadata a source supplies beyond the identity columns: artwork, links,
-- a rating, a biography. One JSON document per artist or album, replaced whenever
-- the node is refreshed. Kept off the identity tables so listing a large library
-- does not drag every link list through every query.
CREATE TABLE music_extras (
    content_id TEXT PRIMARY KEY NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    body       TEXT NOT NULL
);

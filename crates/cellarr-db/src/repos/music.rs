//! The music repository: artists, albums, tracks, track files and metadata
//! profiles.
//!
//! Music's structural nodes are ordinary `content` rows; this repository owns the
//! typed identity beside them (`artist_meta`, `album_meta`, `track_meta`,
//! `track_file`) and writes a node and its identity together, in one
//! transaction, so neither can exist without the other.
//!
//! It is built for a large library on small hardware. Listing every artist with
//! its statistics is a fixed number of grouped queries, not one per artist;
//! tracks are only stored for albums that have been asked about; and matching a
//! release title against the library is an index lookup on a normalized name.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cellarr_core::music::{normalize_name, seq_uuid};
use cellarr_core::{
    Album, Artist, ContentId, LibraryId, MediaFile, MediaFileId, MetadataProfile, Quality,
    QualityProfileId, Track, TrackFile,
};
use sqlx::Row;

use crate::convert::{format_time, parse_uuid};
use crate::dialect::{pq, DbConnection, DbPool, DbRow};
use crate::error::{DbError, Result};
use crate::writer::WriterHandle;

/// What is needed to add an artist.
#[derive(Debug, Clone, Default)]
pub struct NewArtist {
    /// A preset sequence id (a migration preserving the source's id); `None`
    /// takes the next one.
    pub id: Option<i64>,
    /// MusicBrainz artist id.
    pub mbid: String,
    /// Display name.
    pub name: String,
    /// Sort name.
    pub sort_name: Option<String>,
    /// Disambiguation comment.
    pub disambiguation: Option<String>,
    /// MusicBrainz artist type.
    pub artist_type: Option<String>,
    /// Whether the artist has ended.
    pub ended: bool,
    /// Biography.
    pub overview: Option<String>,
    /// Genre names.
    pub genres: Vec<String>,
    /// Alternative names to match releases against.
    pub aliases: Vec<String>,
    /// The artist's folder.
    pub path: String,
    /// Quality profile for the artist's albums.
    pub quality_profile_id: Option<QualityProfileId>,
    /// Metadata profile id.
    pub metadata_profile_id: i64,
    /// Whether the artist starts monitored.
    pub monitored: bool,
    /// Whether later-discovered albums start monitored.
    pub monitor_new_albums: bool,
    /// When the artist was added; `None` means now.
    pub added_at: Option<String>,
}

/// What is needed to add (or refresh) an album.
#[derive(Debug, Clone, Default)]
pub struct NewAlbum {
    /// A preset sequence id; `None` takes the next one.
    pub id: Option<i64>,
    /// MusicBrainz release-group id.
    pub mbid: String,
    /// Title.
    pub title: String,
    /// Disambiguation comment.
    pub disambiguation: Option<String>,
    /// Primary type.
    pub album_type: String,
    /// Secondary types.
    pub secondary_types: Vec<String>,
    /// First release date.
    pub release_date: Option<String>,
    /// The release whose track list is used, when already chosen.
    pub release_mbid: Option<String>,
    /// Whether a newly inserted album starts monitored. Ignored for an album
    /// that already exists: a refresh never overrides the user's choice.
    pub monitored: bool,
    /// When the album was added; `None` means now.
    pub added_at: Option<String>,
}

/// What is needed to add a track.
#[derive(Debug, Clone, Default)]
pub struct NewTrack {
    /// A preset sequence id; `None` takes the next one.
    pub id: Option<i64>,
    /// MusicBrainz recording id.
    pub mbid: Option<String>,
    /// Title.
    pub title: String,
    /// Disc number.
    pub disc: u32,
    /// Position on the disc.
    pub position: u32,
    /// Length in milliseconds.
    pub duration_ms: Option<u32>,
}

/// File statistics for an artist or an album.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MusicStats {
    /// Albums under the artist (1 for an album's own statistics).
    pub album_count: i64,
    /// Tracks known (only albums whose track list has been fetched contribute).
    pub track_count: i64,
    /// Tracks that have a file.
    pub track_file_count: i64,
    /// Total size of those files, in bytes.
    pub size_on_disk: i64,
}

/// An album together with the artist name it is released under, the pair a
/// release title is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumWithArtist {
    /// The album.
    pub album: Album,
    /// Its artist's display name.
    pub artist_name: String,
}

/// How a wanted-album listing is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumSort {
    /// By sequence id.
    Id,
    /// By release date.
    ReleaseDate,
    /// By title.
    Title,
}

/// Reads/writes for the music tables.
#[derive(Clone)]
pub struct MusicRepo {
    pool: DbPool,
    writer: WriterHandle,
}

const ARTIST_COLS: &str = "a.id, a.content_id, a.mbid, a.name, a.sort_name, a.disambiguation, \
     a.artist_type, a.ended, a.overview, a.genres, a.path, a.quality_profile_id, \
     a.metadata_profile_id, a.monitor_new_albums, a.added_at, a.refreshed_at, c.monitored, \
     (SELECT x.body FROM music_extras x WHERE x.content_id = a.content_id) AS extras";

const ALBUM_COLS: &str = "al.id, al.content_id, al.artist_content_id, al.mbid, al.title, \
     al.disambiguation, al.album_type, al.secondary_types, al.release_date, al.release_mbid, \
     al.added_at, al.tracks_fetched_at, al.any_release_ok, c.monitored";

const TRACK_COLS: &str =
    "t.id, t.content_id, t.album_content_id, t.mbid, t.title, t.disc, t.position, t.duration_ms";

const TRACK_FILE_COLS: &str = "tf.id AS tf_id, tf.track_content_id, tf.album_content_id, \
     tf.artist_content_id, tf.added_at, mf.id AS mf_id, mf.path, mf.size, mf.quality, \
     mf.languages, mf.media_info, mf.custom_format_score";

impl MusicRepo {
    pub(crate) fn new(pool: DbPool, writer: WriterHandle) -> Self {
        Self { pool, writer }
    }

    /// Run a write in the writer's transaction and hand its result back.
    async fn write<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(
                &'c mut DbConnection,
            ) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'c>>
            + Send
            + 'static,
    {
        let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let out = slot.clone();
        self.writer
            .submit(move |conn| {
                Box::pin(async move {
                    let value = job(conn).await?;
                    if let Ok(mut guard) = out.lock() {
                        *guard = Some(value);
                    }
                    Ok(())
                })
            })
            .await?;
        let value = slot.lock().ok().and_then(|mut g| g.take());
        value.ok_or(DbError::Decode {
            column: "write result",
            detail: "the write completed without producing its result".to_string(),
        })
    }

    // --- artists -------------------------------------------------------------

    /// Add an artist: its content node, identity, aliases and search title, in
    /// one transaction.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure, including a duplicate MusicBrainz
    /// id (an artist is added once).
    pub async fn add_artist(&self, library: LibraryId, new: NewArtist) -> Result<Artist> {
        let now = format_time(time::OffsetDateTime::now_utc())?;
        self.write(move |conn| {
            Box::pin(async move {
                let id = next_seq(conn, "artist", new.id).await?;
                let content_id = ContentId::from_uuid(seq_uuid(seq_u32(id)));
                let added_at = new.added_at.clone().unwrap_or(now);
                insert_node(conn, content_id, library, None, "artist", "{\"type\":\"album\"}", new.monitored)
                    .await?;
                sqlx::query(pq(
                    "INSERT INTO artist_meta
                        (id, content_id, mbid, name, name_norm, sort_name, disambiguation,
                         artist_type, ended, overview, genres, path, quality_profile_id,
                         metadata_profile_id, monitor_new_albums, added_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                ))
                .bind(id)
                .bind(content_id.to_string())
                .bind(&new.mbid)
                .bind(&new.name)
                .bind(normalize_name(&new.name))
                .bind(&new.sort_name)
                .bind(&new.disambiguation)
                .bind(&new.artist_type)
                .bind(i64::from(new.ended))
                .bind(&new.overview)
                .bind(serde_json::to_string(&new.genres)?)
                .bind(&new.path)
                .bind(new.quality_profile_id.map(|p| p.to_string()))
                .bind(new.metadata_profile_id)
                .bind(i64::from(new.monitor_new_albums))
                .bind(&added_at)
                .execute(&mut *conn)
                .await?;
                write_aliases(conn, content_id, &new.aliases).await?;
                index_new_title(conn, content_id, &new.name).await?;
                Ok(Artist {
                    id,
                    content_id,
                    mbid: new.mbid,
                    name: new.name,
                    sort_name: new.sort_name,
                    disambiguation: new.disambiguation,
                    artist_type: new.artist_type,
                    ended: new.ended,
                    overview: new.overview,
                    genres: new.genres,
                    path: new.path,
                    quality_profile_id: new.quality_profile_id,
                    metadata_profile_id: new.metadata_profile_id,
                    monitored: new.monitored,
                    monitor_new_albums: new.monitor_new_albums,
                    added_at,
                    refreshed_at: None,
                    extras: None,
                })
            })
        })
        .await
    }

    /// Persist an artist's editable fields (profiles, path, monitoring, and the
    /// metadata a refresh may have corrected).
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn update_artist(&self, artist: &Artist, aliases: Option<Vec<String>>) -> Result<()> {
        let a = artist.clone();
        self.writer
            .submit(move |conn| {
                Box::pin(async move {
                    sqlx::query(pq(
                        "UPDATE artist_meta SET name = ?2, name_norm = ?3, sort_name = ?4,
                            disambiguation = ?5, artist_type = ?6, ended = ?7, overview = ?8,
                            genres = ?9, path = ?10, quality_profile_id = ?11,
                            metadata_profile_id = ?12, monitor_new_albums = ?13
                         WHERE content_id = ?1",
                    ))
                    .bind(a.content_id.to_string())
                    .bind(&a.name)
                    .bind(normalize_name(&a.name))
                    .bind(&a.sort_name)
                    .bind(&a.disambiguation)
                    .bind(&a.artist_type)
                    .bind(i64::from(a.ended))
                    .bind(&a.overview)
                    .bind(serde_json::to_string(&a.genres)?)
                    .bind(&a.path)
                    .bind(a.quality_profile_id.map(|p| p.to_string()))
                    .bind(a.metadata_profile_id)
                    .bind(i64::from(a.monitor_new_albums))
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(pq("UPDATE content SET monitored = ?2 WHERE id = ?1"))
                        .bind(a.content_id.to_string())
                        .bind(i64::from(a.monitored))
                        .execute(&mut *conn)
                        .await?;
                    if let Some(aliases) = aliases {
                        sqlx::query(pq("DELETE FROM artist_alias WHERE artist_content_id = ?1"))
                            .bind(a.content_id.to_string())
                            .execute(&mut *conn)
                            .await?;
                        write_aliases(conn, a.content_id, &aliases).await?;
                    }
                    index_title(conn, a.content_id, &a.name).await?;
                    Ok(())
                })
            })
            .await
    }

    /// Stamp an artist's album list as refreshed now.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn mark_artist_refreshed(&self, artist: ContentId) -> Result<()> {
        let now = format_time(time::OffsetDateTime::now_utc())?;
        self.writer
            .submit(move |conn| {
                Box::pin(async move {
                    sqlx::query(pq(
                        "UPDATE artist_meta SET refreshed_at = ?2 WHERE content_id = ?1",
                    ))
                    .bind(artist.to_string())
                    .bind(now)
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
    }

    /// Artists whose album list is stalest, least-recently-refreshed first
    /// (never-refreshed ahead of all).
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artists_due_for_refresh(&self, before: &str, limit: usize) -> Result<Vec<Artist>> {
        let sql = format!(
            "SELECT {ARTIST_COLS} FROM artist_meta a JOIN content c ON c.id = a.content_id
             WHERE a.refreshed_at IS NULL OR a.refreshed_at < ?1
             ORDER BY (a.refreshed_at IS NULL) DESC, a.refreshed_at ASC LIMIT ?2"
        );
        let rows = sqlx::query(pq(&sql))
            .bind(before)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_artist).collect()
    }

    /// Every artist, by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artists(&self) -> Result<Vec<Artist>> {
        let sql = format!(
            "SELECT {ARTIST_COLS} FROM artist_meta a JOIN content c ON c.id = a.content_id
             ORDER BY a.id ASC"
        );
        let rows = sqlx::query(pq(&sql)).fetch_all(&self.pool).await?;
        rows.into_iter().map(row_to_artist).collect()
    }

    /// One artist by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist(&self, id: i64) -> Result<Option<Artist>> {
        self.artist_where("a.id = ?1", ArtistKey::Seq(id)).await
    }

    /// One artist by content id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist_by_content(&self, id: ContentId) -> Result<Option<Artist>> {
        self.artist_where("a.content_id = ?1", ArtistKey::Text(id.to_string()))
            .await
    }

    /// One artist by MusicBrainz id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist_by_mbid(&self, mbid: &str) -> Result<Option<Artist>> {
        self.artist_where("a.mbid = ?1", ArtistKey::Text(mbid.to_string()))
            .await
    }

    async fn artist_where(&self, predicate: &str, key: ArtistKey) -> Result<Option<Artist>> {
        let sql = format!(
            "SELECT {ARTIST_COLS} FROM artist_meta a JOIN content c ON c.id = a.content_id
             WHERE {predicate}"
        );
        let sql = pq(&sql);
        let query = sqlx::query(sql);
        let query = match key {
            ArtistKey::Seq(n) => query.bind(n),
            ArtistKey::Text(s) => query.bind(s),
        };
        query
            .fetch_optional(&self.pool)
            .await?
            .map(row_to_artist)
            .transpose()
    }

    /// Delete an artist with everything under it. Returns the paths of the
    /// files that were tracked, so the caller can decide what to do on disk.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn delete_artist(&self, artist: ContentId) -> Result<Vec<String>> {
        self.delete_scope("artist_content_id", artist).await
    }

    /// Delete an album with its tracks. Returns the tracked file paths.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn delete_album(&self, album: ContentId) -> Result<Vec<String>> {
        self.delete_scope("album_content_id", album).await
    }

    async fn delete_scope(&self, column: &'static str, id: ContentId) -> Result<Vec<String>> {
        self.write(move |conn| {
            Box::pin(async move {
                let rows = sqlx::query(pq(&format!(
                    "SELECT mf.path FROM track_file tf JOIN media_file mf ON mf.id = tf.media_file_id
                     WHERE tf.{column} = ?1"
                )))
                .bind(id.to_string())
                .fetch_all(&mut *conn)
                .await?;
                let paths = rows
                    .into_iter()
                    .map(|r| r.try_get::<String, _>("path").map_err(DbError::from))
                    .collect::<Result<Vec<_>>>()?;
                sqlx::query(pq(&format!(
                    "DELETE FROM media_file WHERE id IN
                        (SELECT media_file_id FROM track_file WHERE {column} = ?1)"
                )))
                .bind(id.to_string())
                .execute(&mut *conn)
                .await?;
                // The subtree's FTS rows have no foreign key to cascade through.
                sqlx::query(pq(
                    "DELETE FROM content_fts WHERE content_id IN (
                        SELECT id FROM content WHERE id = ?1 OR parent_id = ?1
                           OR parent_id IN (SELECT id FROM content WHERE parent_id = ?1))",
                ))
                .bind(id.to_string())
                .execute(&mut *conn)
                .await?;
                sqlx::query(pq("DELETE FROM content WHERE id = ?1"))
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(paths)
            })
        })
        .await
    }

    // --- albums --------------------------------------------------------------

    /// Insert the albums that are new for an artist and refresh the metadata of
    /// those already present (matched by MusicBrainz id). A refresh never
    /// changes an existing album's monitored flag. Returns the albums that were
    /// newly inserted.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn upsert_albums(
        &self,
        library: LibraryId,
        artist: ContentId,
        albums: Vec<NewAlbum>,
    ) -> Result<Vec<Album>> {
        let now = format_time(time::OffsetDateTime::now_utc())?;
        self.write(move |conn| {
            Box::pin(async move {
                let mut inserted = Vec::new();
                for new in albums {
                    let existing = sqlx::query(pq(
                        "SELECT content_id, title FROM album_meta WHERE mbid = ?1",
                    ))
                    .bind(&new.mbid)
                    .fetch_optional(&mut *conn)
                    .await?;
                    if let Some(row) = existing {
                        let content_id: String = row.try_get("content_id")?;
                        let old_title: String = row.try_get("title")?;
                        sqlx::query(pq("UPDATE album_meta SET title = ?2, title_norm = ?3,
                                disambiguation = ?4, album_type = ?5, secondary_types = ?6,
                                release_date = ?7
                             WHERE content_id = ?1"))
                        .bind(&content_id)
                        .bind(&new.title)
                        .bind(normalize_name(&new.title))
                        .bind(&new.disambiguation)
                        .bind(&new.album_type)
                        .bind(serde_json::to_string(&new.secondary_types)?)
                        .bind(&new.release_date)
                        .execute(&mut *conn)
                        .await?;
                        let cid = ContentId::from_uuid(parse_uuid("content_id", &content_id)?);
                        if old_title != new.title {
                            index_title(conn, cid, &new.title).await?;
                        }
                        write_content_meta(conn, cid, &new.title, new.release_date.as_deref())
                            .await?;
                        continue;
                    }
                    let id = next_seq(conn, "album", new.id).await?;
                    let content_id = ContentId::from_uuid(seq_uuid(seq_u32(id)));
                    let added_at = new.added_at.clone().unwrap_or_else(|| now.clone());
                    insert_node(
                        conn,
                        content_id,
                        library,
                        Some(artist),
                        "album",
                        "{\"type\":\"album\"}",
                        new.monitored,
                    )
                    .await?;
                    sqlx::query(pq("INSERT INTO album_meta
                            (id, content_id, artist_content_id, mbid, title, title_norm,
                             disambiguation, album_type, secondary_types, release_date,
                             release_mbid, added_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"))
                    .bind(id)
                    .bind(content_id.to_string())
                    .bind(artist.to_string())
                    .bind(&new.mbid)
                    .bind(&new.title)
                    .bind(normalize_name(&new.title))
                    .bind(&new.disambiguation)
                    .bind(&new.album_type)
                    .bind(serde_json::to_string(&new.secondary_types)?)
                    .bind(&new.release_date)
                    .bind(&new.release_mbid)
                    .bind(&added_at)
                    .execute(&mut *conn)
                    .await?;
                    index_new_title(conn, content_id, &new.title).await?;
                    write_content_meta(conn, content_id, &new.title, new.release_date.as_deref())
                        .await?;
                    inserted.push(Album {
                        id,
                        content_id,
                        artist_content_id: artist,
                        mbid: new.mbid,
                        title: new.title,
                        disambiguation: new.disambiguation,
                        album_type: new.album_type,
                        secondary_types: new.secondary_types,
                        release_date: new.release_date,
                        monitored: new.monitored,
                        release_mbid: new.release_mbid,
                        added_at,
                        tracks_fetched_at: None,
                        any_release_ok: true,
                    });
                }
                Ok(inserted)
            })
        })
        .await
    }

    /// Every album, by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn albums(&self) -> Result<Vec<Album>> {
        self.albums_where("1 = 1", None).await
    }

    /// An artist's albums, by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn albums_for_artist(&self, artist: ContentId) -> Result<Vec<Album>> {
        self.albums_where("al.artist_content_id = ?1", Some(artist.to_string()))
            .await
    }

    async fn albums_where(&self, predicate: &str, bind: Option<String>) -> Result<Vec<Album>> {
        let sql = format!(
            "SELECT {ALBUM_COLS} FROM album_meta al JOIN content c ON c.id = al.content_id
             WHERE {predicate} ORDER BY al.id ASC"
        );
        let sql = pq(&sql);
        let mut query = sqlx::query(sql);
        if let Some(value) = bind {
            query = query.bind(value);
        }
        let rows = query.fetch_all(&self.pool).await?;
        rows.into_iter().map(row_to_album).collect()
    }

    /// One album by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn album(&self, id: i64) -> Result<Option<Album>> {
        let sql = format!(
            "SELECT {ALBUM_COLS} FROM album_meta al JOIN content c ON c.id = al.content_id
             WHERE al.id = ?1"
        );
        sqlx::query(pq(&sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(row_to_album)
            .transpose()
    }

    /// One album by content id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn album_by_content(&self, id: ContentId) -> Result<Option<Album>> {
        Ok(self
            .albums_where("al.content_id = ?1", Some(id.to_string()))
            .await?
            .into_iter()
            .next())
    }

    /// One album by MusicBrainz release-group id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn album_by_mbid(&self, mbid: &str) -> Result<Option<Album>> {
        Ok(self
            .albums_where("al.mbid = ?1", Some(mbid.to_string()))
            .await?
            .into_iter()
            .next())
    }

    /// Albums whose title and artist both match the given normalized names. The
    /// artist is matched on its name or any alias.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn albums_named(
        &self,
        artist_norm: &str,
        album_norms: &[String],
    ) -> Result<Vec<AlbumWithArtist>> {
        let mut out = Vec::new();
        for album_norm in album_norms {
            let sql = format!(
                "SELECT {ALBUM_COLS}, ar.name AS artist_name
                 FROM album_meta al
                 JOIN content c ON c.id = al.content_id
                 JOIN artist_meta ar ON ar.content_id = al.artist_content_id
                 WHERE al.title_norm = ?1
                   AND (ar.name_norm = ?2 OR EXISTS (
                        SELECT 1 FROM artist_alias x
                        WHERE x.artist_content_id = ar.content_id AND x.alias_norm = ?2))"
            );
            let rows = sqlx::query(pq(&sql))
                .bind(album_norm)
                .bind(artist_norm)
                .fetch_all(&self.pool)
                .await?;
            for row in rows {
                let artist_name: String = row.try_get("artist_name")?;
                out.push(AlbumWithArtist {
                    album: row_to_album(row)?,
                    artist_name,
                });
            }
        }
        Ok(out)
    }

    /// Albums for which `"<artist> <album>"`, normalized, equals `blob` — the
    /// match for a title that names both without a separator between them.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn albums_named_jointly(&self, blob: &str) -> Result<Vec<AlbumWithArtist>> {
        // Narrow by the artist first: only artists whose name the blob begins
        // with can match, and that is an indexed handful, not the album table.
        let words: Vec<&str> = blob.split(' ').collect();
        let mut out = Vec::new();
        for split in 1..words.len() {
            let artist_norm = words[..split].join(" ");
            let album_norm = words[split..].join(" ");
            out.extend(self.albums_named(&artist_norm, &[album_norm]).await?);
        }
        Ok(out)
    }

    /// Monitored albums of monitored artists that have no file at all and whose
    /// release date is not in the future, one page of them plus the total.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn missing_albums(
        &self,
        today: &str,
        sort: AlbumSort,
        ascending: bool,
        offset: u64,
        limit: u64,
    ) -> Result<(u64, Vec<Album>)> {
        const WHERE: &str = "c.monitored = 1 AND ac.monitored = 1
               AND (al.release_date IS NULL OR al.release_date <= ?1)
               AND NOT EXISTS (
                   SELECT 1 FROM track_file tf WHERE tf.album_content_id = al.content_id)";
        let total: i64 = sqlx::query(pq(&format!(
            "SELECT count(*) AS n FROM album_meta al
             JOIN content c ON c.id = al.content_id
             JOIN content ac ON ac.id = al.artist_content_id
             WHERE {WHERE}"
        )))
        .bind(today)
        .fetch_one(&self.pool)
        .await?
        .try_get("n")?;
        let column = match sort {
            AlbumSort::Id => "al.id",
            AlbumSort::ReleaseDate => "al.release_date",
            AlbumSort::Title => "al.title_norm",
        };
        let direction = if ascending { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT {ALBUM_COLS} FROM album_meta al
             JOIN content c ON c.id = al.content_id
             JOIN content ac ON ac.id = al.artist_content_id
             WHERE {WHERE}
             ORDER BY {column} {direction}, al.id ASC LIMIT ?2 OFFSET ?3"
        );
        let rows = sqlx::query(pq(&sql))
            .bind(today)
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .bind(i64::try_from(offset).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await?;
        let albums = rows.into_iter().map(row_to_album).collect::<Result<_>>()?;
        Ok((u64::try_from(total).unwrap_or(0), albums))
    }

    /// Set the monitored flag on a set of nodes (artists, albums or tracks).
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn set_monitored(&self, ids: Vec<ContentId>, monitored: bool) -> Result<()> {
        self.writer
            .submit(move |conn| {
                Box::pin(async move {
                    for id in ids {
                        sqlx::query(pq("UPDATE content SET monitored = ?2 WHERE id = ?1"))
                            .bind(id.to_string())
                            .bind(i64::from(monitored))
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                })
            })
            .await
    }

    // --- tracks --------------------------------------------------------------

    /// Store an album's track list, taken from the release `release_mbid`.
    ///
    /// Tracks are matched to existing ones by disc and position, so re-fetching
    /// keeps every track's id and file. A track that is no longer on the list is
    /// removed unless it has a file, in which case it is kept: a file is never
    /// orphaned by a metadata change.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn set_album_tracks(
        &self,
        library: LibraryId,
        album: ContentId,
        release_mbid: Option<String>,
        tracks: Vec<NewTrack>,
    ) -> Result<Vec<Track>> {
        let now = format_time(time::OffsetDateTime::now_utc())?;
        self.write(move |conn| {
            Box::pin(
                async move { store_tracks(conn, library, album, release_mbid, tracks, &now).await },
            )
        })
        .await
    }

    /// Store the track lists of many albums in one transaction. A migration uses
    /// this: a large library is tens of thousands of albums, and one transaction
    /// per album is the difference between minutes and an hour.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure; nothing is stored then.
    pub async fn set_many_album_tracks(
        &self,
        library: LibraryId,
        albums: Vec<(ContentId, Option<String>, Vec<NewTrack>)>,
    ) -> Result<usize> {
        let now = format_time(time::OffsetDateTime::now_utc())?;
        self.write(move |conn| {
            Box::pin(async move {
                let mut stored = 0usize;
                for (album, release, tracks) in albums {
                    stored += store_tracks(conn, library, album, release, tracks, &now)
                        .await?
                        .len();
                }
                Ok(stored)
            })
        })
        .await
    }

    /// An album's tracks, in disc and position order.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn tracks_for_album(&self, album: ContentId) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {TRACK_COLS} FROM track_meta t WHERE t.album_content_id = ?1
             ORDER BY t.disc ASC, t.position ASC"
        );
        let rows = sqlx::query(pq(&sql))
            .bind(album.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_track).collect()
    }

    /// Every stored track of an artist.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn tracks_for_artist(&self, artist: ContentId) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {TRACK_COLS} FROM track_meta t
             JOIN album_meta al ON al.content_id = t.album_content_id
             WHERE al.artist_content_id = ?1
             ORDER BY al.id ASC, t.disc ASC, t.position ASC"
        );
        let rows = sqlx::query(pq(&sql))
            .bind(artist.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_track).collect()
    }

    /// One track by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn track(&self, id: i64) -> Result<Option<Track>> {
        let sql = format!("SELECT {TRACK_COLS} FROM track_meta t WHERE t.id = ?1");
        sqlx::query(pq(&sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(row_to_track)
            .transpose()
    }

    /// One track by content id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn track_by_content(&self, id: ContentId) -> Result<Option<Track>> {
        let sql = format!("SELECT {TRACK_COLS} FROM track_meta t WHERE t.content_id = ?1");
        sqlx::query(pq(&sql))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .map(row_to_track)
            .transpose()
    }

    // --- track files ---------------------------------------------------------

    /// Record that `file` satisfies `track`: create the media file if it is new,
    /// link it to the track **and to the track's album** (so the album, which is
    /// the unit that is searched and upgraded, knows what it holds), and give it
    /// its sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure, or if the track is unknown.
    pub async fn record_track_file(
        &self,
        track: ContentId,
        file: MediaFile,
        preset_id: Option<i64>,
        added_at: Option<String>,
    ) -> Result<i64> {
        let now = format_time(time::OffsetDateTime::now_utc())?;
        self.write(move |conn| {
            Box::pin(async move {
                let owner = sqlx::query(pq("SELECT t.album_content_id, al.artist_content_id
                     FROM track_meta t JOIN album_meta al ON al.content_id = t.album_content_id
                     WHERE t.content_id = ?1"))
                .bind(track.to_string())
                .fetch_optional(&mut *conn)
                .await?
                .ok_or(DbError::Decode {
                    column: "track_content_id",
                    detail: format!("no track {track}"),
                })?;
                let album: String = owner.try_get("album_content_id")?;
                let artist: String = owner.try_get("artist_content_id")?;

                let existing = sqlx::query(pq(
                    "SELECT mf.id AS mf_id, tf.id AS tf_id FROM media_file mf
                     LEFT JOIN track_file tf ON tf.media_file_id = mf.id
                     WHERE mf.path = ?1",
                ))
                .bind(&file.path)
                .fetch_optional(&mut *conn)
                .await?;
                let (media_file_id, existing_seq) = match existing {
                    Some(row) => (
                        row.try_get::<String, _>("mf_id")?,
                        row.try_get::<Option<i64>, _>("tf_id")?,
                    ),
                    None => (String::new(), None),
                };
                let id = match existing_seq {
                    Some(id) => id,
                    None => next_seq(conn, "track_file", preset_id).await?,
                };
                let media_file_id = if media_file_id.is_empty() {
                    let mf = MediaFileId::from_uuid(seq_uuid(seq_u32(id))).to_string();
                    sqlx::query(pq("INSERT INTO media_file
                            (id, path, size, languages, quality, media_info, quality_rank,
                             custom_format_score)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"))
                    .bind(&mf)
                    .bind(&file.path)
                    .bind(i64::try_from(file.size).unwrap_or(i64::MAX))
                    .bind(serde_json::to_string(&file.languages)?)
                    .bind(serde_json::to_string(&file.quality)?)
                    .bind(
                        file.media_info
                            .as_ref()
                            .map(serde_json::to_string)
                            .transpose()?,
                    )
                    .bind(i64::from(file.quality.rank))
                    .bind(i64::from(file.custom_format_score.unwrap_or(0)))
                    .execute(&mut *conn)
                    .await?;
                    mf
                } else {
                    media_file_id
                };
                sqlx::query(pq("INSERT INTO track_file
                        (id, media_file_id, track_content_id, album_content_id,
                         artist_content_id, added_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(media_file_id) DO UPDATE SET
                        track_content_id = excluded.track_content_id,
                        album_content_id = excluded.album_content_id,
                        artist_content_id = excluded.artist_content_id"))
                .bind(id)
                .bind(&media_file_id)
                .bind(track.to_string())
                .bind(&album)
                .bind(&artist)
                .bind(added_at.unwrap_or(now))
                .execute(&mut *conn)
                .await?;
                for owner in [track.to_string(), album] {
                    sqlx::query(pq(
                        "INSERT INTO content_file (content_id, media_file_id) VALUES (?1, ?2)
                         ON CONFLICT DO NOTHING",
                    ))
                    .bind(owner)
                    .bind(&media_file_id)
                    .execute(&mut *conn)
                    .await?;
                }
                Ok(id)
            })
        })
        .await
    }

    /// The files a track holds at any path other than `keep_path`: what an
    /// upgrade supersedes once the better file is in place.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn other_track_files(
        &self,
        track: ContentId,
        keep_path: &str,
    ) -> Result<Vec<(i64, String)>> {
        let rows = sqlx::query(pq("SELECT tf.id AS id, mf.path AS path FROM track_file tf
             JOIN media_file mf ON mf.id = tf.media_file_id
             WHERE tf.track_content_id = ?1 AND mf.path <> ?2"))
        .bind(track.to_string())
        .bind(keep_path)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| Ok((r.try_get("id")?, r.try_get("path")?)))
            .collect()
    }

    /// An album's track files.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn track_files_for_album(&self, album: ContentId) -> Result<Vec<TrackFile>> {
        self.track_files_where(
            "tf.album_content_id = ?1",
            TrackFileKey::Text(album.to_string()),
        )
        .await
    }

    /// An artist's track files.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn track_files_for_artist(&self, artist: ContentId) -> Result<Vec<TrackFile>> {
        self.track_files_where(
            "tf.artist_content_id = ?1",
            TrackFileKey::Text(artist.to_string()),
        )
        .await
    }

    /// One track file by sequence id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn track_file(&self, id: i64) -> Result<Option<TrackFile>> {
        Ok(self
            .track_files_where("tf.id = ?1", TrackFileKey::Seq(id))
            .await?
            .into_iter()
            .next())
    }

    async fn track_files_where(
        &self,
        predicate: &str,
        key: TrackFileKey,
    ) -> Result<Vec<TrackFile>> {
        let sql = format!(
            "SELECT {TRACK_FILE_COLS} FROM track_file tf
             JOIN media_file mf ON mf.id = tf.media_file_id
             WHERE {predicate} ORDER BY tf.id ASC"
        );
        let sql = pq(&sql);
        let query = sqlx::query(sql);
        let query = match key {
            TrackFileKey::Seq(n) => query.bind(n),
            TrackFileKey::Text(s) => query.bind(s),
        };
        let rows = query.fetch_all(&self.pool).await?;
        rows.into_iter().map(row_to_track_file).collect()
    }

    /// Forget a track file (the row, its links, and its media file). Returns
    /// its path so the caller can remove or recycle the file itself.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn delete_track_file(&self, id: i64) -> Result<Option<String>> {
        self.write(move |conn| {
            Box::pin(async move {
                let row = sqlx::query(pq("SELECT mf.id AS mf_id, mf.path FROM track_file tf
                     JOIN media_file mf ON mf.id = tf.media_file_id WHERE tf.id = ?1"))
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
                let Some(row) = row else {
                    return Ok(None);
                };
                let mf_id: String = row.try_get("mf_id")?;
                let path: String = row.try_get("path")?;
                sqlx::query(pq("DELETE FROM media_file WHERE id = ?1"))
                    .bind(mf_id)
                    .execute(&mut *conn)
                    .await?;
                Ok(Some(path))
            })
        })
        .await
    }

    // --- statistics ----------------------------------------------------------

    /// File statistics for every artist, in three grouped queries.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist_stats(&self) -> Result<HashMap<ContentId, MusicStats>> {
        let mut out: HashMap<ContentId, MusicStats> = HashMap::new();
        for row in sqlx::query(pq(
            "SELECT artist_content_id AS k, count(*) AS n FROM album_meta GROUP BY artist_content_id",
        ))
        .fetch_all(&self.pool)
        .await?
        {
            out.entry(key_of(&row)?).or_default().album_count = row.try_get("n")?;
        }
        for row in sqlx::query(pq("SELECT al.artist_content_id AS k, count(*) AS n
             FROM track_meta t JOIN album_meta al ON al.content_id = t.album_content_id
             GROUP BY al.artist_content_id"))
        .fetch_all(&self.pool)
        .await?
        {
            out.entry(key_of(&row)?).or_default().track_count = row.try_get("n")?;
        }
        for row in sqlx::query(pq(
            "SELECT tf.artist_content_id AS k, count(*) AS n, COALESCE(sum(mf.size), 0) AS bytes
             FROM track_file tf JOIN media_file mf ON mf.id = tf.media_file_id
             GROUP BY tf.artist_content_id",
        ))
        .fetch_all(&self.pool)
        .await?
        {
            let entry = out.entry(key_of(&row)?).or_default();
            entry.track_file_count = row.try_get("n")?;
            entry.size_on_disk = row.try_get("bytes")?;
        }
        Ok(out)
    }

    /// File statistics for the named artists only, by indexed lookups: the cost
    /// of one artist's statistics must not be the cost of the whole library's.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist_stats_for(
        &self,
        artists: &[ContentId],
    ) -> Result<HashMap<ContentId, MusicStats>> {
        let mut out: HashMap<ContentId, MusicStats> = HashMap::new();
        for chunk in artists.chunks(400) {
            let marks = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let queries = [
                format!("SELECT artist_content_id AS k, count(*) AS n FROM album_meta WHERE artist_content_id IN ({marks}) GROUP BY artist_content_id"),
                format!("SELECT al.artist_content_id AS k, count(*) AS n FROM track_meta t JOIN album_meta al ON al.content_id = t.album_content_id WHERE al.artist_content_id IN ({marks}) GROUP BY al.artist_content_id"),
                format!("SELECT tf.artist_content_id AS k, count(*) AS n, COALESCE(sum(mf.size), 0) AS bytes FROM track_file tf JOIN media_file mf ON mf.id = tf.media_file_id WHERE tf.artist_content_id IN ({marks}) GROUP BY tf.artist_content_id"),
            ];
            for (i, sql) in queries.iter().enumerate() {
                let sql = pq(sql);
                let mut q = sqlx::query(sql);
                for id in chunk {
                    q = q.bind(id.to_string());
                }
                for row in q.fetch_all(&self.pool).await? {
                    let e = out.entry(key_of(&row)?).or_default();
                    match i {
                        0 => e.album_count = row.try_get("n")?,
                        1 => e.track_count = row.try_get("n")?,
                        _ => {
                            e.track_file_count = row.try_get("n")?;
                            e.size_on_disk = row.try_get("bytes")?;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// File statistics per album: for every album, or only one artist's.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn album_stats(
        &self,
        artist: Option<ContentId>,
    ) -> Result<HashMap<ContentId, MusicStats>> {
        let mut out: HashMap<ContentId, MusicStats> = HashMap::new();
        let (track_sql, file_sql) = match artist {
            Some(_) => (
                "SELECT t.album_content_id AS k, count(*) AS n FROM track_meta t
                 JOIN album_meta al ON al.content_id = t.album_content_id
                 WHERE al.artist_content_id = ?1 GROUP BY t.album_content_id",
                "SELECT tf.album_content_id AS k, count(*) AS n, COALESCE(sum(mf.size), 0) AS bytes
                 FROM track_file tf JOIN media_file mf ON mf.id = tf.media_file_id
                 WHERE tf.artist_content_id = ?1 GROUP BY tf.album_content_id",
            ),
            None => (
                "SELECT album_content_id AS k, count(*) AS n FROM track_meta
                 GROUP BY album_content_id",
                "SELECT tf.album_content_id AS k, count(*) AS n, COALESCE(sum(mf.size), 0) AS bytes
                 FROM track_file tf JOIN media_file mf ON mf.id = tf.media_file_id
                 GROUP BY tf.album_content_id",
            ),
        };
        let bind = artist.map(|a| a.to_string());
        let track_sql = pq(track_sql);
        let mut q = sqlx::query(track_sql);
        if let Some(b) = &bind {
            q = q.bind(b);
        }
        for row in q.fetch_all(&self.pool).await? {
            let entry = out.entry(key_of(&row)?).or_default();
            entry.album_count = 1;
            entry.track_count = row.try_get("n")?;
        }
        let file_sql = pq(file_sql);
        let mut q = sqlx::query(file_sql);
        if let Some(b) = &bind {
            q = q.bind(b);
        }
        for row in q.fetch_all(&self.pool).await? {
            let entry = out.entry(key_of(&row)?).or_default();
            entry.album_count = 1;
            entry.track_file_count = row.try_get("n")?;
            entry.size_on_disk = row.try_get("bytes")?;
        }
        Ok(out)
    }

    /// Albums that have no track list yet, monitored ones first.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn albums_without_tracks(&self, limit: usize) -> Result<Vec<Album>> {
        let sql = format!(
            "SELECT {ALBUM_COLS} FROM album_meta al JOIN content c ON c.id = al.content_id
             WHERE al.tracks_fetched_at IS NULL
             ORDER BY c.monitored DESC, al.id ASC LIMIT ?1"
        );
        let rows = sqlx::query(pq(&sql))
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(row_to_album).collect()
    }

    /// The artist an album belongs to, by the album's content id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist_of_album(&self, album: ContentId) -> Result<Option<Artist>> {
        self.artist_where(
            "a.content_id = (SELECT artist_content_id FROM album_meta WHERE content_id = ?1)",
            ArtistKey::Text(album.to_string()),
        )
        .await
    }

    /// For every artist: its most recent album released on or before `today`, and
    /// its next one after it. Dates compare as text, which
    /// orders `yyyy`, `yyyy-mm` and `yyyy-mm-dd` correctly against each other.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn artist_album_bounds(
        &self,
        today: &str,
    ) -> Result<HashMap<ContentId, (Option<ContentId>, Option<ContentId>)>> {
        let rows = sqlx::query(pq(
            "SELECT artist_content_id AS k, content_id, release_date FROM album_meta
             WHERE release_date IS NOT NULL AND release_date <> ''
             ORDER BY artist_content_id, release_date ASC, id ASC",
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out: HashMap<ContentId, (Option<ContentId>, Option<ContentId>)> = HashMap::new();
        for r in rows {
            let artist = key_of(&r)?;
            let id = content_id(&r, "content_id")?;
            let date: String = r.try_get("release_date")?;
            let entry = out.entry(artist).or_default();
            if date.as_str() <= today {
                // Ascending, so the last one seen wins.
                entry.0 = Some(id);
            } else if entry.1.is_none() {
                entry.1 = Some(id);
            }
        }
        Ok(out)
    }

    // --- quality-profile integer ids ------------------------------------------

    /// The integer id of a quality profile, assigned on first use.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn profile_seq(&self, profile: QualityProfileId) -> Result<i64> {
        self.write(move |conn| {
            Box::pin(async move {
                let found = sqlx::query(pq(
                    "SELECT seq FROM quality_profile_seq WHERE profile_id = ?1",
                ))
                .bind(profile.to_string())
                .fetch_optional(&mut *conn)
                .await?;
                if let Some(row) = found {
                    return Ok(row.try_get::<i64, _>("seq")?);
                }
                let next = next_seq(conn, "quality_profile", None).await?;
                sqlx::query(pq(
                    "INSERT INTO quality_profile_seq (profile_id, seq) VALUES (?1, ?2)",
                ))
                .bind(profile.to_string())
                .bind(next)
                .execute(&mut *conn)
                .await?;
                Ok(next)
            })
        })
        .await
    }

    /// Give a profile a specific integer id (a migration preserving the source
    /// install's ids). Fails if the integer or the profile is already assigned
    /// differently.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure or conflict.
    pub async fn claim_profile_seq(&self, profile: QualityProfileId, seq: i64) -> Result<()> {
        self.write(move |conn| {
            Box::pin(async move {
                next_seq(conn, "quality_profile", Some(seq)).await?;
                sqlx::query(pq(
                    "INSERT INTO quality_profile_seq (profile_id, seq) VALUES (?1, ?2)",
                ))
                .bind(profile.to_string())
                .bind(seq)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
    }

    /// The profile an integer id names.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn profile_by_seq(&self, seq: i64) -> Result<Option<QualityProfileId>> {
        let row = sqlx::query(pq(
            "SELECT profile_id FROM quality_profile_seq WHERE seq = ?1",
        ))
        .bind(seq)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let s: String = r.try_get("profile_id")?;
            Ok(QualityProfileId::from_uuid(parse_uuid("profile_id", &s)?))
        })
        .transpose()
    }

    /// Every assigned `(profile, integer id)` pair.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn profile_seqs(&self) -> Result<HashMap<QualityProfileId, i64>> {
        let rows = sqlx::query(pq("SELECT profile_id, seq FROM quality_profile_seq"))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let s: String = r.try_get("profile_id")?;
                Ok((
                    QualityProfileId::from_uuid(parse_uuid("profile_id", &s)?),
                    r.try_get("seq")?,
                ))
            })
            .collect()
    }

    // --- extras ---------------------------------------------------------------

    /// Replace the rich-metadata document of an artist or album.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn set_extras(&self, node: ContentId, body: String) -> Result<()> {
        self.writer
            .submit(move |conn| {
                Box::pin(async move {
                    sqlx::query(pq(
                        "INSERT INTO music_extras (content_id, body) VALUES (?1, ?2) \
                         ON CONFLICT(content_id) DO UPDATE SET body = excluded.body",
                    ))
                    .bind(node.to_string())
                    .bind(body)
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
    }

    /// The rich-metadata documents of the given nodes (those that have one).
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn extras_of(&self, nodes: &[ContentId]) -> Result<HashMap<ContentId, String>> {
        let mut out = HashMap::with_capacity(nodes.len());
        for chunk in nodes.chunks(400) {
            let marks = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql =
                format!("SELECT content_id, body FROM music_extras WHERE content_id IN ({marks})");
            let mut q = sqlx::query(pq(&sql));
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                let id: String = row.try_get("content_id")?;
                out.insert(
                    ContentId::from_uuid(parse_uuid("content_id", &id)?),
                    row.try_get("body")?,
                );
            }
        }
        Ok(out)
    }

    // --- history --------------------------------------------------------------

    /// A page of history for music content, newest first, with the total.
    ///
    /// `album` / `artist` narrow to one album or all of an artist's albums;
    /// `kinds` keeps only events of those names (`grabbed`, `imported`, …); an
    /// empty `kinds` keeps every event.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query/decode failure.
    pub async fn history_page(
        &self,
        album: Option<ContentId>,
        artist: Option<ContentId>,
        kinds: &[&str],
        since: Option<&str>,
        offset: u64,
        limit: u64,
    ) -> Result<(u64, Vec<cellarr_core::HistoryRecord>)> {
        let mut clauses = vec!["h.content_id IN (SELECT content_id FROM album_meta)".to_string()];
        let mut binds: Vec<String> = Vec::new();
        let next = |value: String, binds: &mut Vec<String>| {
            binds.push(value);
            binds.len()
        };
        if let Some(album) = album {
            let n = next(album.to_string(), &mut binds);
            clauses.push(format!("h.content_id = ?{n}"));
        }
        if let Some(artist) = artist {
            let n = next(artist.to_string(), &mut binds);
            clauses.push(format!(
                "h.content_id IN (SELECT content_id FROM album_meta WHERE artist_content_id = ?{n})"
            ));
        }
        if let Some(since) = since {
            let n = next(since.to_string(), &mut binds);
            clauses.push(format!("h.at > ?{n}"));
        }
        if !kinds.is_empty() {
            let mut ors = Vec::new();
            for kind in kinds {
                let n = next(format!("{{\"event\":\"{kind}\"%"), &mut binds);
                ors.push(format!("h.event LIKE ?{n}"));
            }
            clauses.push(format!("({})", ors.join(" OR ")));
        }
        let predicate = clauses.join(" AND ");

        let count_sql = format!("SELECT count(*) AS n FROM history h WHERE {predicate}");
        let count_sql = pq(&count_sql);
        let mut q = sqlx::query(count_sql);
        for b in &binds {
            q = q.bind(b);
        }
        let total: i64 = q.fetch_one(&self.pool).await?.try_get("n")?;

        let n_limit = binds.len() + 1;
        let page_sql = format!(
            "SELECT h.at, h.content_id, h.run_id, h.event FROM history h WHERE {predicate}
             ORDER BY h.at DESC, h.id DESC LIMIT ?{n_limit} OFFSET ?{}",
            n_limit + 1
        );
        let page_sql = pq(&page_sql);
        let mut q = sqlx::query(page_sql);
        for b in &binds {
            q = q.bind(b);
        }
        let rows = q
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .bind(i64::try_from(offset).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await?;
        let records = rows
            .into_iter()
            .map(|row| {
                let at: String = row.try_get("at")?;
                let content: String = row.try_get("content_id")?;
                let run: String = row.try_get("run_id")?;
                let event: String = row.try_get("event")?;
                Ok(cellarr_core::HistoryRecord {
                    at: crate::convert::parse_time("at", &at)?,
                    content_id: ContentId::from_uuid(parse_uuid("content_id", &content)?),
                    run_id: cellarr_core::PipelineRunId::from_uuid(parse_uuid("run_id", &run)?),
                    event: serde_json::from_str(&event)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((u64::try_from(total).unwrap_or(0), records))
    }

    // --- quality ---------------------------------------------------------------

    /// For every album that has files: the weakest quality rank among them, with
    /// its artist's quality profile. An album is only as good as its worst track,
    /// which is what "cutoff not met" asks about.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn album_min_quality(
        &self,
    ) -> Result<Vec<(ContentId, u32, Option<QualityProfileId>)>> {
        let rows = sqlx::query(pq(
            "SELECT tf.album_content_id AS album, min(mf.quality_rank) AS rank,
                    ar.quality_profile_id AS profile
             FROM track_file tf
             JOIN media_file mf ON mf.id = tf.media_file_id
             JOIN artist_meta ar ON ar.content_id = tf.artist_content_id
             GROUP BY tf.album_content_id, ar.quality_profile_id",
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let album: String = r.try_get("album")?;
                let rank: Option<i64> = r.try_get("rank")?;
                let profile: Option<String> = r.try_get("profile")?;
                Ok((
                    ContentId::from_uuid(parse_uuid("album", &album)?),
                    u32::try_from(rank.unwrap_or(0)).unwrap_or(0),
                    profile
                        .map(|p| parse_uuid("profile", &p).map(QualityProfileId::from_uuid))
                        .transpose()?,
                ))
            })
            .collect()
    }

    /// Albums by content id, in one query.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn albums_by_content(&self, ids: &[ContentId]) -> Result<Vec<Album>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(400) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT {ALBUM_COLS} FROM album_meta al JOIN content c ON c.id = al.content_id
                 WHERE al.content_id IN ({placeholders}) ORDER BY al.id ASC"
            );
            let sql = pq(&sql);
            let mut q = sqlx::query(sql);
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                out.push(row_to_album(row)?);
            }
        }
        Ok(out)
    }

    /// File statistics for exactly the albums named, in two queries however many.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn album_stats_for(
        &self,
        albums: &[ContentId],
    ) -> Result<HashMap<ContentId, MusicStats>> {
        let mut out: HashMap<ContentId, MusicStats> = HashMap::new();
        for chunk in albums.chunks(400) {
            let marks = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let tracks = format!(
                "SELECT album_content_id AS k, count(*) AS n FROM track_meta
                 WHERE album_content_id IN ({marks}) GROUP BY album_content_id"
            );
            let tracks = pq(&tracks);
            let mut q = sqlx::query(tracks);
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                let e = out.entry(key_of(&row)?).or_default();
                e.album_count = 1;
                e.track_count = row.try_get("n")?;
            }
            let files = format!(
                "SELECT tf.album_content_id AS k, count(*) AS n, COALESCE(sum(mf.size), 0) AS bytes
                 FROM track_file tf JOIN media_file mf ON mf.id = tf.media_file_id
                 WHERE tf.album_content_id IN ({marks}) GROUP BY tf.album_content_id"
            );
            let files = pq(&files);
            let mut q = sqlx::query(files);
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                let e = out.entry(key_of(&row)?).or_default();
                e.album_count = 1;
                e.track_file_count = row.try_get("n")?;
                e.size_on_disk = row.try_get("bytes")?;
            }
        }
        Ok(out)
    }

    // --- metadata profiles ---------------------------------------------------

    /// Every metadata profile. A database that has none yet reports the
    /// standard profile, so id 1 always resolves.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query/decode failure.
    pub async fn metadata_profiles(&self) -> Result<Vec<MetadataProfile>> {
        let rows = sqlx::query(pq("SELECT body FROM metadata_profile ORDER BY id ASC"))
            .fetch_all(&self.pool)
            .await?;
        let mut profiles = rows
            .into_iter()
            .map(|r| {
                let body: String = r.try_get("body")?;
                serde_json::from_str::<MetadataProfile>(&body).map_err(DbError::from)
            })
            .collect::<Result<Vec<_>>>()?;
        if profiles.is_empty() {
            profiles.push(MetadataProfile::standard());
        }
        Ok(profiles)
    }

    /// One metadata profile by id.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query/decode failure.
    pub async fn metadata_profile(&self, id: i64) -> Result<Option<MetadataProfile>> {
        Ok(self
            .metadata_profiles()
            .await?
            .into_iter()
            .find(|p| p.id == id))
    }

    /// Create or replace a metadata profile. An id of 0 takes the next free id.
    /// Returns the stored profile.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn upsert_metadata_profile(
        &self,
        profile: MetadataProfile,
    ) -> Result<MetadataProfile> {
        self.write(move |conn| {
            Box::pin(async move {
                let mut profile = profile;
                if profile.id <= 0 {
                    let max: i64 =
                        sqlx::query(pq("SELECT COALESCE(max(id), 1) AS m FROM metadata_profile"))
                            .fetch_one(&mut *conn)
                            .await?
                            .try_get("m")?;
                    profile.id = max + 1;
                }
                sqlx::query(pq(
                    "INSERT INTO metadata_profile (id, name, body) VALUES (?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name, body = excluded.body",
                ))
                .bind(profile.id)
                .bind(&profile.name)
                .bind(serde_json::to_string(&profile)?)
                .execute(&mut *conn)
                .await?;
                Ok(profile)
            })
        })
        .await
    }

    /// Delete a metadata profile. Returns whether one was deleted.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn delete_metadata_profile(&self, id: i64) -> Result<bool> {
        self.write(move |conn| {
            Box::pin(async move {
                let result = sqlx::query(pq("DELETE FROM metadata_profile WHERE id = ?1"))
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                Ok(result.rows_affected() > 0)
            })
        })
        .await
    }
}

enum ArtistKey {
    Seq(i64),
    Text(String),
}

enum TrackFileKey {
    Seq(i64),
    Text(String),
}

/// Reconcile an album's stored tracks with `tracks`: keep matching ones (by disc
/// and position) with their ids, add new ones, drop vanished ones that have no
/// file, and stamp the album as fetched.
async fn store_tracks(
    conn: &mut DbConnection,
    library: LibraryId,
    album: ContentId,
    release_mbid: Option<String>,
    tracks: Vec<NewTrack>,
    now: &str,
) -> Result<Vec<Track>> {
    let existing = sqlx::query(pq(&format!(
        "SELECT {TRACK_COLS} FROM track_meta t WHERE t.album_content_id = ?1"
    )))
    .bind(album.to_string())
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(row_to_track)
    .collect::<Result<Vec<_>>>()?;
    let mut by_slot: HashMap<(u32, u32), Track> = existing
        .into_iter()
        .map(|t| ((t.disc, t.position), t))
        .collect();

    let mut out = Vec::with_capacity(tracks.len());
    for new in tracks {
        if let Some(mut kept) = by_slot.remove(&(new.disc, new.position)) {
            sqlx::query(pq(
                "UPDATE track_meta SET mbid = ?2, title = ?3, duration_ms = ?4
                 WHERE content_id = ?1",
            ))
            .bind(kept.content_id.to_string())
            .bind(&new.mbid)
            .bind(&new.title)
            .bind(new.duration_ms.map(i64::from))
            .execute(&mut *conn)
            .await?;
            kept.mbid = new.mbid;
            kept.title = new.title;
            kept.duration_ms = new.duration_ms;
            out.push(kept);
            continue;
        }
        let id = next_seq(conn, "track", new.id).await?;
        let content_id = ContentId::from_uuid(seq_uuid(seq_u32(id)));
        let coords = format!(
            "{{\"type\":\"track\",\"disc\":{},\"track\":{}}}",
            new.disc, new.position
        );
        insert_node(
            conn,
            content_id,
            library,
            Some(album),
            "track",
            &coords,
            true,
        )
        .await?;
        sqlx::query(pq("INSERT INTO track_meta
                (id, content_id, album_content_id, mbid, title, disc, position, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"))
        .bind(id)
        .bind(content_id.to_string())
        .bind(album.to_string())
        .bind(&new.mbid)
        .bind(&new.title)
        .bind(i64::from(new.disc))
        .bind(i64::from(new.position))
        .bind(new.duration_ms.map(i64::from))
        .execute(&mut *conn)
        .await?;
        out.push(Track {
            id,
            content_id,
            album_content_id: album,
            mbid: new.mbid,
            title: new.title,
            disc: new.disc,
            position: new.position,
            duration_ms: new.duration_ms,
        });
    }

    for stale in by_slot.into_values() {
        let has_file = sqlx::query(pq(
            "SELECT 1 AS x FROM track_file WHERE track_content_id = ?1 LIMIT 1",
        ))
        .bind(stale.content_id.to_string())
        .fetch_optional(&mut *conn)
        .await?
        .is_some();
        if has_file {
            out.push(stale);
            continue;
        }
        sqlx::query(pq("DELETE FROM content WHERE id = ?1"))
            .bind(stale.content_id.to_string())
            .execute(&mut *conn)
            .await?;
    }

    sqlx::query(pq("UPDATE album_meta SET tracks_fetched_at = ?2,
            release_mbid = COALESCE(?3, release_mbid)
         WHERE content_id = ?1"))
    .bind(album.to_string())
    .bind(now)
    .bind(&release_mbid)
    .execute(&mut *conn)
    .await?;
    out.sort_by_key(|t| (t.disc, t.position));
    Ok(out)
}

fn key_of(row: &DbRow) -> Result<ContentId> {
    let k: String = row.try_get("k")?;
    Ok(ContentId::from_uuid(parse_uuid("content_id", &k)?))
}

/// A sequence id as the `u32` a content id embeds. Ids are positive and far
/// below `u32::MAX`; an out-of-range value (a corrupt counter) saturates rather
/// than wrapping into another node's id.
fn seq_u32(id: i64) -> u32 {
    u32::try_from(id).unwrap_or(u32::MAX)
}

/// Take the next id of sequence `name`, or claim `preset` and raise the counter
/// to cover it.
async fn next_seq(conn: &mut DbConnection, name: &str, preset: Option<i64>) -> Result<i64> {
    let current: i64 = sqlx::query(pq("SELECT value FROM music_seq WHERE name = ?1"))
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?
        .map(|r| r.try_get::<i64, _>("value"))
        .transpose()?
        .unwrap_or(0);
    let (id, value) = match preset {
        Some(id) => (id, current.max(id)),
        None => (current + 1, current + 1),
    };
    sqlx::query(pq("INSERT INTO music_seq (name, value) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET value = excluded.value"))
    .bind(name)
    .bind(value)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

async fn insert_node(
    conn: &mut DbConnection,
    id: ContentId,
    library: LibraryId,
    parent: Option<ContentId>,
    kind: &str,
    coords: &str,
    monitored: bool,
) -> Result<()> {
    sqlx::query(pq(
        "INSERT INTO content (id, library_id, media_type, parent_id, kind, coords, monitored)
         VALUES (?1, ?2, 'music', ?3, ?4, ?5, ?6)",
    ))
    .bind(id.to_string())
    .bind(library.to_string())
    .bind(parent.map(|p| p.to_string()))
    .bind(kind)
    .bind(coords)
    .bind(i64::from(monitored))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Index the title of a node that has just been created. There is nothing to
/// delete first, and skipping the delete matters: the index is keyed by an
/// unindexed column, so every delete scans it, and a library of tens of
/// thousands of albums would otherwise be quadratic to build.
async fn index_new_title(conn: &mut DbConnection, id: ContentId, title: &str) -> Result<()> {
    sqlx::query(pq(
        "INSERT INTO content_fts (content_id, title) VALUES (?1, ?2)",
    ))
    .bind(id.to_string())
    .bind(title)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Re-index the title of an existing node (replacing its row).
async fn index_title(conn: &mut DbConnection, id: ContentId, title: &str) -> Result<()> {
    sqlx::query(pq("DELETE FROM content_fts WHERE content_id = ?1"))
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    sqlx::query(pq(
        "INSERT INTO content_fts (content_id, title) VALUES (?1, ?2)",
    ))
    .bind(id.to_string())
    .bind(title)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The generic per-node metadata the media-agnostic surfaces read (title, year,
/// release date), kept in step with the album's identity.
async fn write_content_meta(
    conn: &mut DbConnection,
    id: ContentId,
    title: &str,
    release_date: Option<&str>,
) -> Result<()> {
    let year: Option<i64> = release_date
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok());
    sqlx::query(pq(
        "INSERT INTO content_meta (content_id, title, year, air_date) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(content_id) DO UPDATE SET
            title = excluded.title, year = excluded.year, air_date = excluded.air_date",
    ))
    .bind(id.to_string())
    .bind(title)
    .bind(year)
    .bind(release_date)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn write_aliases(
    conn: &mut DbConnection,
    artist: ContentId,
    aliases: &[String],
) -> Result<()> {
    for alias in aliases {
        let norm = normalize_name(alias);
        if norm.is_empty() {
            continue;
        }
        sqlx::query(pq(
            "INSERT INTO artist_alias (artist_content_id, alias_norm) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
        ))
        .bind(artist.to_string())
        .bind(norm)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn content_id(row: &DbRow, column: &'static str) -> Result<ContentId> {
    let s: String = row.try_get(column)?;
    Ok(ContentId::from_uuid(parse_uuid(column, &s)?))
}

fn json_list(row: &DbRow, column: &'static str) -> Result<Vec<String>> {
    let s: String = row.try_get(column)?;
    Ok(serde_json::from_str(&s)?)
}

fn row_to_artist(row: DbRow) -> Result<Artist> {
    let quality_profile_id = row
        .try_get::<Option<String>, _>("quality_profile_id")?
        .map(|s| parse_uuid("quality_profile_id", &s).map(QualityProfileId::from_uuid))
        .transpose()?;
    Ok(Artist {
        id: row.try_get("id")?,
        content_id: content_id(&row, "content_id")?,
        mbid: row.try_get("mbid")?,
        name: row.try_get("name")?,
        sort_name: row.try_get("sort_name")?,
        disambiguation: row.try_get("disambiguation")?,
        artist_type: row.try_get("artist_type")?,
        ended: row.try_get::<i64, _>("ended")? != 0,
        overview: row.try_get("overview")?,
        genres: json_list(&row, "genres")?,
        path: row.try_get("path")?,
        quality_profile_id,
        metadata_profile_id: row.try_get("metadata_profile_id")?,
        monitored: row.try_get::<i64, _>("monitored")? != 0,
        monitor_new_albums: row.try_get::<i64, _>("monitor_new_albums")? != 0,
        added_at: row.try_get("added_at")?,
        refreshed_at: row.try_get("refreshed_at")?,
        extras: row.try_get("extras")?,
    })
}

fn row_to_album(row: DbRow) -> Result<Album> {
    Ok(Album {
        id: row.try_get("id")?,
        content_id: content_id(&row, "content_id")?,
        artist_content_id: content_id(&row, "artist_content_id")?,
        mbid: row.try_get("mbid")?,
        title: row.try_get("title")?,
        disambiguation: row.try_get("disambiguation")?,
        album_type: row.try_get("album_type")?,
        secondary_types: json_list(&row, "secondary_types")?,
        release_date: row.try_get("release_date")?,
        monitored: row.try_get::<i64, _>("monitored")? != 0,
        release_mbid: row.try_get("release_mbid")?,
        added_at: row.try_get("added_at")?,
        tracks_fetched_at: row.try_get("tracks_fetched_at")?,
        any_release_ok: row.try_get::<i64, _>("any_release_ok")? != 0,
    })
}

fn row_to_track(row: DbRow) -> Result<Track> {
    let as_u32 = |v: i64| u32::try_from(v).unwrap_or(0);
    Ok(Track {
        id: row.try_get("id")?,
        content_id: content_id(&row, "content_id")?,
        album_content_id: content_id(&row, "album_content_id")?,
        mbid: row.try_get("mbid")?,
        title: row.try_get("title")?,
        disc: as_u32(row.try_get("disc")?),
        position: as_u32(row.try_get("position")?),
        duration_ms: row.try_get::<Option<i64>, _>("duration_ms")?.map(as_u32),
    })
}

fn row_to_track_file(row: DbRow) -> Result<TrackFile> {
    let mf_id: String = row.try_get("mf_id")?;
    let quality: Option<String> = row.try_get("quality")?;
    let quality = quality
        .as_deref()
        .map(serde_json::from_str::<Quality>)
        .transpose()?
        .unwrap_or_else(|| Quality::new("Unknown", 0));
    let languages: String = row.try_get("languages")?;
    let media_info: Option<String> = row.try_get("media_info")?;
    let score: i64 = row.try_get("custom_format_score")?;
    Ok(TrackFile {
        id: row.try_get("tf_id")?,
        file: MediaFile {
            id: MediaFileId::from_uuid(parse_uuid("media_file_id", &mf_id)?),
            path: row.try_get("path")?,
            size: u64::try_from(row.try_get::<i64, _>("size")?).unwrap_or(0),
            quality,
            languages: serde_json::from_str(&languages)?,
            media_info: media_info
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            custom_format_score: i32::try_from(score).ok(),
            release_type: None,
        },
        track_content_id: content_id(&row, "track_content_id")?,
        album_content_id: content_id(&row, "album_content_id")?,
        artist_content_id: content_id(&row, "artist_content_id")?,
        added_at: row.try_get("added_at")?,
    })
}

// --- album releases (pressings) ----------------------------------------------

/// A pressing to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRelease {
    /// Keep this id (a migration keeps Lidarr's); `None` assigns the next one.
    pub id: Option<i64>,
    /// MusicBrainz release id.
    pub mbid: String,
    /// Title.
    pub title: String,
    /// Status.
    pub status: Option<String>,
    /// Release date.
    pub release_date: Option<String>,
    /// Disambiguation.
    pub disambiguation: Option<String>,
    /// Countries.
    pub country: Vec<String>,
    /// Labels.
    pub label: Vec<String>,
    /// Discs: `(number, name, format)`.
    pub media: Vec<(u32, String, String)>,
    /// Tracks.
    pub track_count: u32,
    /// Length.
    pub duration_ms: u64,
}

fn media_json(media: &[(u32, String, String)]) -> String {
    serde_json::to_string(
        &media
            .iter()
            .map(|(n, name, format)| serde_json::json!({"number": n, "name": name, "format": format}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into())
}

fn row_to_release(row: DbRow) -> Result<cellarr_core::AlbumRelease> {
    let media: Vec<serde_json::Value> =
        serde_json::from_str(&row.try_get::<String, _>("media")?).unwrap_or_default();
    Ok(cellarr_core::AlbumRelease {
        id: row.try_get("id")?,
        album_content_id: content_id(&row, "album_content_id")?,
        mbid: row.try_get("mbid")?,
        title: row.try_get("title")?,
        status: row.try_get("status")?,
        release_date: row.try_get("release_date")?,
        disambiguation: row.try_get("disambiguation")?,
        country: json_list(&row, "country")?,
        label: json_list(&row, "label")?,
        media: media
            .iter()
            .map(|m| {
                (
                    m["number"].as_u64().unwrap_or(1) as u32,
                    m["name"].as_str().unwrap_or_default().to_string(),
                    m["format"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect(),
        track_count: u32::try_from(row.try_get::<i64, _>("track_count")?).unwrap_or(0),
        duration_ms: u64::try_from(row.try_get::<i64, _>("duration_ms")?).unwrap_or(0),
    })
}

/// Upsert pressings of an album by mbid, keeping ids. Pressings no longer
/// listed are kept unless `prune` (a pinned one is never pruned).
async fn store_releases(
    conn: &mut DbConnection,
    album: ContentId,
    releases: &[NewRelease],
    prune: bool,
) -> Result<()> {
    let key = album.to_string();
    for r in releases {
        let existing: Option<i64> = sqlx::query(pq(
            "SELECT id FROM album_release WHERE album_content_id = ?1 AND mbid = ?2",
        ))
        .bind(&key)
        .bind(&r.mbid)
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.try_get("id"))
        .transpose()?;
        let id = match existing {
            Some(id) => id,
            None => next_seq(conn, "release", r.id).await?,
        };
        sqlx::query(pq(
            "INSERT INTO album_release (id, album_content_id, mbid, title, status, release_date,
                 disambiguation, country, label, media, track_count, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, status = excluded.status,
                 release_date = excluded.release_date, disambiguation = excluded.disambiguation,
                 country = excluded.country, label = excluded.label, media = excluded.media,
                 track_count = excluded.track_count, duration_ms = excluded.duration_ms",
        ))
        .bind(id)
        .bind(&key)
        .bind(&r.mbid)
        .bind(&r.title)
        .bind(&r.status)
        .bind(&r.release_date)
        .bind(&r.disambiguation)
        .bind(serde_json::to_string(&r.country).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(&r.label).unwrap_or_else(|_| "[]".into()))
        .bind(media_json(&r.media))
        .bind(i64::from(r.track_count))
        .bind(i64::try_from(r.duration_ms).unwrap_or(i64::MAX))
        .execute(&mut *conn)
        .await?;
    }
    if prune {
        let keep: Vec<&str> = releases.iter().map(|r| r.mbid.as_str()).collect();
        let rows = sqlx::query(pq(
            "SELECT r.id, r.mbid, al.release_mbid FROM album_release r
             JOIN album_meta al ON al.content_id = r.album_content_id
             WHERE r.album_content_id = ?1",
        ))
        .bind(&key)
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            let mbid: String = row.try_get("mbid")?;
            let pinned: Option<String> = row.try_get("release_mbid")?;
            if !keep.contains(&mbid.as_str()) && pinned.as_deref() != Some(mbid.as_str()) {
                sqlx::query(pq("DELETE FROM album_release WHERE id = ?1"))
                    .bind(row.try_get::<i64, _>("id")?)
                    .execute(&mut *conn)
                    .await?;
            }
        }
    }
    Ok(())
}

impl MusicRepo {
    /// Store an album's pressings (all of them, from a metadata fetch).
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn set_album_releases(
        &self,
        album: ContentId,
        releases: Vec<NewRelease>,
    ) -> Result<()> {
        self.write(move |conn| {
            Box::pin(async move { store_releases(conn, album, &releases, true).await })
        })
        .await
    }

    /// Store the pressings of many albums in one transaction (a migration),
    /// keeping their given ids.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn set_many_album_releases(
        &self,
        batch: Vec<(ContentId, Vec<NewRelease>)>,
    ) -> Result<()> {
        self.write(move |conn| {
            Box::pin(async move {
                for (album, releases) in &batch {
                    store_releases(conn, *album, releases, false).await?;
                }
                Ok(())
            })
        })
        .await
    }

    /// The pressings of the given albums, by album.
    ///
    /// # Errors
    /// Returns a [`DbError`] on query failure.
    pub async fn releases_for_albums(
        &self,
        albums: &[ContentId],
    ) -> Result<HashMap<ContentId, Vec<cellarr_core::AlbumRelease>>> {
        let mut out: HashMap<ContentId, Vec<cellarr_core::AlbumRelease>> = HashMap::new();
        for chunk in albums.chunks(400) {
            let marks = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT * FROM album_release WHERE album_content_id IN ({marks}) ORDER BY id"
            );
            let mut q = sqlx::query(pq(&sql));
            for id in chunk {
                q = q.bind(id.to_string());
            }
            for row in q.fetch_all(&self.pool).await? {
                let r = row_to_release(row)?;
                out.entry(r.album_content_id).or_default().push(r);
            }
        }
        Ok(out)
    }

    /// Pin a pressing (or clear the pin with `None`) and say whether only it will do.
    ///
    /// # Errors
    /// Returns a [`DbError`] on write failure.
    pub async fn pin_release(
        &self,
        album: ContentId,
        release_mbid: Option<String>,
        any_release_ok: bool,
    ) -> Result<()> {
        self.write(move |conn| {
            Box::pin(async move {
                sqlx::query(pq(
                    "UPDATE album_meta SET release_mbid = ?2, any_release_ok = ?3 WHERE content_id = ?1",
                ))
                .bind(album.to_string())
                .bind(release_mbid)
                .bind(i64::from(any_release_ok))
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
    }
}

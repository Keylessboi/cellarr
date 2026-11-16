//! Importing an existing Lidarr install.
//!
//! Lidarr keeps everything in one SQLite database. This reads it **read-only**
//! (the running install is never touched, and can keep running) and writes the
//! equivalent records into cellarr: the artists, albums, tracks and track files
//! with **their original integer ids**, so every client keyed on a Lidarr id
//! (a search cursor, a script's profile id) keeps working; the quality and
//! metadata profiles; the root folders, indexers, download clients and path
//! mappings; and the blocklist.
//!
//! Files are recognized **in place**: a track file's path is recorded as it is,
//! and nothing on disk is moved or rewritten.
//!
//! The column names read here are Lidarr's persisted model (its `Artists`,
//! `ArtistMetadata`, `Albums`, `AlbumReleases`, `Tracks`, `TrackFiles`, … tables).
//! Embedded JSON documents (quality, profile items, secondary types) are read
//! tolerantly: key case is ignored and a reference may be a bare id or an object
//! carrying one, because Lidarr has stored both over its history.

use std::collections::HashMap;

use cellarr_core::music::{lidarr_quality_name, normalize_name};
use cellarr_core::{
    ContentId, Library, LibraryId, MediaFile, MediaFileId, MetadataProfile, Quality,
    QualityProfile, QualityProfileId, QualityRanking, RemotePathMapping,
};
use cellarr_db::{Database, NewAlbum, NewArtist, NewTrack};
use serde_json::Value;
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::Row;

use crate::error::Result;
use crate::source::opt_text;

/// How many albums' tracks are stored per transaction.
const TRACK_BATCH: usize = 400;

/// What a Lidarr import wrote (or, in a preview, would write).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MusicReport {
    /// Artists.
    pub artists: usize,
    /// Albums.
    pub albums: usize,
    /// Tracks (of each album's selected release).
    pub tracks: usize,
    /// Track files recognized in place.
    pub track_files: usize,
    /// Track files that no track points at, left out.
    pub unmapped_files: usize,
    /// Quality profiles.
    pub quality_profiles: usize,
    /// Metadata profiles.
    pub metadata_profiles: usize,
    /// Blocklist entries.
    pub blocklist: usize,
    /// Remote path mappings.
    pub remote_path_mappings: usize,
    /// History rows left behind (cellarr starts a fresh history).
    pub history_skipped: usize,
}

pub(crate) async fn table_exists(pool: &SqlitePool, name: &str) -> Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")
            .bind(name)
            .fetch_optional(pool)
            .await?
            .is_some(),
    )
}

/// Whether the database is a Lidarr one: it has the artist/album/release trio.
pub(crate) async fn is_lidarr(pool: &SqlitePool) -> Result<bool> {
    Ok(table_exists(pool, "Artists").await?
        && table_exists(pool, "ArtistMetadata").await?
        && table_exists(pool, "AlbumReleases").await?)
}

async fn count(pool: &SqlitePool, table: &str) -> usize {
    sqlx::query(&format!("SELECT count(*) AS n FROM {table}"))
        .fetch_one(pool)
        .await
        .ok()
        .and_then(|r| r.try_get::<i64, _>("n").ok())
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

/// Row counts for a preview.
pub(crate) async fn preview(pool: &SqlitePool) -> Result<MusicReport> {
    Ok(MusicReport {
        artists: count(pool, "Artists").await,
        albums: count(pool, "Albums").await,
        tracks: sqlx::query(
            "SELECT count(*) AS n FROM Tracks t JOIN AlbumReleases r ON r.Id = t.AlbumReleaseId
             WHERE r.Monitored = 1",
        )
        .fetch_one(pool)
        .await?
        .try_get::<i64, _>("n")
        .ok()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0),
        track_files: count(pool, "TrackFiles").await,
        quality_profiles: count(pool, "QualityProfiles").await,
        metadata_profiles: count(pool, "MetadataProfiles").await,
        blocklist: count(pool, "Blocklist").await,
        history_skipped: count(pool, "History").await,
        ..MusicReport::default()
    })
}

// --- tolerant JSON ---------------------------------------------------------------

/// Lowercase every object key, recursively, so `Allowed`, `allowed` and `ALLOWED`
/// read the same.
fn lower_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k.to_lowercase(), lower_keys(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(lower_keys).collect()),
        other => other,
    }
}

fn parse_json(raw: Option<String>) -> Value {
    raw.and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map_or(Value::Null, lower_keys)
}

/// The id a reference carries: a bare number, or an object with an `id`.
fn ref_id(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.get("id").and_then(Value::as_i64))
}

/// The name a reference carries, when it is an object with one.
fn ref_name(v: &Value) -> Option<String> {
    v.get("name").and_then(Value::as_str).map(str::to_string)
}

fn quality_of(v: &Value, ranking: &QualityRanking) -> Quality {
    let q = v.get("quality").unwrap_or(v);
    let name = ref_id(q)
        .and_then(|id| u32::try_from(id).ok())
        .and_then(lidarr_quality_name)
        .map(str::to_string)
        .or_else(|| ref_name(q));
    name.and_then(|n| ranking.by_name(&n)).unwrap_or_else(|| {
        ranking
            .by_name("Unknown")
            .unwrap_or_else(|| Quality::new("Unknown", 0))
    })
}

/// A date column as `yyyy-mm-dd` (Lidarr stores a full timestamp).
fn date_part(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.chars().take(10).collect::<String>())
        .filter(|s| s.len() == 10 && s.starts_with(|c: char| c.is_ascii_digit()))
}

fn timestamp(raw: Option<String>) -> Option<String> {
    raw.map(|s| {
        let s = s.trim().replace(' ', "T");
        if s.ends_with('Z') || s.contains('+') {
            s
        } else {
            format!("{s}Z")
        }
    })
    .filter(|s| s.len() >= 10)
}

// --- profiles --------------------------------------------------------------------

fn quality_profile(row: &SqliteRow, ranking: &QualityRanking) -> (i64, QualityProfile) {
    let id: i64 = row.try_get("Id").unwrap_or(0);
    let items = parse_json(opt_text(row, "Items"));
    let mut allowed: Vec<u32> = Vec::new();
    // Group id → the ranks of its members, for resolving a cutoff set on a group.
    let mut groups: HashMap<i64, Vec<u32>> = HashMap::new();
    for item in items.as_array().into_iter().flatten() {
        let on = item
            .get("allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let members: Vec<u32> = match item
            .get("items")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
        {
            Some(children) => children
                .iter()
                .map(|c| quality_of(c, ranking).rank)
                .collect(),
            None => vec![quality_of(item, ranking).rank],
        };
        if let Some(gid) = item.get("id").and_then(Value::as_i64) {
            groups.insert(gid, members.clone());
        }
        if on {
            for r in members {
                if !allowed.contains(&r) {
                    allowed.push(r);
                }
            }
        }
    }
    let cutoff_id: i64 = row.try_get("Cutoff").unwrap_or(0);
    let cutoff = groups
        .get(&cutoff_id)
        .and_then(|m| m.iter().copied().min())
        .or_else(|| {
            u32::try_from(cutoff_id)
                .ok()
                .and_then(lidarr_quality_name)
                .and_then(|n| ranking.by_name(n))
                .map(|q| q.rank)
        })
        .filter(|r| allowed.contains(r))
        .or_else(|| allowed.iter().copied().min())
        .unwrap_or(0);
    let score = |col: &str| i32::try_from(row.try_get::<i64, _>(col).unwrap_or(0)).unwrap_or(0);
    (
        id,
        QualityProfile {
            id: QualityProfileId::new(),
            name: row.try_get("Name").unwrap_or_default(),
            allowed_qualities: allowed,
            upgrades_allowed: row.try_get::<i64, _>("UpgradeAllowed").unwrap_or(1) != 0,
            cutoff_quality: cutoff,
            min_custom_format_score: score("MinFormatScore"),
            upgrade_until_custom_format_score: score("CutoffFormatScore"),
            required_languages: Vec::new(),
        },
    )
}

/// The names a profile's embedded list allows: each item holds `allowed` and an
/// object (named `key`) that carries an id or a name.
fn allowed_names(v: &Value, key: &str, by_id: &[&str]) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|i| i.get("allowed").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|i| {
            let r = i.get(key)?;
            ref_name(r).or_else(|| {
                ref_id(r)
                    .and_then(|id| usize::try_from(id).ok())
                    .and_then(|id| by_id.get(id))
                    .map(|n| (*n).to_string())
            })
        })
        .collect()
}

fn metadata_profile(row: &SqliteRow) -> MetadataProfile {
    use cellarr_core::music::{PRIMARY_ALBUM_TYPES, RELEASE_STATUSES, SECONDARY_ALBUM_TYPES};
    let primary = parse_json(opt_text(row, "PrimaryAlbumTypes"));
    let secondary = parse_json(opt_text(row, "SecondaryAlbumTypes"));
    let statuses = parse_json(opt_text(row, "ReleaseStatuses"));
    MetadataProfile {
        id: row.try_get("Id").unwrap_or(0),
        name: row.try_get("Name").unwrap_or_default(),
        primary_types: allowed_names(&primary, "primaryalbumtype", PRIMARY_ALBUM_TYPES),
        secondary_types: allowed_names(&secondary, "secondaryalbumtype", SECONDARY_ALBUM_TYPES),
        release_statuses: allowed_names(&statuses, "releasestatus", RELEASE_STATUSES),
    }
}

// --- the import ------------------------------------------------------------------

struct ArtistRow {
    id: i64,
    metadata_id: i64,
    mbid: String,
    name: String,
    sort_name: Option<String>,
    disambiguation: Option<String>,
    kind: Option<String>,
    ended: bool,
    overview: Option<String>,
    genres: Vec<String>,
    aliases: Vec<String>,
    path: String,
    monitored: bool,
    monitor_new: bool,
    quality_profile: i64,
    metadata_profile: i64,
    added: Option<String>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn artist_row(r: &SqliteRow) -> ArtistRow {
    ArtistRow {
        id: r.try_get("Id").unwrap_or(0),
        metadata_id: r.try_get("ArtistMetadataId").unwrap_or(0),
        mbid: r.try_get("ForeignArtistId").unwrap_or_default(),
        name: r.try_get("Name").unwrap_or_default(),
        sort_name: opt_text(r, "SortName"),
        disambiguation: opt_text(r, "Disambiguation"),
        kind: opt_text(r, "Type"),
        ended: r.try_get::<i64, _>("Status").unwrap_or(0) == 1,
        overview: opt_text(r, "Overview"),
        genres: strings(&parse_json(opt_text(r, "Genres"))),
        aliases: strings(&parse_json(opt_text(r, "Aliases"))),
        path: r.try_get("Path").unwrap_or_default(),
        monitored: r.try_get::<i64, _>("Monitored").unwrap_or(1) != 0,
        // Lidarr's `NewItemMonitorTypes`: 0 = All, 1 = None, 2 = New.
        monitor_new: r.try_get::<i64, _>("MonitorNewItems").unwrap_or(0) == 0,
        quality_profile: r.try_get("QualityProfileId").unwrap_or(0),
        metadata_profile: r.try_get("MetadataProfileId").unwrap_or(1),
        added: timestamp(opt_text(r, "Added")),
    }
}

/// Import everything music-related from a Lidarr database into `db`.
pub(crate) async fn import(pool: &SqlitePool, db: &Database) -> Result<MusicReport> {
    let ranking = QualityRanking::default();
    let music = db.music();
    let mut report = MusicReport::default();

    // --- profiles: keep Lidarr's integer ids -----------------------------------
    let mut profile_ids: HashMap<i64, QualityProfileId> = HashMap::new();
    for row in sqlx::query("SELECT * FROM QualityProfiles ORDER BY Id ASC")
        .fetch_all(pool)
        .await?
    {
        let (seq, profile) = quality_profile(&row, &ranking);
        db.profiles().upsert_profile(&profile).await?;
        music.claim_profile_seq(profile.id, seq).await?;
        profile_ids.insert(seq, profile.id);
        report.quality_profiles += 1;
    }
    for row in sqlx::query("SELECT * FROM MetadataProfiles ORDER BY Id ASC")
        .fetch_all(pool)
        .await?
    {
        music
            .upsert_metadata_profile(metadata_profile(&row))
            .await?;
        report.metadata_profiles += 1;
    }

    // --- the library -----------------------------------------------------------
    let roots: Vec<String> = sqlx::query("SELECT Path FROM RootFolders ORDER BY Id ASC")
        .fetch_all(pool)
        .await?
        .iter()
        .filter_map(|r| r.try_get::<String, _>("Path").ok())
        .collect();
    let library = Library {
        id: LibraryId::new(),
        media_type: cellarr_core::MediaType::Music,
        name: "Music".to_string(),
        root_folders: roots,
        default_quality_profile: profile_ids
            .values()
            .next()
            .copied()
            .unwrap_or_else(QualityProfileId::new),
    };
    db.config().upsert_library(&library).await?;

    // --- artists and their albums ---------------------------------------------
    let artists: Vec<ArtistRow> = sqlx::query(
        "SELECT a.Id, a.ArtistMetadataId, a.SortName, a.Path, a.Monitored, a.MonitorNewItems,
                a.QualityProfileId, a.MetadataProfileId, a.Added,
                m.ForeignArtistId, m.Name, m.Disambiguation, m.Type, m.Status, m.Overview,
                m.Genres, m.Aliases
         FROM Artists a JOIN ArtistMetadata m ON m.Id = a.ArtistMetadataId
         ORDER BY a.Id ASC",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .map(artist_row)
    .collect();

    // The release each album uses: its monitored one, else its first.
    let selected: HashMap<i64, (i64, String)> = sqlx::query(
        "SELECT r.AlbumId, r.Id, r.ForeignReleaseId, r.Monitored FROM AlbumReleases r
         ORDER BY r.AlbumId ASC, r.Monitored DESC, r.Id ASC",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .fold(HashMap::new(), |mut m, r| {
        let album: i64 = r.try_get("AlbumId").unwrap_or(0);
        m.entry(album).or_insert_with(|| {
            (
                r.try_get("Id").unwrap_or(0),
                r.try_get("ForeignReleaseId").unwrap_or_default(),
            )
        });
        m
    });

    // Album id → (album content id, selected release row id), for tracks and files.
    let mut albums_by_id: HashMap<i64, (ContentId, i64, Option<String>)> = HashMap::new();
    for a in &artists {
        let artist = music
            .add_artist(
                library.id,
                NewArtist {
                    id: Some(a.id),
                    mbid: a.mbid.clone(),
                    name: a.name.clone(),
                    sort_name: a.sort_name.clone().filter(|s| *s != a.name.to_lowercase()),
                    disambiguation: a.disambiguation.clone(),
                    artist_type: a.kind.clone(),
                    ended: a.ended,
                    overview: a.overview.clone(),
                    genres: a.genres.clone(),
                    aliases: a.aliases.clone(),
                    path: a.path.clone(),
                    quality_profile_id: profile_ids.get(&a.quality_profile).copied(),
                    metadata_profile_id: a.metadata_profile,
                    monitored: a.monitored,
                    monitor_new_albums: a.monitor_new,
                    added_at: a.added.clone(),
                },
            )
            .await?;
        report.artists += 1;

        let rows = sqlx::query(
            "SELECT Id, ForeignAlbumId, Title, Disambiguation, ReleaseDate, AlbumType,
                    SecondaryTypes, Monitored, Added
             FROM Albums WHERE ArtistMetadataId = ?1 ORDER BY Id ASC",
        )
        .bind(a.metadata_id)
        .fetch_all(pool)
        .await?;
        let mut new_albums = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: i64 = r.try_get("Id").unwrap_or(0);
            let secondary: Vec<String> = parse_json(opt_text(r, "SecondaryTypes"))
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    ref_name(t).or_else(|| {
                        ref_id(t)
                            .and_then(|i| usize::try_from(i).ok())
                            .and_then(|i| cellarr_core::music::SECONDARY_ALBUM_TYPES.get(i))
                            .map(|n| (*n).to_string())
                    })
                })
                // Lidarr stores "Studio" as an explicit type; cellarr models a
                // studio album as having none.
                .filter(|t| t != "Studio")
                .collect();
            new_albums.push(NewAlbum {
                id: Some(id),
                mbid: r.try_get("ForeignAlbumId").unwrap_or_default(),
                title: r.try_get("Title").unwrap_or_default(),
                disambiguation: opt_text(r, "Disambiguation"),
                album_type: opt_text(r, "AlbumType").unwrap_or_else(|| "Album".to_string()),
                secondary_types: secondary,
                release_date: date_part(opt_text(r, "ReleaseDate")),
                release_mbid: selected.get(&id).map(|(_, mbid)| mbid.clone()),
                monitored: r.try_get::<i64, _>("Monitored").unwrap_or(1) != 0,
                added_at: timestamp(opt_text(r, "Added")),
            });
        }
        let inserted = music
            .upsert_albums(library.id, artist.content_id, new_albums)
            .await?;
        report.albums += inserted.len();
        for al in inserted {
            let release = selected.get(&al.id).cloned();
            albums_by_id.insert(
                al.id,
                (
                    al.content_id,
                    release.as_ref().map_or(0, |r| r.0),
                    release.map(|r| r.1),
                ),
            );
        }
    }

    // --- tracks of each album's selected release -------------------------------
    //
    // One ordered pass over the Tracks table, not a query per album: it needs no
    // index (a database that lost them still imports in seconds), reads each row
    // once, and holds only one album's tracks at a time.
    let release_owner: HashMap<i64, (ContentId, Option<String>)> = albums_by_id
        .values()
        .filter(|(_, release_row, _)| *release_row != 0)
        .map(|(content, release_row, mbid)| (*release_row, (*content, mbid.clone())))
        .collect();
    let mut batch: Vec<(ContentId, Option<String>, Vec<NewTrack>)> = Vec::new();
    let mut track_of_file: HashMap<i64, i64> = HashMap::new();
    let mut current: Option<(i64, Vec<NewTrack>, HashMap<i64, u32>)> = None;
    let flush = |current: &mut Option<(i64, Vec<NewTrack>, HashMap<i64, u32>)>,
                 batch: &mut Vec<(ContentId, Option<String>, Vec<NewTrack>)>,
                 report: &mut MusicReport| {
        if let Some((release, tracks, _)) = current.take() {
            if let Some((content, mbid)) = release_owner.get(&release) {
                report.tracks += tracks.len();
                batch.push((*content, mbid.clone(), tracks));
            }
        }
    };
    {
        use futures::TryStreamExt;
        let mut rows = sqlx::query(
            "SELECT Id, ForeignRecordingId, Title, Duration, MediumNumber, AbsoluteTrackNumber,
                    TrackFileId, AlbumReleaseId
             FROM Tracks ORDER BY AlbumReleaseId ASC, MediumNumber ASC, AbsoluteTrackNumber ASC, Id ASC",
        )
        .fetch(pool);
        while let Some(r) = rows.try_next().await? {
            let release: i64 = r.try_get("AlbumReleaseId").unwrap_or(0);
            if !release_owner.contains_key(&release) {
                continue;
            }
            if current.as_ref().is_some_and(|(cur, _, _)| *cur != release) {
                flush(&mut current, &mut batch, &mut report);
                if batch.len() >= TRACK_BATCH {
                    music
                        .set_many_album_tracks(library.id, std::mem::take(&mut batch))
                        .await?;
                }
            }
            let (_, tracks, per_disc) =
                current.get_or_insert_with(|| (release, Vec::new(), HashMap::new()));
            let medium: i64 = r.try_get("MediumNumber").unwrap_or(1).max(1);
            let position = per_disc.entry(medium).or_insert(0);
            *position += 1;
            let id: i64 = r.try_get("Id").unwrap_or(0);
            let file: i64 = r.try_get("TrackFileId").unwrap_or(0);
            if file > 0 {
                track_of_file.entry(file).or_insert(id);
            }
            tracks.push(NewTrack {
                id: Some(id),
                mbid: opt_text(&r, "ForeignRecordingId"),
                title: r.try_get("Title").unwrap_or_default(),
                disc: u32::try_from(medium).unwrap_or(1),
                position: *position,
                duration_ms: r
                    .try_get::<i64, _>("Duration")
                    .ok()
                    .and_then(|d| u32::try_from(d).ok())
                    .filter(|d| *d > 0),
            });
        }
    }
    flush(&mut current, &mut batch, &mut report);
    if !batch.is_empty() {
        music.set_many_album_tracks(library.id, batch).await?;
    }

    // --- track files, recognized in place --------------------------------------
    for r in sqlx::query(
        "SELECT Id, Path, Size, DateAdded, Quality, MediaInfo FROM TrackFiles ORDER BY Id ASC",
    )
    .fetch_all(pool)
    .await?
    {
        let id: i64 = r.try_get("Id").unwrap_or(0);
        let track = match track_of_file.get(&id) {
            Some(seq) => music.track(*seq).await?.map(|t| t.content_id),
            None => None,
        };
        let Some(track) = track else {
            report.unmapped_files += 1;
            continue;
        };
        let info = parse_json(opt_text(&r, "MediaInfo"));
        let media_info = (!info.is_null()).then(|| {
            serde_json::json!({
                "source": "lidarr-import",
                "codec": info.get("audioformat"),
                "bitrateKbps": info.get("audiobitrate").and_then(Value::as_str)
                    .and_then(|s| s.split_whitespace().next()).and_then(|n| n.parse::<u32>().ok()),
                "bitDepth": info.get("audiobits").and_then(Value::as_str)
                    .and_then(|s| s.trim_end_matches("bit").trim().parse::<u32>().ok())
                    .or_else(|| info.get("audiobits").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())),
                "sampleRate": info.get("audiosamplerate"),
            })
        });
        let quality = quality_of(&parse_json(opt_text(&r, "Quality")), &ranking);
        music
            .record_track_file(
                track,
                MediaFile {
                    id: MediaFileId::new(),
                    path: r.try_get("Path").unwrap_or_default(),
                    size: u64::try_from(r.try_get::<i64, _>("Size").unwrap_or(0)).unwrap_or(0),
                    quality,
                    languages: Vec::new(),
                    media_info,
                    custom_format_score: None,
                    release_type: None,
                },
                Some(id),
                timestamp(opt_text(&r, "DateAdded")),
            )
            .await?;
        report.track_files += 1;
    }

    // --- remote path mappings and the blocklist ---------------------------------
    if table_exists(pool, "RemotePathMappings").await? {
        for r in sqlx::query("SELECT Id, Host, RemotePath, LocalPath FROM RemotePathMappings")
            .fetch_all(pool)
            .await?
        {
            db.config()
                .upsert_remote_path_mapping(&RemotePathMapping {
                    id: format!("lidarr-{}", r.try_get::<i64, _>("Id").unwrap_or(0)),
                    host: r.try_get("Host").unwrap_or_default(),
                    remote_path: r.try_get("RemotePath").unwrap_or_default(),
                    local_path: r.try_get("LocalPath").unwrap_or_default(),
                })
                .await?;
            report.remote_path_mappings += 1;
        }
    }
    report.blocklist = import_blocklist(pool, db, &albums_by_id).await?;
    report.history_skipped = count(pool, "History").await;
    // Silence the unused-import lint for the helper some builds do not reach.
    let _ = normalize_name;
    Ok(report)
}

/// Lidarr's blocklist, keyed by the release's name (the identity it recorded) or,
/// for a torrent, its info hash. A release the indexers return under that name is
/// then refused for that album, as before.
async fn import_blocklist(
    pool: &SqlitePool,
    db: &Database,
    albums: &HashMap<i64, (ContentId, i64, Option<String>)>,
) -> Result<usize> {
    use cellarr_core::BlocklistRepository;
    if !table_exists(pool, "Blocklist").await? {
        return Ok(0);
    }
    let mut written = 0;
    for r in sqlx::query(
        "SELECT SourceTitle, AlbumIds, Date, Protocol, Indexer, Message, TorrentInfoHash FROM Blocklist",
    )
    .fetch_all(pool)
    .await?
    {
        let title: String = r.try_get("SourceTitle").unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let at = timestamp(opt_text(&r, "Date"))
            .and_then(|d| time::OffsetDateTime::parse(&d, &time::format_description::well_known::Rfc3339).ok())
            .unwrap_or_else(time::OffsetDateTime::now_utc);
        // Lidarr records the protocol as text (`TorrentDownloadProtocol`), older
        // rows as a number (1 = usenet, 2 = torrent).
        let protocol = match opt_text(&r, "Protocol") {
            Some(p) => {
                if p.to_ascii_lowercase().contains("usenet") { "usenet" } else { "torrent" }
            }
            None => match r.try_get::<i64, _>("Protocol").ok() {
                Some(1) => "usenet",
                _ => "torrent",
            },
        };
        let album_ids: Vec<i64> = parse_json(opt_text(&r, "AlbumIds"))
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        for album in album_ids {
            let Some((content, _, _)) = albums.get(&album) else {
                continue;
            };
            let entry = cellarr_core::BlocklistEntry {
                id: uuid::Uuid::new_v4().to_string(),
                content_id: *content,
                release_key: title.trim().to_ascii_lowercase(),
                title: title.clone(),
                indexer: opt_text(&r, "Indexer"),
                protocol: Some(protocol.to_string()),
                reason: opt_text(&r, "Message").unwrap_or_else(|| "imported from Lidarr".to_string()),
                blocklisted_at: at,
            };
            BlocklistRepository::add(&db.blocklist(), &entry).await?;
            written += 1;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_keys_are_read_case_insensitively() {
        let v = lower_keys(serde_json::json!({ "Quality": { "Id": 6 }, "Allowed": true }));
        assert_eq!(v["quality"]["id"], 6);
        assert_eq!(v["allowed"], true);
    }

    #[test]
    fn a_reference_is_a_bare_id_or_an_object_with_one() {
        assert_eq!(ref_id(&serde_json::json!(6)), Some(6));
        assert_eq!(
            ref_id(&serde_json::json!({ "id": 6, "name": "FLAC" })),
            Some(6)
        );
        assert_eq!(ref_id(&serde_json::json!("x")), None);
    }

    #[test]
    fn timestamps_and_dates_are_normalized() {
        assert_eq!(
            date_part(Some("2019-05-01 00:00:00".into())).as_deref(),
            Some("2019-05-01")
        );
        assert_eq!(
            date_part(Some("2019-05-01T00:00:00Z".into())).as_deref(),
            Some("2019-05-01")
        );
        assert_eq!(date_part(Some("".into())), None);
        assert_eq!(
            timestamp(Some("2020-01-02 03:04:05".into())).as_deref(),
            Some("2020-01-02T03:04:05Z")
        );
        assert_eq!(
            timestamp(Some("2020-01-02T03:04:05Z".into())).as_deref(),
            Some("2020-01-02T03:04:05Z")
        );
    }

    #[test]
    fn a_quality_is_read_from_an_int_or_an_object() {
        let r = QualityRanking::default();
        assert_eq!(
            quality_of(&serde_json::json!({ "quality": 6 }), &r).name,
            "FLAC"
        );
        assert_eq!(
            quality_of(
                &serde_json::json!({ "quality": { "id": 4, "name": "MP3-320" } }),
                &r
            )
            .name,
            "MP3-320"
        );
        assert_eq!(
            quality_of(&serde_json::json!({ "quality": 999 }), &r).name,
            "Unknown"
        );
    }
}

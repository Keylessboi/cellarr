//! Lidarr API handlers for activity: the queue, history, the wanted lists, the
//! calendar, the blocklist and the release-title parser.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::Json;
use cellarr_core::{Album, Artist, ContentId, HistoryEvent, HistoryRecord};
use cellarr_db::AlbumSort;
use serde_json::{json, Value};

use super::handlers::profile_seqs;
use super::{app, page, resources};
use crate::error::{ApiError, ApiResult};
use crate::shim::{self, FaceState};
use crate::state::AppState;

type Params = Query<Vec<(String, String)>>;

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn number(q: &[(String, String)], key: &str, default: u64) -> u64 {
    param(q, key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn flag(q: &[(String, String)], key: &str) -> bool {
    param(q, key).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn today() -> String {
    time::OffsetDateTime::now_utc().date().to_string()
}

/// Everything needed to attach an album's and its artist's ids and resources to
/// a record, loaded once per request.
struct Library {
    albums: HashMap<ContentId, Album>,
    artists: HashMap<ContentId, Artist>,
    seqs: HashMap<cellarr_core::QualityProfileId, i64>,
}

impl Library {
    async fn load(state: &AppState) -> ApiResult<Self> {
        let music = state.db.music();
        let artists: HashMap<ContentId, Artist> = music
            .artists()
            .await?
            .into_iter()
            .map(|a| (a.content_id, a))
            .collect();
        let albums = music
            .albums()
            .await?
            .into_iter()
            .map(|a| (a.content_id, a))
            .collect();
        let seqs = profile_seqs(state, artists.values().filter_map(|a| a.quality_profile_id).collect()).await?;
        Ok(Self { albums, artists, seqs })
    }

    fn artist_json(&self, a: &Artist) -> Value {
        resources::artist_ref(a, a.quality_profile_id.and_then(|p| self.seqs.get(&p).copied()))
    }

    /// The `albumId`, `artistId` and (optionally) embedded resources of an album.
    fn attach(&self, record: &mut Value, album: ContentId, embed: bool) {
        let Some(al) = self.albums.get(&album) else {
            return;
        };
        let artist = self.artists.get(&al.artist_content_id);
        record["albumId"] = json!(al.id);
        record["artistId"] = json!(artist.map_or(0, |a| a.id));
        if embed {
            record["album"] = resources::album(
                al,
                artist.map_or(0, |a| a.id),
                None,
                None,
                None,
                &[],
            );
            if let Some(a) = artist {
                record["artist"] = self.artist_json(a);
            }
        }
    }
}

// --- queue ---------------------------------------------------------------------

async fn queue_records(fs: &FaceState, embed: bool) -> ApiResult<Vec<Value>> {
    let Json(base) = shim::queue(State(fs.clone())).await?;
    let library = Library::load(app(fs)).await?;
    let records = base
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(records.len());
    for mut r in records {
        let content = r
            .get("contentId")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(ContentId::from_uuid);
        if let Some(c) = content {
            library.attach(&mut r, c, embed);
        }
        let size = r["size"].as_u64().unwrap_or(0);
        let left = r["sizeleft"].as_u64().unwrap_or(0);
        r["quality"] = resources::quality_object("Unknown");
        r["customFormats"] = json!([]);
        r["customFormatScore"] = json!(0);
        r["sizeleft"] = json!(left.min(size));
        r["timeleft"] = Value::Null;
        r["estimatedCompletionTime"] = Value::Null;
        r["statusMessages"] = json!([]);
        r["added"] = json!(time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default());
        r["downloadClientHasPostImportCategory"] = json!(false);
        r["downloadForced"] = json!(false);
        r["outputPath"] = Value::Null;
        out.push(r);
    }
    Ok(out)
}

pub(crate) async fn queue(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let embed = flag(&q, "includeAlbum") || flag(&q, "includeArtist");
    let mut records = queue_records(&fs, embed).await?;
    if let Some(album) = param(&q, "albumIds").and_then(|v| v.parse::<i64>().ok()) {
        records.retain(|r| r["albumId"].as_i64() == Some(album));
    }
    let total = records.len() as u64;
    let size = number(&q, "pageSize", 10).max(1);
    let pg = number(&q, "page", 1).max(1);
    let start = usize::try_from((pg - 1) * size).unwrap_or(usize::MAX);
    let slice: Vec<Value> = records
        .into_iter()
        .skip(start)
        .take(usize::try_from(size).unwrap_or(usize::MAX))
        .collect();
    Ok(Json(page(
        slice,
        total,
        pg,
        size,
        param(&q, "sortKey").unwrap_or("timeleft"),
        param(&q, "sortDirection").unwrap_or("ascending"),
    )))
}

pub(crate) async fn queue_status(State(fs): State<FaceState>) -> ApiResult<Json<Value>> {
    let records = queue_records(&fs, false).await?;
    let errors = records.iter().filter(|r| r["trackedDownloadStatus"] == "error").count();
    let warnings = records.iter().filter(|r| r["trackedDownloadStatus"] == "warning").count();
    Ok(Json(json!({
        "id": 1,
        "totalCount": records.len(),
        "count": records.len(),
        "unknownCount": 0,
        "errors": errors > 0,
        "warnings": warnings > 0,
        "unknownErrors": false,
        "unknownWarnings": false,
    })))
}

pub(crate) async fn queue_details(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let mut records = queue_records(&fs, flag(&q, "includeAlbum")).await?;
    if let Some(album) = param(&q, "albumId").and_then(|v| v.parse::<i64>().ok()) {
        records.retain(|r| r["albumId"].as_i64() == Some(album));
    }
    Ok(Json(records))
}

/// Lidarr removes from the client unless told not to.
fn removal(q: &[(String, String)]) -> shim::QueueRemoveQuery {
    shim::QueueRemoveQuery {
        remove_from_client: Some(param(q, "removeFromClient").map_or(true, |v| v.eq_ignore_ascii_case("true"))),
        blocklist: Some(flag(q, "blocklist")),
    }
}

pub(crate) async fn queue_remove(
    State(fs): State<FaceState>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    shim::delete_queue_item(State(fs), Path(id), Query(removal(&q))).await?;
    Ok(Json(json!({})))
}

pub(crate) async fn queue_remove_bulk(
    State(fs): State<FaceState>,
    Query(q): Params,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let ids: Vec<i64> = body
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    for id in ids {
        shim::delete_queue_item(State(fs.clone()), Path(id.to_string()), Query(removal(&q))).await?;
    }
    Ok(Json(json!({})))
}

// --- history -------------------------------------------------------------------

/// Lidarr's numeric history event types.
fn event_kinds(code: &str) -> &'static [&'static str] {
    match code {
        "1" | "grabbed" => &["grabbed"],
        "3" | "trackFileImported" => &["imported", "upgraded"],
        "4" | "downloadFailed" => &["download_failed"],
        "5" | "trackFileDeleted" => &["deleted"],
        "8" | "downloadImported" => &["imported", "upgraded"],
        _ => &[],
    }
}

fn event_name(e: &HistoryEvent) -> (&'static str, u32) {
    match e {
        HistoryEvent::Grabbed { .. } => ("grabbed", 1),
        HistoryEvent::Imported { .. } | HistoryEvent::Upgraded { .. } => ("downloadImported", 8),
        HistoryEvent::DownloadFailed { .. } => ("downloadFailed", 4),
        HistoryEvent::Deleted { .. } => ("trackFileDeleted", 5),
        _ => ("unknown", 0),
    }
}

/// A stable id for a history row: the history stream has none of its own, so it
/// is derived from what makes a row distinct.
fn history_id(r: &HistoryRecord) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let key = format!("{}|{}|{}", r.run_id, r.at, serde_json::to_string(&r.event).unwrap_or_default());
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    i64::try_from(h & 0x7fff_ffff).unwrap_or(0)
}

fn history_record(r: &HistoryRecord, library: &Library, embed: bool) -> Value {
    let (name, _) = event_name(&r.event);
    let at = r
        .at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let title = library
        .albums
        .get(&r.content_id)
        .map(|al| {
            let artist = library.artists.get(&al.artist_content_id).map_or("", |a| a.name.as_str());
            format!("{artist} - {}", al.title)
        })
        .unwrap_or_default();
    let mut v = json!({
        "id": history_id(r),
        "albumId": 0,
        "artistId": 0,
        "trackId": 0,
        "sourceTitle": title,
        "quality": resources::quality_object("Unknown"),
        "customFormats": [],
        "customFormatScore": 0,
        "qualityCutoffNotMet": false,
        "date": at,
        "downloadId": null,
        "eventType": name,
        "data": {},
    });
    library.attach(&mut v, r.content_id, embed);
    v
}

pub(crate) async fn history(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let size = number(&q, "pageSize", 10).clamp(1, 1000);
    let pg = number(&q, "page", 1).max(1);
    // An id that names nothing yields an empty page, as in Lidarr, not an error:
    // clients ask about albums that may have been removed since.
    let empty = || page(Vec::new(), 0, pg, size, "date", "descending");
    let album = match param(&q, "albumId").and_then(|v| v.parse::<i64>().ok()).filter(|i| *i != 0) {
        Some(id) => match state.db.music().album(id).await? {
            Some(a) => Some(a.content_id),
            None => return Ok(Json(empty())),
        },
        None => None,
    };
    let artist = match param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()).filter(|i| *i != 0) {
        Some(id) => match state.db.music().artist(id).await? {
            Some(a) => Some(a.content_id),
            None => return Ok(Json(empty())),
        },
        None => None,
    };
    let kinds: Vec<&str> = q
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("eventType"))
        .flat_map(|(_, v)| event_kinds(v).iter().copied())
        .collect();
    let (total, records) = state
        .db
        .music()
        .history_page(album, artist, &kinds, None, (pg - 1) * size, size)
        .await?;
    let library = Library::load(state).await?;
    let embed = flag(&q, "includeAlbum") || flag(&q, "includeArtist");
    Ok(Json(page(
        records.iter().map(|r| history_record(r, &library, embed)).collect(),
        total,
        pg,
        size,
        param(&q, "sortKey").unwrap_or("date"),
        param(&q, "sortDirection").unwrap_or("descending"),
    )))
}

pub(crate) async fn history_since(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let since = param(&q, "date").ok_or_else(|| ApiError::BadRequest("date is required".into()))?;
    let kinds: Vec<&str> = param(&q, "eventType")
        .map(|c| event_kinds(c).to_vec())
        .unwrap_or_default();
    let (_, records) = state.db.music().history_page(None, None, &kinds, Some(since), 0, 1000).await?;
    let library = Library::load(state).await?;
    Ok(Json(records.iter().map(|r| history_record(r, &library, false)).collect()))
}

pub(crate) async fn history_artist(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let id = param(&q, "artistId")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| ApiError::BadRequest("artistId is required".into()))?;
    let artist = state
        .db
        .music()
        .artist(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("artist {id} not found")))?;
    let kinds: Vec<&str> = param(&q, "eventType").map(|c| event_kinds(c).to_vec()).unwrap_or_default();
    let (_, records) = state
        .db
        .music()
        .history_page(None, Some(artist.content_id), &kinds, None, 0, 1000)
        .await?;
    let library = Library::load(state).await?;
    Ok(Json(records.iter().map(|r| history_record(r, &library, false)).collect()))
}

// --- wanted --------------------------------------------------------------------

async fn album_page(
    state: &AppState,
    albums: &[Album],
    include_artist: bool,
) -> ApiResult<Vec<Value>> {
    let library = Library::load(state).await?;
    let stats = state.db.music().album_stats(None).await?;
    Ok(albums
        .iter()
        .map(|al| {
            let artist = library.artists.get(&al.artist_content_id);
            resources::album(
                al,
                artist.map_or(0, |a| a.id),
                include_artist.then(|| artist.map(|a| library.artist_json(a))).flatten(),
                Some(stats.get(&al.content_id).copied().unwrap_or_default()),
                None,
                &[],
            )
        })
        .collect())
}

pub(crate) async fn wanted_missing(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let size = number(&q, "pageSize", 10).clamp(1, 1000);
    let pg = number(&q, "page", 1).max(1);
    let key = param(&q, "sortKey").unwrap_or("albums.releaseDate");
    let sort = match key.to_ascii_lowercase().as_str() {
        k if k.contains("title") => AlbumSort::Title,
        k if k.contains("release") => AlbumSort::ReleaseDate,
        _ => AlbumSort::Id,
    };
    let ascending = !param(&q, "sortDirection").is_some_and(|d| d.eq_ignore_ascii_case("descending"));
    let (total, albums) = state
        .db
        .music()
        .missing_albums(&today(), sort, ascending, (pg - 1) * size, size)
        .await?;
    let records = album_page(state, &albums, flag(&q, "includeArtist")).await?;
    Ok(Json(page(
        records,
        total,
        pg,
        size,
        key,
        param(&q, "sortDirection").unwrap_or("ascending"),
    )))
}

pub(crate) async fn wanted_cutoff(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    use cellarr_core::repo::ProfileRepository;
    let state = app(&fs);
    let profiles: HashMap<cellarr_core::QualityProfileId, (u32, bool)> = state
        .db
        .profiles()
        .list_profiles()
        .await?
        .into_iter()
        .map(|p| (p.id, (p.cutoff_quality, p.upgrades_allowed)))
        .collect();
    let mut unmet = Vec::new();
    for (album, rank, profile) in state.db.music().album_min_quality().await? {
        if let Some((cutoff, upgrades)) = profile.and_then(|p| profiles.get(&p).copied()) {
            if upgrades && rank < cutoff {
                unmet.push(album);
            }
        }
    }
    let mut albums: Vec<Album> = state
        .db
        .music()
        .albums_by_content(&unmet)
        .await?
        .into_iter()
        .filter(|a| a.monitored)
        .collect();
    albums.sort_by(|a, b| b.release_date.cmp(&a.release_date).then(a.id.cmp(&b.id)));
    let total = albums.len() as u64;
    let size = number(&q, "pageSize", 10).clamp(1, 1000);
    let pg = number(&q, "page", 1).max(1);
    let slice: Vec<Album> = albums
        .into_iter()
        .skip(usize::try_from((pg - 1) * size).unwrap_or(usize::MAX))
        .take(usize::try_from(size).unwrap_or(usize::MAX))
        .collect();
    let records = album_page(state, &slice, flag(&q, "includeArtist")).await?;
    Ok(Json(page(records, total, pg, size, "albums.releaseDate", "descending")))
}

pub(crate) async fn calendar(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let start = param(&q, "start").map_or_else(String::new, |s| s.chars().take(10).collect());
    let end = param(&q, "end").map_or_else(|| "9999-12-31".to_string(), |s| s.chars().take(10).collect());
    let unmonitored = flag(&q, "unmonitored");
    let albums: Vec<Album> = state
        .db
        .music()
        .albums()
        .await?
        .into_iter()
        .filter(|a| {
            a.release_date.as_deref().is_some_and(|d| {
                // Partial dates compare by their first day.
                let day = match d.len() {
                    4 => format!("{d}-01-01"),
                    7 => format!("{d}-01"),
                    _ => d.to_string(),
                };
                day >= start && day <= end
            }) && (unmonitored || a.monitored)
        })
        .collect();
    Ok(Json(album_page(state, &albums, flag(&q, "includeArtist")).await?))
}

// --- blocklist -----------------------------------------------------------------

pub(crate) async fn blocklist(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    use cellarr_core::BlocklistRepository;
    let state = app(&fs);
    let library = Library::load(state).await?;
    let mut entries = BlocklistRepository::list(&state.db.blocklist()).await?;
    entries.sort_by(|a, b| b.blocklisted_at.cmp(&a.blocklisted_at));
    // Only music releases: an entry belongs to the music library when the album it
    // was blocked for is one.
    let music: Vec<_> = entries
        .into_iter()
        .filter(|e| library.albums.contains_key(&e.content_id))
        .collect();
    let total = music.len() as u64;
    let size = number(&q, "pageSize", 10).clamp(1, 1000);
    let pg = number(&q, "page", 1).max(1);
    let records: Vec<Value> = music
        .iter()
        .skip(usize::try_from((pg - 1) * size).unwrap_or(usize::MAX))
        .take(usize::try_from(size).unwrap_or(usize::MAX))
        .map(|e| {
            let mut v = json!({
                "id": shim::blocklist_numeric_id(&e.id),
                "albumIds": [],
                "sourceTitle": e.title,
                "quality": resources::quality_object("Unknown"),
                "customFormats": [],
                "customFormatScore": 0,
                "date": e.blocklisted_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                "protocol": e.protocol.clone().unwrap_or_else(|| "torrent".into()),
                "indexer": e.indexer.clone().unwrap_or_default(),
                "message": e.reason,
            });
            library.attach(&mut v, e.content_id, false);
            if let Some(id) = v["albumId"].as_i64() {
                v["albumIds"] = json!([id]);
            }
            v
        })
        .collect();
    Ok(Json(page(
        records,
        total,
        pg,
        size,
        param(&q, "sortKey").unwrap_or("date"),
        param(&q, "sortDirection").unwrap_or("descending"),
    )))
}

// --- parse ---------------------------------------------------------------------

pub(crate) async fn parse(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let title = param(&q, "title").ok_or_else(|| ApiError::BadRequest("title is required".into()))?;
    let parsed = cellarr_parse::parse_music_title(title);
    let music = parsed.music.clone().unwrap_or_default();
    let quality = music
        .quality
        .as_deref()
        .map_or_else(|| "Unknown".to_string(), str::to_string);
    let mut albums = Vec::new();
    if let (Some(artist), Some(album)) = (&music.artist, &music.album) {
        let found = state
            .db
            .music()
            .albums_named(
                &cellarr_core::normalize_name(artist),
                &[cellarr_core::normalize_name(album)],
            )
            .await?;
        albums = found.into_iter().map(|h| h.album).collect();
    }
    let embedded = album_page(state, &albums, true).await?;
    Ok(Json(json!({
        "id": 0,
        "title": title,
        "parsedAlbumInfo": {
            "albumTitle": music.album,
            "artistName": music.artist,
            "artistTitleInfo": { "title": music.artist, "titleWithoutYear": music.artist, "year": 0 },
            "quality": resources::quality_object(&quality),
            "releaseDate": parsed.year.map(|y| y.to_string()),
            "discography": music.discography,
            "releaseGroup": parsed.group,
            "releaseHash": "",
        },
        "artist": embedded.first().and_then(|a| a.get("artist")).cloned(),
        "albums": embedded,
    })))
}

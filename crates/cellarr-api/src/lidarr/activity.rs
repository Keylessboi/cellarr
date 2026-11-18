//! Lidarr API handlers for activity: the queue, history, the wanted lists, the
//! calendar, the blocklist and the release-title parser.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::Json;
use cellarr_core::{Album, Artist, ContentId};
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
    param(q, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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
    /// The artists (there are hundreds) and exactly the albums `ids` name. A page
    /// of history or a queue refresh must cost work proportional to the page, not
    /// to a library of tens of thousands of albums.
    async fn load(state: &AppState, ids: &[ContentId]) -> ApiResult<Self> {
        let music = state.db.music();
        let artists: HashMap<ContentId, Artist> = music
            .artists()
            .await?
            .into_iter()
            .map(|a| (a.content_id, a))
            .collect();
        let albums = music
            .albums_by_content(ids)
            .await?
            .into_iter()
            .map(|a| (a.content_id, a))
            .collect();
        let seqs = profile_seqs(
            state,
            artists
                .values()
                .filter_map(|a| a.quality_profile_id)
                .collect(),
        )
        .await?;
        Ok(Self {
            albums,
            artists,
            seqs,
        })
    }

    fn artist_json(&self, a: &Artist) -> Value {
        resources::artist_ref(
            a,
            a.quality_profile_id
                .and_then(|p| self.seqs.get(&p).copied()),
        )
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
            record["album"] =
                resources::album(al, artist.map_or(0, |a| a.id), None, None, None, &[]);
            if let Some(a) = artist {
                record["artist"] = self.artist_json(a);
            }
        }
    }
}

// --- queue ---------------------------------------------------------------------

async fn queue_records(fs: &FaceState, embed: bool) -> ApiResult<Vec<Value>> {
    let Json(base) = shim::queue(State(fs.clone())).await?;
    let records = base
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ids: Vec<ContentId> = records
        .iter()
        .filter_map(|r| r.get("contentId").and_then(Value::as_str))
        .filter_map(|s| s.parse::<uuid::Uuid>().ok())
        .map(ContentId::from_uuid)
        .collect();
    let library = Library::load(app(fs), &ids).await?;
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
        r["trackFileCount"] = json!(0);
        r["trackHasFileCount"] = json!(0);
        r["downloadClientHasPostImportCategory"] = json!(false);
        r["downloadForced"] = json!(false);
        r["outputPath"] = Value::Null;
        out.push(r);
    }
    Ok(out)
}

pub(crate) async fn queue(State(fs): State<FaceState>, Query(q): Params) -> ApiResult<Json<Value>> {
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
    let errors = records
        .iter()
        .filter(|r| r["trackedDownloadStatus"] == "error")
        .count();
    let warnings = records
        .iter()
        .filter(|r| r["trackedDownloadStatus"] == "warning")
        .count();
    Ok(Json(json!({
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
        remove_from_client: Some(
            param(q, "removeFromClient").is_none_or(|v| v.eq_ignore_ascii_case("true")),
        ),
        blocklist: Some(flag(q, "blocklist")),
    }
}

pub(crate) async fn queue_remove(
    State(fs): State<FaceState>,
    Path(id): Path<String>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let _ = shim::delete_queue_item(State(fs), Path(id), Query(removal(&q))).await?;
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
        let _ =
            shim::delete_queue_item(State(fs.clone()), Path(id.to_string()), Query(removal(&q)))
                .await?;
    }
    Ok(Json(json!({})))
}

// --- history -------------------------------------------------------------------

// --- wanted --------------------------------------------------------------------

async fn album_page(
    state: &AppState,
    albums: &[Album],
    include_artist: bool,
) -> ApiResult<Vec<Value>> {
    let ids: Vec<ContentId> = albums.iter().map(|a| a.content_id).collect();
    let library = Library::load(state, &[]).await?;
    let stats = state.db.music().album_stats_for(&ids).await?;
    Ok(albums
        .iter()
        .map(|al| {
            let artist = library.artists.get(&al.artist_content_id);
            resources::album(
                al,
                artist.map_or(0, |a| a.id),
                include_artist
                    .then(|| artist.map(|a| library.artist_json(a)))
                    .flatten(),
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
    let ascending =
        !param(&q, "sortDirection").is_some_and(|d| d.eq_ignore_ascii_case("descending"));
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
    Ok(Json(page(
        records,
        total,
        pg,
        size,
        "albums.releaseDate",
        "descending",
    )))
}

pub(crate) async fn calendar(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let start = param(&q, "start").map_or_else(String::new, |s| s.chars().take(10).collect());
    let end = param(&q, "end").map_or_else(
        || "9999-12-31".to_string(),
        |s| s.chars().take(10).collect(),
    );
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
    Ok(Json(
        album_page(state, &albums, flag(&q, "includeArtist")).await?,
    ))
}

// --- blocklist -----------------------------------------------------------------

pub(crate) async fn blocklist(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    use cellarr_core::BlocklistRepository;
    let state = app(&fs);
    let mut entries = BlocklistRepository::list(&state.db.blocklist()).await?;
    let ids: Vec<ContentId> = entries.iter().map(|e| e.content_id).collect();
    let library = Library::load(state, &ids).await?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.blocklisted_at));
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

pub(crate) async fn parse(State(fs): State<FaceState>, Query(q): Params) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let title =
        param(&q, "title").ok_or_else(|| ApiError::BadRequest("title is required".into()))?;
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

/// One album, by id, the way the calendar and the wanted lists address it.
pub(crate) async fn album_by_id(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let album = state
        .db
        .music()
        .album(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("album {id} not found")))?;
    Ok(Json(
        album_page(state, &[album], flag(&q, "includeArtist"))
            .await?
            .remove(0),
    ))
}

// --- music history, as Lidarr serves it ---------------------------------------

/// Lidarr's name for an event number.
fn event_type_name(n: i64) -> &'static str {
    match n {
        1 => "grabbed",
        2 => "artistFolderImported",
        3 => "trackFileImported",
        4 => "downloadFailed",
        5 => "trackFileDeleted",
        6 => "trackFileRenamed",
        7 => "albumImportIncomplete",
        8 => "downloadImported",
        9 => "trackFileRetagged",
        10 => "downloadIgnored",
        _ => "unknown",
    }
}

/// An event filter value: Lidarr's number, or its name.
fn event_type_number(v: &str) -> Option<i64> {
    v.parse::<i64>()
        .ok()
        .or_else(|| (0..=10).find(|n| event_type_name(*n).eq_ignore_ascii_case(v.trim())))
}

/// Lidarr keeps `data` as strings; so do its clients.
fn string_data(data: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(o) = data.as_object() {
        for (k, v) in o {
            match v {
                Value::Null => {}
                Value::String(s) => {
                    out.insert(k.clone(), json!(s));
                }
                other => {
                    out.insert(k.clone(), json!(other.to_string()));
                }
            }
        }
    }
    Value::Object(out)
}

async fn render_music_history(
    state: &AppState,
    rows: &[cellarr_db::MusicHistoryRow],
    embed_album: bool,
    embed_artist: bool,
) -> ApiResult<Vec<Value>> {
    render_music_history_cached(
        state,
        rows,
        embed_album,
        embed_artist,
        &mut HashMap::new(),
        &mut HashMap::new(),
    )
    .await
}

/// [`render_music_history`] with the embedded albums and artists kept across
/// calls, so a walk over many pages renders each album once.
async fn render_music_history_cached(
    state: &AppState,
    rows: &[cellarr_db::MusicHistoryRow],
    embed_album: bool,
    embed_artist: bool,
    albums: &mut HashMap<i64, Value>,
    artists: &mut HashMap<i64, Value>,
) -> ApiResult<Vec<Value>> {
    let music = state.db.music();
    if embed_album || embed_artist {
        let mut seen = std::collections::BTreeSet::new();
        let mut list = Vec::new();
        for r in rows {
            if r.album_id > 0 && !albums.contains_key(&r.album_id) && seen.insert(r.album_id) {
                if let Some(a) = music.album(r.album_id).await? {
                    list.push(a);
                }
            }
        }
        for v in crate::lidarr::handlers::render_albums(state, &list).await? {
            if let Some(id) = v["id"].as_i64() {
                if let Some(artist) = v.get("artist").cloned() {
                    if let Some(aid) = artist["id"].as_i64() {
                        artists.insert(aid, artist);
                    }
                }
                albums.insert(id, v);
            }
        }
    }
    Ok(rows
        .iter()
        .map(|r| {
            let name = r.quality["quality"]
                .as_i64()
                .and_then(|id| u32::try_from(id).ok())
                .and_then(cellarr_core::lidarr_quality_name)
                .unwrap_or("Unknown");
            let mut v = json!({
                "id": r.id,
                "albumId": r.album_id,
                "artistId": r.artist_id,
                "trackId": r.track_id,
                "sourceTitle": r.source_title,
                "quality": {
                    "quality": { "id": cellarr_core::lidarr_quality_id(name), "name": name },
                    "revision": r.quality.get("revision").cloned().unwrap_or(json!({ "version": 1, "real": 0, "isRepack": false })),
                },
                "customFormats": [],
                "customFormatScore": 0,
                "qualityCutoffNotMet": false,
                "date": r.date,
                "downloadId": r.download_id,
                "eventType": event_type_name(r.event_type),
                "data": string_data(&r.data),
            });
            if embed_album {
                if let Some(a) = albums.get(&r.album_id) {
                    v["album"] = a.clone();
                }
            }
            if embed_artist {
                if let Some(a) = artists.get(&r.artist_id) {
                    v["artist"] = a.clone();
                }
            }
            v
        })
        .collect())
}

fn history_filter(
    q: &std::collections::HashMap<String, String>,
    raw: &[(String, String)],
) -> cellarr_db::MusicHistoryFilter {
    let mut f = cellarr_db::MusicHistoryFilter {
        album_id: q
            .get("albumId")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|i| *i != 0),
        download_id: q.get("downloadId").filter(|d| !d.is_empty()).cloned(),
        ..Default::default()
    };
    for (k, v) in raw {
        if k.eq_ignore_ascii_case("eventType") {
            for part in v.split(',') {
                if let Some(n) = event_type_number(part) {
                    f.event_types.push(n);
                }
            }
        }
        if k.eq_ignore_ascii_case("artistIds") || k.eq_ignore_ascii_case("artistId") {
            for part in v.split(',') {
                if let Ok(n) = part.trim().parse::<i64>() {
                    f.artist_ids.push(n);
                }
            }
        }
    }
    f
}

/// `GET /history`: a page of music history, filtered as Lidarr filters it.
pub(crate) async fn music_history_page(
    State(fs): State<FaceState>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let pairs = query_pairs(raw.as_deref());
    let q: std::collections::HashMap<String, String> = pairs.iter().cloned().collect();
    let size = q
        .get("pageSize")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10)
        .clamp(1, 1000);
    let pg = q
        .get("page")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1)
        .max(1);
    let ascending = q
        .get("sortDirection")
        .is_some_and(|d| d.eq_ignore_ascii_case("ascending"));
    let filter = history_filter(&q, &pairs);
    let (total, rows) = state
        .db
        .music()
        .music_history(&filter, ascending, (pg - 1) * size, size)
        .await?;
    let truthy = |k: &str| q.get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let records = render_music_history(
        state,
        &rows,
        truthy("includeAlbum"),
        truthy("includeArtist"),
    )
    .await?;
    Ok(Json(json!({
        "page": pg,
        "pageSize": size,
        "sortKey": q.get("sortKey").cloned().unwrap_or_else(|| "date".into()),
        "sortDirection": if ascending { "ascending" } else { "descending" },
        "totalRecords": total,
        "records": records,
    })))
}

/// Every matching row as one JSON array, streamed to the client a page at a
/// time. An artist's history can run to hundreds of thousands of rows, and with
/// `includeAlbum` each carries its album: the whole answer can be gigabytes, so
/// it is never held in memory. The first page is rendered before the response
/// starts, so a bad query is still a proper error status; a failure later can
/// only cut the stream short.
async fn history_array(
    state: &AppState,
    filter: &cellarr_db::MusicHistoryFilter,
    ascending: bool,
    embed_album: bool,
    embed_artist: bool,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let mut walk = HistoryWalk {
        state: state.clone(),
        filter: filter.clone(),
        ascending,
        embed_album,
        embed_artist,
        albums: HashMap::new(),
        artists: HashMap::new(),
        first: true,
        done: false,
    };
    let head = walk.next_chunk().await?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(2);
    tokio::spawn(async move {
        if tx.send(Ok(head)).await.is_err() {
            return;
        }
        while !walk.done {
            let chunk = walk
                .next_chunk()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()));
            let failed = chunk.is_err();
            if tx.send(chunk).await.is_err() || failed {
                return;
            }
        }
    });
    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response())
}

/// A keyset walk over music history, rendering one page of JSON at a time.
struct HistoryWalk {
    state: AppState,
    filter: cellarr_db::MusicHistoryFilter,
    ascending: bool,
    embed_album: bool,
    embed_artist: bool,
    albums: HashMap<i64, Value>,
    artists: HashMap<i64, Value>,
    first: bool,
    done: bool,
}

impl HistoryWalk {
    const PAGE: u64 = 2000;
    /// Embedded albums kept between pages; past this the cache starts over so
    /// a walk across a whole library stays bounded.
    const CACHE: usize = 1000;

    async fn next_chunk(&mut self) -> ApiResult<Vec<u8>> {
        let mut out = Vec::new();
        if self.first {
            out.push(b'[');
        }
        let (_, rows) = self
            .state
            .db
            .music()
            .music_history(&self.filter, self.ascending, 0, Self::PAGE)
            .await?;
        self.filter.after = rows.last().and_then(|r| Some((r.date.clone(), r.id?)));
        if self.albums.len() > Self::CACHE {
            self.albums.clear();
            self.artists.clear();
        }
        let page = render_music_history_cached(
            &self.state,
            &rows,
            self.embed_album,
            self.embed_artist,
            &mut self.albums,
            &mut self.artists,
        )
        .await?;
        for mut v in page {
            super::strip_foreign(&mut v);
            if !self.first {
                out.push(b',');
            }
            self.first = false;
            serde_json::to_writer(&mut out, &v).map_err(|e| ApiError::Internal(e.to_string()))?;
        }
        if (rows.len() as u64) < Self::PAGE || self.filter.after.is_none() {
            if self.first {
                // No rows at all: the opening bracket went out above.
                self.first = false;
            }
            out.push(b']');
            self.done = true;
        }
        Ok(out)
    }
}

/// `GET /history/artist?artistId=[&albumId=][&eventType=]`: every row, newest first.
pub(crate) async fn music_history_artist(
    State(fs): State<FaceState>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> ApiResult<axum::response::Response> {
    let state = app(&fs);
    let pairs = query_pairs(raw.as_deref());
    let q: std::collections::HashMap<String, String> = pairs.iter().cloned().collect();
    let filter = history_filter(&q, &pairs);
    if filter.artist_ids.is_empty() {
        return Err(ApiError::BadRequest("artistId is required".into()));
    }
    let truthy = |k: &str| q.get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    history_array(
        state,
        &filter,
        false,
        truthy("includeAlbum"),
        truthy("includeArtist"),
    )
    .await
}

/// `GET /history/since?date=[&eventType=]`: every row on or after `date`, oldest first.
pub(crate) async fn music_history_since(
    State(fs): State<FaceState>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> ApiResult<axum::response::Response> {
    let state = app(&fs);
    let pairs = query_pairs(raw.as_deref());
    let q: std::collections::HashMap<String, String> = pairs.iter().cloned().collect();
    let mut filter = history_filter(&q, &pairs);
    filter.since = q.get("date").cloned();
    let truthy = |k: &str| q.get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    history_array(
        state,
        &filter,
        true,
        truthy("includeAlbum"),
        truthy("includeArtist"),
    )
    .await
}

/// Decode a raw query string into ordered pairs (repeated keys kept).
fn query_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    let decode = |s: &str| {
        let s = s.replace('+', " ");
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    };
    raw.unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

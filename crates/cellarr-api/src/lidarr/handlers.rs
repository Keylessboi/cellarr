//! Lidarr API handlers for the music library: artists, albums, tracks, files and
//! the profiles that govern them.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::Json;
use cellarr_core::repo::ProfileRepository;
use cellarr_core::{
    Album, Artist, ContentId, Library, LibraryId, MediaType, MetadataProfile, QualityProfile,
    QualityProfileId, QualityRanking,
};
use cellarr_db::{MusicStats, NewAlbum};
use cellarr_jobs::music_sync::{AddArtist, MonitorMode};
use serde_json::{json, Value};

use super::quality::{id_for_rank, lidarr_quality_id, rank_for_id};
use super::{app, catalogue_error, resources};
use crate::error::{ApiError, ApiResult};
use crate::shim::{self, FaceState};
use crate::state::AppState;

type Params = Query<Vec<(String, String)>>;

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn params<'a>(q: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    q.iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
        .collect()
}

fn flag(q: &[(String, String)], key: &str) -> bool {
    param(q, key).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn id_list(body: &Value, key: &str) -> Vec<i64> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// The integer id of each profile in `wanted`, assigning any not yet numbered.
pub(crate) async fn profile_seqs(
    state: &AppState,
    wanted: Vec<QualityProfileId>,
) -> ApiResult<HashMap<QualityProfileId, i64>> {
    let mut known = state.db.music().profile_seqs().await?;
    for id in wanted {
        if !known.contains_key(&id) {
            let seq = state.db.music().profile_seq(id).await?;
            known.insert(id, seq);
        }
    }
    Ok(known)
}

async fn artist_or_404(state: &AppState, id: i64) -> ApiResult<Artist> {
    state
        .db
        .music()
        .artist(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("artist {id} not found")))
}

async fn album_or_404(state: &AppState, id: i64) -> ApiResult<Album> {
    state
        .db
        .music()
        .album(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("album {id} not found")))
}

async fn artist_resource(state: &AppState, a: &Artist) -> ApiResult<Value> {
    let stats = state.db.music().artist_stats().await?;
    let seqs = profile_seqs(state, a.quality_profile_id.into_iter().collect()).await?;
    Ok(resources::artist(
        a,
        Some(stats.get(&a.content_id).copied().unwrap_or_default()),
        a.quality_profile_id.and_then(|p| seqs.get(&p).copied()),
    ))
}

/// Render albums with their artists and statistics.
async fn album_resources(state: &AppState, albums: &[Album], with_tracks: bool) -> ApiResult<Vec<Value>> {
    let music = state.db.music();
    let artists: HashMap<ContentId, Artist> = music
        .artists()
        .await?
        .into_iter()
        .map(|a| (a.content_id, a))
        .collect();
    let seqs = profile_seqs(state, artists.values().filter_map(|a| a.quality_profile_id).collect()).await?;
    let seq_of = |a: &Artist| a.quality_profile_id.and_then(|p| seqs.get(&p).copied());
    let stats: HashMap<ContentId, MusicStats> = music.album_stats(None).await?;
    let mut out = Vec::with_capacity(albums.len());
    for al in albums {
        let artist = artists.get(&al.artist_content_id);
        let tracks = if with_tracks {
            music.tracks_for_album(al.content_id).await?
        } else {
            Vec::new()
        };
        out.push(resources::album(
            al,
            artist.map_or(0, |a| a.id),
            artist.map(|a| resources::artist_ref(a, seq_of(a))),
            Some(stats.get(&al.content_id).copied().unwrap_or_default()),
            artist.and_then(seq_of),
            &tracks,
        ));
    }
    Ok(out)
}

// --- artists -----------------------------------------------------------------

pub(crate) async fn list_artists(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let music = state.db.music();
    let mut artists = music.artists().await?;
    if let Some(mbid) = param(&q, "mbId").or_else(|| param(&q, "foreignArtistId")) {
        artists.retain(|a| a.mbid.eq_ignore_ascii_case(mbid));
    }
    let stats = music.artist_stats().await?;
    let seqs = profile_seqs(state, artists.iter().filter_map(|a| a.quality_profile_id).collect()).await?;
    Ok(Json(
        artists
            .iter()
            .map(|a| {
                resources::artist(
                    a,
                    Some(stats.get(&a.content_id).copied().unwrap_or_default()),
                    a.quality_profile_id.and_then(|p| seqs.get(&p).copied()),
                )
            })
            .collect(),
    ))
}

pub(crate) async fn get_artist(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let a = artist_or_404(state, id).await?;
    Ok(Json(artist_resource(state, &a).await?))
}

/// The library music is added to, created on first use from the root folder the
/// request names.
async fn music_library(state: &AppState, root: Option<&str>) -> ApiResult<Library> {
    match shim::pick_library(state, MediaType::Music, root).await {
        Ok(lib) => Ok(lib),
        Err(ApiError::NotFound(_)) => {
            let profile = state
                .db
                .profiles()
                .list_profiles()
                .await?
                .into_iter()
                .next()
                .map_or_else(QualityProfileId::new, |p| p.id);
            let lib = Library {
                id: LibraryId::new(),
                media_type: MediaType::Music,
                name: "Music".to_string(),
                root_folders: root.map(str::to_string).into_iter().collect(),
                default_quality_profile: profile,
            };
            state.db.config().upsert_library(&lib).await?;
            Ok(lib)
        }
        Err(e) => Err(e),
    }
}

/// A name made safe to be one path component.
fn safe_folder(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches('.').trim().to_string();
    if trimmed.is_empty() { "Unknown Artist".to_string() } else { trimmed }
}

async fn resolve_profile(state: &AppState, body: &Value) -> ApiResult<Option<QualityProfileId>> {
    match body.get("qualityProfileId").and_then(Value::as_i64) {
        Some(seq) if seq > 0 => state
            .db
            .music()
            .profile_by_seq(seq)
            .await?
            .map(Some)
            .ok_or_else(|| ApiError::BadRequest(format!("unknown quality profile {seq}"))),
        _ => Ok(None),
    }
}

/// Add an artist from a Lidarr-shaped body, returning the stored artist.
async fn add_artist_from(state: &AppState, body: &Value) -> ApiResult<Artist> {
    let catalogue = state
        .music
        .as_ref()
        .ok_or_else(|| ApiError::Domain("no music metadata source is configured".into()))?;
    let mbid = body
        .get("foreignArtistId")
        .or_else(|| body.get("mbId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("foreignArtistId is required".into()))?;
    if let Some(existing) = state.db.music().artist_by_mbid(mbid).await? {
        return Ok(existing);
    }
    let name = body.get("artistName").and_then(Value::as_str).unwrap_or(mbid);
    let root = body.get("rootFolderPath").and_then(Value::as_str).filter(|s| !s.is_empty());
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| root.map(|r| format!("{}/{}", r.trim_end_matches('/'), safe_folder(name))))
        .ok_or_else(|| ApiError::BadRequest("a rootFolderPath or path is required".into()))?;
    let library = music_library(state, root).await?;
    let options = body.get("addOptions").cloned().unwrap_or(Value::Null);
    let monitor = options
        .get("monitor")
        .and_then(Value::as_str)
        .map_or(MonitorMode::All, MonitorMode::parse);
    let monitor_new = body
        .get("monitorNewItems")
        .and_then(Value::as_str)
        .map_or(true, |m| !m.eq_ignore_ascii_case("none"));
    let artist = catalogue
        .add_artist(AddArtist {
            library: library.id,
            mbid: mbid.to_string(),
            path,
            quality_profile_id: resolve_profile(state, body).await?.or(Some(library.default_quality_profile)),
            metadata_profile_id: body.get("metadataProfileId").and_then(Value::as_i64).unwrap_or(1),
            monitored: body.get("monitored").and_then(Value::as_bool).unwrap_or(true),
            monitor,
            monitor_new_albums: monitor_new,
        })
        .await
        .map_err(catalogue_error)?;
    if options.get("searchForMissingAlbums").and_then(Value::as_bool).unwrap_or(false) {
        super::command::search_artist(state, &artist).await?;
    }
    Ok(artist)
}

pub(crate) async fn add_artist(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let artist = add_artist_from(state, &body).await?;
    Ok(Json(artist_resource(state, &artist).await?))
}

pub(crate) async fn update_artist(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let mut a = artist_or_404(state, id).await?;
    apply_artist_fields(state, &mut a, &body).await?;
    state.db.music().update_artist(&a, None).await?;
    Ok(Json(artist_resource(state, &a).await?))
}

/// Apply the editable fields present in `body` to an artist. A field that is
/// absent is left alone, which is what makes this serve both a full PUT and the
/// partial bulk editor.
async fn apply_artist_fields(state: &AppState, a: &mut Artist, body: &Value) -> ApiResult<()> {
    if let Some(m) = body.get("monitored").and_then(Value::as_bool) {
        a.monitored = m;
    }
    if let Some(m) = body.get("monitorNewItems").and_then(Value::as_str) {
        a.monitor_new_albums = !m.eq_ignore_ascii_case("none");
    }
    if let Some(p) = body.get("metadataProfileId").and_then(Value::as_i64).filter(|p| *p > 0) {
        a.metadata_profile_id = p;
    }
    if body.get("qualityProfileId").and_then(Value::as_i64).is_some_and(|p| p > 0) {
        a.quality_profile_id = resolve_profile(state, body).await?;
    }
    if let Some(path) = body.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()) {
        a.path = path.to_string();
    } else if let Some(root) = body.get("rootFolderPath").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        a.path = format!("{}/{}", root.trim_end_matches('/'), resources::folder_of(&a.path));
    }
    Ok(())
}

async fn remove_artist(state: &AppState, artist: &Artist, delete_files: bool) -> ApiResult<()> {
    let paths = state.db.music().delete_artist(artist.content_id).await?;
    if delete_files {
        if let Some(node) = state.db.content().get_node(artist.content_id).await.ok().flatten() {
            shim::recycle_content_files(state, &node, &paths).await?;
        } else {
            tracing::warn!(artist = artist.id, "artist already gone; its files were not removed");
        }
    }
    Ok(())
}

pub(crate) async fn delete_artist(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let a = artist_or_404(state, id).await?;
    let node = state.db.content().get_node(a.content_id).await?;
    // Files are removed against the library root, so look the node up before the
    // rows disappear.
    let paths = state.db.music().delete_artist(a.content_id).await?;
    if flag(&q, "deleteFiles") {
        if let Some(node) = node {
            shim::recycle_content_files(state, &node, &paths).await?;
        }
    }
    Ok(Json(json!({})))
}

pub(crate) async fn artist_editor(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let mut out = Vec::new();
    for id in id_list(&body, "artistIds") {
        let mut a = artist_or_404(state, id).await?;
        apply_artist_fields(state, &mut a, &body).await?;
        state.db.music().update_artist(&a, None).await?;
        out.push(artist_resource(state, &a).await?);
    }
    Ok(Json(out))
}

pub(crate) async fn artist_editor_delete(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let delete_files = body.get("deleteFiles").and_then(Value::as_bool).unwrap_or(false);
    for id in id_list(&body, "artistIds") {
        if let Some(a) = state.db.music().artist(id).await? {
            let node = state.db.content().get_node(a.content_id).await?;
            let paths = state.db.music().delete_artist(a.content_id).await?;
            if delete_files {
                if let Some(node) = node {
                    shim::recycle_content_files(state, &node, &paths).await?;
                }
            }
        }
    }
    let _ = remove_artist;
    Ok(Json(json!({})))
}

/// A lookup term of the form `lidarr:<mbid>` / `mbid:<mbid>` names one entity.
fn mbid_term(term: &str) -> Option<&str> {
    let t = term.trim();
    ["lidarr:", "mbid:", "lidarrid:"]
        .iter()
        .find_map(|p| t.strip_prefix(p))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) async fn artist_lookup(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let term = param(&q, "term").unwrap_or_default().trim();
    if term.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let catalogue = state
        .music
        .as_ref()
        .ok_or_else(|| ApiError::Domain("no music metadata source is configured".into()))?;
    let hits = match mbid_term(term) {
        Some(mbid) => {
            let profile = state.db.music().metadata_profile(1).await?.unwrap_or_else(MetadataProfile::standard);
            vec![catalogue.lookup_artist(mbid, &profile).await.map_err(catalogue_error)?.0]
        }
        None => catalogue.search_artists(term).await.map_err(catalogue_error)?,
    };
    let known: HashMap<String, Artist> = state
        .db
        .music()
        .artists()
        .await?
        .into_iter()
        .map(|a| (a.mbid.clone(), a))
        .collect();
    Ok(Json(hits.iter().map(|h| lookup_artist_resource(h, known.get(&h.mbid))).collect()))
}

fn lookup_artist_resource(h: &cellarr_meta::MbArtist, known: Option<&Artist>) -> Value {
    json!({
        "id": known.map_or(0, |a| a.id),
        "status": if h.ended { "ended" } else { "continuing" },
        "ended": h.ended,
        "artistName": h.name,
        "foreignArtistId": h.mbid,
        "mbId": h.mbid,
        "tadbId": 0,
        "discogsId": 0,
        "allMusicId": "",
        "overview": "",
        "artistType": h.artist_type.clone().unwrap_or_default(),
        "disambiguation": h.disambiguation.clone().unwrap_or_default(),
        "links": [],
        "images": [],
        "genres": h.genres,
        "cleanName": resources::clean_name(&h.name),
        "sortName": h.sort_name.clone().unwrap_or_else(|| h.name.to_lowercase()),
        "tags": [],
        "ratings": { "votes": 0, "value": 0.0 },
        "monitored": known.is_some_and(|a| a.monitored),
        "path": known.map(|a| a.path.clone()),
        "qualityProfileId": 0,
        "metadataProfileId": 0,
        "monitorNewItems": "all",
    })
}

// --- albums --------------------------------------------------------------------

pub(crate) async fn list_albums(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let music = state.db.music();
    let albums: Vec<Album> = if let Some(mbid) = param(&q, "foreignAlbumId") {
        music.album_by_mbid(mbid).await?.into_iter().collect()
    } else if let Some(artist) = param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
        let a = artist_or_404(state, artist).await?;
        music.albums_for_artist(a.content_id).await?
    } else {
        let ids: HashSet<i64> = params(&q, "albumIds").iter().filter_map(|v| v.parse().ok()).collect();
        let all = music.albums().await?;
        if ids.is_empty() { all } else { all.into_iter().filter(|a| ids.contains(&a.id)).collect() }
    };
    Ok(Json(album_resources(state, &albums, flag(&q, "includeTracks")).await?))
}

pub(crate) async fn get_album(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let al = album_or_404(state, id).await?;
    Ok(Json(album_resources(state, &[al], true).await?.remove(0)))
}

pub(crate) async fn update_album(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let al = album_or_404(state, id).await?;
    if let Some(m) = body.get("monitored").and_then(Value::as_bool) {
        state.db.music().set_monitored(vec![al.content_id], m).await?;
    }
    let fresh = album_or_404(state, id).await?;
    Ok(Json(album_resources(state, &[fresh], true).await?.remove(0)))
}

pub(crate) async fn delete_album(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let al = album_or_404(state, id).await?;
    let node = state.db.content().get_node(al.content_id).await?;
    let paths = state.db.music().delete_album(al.content_id).await?;
    if flag(&q, "deleteFiles") {
        if let Some(node) = node {
            shim::recycle_content_files(state, &node, &paths).await?;
        }
    }
    Ok(Json(json!({})))
}

pub(crate) async fn album_monitor(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let monitored = body.get("monitored").and_then(Value::as_bool).unwrap_or(true);
    let mut albums = Vec::new();
    for id in id_list(&body, "albumIds") {
        albums.push(album_or_404(state, id).await?);
    }
    state
        .db
        .music()
        .set_monitored(albums.iter().map(|a| a.content_id).collect(), monitored)
        .await?;
    let mut fresh = Vec::new();
    for a in &albums {
        fresh.push(album_or_404(state, a.id).await?);
    }
    Ok(Json(album_resources(state, &fresh, false).await?))
}

/// Add an album (and its artist, if the library does not hold it yet).
pub(crate) async fn add_album(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let mbid = body
        .get("foreignAlbumId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("foreignAlbumId is required".into()))?;
    if let Some(existing) = state.db.music().album_by_mbid(mbid).await? {
        return Ok(Json(album_resources(state, &[existing], true).await?.remove(0)));
    }
    // The album's artist: by id when the client names one we hold, else added.
    let artist = match body.get("artistId").and_then(Value::as_i64).filter(|i| *i > 0) {
        Some(id) => artist_or_404(state, id).await?,
        None => {
            let artist_body = body
                .get("artist")
                .ok_or_else(|| ApiError::BadRequest("an album needs its artist".into()))?;
            add_artist_from(state, artist_body).await?
        }
    };
    let node = state
        .db
        .content()
        .get_node(artist.content_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("artist node missing".into()))?;
    // The album may have been filtered out of the artist by its metadata profile
    // (an EP, a single). Asking for it by name overrides the profile: add it.
    if state.db.music().album_by_mbid(mbid).await?.is_none() {
        let secondary: Vec<String> = body
            .get("secondaryTypes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        state
            .db
            .music()
            .upsert_albums(
                node.library_id,
                artist.content_id,
                vec![NewAlbum {
                    id: None,
                    mbid: mbid.to_string(),
                    title: body.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
                    disambiguation: body.get("disambiguation").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
                    album_type: body.get("albumType").and_then(Value::as_str).unwrap_or("Album").to_string(),
                    secondary_types: secondary,
                    release_date: body
                        .get("releaseDate")
                        .and_then(Value::as_str)
                        .map(|d| d.chars().take(10).collect()),
                    release_mbid: None,
                    monitored: body.get("monitored").and_then(Value::as_bool).unwrap_or(true),
                    added_at: None,
                }],
            )
            .await?;
    }
    let album = state
        .db
        .music()
        .album_by_mbid(mbid)
        .await?
        .ok_or_else(|| ApiError::Internal("album vanished after being added".into()))?;
    if body.get("monitored").and_then(Value::as_bool).unwrap_or(true) && !album.monitored {
        state.db.music().set_monitored(vec![album.content_id], true).await?;
    }
    let fresh = album_or_404(state, album.id).await?;
    Ok(Json(album_resources(state, &[fresh], true).await?.remove(0)))
}

pub(crate) async fn album_lookup(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let term = param(&q, "term").unwrap_or_default().trim();
    if term.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let catalogue = state
        .music
        .as_ref()
        .ok_or_else(|| ApiError::Domain("no music metadata source is configured".into()))?;
    let hits = catalogue.search_albums(term).await.map_err(catalogue_error)?;
    let known_albums: HashMap<String, Album> = state
        .db
        .music()
        .albums()
        .await?
        .into_iter()
        .map(|a| (a.mbid.clone(), a))
        .collect();
    let known_artists: HashMap<String, Artist> = state
        .db
        .music()
        .artists()
        .await?
        .into_iter()
        .map(|a| (a.mbid.clone(), a))
        .collect();
    let mut out = Vec::new();
    for h in hits {
        let (artist_mbid, artist_name) = h.artist.clone().unwrap_or_default();
        let artist = known_artists.get(&artist_mbid);
        let artist_json = artist.map_or_else(
            || {
                json!({
                    "id": 0,
                    "artistName": artist_name,
                    "foreignArtistId": artist_mbid,
                    "mbId": artist_mbid,
                    "monitored": false,
                    "qualityProfileId": 0,
                    "metadataProfileId": 0,
                    "tags": [],
                    "images": [],
                    "links": [],
                    "genres": [],
                    "path": null,
                })
            },
            |a| resources::artist_ref(a, None),
        );
        let existing = known_albums.get(&h.mbid);
        out.push(json!({
            "id": existing.map_or(0, |a| a.id),
            "title": h.title,
            "disambiguation": h.disambiguation.clone().unwrap_or_default(),
            "overview": "",
            "artistId": artist.map_or(0, |a| a.id),
            "foreignAlbumId": h.mbid,
            "monitored": existing.is_some_and(|a| a.monitored),
            "anyReleaseOk": true,
            "profileId": 0,
            "duration": 0,
            "albumType": h.primary_type,
            "secondaryTypes": h.secondary_types,
            "mediumCount": 0,
            "ratings": { "votes": 0, "value": 0.0 },
            "releaseDate": resources::release_date_iso(h.first_release_date.as_deref()),
            "releases": [],
            "genres": [],
            "media": [],
            "artist": artist_json,
            "images": [{
                "coverType": "cover",
                "url": format!("https://coverartarchive.org/release-group/{}/front-500", h.mbid),
                "remoteUrl": format!("https://coverartarchive.org/release-group/{}/front-500", h.mbid),
            }],
            "links": [],
        }));
    }
    Ok(Json(out))
}

// --- tracks and files ----------------------------------------------------------

async fn tracks_for_albums(state: &AppState, albums: &[Album]) -> ApiResult<Vec<Value>> {
    let music = state.db.music();
    let artists: HashMap<ContentId, Artist> = music
        .artists()
        .await?
        .into_iter()
        .map(|a| (a.content_id, a))
        .collect();
    let mut out = Vec::new();
    for al in albums {
        // A track list is fetched the first time it is asked for.
        if al.tracks_fetched_at.is_none() {
            if let Some(catalogue) = &state.music {
                if let Err(e) = catalogue.ensure_tracks(al.content_id).await {
                    tracing::warn!(album = al.id, error = %e, "could not fetch the album's tracks");
                }
            }
        }
        let files: HashMap<ContentId, cellarr_core::TrackFile> = music
            .track_files_for_album(al.content_id)
            .await?
            .into_iter()
            .map(|f| (f.track_content_id, f))
            .collect();
        let artist = artists.get(&al.artist_content_id);
        let artist_id = artist.map_or(0, |a| a.id);
        for t in music.tracks_for_album(al.content_id).await? {
            let file = files.get(&t.content_id);
            out.push(resources::track(
                &t,
                al.id,
                artist_id,
                file,
                artist.map(|a| resources::artist_ref(a, None)),
            ));
        }
    }
    Ok(out)
}

pub(crate) async fn list_tracks(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let music = state.db.music();
    let albums: Vec<Album> = if let Some(id) = param(&q, "albumId").and_then(|v| v.parse::<i64>().ok()) {
        vec![album_or_404(state, id).await?]
    } else if let Some(id) = param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
        let a = artist_or_404(state, id).await?;
        music.albums_for_artist(a.content_id).await?
    } else {
        return Err(ApiError::BadRequest("albumId or artistId is required".into()));
    };
    let wanted: HashSet<i64> = params(&q, "trackIds").iter().filter_map(|v| v.parse().ok()).collect();
    let mut tracks = tracks_for_albums(state, &albums).await?;
    if !wanted.is_empty() {
        tracks.retain(|t| t["id"].as_i64().is_some_and(|i| wanted.contains(&i)));
    }
    Ok(Json(tracks))
}

pub(crate) async fn get_track(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let t = state
        .db
        .music()
        .track(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("track {id} not found")))?;
    let al = state
        .db
        .music()
        .album_by_content(t.album_content_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("album missing".into()))?;
    let found = tracks_for_albums(state, &[al]).await?;
    found
        .into_iter()
        .find(|v| v["id"].as_i64() == Some(id))
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("track {id} not found")))
}

async fn cutoff_ranks(state: &AppState) -> ApiResult<HashMap<ContentId, u32>> {
    // The cutoff of the profile each artist uses, keyed by artist content id.
    let profiles: HashMap<QualityProfileId, u32> = state
        .db
        .profiles()
        .list_profiles()
        .await?
        .into_iter()
        .map(|p| (p.id, p.cutoff_quality))
        .collect();
    Ok(state
        .db
        .music()
        .artists()
        .await?
        .into_iter()
        .filter_map(|a| {
            a.quality_profile_id
                .and_then(|p| profiles.get(&p).copied())
                .map(|c| (a.content_id, c))
        })
        .collect())
}

async fn file_resources(state: &AppState, files: Vec<cellarr_core::TrackFile>) -> ApiResult<Vec<Value>> {
    let music = state.db.music();
    let artists: HashMap<ContentId, i64> = music.artists().await?.into_iter().map(|a| (a.content_id, a.id)).collect();
    let albums: HashMap<ContentId, i64> = music.albums().await?.into_iter().map(|a| (a.content_id, a.id)).collect();
    let cutoffs = cutoff_ranks(state).await?;
    Ok(files
        .iter()
        .map(|f| {
            resources::track_file(
                f,
                artists.get(&f.artist_content_id).copied().unwrap_or(0),
                albums.get(&f.album_content_id).copied().unwrap_or(0),
                cutoffs.get(&f.artist_content_id).copied(),
            )
        })
        .collect())
}

pub(crate) async fn list_track_files(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let music = state.db.music();
    // Files here are always attached to a track; "unmapped" files do not exist.
    if flag(&q, "unmapped") {
        return Ok(Json(Vec::new()));
    }
    let files = if let Some(id) = param(&q, "albumId").and_then(|v| v.parse::<i64>().ok()) {
        music.track_files_for_album(album_or_404(state, id).await?.content_id).await?
    } else if let Some(id) = param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
        music.track_files_for_artist(artist_or_404(state, id).await?.content_id).await?
    } else {
        let ids: Vec<i64> = params(&q, "trackFileIds").iter().filter_map(|v| v.parse().ok()).collect();
        if ids.is_empty() {
            return Err(ApiError::BadRequest("albumId, artistId or trackFileIds is required".into()));
        }
        let mut found = Vec::new();
        for id in ids {
            found.extend(music.track_file(id).await?);
        }
        found
    };
    Ok(Json(file_resources(state, files).await?))
}

pub(crate) async fn get_track_file(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let f = state
        .db
        .music()
        .track_file(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("track file {id} not found")))?;
    Ok(Json(file_resources(state, vec![f]).await?.remove(0)))
}

async fn remove_track_file(state: &AppState, id: i64) -> ApiResult<()> {
    let Some(f) = state.db.music().track_file(id).await? else {
        return Ok(()); // idempotent
    };
    let node = state.db.content().get_node(f.artist_content_id).await?;
    // The bytes go first: a refused out-of-root path leaves the row intact rather
    // than orphaning a file that is still on disk.
    if let Some(node) = node {
        shim::recycle_content_files(state, &node, std::slice::from_ref(&f.file.path)).await?;
    }
    state.db.music().delete_track_file(id).await?;
    Ok(())
}

pub(crate) async fn delete_track_file(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    remove_track_file(app(&fs), id).await?;
    Ok(Json(json!({})))
}

pub(crate) async fn delete_track_files(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    for id in id_list(&body, "trackFileIds") {
        remove_track_file(app(&fs), id).await?;
    }
    Ok(Json(json!({})))
}

// --- quality profiles ----------------------------------------------------------

fn music_items(allowed: &[u32]) -> Vec<Value> {
    QualityRanking::default()
        .for_media(MediaType::Music)
        .into_iter()
        .filter(|d| d.rank != 0)
        .map(|d| {
            json!({
                "quality": { "id": lidarr_quality_id(&d.name), "name": d.name },
                "items": [],
                "allowed": allowed.contains(&d.rank),
            })
        })
        .collect()
}

fn profile_resource(p: &QualityProfile, seq: i64) -> Value {
    json!({
        "id": seq,
        "name": p.name,
        "upgradeAllowed": p.upgrades_allowed,
        "cutoff": id_for_rank(p.cutoff_quality),
        "items": music_items(&p.allowed_qualities),
        "minFormatScore": p.min_custom_format_score,
        "cutoffFormatScore": p.upgrade_until_custom_format_score,
        "formatItems": [],
    })
}

/// Read a quality profile from the resource a client sends. Groups are flattened
/// to their member qualities; a cutoff that names a group means its lowest member.
fn profile_from_body(body: &Value, id: QualityProfileId) -> ApiResult<QualityProfile> {
    fn walk(items: &[Value], allowed: &mut Vec<u32>) {
        for item in items {
            let on = item.get("allowed").and_then(Value::as_bool).unwrap_or(false);
            if let Some(q) = item.get("quality").and_then(|q| q.get("id")).and_then(Value::as_u64) {
                if let Some(rank) = u32::try_from(q).ok().and_then(rank_for_id) {
                    if on && !allowed.contains(&rank) {
                        allowed.push(rank);
                    }
                }
            }
            if let Some(children) = item.get("items").and_then(Value::as_array) {
                // A group's members inherit the group's own `allowed`.
                let mut inner = Vec::new();
                walk(children, &mut inner);
                if on {
                    for child in children {
                        if let Some(rank) = child
                            .get("quality")
                            .and_then(|q| q.get("id"))
                            .and_then(Value::as_u64)
                            .and_then(|q| u32::try_from(q).ok())
                            .and_then(rank_for_id)
                        {
                            if !allowed.contains(&rank) {
                                allowed.push(rank);
                            }
                        }
                    }
                }
                for r in inner {
                    if !allowed.contains(&r) {
                        allowed.push(r);
                    }
                }
            }
        }
    }
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("a quality profile needs a name".into()))?;
    let mut allowed = Vec::new();
    if let Some(items) = body.get("items").and_then(Value::as_array) {
        walk(items, &mut allowed);
    }
    if allowed.is_empty() {
        return Err(ApiError::BadRequest("a quality profile must allow at least one quality".into()));
    }
    let cutoff_id = body.get("cutoff").and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok());
    let cutoff = cutoff_id
        .and_then(rank_for_id)
        .filter(|r| allowed.contains(r))
        // A group id (or an unknown one): the lowest allowed quality is the
        // weakest cutoff that is still satisfiable.
        .or_else(|| allowed.iter().copied().min())
        .unwrap_or(0);
    Ok(QualityProfile {
        id,
        name: name.to_string(),
        allowed_qualities: allowed,
        upgrades_allowed: body.get("upgradeAllowed").and_then(Value::as_bool).unwrap_or(true),
        cutoff_quality: cutoff,
        min_custom_format_score: body.get("minFormatScore").and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok()).unwrap_or(0),
        upgrade_until_custom_format_score: body.get("cutoffFormatScore").and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok()).unwrap_or(0),
        required_languages: Vec::new(),
    })
}

/// The quality profiles that grade music: those allowing at least one music
/// quality. A profile for video is not shown to a music client.
fn is_music_profile(p: &QualityProfile) -> bool {
    p.allowed_qualities
        .iter()
        .any(|r| cellarr_core::quality_rank_is_for(*r, MediaType::Music) && *r != 0)
}

pub(crate) async fn quality_profiles(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let profiles: Vec<QualityProfile> = state
        .db
        .profiles()
        .list_profiles()
        .await?
        .into_iter()
        .filter(is_music_profile)
        .collect();
    let seqs = profile_seqs(state, profiles.iter().map(|p| p.id).collect()).await?;
    Ok(Json(profiles.iter().map(|p| profile_resource(p, seqs[&p.id])).collect()))
}

pub(crate) async fn get_quality_profile(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let pid = state
        .db
        .music()
        .profile_by_seq(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("quality profile {id} not found")))?;
    let p = state
        .db
        .profiles()
        .list_profiles()
        .await?
        .into_iter()
        .find(|p| p.id == pid)
        .ok_or_else(|| ApiError::NotFound(format!("quality profile {id} not found")))?;
    Ok(Json(profile_resource(&p, id)))
}

pub(crate) async fn quality_profile_schema(State(_fs): State<FaceState>) -> Json<Value> {
    let ranking = QualityRanking::default();
    let best = ranking
        .for_media(MediaType::Music)
        .last()
        .map_or(0, |d| d.rank);
    Json(json!({
        "name": "",
        "upgradeAllowed": false,
        "cutoff": id_for_rank(best),
        "items": music_items(&[]),
        "minFormatScore": 0,
        "cutoffFormatScore": 0,
        "formatItems": [],
    }))
}

pub(crate) async fn create_quality_profile(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let profile = profile_from_body(&body, QualityProfileId::new())?;
    state.db.profiles().upsert_profile(&profile).await?;
    let seq = state.db.music().profile_seq(profile.id).await?;
    Ok(Json(profile_resource(&profile, seq)))
}

pub(crate) async fn update_quality_profile(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let pid = state
        .db
        .music()
        .profile_by_seq(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("quality profile {id} not found")))?;
    let profile = profile_from_body(&body, pid)?;
    state.db.profiles().upsert_profile(&profile).await?;
    Ok(Json(profile_resource(&profile, id)))
}

pub(crate) async fn delete_quality_profile(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    if let Some(pid) = state.db.music().profile_by_seq(id).await? {
        let in_use = state
            .db
            .music()
            .artists()
            .await?
            .iter()
            .any(|a| a.quality_profile_id == Some(pid));
        if in_use {
            return Err(ApiError::BadRequest("the profile is in use by an artist".into()));
        }
        state.db.profiles().delete_profile(pid).await?;
    }
    Ok(Json(json!({})))
}

pub(crate) async fn quality_definitions(State(_fs): State<FaceState>) -> Json<Vec<Value>> {
    Json(
        QualityRanking::default()
            .for_media(MediaType::Music)
            .into_iter()
            .filter(|d| d.rank != 0)
            .map(|d| {
                json!({
                    "id": lidarr_quality_id(&d.name),
                    "quality": { "id": lidarr_quality_id(&d.name), "name": d.name },
                    "title": d.display_title(),
                    "weight": d.rank,
                    "minSize": d.min_size_per_min.map(|v| v as f64 / 1_048_576.0),
                    "maxSize": d.max_size_per_min.map(|v| v as f64 / 1_048_576.0),
                    "preferredSize": d.preferred_size_per_min.map(|v| v as f64 / 1_048_576.0),
                })
            })
            .collect(),
    )
}

// --- metadata profiles ---------------------------------------------------------

pub(crate) async fn metadata_profiles(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let profiles = app(&fs).db.music().metadata_profiles().await?;
    Ok(Json(profiles.iter().map(resources::metadata_profile).collect()))
}

pub(crate) async fn get_metadata_profile(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    app(&fs)
        .db
        .music()
        .metadata_profile(id)
        .await?
        .map(|p| Json(resources::metadata_profile(&p)))
        .ok_or_else(|| ApiError::NotFound(format!("metadata profile {id} not found")))
}

pub(crate) async fn metadata_profile_schema(State(_fs): State<FaceState>) -> Json<Value> {
    let mut empty = MetadataProfile::standard();
    empty.id = 0;
    empty.name = String::new();
    empty.primary_types.clear();
    empty.secondary_types.clear();
    empty.release_statuses.clear();
    Json(resources::metadata_profile(&empty))
}

pub(crate) async fn create_metadata_profile(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let stored = app(&fs)
        .db
        .music()
        .upsert_metadata_profile(resources::metadata_profile_from(&body, 0))
        .await?;
    Ok(Json(resources::metadata_profile(&stored)))
}

pub(crate) async fn update_metadata_profile(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let stored = app(&fs)
        .db
        .music()
        .upsert_metadata_profile(resources::metadata_profile_from(&body, id))
        .await?;
    Ok(Json(resources::metadata_profile(&stored)))
}

pub(crate) async fn delete_metadata_profile(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    if id == 1 {
        return Err(ApiError::BadRequest("the default metadata profile cannot be deleted".into()));
    }
    if state.db.music().artists().await?.iter().any(|a| a.metadata_profile_id == id) {
        return Err(ApiError::BadRequest("the profile is in use by an artist".into()));
    }
    state.db.music().delete_metadata_profile(id).await?;
    Ok(Json(json!({})))
}

// --- shared resources with Lidarr-specific extras ------------------------------

pub(crate) async fn root_folders(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let Json(base) = shim::root_folders(State(fs)).await?;
    Ok(Json(
        base.into_iter()
            .map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    let path = o.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
                    o.entry("name").or_insert_with(|| json!(resources::folder_of(&path)));
                    o.entry("defaultMetadataProfileId").or_insert(json!(1));
                    o.entry("defaultQualityProfileId").or_insert(json!(0));
                    o.entry("defaultMonitorOption").or_insert(json!("all"));
                    o.entry("defaultNewItemMonitorOption").or_insert(json!("all"));
                    o.entry("defaultTags").or_insert(json!([]));
                    o.entry("totalSpace").or_insert(json!(0));
                }
                v
            })
            .collect(),
    ))
}

pub(crate) async fn diskspace(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let Json(roots) = shim::root_folders(State(fs)).await?;
    Ok(Json(
        roots
            .into_iter()
            .map(|r| {
                json!({
                    "path": r["path"],
                    "label": "",
                    "freeSpace": r["freeSpace"],
                    "totalSpace": 0,
                })
            })
            .collect(),
    ))
}

pub(crate) async fn config_host(State(fs): State<FaceState>) -> Json<Value> {
    let auth = if app(&fs).auth.accepts(None) { "none" } else { "external" };
    Json(json!({
        "id": 1, "bindAddress": "*", "port": 8686, "sslPort": 6868, "enableSsl": false,
        "launchBrowser": false, "authenticationMethod": auth, "authenticationRequired": "disabledForLocalAddresses",
        "analyticsEnabled": false, "username": "", "password": "", "logLevel": "info",
        "consoleLogLevel": "", "branch": "main", "apiKey": "", "sslCertPath": "", "sslCertPassword": "",
        "urlBase": "", "instanceName": "Lidarr", "applicationUrl": "", "updateAutomatically": false,
        "updateMechanism": "builtIn", "updateScriptPath": "", "proxyEnabled": false, "proxyType": "http",
        "proxyHostname": "", "proxyPort": 8080, "proxyUsername": "", "proxyPassword": "",
        "proxyBypassFilter": "", "proxyBypassLocalAddresses": true, "certificateValidation": "enabled",
        "backupFolder": "Backups", "backupInterval": 7, "backupRetention": 28,
    }))
}

pub(crate) async fn config_ui(State(_fs): State<FaceState>) -> Json<Value> {
    Json(json!({
        "id": 1, "firstDayOfWeek": 0, "calendarWeekColumnHeader": "ddd M/D", "shortDateFormat": "MMM D YYYY",
        "longDateFormat": "dddd, MMMM D YYYY", "timeFormat": "h(:mm)a", "showRelativeDates": true,
        "enableColorImpairedMode": false, "uiLanguage": 1, "theme": "auto", "expandAlbumByDefault": true,
        "expandSingleByDefault": false, "expandEPByDefault": false, "expandBroadcastByDefault": false,
        "expandOtherByDefault": false,
    }))
}

pub(crate) async fn config_indexer(State(_fs): State<FaceState>) -> Json<Value> {
    Json(json!({ "id": 1, "minimumAge": 0, "maximumSize": 0, "retention": 0, "rssSyncInterval": 15 }))
}

pub(crate) async fn config_downloadclient(State(_fs): State<FaceState>) -> Json<Value> {
    Json(json!({
        "id": 1, "downloadClientWorkingFolders": "_UNPACK_|_FAILED_", "enableCompletedDownloadHandling": true,
        "autoRedownloadFailed": true, "autoRedownloadFailedFromInteractiveSearch": true,
    }))
}

pub(crate) async fn config_metadataprovider(State(_fs): State<FaceState>) -> Json<Value> {
    Json(json!({ "id": 1, "metadataSource": "", "writeAudioTags": "no", "scrubAudioTags": false }))
}

pub(crate) async fn config_naming(State(_fs): State<FaceState>) -> Json<Value> {
    Json(json!({
        "id": 1, "renameTracks": true, "replaceIllegalCharacters": true, "colonReplacementFormat": "delete",
        "standardTrackFormat": "{Album Title} ({Release Year})/{Artist Name} - {Album Title} - {track:00} - {Track Title}",
        "multiDiscTrackFormat": "{Album Title} ({Release Year})/{Medium Format} {medium:00}/{Artist Name} - {Album Title} - {track:00} - {Track Title}",
        "artistFolderFormat": "{Artist Name}",
        "includeArtistName": false, "includeAlbumTitle": false, "includeQuality": false,
        "replaceSpaces": false, "separator": " - ", "numberStyle": "{track:00}",
    }))
}

pub(crate) async fn languages(State(_fs): State<FaceState>) -> Json<Vec<Value>> {
    Json(vec![json!({ "id": 1, "name": "English", "nameLower": "english" })])
}

pub(crate) async fn indexer_flags(State(_fs): State<FaceState>) -> Json<Vec<Value>> {
    Json(vec![
        json!({ "id": 1, "name": "G_Freeleech", "nameLower": "g_freeleech" }),
        json!({ "id": 2, "name": "G_Halfleech", "nameLower": "g_halfleech" }),
        json!({ "id": 4, "name": "G_DoubleUpload", "nameLower": "g_doubleupload" }),
    ])
}

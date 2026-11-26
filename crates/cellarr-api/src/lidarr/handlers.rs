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

use super::quality::{
    allowed_ranks, cutoff_id_for_rank, lidarr_quality_id, profile_items, rank_for_cutoff_id,
};
use super::validation::Failures;
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
        if let std::collections::hash_map::Entry::Vacant(e) = known.entry(id) {
            let seq = state.db.music().profile_seq(id).await?;
            e.insert(seq);
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
    let stats = state.db.music().artist_stats_for(&[a.content_id]).await?;
    let seqs = profile_seqs(state, a.quality_profile_id.into_iter().collect()).await?;
    let mut v = resources::artist(
        a,
        Some(stats.get(&a.content_id).copied().unwrap_or_default()),
        a.quality_profile_id.and_then(|p| seqs.get(&p).copied()),
    );
    attach_neighbours(state, std::slice::from_mut(&mut v), &[a]).await?;
    Ok(v)
}

/// Fill each artist resource's `lastAlbum` (the newest released) and `nextAlbum`
/// (the soonest to come), which dashboards and the artist list show.
async fn attach_neighbours(
    state: &AppState,
    resources: &mut [Value],
    artists: &[&Artist],
) -> ApiResult<()> {
    let today = time::OffsetDateTime::now_utc().date().to_string();
    let bounds = state.db.music().artist_album_bounds(&today).await?;
    let wanted: Vec<ContentId> = artists
        .iter()
        .filter_map(|a| bounds.get(&a.content_id))
        .flat_map(|(last, next)| last.iter().chain(next.iter()).copied())
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let albums: HashMap<ContentId, Album> = state
        .db
        .music()
        .albums_by_content(&wanted)
        .await?
        .into_iter()
        .map(|a| (a.content_id, a))
        .collect();
    for (resource, artist) in resources.iter_mut().zip(artists) {
        let Some((last, next)) = bounds.get(&artist.content_id) else {
            continue;
        };
        // Lidarr embeds the album's artist (without its own neighbours) here.
        let seq = resource["qualityProfileId"].as_i64();
        let embedded = resources::artist_ref(artist, seq);
        let render = |c: &Option<ContentId>| {
            c.and_then(|c| albums.get(&c))
                .map(|al| resources::album(al, artist.id, Some(embedded.clone()), None, None, &[]))
        };
        resource["lastAlbum"] = render(last).unwrap_or(Value::Null);
        resource["nextAlbum"] = render(next).unwrap_or(Value::Null);
    }
    Ok(())
}

/// What rendering albums needs beyond the albums themselves: every artist (there
/// are hundreds) and their profile ids. Loaded once per request.
struct AlbumRenderer {
    artists: HashMap<ContentId, Artist>,
    seqs: HashMap<QualityProfileId, i64>,
}

impl AlbumRenderer {
    async fn load(state: &AppState) -> ApiResult<Self> {
        let artists: HashMap<ContentId, Artist> = state
            .db
            .music()
            .artists()
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
        Ok(Self { artists, seqs })
    }

    /// Render one slice of albums. Statistics are fetched for exactly the slice.
    async fn render(
        &self,
        state: &AppState,
        albums: &[Album],
        with_tracks: bool,
    ) -> ApiResult<Vec<Value>> {
        let music = state.db.music();
        let seq_of = |a: &Artist| {
            a.quality_profile_id
                .and_then(|p| self.seqs.get(&p).copied())
        };
        let wanted: Vec<ContentId> = albums.iter().map(|a| a.content_id).collect();
        let stats: HashMap<ContentId, MusicStats> = music.album_stats_for(&wanted).await?;
        let pressings = music.releases_for_albums(&wanted).await?;
        let mut out = Vec::with_capacity(albums.len());
        for al in albums {
            let artist = self.artists.get(&al.artist_content_id);
            let tracks = if with_tracks {
                music.tracks_for_album(al.content_id).await?
            } else {
                Vec::new()
            };
            let mut v = resources::album(
                al,
                artist.map_or(0, |a| a.id),
                artist.map(|a| resources::artist_ref(a, seq_of(a))),
                Some(stats.get(&al.content_id).copied().unwrap_or_default()),
                artist.and_then(seq_of),
                &tracks,
            );
            if let Some(list) = pressings.get(&al.content_id) {
                resources::set_releases(&mut v, al, list);
            }
            out.push(v);
        }
        Ok(out)
    }
}

/// Render albums (with their artists and statistics) for other modules.
pub(crate) async fn render_albums(state: &AppState, albums: &[Album]) -> ApiResult<Vec<Value>> {
    album_resources(state, albums, false).await
}

/// Render albums with their artists and statistics.
async fn album_resources(
    state: &AppState,
    albums: &[Album],
    with_tracks: bool,
) -> ApiResult<Vec<Value>> {
    AlbumRenderer::load(state)
        .await?
        .render(state, albums, with_tracks)
        .await
}

/// Render albums straight to a JSON array body, a slice at a time.
///
/// The whole album list of a large library is tens of megabytes of JSON; building it
/// as one tree of values first would hold several times that in memory at once. Here
/// each slice is rendered, written to the body, and dropped, so peak memory is the
/// body itself plus one slice.
async fn album_array(
    state: &AppState,
    albums: &[Album],
    with_tracks: bool,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    const SLICE: usize = 250;
    let renderer = AlbumRenderer::load(state).await?;
    let mut body: Vec<u8> = Vec::with_capacity(albums.len().min(50_000) * 1800 + 2);
    body.push(b'[');
    let mut first = true;
    for chunk in albums.chunks(SLICE) {
        for value in renderer.render(state, chunk, with_tracks).await? {
            if !first {
                body.push(b',');
            }
            first = false;
            serde_json::to_writer(&mut body, &value)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }
    }
    body.push(b']');
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response())
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
    let seqs = profile_seqs(
        state,
        artists
            .iter()
            .filter_map(|a| a.quality_profile_id)
            .collect(),
    )
    .await?;
    let mut out: Vec<Value> = artists
        .iter()
        .map(|a| {
            resources::artist(
                a,
                Some(stats.get(&a.content_id).copied().unwrap_or_default()),
                a.quality_profile_id.and_then(|p| seqs.get(&p).copied()),
            )
        })
        .collect();
    let refs: Vec<&Artist> = artists.iter().collect();
    attach_neighbours(state, &mut out, &refs).await?;
    Ok(Json(out))
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
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches('.').trim().to_string();
    if trimmed.is_empty() {
        "Unknown Artist".to_string()
    } else {
        trimmed
    }
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
    let name = body
        .get("artistName")
        .and_then(Value::as_str)
        .unwrap_or(mbid);
    let root = body
        .get("rootFolderPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
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
        .is_none_or(|m| !m.eq_ignore_ascii_case("none"));
    let artist = catalogue
        .add_artist(AddArtist {
            library: library.id,
            mbid: mbid.to_string(),
            path,
            quality_profile_id: resolve_profile(state, body)
                .await?
                .or(Some(library.default_quality_profile)),
            metadata_profile_id: body
                .get("metadataProfileId")
                .and_then(Value::as_i64)
                .unwrap_or(1),
            // Lidarr adds an artist asked to monitor no albums as unmonitored,
            // whatever `monitored` said; every other mode keeps the flag.
            monitored: monitor != MonitorMode::None
                && body
                    .get("monitored")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            monitor,
            monitor_new_albums: monitor_new,
        })
        .await
        .map_err(catalogue_error)?;
    if options
        .get("searchForMissingAlbums")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        super::command::search_artist(state, &artist).await?;
    }
    Ok(artist)
}

pub(crate) async fn add_artist(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, axum::response::Response> {
    use axum::response::IntoResponse;
    let state = app(&fs);
    validate_new_artist(state, &body)
        .await
        .map_err(IntoResponse::into_response)?;
    let artist = add_artist_from(state, &body)
        .await
        .map_err(IntoResponse::into_response)?;
    artist_resource(state, &artist)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

/// The checks Lidarr runs before it adds an artist, reported the way it reports
/// them: every finding at once, as an array.
async fn validate_new_artist(state: &AppState, body: &Value) -> Result<(), Failures> {
    let int = |k: &str| body.get(k).and_then(Value::as_i64).unwrap_or(0);
    let mut f = Failures::new();
    f.positive("QualityProfileId", int("qualityProfileId"));
    f.positive("MetadataProfileId", int("metadataProfileId"));
    if int("qualityProfileId") > 0 {
        let known = state
            .db
            .music()
            .profile_by_seq(int("qualityProfileId"))
            .await
            .ok()
            .flatten()
            .is_some();
        if !known {
            f.add(
                "QualityProfileId",
                "Quality Profile does not exist",
                "QualityProfileExistsValidator",
                Some(json!(int("qualityProfileId"))),
            );
        }
    }
    if int("metadataProfileId") > 0
        && state
            .db
            .music()
            .metadata_profile(int("metadataProfileId"))
            .await
            .ok()
            .flatten()
            .is_none()
    {
        f.add(
            "MetadataProfileId",
            "Metadata Profile does not exist",
            "MetadataProfileExistsValidator",
            Some(json!(int("metadataProfileId"))),
        );
    }
    let mbid = body
        .get("foreignArtistId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    f.not_empty("ForeignArtistId", mbid);
    if !mbid.is_empty()
        && state
            .db
            .music()
            .artist_by_mbid(mbid)
            .await
            .ok()
            .flatten()
            .is_some()
    {
        f.add(
            "ForeignArtistId",
            "This artist has already been added.",
            "ArtistExistsValidator",
            Some(json!(mbid)),
        );
    }
    f.check()
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
    if let Some(p) = body
        .get("metadataProfileId")
        .and_then(Value::as_i64)
        .filter(|p| *p > 0)
    {
        a.metadata_profile_id = p;
    }
    if body
        .get("qualityProfileId")
        .and_then(Value::as_i64)
        .is_some_and(|p| p > 0)
    {
        a.quality_profile_id = resolve_profile(state, body).await?;
    }
    if let Some(path) = body
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        a.path = path.to_string();
    } else if let Some(root) = body
        .get("rootFolderPath")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
    {
        a.path = format!(
            "{}/{}",
            root.trim_end_matches('/'),
            resources::folder_of(&a.path)
        );
    }
    Ok(())
}

async fn remove_artist(state: &AppState, artist: &Artist, delete_files: bool) -> ApiResult<()> {
    let paths = state.db.music().delete_artist(artist.content_id).await?;
    if delete_files {
        if let Some(node) = state
            .db
            .content()
            .get_node(artist.content_id)
            .await
            .ok()
            .flatten()
        {
            shim::recycle_content_files(state, &node, &paths).await?;
        } else {
            tracing::warn!(
                artist = artist.id,
                "artist already gone; its files were not removed"
            );
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
    let delete_files = body
        .get("deleteFiles")
        .and_then(Value::as_bool)
        .unwrap_or(false);
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
            let profile = state
                .db
                .music()
                .metadata_profile(1)
                .await?
                .unwrap_or_else(MetadataProfile::standard);
            vec![
                catalogue
                    .lookup_artist(mbid, &profile)
                    .await
                    .map_err(catalogue_error)?
                    .0,
            ]
        }
        None => catalogue
            .search_artists(term)
            .await
            .map_err(catalogue_error)?,
    };
    let known: HashMap<String, Artist> = state
        .db
        .music()
        .artists()
        .await?
        .into_iter()
        .map(|a| (a.mbid.clone(), a))
        .collect();
    Ok(Json(
        hits.iter()
            .map(|h| lookup_artist_resource(h, known.get(&h.mbid)))
            .collect(),
    ))
}

fn lookup_artist_resource(h: &cellarr_meta::MbArtist, known: Option<&Artist>) -> Value {
    let images: Vec<Value> = h
        .images
        .iter()
        .map(|i| json!({ "coverType": i.cover_type, "url": i.url }))
        .collect();
    let mut v = json!({
        "status": if h.ended { "ended" } else { "continuing" },
        "ended": h.ended,
        "artistName": h.name,
        "foreignArtistId": h.mbid,
        "tadbId": 0,
        "discogsId": 0,
        "overview": h.overview.clone().unwrap_or_default(),
        "artistType": h.artist_type.clone().unwrap_or_default(),
        "disambiguation": h.disambiguation.clone().unwrap_or_default(),
        "links": h.links.iter().map(|l| json!({ "url": l.url, "name": l.name })).collect::<Vec<_>>(),
        "nextAlbum": null,
        "lastAlbum": null,
        "images": resources::images_of(Some(&Value::Array(images))),
        "qualityProfileId": 0,
        "metadataProfileId": 0,
        "monitored": known.is_some_and(|a| a.monitored),
        "monitorNewItems": "all",
        "folder": resources::folder_of(&h.name),
        "genres": h.genres,
        "tags": [],
        "added": "0001-01-01T00:00:00Z",
        "ratings": h.rating.map_or(json!({ "votes": 0, "value": 0.0 }), |r| json!({ "votes": r.votes, "value": r.value })),
    });
    // A lookup of an artist already in the library carries its identity, like
    // Lidarr's: the id and the library path.
    if let (Some(a), Some(o)) = (known, v.as_object_mut()) {
        o.insert("id".into(), json!(a.id));
        o.insert("path".into(), json!(a.path));
        o.insert("cleanName".into(), json!(resources::clean_name(&a.name)));
        o.insert(
            "sortName".into(),
            json!(a.sort_name.clone().unwrap_or_else(|| a.name.to_lowercase())),
        );
    }
    v
}

// --- albums --------------------------------------------------------------------

pub(crate) async fn list_albums(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<axum::response::Response> {
    let state = app(&fs);
    let music = state.db.music();
    let albums: Vec<Album> = if let Some(mbid) = param(&q, "foreignAlbumId") {
        music.album_by_mbid(mbid).await?.into_iter().collect()
    } else if let Some(artist) = param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
        // Lidarr answers an artist it does not know with an empty list, not a 404.
        match music.artist(artist).await? {
            Some(a) => music.albums_for_artist(a.content_id).await?,
            None => Vec::new(),
        }
    } else {
        let ids: HashSet<i64> = params(&q, "albumIds")
            .iter()
            .filter_map(|v| v.parse().ok())
            .collect();
        let all = music.albums().await?;
        if ids.is_empty() {
            all
        } else {
            all.into_iter().filter(|a| ids.contains(&a.id)).collect()
        }
    };
    album_array(state, &albums, flag(&q, "includeTracks")).await
}

pub(crate) async fn get_album(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let al = album_or_404(state, id).await?;
    let al = with_releases(state, al).await?;
    Ok(Json(album_resources(state, &[al], true).await?.remove(0)))
}

/// The longest an add or a read waits for the metadata source to supply an album's
/// pressings; past it the album is answered with whatever it has.
const RELEASES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Make sure `al` has its pressings (and track list) stored, as real Lidarr's albums
/// always do, so a client that adds an album and reads it straight back sees
/// `releases` with a monitored one. A slow or failing metadata source leaves the
/// album as it was: no pressings yet, retried on the next read.
async fn with_releases(state: &AppState, al: Album) -> ApiResult<Album> {
    let Some(catalogue) = &state.music else {
        return Ok(al);
    };
    match tokio::time::timeout(RELEASES_TIMEOUT, catalogue.ensure_releases(al.content_id)).await {
        Ok(Ok(())) => album_or_404(state, al.id).await,
        Ok(Err(e)) => {
            tracing::warn!(album = al.id, error = %e, "could not fetch the album's releases");
            Ok(al)
        }
        Err(_) => {
            tracing::warn!(album = al.id, "timed out fetching the album's releases");
            Ok(al)
        }
    }
}

pub(crate) async fn update_album(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let al = album_or_404(state, id).await?;
    if let Some(m) = body.get("monitored").and_then(Value::as_bool) {
        state
            .db
            .music()
            .set_monitored(vec![al.content_id], m)
            .await?;
    }
    apply_release_choice(state, &al, &body).await?;
    let fresh = album_or_404(state, id).await?;
    Ok(Json(
        album_resources(state, &[fresh], true).await?.remove(0),
    ))
}

/// Pin the pressing a PUT marks monitored (and `anyReleaseOk`), switching the
/// album's track list to it, as Lidarr does when a release is chosen.
async fn apply_release_choice(state: &AppState, al: &Album, body: &Value) -> ApiResult<()> {
    let any_ok = body
        .get("anyReleaseOk")
        .and_then(Value::as_bool)
        .unwrap_or(al.any_release_ok);
    let pressings = state
        .db
        .music()
        .releases_for_albums(&[al.content_id])
        .await?
        .remove(&al.content_id)
        .unwrap_or_default();
    let wanted = body
        .get("releases")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .filter(|r| r["monitored"] == json!(true))
                .find_map(|r| {
                    let id = r["id"].as_i64();
                    let mbid = r["foreignReleaseId"].as_str();
                    pressings.iter().find(|p| {
                        Some(p.id) == id || mbid.is_some_and(|m| m.eq_ignore_ascii_case(&p.mbid))
                    })
                })
        })
        .map(|p| p.mbid.clone());
    let current = al.release_mbid.clone();
    match wanted.clone() {
        Some(mbid) if Some(&mbid) != current.as_ref() => {
            let catalogue = state
                .music
                .as_ref()
                .ok_or_else(|| ApiError::Domain("no music metadata source is configured".into()))?;
            catalogue
                .select_release(al.content_id, &mbid, any_ok)
                .await
                .map_err(catalogue_error)?;
        }
        _ if any_ok != al.any_release_ok => {
            state
                .db
                .music()
                .pin_release(al.content_id, current.clone(), any_ok)
                .await?;
        }
        _ => {}
    }
    // Lidarr saves each release's monitored flag as sent: a PUT that lists the
    // pressings with none monitored leaves the album with no tracks to want, and
    // one that monitors the pinned pressing again restores them.
    if let Some(list) = body.get("releases").and_then(Value::as_array) {
        let none_monitored = !list.is_empty() && list.iter().all(|r| r["monitored"] != json!(true));
        if none_monitored {
            state
                .db
                .music()
                .set_release_monitored(al.content_id, false)
                .await?;
        } else if !al.release_monitored && wanted.is_some() && wanted == current {
            state
                .db
                .music()
                .set_release_monitored(al.content_id, true)
                .await?;
        }
    }
    Ok(())
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
    let monitored = body
        .get("monitored")
        .and_then(Value::as_bool)
        .unwrap_or(true);
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
        let existing = with_releases(state, existing).await?;
        return Ok(Json(
            album_resources(state, &[existing], true).await?.remove(0),
        ));
    }
    // The album's artist: by id when the client names one we hold, else added.
    let artist = match body
        .get("artistId")
        .and_then(Value::as_i64)
        .filter(|i| *i > 0)
    {
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
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
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
                    title: body
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    disambiguation: body
                        .get("disambiguation")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    album_type: body
                        .get("albumType")
                        .and_then(Value::as_str)
                        .unwrap_or("Album")
                        .to_string(),
                    secondary_types: secondary,
                    release_date: body
                        .get("releaseDate")
                        .and_then(Value::as_str)
                        .map(|d| d.chars().take(10).collect()),
                    release_mbid: None,
                    monitored: body
                        .get("monitored")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
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
    if body
        .get("monitored")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && !album.monitored
    {
        state
            .db
            .music()
            .set_monitored(vec![album.content_id], true)
            .await?;
    }
    let fresh = album_or_404(state, album.id).await?;
    let fresh = with_releases(state, fresh).await?;
    Ok(Json(
        album_resources(state, &[fresh], true).await?.remove(0),
    ))
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
    let hits = match mbid_term(term) {
        // Lidarr answers `lidarr:<release-group id>` with exactly that album.
        Some(mbid) => catalogue
            .lookup_album(mbid)
            .await
            .map_err(catalogue_error)?
            .into_iter()
            .collect(),
        None => catalogue
            .search_albums(term)
            .await
            .map_err(catalogue_error)?,
    };
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
        let releases: Vec<Value> = h
            .releases
            .iter()
            .enumerate()
            .map(|(n, r)| lookup_release(r, n == 0))
            .collect();
        let duration = h.releases.first().map_or(0, release_duration);
        let media = h.releases.first().map_or_else(Vec::new, release_media);
        let images: Vec<Value> = if h.images.is_empty() {
            vec![json!({
                "coverType": "cover",
                "url": format!("{}/{}/front-500", resources::COVER_ART_BASE, h.mbid),
            })]
        } else {
            h.images
                .iter()
                .map(|i| json!({ "coverType": i.cover_type, "url": i.url }))
                .collect()
        };
        let mut album = json!({
            "title": h.title,
            "disambiguation": h.disambiguation.clone().unwrap_or_default(),
            "overview": h.overview.clone().unwrap_or_default(),
            "artistId": artist.map_or(0, |a| a.id),
            "foreignAlbumId": h.mbid,
            "monitored": existing.is_some_and(|a| a.monitored),
            "anyReleaseOk": true,
            "profileId": 0,
            "duration": duration,
            "albumType": h.primary_type,
            "secondaryTypes": h.secondary_types,
            "mediumCount": media.len(),
            "ratings": h.rating.map_or(json!({ "votes": 0, "value": 0 }), |r| json!({ "votes": r.votes, "value": r.value })),
            "releaseDate": resources::release_date_iso(h.first_release_date.as_deref()),
            "releases": releases,
            "genres": h.genres,
            "media": media,
            "artist": artist_json,
            "images": resources::images_of(Some(&Value::Array(images))),
            "links": h.links.iter().map(|l| json!({ "url": l.url, "name": l.name })).collect::<Vec<_>>(),
        });
        if let (Some(al), Some(o)) = (existing, album.as_object_mut()) {
            o.insert("id".into(), json!(al.id));
        }
        out.push(album);
    }
    Ok(Json(out))
}

fn release_duration(r: &cellarr_meta::MbRelease) -> u64 {
    r.media
        .iter()
        .flat_map(|m| &m.tracks)
        .filter_map(|t| t.length_ms)
        .map(u64::from)
        .sum()
}

fn release_media(r: &cellarr_meta::MbRelease) -> Vec<Value> {
    r.media
        .iter()
        .map(|m| {
            json!({
                "mediumNumber": m.position,
                "mediumName": "",
                "mediumFormat": m.format.clone().unwrap_or_default(),
            })
        })
        .collect()
}

fn lookup_release(r: &cellarr_meta::MbRelease, first: bool) -> Value {
    json!({
        "id": 0,
        "albumId": 0,
        "foreignReleaseId": r.mbid,
        "title": r.title,
        "status": r.status.clone().unwrap_or_default(),
        "duration": release_duration(r),
        "trackCount": r.track_count(),
        "media": release_media(r),
        "mediaCount": r.media.len(),
        "disambiguation": "",
        "country": r.country.iter().collect::<Vec<_>>(),
        "label": [],
        "format": r.media.first().and_then(|m| m.format.clone()).unwrap_or_default(),
        "monitored": first,
    })
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
    let albums: Vec<Album> =
        if let Some(id) = param(&q, "albumId").and_then(|v| v.parse::<i64>().ok()) {
            // An id that names nothing is an empty list, not an error, as in Lidarr.
            music.album(id).await?.into_iter().collect()
        } else if let Some(id) = param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
            match music.artist(id).await? {
                Some(a) => music.albums_for_artist(a.content_id).await?,
                None => Vec::new(),
            }
        } else {
            return Err(ApiError::BadRequest(
                "albumId or artistId is required".into(),
            ));
        };
    let wanted: HashSet<i64> = params(&q, "trackIds")
        .iter()
        .filter_map(|v| v.parse().ok())
        .collect();
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

/// The cutoff of the profile each of `artists` uses, keyed by artist content id.
async fn cutoff_ranks(state: &AppState, artists: &[Artist]) -> ApiResult<HashMap<ContentId, u32>> {
    let profiles: HashMap<QualityProfileId, u32> = state
        .db
        .profiles()
        .list_profiles()
        .await?
        .into_iter()
        .map(|p| (p.id, p.cutoff_quality))
        .collect();
    Ok(artists
        .iter()
        .filter_map(|a| {
            a.quality_profile_id
                .and_then(|p| profiles.get(&p).copied())
                .map(|c| (a.content_id, c))
        })
        .collect())
}

async fn file_resources(
    state: &AppState,
    files: Vec<cellarr_core::TrackFile>,
) -> ApiResult<Vec<Value>> {
    let music = state.db.music();
    // Only the artists and albums the files belong to: a request for one album's
    // files must not cost a read of every album in the library.
    let artist_ids: Vec<ContentId> = files
        .iter()
        .map(|f| f.artist_content_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let album_ids: Vec<ContentId> = files
        .iter()
        .map(|f| f.album_content_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let artist_rows = music.artists_by_content(&artist_ids).await?;
    let artists: HashMap<ContentId, i64> =
        artist_rows.iter().map(|a| (a.content_id, a.id)).collect();
    let albums: HashMap<ContentId, i64> = music
        .albums_by_content(&album_ids)
        .await?
        .into_iter()
        .map(|a| (a.content_id, a.id))
        .collect();
    let cutoffs = cutoff_ranks(state, &artist_rows).await?;
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
    // Every stored track file is attached to a track, and nothing records the
    // audio files a scan could not link to one (the rescan skips music roots),
    // so there is nothing honest to list. docs/19-lidarr-compat.md has the gap.
    if flag(&q, "unmapped") {
        return Ok(Json(Vec::new()));
    }
    let files = if let Some(id) = param(&q, "albumId").and_then(|v| v.parse::<i64>().ok()) {
        match music.album(id).await? {
            Some(a) => music.track_files_for_album(a.content_id).await?,
            None => Vec::new(),
        }
    } else if let Some(id) = param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
        match music.artist(id).await? {
            Some(a) => music.track_files_for_artist(a.content_id).await?,
            None => Vec::new(),
        }
    } else {
        let ids: Vec<i64> = params(&q, "trackFileIds")
            .iter()
            .filter_map(|v| v.parse().ok())
            .collect();
        if ids.is_empty() {
            return Err(ApiError::BadRequest(
                "albumId, artistId or trackFileIds is required".into(),
            ));
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

fn profile_resource(p: &QualityProfile, seq: i64) -> Value {
    json!({
        "id": seq,
        "name": p.name,
        "upgradeAllowed": p.upgrades_allowed,
        "cutoff": cutoff_id_for_rank(p.cutoff_quality),
        "items": profile_items(&p.allowed_qualities),
        "minFormatScore": p.min_custom_format_score,
        "cutoffFormatScore": p.upgrade_until_custom_format_score,
        "formatItems": [],
    })
}

/// Read a quality profile from the resource a client sends. Tiers are flattened to
/// their member qualities; a cutoff naming a tier means its lowest member.
fn profile_from_body(body: &Value, id: QualityProfileId) -> ApiResult<QualityProfile> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("a quality profile needs a name".into()))?;
    let allowed = body
        .get("items")
        .and_then(Value::as_array)
        .map(|items| allowed_ranks(items))
        .unwrap_or_default();
    if allowed.is_empty() {
        return Err(ApiError::BadRequest(
            "a quality profile must allow at least one quality".into(),
        ));
    }
    let cutoff = body
        .get("cutoff")
        .and_then(Value::as_u64)
        .and_then(|c| u32::try_from(c).ok())
        .and_then(rank_for_cutoff_id)
        .filter(|r| allowed.contains(r))
        .or_else(|| allowed.iter().copied().min())
        .unwrap_or(0);
    Ok(QualityProfile {
        id,
        name: name.to_string(),
        allowed_qualities: allowed,
        upgrades_allowed: body
            .get("upgradeAllowed")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        cutoff_quality: cutoff,
        min_custom_format_score: body
            .get("minFormatScore")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0),
        upgrade_until_custom_format_score: body
            .get("cutoffFormatScore")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0),
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
    Ok(Json(
        profiles
            .iter()
            .map(|p| profile_resource(p, seqs[&p.id]))
            .collect(),
    ))
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
    Json(json!({
        "name": "",
        "upgradeAllowed": false,
        "cutoff": 0,
        "items": profile_items(&[]),
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
            return Err(ApiError::BadRequest(
                "the profile is in use by an artist".into(),
            ));
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
    Ok(Json(
        profiles.iter().map(resources::metadata_profile).collect(),
    ))
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
    let mut v = resources::metadata_profile(&empty);
    if let Some(o) = v.as_object_mut() {
        o.remove("id");
        o.remove("name");
    }
    Json(v)
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
        return Err(ApiError::BadRequest(
            "the default metadata profile cannot be deleted".into(),
        ));
    }
    if state
        .db
        .music()
        .artists()
        .await?
        .iter()
        .any(|a| a.metadata_profile_id == id)
    {
        return Err(ApiError::BadRequest(
            "the profile is in use by an artist".into(),
        ));
    }
    state.db.music().delete_metadata_profile(id).await?;
    Ok(Json(json!({})))
}

// --- shared resources with Lidarr-specific extras ------------------------------

fn decorate_root(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        let path = o
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        o.entry("name")
            .or_insert_with(|| json!(resources::folder_of(&path)));
        o.entry("defaultMetadataProfileId").or_insert(json!(1));
        o.entry("defaultQualityProfileId").or_insert(json!(0));
        o.entry("defaultMonitorOption").or_insert(json!("all"));
        o.entry("defaultNewItemMonitorOption")
            .or_insert(json!("all"));
        o.entry("defaultTags").or_insert(json!([]));
        o.entry("totalSpace").or_insert(json!(0));
    }
    v
}

pub(crate) async fn create_root_folder(
    State(fs): State<FaceState>,
    Json(raw): Json<Value>,
) -> Result<Json<Value>, axum::response::Response> {
    use axum::response::IntoResponse;
    let mut f = Failures::new();
    let path = raw.get("path").and_then(Value::as_str).unwrap_or_default();
    if path.trim().is_empty() {
        f.add("Path", "Invalid Path: '{path}'", "PathValidator", None);
    } else if !std::path::Path::new(path).is_dir() {
        f.add(
            "Path",
            &format!("Path '{path}' does not exist"),
            "PathExistsValidator",
            Some(json!(path)),
        );
    }
    f.positive(
        "DefaultMetadataProfileId",
        raw.get("defaultMetadataProfileId")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    f.positive(
        "DefaultQualityProfileId",
        raw.get("defaultQualityProfileId")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    f.check().map_err(IntoResponse::into_response)?;
    let body: shim::RootFolderBody = serde_json::from_value(raw)
        .map_err(|e| ApiError::BadRequest(e.to_string()).into_response())?;
    let Json(v) = shim::create_root_folder(State(fs), Json(body))
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(Json(decorate_root(v)))
}

pub(crate) async fn create_tag(
    State(fs): State<FaceState>,
    Json(raw): Json<Value>,
) -> Result<Json<Value>, axum::response::Response> {
    use axum::response::IntoResponse;
    let label = raw.get("label").and_then(Value::as_str).unwrap_or_default();
    let mut f = Failures::new();
    f.not_empty("Label", label);
    f.check().map_err(IntoResponse::into_response)?;
    let tag = fs
        .state
        .db
        .tags()
        .create(label.trim())
        .await
        .map_err(|e| ApiError::from(e).into_response())?;
    Ok(Json(shim::v3_tag(&tag)))
}

pub(crate) async fn root_folders(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let Json(base) = shim::root_folders(State(fs)).await?;
    Ok(Json(base.into_iter().map(decorate_root).collect()))
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

pub(crate) async fn languages(State(_fs): State<FaceState>) -> Json<Vec<Value>> {
    Json(vec![
        json!({ "id": 1, "name": "English", "nameLower": "english" }),
    ])
}

pub(crate) async fn indexer_flags(State(_fs): State<FaceState>) -> Json<Vec<Value>> {
    Json(vec![
        json!({ "id": 1, "name": "G_Freeleech", "nameLower": "g_freeleech" }),
        json!({ "id": 2, "name": "G_Halfleech", "nameLower": "g_halfleech" }),
        json!({ "id": 4, "name": "G_DoubleUpload", "nameLower": "g_doubleupload" }),
    ])
}

pub(crate) async fn system_status(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Value>> {
    let _ = &q;
    let Json(mut v) = shim::system_status(State(fs), Query(shim::StatusQuery::default())).await?;
    if let Some(o) = v.as_object_mut() {
        // Lidarr's status carries none of cellarr's packaging fields.
        for k in [
            "packageVersion",
            "packageAuthor",
            "packageUpdateMechanismMessage",
        ] {
            o.remove(k);
        }
    }
    Ok(Json(v))
}

/// A configuration singleton's PUT: accepted and echoed. cellarr keeps these
/// settings elsewhere (its own configuration), so a client's write to Lidarr's copy
/// has nothing to change; echoing keeps clients that save the page happy.
pub(crate) async fn echo_put(Json(body): Json<Value>) -> Json<Value> {
    Json(body)
}

pub(crate) async fn echo_list(Json(body): Json<Value>) -> Json<Value> {
    Json(body)
}

pub(crate) async fn root_folder(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let Json(all) = root_folders(State(fs)).await?;
    all.into_iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("root folder {id} not found")))
}

pub(crate) async fn root_folder_update(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let Json(current) = root_folder(State(fs), Path(id)).await?;
    Ok(Json(shim::merge(current, body)))
}

pub(crate) async fn remote_path_mapping(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let Json(all) = shim::list_remote_path_mappings(State(fs)).await?;
    all.into_iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("remote path mapping {id} not found")))
}

pub(crate) async fn download_client(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let Json(all) = list_download_clients(State(fs)).await?;
    all.into_iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("download client {id} not found")))
}

pub(crate) async fn quality_definition(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let Json(all) = quality_definitions(State(fs)).await;
    all.into_iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("quality definition {id} not found")))
}

pub(crate) async fn language(
    State(_fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    if id == 1 {
        Ok(Json(
            json!({ "id": 1, "name": "English", "nameLower": "english" }),
        ))
    } else {
        Err(ApiError::NotFound(format!("language {id} not found")))
    }
}

/// `GET /config/naming/examples`: the formats' output for a sample track, so a
/// client previews what an import would name a file.
pub(crate) async fn naming_examples(State(_fs): State<FaceState>) -> Json<Value> {
    let render = |format: &str, medium: bool| -> String {
        let mut tokens = vec![
            ("Artist Folder".to_string(), "Artist Name".to_string()),
            ("Artist Name".to_string(), "Artist Name".to_string()),
            ("Album Title".to_string(), "Album Title".to_string()),
            ("Release Year".to_string(), "2020".to_string()),
            ("Track".to_string(), "01".to_string()),
            ("Track Title".to_string(), "Track Title".to_string()),
            ("Extension".to_string(), "flac".to_string()),
        ];
        if medium {
            tokens.push(("Medium".to_string(), "01".to_string()));
        }
        cellarr_fs::render_name(format, &cellarr_core::NamingTokens { tokens }).unwrap_or_default()
    };
    let single =
        "{Artist Folder}/{Album Title} ({Release Year})/{Track} - {Track Title}.{Extension}";
    let multi = "{Artist Folder}/{Album Title} ({Release Year})/CD{Medium}/{Track} - {Track Title}.{Extension}";
    Json(json!({
        "singleTrackExample": render(single, false),
        "multiTrackExample": render(single, false),
        "singleMediumTrackExample": render(multi, true),
        "multiMediumTrackExample": render(multi, true),
        "artistFolderExample": "Artist Name",
    }))
}

// --- configuration singletons --------------------------------------------------
//
// Lidarr's settings pages (`/config/host`, `/config/naming`, …) are single records.
// cellarr keeps the settings that matter in its own configuration; these resources
// exist so a client can read and save Lidarr's page. The defaults are what a fresh
// Lidarr reports (`templates/*.json`, recorded from one); a PUT is stored, so a
// later GET returns what was saved.

const CONFIGS: &[(&str, &str)] = &[
    ("host", include_str!("templates/host.json")),
    ("ui", include_str!("templates/ui.json")),
    ("naming", include_str!("templates/naming.json")),
    (
        "mediamanagement",
        include_str!("templates/mediamanagement.json"),
    ),
    ("indexer", include_str!("templates/indexer.json")),
    (
        "downloadclient",
        include_str!("templates/downloadclient.json"),
    ),
    (
        "metadataprovider",
        include_str!("templates/metadataprovider.json"),
    ),
];

fn config_key(name: &str) -> String {
    format!("lidarr:config:{name}")
}

pub(crate) async fn read_config(state: &AppState, name: &str) -> ApiResult<Value> {
    let default = CONFIGS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, t)| serde_json::from_str::<Value>(t).ok())
        .ok_or_else(|| ApiError::NotFound(format!("no such configuration: {name}")))?;
    let stored = state.db.cache().get(&config_key(name)).await.ok().flatten();
    let mut value = stored
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(default.clone());
    // Fields a newer template adds appear even in an older saved copy.
    shim::merge_into(&mut value, Value::Null);
    if let (Some(v), Some(d)) = (value.as_object_mut(), default.as_object()) {
        for (k, x) in d {
            v.entry(k.clone()).or_insert_with(|| x.clone());
        }
    }
    Ok(value)
}

macro_rules! config_handlers {
    ($($get:ident, $put:ident => $name:literal),* $(,)?) => {$(
        pub(crate) async fn $get(State(fs): State<FaceState>) -> ApiResult<Json<Value>> {
            Ok(Json(read_config(app(&fs), $name).await?))
        }
        pub(crate) async fn $put(
            State(fs): State<FaceState>,
            Json(body): Json<Value>,
        ) -> ApiResult<Json<Value>> {
            let state = app(&fs);
            let mut current = read_config(state, $name).await?;
            shim::merge_into(&mut current, body);
            state
                .db
                .cache()
                .put(&config_key($name), &current.to_string(), None)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            Ok(Json(current))
        }
    )*};
}

config_handlers! {
    config_host, put_config_host => "host",
    config_ui, put_config_ui => "ui",
    config_naming, put_config_naming => "naming",
    config_mediamanagement, put_config_mediamanagement => "mediamanagement",
    config_indexer, put_config_indexer => "indexer",
    config_downloadclient, put_config_downloadclient => "downloadclient",
    config_metadataprovider, put_config_metadataprovider => "metadataprovider",
}

// --- download clients and indexer schemas (Lidarr-shaped) -------------------

/// Lidarr's category field is `musicCategory`; the shared shim stores a single
/// typed `category`. Rename on the way out so Lidarr-speaking tools see the
/// field they know.
fn lidarr_client(mut v: Value) -> Value {
    if let Some(fields) = v.get_mut("fields").and_then(Value::as_array_mut) {
        for f in fields {
            if f["name"] == "category" {
                f["name"] = json!("musicCategory");
            }
        }
    }
    v
}

pub(crate) async fn list_download_clients(
    State(fs): State<FaceState>,
) -> ApiResult<Json<Vec<Value>>> {
    let Json(all) = shim::list_download_clients(State(fs)).await?;
    Ok(Json(all.into_iter().map(lidarr_client).collect()))
}

pub(crate) async fn create_download_client(
    State(fs): State<FaceState>,
    q: Query<shim::ForceSaveQuery>,
    body: Json<shim::DownloadClientBody>,
) -> ApiResult<Json<Value>> {
    let Json(v) = shim::create_download_client(State(fs), q, body).await?;
    Ok(Json(lidarr_client(v)))
}

pub(crate) async fn update_download_client(
    State(fs): State<FaceState>,
    id: Path<String>,
    q: Query<shim::ForceSaveQuery>,
    body: Json<shim::DownloadClientBody>,
) -> ApiResult<Json<Value>> {
    let Json(v) = shim::update_download_client(State(fs), id, q, body).await?;
    Ok(Json(lidarr_client(v)))
}

/// A recorded schema list with the loaded plugins of `kind` appended.
fn schema_with_plugins(template: &str, kind: cellarr_plugins::catalog::EntryKind) -> Value {
    let mut list: Vec<Value> = serde_json::from_str(template).unwrap_or_default();
    list.extend(
        cellarr_plugins::catalog::of_kind(kind)
            .iter()
            .map(cellarr_plugins::catalog::schema_resource),
    );
    Value::Array(list)
}

pub(crate) async fn download_client_schema() -> Json<Value> {
    Json(schema_with_plugins(
        include_str!("templates/downloadclient_schema.json"),
        cellarr_plugins::catalog::EntryKind::DownloadClient,
    ))
}

pub(crate) async fn indexer_schema() -> Json<Value> {
    Json(schema_with_plugins(
        include_str!("templates/indexer_schema.json"),
        cellarr_plugins::catalog::EntryKind::Indexer,
    ))
}

/// `GET /system/plugins`: installed plugins in Lidarr's shape. Native ones come
/// from the sidecar; WASM ones are listed with their version.
pub(crate) async fn system_plugins() -> Json<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    if let Some(sidecar) = cellarr_plugins::sidecar::global() {
        if let Ok(Value::Array(list)) = sidecar.installed().await {
            out.extend(list);
        }
    }
    let next = out.len();
    out.extend(
        cellarr_plugins::catalog::entries()
            .iter()
            .filter(|e| e.runtime == "wasm")
            .enumerate()
            .map(|(i, e)| {
                json!({
                    "id": next + i + 1,
                    "name": e.name,
                    "owner": "cellarr",
                    "githubUrl": "",
                    "installedVersion": e.version,
                    "availableVersion": e.version,
                    "updateAvailable": false,
                })
            }),
    );
    Json(out)
}

/// `GET /plugin`: the loaded plugins, WASM and native.
pub(crate) async fn list_plugins() -> Json<Vec<Value>> {
    Json(
        cellarr_plugins::catalog::entries()
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "version": e.version,
                    "description": e.description,
                    "kind": match e.kind {
                        cellarr_plugins::catalog::EntryKind::Indexer => "indexer",
                        cellarr_plugins::catalog::EntryKind::DownloadClient => "downloadClient",
                    },
                    "protocol": e.protocol,
                    "runtime": e.runtime,
                })
            })
            .collect(),
    )
}

// --- small endpoints Lidarr serves that have nothing to show here ---------------

/// `GET /log`: the paged application log. cellarr logs to its own sink, not to a
/// table, so the page is empty; the envelope is Lidarr's.
pub(crate) async fn log_page(Query(q): Params) -> Json<Value> {
    let int = |k: &str, d: i64| param(&q, k).and_then(|v| v.parse().ok()).unwrap_or(d);
    Json(json!({
        "page": int("page", 1),
        "pageSize": int("pageSize", 10),
        "sortKey": param(&q, "sortKey").unwrap_or("Id"),
        "sortDirection": param(&q, "sortDirection").unwrap_or("descending"),
        "totalRecords": 0,
        "records": [],
    }))
}

/// `GET /rename`, `GET /retag`, `GET /manualimport`: previews with nothing pending.
pub(crate) async fn empty_list() -> Json<Vec<Value>> {
    Json(Vec::new())
}

/// `GET /system/backup`: the backups, or none when the backup engine is not set up.
pub(crate) async fn list_backups(State(fs): State<FaceState>) -> Json<Vec<Value>> {
    match shim::list_backups(State(fs)).await {
        Ok(Json(v)) => Json(v),
        Err(_) => Json(Vec::new()),
    }
}

/// `GET /tag/detail`: each tag with the things that use it.
pub(crate) async fn tag_details(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let Json(tags) = shim::list_tags(State(fs)).await?;
    Ok(Json(
        tags.into_iter()
            .map(|mut t| {
                if let Some(o) = t.as_object_mut() {
                    for k in [
                        "delayProfileIds",
                        "importListIds",
                        "notificationIds",
                        "restrictionIds",
                        "indexerIds",
                        "downloadClientIds",
                        "autoTagIds",
                        "artistIds",
                    ] {
                        o.insert(k.into(), json!([]));
                    }
                }
                t
            })
            .collect(),
    ))
}

/// `POST /release/push`: an indexer pushes a release it found. The fields Lidarr
/// requires are checked and reported the way it reports them.
pub(crate) async fn push_release(
    Json(body): Json<Value>,
) -> Result<Json<Vec<Value>>, axum::response::Response> {
    use axum::response::IntoResponse;
    let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default();
    let mut f = Failures::new();
    f.not_empty("Title", text("title"));
    if text("downloadUrl").is_empty() && text("magnetUrl").is_empty() {
        f.not_empty("DownloadUrl", "");
        f.not_empty("MagnetUrl", "");
    }
    f.not_empty("Protocol", text("protocol"));
    f.not_empty("PublishDate", text("publishDate"));
    f.check().map_err(IntoResponse::into_response)?;
    // A pushed release is accepted but not acted on yet: grabbing happens through
    // search, which has the full decision context.
    Ok(Json(Vec::new()))
}

// --- delay profiles: the catch-all is id 1, as in Lidarr --------------------

/// The shared numeric id of the catch-all delay profile (untagged, last in
/// order), which Lidarr numbers 1.
async fn catch_all_delay_id(fs: &FaceState) -> ApiResult<Option<i64>> {
    let all = fs.state.db.profiles().list_delay_profiles().await?;
    Ok(all
        .iter()
        .filter(|dp| dp.tags.is_empty())
        .max_by_key(|dp| dp.order)
        .map(|dp| shim::dp_numeric_id(dp.id)))
}

fn lidarr_delay(mut v: Value, catch_all: Option<i64>) -> Value {
    if v["id"].as_i64().is_some() && v["id"].as_i64() == catch_all {
        v["id"] = json!(1);
    }
    if let Some(o) = v.as_object_mut() {
        o.entry("bypassIfAboveCustomFormatScore")
            .or_insert(json!(false));
        o.entry("minimumCustomFormatScore").or_insert(json!(0));
    }
    v
}

/// Lidarr's id for a delay profile back to cellarr's.
async fn shim_delay_id(fs: &FaceState, id: &str) -> ApiResult<String> {
    if id == "1" {
        if let Some(real) = catch_all_delay_id(fs).await? {
            return Ok(real.to_string());
        }
    }
    Ok(id.to_string())
}

pub(crate) async fn list_delay_profiles(
    State(fs): State<FaceState>,
) -> ApiResult<Json<Vec<Value>>> {
    let catch_all = catch_all_delay_id(&fs).await?;
    let Json(all) = shim::list_delay_profiles(State(fs)).await?;
    Ok(Json(
        all.into_iter()
            .map(|v| lidarr_delay(v, catch_all))
            .collect(),
    ))
}

pub(crate) async fn get_delay_profile(
    State(fs): State<FaceState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let catch_all = catch_all_delay_id(&fs).await?;
    let real = shim_delay_id(&fs, &id).await?;
    let Json(v) = shim::get_delay_profile(State(fs), Path(real)).await?;
    Ok(Json(lidarr_delay(v, catch_all)))
}

pub(crate) async fn update_delay_profile(
    State(fs): State<FaceState>,
    Path(id): Path<String>,
    body: Json<shim::DelayProfileBody>,
) -> ApiResult<Json<Value>> {
    let catch_all = catch_all_delay_id(&fs).await?;
    let real = shim_delay_id(&fs, &id).await?;
    let Json(v) = shim::update_delay_profile(State(fs), Path(real), body).await?;
    Ok(Json(lidarr_delay(v, catch_all)))
}

pub(crate) async fn delete_delay_profile(
    State(fs): State<FaceState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let real = shim_delay_id(&fs, &id).await?;
    shim::delete_delay_profile(State(fs), Path(real)).await
}

// --- manual import ---------------------------------------------------------------

/// `GET /manualimport?downloadId=|folder=[&artistId=]`: what a download (or a
/// folder) would import as, file by file. Moves nothing.
pub(crate) async fn manual_import(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let Some(seam) = state.manual_import.clone() else {
        return Ok(Json(Vec::new()));
    };
    let music = state.db.music();
    let artist = match param(&q, "artistId").and_then(|v| v.parse::<i64>().ok()) {
        Some(id) => music.artist(id).await?.map(|a| a.content_id),
        None => None,
    };
    let download_id = param(&q, "downloadId").map(str::to_string);
    let candidates = seam
        .scan_music(crate::manual_import::MusicScanRequest {
            folder: param(&q, "folder").map(str::to_string),
            download_id: download_id.clone(),
            artist,
        })
        .await
        .map_err(ApiError::BadRequest)?;

    // Render each album, its artist and its tracks once.
    let album_ids: Vec<ContentId> = candidates.iter().filter_map(|c| c.album).collect();
    let albums: HashMap<ContentId, Album> = music
        .albums_by_content(&album_ids)
        .await?
        .into_iter()
        .map(|a| (a.content_id, a))
        .collect();
    let renderer = AlbumRenderer::load(state).await?;
    let unique: Vec<Album> = {
        let mut seen = HashSet::new();
        albums
            .values()
            .filter(|a| seen.insert(a.content_id))
            .cloned()
            .collect()
    };
    let rendered = renderer.render(state, &unique, false).await?;
    let album_json: HashMap<ContentId, Value> =
        unique.iter().map(|a| a.content_id).zip(rendered).collect();
    let pressings = music.releases_for_albums(&album_ids).await?;
    let mut tracks_by_content: HashMap<ContentId, cellarr_core::Track> = HashMap::new();
    for a in &unique {
        for t in music.tracks_for_album(a.content_id).await? {
            tracks_by_content.insert(t.content_id, t);
        }
    }

    let mut out = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        let album = c.album.and_then(|id| albums.get(&id));
        let album_v = c.album.and_then(|id| album_json.get(&id)).cloned();
        let artist_v = album_v.as_ref().and_then(|v| v.get("artist").cloned());
        let release_id = album.map(|al| {
            pressings
                .get(&al.content_id)
                .and_then(|list| {
                    list.iter()
                        .find(|r| Some(&r.mbid) == al.release_mbid.as_ref())
                        .or(list.first())
                        .map(|r| r.id)
                })
                .unwrap_or(al.id)
        });
        let tracks: Vec<Value> = c
            .tracks
            .iter()
            .filter_map(|t| tracks_by_content.get(t))
            .map(|t| {
                json!({
                    "id": t.id,
                    "albumId": album.map_or(0, |a| a.id),
                    "title": t.title,
                    "trackNumber": t.position.to_string(),
                    "absoluteTrackNumber": t.position,
                    "mediumNumber": t.disc,
                    "duration": t.duration_ms.unwrap_or(0),
                    "hasFile": false,
                })
            })
            .collect();
        let quality_name = c.quality.clone().unwrap_or_else(|| "Unknown".into());
        out.push(json!({
            "id": i + 1,
            "path": c.path,
            "name": std::path::Path::new(&c.path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            "size": c.size,
            "artist": artist_v,
            "album": album_v,
            "albumReleaseId": release_id.unwrap_or(0),
            "tracks": tracks,
            "quality": {
                "quality": {
                    "id": cellarr_core::lidarr_quality_id(&quality_name),
                    "name": quality_name,
                },
                "revision": { "version": 1, "real": 0, "isRepack": false },
            },
            "releaseGroup": "",
            "indexerFlags": 0,
            "downloadId": c.download_id,
            "additionalFile": false,
            "replaceExistingFiles": false,
            "disableReleaseSwitching": false,
            "rejections": c.rejections.iter().map(|r| json!({ "reason": r, "type": "permanent" })).collect::<Vec<_>>(),
        }));
    }
    Ok(Json(out))
}

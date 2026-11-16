//! Lidarr's interactive search: list the releases the indexers offer for an album,
//! and grab the one the user picks.
//!
//! Both are thin: the search and the grab are the pipeline's own (the same ones the
//! other faces use), and this module only addresses them by album and renders the
//! result as Lidarr's `ReleaseResource`.

use axum::extract::{Query, State};
use axum::Json;
use serde_json::{json, Value};

use super::{app, resources};
use crate::error::{ApiError, ApiResult};
use crate::shim::{self, FaceState, GrabReleaseBody, ReleaseQuery};

type Params = Query<Vec<(String, String)>>;

/// `GET /release?albumId=` — ranked candidates for one album, grabbing nothing.
pub(crate) async fn search(
    State(fs): State<FaceState>,
    Query(q): Params,
) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let album_id = q
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("albumId"))
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .ok_or_else(|| ApiError::BadRequest("albumId is required".into()))?;
    let album = state
        .db
        .music()
        .album(album_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("album {album_id} not found")))?;
    let artist = state
        .db
        .music()
        .artist_by_content(album.artist_content_id)
        .await?;

    let Json(rows) = shim::release_search(
        State(fs.clone()),
        Query(ReleaseQuery {
            content_id: Some(album.content_id.to_string()),
            movie_id: None,
            series_id: None,
            season_number: None,
        }),
    )
    .await?;

    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let rejected = r["rejected"].as_bool().unwrap_or(false);
                let quality = r["quality"]["quality"]["name"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_string();
                let rank = r["quality"]["quality"]["id"].as_u64().unwrap_or(0);
                let torrent = r["protocol"] == "torrent";
                json!({
                    "id": 0,
                    "guid": r["guid"],
                    "quality": resources::quality_object(&quality),
                    "qualityWeight": rank,
                    "age": 0,
                    "ageHours": 0.0,
                    "ageMinutes": 0.0,
                    "size": r["size"],
                    "indexerId": r["indexerId"],
                    "indexer": r["indexer"],
                    "releaseGroup": null,
                    "subGroup": null,
                    "releaseHash": "",
                    "title": r["title"],
                    "discography": false,
                    "sceneSource": false,
                    "airDate": null,
                    "artistName": artist.as_ref().map(|a| a.name.clone()),
                    "albumTitle": album.title,
                    "approved": !rejected,
                    "temporarilyRejected": false,
                    "rejected": rejected,
                    "rejections": r["rejections"],
                    "publishDate": now,
                    "commentUrl": null,
                    "downloadUrl": r["downloadUrl"],
                    "infoUrl": null,
                    "downloadAllowed": true,
                    "releaseWeight": 0,
                    "customFormats": [],
                    "customFormatScore": r["customFormatScore"],
                    "magnetUrl": r["downloadUrl"].as_str().filter(|u| u.starts_with("magnet:")),
                    "infoHash": null,
                    "seeders": if torrent { r["seeders"].clone() } else { Value::Null },
                    "leechers": null,
                    "protocol": r["protocol"],
                    "indexerFlags": 0,
                    "artistId": artist.as_ref().map(|a| a.id),
                    "albumId": album.id,
                    "downloadClientId": null,
                    "downloadClient": null,
                })
            })
            .collect(),
    ))
}

/// `POST /release` — grab the chosen release for its album.
pub(crate) async fn grab(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let album_id = body
        .get("albumId")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::BadRequest("the release must carry its albumId".into()))?;
    let album = state
        .db
        .music()
        .album(album_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("album {album_id} not found")))?;
    let Json(outcome) = shim::grab_release(
        State(fs.clone()),
        Json(GrabReleaseBody {
            guid: body.get("guid").and_then(Value::as_str).map(str::to_string),
            content_id: Some(album.content_id.to_string()),
            movie_id: None,
            series_id: None,
        }),
    )
    .await?;
    if outcome["grabbed"] == false {
        return Err(ApiError::Domain(
            outcome["message"]
                .as_str()
                .unwrap_or("the release could not be grabbed")
                .to_string(),
        ));
    }
    Ok(Json(body))
}

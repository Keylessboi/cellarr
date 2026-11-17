//! The Lidarr command endpoints.
//!
//! Lidarr's clients drive it by posting a command (`AlbumSearch`,
//! `RefreshArtist`, …) and polling `GET /command/{id}` until it finishes. cellarr
//! does the work as scheduler jobs; this module is the translation: a command is
//! an integer id over zero or more jobs, and its status is read from theirs.

use std::sync::Mutex;

use axum::extract::{Path, State};
use axum::Json;
use cellarr_core::Artist;
use cellarr_jobs::{JobKind, JobState, RetryPolicy};
use serde_json::{json, Value};
use time::OffsetDateTime;

use super::app;
use crate::commands;
use crate::error::{ApiError, ApiResult};
use crate::shim::FaceState;
use crate::state::AppState;

/// How many finished commands are remembered.
const KEEP: usize = 200;

struct Entry {
    id: i64,
    name: String,
    body: Value,
    jobs: Vec<String>,
    queued: OffsetDateTime,
    cancelled: bool,
}

#[derive(Default)]
struct Inner {
    next: i64,
    entries: Vec<Entry>,
}

/// The commands the Lidarr face has accepted, newest last.
#[derive(Default)]
pub struct CommandLog {
    inner: Mutex<Inner>,
}

impl CommandLog {
    fn record(&self, name: &str, body: Value, jobs: Vec<String>) -> i64 {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.next += 1;
        let id = inner.next;
        inner.entries.push(Entry {
            id,
            name: name.to_string(),
            body,
            jobs,
            queued: OffsetDateTime::now_utc(),
            cancelled: false,
        });
        if inner.entries.len() > KEEP {
            let drop = inner.entries.len() - KEEP;
            inner.entries.drain(..drop);
        }
        id
    }

    fn snapshot(&self) -> Vec<(i64, String, Value, Vec<String>, OffsetDateTime, bool)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .iter()
            .map(|e| {
                (
                    e.id,
                    e.name.clone(),
                    e.body.clone(),
                    e.jobs.clone(),
                    e.queued,
                    e.cancelled,
                )
            })
            .collect()
    }

    fn cancel(&self, id: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(e) = inner.entries.iter_mut().find(|e| e.id == id) {
            e.cancelled = true;
        }
    }
}

/// Queue a job and return its id. The daemon's scheduler loop runs it; the
/// request does not wait.
async fn queue_job(state: &AppState, kind: JobKind) -> ApiResult<String> {
    state
        .scheduler
        .submit_now(kind, RetryPolicy::default())
        .await
        .map_err(|e| ApiError::Command(e.to_string()))
}

/// Search every monitored album of `artist` that has no file.
pub(crate) async fn search_artist(state: &AppState, artist: &Artist) -> ApiResult<Vec<String>> {
    let music = state.db.music();
    let files: std::collections::HashSet<_> = music
        .track_files_for_artist(artist.content_id)
        .await?
        .into_iter()
        .map(|f| f.album_content_id)
        .collect();
    let mut jobs = Vec::new();
    for album in music.albums_for_artist(artist.content_id).await? {
        if album.monitored && !files.contains(&album.content_id) {
            jobs.push(
                queue_job(
                    state,
                    JobKind::ManualSearch {
                        content_id: album.content_id.to_string(),
                    },
                )
                .await?,
            );
        }
    }
    Ok(jobs)
}

fn ids(body: &Value, key: &str) -> Vec<i64> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// How many albums one Search Sniper run searches (`CELLARR_SEARCH_SNIPER_COUNT`).
fn sniper_count() -> usize {
    std::env::var("CELLARR_SEARCH_SNIPER_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(5)
}

/// Albums Search Sniper picked recently, so consecutive runs spread out.
static SNIPED: std::sync::Mutex<Vec<(cellarr_core::ContentId, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

/// `SearchSniper` (a Tubifarry command, which installs often schedule by hand):
/// search a few randomly chosen wanted albums. Done natively, because it acts
/// on the library, and the library is cellarr's.
async fn search_sniper(state: &AppState) -> ApiResult<(Vec<String>, String)> {
    let today = time::OffsetDateTime::now_utc().date().to_string();
    let music = state.db.music();
    let (_, wanted) = music
        .missing_albums(&today, cellarr_db::AlbumSort::Id, true, 0, 1_000_000)
        .await?;
    let day = std::time::Duration::from_secs(24 * 3600);
    let recent: Vec<cellarr_core::ContentId> = {
        let mut g = SNIPED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.retain(|(_, at)| at.elapsed() < day);
        g.iter().map(|(c, _)| *c).collect()
    };
    let mut eligible: Vec<_> = wanted
        .into_iter()
        .filter(|a| !recent.contains(&a.content_id))
        .collect();
    let collected = eligible.len();
    // A small xorshift from the clock is plenty for picking a few albums.
    let mut seed = time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u64 | 1;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let take = sniper_count().min(eligible.len());
    for i in 0..take {
        let j = i + (next() as usize) % (eligible.len() - i);
        eligible.swap(i, j);
    }
    let mut jobs = Vec::new();
    for album in eligible.into_iter().take(take) {
        jobs.push(
            queue_job(
                state,
                JobKind::ManualSearch {
                    content_id: album.content_id.to_string(),
                },
            )
            .await?,
        );
        SNIPED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((album.content_id, std::time::Instant::now()));
    }
    let message = format!(
        "Collected {collected} eligible album(s). Queued {} album(s) for search",
        jobs.len()
    );
    Ok((jobs, message))
}

/// Turn a command into the jobs that carry it out. `Ok(None)` is a command that
/// is recognized but has nothing to do.
async fn jobs_for(state: &AppState, name: &str, body: &Value) -> ApiResult<Option<Vec<String>>> {
    let music = state.db.music();
    let lower = name.to_ascii_lowercase();
    let artist_id = body.get("artistId").and_then(Value::as_i64);
    match lower.as_str() {
        "albumsearch" => {
            let mut jobs = Vec::new();
            for id in ids(body, "albumIds") {
                let album = music
                    .album(id)
                    .await?
                    .ok_or_else(|| ApiError::NotFound(format!("album {id} not found")))?;
                jobs.push(
                    queue_job(
                        state,
                        JobKind::ManualSearch {
                            content_id: album.content_id.to_string(),
                        },
                    )
                    .await?,
                );
            }
            Ok(Some(jobs))
        }
        "artistsearch" | "missingalbumsearch" if artist_id.is_some() => {
            let id = artist_id.unwrap_or_default();
            let artist = music
                .artist(id)
                .await?
                .ok_or_else(|| ApiError::NotFound(format!("artist {id} not found")))?;
            Ok(Some(search_artist(state, &artist).await?))
        }
        "missingalbumsearch" | "cutoffunmetalbumsearch" => Ok(Some(vec![
            queue_job(state, JobKind::MissingItemSearch).await?,
        ])),
        "refreshalbum" => {
            // An album is refreshed with its artist (that is where its release
            // groups come from); its track list is filled afresh.
            let mut ids_wanted = ids(body, "albumIds");
            if let Some(one) = body.get("albumId").and_then(Value::as_i64) {
                ids_wanted.push(one);
            }
            ids_wanted.sort_unstable();
            ids_wanted.dedup();
            let mut jobs = Vec::new();
            let mut artists = std::collections::BTreeSet::new();
            for id in ids_wanted {
                let album = music
                    .album(id)
                    .await?
                    .ok_or_else(|| ApiError::NotFound(format!("album {id} not found")))?;
                if artists.insert(album.artist_content_id.to_string()) {
                    jobs.push(
                        queue_job(
                            state,
                            JobKind::RefreshArtist {
                                content_id: album.artist_content_id.to_string(),
                            },
                        )
                        .await?,
                    );
                }
            }
            jobs.push(queue_job(state, JobKind::FillTracks).await?);
            Ok(Some(jobs))
        }
        "refreshartist" => {
            let jobs = match artist_id {
                Some(id) => {
                    let artist = music
                        .artist(id)
                        .await?
                        .ok_or_else(|| ApiError::NotFound(format!("artist {id} not found")))?;
                    vec![
                        queue_job(
                            state,
                            JobKind::RefreshArtist {
                                content_id: artist.content_id.to_string(),
                            },
                        )
                        .await?,
                    ]
                }
                None => {
                    let mut jobs = Vec::new();
                    for a in music.artists().await? {
                        jobs.push(
                            queue_job(
                                state,
                                JobKind::RefreshArtist {
                                    content_id: a.content_id.to_string(),
                                },
                            )
                            .await?,
                        );
                    }
                    jobs
                }
            };
            Ok(Some(jobs))
        }
        "rescanfolders" | "rescanartist" | "downloadedalbumsscan" => {
            Ok(Some(vec![queue_job(state, JobKind::RescanLibrary).await?]))
        }
        "rsssync" => Ok(Some(vec![queue_job(state, JobKind::RssSync).await?])),
        "processmonitoreddownloads" | "checkforfinisheddownload" => Ok(Some(vec![
            queue_job(state, JobKind::ReconcileDownloads).await?,
        ])),
        // Recognized housekeeping that has nothing to do here.
        "checkhealth" | "messaging" | "cleanupmediacovers" | "applicationupdate"
        | "housekeeping" => Ok(None),
        // Native Lidarr plugins live in the sidecar; installing one is its job.
        "installplugin" | "uninstallplugin" => {
            let sidecar = cellarr_plugins::sidecar::global().ok_or_else(|| {
                ApiError::BadRequest(
                    "native Lidarr plugins need the plugin sidecar (plugins.dotnet_sidecar_url)"
                        .into(),
                )
            })?;
            let url = body
                .get("githubUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::BadRequest("githubUrl is required".into()))?;
            let result = if lower == "installplugin" {
                sidecar.install(url).await
            } else {
                sidecar
                    .forward_command(&json!({ "name": "UninstallPlugin", "githubUrl": url }))
                    .await
            };
            result.map_err(|e| ApiError::Command(e.to_string()))?;
            Ok(None)
        }
        _ => Err(ApiError::BadRequest(format!("unknown command: {name}"))),
    }
}

async fn status_of(state: &AppState, jobs: &[String], cancelled: bool) -> &'static str {
    if cancelled {
        return "aborted";
    }
    if jobs.is_empty() {
        return "completed";
    }
    let all = commands::list_jobs(&state.scheduler)
        .await
        .unwrap_or_default();
    let mut running = false;
    let mut queued = false;
    for id in jobs {
        match all.iter().find(|j| &j.id == id).map(|j| j.state) {
            Some(JobState::Failed) => return "failed",
            Some(JobState::Running | JobState::Retrying) => running = true,
            Some(JobState::Scheduled) => queued = true,
            // Done, or no longer in the store: finished.
            Some(JobState::Done) | None => {}
        }
    }
    if running {
        "started"
    } else if queued {
        "queued"
    } else {
        "completed"
    }
}

fn resource(id: i64, name: &str, body: &Value, queued: OffsetDateTime, status: &str) -> Value {
    let when = queued
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let done = matches!(status, "completed" | "failed" | "aborted");
    let mut body = body.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("name".into(), json!(name));
        o.entry("sendUpdatesToClient").or_insert(json!(true));
        o.entry("updateScheduledTask").or_insert(json!(true));
        o.entry("completionMessage").or_insert(json!(null));
        o.entry("requiresDiskAccess").or_insert(json!(false));
        o.entry("isExclusive").or_insert(json!(false));
        o.entry("isTypeExclusive").or_insert(json!(false));
        o.entry("isLongRunning").or_insert(json!(false));
        o.entry("trigger").or_insert(json!("manual"));
    }
    json!({
        "id": id,
        "name": name,
        "commandName": name,
        "message": body.get("completionMessage").and_then(Value::as_str).unwrap_or_default(),
        "body": body,
        "priority": "normal",
        "status": status,
        "result": match status { "completed" => "successful", "failed" => "unsuccessful", _ => "unknown" },
        "queued": when,
        "started": if status == "queued" { Value::Null } else { json!(when) },
        "ended": if done { json!(when) } else { Value::Null },
        "duration": if done { json!("00:00:00") } else { Value::Null },
        "trigger": "manual",
        "stateChangeTime": when,
        "sendUpdatesToClient": true,
        "updateScheduledTask": true,
        "clientUserAgent": null,
        "exception": null,
        "lastExecutionTime": null,
    })
}

pub(crate) async fn create(
    State(fs): State<FaceState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("a command needs a name".into()))?
        .to_string();
    let mut body = body;
    let jobs = if name.eq_ignore_ascii_case("searchsniper") {
        let (jobs, message) = search_sniper(state).await?;
        if let Some(o) = body.as_object_mut() {
            o.insert("completionMessage".into(), json!(message));
        }
        jobs
    } else {
        jobs_for(state, &name, &body).await?.unwrap_or_default()
    };
    let id = state
        .lidarr_commands
        .record(&name, body.clone(), jobs.clone());
    let status = status_of(state, &jobs, false).await;
    Ok(Json(resource(
        id,
        &name,
        &body,
        OffsetDateTime::now_utc(),
        status,
    )))
}

pub(crate) async fn list(State(fs): State<FaceState>) -> ApiResult<Json<Vec<Value>>> {
    let state = app(&fs);
    let mut out = Vec::new();
    for (id, name, body, jobs, queued, cancelled) in state.lidarr_commands.snapshot() {
        let status = status_of(state, &jobs, cancelled).await;
        out.push(resource(id, &name, &body, queued, status));
    }
    Ok(Json(out))
}

pub(crate) async fn get(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let state = app(&fs);
    for (cid, name, body, jobs, queued, cancelled) in state.lidarr_commands.snapshot() {
        if cid == id {
            let status = status_of(state, &jobs, cancelled).await;
            return Ok(Json(resource(cid, &name, &body, queued, status)));
        }
    }
    Err(ApiError::NotFound(format!("command {id} not found")))
}

pub(crate) async fn cancel(
    State(fs): State<FaceState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    app(&fs).lidarr_commands.cancel(id);
    Ok(Json(json!({})))
}

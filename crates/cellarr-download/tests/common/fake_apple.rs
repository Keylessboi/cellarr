//! Fake Apple Music catalogue and a fake `applemusic-decryptor`, both over real
//! HTTP on loopback. They take any token (nothing here is, or may be, a real
//! credential) and record what they were sent so a test can assert on it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

pub const DEV_TOKEN: &str = "fake-developer-token";
pub const USER_TOKEN: &str = "fake-music-user-token";

// ------------------------------------------------------------ the catalogue

#[derive(Default)]
pub struct Catalog {
    pub albums: Vec<Value>,
    /// `(storefront, term, limit)` of each search.
    pub searches: Vec<(String, String, String)>,
    /// `(storefront, id)` of each lookup.
    pub lookups: Vec<(String, String)>,
    /// `(authorization, cookie, origin)` of each request.
    pub seen: Vec<(String, String, String)>,
    /// Answer every request with this status.
    pub fail_status: Option<u16>,
}

pub type SharedCatalog = Arc<Mutex<Catalog>>;

pub struct CatalogServer {
    pub url: String,
    pub state: SharedCatalog,
}

fn header(h: &HeaderMap, k: &str) -> String {
    h.get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

pub async fn start_catalog(albums: Vec<Value>) -> CatalogServer {
    let state: SharedCatalog = Arc::new(Mutex::new(Catalog {
        albums,
        ..Catalog::default()
    }));
    let app = Router::new()
        .route(
            "/v1/catalog/{sf}/search",
            get(
                |State(s): State<SharedCatalog>,
                 Path(sf): Path<String>,
                 Query(q): Query<HashMap<String, String>>,
                 h: HeaderMap| async move {
                    let mut c = s.lock().unwrap();
                    c.seen.push((
                        header(&h, "authorization"),
                        header(&h, "cookie"),
                        header(&h, "origin"),
                    ));
                    if let Some(code) = c.fail_status {
                        return StatusCode::from_u16(code).unwrap().into_response();
                    }
                    let term = q.get("term").cloned().unwrap_or_default();
                    c.searches.push((
                        sf,
                        term.clone(),
                        q.get("limit").cloned().unwrap_or_default(),
                    ));
                    // Apple's search is fuzzy: any word of the term matches a title or artist.
                    let words: Vec<String> = term
                        .to_lowercase()
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                    let hits: Vec<Value> = c
                        .albums
                        .iter()
                        .filter(|a| {
                            let t = format!(
                                "{} {}",
                                a["attributes"]["name"].as_str().unwrap_or_default(),
                                a["attributes"]["artistName"].as_str().unwrap_or_default()
                            )
                            .to_lowercase();
                            words.iter().any(|w| t.contains(w.as_str()))
                        })
                        .map(|a| {
                            let mut a = a.clone();
                            a.as_object_mut().unwrap().remove("relationships");
                            a
                        })
                        .collect();
                    Json(json!({"results": {"albums": {"data": hits}}})).into_response()
                },
            ),
        )
        .route(
            "/v1/catalog/{sf}/albums/{id}",
            get(
                |State(s): State<SharedCatalog>,
                 Path((sf, id)): Path<(String, String)>,
                 h: HeaderMap| async move {
                    let mut c = s.lock().unwrap();
                    c.seen.push((
                        header(&h, "authorization"),
                        header(&h, "cookie"),
                        header(&h, "origin"),
                    ));
                    if let Some(code) = c.fail_status {
                        return StatusCode::from_u16(code).unwrap().into_response();
                    }
                    c.lookups.push((sf, id.clone()));
                    match c.albums.iter().find(|a| a["id"] == json!(id)) {
                        Some(a) => Json(json!({"data": [a]})).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // The client appends `catalog/..` to its base, so the base is `<url>/v1/`.
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    CatalogServer { url, state }
}

/// One catalogue album with `n` tracks (a fuzzy search hit drops the tracks).
pub fn album(id: &str, title: &str, artist: &str, n: usize, traits: Option<&[&str]>) -> Value {
    let tracks: Vec<Value> = (1..=n)
        .map(|i| {
            json!({"id": format!("{id}0{i}"), "type": "songs", "attributes": {
                "name": format!("Track {i}"), "trackNumber": i, "discNumber": 1,
                "durationInMillis": 200_000 + i * 1000, "isrc": format!("ISRC{id}{i}")}})
        })
        .collect();
    let mut attrs = json!({"name": title, "artistName": artist, "releaseDate": "1959-08-17",
        "trackCount": n, "url": format!("https://music.apple.com/us/album/{id}")});
    if let Some(t) = traits {
        attrs["audioTraits"] = json!(t);
    }
    json!({"id": id, "type": "albums", "attributes": attrs,
           "relationships": {"tracks": {"data": tracks}}})
}

// ---------------------------------------------------------- the decryptor

pub struct Job {
    pub id: String,
    pub request: Value,
    pub status: String,
    pub progress: f64,
    pub directory: Option<String>,
    pub files: Vec<String>,
    pub error: Option<String>,
}

type Materialize = dyn Fn(&FsPath, &Value) -> Vec<String> + Send + Sync;

pub struct Decryptor {
    pub jobs: Vec<Job>,
    /// Refuse `POST /v1/download` with this error.
    pub refuse: Option<String>,
    /// Each poll moves a job queued -> running -> completed.
    pub auto_advance: bool,
    pub cancelled: Vec<String>,
    pub polls: u32,
    pub health_ok: bool,
    /// The decryptor's `OUTPUT_DIR`.
    pub output_dir: PathBuf,
    /// Writes the album's files under `OUTPUT_DIR` when a job completes and
    /// returns them relative to it (what the real decryptor reports in `files`).
    pub materialize: Option<Arc<Materialize>>,
    next: u32,
}

pub type SharedDecryptor = Arc<Mutex<Decryptor>>;

pub struct DecryptorServer {
    pub url: String,
    pub state: SharedDecryptor,
}

impl DecryptorServer {
    pub fn set_status(&self, status: &str) {
        for j in &mut self.state.lock().unwrap().jobs {
            j.status = status.to_string();
        }
    }
}

fn contained(requested: &str) -> bool {
    let r = requested.trim().trim_matches('/');
    !requested.trim().starts_with('/') && !r.split('/').any(|p| p == "..")
}

pub async fn start_decryptor(output_dir: PathBuf) -> DecryptorServer {
    let state: SharedDecryptor = Arc::new(Mutex::new(Decryptor {
        jobs: Vec::new(),
        refuse: None,
        auto_advance: false,
        cancelled: Vec::new(),
        polls: 0,
        health_ok: true,
        output_dir,
        materialize: None,
        next: 0,
    }));
    let app = Router::new()
        .route(
            "/healthz",
            get(|State(s): State<SharedDecryptor>| async move {
                if s.lock().unwrap().health_ok {
                    Json(json!({"status": "ok"})).into_response()
                } else {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }),
        )
        .route(
            "/v1/download",
            axum::routing::post(
                |State(s): State<SharedDecryptor>, body: String| async move {
                    let mut d = s.lock().unwrap();
                    let Ok(req) = serde_json::from_str::<Value>(&body) else {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad body"})))
                            .into_response();
                    };
                    if req["albumId"].is_null() && req["tracks"].is_null() {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": "albumId or tracks is required"})),
                        )
                            .into_response();
                    }
                    if let Some(why) = d.refuse.clone() {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": why})))
                            .into_response();
                    }
                    d.next += 1;
                    let id = format!("job{}", d.next);
                    let dir = req["directory"].as_str().unwrap_or_default().to_string();
                    let (status, error) = if contained(&dir) {
                        ("queued", None)
                    } else {
                        (
                            "failed",
                            Some(format!("refusing a directory outside OUTPUT_DIR: {dir}")),
                        )
                    };
                    d.jobs.push(Job {
                        id: id.clone(),
                        request: req,
                        status: status.into(),
                        progress: 0.0,
                        directory: None,
                        files: Vec::new(),
                        error,
                    });
                    (StatusCode::ACCEPTED, Json(json!({"job_id": id}))).into_response()
                },
            ),
        )
        .route(
            "/v1/job/{id}",
            get(
                |State(s): State<SharedDecryptor>, Path(id): Path<String>| async move {
                    let mut d = s.lock().unwrap();
                    d.polls += 1;
                    let auto = d.auto_advance;
                    let out = d.output_dir.clone();
                    let mat = d.materialize.clone();
                    let Some(j) = d.jobs.iter_mut().find(|j| j.id == id) else {
                        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown job"})))
                            .into_response();
                    };
                    if auto {
                        match j.status.as_str() {
                            "queued" => j.status = "running".into(),
                            "running" => {
                                j.status = "completed".into();
                                j.progress = 100.0;
                            }
                            _ => {}
                        }
                    }
                    if j.status == "completed" && j.files.is_empty() {
                        j.directory = j.request["directory"].as_str().map(String::from);
                        if let Some(m) = mat {
                            j.files = m(&out, &j.request);
                        }
                    }
                    Json(
                        json!({"status": j.status, "progress": j.progress, "total": 0,
                        "directory": j.directory, "files": j.files, "error": j.error}),
                    )
                    .into_response()
                },
            )
            .delete(
                |State(s): State<SharedDecryptor>, Path(id): Path<String>| async move {
                    let mut d = s.lock().unwrap();
                    d.cancelled.push(id.clone());
                    if let Some(j) = d.jobs.iter_mut().find(|j| j.id == id) {
                        j.status = "cancelled".into();
                    }
                    Json(json!({"cancelled": true}))
                },
            ),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    DecryptorServer { url, state }
}

#[allow(unused)]
fn _assert_response(_: Response) {}

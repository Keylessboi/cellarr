//! A fake slskd HTTP server: just the endpoints cellarr uses, with the JSON
//! shapes slskd answers with. State is shared so a test can script responses and
//! inspect what cellarr sent.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::extract::{Path, RawQuery, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub const API_KEY: &str = "test-key";

#[derive(Default)]
pub struct Fake {
    /// What `GET /searches/{id}/responses` returns.
    pub responses: Vec<Value>,
    /// Search texts received.
    pub searches: Vec<String>,
    /// `GET /searches/{id}` calls that answer "not complete" first.
    pub incomplete_polls: u32,
    pub polls: u32,
    pub searches_deleted: u32,
    /// `(username, body)` of each enqueue.
    pub enqueued: Vec<(String, Value)>,
    /// When set, enqueue answers with this `failed` list.
    pub refuse: Option<String>,
    /// username -> files: (id, filename, size, state, bytes)
    pub transfers: Vec<(String, String, String, u64, String, u64)>,
    pub deleted: Vec<String>,
    /// Every transfer reads as `Completed, Succeeded` once listed (the files are
    /// already on disk in the test).
    pub auto_complete: bool,
    pub auth_failures: u32,
    /// The transfers listing answers 500 while set.
    pub fail_listing: bool,
    /// The transfers listing answers an empty array while set.
    pub empty_listing: bool,
    /// Every `DELETE` as `(username, id, raw query)`.
    pub delete_calls: Vec<(String, String, String)>,
    next_id: u32,
}

pub type Shared = Arc<Mutex<Fake>>;

pub struct Server {
    pub url: String,
    pub state: Shared,
}

impl Server {
    pub fn set_state_all(&self, state: &str) {
        let mut f = self.state.lock().unwrap();
        for t in &mut f.transfers {
            t.4 = state.to_string();
            if state.contains("Succeeded") {
                t.5 = t.3;
            }
        }
    }
}

async fn auth(State(s): State<Shared>, req: Request, next: Next) -> Response {
    if req.headers().get("x-api-key").and_then(|v| v.to_str().ok()) != Some(API_KEY) {
        s.lock().unwrap().auth_failures += 1;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

pub async fn start(responses: Vec<Value>) -> Server {
    let state: Shared = Arc::new(Mutex::new(Fake {
        responses,
        ..Fake::default()
    }));
    let app = Router::new()
        .route(
            "/api/v0/application",
            get(|| async { Json(json!({"version": {"current": "0.0.0"}})) }),
        )
        .route(
            "/api/v0/searches",
            post(|State(s): State<Shared>, Json(b): Json<Value>| async move {
                s.lock()
                    .unwrap()
                    .searches
                    .push(b["searchText"].as_str().unwrap_or_default().to_string());
                Json(json!({"id": b["id"], "state": "InProgress", "isComplete": false}))
            }),
        )
        .route(
            "/api/v0/searches/{id}",
            get(|State(s): State<Shared>, Path(id): Path<String>| async move {
                let mut f = s.lock().unwrap();
                f.polls += 1;
                let done = f.polls > f.incomplete_polls;
                Json(json!({"id": id, "isComplete": done,
                    "state": if done {"Completed, ResponseLimitReached"} else {"InProgress"}}))
            })
            .delete(|State(s): State<Shared>| async move {
                s.lock().unwrap().searches_deleted += 1;
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/api/v0/searches/{id}/responses",
            get(|State(s): State<Shared>| async move {
                Json(Value::Array(s.lock().unwrap().responses.clone()))
            }),
        )
        .route(
            "/api/v0/transfers/downloads/{user}",
            post(
                |State(s): State<Shared>, Path(user): Path<String>, Json(b): Json<Value>| async move {
                    let mut f = s.lock().unwrap();
                    if let Some(why) = f.refuse.clone() {
                        return (
                            StatusCode::CREATED,
                            Json(json!({"enqueued": [], "failed": [why]})),
                        );
                    }
                    for file in b.as_array().cloned().unwrap_or_default() {
                        f.next_id += 1;
                        let id = format!("id-{}", f.next_id);
                        f.transfers.push((
                            user.clone(),
                            id,
                            file["filename"].as_str().unwrap_or_default().to_string(),
                            file["size"].as_u64().unwrap_or(0),
                            "Queued, Remotely".into(),
                            0,
                        ));
                    }
                    f.enqueued.push((user, b));
                    (StatusCode::CREATED, Json(json!({"enqueued": [], "failed": []})))
                },
            ),
        )
        .route(
            "/api/v0/transfers/downloads",
            get(|State(s): State<Shared>| async move {
                let mut f = s.lock().unwrap();
                if f.fail_listing {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!("boom")));
                }
                if f.empty_listing {
                    return (StatusCode::OK, Json(json!([])));
                }
                if f.auto_complete {
                    for t in &mut f.transfers {
                        t.4 = "Completed, Succeeded".into();
                        t.5 = t.3;
                    }
                }
                let mut users: Vec<String> = f.transfers.iter().map(|t| t.0.clone()).collect();
                users.dedup();
                let out: Vec<Value> = users
                    .iter()
                    .map(|u| {
                        let files: Vec<Value> = f
                            .transfers
                            .iter()
                            .filter(|t| &t.0 == u)
                            .map(|t| {
                                json!({"id": t.1, "username": t.0, "filename": t.2, "size": t.3,
                                       "state": t.4, "bytesTransferred": t.5})
                            })
                            .collect();
                        json!({"username": u, "directories": [{"directory": "d", "fileCount": files.len(), "files": files}]})
                    })
                    .collect();
                (StatusCode::OK, Json(Value::Array(out)))
            }),
        )
        .route(
            "/api/v0/transfers/downloads/{user}/{id}",
            delete(
                |State(s): State<Shared>,
                 Path((u, id)): Path<(String, String)>,
                 RawQuery(q): RawQuery| async move {
                    let mut f = s.lock().unwrap();
                    f.delete_calls.push((u, id.clone(), q.unwrap_or_default()));
                    f.deleted.push(id.clone());
                    f.transfers.retain(|t| t.1 != id);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, state }
}

/// A peer's search response with `n` tracks in `dir`.
pub fn peer(user: &str, dir: &str, n: usize, ext: &str, rate: Option<u32>) -> Value {
    let files: Vec<Value> = (1..=n)
        .map(|i| {
            let mut f = json!({"filename": format!("{dir}\\{i:02} - Track {i}.{ext}"),
                "size": 1000 + i, "extension": ext, "isLocked": false});
            if let Some(r) = rate {
                f["bitRate"] = json!(r);
            }
            f
        })
        .collect();
    json!({"username": user, "fileCount": n, "files": files,
           "hasFreeUploadSlot": true, "uploadSpeed": 1000, "queueLength": 0})
}

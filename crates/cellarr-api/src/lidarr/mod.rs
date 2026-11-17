//! The Lidarr-compatible API: `/api/v1` as Lidarr serves it.
//!
//! cellarr presents a Lidarr face so an existing music stack keeps working when
//! cellarr replaces Lidarr: Prowlarr syncs indexers to it, Octo and scripts add
//! artists and queue searches, dashboards read its queue and wanted list. The
//! face is **a translation layer and nothing more**. Every handler reads or
//! writes cellarr's own records (`cellarr_db::MusicRepo`, the grab queue, the
//! scheduler); none holds state of its own besides the in-memory command log.
//!
//! ## Staying current with Lidarr
//!
//! Lidarr's API is a published contract (its OpenAPI document), and the whole
//! point of this layout is that following it is mechanical:
//!
//! - **One module per concern.** [`resources`] is the only place Lidarr's field
//!   names appear; [`quality`] the only place its quality numbering appears;
//!   [`handlers`], [`activity`] and [`command`] only route and call the data layer.
//! - **A pinned contract.** `tests/contracts/lidarr-v1.json` records the paths,
//!   methods and resource fields cellarr is checked against, generated from
//!   Lidarr's OpenAPI document by `scripts/lidarr-contract`. When Lidarr changes,
//!   regenerating it and running the contract test says exactly which endpoint or
//!   field moved; see `docs/18-lidarr-compat.md`.
//! - **Shared resources are shared.** Indexers, download clients, tags, root
//!   folders, notifications and the system endpoints are the same resources in
//!   every *arr, so this face mounts the shim's handlers for them rather than
//!   re-implementing them. A fix there fixes every face.
//!
//! Unlike the video faces, this one speaks **API v1** and is intended to be
//! served at the root of its own listener (`/api/v1/...`), the place Lidarr's
//! clients look; it is also mounted at `/lidarr/api/v1` on the main listener.

pub mod activity;
pub mod command;
pub mod defaults;
pub mod handlers;
pub mod quality;
pub mod release;
pub mod resources;
pub mod validation;

use axum::http::{HeaderValue, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};

pub use command::CommandLog;

use crate::auth::require_api_key;
use crate::error::ApiError;
use crate::shim::{self, Face, FaceState, LIDARR_VERSION};
use crate::state::AppState;

/// Build the Lidarr face's router (every path relative to `/api/v1`).
///
/// Every route requires the API key, as in Lidarr itself.
pub fn router(state: AppState) -> Router {
    // Lidarr matches routes case-insensitively, and clients rely on it: Octo
    // and the queue-maintenance script both ask for `/trackFile`. Lower-case the
    // path before routing (a log file's own name is left as it is).
    let inner = routes(state);
    Router::new().fallback_service(
        tower::ServiceBuilder::new()
            .map_request(|mut req: Request<axum::body::Body>| {
                let lowered = lower_path(req.uri());
                if let Some(uri) = lowered {
                    *req.uri_mut() = uri;
                }
                req
            })
            .service(inner),
    )
}

/// The request URI with its path lower-cased, or `None` when it already is.
fn lower_path(uri: &axum::http::Uri) -> Option<axum::http::Uri> {
    let path = uri.path();
    let (head, tail) = match path.to_ascii_lowercase().find("/log/file/") {
        Some(i) => (
            &path[..i + "/log/file/".len()],
            &path[i + "/log/file/".len()..],
        ),
        None => (path, ""),
    };
    if !head.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut rebuilt = format!("{}{tail}", head.to_ascii_lowercase());
    if let Some(q) = uri.query() {
        rebuilt.push('?');
        rebuilt.push_str(q);
    }
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = rebuilt.parse().ok();
    axum::http::Uri::from_parts(parts).ok()
}

fn routes(state: AppState) -> Router {
    let fs = FaceState {
        state: state.clone(),
        face: Face::Lidarr,
    };

    Router::new()
        // --- system, shared with the other faces ------------------------------
        .route("/system/status", get(handlers::system_status))
        .route("/system/task", get(shim::system_tasks))
        .route("/system/task/{id}", get(shim::system_task))
        .route("/health", get(shim::health))
        .route(
            "/system/backup",
            get(handlers::list_backups).post(shim::create_backup),
        )
        .route("/system/backup/{id}", delete(shim::delete_backup))
        .route("/system/backup/restore/{id}", post(shim::restore_backup_id))
        .route(
            "/system/backup/restore/upload",
            post(shim::restore_backup_upload),
        )
        .route("/log", get(handlers::log_page))
        .route("/rename", get(handlers::empty_list))
        .route("/retag", get(handlers::empty_list))
        .route("/manualimport", get(handlers::empty_list))
        .route("/release/push", post(handlers::push_release))
        .route("/tag/detail", get(handlers::tag_details))
        .route("/ping", get(ping).head(ping))
        .route("/plugin", get(handlers::list_plugins))
        .route("/system/plugins", get(handlers::system_plugins))
        .route("/log/file", get(shim::list_log_files))
        .route("/log/file/{name}", get(shim::read_log_file))
        .route("/update", get(shim::list_updates))
        .route("/diskspace", get(handlers::diskspace))
        // --- configuration shared with the other faces ------------------------
        .route(
            "/rootfolder",
            get(handlers::root_folders).post(handlers::create_root_folder),
        )
        .route("/rootfolder/{id}", delete(shim::delete_root_folder))
        .route("/tag", get(shim::list_tags).post(handlers::create_tag))
        .route(
            "/tag/{id}",
            get(shim::get_tag)
                .put(shim::update_tag)
                .delete(shim::delete_tag),
        )
        .route(
            "/indexer",
            get(shim::list_indexers).post(shim::create_indexer),
        )
        .route("/indexer/schema", get(handlers::indexer_schema))
        .route("/indexer/test", post(shim::test_indexer))
        .route(
            "/indexer/{id}",
            get(shim::get_indexer)
                .put(shim::update_indexer)
                .delete(shim::delete_indexer),
        )
        .route(
            "/downloadclient",
            get(handlers::list_download_clients).post(handlers::create_download_client),
        )
        .route(
            "/downloadclient/schema",
            get(handlers::download_client_schema),
        )
        .route("/downloadclient/test", post(shim::test_download_client))
        .route(
            "/downloadclient/{id}",
            put(handlers::update_download_client).delete(shim::delete_download_client),
        )
        .route(
            "/remotepathmapping",
            get(shim::list_remote_path_mappings).post(shim::create_remote_path_mapping),
        )
        .route(
            "/remotepathmapping/{id}",
            put(shim::update_remote_path_mapping).delete(shim::delete_remote_path_mapping),
        )
        .route(
            "/notification",
            get(shim::list_notifications).post(shim::create_notification),
        )
        .route("/notification/schema", get(shim::notification_schema))
        .route("/notification/test", post(shim::test_notification))
        .route(
            "/notification/{id}",
            get(shim::get_notification)
                .put(shim::update_notification)
                .delete(shim::delete_notification),
        )
        .route(
            "/customformat",
            get(shim::list_custom_formats).post(shim::create_custom_format),
        )
        .route("/customformat/schema", get(shim::custom_format_schema))
        .route(
            "/customformat/{id}",
            get(shim::get_custom_format)
                .put(shim::update_custom_format)
                .delete(shim::delete_custom_format),
        )
        .route(
            "/delayprofile",
            get(handlers::list_delay_profiles).post(shim::create_delay_profile),
        )
        .route(
            "/delayprofile/{id}",
            get(handlers::get_delay_profile)
                .put(handlers::update_delay_profile)
                .delete(handlers::delete_delay_profile),
        )
        .route(
            "/releaseprofile",
            get(shim::list_release_profiles).post(shim::create_release_profile),
        )
        .route(
            "/releaseprofile/{id}",
            get(shim::get_release_profile)
                .put(shim::update_release_profile)
                .delete(shim::delete_release_profile),
        )
        .route(
            "/importlist",
            get(shim::list_import_lists).post(shim::create_import_list),
        )
        .route("/importlist/schema", get(shim::import_list_schema))
        .route(
            "/importlist/{id}",
            get(shim::get_import_list)
                .put(shim::update_import_list)
                .delete(shim::delete_import_list),
        )
        .route(
            "/importlistexclusion",
            get(shim::list_import_list_exclusions).post(shim::create_import_list_exclusion),
        )
        .route(
            "/importlistexclusion/{id}",
            delete(shim::delete_import_list_exclusion),
        )
        .route("/config/naming/examples", get(handlers::naming_examples))
        .route(
            "/config/host",
            get(handlers::config_host).put(handlers::put_config_host),
        )
        .route(
            "/config/host/{id}",
            get(handlers::config_host).put(handlers::put_config_host),
        )
        .route(
            "/config/ui",
            get(handlers::config_ui).put(handlers::put_config_ui),
        )
        .route(
            "/config/ui/{id}",
            get(handlers::config_ui).put(handlers::put_config_ui),
        )
        .route(
            "/config/naming",
            get(handlers::config_naming).put(handlers::put_config_naming),
        )
        .route(
            "/config/naming/{id}",
            get(handlers::config_naming).put(handlers::put_config_naming),
        )
        .route(
            "/config/mediamanagement",
            get(handlers::config_mediamanagement).put(handlers::put_config_mediamanagement),
        )
        .route(
            "/config/mediamanagement/{id}",
            get(handlers::config_mediamanagement).put(handlers::put_config_mediamanagement),
        )
        .route(
            "/config/indexer",
            get(handlers::config_indexer).put(handlers::put_config_indexer),
        )
        .route(
            "/config/indexer/{id}",
            get(handlers::config_indexer).put(handlers::put_config_indexer),
        )
        .route(
            "/config/downloadclient",
            get(handlers::config_downloadclient).put(handlers::put_config_downloadclient),
        )
        .route(
            "/config/downloadclient/{id}",
            get(handlers::config_downloadclient).put(handlers::put_config_downloadclient),
        )
        .route(
            "/config/metadataprovider",
            get(handlers::config_metadataprovider).put(handlers::put_config_metadataprovider),
        )
        .route(
            "/config/metadataprovider/{id}",
            get(handlers::config_metadataprovider).put(handlers::put_config_metadataprovider),
        )
        .route("/language", get(handlers::languages))
        .route("/indexerflag", get(handlers::indexer_flags))
        // --- the music library ------------------------------------------------
        .route(
            "/artist",
            get(handlers::list_artists).post(handlers::add_artist),
        )
        .route("/artist/lookup", get(handlers::artist_lookup))
        .route(
            "/artist/editor",
            put(handlers::artist_editor).delete(handlers::artist_editor_delete),
        )
        .route(
            "/artist/{id}",
            get(handlers::get_artist)
                .put(handlers::update_artist)
                .delete(handlers::delete_artist),
        )
        .route(
            "/album",
            get(handlers::list_albums).post(handlers::add_album),
        )
        .route("/album/lookup", get(handlers::album_lookup))
        .route("/album/monitor", put(handlers::album_monitor))
        .route(
            "/album/{id}",
            get(handlers::get_album)
                .put(handlers::update_album)
                .delete(handlers::delete_album),
        )
        .route("/track", get(handlers::list_tracks))
        .route("/track/{id}", get(handlers::get_track))
        .route("/trackfile", get(handlers::list_track_files))
        .route("/trackfile/bulk", delete(handlers::delete_track_files))
        .route(
            "/trackfile/{id}",
            get(handlers::get_track_file).delete(handlers::delete_track_file),
        )
        // --- profiles ---------------------------------------------------------
        .route(
            "/qualityprofile",
            get(handlers::quality_profiles).post(handlers::create_quality_profile),
        )
        .route(
            "/qualityprofile/schema",
            get(handlers::quality_profile_schema),
        )
        .route(
            "/qualityprofile/{id}",
            get(handlers::get_quality_profile)
                .put(handlers::update_quality_profile)
                .delete(handlers::delete_quality_profile),
        )
        .route("/qualitydefinition", get(handlers::quality_definitions))
        .route(
            "/metadataprofile",
            get(handlers::metadata_profiles).post(handlers::create_metadata_profile),
        )
        .route(
            "/metadataprofile/schema",
            get(handlers::metadata_profile_schema),
        )
        .route(
            "/metadataprofile/{id}",
            get(handlers::get_metadata_profile)
                .put(handlers::update_metadata_profile)
                .delete(handlers::delete_metadata_profile),
        )
        // --- activity ---------------------------------------------------------
        .route("/queue", get(activity::queue))
        .route("/queue/status", get(activity::queue_status))
        .route("/queue/details", get(activity::queue_details))
        .route("/queue/bulk", delete(activity::queue_remove_bulk))
        .route("/queue/{id}", delete(activity::queue_remove))
        .route("/history", get(activity::history))
        .route("/history/since", get(activity::history_since))
        .route("/history/artist", get(activity::history_artist))
        .route("/wanted/missing", get(activity::wanted_missing))
        .route("/wanted/cutoff", get(activity::wanted_cutoff))
        .route("/calendar", get(activity::calendar))
        .route("/blocklist", get(activity::blocklist))
        .route("/blocklist/bulk", delete(shim::delete_blocklist_bulk))
        .route("/blocklist/{id}", delete(shim::delete_blocklist_item))
        .route("/parse", get(activity::parse))
        .route("/release", get(release::search).post(release::grab))
        // --- resources addressed by id: singletons and single reads ----------
        .route(
            "/rootfolder/{id}",
            get(handlers::root_folder).put(handlers::root_folder_update),
        )
        .route(
            "/remotepathmapping/{id}",
            get(handlers::remote_path_mapping),
        )
        .route("/downloadclient/{id}", get(handlers::download_client))
        .route(
            "/qualitydefinition/{id}",
            get(handlers::quality_definition).put(handlers::echo_put),
        )
        .route("/qualitydefinition/update", put(handlers::echo_list))
        .route("/language/{id}", get(handlers::language))
        .route("/calendar/{id}", get(activity::album_by_id))
        .route("/wanted/missing/{id}", get(activity::album_by_id))
        .route("/wanted/cutoff/{id}", get(activity::album_by_id))
        // --- commands ---------------------------------------------------------
        .route("/command", get(command::list).post(command::create))
        .route("/command/{id}", get(command::get).delete(command::cancel))
        .with_state(fs.clone())
        .layer(middleware::from_fn_with_state(state, require_api_key))
        .fallback(not_found)
        .layer(middleware::from_fn(compat))
        .layer(middleware::from_fn(version_header))
}

/// Collection endpoints whose `POST` Lidarr answers `201 Created` (every other `POST`,
/// such as a connection test or a grab, is `200`).
const CREATED: &[&str] = &[
    "/artist",
    "/album",
    "/rootfolder",
    "/tag",
    "/qualityprofile",
    "/metadataprofile",
    "/indexer",
    "/downloadclient",
    "/notification",
    "/importlist",
    "/importlistexclusion",
    "/remotepathmapping",
    "/customformat",
    "/delayprofile",
    "/releaseprofile",
    "/command",
    "/autotagging",
    "/customfilter",
    "/metadata",
];

/// Fields cellarr's shared handlers add that Lidarr's resources do not carry.
const NOT_LIDARR: &[&str] = &[
    "managed",
    "unmappedFolders",
    // Fields cellarr's resources carry that a real Lidarr never sends.
    "allMusicId",
    "cleanTitle",
    "grabbed",
    "lastSearchTime",
    "mbId",
    "members",
    "remotePoster",
    "mediaCount",
    "selectOptionsProviderAction",
    "titleWithoutYear",
];

fn strip_foreign(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in NOT_LIDARR {
                m.remove(*k);
            }
            // An artist in the library has a root folder and no `folder`; only a
            // lookup result (which has no root folder) carries one.
            if m.contains_key("rootFolderPath") {
                m.remove("folder");
            }
            // An artist embedded in an album carries neither.
            if let Some(Value::Object(a)) = m.get_mut("artist") {
                if a.remove("rootFolderPath").is_some() {
                    a.remove("folder");
                }
            }
            m.values_mut().for_each(strip_foreign);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_foreign),
        _ => {}
    }
}

/// Make cellarr's answers Lidarr-shaped where the shared handlers differ in small,
/// systematic ways: `201` on create, `{message, description}` errors, and no
/// cellarr-only fields. Everything else passes through untouched.
async fn compat(req: Request<axum::body::Body>, next: Next) -> Response {
    use axum::http::{header, Method, StatusCode};
    let method = req.method().clone();
    let path = req.uri().path().trim_end_matches('/').to_string();
    let resp = next.run(req).await;
    let (mut parts, body) = resp.into_parts();
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if !is_json {
        // A body that would not parse (axum answers in plain text) is reported the
        // way ASP.NET does: an RFC 9110 problem document naming the path.
        if matches!(parts.status.as_u16(), 400 | 415 | 422) {
            if let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await {
                let text = String::from_utf8_lossy(&bytes).to_string();
                parts.headers.remove(header::CONTENT_LENGTH);
                parts.status = StatusCode::BAD_REQUEST;
                parts.headers.insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("application/problem+json"),
                );
                let doc = json!({
                    "type": "https://tools.ietf.org/html/rfc9110#section-15.5.1",
                    "title": "One or more validation errors occurred.",
                    "status": 400,
                    "errors": { "$": [text] },
                });
                return Response::from_parts(parts, axum::body::Body::from(doc.to_string()));
            }
            return Response::from_parts(parts, axum::body::Body::empty());
        }
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, 512 * 1024 * 1024).await else {
        return Response::from_parts(parts, axum::body::Body::empty());
    };
    // Large list bodies (the album dump) are not rewritten: they carry none of the
    // fields below, and re-parsing tens of megabytes would defeat the streaming.
    if bytes.len() > 8 * 1024 * 1024 {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    if parts.status.is_client_error() || parts.status.is_server_error() {
        if let (Some(msg), true) = (value.get("message").cloned(), value.get("code").is_some()) {
            value = json!({ "message": msg, "description": msg });
        }
    } else {
        strip_foreign(&mut value);
        if method == Method::POST
            && parts.status == StatusCode::OK
            && CREATED.contains(&path.as_str())
        {
            parts.status = StatusCode::CREATED;
        }
        // Lidarr answers an artist edit, the bulk editor and the bulk album
        // monitor with 202 Accepted.
        let accepted = path == "/artist/editor"
            || path == "/album/monitor"
            || path
                .strip_prefix("/artist/")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
        if method == Method::PUT && parts.status == StatusCode::OK && accepted {
            parts.status = StatusCode::ACCEPTED;
        }
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    let out = serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec());
    Response::from_parts(parts, axum::body::Body::from(out))
}

/// The router for a dedicated Lidarr listener: `/api/v1/*`, the open `/ping`
/// liveness probe at the root, and nothing else.
pub fn listener_router(state: AppState) -> Router {
    Router::new()
        .route("/ping", get(ping).head(ping))
        .nest("/api/v1", router(state))
        .fallback(not_found)
}

async fn ping() -> Json<Value> {
    Json(json!({ "status": "OK" }))
}

async fn not_found() -> ApiError {
    ApiError::NotFound("unknown api endpoint".into())
}

/// Every response carries the emulated Lidarr version, which Prowlarr reads from
/// the header to gate its integration.
async fn version_header(req: Request<axum::body::Body>, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "X-Application-Version",
        HeaderValue::from_static(LIDARR_VERSION),
    );
    resp
}

/// Shared by the handler modules: the `AppState` of a face state.
pub(crate) fn app(fs: &FaceState) -> &AppState {
    &fs.state
}

/// A catalogue error as the API error a client should see.
pub(crate) fn catalogue_error(e: cellarr_jobs::music_sync::CatalogueError) -> ApiError {
    use cellarr_jobs::music_sync::CatalogueError as E;
    match e {
        E::NotFound(m) => ApiError::NotFound(m),
        E::Metadata(e) => ApiError::Upstream(e.to_string()),
        E::Db(e) => ApiError::Db(e),
    }
}

/// The paged envelope Lidarr wraps list endpoints in.
#[must_use]
pub(crate) fn page(
    records: Vec<Value>,
    total: u64,
    page: u64,
    size: u64,
    key: &str,
    dir: &str,
) -> Value {
    json!({
        "page": page,
        "pageSize": size,
        "sortKey": key,
        "sortDirection": dir,
        "totalRecords": total,
        "records": records,
    })
}

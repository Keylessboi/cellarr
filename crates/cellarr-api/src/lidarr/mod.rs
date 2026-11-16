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
pub mod handlers;
pub mod quality;
pub mod resources;

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
    let fs = FaceState {
        state: state.clone(),
        face: Face::Lidarr,
    };

    let routes = Router::new()
        // --- system, shared with the other faces ------------------------------
        .route("/system/status", get(shim::system_status))
        .route("/system/task", get(shim::system_tasks))
        .route("/system/task/{id}", get(shim::system_task))
        .route("/health", get(shim::health))
        .route("/system/backup", get(shim::list_backups).post(shim::create_backup))
        .route("/system/backup/{id}", delete(shim::delete_backup))
        .route("/system/backup/restore/{id}", post(shim::restore_backup_id))
        .route("/system/backup/restore/upload", post(shim::restore_backup_upload))
        .route("/log/file", get(shim::list_log_files))
        .route("/log/file/{name}", get(shim::read_log_file))
        .route("/update", get(shim::list_updates))
        .route("/diskspace", get(handlers::diskspace))
        // --- configuration shared with the other faces ------------------------
        .route("/rootfolder", get(handlers::root_folders).post(shim::create_root_folder))
        .route("/rootfolder/{id}", delete(shim::delete_root_folder))
        .route("/tag", get(shim::list_tags).post(shim::create_tag))
        .route(
            "/tag/{id}",
            get(shim::get_tag).put(shim::update_tag).delete(shim::delete_tag),
        )
        .route("/indexer", get(shim::list_indexers).post(shim::create_indexer))
        .route("/indexer/schema", get(shim::indexer_schema))
        .route("/indexer/test", post(shim::test_indexer))
        .route(
            "/indexer/{id}",
            get(shim::get_indexer)
                .put(shim::update_indexer)
                .delete(shim::delete_indexer),
        )
        .route(
            "/downloadclient",
            get(shim::list_download_clients).post(shim::create_download_client),
        )
        .route("/downloadclient/schema", get(shim::download_client_schema))
        .route("/downloadclient/test", post(shim::test_download_client))
        .route(
            "/downloadclient/{id}",
            put(shim::update_download_client).delete(shim::delete_download_client),
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
            get(shim::list_delay_profiles).post(shim::create_delay_profile),
        )
        .route(
            "/delayprofile/{id}",
            get(shim::get_delay_profile)
                .put(shim::update_delay_profile)
                .delete(shim::delete_delay_profile),
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
        .route("/config/mediamanagement", get(shim::get_media_management).put(shim::update_media_management))
        .route("/config/host", get(handlers::config_host))
        .route("/config/ui", get(handlers::config_ui))
        .route("/config/indexer", get(handlers::config_indexer))
        .route("/config/downloadclient", get(handlers::config_downloadclient))
        .route("/config/metadataprovider", get(handlers::config_metadataprovider))
        .route("/config/naming", get(handlers::config_naming))
        .route("/language", get(handlers::languages))
        .route("/indexerflag", get(handlers::indexer_flags))
        // --- the music library ------------------------------------------------
        .route("/artist", get(handlers::list_artists).post(handlers::add_artist))
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
        .route("/album", get(handlers::list_albums).post(handlers::add_album))
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
        .route("/qualityprofile/schema", get(handlers::quality_profile_schema))
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
        .route("/metadataprofile/schema", get(handlers::metadata_profile_schema))
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
        // --- commands ---------------------------------------------------------
        .route("/command", get(command::list).post(command::create))
        .route("/command/{id}", get(command::get).delete(command::cancel))
        .with_state(fs.clone())
        .layer(middleware::from_fn_with_state(state, require_api_key))
        .fallback(not_found)
        .layer(middleware::from_fn(version_header));
    routes
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
pub(crate) fn page(records: Vec<Value>, total: u64, page: u64, size: u64, key: &str, dir: &str) -> Value {
    json!({
        "page": page,
        "pageSize": size,
        "sortKey": key,
        "sortDirection": dir,
        "totalRecords": total,
        "records": records,
    })
}

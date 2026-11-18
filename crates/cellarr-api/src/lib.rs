//! cellarr-api — the HTTP API.
//!
//! Serves three surfaces from the one binary (docs/09-api.md):
//!
//! 1. the **native** versioned REST + SSE API under `/api/v1`
//!    ([`native`]/[`stream`]),
//! 2. the **`/api/v3` Radarr/Sonarr compatibility shim** ([`shim`]) — an
//!    external contract the ecosystem depends on, and
//! 3. the embedded **SRCL frontend** assets ([`assets`]).
//!
//! Dependencies are injected via [`AppState`]; reads go through `cellarr-db`
//! repositories and commands through the `cellarr-jobs` scheduler. Errors are
//! structured ([`ApiError`]) with a stable `code`. Live updates are pushed on
//! real domain transitions through the [`events::EventBus`], not a polling timer.

#![forbid(unsafe_code)]

pub mod assets;
pub mod auth;
pub mod backup;
pub mod calendar;
pub mod commands;
pub mod error;
pub mod events;
pub mod fs_health;
pub mod health;
pub mod import_list_sync;
pub mod lidarr;
pub mod logfile;
pub mod manual_import;
pub mod mediacover;
pub mod metadata;
pub mod native;
pub mod notifications;
pub mod openapi;
pub mod queue;
pub mod release_search;
pub mod shim;
pub mod state;
pub mod stream;
pub mod tags;
pub mod webauth;
pub mod webhook;

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

pub use auth::AuthConfig;
pub use backup::{BackupEngine, BackupError, BackupInfo, RestoreOutcome};
pub use error::{ApiError, ApiResult};
pub use events::{DomainEvent, EventBus};
pub use logfile::{LogFileInfo, LogFiles};
pub use metadata::{LookupCandidate, LookupOutcome, MetadataLookup, MetadataLookupError};
pub use notifications::default_senders;
pub use state::AppState;
pub use webhook::ReqwestWebhookSender;

/// Shadow mode's guard: anything but a read is refused, before it reaches a
/// handler. Logging in to the web UI is still allowed.
async fn read_only_guard(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::Method;
    use axum::response::IntoResponse;
    let read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let login = req.uri().path().ends_with("/login") || req.uri().path().ends_with("/logout");
    if state.read_only && !read && !login {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "message": "cellarr is running in shadow mode: read-only",
                "description": "Shadow mode serves reads only, so it can run beside a live install.",
            })),
        )
            .into_response();
    }
    next.run(req).await
}

/// Apply the shadow-mode guard to a router.
pub(crate) fn guarded(router: Router, state: &AppState) -> Router {
    router.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        read_only_guard,
    ))
}

/// Build the complete application router.
///
/// Surfaces:
/// - `/api/v1` — the native cellarr REST + SSE API;
/// - `/api/v3` — cellarr's own v3 face (app surface auto-selected per library);
/// - `/sonarr/api/v3` — the **Sonarr face** (TV resources, Sonarr v4 identity);
/// - `/radarr/api/v3` — the **Radarr face** (movie resources, Radarr v5 identity);
/// - everything else — the embedded SPA assets.
///
/// A real stack adds cellarr **twice**: as a Sonarr at `…/sonarr` and a Radarr
/// at `…/radarr`. The two faces share one handler core; only `appName`/version
/// and which media type's list resources are exposed differ.
///
/// Each v3 mount owns a 404-JSON fallback, so an unknown `/api/v3/*` (or
/// `/sonarr|radarr/api/v3/*`) path returns structured JSON — **not** the SPA
/// HTML (bug B1). Only genuinely non-API paths reach the asset fallback.
pub fn build_router(state: AppState) -> Router {
    let guard_state = state.clone();
    let app = Router::new()
        .nest("/api/v1", native::router(state.clone()))
        .nest("/lidarr/api/v1", lidarr::router(state.clone()))
        .nest("/api/v3", shim::router(state.clone(), shim::Face::Cellarr))
        .nest(
            "/sonarr/api/v3",
            shim::router(state.clone(), shim::Face::Sonarr),
        )
        .nest(
            "/radarr/api/v3",
            shim::router(state.clone(), shim::Face::Radarr),
        )
        // The iCal/ICS calendar feed (`sonarr.ics` / `radarr.ics`), authenticated
        // by the `apikey` query parameter calendar clients append to the URL. Built
        // as its own stateful sub-router and merged so it composes with the
        // already-stateless `nest`ed faces.
        .merge(
            Router::new()
                .route(
                    "/feed/v3/calendar/{file}",
                    axum::routing::get(calendar::calendar_feed),
                )
                .with_state(state.clone()),
        )
        // The web-UI login/logout endpoints. Merged OUTSIDE the v1 apikey
        // middleware (these are user/password auth, not apikey) and explicitly
        // exempted by the gate so an unauthenticated operator can log in.
        .merge(webauth::auth_routes(state.clone()))
        // A top-level liveness probe (distinct from the v3 `/api/v3/health`
        // diagnostic surface). Always reachable through the gate so a monitor can
        // poll a protected install. Stateless and cheap.
        .merge(
            Router::new()
                .route(
                    "/health",
                    get(|| async { axum::Json(serde_json::json!({ "status": "OK" })) }),
                )
                .with_state(state.clone()),
        )
        // Only non-API paths fall through to the embedded frontend; the v3
        // mounts return their own 404 JSON for unknown API paths.
        .fallback(assets::serve)
        // The web-UI authentication gate sits over the whole router: it enforces
        // the configured method (None/Forms/Basic) on the SPA + `/api/v1`, and
        // exempts `/api/v3`, the feed, `/login`/`/logout`, `/health`, and the
        // login page's static assets. `/api/v3` keeps its own apikey middleware
        // (the gate never touches it). Added before TraceLayer so traces wrap the
        // gate decision too.
        .layer(axum::middleware::from_fn_with_state(state, webauth::gate))
        .layer(TraceLayer::new_for_http());
    guarded(app, &guard_state)
}

/// Serve the API on an already-bound listener until the process is stopped.
///
/// Binding is the caller's responsibility (tests bind `127.0.0.1:0`; the daemon
/// binds its configured address) so this crate never assumes a fixed port.
///
/// # Errors
/// Returns any error from the underlying `axum::serve`.
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, build_router(state).into_make_service()).await
}

/// Serve the API until `shutdown` resolves, then stop accepting and let in-flight
/// requests finish (axum's graceful shutdown). The daemon drives this with its
/// signal future so a clean stop drains the server before the database is closed.
///
/// # Errors
/// Returns any error from the underlying `axum::serve`.
pub async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, build_router(state).into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
}

/// Serve the Lidarr face on its own listener — `/api/v1/*` and `/ping` at the
/// root, where Lidarr's clients look for them — until `shutdown` resolves.
///
/// # Errors
/// Returns any error from the underlying `axum::serve`.
pub async fn serve_lidarr_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let router = guarded(lidarr::listener_router(state.clone()), &state);
    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
}

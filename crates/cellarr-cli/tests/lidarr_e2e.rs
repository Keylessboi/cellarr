//! The whole daemon, driven the way a Lidarr client drives it.
//!
//! A real `Daemon` (real scheduler, real HTTP clients, real SQLite) is configured
//! and operated only through its Lidarr API: add a root folder, a qBittorrent
//! client and a Torznab indexer in Lidarr's own field names, add an artist, ask for
//! an album, and wait. Everything the daemon talks to is a local stub server on an
//! OS-assigned port: the metadata service, the indexer and qBittorrent. The stub
//! client "downloads" a folder of tagged FLAC files that the test generated.
//!
//! No Docker, no network, no fixed ports or paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Router};
use cellarr_cli::boot::Daemon;
use cellarr_cli::config::Config;
use serde_json::{json, Value};

const RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
const KID_A: &str = "e75c0549-ad55-39e3-8025-c72c5d4a3c5d";
const HASH: &str = "abcdef0123456789abcdef0123456789abcdef01";
const TITLES: [&str; 3] = [
    "Everything in Its Right Place",
    "Kid A",
    "The National Anthem",
];

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cellarr-meta/tests/fixtures");
    std::fs::read_to_string(p.join(name)).expect("fixture present")
}

/// What the stub qBittorrent has been asked to do, and where its "download" is.
#[derive(Clone)]
struct Stub {
    download_dir: PathBuf,
    added: Arc<Mutex<Vec<String>>>,
    deleted: Arc<Mutex<Vec<String>>>,
    searches: Arc<Mutex<Vec<String>>>,
}

fn json_body(s: String) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], s)
}

async fn meta_artist() -> impl IntoResponse {
    json_body(fixture("lm_artist.json"))
}
async fn meta_album() -> impl IntoResponse {
    json_body(fixture("lm_album.json"))
}
async fn meta_search(
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    match q.get("type").map(String::as_str) {
        Some("album") => json_body(fixture("lm_search_album.json")),
        _ => json_body(fixture("lm_search_artist.json")),
    }
}

const CAPS: &str = r#"<?xml version="1.0"?><caps><server title="stub"/><limits max="100" default="50"/>
<searching><search available="yes" supportedParams="q"/><audio-search available="yes" supportedParams="q,artist,album"/></searching>
<categories><category id="3000" name="Audio"><subcat id="3040" name="Audio/Lossless"/></category></categories></caps>"#;

async fn torznab(
    State(s): State<Stub>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let t = q.get("t").cloned().unwrap_or_default();
    if t == "caps" {
        return (
            [(header::CONTENT_TYPE, "application/xml")],
            CAPS.to_string(),
        );
    }
    s.searches.lock().unwrap().push(format!("{q:?}"));
    let rss = format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><title>stub</title>
<item><title>Radiohead - Kid A (2000) [FLAC]</title><guid isPermaLink="false">stub-kid-a</guid>
<link>magnet:?xt=urn:btih:{HASH}</link>
<enclosure url="magnet:?xt=urn:btih:{HASH}&amp;dn=Kid+A" length="300000000" type="application/x-bittorrent"/>
<torznab:attr name="category" value="3040"/><torznab:attr name="size" value="300000000"/>
<torznab:attr name="seeders" value="25"/><torznab:attr name="peers" value="30"/>
<torznab:attr name="infohash" value="{HASH}"/></item></channel></rss>"#
    );
    ([(header::CONTENT_TYPE, "application/xml")], rss)
}

async fn qb_login() -> impl IntoResponse {
    (
        [(header::SET_COOKIE, "SID=stubsession; HttpOnly; path=/")],
        "Ok.",
    )
}
async fn qb_add(State(s): State<Stub>, body: axum::body::Bytes) -> impl IntoResponse {
    s.added
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&body).to_string());
    "Ok."
}
async fn qb_info(State(s): State<Stub>) -> impl IntoResponse {
    let content = s.download_dir.join("Radiohead - Kid A (2000) [FLAC]");
    json_body(
        json!([{
            "hash": HASH, "name": "Radiohead - Kid A (2000) [FLAC]", "state": "pausedUP",
            "progress": 1.0, "ratio": 2.0, "seeding_time": 100000, "num_seeds": 5, "num_leechs": 0,
            "category": "lidarr", "content_path": content.to_string_lossy(),
            "save_path": s.download_dir.to_string_lossy(),
        }])
        .to_string(),
    )
}
async fn qb_delete(
    State(s): State<Stub>,
    Form(f): Form<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    s.deleted.lock().unwrap().push(format!("{f:?}"));
    "Ok."
}

async fn start_stub(download_dir: PathBuf) -> (String, Stub) {
    let stub = Stub {
        download_dir,
        added: Arc::default(),
        deleted: Arc::default(),
        searches: Arc::default(),
    };
    let app = Router::new()
        .route("/meta/search", get(meta_search))
        .route("/meta/artist/{id}", get(meta_artist))
        .route("/meta/album/{id}", get(meta_album))
        .route("/torznab/api", get(torznab))
        .route("/api/v2/auth/login", post(qb_login))
        .route("/api/v2/app/version", get(|| async { "v4.6.0" }))
        .route("/api/v2/torrents/add", post(qb_add))
        .route("/api/v2/torrents/info", get(qb_info))
        .route("/api/v2/torrents/delete", post(qb_delete))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (base, stub)
}

struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
    async fn get(&self, path: &str) -> Value {
        let r = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }
    async fn send(&self, method: reqwest::Method, path: &str, body: &Value) -> (StatusCode, Value) {
        let r = self
            .http
            .request(method, format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(r.status().as_u16()).unwrap();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn post_ok(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.send(reqwest::Method::POST, path, &body).await;
        assert!(s.is_success(), "POST {path} -> {s}: {v}");
        v
    }
}

async fn within<F, Fut>(secs: u64, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn eventually<F, Fut>(what: &str, secs: u64, f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    assert!(within(secs, f).await, "timed out waiting for: {what}");
}

fn write_download(dir: &Path, album_tag: &str) {
    let folder = dir.join("Radiohead - Kid A (2000) [FLAC]");
    std::fs::create_dir_all(&folder).unwrap();
    for (i, title) in TITLES.iter().enumerate() {
        let n = (i + 1).to_string();
        cellarr_fs::audio::testing::write_flac(
            &folder.join(format!("{n:0>2} - {title}.flac")),
            16,
            &[
                ("ARTIST", "Radiohead"),
                ("ALBUMARTIST", "Radiohead"),
                ("ALBUM", album_tag),
                ("TITLE", title),
                ("TRACKNUMBER", &n),
                ("DATE", "2000"),
            ],
        )
        .unwrap();
    }
}

/// A booted daemon, configured and asked for Kid A; the grab has reached the stub
/// client and the download check has been requested.
struct Fixture {
    api: Api,
    stub: Stub,
    library: PathBuf,
    album_id: i64,
    stop: tokio::sync::oneshot::Sender<()>,
    serve: tokio::task::JoinHandle<anyhow::Result<()>>,
    _tmp: tempfile::TempDir,
}

async fn fixture_for(album_tag: &str) -> Fixture {
    fixture_with(album_tag, false).await
}

async fn fixture_with(album_tag: &str, use_plugins: bool) -> Fixture {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let tmp = tempfile::tempdir().unwrap();
    let downloads = tmp.path().join("downloads");
    let library = tmp.path().join("music");
    std::fs::create_dir_all(&library).unwrap();
    write_download(&downloads, album_tag);
    let (stub_url, stub) = start_stub(downloads.clone()).await;

    let mut config = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    config.api.port = 0;
    config.music.lidarr_metadata_url = Some(format!("{stub_url}/meta"));
    // MusicBrainz is the fallback; point it at the stub too so nothing leaves the box.
    config.music.musicbrainz_url = Some(format!("{stub_url}/mb"));
    if use_plugins {
        let dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../cellarr-plugins/tests/fixtures");
        for f in ["torznab.wasm", "qbit.wasm"] {
            std::fs::copy(fixtures.join(f), dir.join(f)).unwrap();
        }
        config.plugins.dir = Some(dir);
    }
    let daemon = Daemon::boot(&config).await.expect("daemon boots");
    let addr = daemon.addr();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = tokio::spawn(async move {
        daemon
            .serve_until(async move {
                let _ = stop_rx.await;
            })
            .await
    });
    let api = Api {
        http: reqwest::Client::new(),
        base: format!("http://{addr}/lidarr/api/v1"),
    };
    eventually("the API to answer", 20, || async {
        api.http
            .get(format!("{}/system/status", api.base))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    })
    .await;

    // --- configure, in Lidarr's field names -------------------------------------
    let root = api
        .post_ok(
            "/rootfolder",
            json!({
                "name": "Music", "path": library.to_string_lossy(),
                "defaultMetadataProfileId": 1, "defaultQualityProfileId": 1,
                "defaultMonitorOption": "all", "defaultNewItemMonitorOption": "all", "defaultTags": [],
            }),
        )
        .await;
    assert!(root["id"].as_i64().is_some());

    let host_port = stub_url.trim_start_matches("http://");
    let (host, port) = host_port.split_once(':').unwrap();
    api.post_ok(
        "/downloadclient",
        json!({
            "name": "qbit",
            "implementation": if use_plugins { "QBittorrentPlugin" } else { "QBittorrent" },
            "configContract": "QBittorrentSettings",
            "protocol": "torrent", "enable": true, "priority": 1, "tags": [],
            "fields": [
                {"name": "host", "value": host}, {"name": "port", "value": port.parse::<u16>().unwrap()},
                {"name": "useSsl", "value": false}, {"name": "urlBase", "value": ""},
                {"name": "username", "value": "admin"}, {"name": "password", "value": "adminadmin"},
                {"name": "musicCategory", "value": "lidarr"},
            ],
        }),
    )
    .await;
    api.post_ok(
        "/indexer",
        json!({
            "name": "stub-torznab",
            "implementation": if use_plugins { "TorznabPlugin" } else { "Torznab" },
            "configContract": "TorznabSettings",
            "enableRss": true, "enableAutomaticSearch": true, "enableInteractiveSearch": true,
            "priority": 25, "protocol": "torrent", "tags": [],
            "fields": [
                {"name": "baseUrl", "value": format!("{stub_url}/torznab")}, {"name": "apiPath", "value": "/api"},
                {"name": "apiKey", "value": "k"}, {"name": "categories", "value": [3000, 3040]},
                {"name": "minimumSeeders", "value": 1},
            ],
        }),
    )
    .await;

    // --- add the artist with nothing monitored, then ask for one album -------------
    let quality = api.get("/qualityprofile").await[0]["id"].as_i64().unwrap();
    let artist = api
        .post_ok(
            "/artist",
            json!({
                "foreignArtistId": RADIOHEAD, "artistName": "Radiohead",
                "qualityProfileId": quality, "metadataProfileId": 1,
                "rootFolderPath": library.to_string_lossy(), "monitored": true,
                "monitorNewItems": "none",
                "addOptions": {"monitor": "none", "searchForMissingAlbums": false},
            }),
        )
        .await;
    let artist_id = artist["id"].as_i64().unwrap();
    assert!(artist["overview"]
        .as_str()
        .unwrap()
        .contains("English rock band"));
    let albums = api.get(&format!("/album?artistId={artist_id}")).await;
    let kid_a = albums
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["foreignAlbumId"] == KID_A)
        .expect("Kid A was added with the artist")
        .clone();
    assert_eq!(kid_a["monitored"], false);
    let album_id = kid_a["id"].as_i64().unwrap();

    let (s, _) = api
        .send(
            reqwest::Method::PUT,
            "/album/monitor",
            &json!({"albumIds": [album_id], "monitored": true}),
        )
        .await;
    assert!(s.is_success());
    api.post_ok(
        "/command",
        json!({"name": "AlbumSearch", "albumIds": [album_id]}),
    )
    .await;
    // A grab is finished by the download check (Lidarr runs it every minute); ask
    // for it now rather than wait out the clock, once the grab has happened.
    assert!(
        within(30, || async { !stub.added.lock().unwrap().is_empty() }).await,
        "the grab reached the download client"
    );
    api.post_ok("/command", json!({"name": "CheckForFinishedDownload"}))
        .await;
    Fixture {
        api,
        stub,
        library,
        album_id,
        stop,
        serve,
        _tmp: tmp,
    }
}

#[tokio::test]
async fn an_album_is_found_grabbed_downloaded_and_imported_through_the_lidarr_api() {
    let Fixture {
        api,
        stub,
        library,
        album_id,
        stop,
        serve,
        _tmp,
    } = fixture_for("Kid A").await;
    let imported = within(60, || async {
        let files = api.get(&format!("/trackfile?albumId={album_id}")).await;
        files.as_array().is_some_and(|f| f.len() == 3)
    })
    .await;
    if !imported {
        // Say where the chain stopped instead of just timing out.
        let history = api.get("/history?page=1&pageSize=50").await;
        let queue = api.get("/queue?page=1&pageSize=50").await;
        let commands = api.get("/command").await;
        panic!(
            "the album was not imported.\nindexer searches: {:?}\nclient adds: {:?}\nhistory: {history}\nqueue: {queue}\ncommands: {commands}\nlibrary: {:?}",
            stub.searches.lock().unwrap(),
            stub.added.lock().unwrap(),
            walk(&library),
        );
    }

    let searches = stub.searches.lock().unwrap().clone();
    assert!(
        searches.iter().any(|s| s.contains("album")),
        "the indexer was asked for the album: {searches:?}"
    );
    assert_eq!(stub.added.lock().unwrap().len(), 1, "exactly one grab");

    // On disk: three files under <library>/Radiohead/..., readable audio, none left staged.
    let mut found = Vec::new();
    for e in walk(&library) {
        if e.extension().is_some_and(|x| x == "flac") {
            found.push(e);
        }
    }
    assert_eq!(found.len(), 3, "files in the library: {found:?}");
    for f in &found {
        let rel = f
            .strip_prefix(&library)
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            rel.starts_with("Radiohead"),
            "placed under the artist: {rel}"
        );
        assert!(std::fs::metadata(f).unwrap().len() > 0);
    }

    // Over the API: the album is no longer missing and carries full statistics.
    let al = api.get(&format!("/album/{album_id}")).await;
    assert_eq!(al["statistics"]["trackFileCount"], 3);
    assert_eq!(al["statistics"]["percentOfTracks"], 100.0);
    let missing = api.get("/wanted/missing?page=1&pageSize=50").await;
    assert!(missing["records"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["id"] != album_id));
    let history = api
        .get(&format!("/history?page=1&pageSize=50&albumId={album_id}"))
        .await;
    let kinds: Vec<String> = history["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["eventType"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(kinds.iter().any(|k| k == "grabbed"), "{kinds:?}");
    assert!(
        kinds.iter().any(|k| k.contains("mport")),
        "an import event is recorded: {kinds:?}"
    );
    let queue = api.get("/queue?page=1&pageSize=50").await;
    assert_eq!(queue["totalRecords"], 0, "nothing left in the queue");

    stop.send(()).unwrap();
    serve.await.unwrap().expect("clean shutdown");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[allow(dead_code)]
fn _unused(_: HeaderMap) {}

#[tokio::test]
async fn a_download_tagged_as_another_album_is_never_filed_into_the_library() {
    let Fixture {
        api,
        stub,
        library,
        album_id,
        stop,
        serve,
        _tmp,
    } = fixture_for("A Completely Different Album").await;

    // Give the download check time to run and settle; then nothing may have been filed.
    let imported = within(25, || async {
        let files = api.get(&format!("/trackfile?albumId={album_id}")).await;
        files.as_array().is_some_and(|f| !f.is_empty())
    })
    .await;
    assert!(!imported, "a mis-tagged download was imported");
    assert!(
        walk(&library).is_empty(),
        "the library stays empty: {:?}",
        walk(&library)
    );
    // The download itself is untouched: still there for the user to look at.
    assert_eq!(walk(&stub.download_dir).len(), 3, "source files are intact");
    let al = api.get(&format!("/album/{album_id}")).await;
    assert_eq!(al["statistics"]["trackFileCount"], 0);

    stop.send(()).unwrap();
    serve.await.unwrap().expect("clean shutdown");
}

#[tokio::test]
async fn the_same_flow_works_with_a_plugin_indexer_and_a_plugin_download_client() {
    let Fixture {
        api,
        stub,
        library,
        album_id,
        stop,
        serve,
        _tmp,
    } = fixture_with("Kid A", true).await;

    // The plugins are listed, and offered in the schemas beside the built-ins.
    let plugins = api.get("/plugin").await;
    let names: Vec<&str> = plugins
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.contains(&"TorznabPlugin") && names.contains(&"QBittorrentPlugin"),
        "{names:?}"
    );
    let schema = api.get("/indexer/schema").await;
    assert!(schema
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["implementation"] == "TorznabPlugin"));
    let clients = api.get("/downloadclient").await;
    assert_eq!(clients[0]["implementation"], "QBittorrentPlugin");

    let imported = within(60, || async {
        let files = api.get(&format!("/trackfile?albumId={album_id}")).await;
        files.as_array().is_some_and(|f| f.len() == 3)
    })
    .await;
    assert!(
        imported,
        "not imported. searches: {:?} adds: {:?} queue: {}",
        stub.searches.lock().unwrap(),
        stub.added.lock().unwrap(),
        api.get("/queue?page=1&pageSize=50").await
    );
    assert_eq!(
        stub.added.lock().unwrap().len(),
        1,
        "the plugin client made the grab"
    );
    assert!(stub.added.lock().unwrap()[0].contains("category=lidarr"));
    assert_eq!(
        walk(&library)
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
            .count(),
        3
    );

    stop.send(()).unwrap();
    serve.await.unwrap().expect("clean shutdown");
}

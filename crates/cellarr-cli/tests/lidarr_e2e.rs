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
<pubDate>Mon, 16 Nov 2026 10:00:00 +0000</pubDate>
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
            "size": 300000000, "eta": 8640000,
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
        .route("/api/v2/app/webapiVersion", get(|| async { "2.9.3" }))
        .route(
            "/api/v2/app/preferences",
            get(|| async {
                json_body(
                    json!({"max_ratio_enabled": false, "max_seeding_time_enabled": false,
                    "queueing_enabled": false, "dht": true})
                    .to_string(),
                )
            }),
        )
        .route(
            "/api/v2/torrents/categories",
            get(|| async {
                json_body(json!({"lidarr": {"name": "lidarr", "savePath": ""}}).to_string())
            }),
        )
        .route(
            "/api/v2/torrents/properties",
            get(|| async {
                json_body(json!({"seeding_time": 100000, "save_path": "/"}).to_string())
            }),
        )
        .route(
            "/api/v2/torrents/files",
            get(|| async { json_body("[]".to_string()) }),
        )
        .fallback(|| async { "Ok." })
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

/// Kid A as its standard pressing lists it: (title, seconds, recording id).
/// Track listings are facts, recorded from the metadata service.
const KID_A_RELEASE: &str = "e1bdf797-8b89-4d4c-8986-bcb0974d9726";
const KID_A_TRACKS: [(&str, u64, &str); 10] = [
    (
        "Everything in Its Right Place",
        251,
        "60bd9d53-01ff-4562-8058-eb44b3940317",
    ),
    ("Kid A", 285, "0852f01a-1832-453d-ae8e-345499ecc027"),
    (
        "The National Anthem",
        352,
        "c7492f6e-adf3-4ff5-85dc-33d527dc81d8",
    ),
    (
        "How to Disappear Completely",
        356,
        "740d324e-4356-4116-812d-17ea76a190ce",
    ),
    ("Treefingers", 223, "31c89fdc-afdb-4d84-bc61-2dbabf0b4f23"),
    ("Optimistic", 316, "4d7b8fcb-fafc-4416-9170-752f12e10a2e"),
    ("In Limbo", 211, "27f5b46c-f749-40e7-bc10-1524393168f0"),
    ("Idioteque", 309, "1c0377bb-c00b-4bbe-b4b2-615f13324adc"),
    ("Morning Bell", 276, "5616efb3-4c50-40ab-a792-e2e34faae75f"),
    (
        "Motion Picture Soundtrack",
        419,
        "f8764ab0-996b-40c8-9645-10f47e8c43d0",
    ),
];

/// The album tag that asks for the whole pressing, MusicBrainz-tagged.
const FULL_KID_A: &str = "Kid A (complete)";

fn write_full_kid_a(folder: &Path) {
    std::fs::create_dir_all(folder).unwrap();
    for (i, (title, secs, recording)) in KID_A_TRACKS.iter().enumerate() {
        let n = (i + 1).to_string();
        cellarr_fs::audio::testing::write_flac_secs(
            &folder.join(format!("{n:0>2} - {title}.flac")),
            16,
            *secs,
            &[
                ("ARTIST", "Radiohead"),
                ("ALBUMARTIST", "Radiohead"),
                ("ALBUM", "Kid A"),
                ("TITLE", title),
                ("TRACKNUMBER", &n),
                ("TRACKTOTAL", "10"),
                ("DISCNUMBER", "1"),
                ("DATE", "2000"),
                ("MUSICBRAINZ_ARTISTID", RADIOHEAD),
                ("MUSICBRAINZ_ALBUMARTISTID", RADIOHEAD),
                ("MUSICBRAINZ_RELEASEGROUPID", KID_A),
                ("MUSICBRAINZ_ALBUMID", KID_A_RELEASE),
                ("MUSICBRAINZ_TRACKID", recording),
            ],
        )
        .unwrap();
    }
}

fn write_download(dir: &Path, album_tag: &str) {
    if album_tag == FULL_KID_A {
        write_full_kid_a(&dir.join("Radiohead - Kid A (2000) [FLAC]"));
        return;
    }
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
    _sidecar: Option<LidarrProcess>,
    sidecar_url: Option<String>,
    _tmp: tempfile::TempDir,
}

async fn fixture_for(album_tag: &str) -> Fixture {
    fixture_with(album_tag, Mode::Builtin).await
}

/// Which implementations the daemon is configured with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// cellarr's own Torznab and qBittorrent.
    Builtin,
    /// The example WASM plugins.
    Wasm,
    /// A real Lidarr as the native-plugin sidecar, using its Torznab and
    /// qBittorrent implementations (exposed as `*ViaLidarr`).
    Sidecar,
}

/// A real Lidarr process, killed when dropped.
struct LidarrProcess(std::process::Child);

impl Drop for LidarrProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Where the sidecar's console output goes: `$LIDARR_SIDECAR_LOG`, or nowhere.
fn sidecar_log(suffix: &str) -> std::process::Stdio {
    std::env::var("LIDARR_SIDECAR_LOG")
        .ok()
        .and_then(|p| std::fs::File::create(format!("{p}{suffix}")).ok())
        .map_or_else(std::process::Stdio::null, std::process::Stdio::from)
}

const SIDECAR_KEY: &str = "sidecarkey0123456789abcdef012345";

/// Start the Lidarr at `$LIDARR_SIDECAR_BIN` on a free port in `data`.
async fn start_lidarr(bin: &Path, data: &Path) -> (String, LidarrProcess) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::create_dir_all(data).unwrap();
    std::fs::write(
        data.join("config.xml"),
        format!(
            "<Config><Port>{port}</Port><BindAddress>127.0.0.1</BindAddress><ApiKey>{SIDECAR_KEY}</ApiKey>\
             <AuthenticationMethod>External</AuthenticationMethod><AuthenticationRequired>DisabledForLocalAddresses</AuthenticationRequired>\
             <LogLevel>info</LogLevel><UpdateMechanism>BuiltIn</UpdateMechanism></Config>"
        ),
    )
    .unwrap();
    let child = std::process::Command::new(bin.join("Lidarr"))
        .args(["-nobrowser", &format!("-data={}", data.display())])
        .current_dir(bin)
        .env("DOTNET_SYSTEM_GLOBALIZATION_INVARIANT", "1")
        .stdout(sidecar_log(""))
        .stderr(sidecar_log(".err"))
        .spawn()
        .expect("Lidarr starts");
    let url = format!("http://127.0.0.1:{port}");
    let proc = LidarrProcess(child);
    let http = reqwest::Client::new();
    eventually("the sidecar Lidarr to answer", 120, || async {
        http.get(format!("{url}/api/v1/system/status"))
            .header("X-Api-Key", SIDECAR_KEY)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    })
    .await;
    (url, proc)
}

async fn fixture_with(album_tag: &str, mode: Mode) -> Fixture {
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
    let mut sidecar = None;
    let mut sidecar_url = None;
    if mode == Mode::Sidecar {
        let bin = PathBuf::from(std::env::var("LIDARR_SIDECAR_BIN").expect("LIDARR_SIDECAR_BIN"));
        let (url, proc) = start_lidarr(&bin, &tmp.path().join("sidecar")).await;
        sidecar = Some(proc);
        sidecar_url = Some(url.clone());
        config.plugins.dotnet_sidecar_url = Some(url);
        config.plugins.dotnet_sidecar_key = Some(SIDECAR_KEY.into());
        config.plugins.dotnet_sidecar_root = Some(tmp.path().join("sidecar-root"));
        config.plugins.dotnet_sidecar_expose_builtins = true;
    }
    if mode == Mode::Wasm {
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

    if mode == Mode::Sidecar {
        eventually(
            "the sidecar's implementations to be discovered",
            90,
            || async {
                api.get("/plugin").await.as_array().is_some_and(|a| {
                    a.iter().any(|p| p["name"] == "TorznabViaLidarr")
                        && a.iter().any(|p| p["name"] == "QBittorrentViaLidarr")
                })
            },
        )
        .await;
    }

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
            "implementation": match mode {
                Mode::Builtin => "QBittorrent",
                Mode::Wasm => "QBittorrentPlugin",
                Mode::Sidecar => "QBittorrentViaLidarr",
            },
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
            "implementation": match mode {
                Mode::Builtin => "Torznab",
                Mode::Wasm => "TorznabPlugin",
                Mode::Sidecar => "TorznabViaLidarr",
            },
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
        within(if mode == Mode::Sidecar { 240 } else { 30 }, || async {
            !stub.added.lock().unwrap().is_empty()
        })
        .await,
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
        _sidecar: sidecar,
        sidecar_url,
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
        ..
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
    // A torrent stays in the client after import, still seeding, as Lidarr
    // leaves it; removing it is the client's or the operator's call.
    assert!(
        stub.deleted.lock().unwrap().is_empty(),
        "the torrent was removed from qBittorrent after import: {:?}",
        stub.deleted.lock().unwrap()
    );

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
        ..
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
        ..
    } = fixture_with("Kid A", Mode::Wasm).await;

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

#[tokio::test]
async fn the_same_flow_works_through_a_real_lidarr_sidecar() {
    if std::env::var("LIDARR_SIDECAR_BIN").is_err() {
        eprintln!("skipped: set LIDARR_SIDECAR_BIN to a Lidarr install to run the sidecar test");
        return;
    }
    let Fixture {
        api,
        stub,
        library,
        album_id,
        stop,
        serve,
        // Bound, not `..`: an unbound field is dropped here, which would stop
        // the sidecar and delete the temp dir mid-test.
        _sidecar,
        _tmp,
        ..
    } = fixture_with("Kid A", Mode::Sidecar).await;

    // The search went through the sidecar's Torznab to the stub, and the grab
    // through the sidecar's qBittorrent.
    let imported = within(120, || async {
        let _ = api
            .send(
                reqwest::Method::POST,
                "/command",
                &json!({"name": "CheckForFinishedDownload"}),
            )
            .await;
        let files = api.get(&format!("/trackfile?albumId={album_id}")).await;
        files.as_array().is_some_and(|f| f.len() == 3)
    })
    .await;
    if !imported {
        let sidecar_queue = match &_sidecar {
            Some(_) => {
                let url = std::env::var("LIDARR_SIDECAR_URL_SEEN").unwrap_or_default();
                url
            }
            None => String::new(),
        };
        panic!(
            "not imported. adds: {:?}\ncellarr queue: {}\n{sidecar_queue}",
            stub.added.lock().unwrap(),
            api.get("/queue?page=1&pageSize=50").await
        );
    }
    assert!(
        !stub.searches.lock().unwrap().is_empty(),
        "the sidecar searched the indexer"
    );
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

#[tokio::test]
async fn a_complete_album_is_staged_by_the_sidecar_and_imported_from_there() {
    if std::env::var("LIDARR_SIDECAR_BIN").is_err() {
        eprintln!("skipped: set LIDARR_SIDECAR_BIN to a Lidarr install to run the sidecar test");
        return;
    }
    let Fixture {
        api,
        library,
        album_id,
        stop,
        serve,
        _sidecar,
        sidecar_url,
        _tmp,
        ..
    } = fixture_with(FULL_KID_A, Mode::Sidecar).await;
    let sidecar_url = sidecar_url.unwrap();
    let http = reqwest::Client::new();

    let imported = within(240, || async {
        let _ = api
            .send(
                reqwest::Method::POST,
                "/command",
                &json!({"name": "CheckForFinishedDownload"}),
            )
            .await;
        let files = api.get(&format!("/trackfile?albumId={album_id}")).await;
        files.as_array().is_some_and(|f| !f.is_empty())
    })
    .await;
    let history: Value = http
        .get(format!("{sidecar_url}/api/v1/history?page=1&pageSize=100"))
        .header("X-Api-Key", SIDECAR_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events: Vec<String> = history["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["eventType"].as_str().map(str::to_string))
        .collect();
    assert!(imported, "not imported; sidecar history: {events:?}");
    assert!(
        events.iter().any(|e| e == "downloadImported"),
        "the sidecar staged the download: {events:?}"
    );
    let in_library: Vec<PathBuf> = walk(&library)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        .collect();
    assert!(!in_library.is_empty(), "files reached the library");
    for f in &in_library {
        assert!(
            f.starts_with(&library),
            "only the library holds cellarr's import: {f:?}"
        );
    }

    stop.send(()).unwrap();
    serve.await.unwrap().expect("clean shutdown");
}

#[tokio::test]
async fn manual_import_lists_a_download_and_imports_a_folder_as_the_maintenance_script_does() {
    let Fixture {
        api,
        library,
        album_id,
        stop,
        serve,
        _tmp,
        ..
    } = fixture_for("A Completely Different Album").await;

    // The grabbed download is tagged as another album: listed, but refused.
    let listed = api
        .get(&format!(
            "/manualimport?downloadId={}&filterExistingFiles=true",
            HASH.to_uppercase()
        ))
        .await;
    let items = listed.as_array().unwrap();
    assert_eq!(items.len(), 3, "{listed}");
    assert!(
        items
            .iter()
            .all(|i| !i["rejections"].as_array().unwrap().is_empty()),
        "a mis-tagged download is not offered for import: {listed}"
    );

    // A folder of the right album, scanned and imported exactly as the script does.
    let folder = _tmp.path().join("by-hand");
    write_download(&folder, "Kid A");
    let folder = folder.join("Radiohead - Kid A (2000) [FLAC]");
    let scanned = api
        .get(&format!(
            "/manualimport?folder={}&filterExistingFiles=true",
            urlencode(&folder.to_string_lossy())
        ))
        .await;
    let files: Vec<Value> = scanned
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| {
            let artist = item["artist"]["id"].as_i64()?;
            let album = item["album"]["id"].as_i64()?;
            let release = item["albumReleaseId"].as_i64().filter(|r| *r > 0)?;
            let tracks: Vec<i64> = item["tracks"]
                .as_array()?
                .iter()
                .filter_map(|t| t["id"].as_i64())
                .collect();
            (!tracks.is_empty()).then(|| {
                json!({
                    "path": item["path"], "artistId": artist, "albumId": album,
                    "albumReleaseId": release, "trackIds": tracks,
                    "quality": item["quality"], "indexerFlags": 0,
                    "downloadId": item["downloadId"], "disableReleaseSwitching": false,
                })
            })
        })
        .collect();
    assert_eq!(files.len(), 3, "every file maps to a track: {scanned}");
    assert!(files.iter().all(|f| f["albumId"] == album_id));

    let cmd = api
        .post_ok(
            "/command",
            json!({"name": "ManualImport", "files": files, "importMode": "auto", "replaceExistingFiles": false}),
        )
        .await;
    let cid = cmd["id"].as_i64().unwrap();
    let done = within(60, || async {
        let st = api.get(&format!("/command/{cid}")).await;
        st["status"] == "completed" || st["status"] == "failed"
    })
    .await;
    let st = api.get(&format!("/command/{cid}")).await;
    assert!(
        done && st["status"] == "completed",
        "ManualImport ended {st}"
    );
    let files = api.get(&format!("/trackfile?albumId={album_id}")).await;
    assert_eq!(files.as_array().unwrap().len(), 3, "{files}");
    assert_eq!(
        walk(&library)
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
            .count(),
        3
    );
    // The source folder is untouched (imports copy or hardlink).
    assert_eq!(walk(&folder).len(), 3);

    stop.send(()).unwrap();
    serve.await.unwrap().expect("clean shutdown");
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A field's type as the production clients parse it. Octo (in particular)
/// treats a value of the wrong kind as an error, so each must be exactly right.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// A 32-bit integer (Octo's `Int32`).
    Int,
    /// A string.
    Str,
    /// A boolean.
    Bool,
    /// An RFC 3339 date string.
    Date,
    /// An object.
    Obj,
    /// An array.
    Arr,
}

/// Check `field` of `v` (dotted for nesting) is absent/null-tolerant or of `kind`;
/// `required` fields must be present and non-null.
fn check(
    problems: &mut Vec<String>,
    what: &str,
    v: &Value,
    field: &str,
    kind: Kind,
    required: bool,
) {
    let mut cur = v;
    for part in field.split('.') {
        cur = &cur[part];
    }
    let ok = match (cur, kind) {
        (Value::Null, _) => !required,
        (Value::Number(n), Kind::Int) => n.as_i64().is_some_and(|i| i32::try_from(i).is_ok()),
        (Value::String(_), Kind::Str) => true,
        (Value::String(s), Kind::Date) => {
            s.len() >= 19 && s.as_bytes()[4] == b'-' && s.contains('T')
        }
        (Value::Bool(_), Kind::Bool) => true,
        (Value::Object(_), Kind::Obj) => true,
        (Value::Array(_), Kind::Arr) => true,
        _ => false,
    };
    if !ok {
        problems.push(format!(
            "{what}: {field} should be {kind:?}{}, is {cur}",
            if required { " (required)" } else { "" }
        ));
    }
}

#[tokio::test]
async fn every_field_the_production_clients_read_has_the_type_they_parse() {
    use Kind::*;
    let Fixture {
        api,
        album_id,
        stop,
        serve,
        _tmp,
        ..
    } = fixture_for("Kid A").await;
    assert!(
        within(60, || async {
            api.get(&format!("/trackfile?albumId={album_id}"))
                .await
                .as_array()
                .is_some_and(|f| f.len() == 3)
        })
        .await,
        "the album imports"
    );
    let mut p = Vec::new();

    // Octo: get_options, system status.
    for r in api.get("/rootfolder").await.as_array().unwrap() {
        check(&mut p, "rootfolder", r, "id", Int, true);
        check(&mut p, "rootfolder", r, "path", Str, true);
    }
    for path in ["/qualityprofile", "/metadataprofile"] {
        for r in api.get(path).await.as_array().unwrap() {
            check(&mut p, path, r, "id", Int, true);
            check(&mut p, path, r, "name", Str, true);
        }
    }
    let status = api.get("/system/status").await;
    check(&mut p, "system/status", &status, "version", Str, true);
    for path in [
        "/config/downloadclient",
        "/config/metadataprovider",
        "/config/mediamanagement",
    ] {
        let v = api.get(path).await;
        check(&mut p, path, &v, "id", Int, true);
    }

    // Octo + on-heart: artists.
    let artists = api.get("/artist").await;
    for a in artists.as_array().unwrap() {
        check(&mut p, "artist", a, "id", Int, true);
        check(&mut p, "artist", a, "artistName", Str, true);
        check(&mut p, "artist", a, "foreignArtistId", Str, true);
        check(&mut p, "artist", a, "path", Str, false);
        check(&mut p, "artist", a, "monitored", Bool, true);
        check(&mut p, "artist", a, "monitorNewItems", Str, false);
        check(&mut p, "artist", a, "added", Date, false);
        check(&mut p, "artist", a, "statistics.trackFileCount", Int, false);
    }
    let lookup = api.get("/artist/lookup?term=radiohead").await;
    for a in lookup.as_array().unwrap() {
        check(&mut p, "artist/lookup", a, "foreignArtistId", Str, true);
        check(&mut p, "artist/lookup", a, "folder", Str, false);
    }

    // Octo: albums, with pressings and pins.
    let album = api.get(&format!("/album/{album_id}")).await;
    for (f, k, req) in [
        ("id", Int, true),
        ("foreignAlbumId", Str, true),
        ("title", Str, true),
        ("monitored", Bool, true),
        ("anyReleaseOk", Bool, true),
        ("artistId", Int, true),
        ("releases", Arr, true),
        ("releaseDate", Date, false),
        ("artist.artistName", Str, true),
        ("statistics.trackCount", Int, true),
        ("statistics.trackFileCount", Int, true),
    ] {
        check(&mut p, "album", &album, f, k, req);
    }
    for r in album["releases"].as_array().unwrap() {
        check(&mut p, "album.releases", r, "id", Int, true);
        check(&mut p, "album.releases", r, "foreignReleaseId", Str, true);
        check(&mut p, "album.releases", r, "title", Str, true);
        check(&mut p, "album.releases", r, "monitored", Bool, true);
    }
    for path in [
        format!("/album?artistId={}", album["artistId"]),
        format!("/album?foreignAlbumId={KID_A}"),
    ] {
        assert!(
            !api.get(&path).await.as_array().unwrap().is_empty(),
            "{path}"
        );
    }

    // Octo: tracks and track files (asked for as `trackFile`, too).
    for t in api
        .get(&format!("/track?albumId={album_id}"))
        .await
        .as_array()
        .unwrap()
    {
        check(&mut p, "track", t, "id", Int, true);
        check(&mut p, "track", t, "trackFileId", Int, false);
        check(&mut p, "track", t, "trackNumber", Str, true);
        check(&mut p, "track", t, "title", Str, true);
    }
    for path in [
        format!("/trackFile?albumId={album_id}"),
        format!("/trackfile?artistId={}", album["artistId"]),
    ] {
        let files = api.get(&path).await;
        assert!(!files.as_array().unwrap().is_empty(), "{path}");
        for f in files.as_array().unwrap() {
            check(&mut p, &path, f, "id", Int, true);
            check(&mut p, &path, f, "artistId", Int, true);
            check(&mut p, &path, f, "albumId", Int, true);
            check(&mut p, &path, f, "path", Str, true);
            check(&mut p, &path, f, "size", Int, true);
            check(&mut p, &path, f, "quality.quality.name", Str, true);
        }
    }

    // Octo + the maintenance script: history, by album and by artist and event.
    let page = api
        .get(&format!(
            "/history?albumId={album_id}&eventType=1&pageSize=1"
        ))
        .await;
    check(&mut p, "history", &page, "totalRecords", Int, true);
    check(&mut p, "history", &page, "records", Arr, true);
    assert!(
        !page["records"].as_array().unwrap().is_empty(),
        "eventType=1 finds the grab: {page}"
    );
    for r in page["records"].as_array().unwrap() {
        for (f, k, req) in [
            ("id", Int, true),
            ("eventType", Str, true),
            ("albumId", Int, true),
            ("artistId", Int, true),
            ("trackId", Int, false),
            ("downloadId", Str, false),
            ("sourceTitle", Str, true),
            ("date", Date, true),
            ("data", Obj, false),
        ] {
            check(&mut p, "history", r, f, k, req);
        }
    }
    assert!(
        api.get(&format!(
            "/history?albumId={album_id}&eventType=8&pageSize=1"
        ))
        .await["records"]
            .as_array()
            .is_some_and(|r| !r.is_empty()),
        "eventType=8 (downloadImported) is recorded"
    );
    let by_artist = api
        .get(&format!("/history/artist?artistId={}", album["artistId"]))
        .await;
    assert!(by_artist.as_array().is_some(), "history/artist is an array");

    // Octo: queue details; the maintenance script pages the queue.
    for q in api.get("/queue/details").await.as_array().unwrap() {
        for (f, k, req) in [
            ("id", Int, true),
            ("artistId", Int, false),
            ("albumId", Int, false),
            ("downloadId", Str, false),
            ("title", Str, true),
            ("status", Str, false),
            ("trackedDownloadState", Str, false),
            ("trackedDownloadStatus", Str, false),
            ("size", Int, false),
            ("sizeleft", Int, false),
            ("outputPath", Str, false),
            ("protocol", Str, false),
            ("added", Date, false),
        ] {
            check(&mut p, "queue/details", q, f, k, req);
        }
    }
    let queue = api.get("/queue?page=1&pageSize=50&includeUnknownArtistItems=true&includeArtist=true&includeAlbum=true").await;
    check(&mut p, "queue", &queue, "totalRecords", Int, true);

    // Commands, as Octo and the cronjobs poll them.
    let cmd = api
        .post_ok(
            "/command",
            json!({"name": "RefreshAlbum", "albumId": album_id, "albumIds": [album_id]}),
        )
        .await;
    for (f, k, req) in [
        ("id", Int, true),
        ("name", Str, true),
        ("status", Str, true),
        ("queued", Date, true),
        ("started", Date, false),
        ("ended", Date, false),
        ("trigger", Str, false),
        ("body", Obj, true),
    ] {
        check(&mut p, "command", &cmd, f, k, req);
    }
    let sniper = api
        .post_ok("/command", json!({"name": "SearchSniper"}))
        .await;
    assert!(
        sniper["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Collected ")),
        "SearchSniper reports like Tubifarry's: {sniper}"
    );

    // Octo: health checks.
    for h in api.get("/health").await.as_array().unwrap() {
        check(&mut p, "health", h, "source", Str, true);
        check(&mut p, "health", h, "type", Str, true);
        check(&mut p, "health", h, "message", Str, true);
        check(&mut p, "health", h, "wikiUrl", Str, false);
    }

    // Octo: interactive search results, read field by field.
    for r in api
        .get(&format!("/release?albumId={album_id}"))
        .await
        .as_array()
        .unwrap()
    {
        for (f, k, req) in [
            ("guid", Str, true),
            ("indexerId", Int, true),
            ("title", Str, true),
            ("indexer", Str, false),
            ("quality.quality.name", Str, false),
            ("size", Int, false),
            ("seeders", Int, false),
            ("leechers", Int, false),
            ("protocol", Str, true),
            ("rejected", Bool, false),
        ] {
            check(&mut p, "release", r, f, k, req);
        }
    }

    // The mass search and the maintenance script: wanted, paged.
    let wanted = api
        .get("/wanted/missing?page=1&pageSize=1000&sortKey=id&sortDirection=ascending")
        .await;
    check(&mut p, "wanted/missing", &wanted, "totalRecords", Int, true);
    check(&mut p, "wanted/missing", &wanted, "records", Arr, true);

    // Plain liveness, both spellings the cluster uses.
    assert!(api
        .http
        .get(format!("{}/ping", api.base))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    assert!(
        p.is_empty(),
        "{} field(s) would break a production client:\n  {}",
        p.len(),
        p.join("\n  ")
    );
    stop.send(()).unwrap();
    serve.await.unwrap().expect("clean shutdown");
}

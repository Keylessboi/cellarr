//! The Lidarr-face test harness: a daemon-shaped server over a temp database and a recorded
//! MusicBrainz, plus the calls a real client makes.

#![allow(dead_code)]

use cellarr_api::{AppState, AuthConfig};
use std::sync::Arc;

use cellarr_db::Database;
use cellarr_jobs::music_sync::MusicSync;
use cellarr_meta::{MusicBrainzSource, RecordedFetcher};
use serde_json::{json, Value};

pub const FIX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../cellarr-meta/tests/fixtures"
);
pub const BASE: &str = "https://mb.test/ws/2";
pub const RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
pub const OK_COMPUTER: &str = "b1392450-e666-3926-a536-22c65f834433";
pub const KEY: &str = "lidarr-test-key";

pub struct Lidarr {
    pub base: String,
    pub client: reqwest::Client,
    pub state: AppState,
    _dir: tempfile::TempDir,
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIX}/{name}")).unwrap()
}

pub async fn start(auth: AuthConfig) -> Lidarr {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let fetcher = RecordedFetcher::new()
        .with_body(
            &format!("{BASE}/artist?query="),
            fixture("mb_artist_search_radiohead.json"),
        )
        .with_body(
            &format!("{BASE}/artist/{RADIOHEAD}"),
            fixture("mb_artist_radiohead.json"),
        )
        .with_body(
            &format!("{BASE}/release-group?artist="),
            fixture("mb_release_groups_radiohead.json"),
        )
        .with_body(
            &format!("{BASE}/release-group?query="),
            fixture("mb_release_group_search_ok_computer.json"),
        )
        .with_body(
            &format!("{BASE}/release?release-group="),
            fixture("mb_releases_ok_computer.json"),
        );
    let catalogue = Arc::new(MusicSync::new(
        db.clone(),
        MusicBrainzSource::with_base_url(fetcher, BASE),
    ));
    let state = AppState::new(db, auth).with_music(catalogue);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = cellarr_api::lidarr::listener_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Lidarr {
        base,
        client: reqwest::Client::new(),
        state,
        _dir: dir,
    }
}

impl Lidarr {
    pub fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}/api/v1{path}", self.base))
            .header("X-Api-Key", KEY)
    }

    pub async fn get(&self, path: &str) -> Value {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &Value,
    ) -> reqwest::Response {
        self.req(method, path).json(body).send().await.unwrap()
    }

    /// A quality profile that allows FLAC and MP3-320, as a client would create it.
    pub async fn flac_profile(&self) -> i64 {
        let schema = self.get("/qualityprofile/schema").await;
        let mut items = schema["items"].as_array().unwrap().clone();
        for item in &mut items {
            let name = item["quality"]["name"].as_str().unwrap();
            item["allowed"] = json!(name == "FLAC" || name == "MP3-320");
        }
        let r = self
            .send(
                reqwest::Method::POST,
                "/qualityprofile",
                &json!({ "name": "Lossless", "upgradeAllowed": true, "cutoff": 6, "items": items }),
            )
            .await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        r.json::<Value>().await.unwrap()["id"].as_i64().unwrap()
    }

    /// Add Radiohead the way Octo's heart job does: look it up, then post the
    /// lookup payload back with the profiles and root folder filled in.
    pub async fn add_radiohead(&self, profile: i64) -> Value {
        let hits = self.get("/artist/lookup?term=radiohead").await;
        let mut payload = hits[0].clone();
        payload["qualityProfileId"] = json!(profile);
        payload["metadataProfileId"] = json!(1);
        payload["rootFolderPath"] = json!("/data/media/music");
        payload["monitored"] = json!(true);
        payload["addOptions"] =
            json!({ "monitor": "all", "monitored": true, "searchForMissingAlbums": false });
        let r = self.send(reqwest::Method::POST, "/artist", &payload).await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        r.json().await.unwrap()
    }
}

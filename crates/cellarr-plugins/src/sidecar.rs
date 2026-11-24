//! Native Lidarr plugins, through a Lidarr sidecar.
//!
//! Lidarr's plugins are .NET assemblies built against Lidarr's own internals, so
//! the only faithful host for them is Lidarr. The sidecar is therefore a stock
//! Lidarr, run headless beside cellarr with the user's plugins installed in its
//! `plugins/` folder. cellarr never links Lidarr code: it talks to the sidecar
//! over Lidarr's public HTTP API, as any client would.
//!
//! What the bridge does:
//! - **discovers** the implementations the sidecar's indexer and download-client
//!   schemas offer beyond Lidarr's built-ins (those come from plugins) and
//!   publishes them in the plugin catalog, fields and all;
//! - **mirrors** each cellarr indexer or client configured with one of those
//!   implementations into the sidecar (named `cellarr:<id>`), keeping its settings
//!   in step;
//! - **searches** by mirroring the album (artist added unmonitored, nothing
//!   searched or downloaded on its own) and asking the sidecar for that album's
//!   releases, keeping the ones the mirrored indexer returned;
//! - **downloads** by handing the release to the sidecar (a grab of a release it
//!   found, or a push of one cellarr found), then reading the sidecar's queue for
//!   progress and the finished path.
//!
//! The sidecar imports a finished download into its own root folder, which is a
//! private staging area, never the library. cellarr then imports from there with
//! its own matching and safety rules, and removing the download deletes the
//! staged copy. (Lidarr only tracks a finished download while it is importing
//! it, so letting it stage the files is what makes completion observable for any
//! plugin client, torrent, Usenet or streaming.) A download the sidecar cannot
//! import is handed to cellarr straight from the download folder instead.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use cellarr_core::ids::IndexerId;
use cellarr_core::release::{Protocol, Release};
use cellarr_core::traits::{DownloadClient, DownloadState, DownloadStatus, Indexer, SearchTerms};
use cellarr_core::GrabRequest;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::catalog::{Entry, EntryField, EntryKind};
use crate::error::{PluginError, Result};

/// Lidarr's own indexer implementations (read off a stock install). Anything
/// else a sidecar offers came from a plugin.
pub const BUILTIN_INDEXERS: &[&str] = &[
    "FileList",
    "Gazelle",
    "Headphones",
    "IPTorrents",
    "Newznab",
    "Nyaa",
    "Redacted",
    "TorrentRssIndexer",
    "Torrentleech",
    "Torznab",
];

/// Lidarr's own download-client implementations.
pub const BUILTIN_CLIENTS: &[&str] = &[
    "Aria2",
    "Deluge",
    "Flood",
    "Hadouken",
    "NzbVortex",
    "Nzbget",
    "Pneumatic",
    "QBittorrent",
    "RTorrent",
    "Sabnzbd",
    "TorrentBlackhole",
    "TorrentDownloadStation",
    "TorrentFreeboxDownload",
    "Transmission",
    "UTorrent",
    "UsenetBlackhole",
    "UsenetDownloadStation",
    "Vuze",
];

/// The suffix a built-in sidecar implementation carries in cellarr's catalog
/// when built-ins are exposed (so it cannot collide with cellarr's own).
pub const VIA_SUFFIX: &str = "ViaLidarr";

/// The scheme of a release link that only the sidecar can fetch: `lidarr-sidecar:<indexerId>:<guid>`.
pub const SIDECAR_SCHEME: &str = "lidarr-sidecar:";

/// How to reach the sidecar.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Its base URL (`http://127.0.0.1:8687`).
    pub url: String,
    /// Its API key.
    pub api_key: String,
    /// A folder the sidecar can use as its root folder. It never writes music
    /// there (it does not import), but Lidarr needs one to add an artist.
    pub root_folder: String,
    /// Also expose Lidarr's built-in implementations (named `<Impl>ViaLidarr`).
    pub expose_builtins: bool,
}

/// The bridge.
pub struct Sidecar {
    config: SidecarConfig,
    http: reqwest::Client,
    setup: Mutex<bool>,
    schemas: RwLock<BTreeMap<String, (EntryKind, Value)>>,
    /// Releases the sidecar found recently: guid -> sidecar indexer id, so a
    /// grab of one is handed back by id (the sidecar keeps them cached).
    found: std::sync::Mutex<BTreeMap<String, Found>>,
    /// Mirrors asked for so far (mirror name -> catalog name and settings), so a
    /// grab can fall back to the mirror whose protocol matches the release.
    mirrors: std::sync::Mutex<BTreeMap<String, (String, Value)>>,
    retry_pause: Duration,
    album_wait_polls: u32,
    install_polls: u32,
}

/// What the sidecar told us about a release it found.
#[derive(Debug, Clone)]
struct Found {
    indexer: i64,
    /// The release's protocol as the sidecar names it (`SoulseekDownloadProtocol`).
    protocol: String,
    album: i64,
}

/// What an `InstallPlugin` request came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The sidecar lists the plugin and its implementations are published.
    Loaded,
    /// The sidecar accepted the install but does not list the plugin yet.
    RestartRequired,
}

fn err(e: impl std::fmt::Display) -> PluginError {
    PluginError::Host(format!("lidarr sidecar: {e}"))
}

impl Sidecar {
    /// A bridge to the sidecar at `config.url`.
    #[must_use]
    pub fn new(config: SidecarConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            config,
            http,
            setup: Mutex::new(false),
            schemas: RwLock::new(BTreeMap::new()),
            found: std::sync::Mutex::new(BTreeMap::new()),
            mirrors: std::sync::Mutex::new(BTreeMap::new()),
            retry_pause: Duration::from_secs(2),
            album_wait_polls: 60,
            install_polls: 30,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.config.url.trim_end_matches('/'))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut rb = self
            .http
            .request(method.clone(), self.url(path))
            .header("X-Api-Key", &self.config.api_key);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().await.map_err(err)?;
        let status = resp.status();
        let text = resp.text().await.map_err(err)?;
        if !status.is_success() {
            // A validation failure is an array of findings; say what they are.
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|f| {
                                Some(format!(
                                    "{}: {}",
                                    f["propertyName"].as_str()?,
                                    f["errorMessage"].as_str()?
                                ))
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                })
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| text.split_whitespace().collect::<Vec<_>>().join(" "));
            return Err(err(format!(
                "{method} {path} answered {status}: {}",
                detail.chars().take(500).collect::<String>()
            )));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }

    /// The catalog name for a sidecar implementation, or `None` when it is a
    /// built-in that is not exposed.
    fn catalog_name(&self, kind: EntryKind, implementation: &str) -> Option<String> {
        let builtin = match kind {
            EntryKind::Indexer => BUILTIN_INDEXERS,
            EntryKind::DownloadClient => BUILTIN_CLIENTS,
        }
        .iter()
        .any(|b| b.eq_ignore_ascii_case(implementation));
        match (builtin, self.config.expose_builtins) {
            (false, _) => Some(implementation.to_string()),
            (true, true) => Some(format!("{implementation}{VIA_SUFFIX}")),
            (true, false) => None,
        }
    }

    /// Ask the sidecar what it offers and publish the plugin implementations.
    ///
    /// # Errors
    /// When the sidecar cannot be reached.
    pub async fn discover(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut schemas = BTreeMap::new();
        let plugins = self.get("/system/plugins").await.unwrap_or(Value::Null);
        let version = |name: &str| -> String {
            plugins
                .as_array()
                .and_then(|a| {
                    a.iter().find(|p| {
                        p["name"]
                            .as_str()
                            .is_some_and(|n| name.to_lowercase().contains(&n.to_lowercase()))
                    })
                })
                .and_then(|p| p["installedVersion"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        for (kind, path) in [
            (EntryKind::Indexer, "/indexer/schema"),
            (EntryKind::DownloadClient, "/downloadclient/schema"),
        ] {
            let list = self.get(path).await?;
            for item in list.as_array().cloned().unwrap_or_default() {
                let Some(implementation) = item["implementation"].as_str() else {
                    continue;
                };
                let Some(name) = self.catalog_name(kind, implementation) else {
                    continue;
                };
                entries.push(Entry {
                    name: name.clone(),
                    version: version(implementation),
                    description: format!(
                        "{} (native Lidarr {}, hosted by the sidecar)",
                        item["implementationName"]
                            .as_str()
                            .unwrap_or(implementation),
                        if kind == EntryKind::Indexer {
                            "indexer"
                        } else {
                            "download client"
                        },
                    ),
                    kind,
                    protocol: item["protocol"].as_str().unwrap_or("torrent").to_string(),
                    fields: fields_of(&item),
                    runtime: "dotnet".into(),
                });
                schemas.insert(name.to_ascii_lowercase(), (kind, item));
            }
        }
        if let Ok(mut g) = self.schemas.write() {
            *g = schemas;
        }
        Ok(entries)
    }

    /// Discover and publish the sidecar's implementations to the catalog.
    ///
    /// # Errors
    /// When the sidecar cannot be reached.
    pub async fn refresh_catalog(&self) -> Result<usize> {
        let entries = self.discover().await?;
        let n = entries.len();
        crate::catalog::set_runtime("dotnet", entries);
        Ok(n)
    }

    fn schema(&self, name: &str) -> Option<(EntryKind, Value)> {
        self.schemas
            .read()
            .ok()
            .and_then(|g| g.get(&name.to_ascii_lowercase()).cloned())
    }

    /// One-time setup: the sidecar stages finished downloads (completed-download
    /// handling on) into its own root folder.
    async fn ensure_setup(&self) -> Result<()> {
        let mut done = self.setup.lock().await;
        if *done {
            return Ok(());
        }
        let mut dc = self.get("/config/downloadclient").await?;
        if dc["enableCompletedDownloadHandling"] != json!(true) {
            dc["enableCompletedDownloadHandling"] = json!(true);
            let id = dc["id"].as_i64().unwrap_or(1);
            self.call(
                reqwest::Method::PUT,
                &format!("/config/downloadclient/{id}"),
                Some(&dc),
            )
            .await?;
        }
        self.allow_all_primary_types().await;
        let roots = self.get("/rootfolder").await?;
        let have = roots.as_array().is_some_and(|a| {
            a.iter().any(|r| {
                r["path"]
                    .as_str()
                    .is_some_and(|p| same_path(p, &self.config.root_folder))
            })
        });
        if !have {
            self.call(
                reqwest::Method::POST,
                "/rootfolder",
                Some(&json!({
                    "name": "cellarr", "path": self.config.root_folder,
                    "defaultMetadataProfileId": 1, "defaultQualityProfileId": 1,
                    "defaultMonitorOption": "none", "defaultNewItemMonitorOption": "none",
                    "defaultTags": [],
                })),
            )
            .await?;
        }
        *done = true;
        Ok(())
    }

    /// The sidecar's default metadata profile drops EPs (and singles), so an EP
    /// never gets an album row and its release search finds nothing. Allow every
    /// primary type on the profile new artists are added with. Best effort.
    async fn allow_all_primary_types(&self) {
        let Ok(profiles) = self.get("/metadataprofile").await else {
            return;
        };
        let Some(mut profile) = profiles
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"].as_i64() == Some(1)).cloned())
        else {
            return;
        };
        let mut changed = false;
        if let Some(items) = profile["primaryAlbumTypes"].as_array_mut() {
            for item in items {
                if item["allowed"] == json!(false) {
                    item["allowed"] = json!(true);
                    changed = true;
                }
            }
        }
        if changed {
            if let Err(e) = self
                .call(reqwest::Method::PUT, "/metadataprofile/1", Some(&profile))
                .await
            {
                tracing::warn!(error = %e, "could not widen the sidecar's metadata profile");
            }
        }
    }

    /// Create or update the sidecar's mirror of one cellarr indexer/client.
    async fn ensure_mirror(
        &self,
        catalog_name: &str,
        mirror_name: &str,
        settings: &Value,
    ) -> Result<(EntryKind, i64)> {
        self.ensure_setup().await?;
        if self.schema(catalog_name).is_none() {
            // A plugin installed after boot: look again, a few times, since the
            // sidecar may still be restarting to load it.
            for attempt in 0..4 {
                if attempt > 0 {
                    tokio::time::sleep(self.retry_pause).await;
                }
                if self.refresh_catalog().await.is_ok() && self.schema(catalog_name).is_some() {
                    break;
                }
            }
        }
        if let Ok(mut m) = self.mirrors.lock() {
            m.insert(
                mirror_name.to_string(),
                (catalog_name.to_string(), settings.clone()),
            );
        }
        let (kind, schema) = self.schema(catalog_name).ok_or_else(|| {
            err(format!(
                "the sidecar offers no implementation {catalog_name}"
            ))
        })?;
        let collection = match kind {
            EntryKind::Indexer => "/indexer",
            EntryKind::DownloadClient => "/downloadclient",
        };
        let mut wanted = schema.clone();
        wanted["name"] = json!(mirror_name);
        if let Some(fields) = wanted["fields"].as_array_mut() {
            for f in fields {
                if let Some(v) = f["name"].as_str().and_then(|n| settings.get(n)) {
                    f["value"] = v.clone();
                }
            }
        }
        match kind {
            EntryKind::Indexer => {
                wanted["enableRss"] = json!(false);
                wanted["enableAutomaticSearch"] = json!(true);
                wanted["enableInteractiveSearch"] = json!(true);
                wanted["priority"] = json!(25);
            }
            EntryKind::DownloadClient => {
                wanted["enable"] = json!(true);
                wanted["priority"] = json!(1);
            }
        }
        let existing = self.get(collection).await?;
        let found = existing
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == json!(mirror_name)).cloned());
        let id = match found {
            Some(cur) => {
                let id = cur["id"].as_i64().unwrap_or_default();
                if field_values(&cur) != field_values(&wanted) {
                    wanted["id"] = json!(id);
                    self.call(
                        reqwest::Method::PUT,
                        &format!("{collection}/{id}?forceSave=true"),
                        Some(&wanted),
                    )
                    .await?;
                }
                id
            }
            None => {
                let url = format!("{collection}?forceSave=true");
                match self.call(reqwest::Method::POST, &url, Some(&wanted)).await {
                    Ok(v) => v["id"].as_i64().unwrap_or_default(),
                    Err(first) if kind == EntryKind::DownloadClient => {
                        // Creating an enabled client validates it, and a client
                        // whose test needs more than its key allows fails. Create
                        // it disabled, then enable it with forceSave.
                        let mut off = wanted.clone();
                        off["enable"] = json!(false);
                        let made = self
                            .call(reqwest::Method::POST, &url, Some(&off))
                            .await
                            .map_err(|_| first)?;
                        let id = made["id"].as_i64().unwrap_or_default();
                        wanted["id"] = json!(id);
                        self.call(
                            reqwest::Method::PUT,
                            &format!("{collection}/{id}?forceSave=true"),
                            Some(&wanted),
                        )
                        .await?;
                        id
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        Ok((kind, id))
    }

    /// The sidecar's id for an album, adding the artist (unmonitored) if needed.
    async fn ensure_album(&self, artist_mbid: &str, album_mbid: &str) -> Result<i64> {
        self.ensure_setup().await?;
        let find = || async {
            self.get(&format!("/album?foreignAlbumId={album_mbid}"))
                .await
                .ok()
                .and_then(|v| {
                    v.as_array()
                        .and_then(|a| a.first())
                        .and_then(|a| a["id"].as_i64())
                })
        };
        if let Some(id) = find().await {
            return Ok(id);
        }
        let artists = self.get(&format!("/artist?mbId={artist_mbid}")).await?;
        if artists.as_array().is_none_or(Vec::is_empty) {
            let lookup = self
                .get(&format!("/artist/lookup?term=lidarr:{artist_mbid}"))
                .await?;
            let mut body = lookup
                .as_array()
                .and_then(|a| a.first().cloned())
                .ok_or_else(|| err(format!("the sidecar cannot find artist {artist_mbid}")))?;
            body["qualityProfileId"] = json!(1);
            body["metadataProfileId"] = json!(1);
            body["rootFolderPath"] = json!(self.config.root_folder);
            body["monitored"] = json!(false);
            body["monitorNewItems"] = json!("none");
            body["addOptions"] = json!({"monitor": "none", "searchForMissingAlbums": false});
            self.call(reqwest::Method::POST, "/artist", Some(&body))
                .await?;
        }
        // The album list arrives with the artist's refresh.
        let mut refreshed = false;
        for i in 0..self.album_wait_polls {
            if let Some(id) = find().await {
                return Ok(id);
            }
            if i == self.album_wait_polls / 3 && !refreshed {
                // An artist added before the profile was widened keeps its old
                // album list until refreshed.
                refreshed = true;
                if let Ok(a) = self.get(&format!("/artist?mbId={artist_mbid}")).await {
                    if let Some(aid) = a
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|a| a["id"].as_i64())
                    {
                        let _ = self
                            .call(
                                reqwest::Method::POST,
                                "/command",
                                Some(&json!({"name": "RefreshArtist", "artistId": aid})),
                            )
                            .await;
                    }
                }
            }
            tokio::time::sleep(self.retry_pause).await;
        }
        Err(err(format!(
            "the sidecar has no album {album_mbid} for artist {artist_mbid} (its metadata profile may exclude this album type, or the metadata server does not list it)"
        )))
    }

    /// Search one mirrored indexer for one album.
    ///
    /// # Errors
    /// When the sidecar is unreachable or does not know the album.
    pub async fn search(
        &self,
        catalog_name: &str,
        mirror_name: &str,
        settings: &Value,
        terms: &SearchTerms,
        indexer_id: IndexerId,
    ) -> Result<Vec<Release>> {
        let id = |k: &str| {
            terms
                .ids
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let (Some(artist), Some(album)) = (id("artist_mbid"), id("album_mbid")) else {
            // The sidecar searches by album; without the album's identity there
            // is nothing to ask it.
            return Ok(Vec::new());
        };
        let (_, sidecar_indexer) = self
            .ensure_mirror(catalog_name, mirror_name, settings)
            .await?;
        let album_id = self.ensure_album(&artist, &album).await?;
        let found = self.get(&format!("/release?albumId={album_id}")).await?;
        let mine: Vec<Value> = found
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r["indexerId"].as_i64() == Some(sidecar_indexer))
            .collect();
        if let Ok(mut cache) = self.found.lock() {
            for r in &mine {
                if let Some(g) = r["guid"].as_str() {
                    cache.insert(
                        g.to_string(),
                        Found {
                            indexer: sidecar_indexer,
                            protocol: r["protocol"].as_str().unwrap_or_default().to_string(),
                            album: album_id,
                        },
                    );
                }
            }
            // Bound the cache; the sidecar forgets its own after a while too.
            while cache.len() > 2000 {
                let first = cache.keys().next().cloned();
                if let Some(k) = first {
                    cache.remove(&k);
                }
            }
        }
        Ok(mine.iter().map(|r| release_of(r, indexer_id)).collect())
    }

    /// Hand a release to a mirrored download client. Returns the queue key.
    ///
    /// # Errors
    /// When the sidecar refuses the release.
    pub async fn grab(
        &self,
        catalog_name: &str,
        mirror_name: &str,
        settings: &Value,
        release: &Release,
    ) -> Result<String> {
        let (_, mut client_id) = self
            .ensure_mirror(catalog_name, mirror_name, settings)
            .await?;
        let known = release
            .guid
            .as_ref()
            .and_then(|g| self.found.lock().ok().and_then(|c| c.get(g).cloned()));
        let by_link = release
            .download_url
            .strip_prefix(SIDECAR_SCHEME)
            .and_then(|rest| {
                rest.split_once(':')
                    .map(|(i, g)| (i.parse::<i64>().unwrap_or_default(), g.to_string()))
            });
        if let Some(f) = &known {
            client_id = self.client_for_protocol(client_id, &f.protocol).await?;
        }
        // Only this client may take it: every other enabled client of the
        // sidecar steps back.
        let clients = self.get("/downloadclient").await?;
        for c in clients.as_array().cloned().unwrap_or_default() {
            let id = c["id"].as_i64().unwrap_or_default();
            let want_enabled = id == client_id;
            if c["enable"].as_bool() != Some(want_enabled) {
                let mut c = c.clone();
                c["enable"] = json!(want_enabled);
                self.call(
                    reqwest::Method::PUT,
                    &format!("/downloadclient/{id}?forceSave=true"),
                    Some(&c),
                )
                .await?;
            }
        }
        let cached = known
            .as_ref()
            .zip(release.guid.clone())
            .map(|(f, g)| (f.indexer, g));
        if let Some((indexer, guid)) = by_link.or(cached) {
            // A release the sidecar found itself: grab it from its own cache.
            self.grab_cached(&release.title, indexer, &guid).await?;
        } else {
            let protocol = match release.protocol {
                Protocol::Torrent => "torrent",
                Protocol::Usenet => "usenet",
            };
            let mut push = json!({
                "title": release.title,
                "protocol": protocol,
                "publishDate": now_iso(),
                "size": release.size.unwrap_or(0),
                "seeders": release.seeders,
                "indexer": "cellarr",
            });
            if release.download_url.starts_with("magnet:") {
                push["magnetUrl"] = json!(release.download_url);
            } else {
                push["downloadUrl"] = json!(release.download_url);
            }
            // Lidarr takes a pushed release only for a monitored album. Mirrors
            // are kept unmonitored (so the sidecar never searches on its own), so
            // monitor this one album for the push and put it back after.
            let restore = self.monitor_for_push(&release.title).await?;
            let verdict = self
                .call(reqwest::Method::POST, "/release/push", Some(&push))
                .await;
            self.restore_monitoring(restore).await;
            let verdict = verdict?;
            let first = verdict
                .as_array()
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or(verdict);
            if first["approved"] == json!(false) {
                return Err(PluginError::Guest(format!(
                    "the sidecar rejected the release: {}",
                    first["rejections"]
                )));
            }
        }
        // The grab is in the sidecar's history at once, with the client's id.
        for _ in 0..30 {
            let grabs = self
                .get(
                    "/history?page=1&pageSize=50&sortKey=date&sortDirection=descending&eventType=1",
                )
                .await?;
            let id = grabs["records"].as_array().and_then(|a| {
                a.iter()
                    .find(|r| {
                        r["sourceTitle"]
                            .as_str()
                            .is_some_and(|t| t.eq_ignore_ascii_case(&release.title))
                    })
                    .and_then(|r| r["downloadId"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
            if let Some(id) = id {
                return Ok(id);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(format!("title:{}", release.title))
    }

    /// The sidecar client that should take a release of `protocol`: `chosen`
    /// when it speaks that protocol (or the protocol is unknown), else a
    /// client of the sidecar that does, creating its mirror when cellarr has
    /// asked for one.
    async fn client_for_protocol(&self, chosen: i64, protocol: &str) -> Result<i64> {
        if protocol.is_empty() {
            return Ok(chosen);
        }
        let clients = self.get("/downloadclient").await?;
        let list = clients.as_array().cloned().unwrap_or_default();
        let speaks = |c: &Value| {
            c["protocol"]
                .as_str()
                .is_some_and(|p| p.eq_ignore_ascii_case(protocol))
        };
        if list
            .iter()
            .any(|c| c["id"].as_i64() == Some(chosen) && speaks(c))
        {
            return Ok(chosen);
        }
        // A mirror cellarr configured for this protocol, not yet created.
        let candidates: Vec<(String, String, Value)> = self
            .mirrors
            .lock()
            .map(|m| {
                m.iter()
                    .map(|(n, (c, s))| (n.clone(), c.clone(), s.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (name, catalog, settings) in candidates {
            let matches = self.schema(&catalog).is_some_and(|(k, sc)| {
                k == EntryKind::DownloadClient
                    && sc["protocol"]
                        .as_str()
                        .is_some_and(|p| p.eq_ignore_ascii_case(protocol))
            });
            if matches {
                if let Ok((_, id)) = self.ensure_mirror(&catalog, &name, &settings).await {
                    return Ok(id);
                }
            }
        }
        Ok(list
            .iter()
            .find(|c| speaks(c))
            .and_then(|c| c["id"].as_i64())
            .unwrap_or(chosen))
    }

    /// Grab a release out of the sidecar's search cache. The cache forgets after
    /// about half an hour; when it has, search the album again, find the same
    /// release and grab that, once.
    async fn grab_cached(&self, title: &str, indexer: i64, guid: &str) -> Result<()> {
        let body = json!({ "guid": guid, "indexerId": indexer });
        let first = self
            .call(reqwest::Method::POST, "/release", Some(&body))
            .await;
        let Err(e) = first else { return Ok(()) };
        if !e.to_string().to_ascii_lowercase().contains("not in cache") {
            return Err(e);
        }
        let known = self.found.lock().ok().and_then(|c| c.get(guid).cloned());
        let album = match known.as_ref().map(|f| f.album) {
            Some(a) => Some(a),
            None => self
                .get(&format!("/parse?title={}", encode_query(title)))
                .await
                .ok()
                .and_then(|p| p["albums"][0]["id"].as_i64()),
        };
        let Some(album) = album else { return Err(e) };
        let found = self.get(&format!("/release?albumId={album}")).await?;
        let list = found.as_array().cloned().unwrap_or_default();
        let same = |r: &&Value| r["indexerId"].as_i64() == Some(indexer);
        let hit = list
            .iter()
            .filter(same)
            .find(|r| r["guid"].as_str() == Some(guid))
            .or_else(|| {
                list.iter().filter(same).find(|r| {
                    r["title"]
                        .as_str()
                        .is_some_and(|t| t.eq_ignore_ascii_case(title))
                })
            })
            .or_else(|| {
                list.iter().find(|r| {
                    r["title"]
                        .as_str()
                        .is_some_and(|t| t.eq_ignore_ascii_case(title))
                })
            });
        let Some(hit) = hit else {
            return Err(err(format!(
                "the sidecar's search cache expired and a new search did not find \"{title}\" again"
            )));
        };
        let body = json!({
            "guid": hit["guid"], "indexerId": hit["indexerId"],
        });
        self.call(reqwest::Method::POST, "/release", Some(&body))
            .await
            .map(|_| ())
    }

    /// Monitor the artist and album a release title names, returning what to
    /// put back.
    async fn monitor_for_push(&self, title: &str) -> Result<Vec<(String, Value)>> {
        let parsed = self
            .get(&format!("/parse?title={}", encode_query(title)))
            .await?;
        let artist = parsed["artist"].clone();
        if !artist.is_object() {
            return Err(PluginError::Guest(format!(
                "the sidecar does not know the artist of \"{title}\" (it learns an artist when one of its indexers searches for it)"
            )));
        }
        let mut restore = Vec::new();
        if artist["monitored"] == json!(false) {
            let id = artist["id"].as_i64().unwrap_or_default();
            let mut a = self.get(&format!("/artist/{id}")).await?;
            restore.push((format!("/artist/{id}"), a.clone()));
            a["monitored"] = json!(true);
            self.call(reqwest::Method::PUT, &format!("/artist/{id}"), Some(&a))
                .await?;
        }
        for album in parsed["albums"].as_array().cloned().unwrap_or_default() {
            if album["monitored"] == json!(false) {
                let id = album["id"].as_i64().unwrap_or_default();
                let mut al = self.get(&format!("/album/{id}")).await?;
                restore.push((format!("/album/{id}"), al.clone()));
                al["monitored"] = json!(true);
                self.call(reqwest::Method::PUT, &format!("/album/{id}"), Some(&al))
                    .await?;
            }
        }
        Ok(restore)
    }

    async fn restore_monitoring(&self, restore: Vec<(String, Value)>) {
        for (path, body) in restore.into_iter().rev() {
            if let Err(e) = self.call(reqwest::Method::PUT, &path, Some(&body)).await {
                tracing::warn!(path, error = %e, "could not restore the sidecar's monitoring");
            }
        }
    }

    async fn queue(&self) -> Result<Vec<Value>> {
        let page = self
            .get("/queue?page=1&pageSize=1000&includeUnknownArtistItems=true")
            .await?;
        Ok(page["records"].as_array().cloned().unwrap_or_default())
    }

    async fn queue_record_by_title(&self, title: &str) -> Result<Option<Value>> {
        Ok(self.queue().await?.into_iter().find(|r| {
            r["title"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(title))
        }))
    }

    async fn queue_record(&self, key: &str) -> Result<Option<Value>> {
        if let Some(title) = key.strip_prefix("title:") {
            return self.queue_record_by_title(title).await;
        }
        Ok(self.queue().await?.into_iter().find(|r| {
            r["downloadId"]
                .as_str()
                .is_some_and(|d| d.eq_ignore_ascii_case(key))
        }))
    }

    /// A download's status, from the sidecar's queue.
    ///
    /// # Errors
    /// When the sidecar is unreachable or the download is gone.
    pub async fn status(&self, key: &str) -> Result<DownloadStatus> {
        if let Some(rec) = self.queue_record(key).await? {
            return Ok(status_of(&rec));
        }
        // Gone from the queue: the history says whether it was staged or failed.
        let events = self.history_for(key).await?;
        if events.is_empty() {
            return Err(err(format!("the sidecar has no download {key}")));
        }
        Ok(status_from_history(&events))
    }

    async fn history_for(&self, key: &str) -> Result<Vec<Value>> {
        if key.starts_with("title:") {
            return Ok(Vec::new());
        }
        let page = self
            .get(&format!(
                "/history?page=1&pageSize=200&downloadId={}",
                encode_query(key)
            ))
            .await?;
        Ok(page["records"].as_array().cloned().unwrap_or_default())
    }

    /// Remove a download from the sidecar (and its client).
    ///
    /// # Errors
    /// When the sidecar refuses.
    pub async fn remove(&self, key: &str, delete_data: bool) -> Result<()> {
        // cellarr has its own copy now; the staged one goes.
        self.clear_staged(key).await;
        let Some(rec) = self.queue_record(key).await? else {
            return Ok(());
        };
        let id = rec["id"].as_i64().unwrap_or_default();
        let _ = delete_data; // The sidecar's client decides what removal deletes.
        self.call(
            reqwest::Method::DELETE,
            &format!("/queue/{id}?removeFromClient=true&blocklist=false&skipRedownload=true"),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn clear_staged(&self, key: &str) {
        let Ok(events) = self.history_for(key).await else {
            return;
        };
        let staged: Vec<String> = events
            .iter()
            .filter(|e| e["eventType"] == json!("trackFileImported"))
            .filter_map(|e| e["data"]["importedPath"].as_str().map(str::to_string))
            .collect();
        let albums: std::collections::BTreeSet<i64> = events
            .iter()
            .filter_map(|e| e["albumId"].as_i64())
            .collect();
        for album in albums {
            let Ok(files) = self.get(&format!("/trackfile?albumId={album}")).await else {
                continue;
            };
            for f in files.as_array().cloned().unwrap_or_default() {
                if f["path"]
                    .as_str()
                    .is_some_and(|p| staged.iter().any(|s| s == p))
                {
                    let id = f["id"].as_i64().unwrap_or_default();
                    let _ = self
                        .call(reqwest::Method::DELETE, &format!("/trackfile/{id}"), None)
                        .await;
                }
            }
        }
    }

    /// Ask the sidecar to install a plugin from its GitHub URL (Lidarr restarts
    /// itself to load it), then wait for it to list the plugin and publish its
    /// implementations.
    ///
    /// # Errors
    /// When the sidecar refuses the command.
    pub async fn install(&self, github_url: &str) -> Result<InstallOutcome> {
        self.call(
            reqwest::Method::POST,
            "/command",
            Some(&json!({ "name": "InstallPlugin", "githubUrl": github_url })),
        )
        .await?;
        let wanted = github_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        for _ in 0..self.install_polls {
            tokio::time::sleep(self.retry_pause).await;
            // The sidecar is down while it restarts to load the plugin.
            let Ok(list) = self.installed().await else {
                continue;
            };
            let listed = list.as_array().is_some_and(|a| {
                a.iter().any(|p| {
                    p["githubUrl"].as_str().is_some_and(|u| {
                        u.trim_end_matches('/')
                            .eq_ignore_ascii_case(github_url.trim_end_matches('/'))
                    }) || (!wanted.is_empty()
                        && p["name"]
                            .as_str()
                            .is_some_and(|n| n.to_ascii_lowercase() == wanted))
                })
            });
            if listed && self.refresh_catalog().await.is_ok() {
                return Ok(InstallOutcome::Loaded);
            }
        }
        Ok(InstallOutcome::RestartRequired)
    }

    /// Pass a command straight to the sidecar.
    ///
    /// # Errors
    /// When the sidecar refuses the command.
    pub async fn forward_command(&self, body: &Value) -> Result<Value> {
        self.call(reqwest::Method::POST, "/command", Some(body))
            .await
    }

    /// The plugins the sidecar has installed.
    ///
    /// # Errors
    /// When the sidecar is unreachable.
    pub async fn installed(&self) -> Result<Value> {
        self.get("/system/plugins").await
    }
}

fn encode_query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn field_values(resource: &Value) -> BTreeMap<String, Value> {
    resource["fields"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| Some((f["name"].as_str()?.to_string(), f["value"].clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn fields_of(schema: &Value) -> Vec<EntryField> {
    schema["fields"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(EntryField {
                        name: f["name"].as_str()?.to_string(),
                        label: f["label"].as_str().unwrap_or_default().to_string(),
                        kind: f["type"].as_str().unwrap_or("textbox").to_string(),
                        default_value: f.get("value").cloned().unwrap_or(Value::Null).to_string(),
                        help_text: f["helpText"].as_str().unwrap_or_default().to_string(),
                        advanced: f["advanced"].as_bool().unwrap_or(false),
                        privacy: f.get("privacy").is_some_and(|p| p != "normal"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn release_of(r: &Value, indexer_id: IndexerId) -> Release {
    let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
    let protocol = s("protocol");
    let standard =
        protocol.eq_ignore_ascii_case("torrent") || protocol.eq_ignore_ascii_case("usenet");
    let link = [s("magnetUrl"), s("downloadUrl")]
        .into_iter()
        .find(|l| !l.is_empty());
    // A release in a plugin's own protocol (a streaming service, say) can only
    // be fetched by the sidecar; route it back there.
    let download_url = match link {
        Some(l) if standard => l,
        _ => format!(
            "{SIDECAR_SCHEME}{}:{}",
            r["indexerId"].as_i64().unwrap_or_default(),
            s("guid")
        ),
    };
    Release {
        indexer_id,
        title: s("title"),
        download_url,
        guid: Some(s("guid")).filter(|g| !g.is_empty()),
        protocol: if protocol.eq_ignore_ascii_case("usenet") {
            Protocol::Usenet
        } else {
            Protocol::Torrent
        },
        size: r["size"].as_u64(),
        seeders: r["seeders"].as_u64().map(|v| v as u32),
        indexer_flags: r["indexerFlags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_lowercase))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn status_of(rec: &Value) -> DownloadStatus {
    let status = rec["status"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tracked = rec["trackedDownloadState"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let size = rec["size"].as_f64().unwrap_or(0.0);
    let left = rec["sizeleft"].as_f64().unwrap_or(0.0);
    let progress = if size > 0.0 {
        ((size - left) / size).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let warned = rec["trackedDownloadStatus"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("warning") || s.eq_ignore_ascii_case("error"));
    // A finished download the sidecar is still staging is not done yet. One it
    // will not stage (blocked, failed, or pending with a warning, which it
    // retries forever) is handed to cellarr from the download folder, so
    // cellarr's own matching decides.
    let refused = tracked == "importblocked"
        || tracked == "importfailed"
        || (tracked == "importpending" && warned);
    let state =
        if status == "failed" || tracked.starts_with("downloadfailed") || tracked == "failed" {
            DownloadState::Failed
        } else if refused {
            DownloadState::Completed
        } else if status == "completed" {
            DownloadState::Downloading
        } else if status == "queued" || status == "delay" || status == "paused" {
            DownloadState::Queued
        } else {
            DownloadState::Downloading
        };
    let messages: Vec<String> = rec["statusMessages"]
        .as_array()
        .map(|a| {
            a.iter()
                .flat_map(|m| m["messages"].as_array().cloned().unwrap_or_default())
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    DownloadStatus {
        state,
        progress: if state == DownloadState::Completed {
            1.0
        } else {
            progress
        },
        content_path: (state == DownloadState::Completed)
            .then(|| rec["outputPath"].as_str().map(str::to_string))
            .flatten(),
        ratio: None,
        seeding_time_secs: None,
        peers: None,
        error_string: rec["errorMessage"]
            .as_str()
            .map(str::to_string)
            .or_else(|| (!messages.is_empty()).then(|| messages.join("; "))),
    }
}

/// A download's state from its history, once it has left the queue.
fn status_from_history(events: &[Value]) -> DownloadStatus {
    let kind = |e: &Value| e["eventType"].as_str().unwrap_or_default().to_string();
    let staged: Vec<String> = events
        .iter()
        .filter(|e| kind(e) == "trackFileImported")
        .filter_map(|e| e["data"]["importedPath"].as_str().map(str::to_string))
        .collect();
    let failed = events
        .iter()
        .find(|e| kind(e) == "downloadFailed")
        .map(|e| {
            e["data"]["message"]
                .as_str()
                .unwrap_or("download failed")
                .to_string()
        });
    let done = |state, path: Option<String>, error: Option<String>| DownloadStatus {
        state,
        progress: 1.0,
        content_path: path,
        ratio: None,
        seeding_time_secs: None,
        peers: None,
        error_string: error,
    };
    match (common_parent(&staged), failed) {
        (Some(folder), _) => done(DownloadState::Completed, Some(folder), None),
        (None, Some(message)) => done(DownloadState::Failed, None, Some(message)),
        (None, None) => DownloadStatus {
            state: DownloadState::Downloading,
            progress: 1.0,
            content_path: None,
            ratio: None,
            seeding_time_secs: None,
            peers: None,
            error_string: None,
        },
    }
}

/// The deepest folder containing every path.
fn common_parent(paths: &[String]) -> Option<String> {
    let dirs: Vec<std::path::PathBuf> = paths
        .iter()
        .filter_map(|p| {
            std::path::Path::new(p)
                .parent()
                .map(std::path::Path::to_path_buf)
        })
        .collect();
    let first = dirs.first()?.clone();
    let mut common = first;
    for d in &dirs[1..] {
        while !d.starts_with(&common) {
            common = common.parent()?.to_path_buf();
        }
    }
    Some(common.to_string_lossy().to_string())
}

static SIDECAR: OnceLock<RwLock<Option<Arc<Sidecar>>>> = OnceLock::new();

/// Install the process-wide sidecar bridge.
pub fn install(sidecar: Option<Arc<Sidecar>>) {
    let cell = SIDECAR.get_or_init(|| RwLock::new(None));
    if let Ok(mut g) = cell.write() {
        *g = sidecar;
    }
}

/// The process-wide sidecar bridge, when one is configured.
#[must_use]
pub fn global() -> Option<Arc<Sidecar>> {
    SIDECAR
        .get()
        .and_then(|c| c.read().ok().and_then(|g| g.clone()))
}

/// Discover the sidecar's plugins now and again every few minutes, publishing
/// them to the catalog. A sidecar that starts later is picked up on the next pass.
pub fn spawn_discovery(sidecar: Arc<Sidecar>) {
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(5);
        loop {
            match sidecar.refresh_catalog().await {
                Ok(n) => {
                    tracing::info!(implementations = n, "lidarr sidecar discovered");
                    delay = Duration::from_secs(120);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lidarr sidecar not reachable yet");
                    delay = (delay * 2).min(Duration::from_secs(120));
                }
            }
            tokio::time::sleep(delay).await;
        }
    });
}

/// A cellarr indexer backed by a native plugin in the sidecar.
pub struct SidecarIndexer {
    sidecar: Arc<Sidecar>,
    implementation: String,
    settings: Value,
    indexer_id: IndexerId,
    name: String,
}

impl SidecarIndexer {
    /// The mirror of one configured indexer.
    #[must_use]
    pub fn new(
        sidecar: Arc<Sidecar>,
        implementation: String,
        settings: Value,
        indexer_id: IndexerId,
        name: String,
    ) -> Self {
        Self {
            sidecar,
            implementation,
            settings,
            indexer_id,
            name,
        }
    }
}

#[async_trait]
impl Indexer for SidecarIndexer {
    type Error = PluginError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, terms: &SearchTerms) -> std::result::Result<Vec<Release>, PluginError> {
        self.sidecar
            .search(
                &self.implementation,
                &format!("cellarr:{}", self.indexer_id),
                &self.settings,
                terms,
                self.indexer_id,
            )
            .await
    }

    async fn latest(&self) -> std::result::Result<Vec<Release>, PluginError> {
        // The sidecar searches by album; RSS stays with cellarr's own indexers.
        Ok(Vec::new())
    }
}

/// A cellarr download client backed by a native plugin in the sidecar.
pub struct SidecarDownloadClient {
    sidecar: Arc<Sidecar>,
    implementation: String,
    settings: Value,
    mirror: String,
    name: String,
}

impl SidecarDownloadClient {
    /// The mirror of one configured client.
    #[must_use]
    pub fn new(
        sidecar: Arc<Sidecar>,
        implementation: String,
        settings: Value,
        mirror: String,
        name: String,
    ) -> Self {
        Self {
            sidecar,
            implementation,
            settings,
            mirror,
            name,
        }
    }
}

#[async_trait]
impl DownloadClient for SidecarDownloadClient {
    type Error = PluginError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn add(&self, grab: &GrabRequest) -> std::result::Result<String, PluginError> {
        self.sidecar
            .grab(
                &self.implementation,
                &self.mirror,
                &self.settings,
                &grab.release,
            )
            .await
    }

    async fn status(&self, download_id: &str) -> std::result::Result<DownloadStatus, PluginError> {
        self.sidecar.status(download_id).await
    }

    async fn remove(
        &self,
        download_id: &str,
        delete_data: bool,
    ) -> std::result::Result<(), PluginError> {
        self.sidecar.remove(download_id, delete_data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_download_is_done_once_staged_or_blocked() {
        // Finished but still being staged by the sidecar: not done yet.
        let s = status_of(&json!({
            "status": "completed", "trackedDownloadState": "importPending",
            "size": 100.0, "sizeleft": 0.0, "outputPath": "/downloads/Kid A",
        }));
        assert_eq!(s.state, DownloadState::Downloading);
        // The sidecar could not stage it: cellarr takes the download folder.
        let s = status_of(&json!({
            "status": "completed", "trackedDownloadState": "importBlocked",
            "size": 100.0, "sizeleft": 0.0, "outputPath": "/downloads/Kid A",
        }));
        assert_eq!(s.state, DownloadState::Completed);
        assert_eq!(s.content_path.as_deref(), Some("/downloads/Kid A"));
        // Staged: done, from the staging folder.
        let s = status_from_history(&[
            json!({"eventType": "grabbed"}),
            json!({"eventType": "trackFileImported", "data": {"importedPath": "/stage/Radiohead/Kid A (2000)/01 - a.flac"}}),
            json!({"eventType": "trackFileImported", "data": {"importedPath": "/stage/Radiohead/Kid A (2000)/02 - b.flac"}}),
            json!({"eventType": "downloadImported"}),
        ]);
        assert_eq!(s.state, DownloadState::Completed);
        assert_eq!(
            s.content_path.as_deref(),
            Some("/stage/Radiohead/Kid A (2000)")
        );
        let s = status_from_history(&[
            json!({"eventType": "downloadFailed", "data": {"message": "bad"}}),
        ]);
        assert_eq!(s.state, DownloadState::Failed);
        let s = status_of(&json!({"status": "downloading", "size": 100.0, "sizeleft": 25.0}));
        assert_eq!(s.state, DownloadState::Downloading);
        assert!((s.progress - 0.75).abs() < 1e-6);
        assert!(s.content_path.is_none());
        let s = status_of(
            &json!({"status": "warning", "trackedDownloadState": "downloadFailedPending"}),
        );
        assert_eq!(s.state, DownloadState::Failed);
        // Refused by the sidecar's importer (it would retry forever): cellarr's turn.
        let s = status_of(&json!({
            "status": "completed", "trackedDownloadState": "importPending",
            "trackedDownloadStatus": "warning", "outputPath": "/downloads/Kid A",
        }));
        assert_eq!(s.state, DownloadState::Completed);
        assert_eq!(s.content_path.as_deref(), Some("/downloads/Kid A"));
    }

    #[test]
    fn a_plugin_protocol_release_routes_back_to_the_sidecar() {
        let id = IndexerId::new();
        let r = release_of(
            &json!({"title": "x", "guid": "g", "protocol": "TidalDownloadProtocol", "indexerId": 7,
                    "downloadUrl": "https://tidal.example/123"}),
            id,
        );
        assert_eq!(r.download_url, "lidarr-sidecar:7:g");
        let r = release_of(
            &json!({"title": "x", "guid": "g", "protocol": "torrent", "indexerId": 7,
                    "magnetUrl": "magnet:?xt=urn:btih:abc"}),
            id,
        );
        assert_eq!(r.download_url, "magnet:?xt=urn:btih:abc");
    }

    #[test]
    fn only_non_builtin_implementations_are_plugins_unless_builtins_are_exposed() {
        let mut c = SidecarConfig {
            url: String::new(),
            api_key: String::new(),
            root_folder: String::new(),
            expose_builtins: false,
        };
        let s = Sidecar::new(c.clone());
        assert_eq!(s.catalog_name(EntryKind::Indexer, "Torznab"), None);
        assert_eq!(
            s.catalog_name(EntryKind::Indexer, "Tidal").as_deref(),
            Some("Tidal")
        );
        c.expose_builtins = true;
        let s = Sidecar::new(c);
        assert_eq!(
            s.catalog_name(EntryKind::DownloadClient, "QBittorrent")
                .as_deref(),
            Some("QBittorrentViaLidarr")
        );
    }

    // --- a fake sidecar over a socket ------------------------------------

    type Handler = std::sync::Arc<dyn Fn(&str, &str, &Value) -> (u16, Value) + Send + Sync>;

    async fn fake(handler: Handler) -> Sidecar {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head_end, len) = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut parts = head.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or_default().to_string();
                    let path = target
                        .strip_prefix("/api/v1")
                        .unwrap_or(&target)
                        .to_string();
                    let body: Value =
                        serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
                    let (status, out) = handler(&method, &path, &body);
                    let text = out.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        let mut s = Sidecar::new(SidecarConfig {
            url: format!("http://{addr}"),
            api_key: "k".into(),
            root_folder: "/root".into(),
            expose_builtins: false,
        });
        s.retry_pause = Duration::from_millis(5);
        s.install_polls = 3;
        s.album_wait_polls = 3;
        s
    }

    type Log = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;

    fn logged(log: &Log, method: &str, path: &str, body: &Value) {
        log.lock()
            .unwrap()
            .push((method.into(), path.into(), body.clone()));
    }

    fn slsk_schema() -> Value {
        json!({"implementation": "Slskd", "protocol": "SoulseekDownloadProtocol",
               "fields": [{"name": "baseUrl", "value": ""}]})
    }

    #[tokio::test]
    async fn a_mirror_that_fails_validation_is_created_disabled_then_enabled() {
        let log: Log = Default::default();
        let l = log.clone();
        let sc = fake(std::sync::Arc::new(move |m, p, b| {
            logged(&l, m, p, b);
            match (m, p.split('?').next().unwrap()) {
                ("GET", "/config/downloadclient") => (
                    200,
                    json!({"id": 1, "enableCompletedDownloadHandling": true}),
                ),
                ("GET", "/metadataprofile") => (200, json!([])),
                ("GET", "/rootfolder") => (200, json!([{"path": "/root"}])),
                ("GET", "/downloadclient") => (200, json!([])),
                ("POST", "/downloadclient") if b["enable"] == json!(true) => (
                    400,
                    json!([{"propertyName": "x", "errorMessage": "403 on /api/v0/options"}]),
                ),
                ("POST", "/downloadclient") => (201, json!({"id": 9})),
                ("PUT", "/downloadclient/9") => (202, json!({"id": 9})),
                _ => (404, json!({})),
            }
        }))
        .await;
        sc.schemas
            .write()
            .unwrap()
            .insert("slskd".into(), (EntryKind::DownloadClient, slsk_schema()));
        let (_, id) = sc
            .ensure_mirror("Slskd", "cellarr-slskd", &json!({"baseUrl": "http://s"}))
            .await
            .unwrap();
        assert_eq!(id, 9);
        let log = log.lock().unwrap();
        let posts: Vec<_> = log.iter().filter(|(m, ..)| m == "POST").collect();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1].2["enable"], json!(false));
        let put = log.iter().find(|(m, ..)| m == "PUT").unwrap();
        assert!(put.1.contains("forceSave=true"));
        assert_eq!(put.2["enable"], json!(true));
    }

    #[tokio::test]
    async fn setup_allows_every_primary_album_type() {
        let log: Log = Default::default();
        let l = log.clone();
        let sc = fake(std::sync::Arc::new(move |m, p, b| {
            logged(&l, m, p, b);
            match (m, p) {
                ("GET", "/config/downloadclient") => (
                    200,
                    json!({"id": 1, "enableCompletedDownloadHandling": true}),
                ),
                ("GET", "/metadataprofile") => (
                    200,
                    json!([{"id": 1, "name": "Standard", "primaryAlbumTypes": [
                        {"albumType": {"name": "Album"}, "allowed": true},
                        {"albumType": {"name": "EP"}, "allowed": false}]}]),
                ),
                ("PUT", "/metadataprofile/1") => (202, json!({})),
                ("GET", "/rootfolder") => (200, json!([{"path": "/root"}])),
                _ => (404, json!({})),
            }
        }))
        .await;
        sc.ensure_setup().await.unwrap();
        let log = log.lock().unwrap();
        let put = log.iter().find(|(m, ..)| m == "PUT").unwrap();
        assert!(put.2["primaryAlbumTypes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["allowed"] == json!(true)));
    }

    #[tokio::test]
    async fn an_expired_release_cache_is_searched_again_once() {
        let log: Log = Default::default();
        let l = log.clone();
        let sc = fake(std::sync::Arc::new(move |m, p, b| {
            logged(&l, m, p, b);
            let posts = l
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, p, _)| m == "POST" && p == "/release")
                .count();
            match (m, p.split('?').next().unwrap()) {
                ("POST", "/release") if b["guid"] == json!("old") => (
                    404,
                    json!({"message": "Unable to find matching release, release not in cache"}),
                ),
                ("POST", "/release") => {
                    assert!(posts >= 2);
                    (200, json!({}))
                }
                ("GET", "/release") => (
                    200,
                    json!([{"guid": "new", "indexerId": 3, "title": "Burial - Street Halo"}]),
                ),
                ("GET", "/parse") => (200, json!({"albums": [{"id": 77}]})),
                _ => (404, json!({})),
            }
        }))
        .await;
        sc.grab_cached("Burial - Street Halo", 3, "old")
            .await
            .unwrap();
        let log = log.lock().unwrap();
        let last = log
            .iter()
            .rfind(|(m, p, _)| m == "POST" && p == "/release")
            .unwrap();
        assert_eq!(last.2["guid"], json!("new"));
        assert!(log
            .iter()
            .any(|(_, p, _)| p.starts_with("/release?albumId=77")));
    }

    #[tokio::test]
    async fn a_release_goes_to_the_client_that_speaks_its_protocol() {
        let sc = fake(std::sync::Arc::new(|m, p, _| match (m, p) {
            ("GET", "/downloadclient") => (
                200,
                json!([{"id": 1, "protocol": "torrent"},
                       {"id": 2, "protocol": "SoulseekDownloadProtocol"}]),
            ),
            _ => (404, json!({})),
        }))
        .await;
        assert_eq!(
            sc.client_for_protocol(1, "SoulseekDownloadProtocol")
                .await
                .unwrap(),
            2
        );
        assert_eq!(sc.client_for_protocol(1, "torrent").await.unwrap(), 1);
        assert_eq!(sc.client_for_protocol(1, "").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn install_reports_loaded_only_when_the_sidecar_lists_the_plugin() {
        let listed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let l = listed.clone();
        let sc = fake(std::sync::Arc::new(move |m, p, _| match (m, p) {
            ("POST", "/command") => (201, json!({})),
            ("GET", "/system/plugins") if l.load(std::sync::atomic::Ordering::SeqCst) => (
                200,
                json!([{"name": "Tubifarry", "githubUrl": "https://github.com/TypNull/Tubifarry"}]),
            ),
            ("GET", "/system/plugins") => (200, json!([])),
            ("GET", _) => (200, json!([])),
            _ => (404, json!({})),
        }))
        .await;
        let url = "https://github.com/TypNull/Tubifarry";
        assert_eq!(
            sc.install(url).await.unwrap(),
            InstallOutcome::RestartRequired
        );
        listed.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sc.install(url).await.unwrap(), InstallOutcome::Loaded);
    }
}

//! Native Soulseek support: a client for slskd's HTTP API, the search side that
//! turns Soulseek results into [`Release`]s, and the download client that
//! enqueues them and reports their progress.
//!
//! Soulseek has no indexers and no torrents, so one small module serves both
//! halves the pipeline needs:
//!
//! - [`SlskdIndexer`] (an [`Indexer`]) runs `POST /api/v0/searches`, polls
//!   `GET /api/v0/searches/{id}`, reads `.../responses`, groups the files by
//!   `(user, folder)` and keeps the folders that look like the wanted album
//!   ([`candidates_from_responses`]). Each becomes a release whose
//!   `download_url` is a self-contained `slskd:` link naming the user and the
//!   exact files ([`SlskdTarget`]), so a grab needs no state kept by the indexer.
//! - [`SlskdClient`] (a [`DownloadClient`]) enqueues those files with
//!   `POST /api/v0/transfers/downloads/{username}` and tracks them in
//!   `GET /api/v0/transfers/downloads`. The download id carries the same
//!   target, so status survives a restart.
//!
//! # Where finished files are
//!
//! slskd's non-admin API key cannot read `/options`, so the completed folder is
//! **not** read from slskd. It is derived from the transfer itself plus a
//! configured download root ([`DEFAULT_DOWNLOAD_ROOT`]): slskd files a folder
//! under `<root>/<username>/<last folder name>`; the plain
//! `<root>/<last folder name>` layout is tried when that one does not exist.
//! No remote-path mapping is involved.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cellarr_core::{
    DownloadClient, DownloadState, DownloadStatus, GrabRequest, Indexer, IndexerId, Protocol,
    Release, SearchTerms,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::DownloadError;
use crate::http::{HttpRequest, HttpTransport, ReqwestTransport};
use crate::lifecycle::DownloadProgress;

/// Where slskd's downloads live when the settings do not say.
pub const DEFAULT_DOWNLOAD_ROOT: &str = "/data/torrents/soulseek";

/// The scheme of a Soulseek release's `download_url`.
pub const SLSKD_SCHEME: &str = "slskd:";

/// The protocol name the Lidarr face and the client router use for Soulseek.
pub const SOULSEEK_PROTOCOL: &str = "SoulseekDownloadProtocol";

const ID_PREFIX: &str = "slskd-";

const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "ape", "wv", "alac", "wma",
];

/// Connection and behaviour settings for slskd, shared by the indexer and the
/// client (a row may carry only the half it needs).
#[derive(Debug, Clone, Deserialize)]
pub struct SlskdSettings {
    /// slskd's base URL, e.g. `http://slskd.downloads:5030`.
    #[serde(alias = "baseUrl", alias = "base_url", default)]
    pub base_url: String,
    /// The API key (sent as `X-API-Key`).
    #[serde(alias = "apiKey", alias = "api_key", default)]
    pub api_key: String,
    /// Where slskd's completed downloads are, as this process sees them.
    #[serde(alias = "downloadRoot", default = "default_root")]
    pub download_root: String,
    /// How long a Soulseek search runs, in seconds.
    #[serde(alias = "searchTimeoutSecs", default = "default_search_secs")]
    pub search_timeout_secs: u64,
    /// Delay between search polls, in milliseconds.
    #[serde(alias = "pollIntervalMs", default = "default_poll_ms")]
    pub poll_interval_ms: u64,
    /// Folders with fewer audio files than this are not albums.
    #[serde(alias = "minimumFiles", default = "default_min_files")]
    pub minimum_files: usize,
    /// At most this many releases per search.
    #[serde(alias = "maxReleases", default = "default_max_releases")]
    pub max_releases: usize,
    /// Users never downloaded from: a JSON array or a comma/newline list.
    #[serde(alias = "blockedUsers", default)]
    pub blocked_users: Value,
    /// Skip users whose upload queue is longer than this (0 = no limit).
    #[serde(alias = "maximumQueueLength", default)]
    pub maximum_queue_length: u32,
}

fn default_root() -> String {
    DEFAULT_DOWNLOAD_ROOT.to_string()
}
const fn default_search_secs() -> u64 {
    15
}
const fn default_poll_ms() -> u64 {
    1000
}
const fn default_min_files() -> usize {
    3
}
const fn default_max_releases() -> usize {
    10
}

impl SlskdSettings {
    /// Read settings from a config row's JSON, or say why they cannot be used
    /// (no URL or key). The caller falls back to the plugin path on `Err`.
    ///
    /// # Errors
    /// A readable reason when the row cannot drive the native client.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s: Self = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if s.base_url.trim().is_empty() {
            return Err("no baseUrl".into());
        }
        if s.api_key.trim().is_empty() {
            return Err("no apiKey".into());
        }
        if v.get("native").and_then(Value::as_bool) == Some(false) {
            return Err("native=false (plugin path requested)".into());
        }
        Ok(s)
    }

    fn blocked(&self) -> Vec<String> {
        match &self.blocked_users {
            Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            Value::String(s) => s
                .split([',', '\n', ';'])
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------- encoding

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(crate) fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub(crate) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = B64.iter().position(|b| *b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub(crate) fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One remote file, as slskd names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlskdFile {
    /// The full remote path (backslash separated).
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
}

/// What a grab is: one user's files from one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlskdTarget {
    /// The Soulseek user.
    #[serde(rename = "u")]
    pub username: String,
    /// The remote folder.
    #[serde(rename = "d")]
    pub directory: String,
    /// The files to fetch.
    #[serde(rename = "f")]
    pub files: Vec<SlskdFile>,
}

impl SlskdTarget {
    fn encode(&self, prefix: &str) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{prefix}{}", b64_encode(&json))
    }

    /// The `download_url` of a release for this target.
    #[must_use]
    pub fn to_url(&self) -> String {
        self.encode(SLSKD_SCHEME)
    }

    /// Read a target back from a `slskd:` URL.
    #[must_use]
    pub fn from_url(url: &str) -> Option<Self> {
        Self::decode(url.strip_prefix(SLSKD_SCHEME)?)
    }

    fn decode(payload: &str) -> Option<Self> {
        serde_json::from_slice(&b64_decode(payload)?).ok()
    }

    fn download_id(&self) -> String {
        self.encode(ID_PREFIX)
    }

    fn from_download_id(id: &str) -> Option<Self> {
        Self::decode(id.strip_prefix(ID_PREFIX)?)
    }

    /// The last folder name of the remote directory.
    #[must_use]
    pub fn folder_name(&self) -> &str {
        last_component(&self.directory)
    }
}

/// Whether a config kind is Soulseek: the short name cellarr's own face uses
/// (`slskd`) or the plugin's class names a Lidarr database carries
/// (`SlskdClient` for the download client, `SlskdIndexer` for the indexer).
#[must_use]
pub fn is_slskd_kind(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "slskd" | "slskdclient" | "slskdindexer"
    )
}

/// The v3 `implementation` string for a Soulseek kind: the plugin's own class
/// name when the kind carries it, so a migrated row reports what production does.
#[must_use]
pub fn implementation_name(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "slskdclient" => "SlskdClient",
        "slskdindexer" => "SlskdIndexer",
        _ => "Slskd",
    }
}

/// Whether a release's link is a Soulseek one.
#[must_use]
pub fn is_slskd_url(url: &str) -> bool {
    url.starts_with(SLSKD_SCHEME)
}

pub(crate) fn last_component(path: &str) -> &str {
    path.rsplit(['\\', '/'])
        .find(|p| !p.is_empty())
        .unwrap_or("")
}

fn split_dir(filename: &str) -> (&str, &str) {
    match filename.rfind(['\\', '/']) {
        Some(i) => (&filename[..i], &filename[i + 1..]),
        None => ("", filename),
    }
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

// ----------------------------------------------------------------- the API

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct SearchInfo {
    id: String,
    state: String,
    is_complete: bool,
}

/// One file in a search response.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ResponseFile {
    /// Full remote path.
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
    /// Bit rate in kbps, when the peer reports one.
    pub bit_rate: Option<u32>,
    /// Bit depth, when reported.
    pub bit_depth: Option<u32>,
    /// A locked file cannot be downloaded.
    pub is_locked: bool,
}

/// One peer's answer to a search.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchResponse {
    /// The peer.
    pub username: String,
    /// The files it shares that matched.
    pub files: Vec<ResponseFile>,
    /// Whether it can start an upload right away.
    pub has_free_upload_slot: bool,
    /// Its upload speed.
    pub upload_speed: u64,
    /// How many uploads are queued ahead.
    pub queue_length: u32,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct UserTransfers {
    username: String,
    directories: Vec<DirTransfers>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct DirTransfers {
    files: Vec<Transfer>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
struct Transfer {
    id: String,
    filename: String,
    state: String,
    size: u64,
    bytes_transferred: u64,
    exception: Option<String>,
}

/// How many polls in a row may find none of a grab's files before the grab is
/// reported gone: slskd's listing can come back empty for a moment mid-grab.
pub const EMPTY_LISTING_GRACE: u32 = 5;

/// What was seen of one grab: files (bytes, succeeded), whether anything ever
/// progressed, the last status given, and how many empty listings in a row.
#[derive(Default)]
struct Seen {
    files: BTreeMap<String, (u64, bool)>,
    progressed: bool,
    last: Option<DownloadProgress>,
    empties: u32,
}

fn memory() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, Seen>> {
    static MEMORY: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Seen>>> =
        std::sync::OnceLock::new();
    MEMORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn seen_last(id: &str) -> Option<DownloadProgress> {
    memory().get(id).and_then(|s| s.last.clone())
}

/// An empty listing: the last status while within the grace, else `None`.
fn seen_empty(id: &str) -> Option<DownloadProgress> {
    let mut m = memory();
    let s = m.get_mut(id)?;
    s.empties += 1;
    if s.empties > EMPTY_LISTING_GRACE {
        m.remove(id);
        return None;
    }
    s.last.clone()
}

/// Forget a grab (it was removed).
fn forget(id: &str) {
    memory().remove(id);
}

/// Whether a download id is one this client issued (a Soulseek grab).
#[must_use]
pub fn is_download_id(id: &str) -> bool {
    id.starts_with(ID_PREFIX)
}

/// slskd's HTTP API over the shared transport seam.
pub struct SlskdApi {
    settings: SlskdSettings,
    transport: Box<dyn HttpTransport>,
}

impl SlskdApi {
    /// Over a caller-supplied transport (the test seam).
    #[must_use]
    pub fn new(settings: SlskdSettings, transport: Box<dyn HttpTransport>) -> Self {
        Self {
            settings,
            transport,
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/api/v0/{}",
            self.settings.base_url.trim_end_matches('/'),
            path
        )
    }

    fn req(&self, method: &str, path: &str) -> HttpRequest {
        HttpRequest::new(method, self.url(path))
            .header("x-api-key", self.settings.api_key.clone())
            .header("accept", "application/json")
    }

    async fn call(&self, req: HttpRequest) -> Result<String, DownloadError> {
        let resp = self.transport.send(req).await?;
        match resp.status {
            200..=299 => Ok(resp.body),
            401 | 403 => Err(DownloadError::Auth(format!(
                "slskd answered {}",
                resp.status
            ))),
            404 => Err(DownloadError::NotFound("slskd resource".into())),
            s => Err(DownloadError::Api(format!(
                "slskd answered {s}: {}",
                resp.body.chars().take(200).collect::<String>()
            ))),
        }
    }

    /// Run one Soulseek search to completion (or the timeout) and return what
    /// peers answered. The search is deleted afterwards.
    ///
    /// # Errors
    /// Transport, auth or API failures.
    pub async fn search(&self, text: &str) -> Result<Vec<SearchResponse>, DownloadError> {
        let id = new_guid();
        let timeout_ms = self.settings.search_timeout_secs.max(1) * 1000;
        let body = json!({
            "id": id,
            "searchText": text,
            "searchTimeout": timeout_ms,
            "responseLimit": 200,
            "fileLimit": 20000,
            "filterResponses": true,
            "minimumResponseFileCount": 1,
        });
        let started = self
            .call(
                self.req("POST", "searches")
                    .header("content-type", "application/json")
                    .body(body.to_string()),
            )
            .await?;
        let id = serde_json::from_str::<SearchInfo>(&started)
            .ok()
            .filter(|s| !s.id.is_empty())
            .map_or(id, |s| s.id);

        let deadline = Instant::now() + Duration::from_millis(timeout_ms + 10_000);
        let result = async {
            loop {
                let info: SearchInfo = serde_json::from_str(
                    &self
                        .call(self.req("GET", &format!("searches/{id}")))
                        .await?,
                )
                .map_err(|e| DownloadError::UnexpectedResponse(e.to_string()))?;
                if info.is_complete || info.state.starts_with("Completed") {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(self.settings.poll_interval_ms)).await;
            }
            let body = self
                .call(self.req("GET", &format!("searches/{id}/responses")))
                .await?;
            serde_json::from_str::<Vec<SearchResponse>>(&body)
                .map_err(|e| DownloadError::UnexpectedResponse(e.to_string()))
        }
        .await;
        // Best effort: a leftover search is only clutter in slskd's UI.
        let _ = self
            .call(self.req("DELETE", &format!("searches/{id}")))
            .await;
        result
    }

    /// Enqueue a target's files with slskd.
    ///
    /// # Errors
    /// When slskd refuses any file (other than "already queued").
    pub async fn enqueue(&self, target: &SlskdTarget) -> Result<(), DownloadError> {
        let files: Vec<Value> = target
            .files
            .iter()
            .map(|f| json!({"filename": f.filename, "size": f.size}))
            .collect();
        let body = self
            .call(
                self.req(
                    "POST",
                    &format!("transfers/downloads/{}", pct(&target.username)),
                )
                .header("content-type", "application/json")
                .body(Value::Array(files).to_string()),
            )
            .await?;
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            let failed: Vec<String> = v["failed"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|f| f.to_string())
                        .filter(|f| !f.to_lowercase().contains("already"))
                        .collect()
                })
                .unwrap_or_default();
            if !failed.is_empty() {
                return Err(DownloadError::Api(format!(
                    "slskd refused {} of {} files: {}",
                    failed.len(),
                    target.files.len(),
                    failed[0].chars().take(160).collect::<String>()
                )));
            }
        }
        Ok(())
    }

    async fn downloads(&self) -> Result<Vec<UserTransfers>, DownloadError> {
        let body = self.call(self.req("GET", "transfers/downloads")).await?;
        serde_json::from_str(&body).map_err(|e| DownloadError::UnexpectedResponse(e.to_string()))
    }

    async fn transfers_of(&self, target: &SlskdTarget) -> Result<Vec<Transfer>, DownloadError> {
        let wanted: Vec<&str> = target.files.iter().map(|f| f.filename.as_str()).collect();
        Ok(self
            .downloads()
            .await?
            .into_iter()
            .filter(|u| u.username.eq_ignore_ascii_case(&target.username))
            .flat_map(|u| u.directories)
            .flat_map(|d| d.files)
            .filter(|t| wanted.contains(&t.filename.as_str()))
            .collect())
    }
}

fn new_guid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let c = u128::from(N.fetch_add(1, Ordering::Relaxed));
    let v = nanos.wrapping_mul(6364136223846793005).wrapping_add(c);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 68) as u16 & 0xfff,
        (v >> 56) as u16 & 0xfff,
        (v & 0xffff_ffff_ffff) as u64
    )
}

// ---------------------------------------------------------------- matching

/// Lowercase alphanumeric words joined by single spaces (`&` reads as `and`).
#[must_use]
pub fn norm(s: &str) -> String {
    let s = s.to_lowercase().replace('&', " and ");
    let mapped: String = s
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `s` without its bracketed groups: `Nevermind (Deluxe Edition)` -> `Nevermind`.
#[must_use]
pub fn strip_brackets(s: &str) -> String {
    let (mut out, mut depth) = (String::new(), 0u32);
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn has_words(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && format!(" {hay} ").contains(&format!(" {needle} "))
}

/// A folder of audio files that looks like the wanted album.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The target to enqueue.
    pub target: SlskdTarget,
    /// `FLAC`, `FLAC 24bit`, `MP3 320`, ...
    pub format: String,
    /// Whether the files are FLAC.
    pub lossless: bool,
    /// Sum of file sizes.
    pub size: u64,
    /// The peer can upload immediately.
    pub free_slot: bool,
    /// The peer's upload queue length.
    pub queue_length: u32,
    /// The peer's upload speed.
    pub upload_speed: u64,
}

fn format_label(files: &[&ResponseFile], ext: &str) -> String {
    let mut rates: Vec<u32> = files.iter().filter_map(|f| f.bit_rate).collect();
    rates.sort_unstable();
    let median = rates.get(rates.len() / 2).copied().unwrap_or(0);
    match ext {
        "flac" => {
            if files.iter().any(|f| f.bit_depth.unwrap_or(16) > 16) {
                "FLAC 24bit".into()
            } else {
                "FLAC".into()
            }
        }
        "mp3" => match median {
            315.. => "MP3 320".into(),
            245..=314 => "MP3 256".into(),
            0 => "MP3".into(),
            _ => format!("MP3 {median}"),
        },
        "m4a" | "aac" | "alac" => "AAC".into(),
        other => other.to_uppercase(),
    }
}

/// Pick the folders in `responses` that are `artist` - `album`, best first.
///
/// Deterministic and deliberately plain: a folder qualifies when its last two
/// path parts contain the album title (exactly, or with brackets stripped), the
/// artist appears in the path or in at least half the file names, it has at
/// least `minimum_files` unlocked audio files of one format, and its peer is not
/// blocked or over the queue limit. The format is the folder's majority
/// extension (FLAC wins ties). Ordering: lossless, free slot, shorter queue,
/// more files, faster peer, then user and folder name.
#[must_use]
pub fn candidates_from_responses(
    responses: &[SearchResponse],
    artist: &str,
    album: &str,
    settings: &SlskdSettings,
) -> Vec<Candidate> {
    let artist_n = norm(artist);
    let album_exact = norm(album);
    let album_loose = norm(&strip_brackets(album));
    if album_exact.is_empty() {
        return Vec::new();
    }
    let blocked = settings.blocked();
    let mut groups: BTreeMap<(String, String), (&SearchResponse, Vec<&ResponseFile>)> =
        BTreeMap::new();
    for r in responses {
        if blocked.contains(&r.username.to_lowercase())
            || (settings.maximum_queue_length > 0 && r.queue_length > settings.maximum_queue_length)
        {
            continue;
        }
        for f in &r.files {
            if f.is_locked || !AUDIO_EXTENSIONS.contains(&extension(&f.filename).as_str()) {
                continue;
            }
            let (dir, _) = split_dir(&f.filename);
            groups
                .entry((r.username.clone(), dir.to_string()))
                .or_insert_with(|| (r, Vec::new()))
                .1
                .push(f);
        }
    }

    let mut out = Vec::new();
    for ((user, dir), (resp, files)) in groups {
        let parts: Vec<&str> = dir.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        let tail = parts[parts.len().saturating_sub(2)..].join(" ");
        let tail_n = norm(&tail);
        let album_ok = has_words(&tail_n, &album_exact)
            || (!album_loose.is_empty() && has_words(&tail_n, &album_loose));
        if !album_ok {
            continue;
        }
        let artist_ok = artist_n.is_empty()
            || has_words(&norm(&dir), &artist_n)
            || files
                .iter()
                .filter(|f| has_words(&norm(split_dir(&f.filename).1), &artist_n))
                .count()
                * 2
                >= files.len();
        if !artist_ok {
            continue;
        }
        // The folder's format: majority extension, FLAC on a tie.
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for f in &files {
            *counts.entry(extension(&f.filename)).or_default() += 1;
        }
        let Some(ext) = counts
            .iter()
            .max_by_key(|(e, n)| (**n, e.as_str() == "flac"))
            .map(|(e, _)| e.clone())
        else {
            continue;
        };
        let mut chosen: Vec<&ResponseFile> = files
            .into_iter()
            .filter(|f| extension(&f.filename) == ext)
            .collect();
        if chosen.len() < settings.minimum_files.max(1) {
            continue;
        }
        chosen.sort_by(|a, b| a.filename.cmp(&b.filename));
        out.push(Candidate {
            format: format_label(&chosen, &ext),
            lossless: ext == "flac",
            size: chosen.iter().map(|f| f.size).sum(),
            free_slot: resp.has_free_upload_slot,
            queue_length: resp.queue_length,
            upload_speed: resp.upload_speed,
            target: SlskdTarget {
                username: user,
                directory: dir,
                files: chosen
                    .iter()
                    .map(|f| SlskdFile {
                        filename: f.filename.clone(),
                        size: f.size,
                    })
                    .collect(),
            },
        });
    }
    out.sort_by(|a, b| {
        b.lossless
            .cmp(&a.lossless)
            .then(b.free_slot.cmp(&a.free_slot))
            .then(a.queue_length.cmp(&b.queue_length))
            .then(b.target.files.len().cmp(&a.target.files.len()))
            .then(b.upload_speed.cmp(&a.upload_speed))
            .then(a.target.username.cmp(&b.target.username))
            .then(a.target.directory.cmp(&b.target.directory))
    });
    out.truncate(settings.max_releases.max(1));
    out
}

// ----------------------------------------------------------------- indexer

/// The Soulseek "indexer": searches slskd for an album and returns releases.
pub struct SlskdIndexer {
    id: IndexerId,
    name: String,
    api: SlskdApi,
}

impl SlskdIndexer {
    /// Over the production HTTP transport (a longer timeout than the default,
    /// since slskd answers a poll quickly but a slow box should not abort one).
    #[must_use]
    pub fn new(id: IndexerId, name: impl Into<String>, settings: SlskdSettings) -> Self {
        Self::with_transport(
            id,
            name,
            settings,
            Box::new(ReqwestTransport::with_timeout(Duration::from_secs(30))),
        )
    }

    /// Over a caller-supplied transport.
    #[must_use]
    pub fn with_transport(
        id: IndexerId,
        name: impl Into<String>,
        settings: SlskdSettings,
        transport: Box<dyn HttpTransport>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            api: SlskdApi::new(settings, transport),
        }
    }
}

#[async_trait]
impl Indexer for SlskdIndexer {
    type Error = DownloadError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, terms: &SearchTerms) -> Result<Vec<Release>, DownloadError> {
        let id = |k: &str| {
            terms
                .ids
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let (Some(artist), Some(album)) = (id("artist"), id("album")) else {
            // Soulseek is searched by names; without them there is nothing to ask.
            return Ok(Vec::new());
        };
        let mut queries = vec![norm(&format!("{artist} {album}"))];
        let loose = norm(&format!("{artist} {}", strip_brackets(&album)));
        if !loose.is_empty() && !queries.contains(&loose) {
            queries.push(loose);
        }
        for q in queries {
            let responses = self.api.search(&q).await?;
            let found = candidates_from_responses(&responses, &artist, &album, &self.api.settings);
            if !found.is_empty() {
                return Ok(found
                    .into_iter()
                    .map(|c| Release {
                        indexer_id: self.id,
                        title: format!("{artist} - {album} [{}]", c.format),
                        download_url: c.target.to_url(),
                        guid: Some(format!(
                            "slskd:{}:{}",
                            c.target.username, c.target.directory
                        )),
                        protocol: Protocol::Torrent,
                        size: Some(c.size),
                        seeders: None,
                        indexer_flags: Vec::new(),
                    })
                    .collect());
            }
        }
        Ok(Vec::new())
    }

    async fn latest(&self) -> Result<Vec<Release>, DownloadError> {
        // Soulseek has no feed.
        Ok(Vec::new())
    }
}

// ------------------------------------------------------------------ client

/// The Soulseek download client.
pub struct SlskdClient {
    name: String,
    api: SlskdApi,
}

impl SlskdClient {
    /// Over the production HTTP transport.
    #[must_use]
    pub fn new(name: impl Into<String>, settings: SlskdSettings) -> Self {
        Self::with_transport(name, settings, Box::new(ReqwestTransport::new()))
    }

    /// Over a caller-supplied transport.
    #[must_use]
    pub fn with_transport(
        name: impl Into<String>,
        settings: SlskdSettings,
        transport: Box<dyn HttpTransport>,
    ) -> Self {
        Self {
            name: name.into(),
            api: SlskdApi::new(settings, transport),
        }
    }

    /// Where a target's files land: `<root>/<user>/<folder>`, else
    /// `<root>/<folder>` when only that exists.
    #[must_use]
    pub fn content_dir(&self, target: &SlskdTarget) -> PathBuf {
        let root = Path::new(&self.api.settings.download_root);
        let folder = target.folder_name();
        let with_user = root.join(&target.username).join(folder);
        let flat = root.join(folder);
        if !with_user.exists() && flat.exists() {
            flat
        } else {
            with_user
        }
    }

    /// The richer status the trait projects.
    ///
    /// The status of a grab only moves forward. slskd's listing is large, polled
    /// from several places (the run, the reconcile sweep, the queue view) and
    /// sometimes unreadable, empty or missing files that were listed a moment
    /// ago; each of those used to read as "queued" and made the queue item flap.
    /// So what was seen is remembered per grab (process wide, since the queue
    /// resolves a fresh client per call): once any file progressed or succeeded
    /// the grab never reports `Queued` again, bytes and finished files never go
    /// back, and a read that fails or comes back empty keeps the last status.
    ///
    /// # Errors
    /// [`DownloadError::NotFound`] when slskd has none of the files (and, after
    /// a grab was seen, has had none for [`EMPTY_LISTING_GRACE`] polls running);
    /// a read error when nothing is known about the grab yet.
    pub async fn progress(&self, download_id: &str) -> Result<DownloadProgress, DownloadError> {
        let target = SlskdTarget::from_download_id(download_id)
            .ok_or_else(|| DownloadError::NotFound(download_id.to_string()))?;
        let transfers = match self.api.transfers_of(&target).await {
            Ok(t) => t,
            // A failed or slow read is not news; auth errors are real.
            Err(e @ DownloadError::Auth(_)) => return Err(e),
            Err(e) => {
                return match seen_last(download_id) {
                    Some(last) => Ok(last),
                    None => Err(e),
                };
            }
        };
        if transfers.is_empty() {
            return match seen_empty(download_id) {
                Some(last) => Ok(last),
                None => Err(DownloadError::NotFound(download_id.to_string())),
            };
        }
        let total = target.files.len();
        let done = |t: &Transfer| t.state.contains("Succeeded");
        let ended = |t: &Transfer| t.state.starts_with("Completed");
        let failed: Vec<&Transfer> = transfers.iter().filter(|t| ended(t) && !done(t)).collect();
        let active = transfers.iter().any(|t| {
            t.bytes_transferred > 0
                || done(t)
                || t.state.contains("InProgress")
                || t.state.contains("Initializing")
        });

        let mut memory = memory();
        let seen = memory.entry(download_id.to_string()).or_default();
        seen.empties = 0;
        seen.progressed |= active;
        for t in &transfers {
            let entry = seen.files.entry(t.filename.clone()).or_default();
            entry.0 = entry.0.max(t.bytes_transferred);
            if done(t) {
                entry.1 = true;
            }
        }
        let succeeded = target
            .files
            .iter()
            .filter(|f| seen.files.get(&f.filename).is_some_and(|e| e.1))
            .count();
        let wanted_bytes: u64 = target.files.iter().map(|f| f.size).sum();
        let got: u64 = target
            .files
            .iter()
            .map(|f| match seen.files.get(&f.filename) {
                Some((_, true)) => f.size,
                Some((bytes, false)) => (*bytes).min(f.size),
                None => 0,
            })
            .sum();
        let progress = if wanted_bytes == 0 {
            0.0
        } else {
            (got as f64 / wanted_bytes as f64).clamp(0.0, 1.0)
        };
        let mut p = DownloadProgress {
            state: DownloadState::Queued,
            progress: progress.max(seen.last.as_ref().map_or(0.0, |l| l.progress)),
            content_path: None,
            ratio: None,
            seeding_time_secs: None,
            peers: None,
            error_string: None,
            category: None,
        };
        if let Some(bad) = failed.first() {
            p.state = DownloadState::Failed;
            p.error_string = Some(format!(
                "{} of {total} files failed: {}{}",
                failed.len(),
                bad.state,
                bad.exception
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            ));
        } else if succeeded == total {
            p.state = DownloadState::Completed;
            p.progress = 1.0;
            p.content_path = Some(self.content_dir(&target).to_string_lossy().into_owned());
        } else if seen.progressed {
            p.state = DownloadState::Downloading;
        }
        seen.last = Some(p.clone());
        drop(memory);
        Ok(p)
    }

    /// Cancel and clear the transfers; optionally delete the downloaded files.
    ///
    /// # Errors
    /// Transport failures reaching slskd.
    pub async fn remove_download(
        &self,
        download_id: &str,
        delete_data: bool,
    ) -> Result<(), DownloadError> {
        let Some(target) = SlskdTarget::from_download_id(download_id) else {
            return Err(DownloadError::NotFound(download_id.to_string()));
        };
        for t in self.api.transfers_of(&target).await? {
            let path = format!(
                "transfers/downloads/{}/{}?remove=true",
                pct(&target.username),
                pct(&t.id)
            );
            match self.api.call(self.api.req("DELETE", &path)).await {
                Ok(_) | Err(DownloadError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        forget(download_id);
        if delete_data {
            self.delete_files(&target);
        }
        Ok(())
    }

    /// Delete only the files this grab fetched, then the folder if it is empty.
    /// Never leaves the download root.
    fn delete_files(&self, target: &SlskdTarget) {
        let root = Path::new(&self.api.settings.download_root);
        let dir = self.content_dir(target);
        let safe = dir.starts_with(root)
            && dir != root
            && dir.components().all(|c| !matches!(c, Component::ParentDir));
        if !safe {
            return;
        }
        for f in &target.files {
            let base = split_dir(&f.filename).1;
            if !base.is_empty() && base != "." && base != ".." {
                let _ = std::fs::remove_file(dir.join(base));
            }
        }
        let _ = std::fs::remove_dir(&dir);
    }
}

#[async_trait]
impl DownloadClient for SlskdClient {
    type Error = DownloadError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn add(&self, grab: &GrabRequest) -> Result<String, DownloadError> {
        let target = SlskdTarget::from_url(&grab.release.download_url).ok_or_else(|| {
            DownloadError::Config(format!(
                "not a slskd release link: {}",
                grab.release
                    .download_url
                    .chars()
                    .take(40)
                    .collect::<String>()
            ))
        })?;
        if target.files.is_empty() {
            return Err(DownloadError::Config("slskd release names no files".into()));
        }
        self.api.enqueue(&target).await?;
        // A re-grab of the same files is a new grab: forget what the last one saw.
        forget(&target.download_id());
        Ok(target.download_id())
    }

    async fn status(&self, download_id: &str) -> Result<DownloadStatus, DownloadError> {
        Ok(self.progress(download_id).await?.to_core_status())
    }

    async fn remove(&self, download_id: &str, delete_data: bool) -> Result<(), DownloadError> {
        self.remove_download(download_id, delete_data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SlskdSettings {
        SlskdSettings::from_json(&json!({"baseUrl": "http://x", "apiKey": "k"})).unwrap()
    }

    fn file(name: &str, size: u64, rate: Option<u32>) -> ResponseFile {
        ResponseFile {
            filename: name.into(),
            size,
            bit_rate: rate,
            ..Default::default()
        }
    }

    fn resp(user: &str, files: Vec<ResponseFile>) -> SearchResponse {
        SearchResponse {
            username: user.into(),
            files,
            has_free_upload_slot: true,
            ..Default::default()
        }
    }

    #[test]
    fn base64_round_trips() {
        for s in ["", "a", "ab", "abc", "abcd", "Ünï\\cødé/ø"] {
            assert_eq!(b64_decode(&b64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }

    #[test]
    fn a_target_survives_the_url_and_the_download_id() {
        let t = SlskdTarget {
            username: "bob".into(),
            directory: "@@x\\Music\\Radiohead\\OK Computer".into(),
            files: vec![SlskdFile {
                filename: "@@x\\Music\\Radiohead\\OK Computer\\01.flac".into(),
                size: 5,
            }],
        };
        assert_eq!(SlskdTarget::from_url(&t.to_url()).unwrap(), t);
        assert_eq!(SlskdTarget::from_download_id(&t.download_id()).unwrap(), t);
        assert_eq!(t.folder_name(), "OK Computer");
        assert!(SlskdTarget::from_url("magnet:?xt=x").is_none());
    }

    #[test]
    fn settings_need_a_url_and_a_key_and_can_opt_out() {
        assert!(SlskdSettings::from_json(&json!({"baseUrl": "http://x"})).is_err());
        assert!(SlskdSettings::from_json(&json!({"apiKey": "k"})).is_err());
        assert!(
            SlskdSettings::from_json(&json!({"baseUrl": "u", "apiKey": "k", "native": false}))
                .is_err()
        );
        let s = settings();
        assert_eq!(s.download_root, DEFAULT_DOWNLOAD_ROOT);
    }

    #[test]
    fn flac_beats_mp3_and_a_full_folder_beats_a_partial_one() {
        let dir = "@@a\\Radiohead\\OK Computer (1997)";
        let flac: Vec<_> = (1..=4)
            .map(|i| file(&format!("{dir}\\0{i} - Track.flac"), 100, None))
            .collect();
        let mp3: Vec<_> = (1..=12)
            .map(|i| file(&format!("{dir}\\{i:02} - Track.mp3"), 10, Some(320)))
            .collect();
        let rs = vec![resp("mp3guy", mp3), resp("flacguy", flac)];
        let c = candidates_from_responses(&rs, "Radiohead", "OK Computer", &settings());
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].target.username, "flacguy");
        assert_eq!(c[0].format, "FLAC");
        assert_eq!(c[1].format, "MP3 320");
    }

    #[test]
    fn wrong_album_wrong_artist_too_few_files_locked_and_blocked_are_dropped() {
        let mk = |dir: &str, n: usize| -> Vec<ResponseFile> {
            (1..=n)
                .map(|i| file(&format!("{dir}\\{i:02} - Song.flac"), 1, None))
                .collect()
        };
        let mut locked = mk("Radiohead\\OK Computer", 3);
        locked.iter_mut().for_each(|f| f.is_locked = true);
        let rs = vec![
            resp("wrongalbum", mk("Radiohead\\Kid A", 5)),
            resp("wrongartist", mk("Coldplay\\OK Computer", 5)),
            resp("tiny", mk("Radiohead\\OK Computer", 2)),
            resp("locked", locked),
            resp("blocked", mk("Radiohead\\OK Computer", 5)),
            resp("good", mk("Radiohead\\OK Computer", 5)),
        ];
        let mut s = settings();
        s.blocked_users = json!("Blocked, other");
        let c = candidates_from_responses(&rs, "Radiohead", "OK Computer", &s);
        assert_eq!(
            c.iter()
                .map(|c| c.target.username.as_str())
                .collect::<Vec<_>>(),
            ["good"]
        );
    }

    #[test]
    fn edition_suffix_and_punctuation_are_set_aside() {
        let rs = vec![resp(
            "u",
            (1..=3)
                .map(|i| {
                    file(
                        &format!("Guns N' Roses - Appetite for Destruction\\{i} x.flac"),
                        1,
                        None,
                    )
                })
                .collect(),
        )];
        let c = candidates_from_responses(
            &rs,
            "Guns N' Roses",
            "Appetite for Destruction (Deluxe Edition)",
            &settings(),
        );
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn a_folder_of_mixed_formats_keeps_its_majority_format() {
        let d = "Artist\\Album";
        let mut files: Vec<_> = (1..=4)
            .map(|i| file(&format!("{d}\\{i}.mp3"), 1, Some(192)))
            .collect();
        files.push(file(&format!("{d}\\bonus.flac"), 1, None));
        let c = candidates_from_responses(&[resp("u", files)], "Artist", "Album", &settings());
        assert_eq!(c[0].format, "MP3 192");
        assert_eq!(c[0].target.files.len(), 4);
    }

    #[test]
    fn ordering_is_deterministic_and_capped() {
        let mk = |u: &str| {
            resp(
                u,
                (1..=3)
                    .map(|i| file(&format!("A\\B\\{i}.flac"), 1, None))
                    .collect(),
            )
        };
        let mut s = settings();
        s.max_releases = 2;
        let c = candidates_from_responses(&[mk("zed"), mk("amy"), mk("bob")], "A", "B", &s);
        assert_eq!(
            c.iter()
                .map(|c| c.target.username.as_str())
                .collect::<Vec<_>>(),
            ["amy", "bob"]
        );
    }
}

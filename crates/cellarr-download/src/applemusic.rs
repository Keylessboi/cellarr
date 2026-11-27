//! Native Apple Music support: the catalogue search that turns Apple Music
//! albums into [`Release`]s, and the download client that hands an album's track
//! list to the `applemusic-decryptor` service and follows its job.
//!
//! This replaces the AppleMusicarr Lidarr plugin (a .NET DLL run in the plugin
//! sidecar) and speaks the same protocol, taken from the plugin's own source:
//!
//! - [`AppleMusicIndexer`] (an [`Indexer`]) searches
//!   `GET {api}/catalog/{storefront}/search?term=..&types=albums&limit=..` with the
//!   developer token as a bearer, in tiers (`artist album`, `artist`, `album`),
//!   keeps only albums whose title equals the wanted one ([`select_matching`]),
//!   drops far-future releases, and offers one release per quality tier
//!   (AAC 256, ALAC, Atmos). The release's `download_url` is the plugin's
//!   `applemusicarr://album/<id>?quality=<key>` link, so a grab needs no state
//!   kept by the indexer.
//! - [`AppleMusicClient`] (a [`DownloadClient`]) looks the album up
//!   (`GET {api}/catalog/{storefront}/albums/{id}?include=tracks`), posts the
//!   track list to the decryptor (`POST /v1/download`, answer `{job_id}`), and
//!   follows `GET /v1/job/{id}`. The download id carries everything needed to
//!   follow the job, so status survives a restart.
//!
//! # What this does not do
//!
//! Apple Music audio is FairPlay-protected. **All decryption stays in the
//! decryptor service**; this module only searches the public catalogue and
//! orchestrates that service's jobs, exactly as the plugin did. It does not sign
//! developer tokens from a `.p8` key and does not read the plugin's environment
//! variables: a row without a ready-made `developerToken` is not native and takes
//! the sidecar plugin path.
//!
//! # Where finished files are
//!
//! The decryptor writes under its `OUTPUT_DIR` and reports the album folder
//! relative to it. cellarr rebases that onto [`AppleMusicSettings::download_path`]
//! (the same volume, production: [`DEFAULT_DOWNLOAD_DIR`]).
//!
//! # Secrets
//!
//! The developer token and Music-User-Token never appear in `Debug` output, in
//! error text, or in logs; [`AppleMusicSettings`]'s `Debug` redacts them.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cellarr_core::{
    DownloadClient, DownloadState, DownloadStatus, GrabRequest, Indexer, IndexerId, Protocol,
    Release, SearchTerms,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::DownloadError;
use crate::http::{HttpRequest, HttpTransport, ReqwestTransport};
use crate::lifecycle::DownloadProgress;
use crate::slskd::{b64_decode, b64_encode, pct};

/// Where the decryptor's finished albums live when the settings do not say
/// (production's fixed directory).
pub const DEFAULT_DOWNLOAD_DIR: &str = "/data/torrents/apple";

/// The public Apple Music catalogue API.
pub const DEFAULT_API_BASE: &str = "https://api.music.apple.com/v1/";

/// The web player's own API, which is the only host that accepts its token.
pub const AMP_API_BASE: &str = "https://amp-api.music.apple.com/v1/";

const HOMEPAGE: &str = "https://music.apple.com";
const WEB_PLAYER_TEAM_ID: &str = "AMPWebPlay";

/// The scheme of an Apple Music release's `download_url`.
pub const APPLE_SCHEME: &str = "applemusicarr://";

/// The protocol name the Lidarr face and the client router use for Apple Music.
pub const APPLE_PROTOCOL: &str = "AppleMusicarrDownloadProtocol";

const ID_PREFIX: &str = "apple-";
const GUID_PREFIX: &str = "applemusicarr";
const FALLBACK_TRACK_SECS: u64 = 240;

/// Whether a config row's `kind` is an Apple Music one. A migrated Lidarr row
/// carries the plugin's implementation name lowercased
/// (`applemusicarrindexer`, `applemusicarrdownloadclient`); a row made through
/// cellarr's own face carries `applemusicarr`.
#[must_use]
pub fn is_apple_kind(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "applemusicarr" | "applemusicarrindexer" | "applemusicarrdownloadclient"
    )
}

/// Whether a release's link is an Apple Music one.
#[must_use]
pub fn is_apple_url(url: &str) -> bool {
    url.len() >= APPLE_SCHEME.len() && url[..APPLE_SCHEME.len()].eq_ignore_ascii_case(APPLE_SCHEME)
}

// ---------------------------------------------------------------- quality

/// The quality tiers one album is offered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleQuality {
    /// AAC 256 kbps (what a subscription streams).
    Aac256,
    /// ALAC lossless (needs lossless on the account).
    Alac,
    /// Dolby Atmos (lossy E-AC-3 + JOC).
    Atmos,
}

impl AppleQuality {
    /// Every tier, ascending.
    pub const ALL: [Self; 3] = [Self::Aac256, Self::Alac, Self::Atmos];

    /// The key carried in the link and handed to the decryptor.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Aac256 => "aac256",
            Self::Alac => "alac",
            Self::Atmos => "atmos",
        }
    }

    /// Read a key (`aac256`/`aac`/`alac`/`atmos`, any case).
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "aac256" | "aac" => Some(Self::Aac256),
            "alac" => Some(Self::Alac),
            "atmos" => Some(Self::Atmos),
            _ => None,
        }
    }

    /// The bracket tokens appended to the release title. The first is the codec
    /// the quality parser ranks by; Atmos deliberately parses as AAC VBR, which
    /// is what a file tagged from an E-AC-3 track reads back as.
    #[must_use]
    pub fn title_tokens(self) -> &'static [&'static str] {
        match self {
            Self::Aac256 => &["AAC", "256"],
            Self::Alac => &["ALAC", "LOSSLESS"],
            Self::Atmos => &["AAC", "VBR", "ATMOS"],
        }
    }

    /// Nominal bits per second, used only to estimate a release's size.
    #[must_use]
    pub fn bits_per_second(self) -> u64 {
        match self {
            Self::Aac256 => 256_000,
            Self::Alac => 2_304_000,
            Self::Atmos => 768_000,
        }
    }
}

// -------------------------------------------------------------- the link

/// Parse an `applemusicarr://album/<id>?quality=<key>` link, or the guid form
/// `applemusicarr:<id>:<key>` (optionally behind a `<indexer id>_` prefix, which
/// Lidarr adds when it persists a release).
#[must_use]
pub fn parse_release_link(value: &str) -> Option<(String, AppleQuality)> {
    let text = value.trim();
    let lower = text.to_ascii_lowercase();
    let url_prefix = format!("{APPLE_SCHEME}album/");
    if let Some(at) = lower.find(&url_prefix) {
        let rest = &text[at + url_prefix.len()..];
        let (id_part, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        let id = unescape(id_part.trim_matches('/'));
        if id.is_empty() {
            return None;
        }
        let quality = query
            .and_then(|q| {
                q.split('&')
                    .filter_map(|p| p.split_once('='))
                    .find(|(k, _)| k.eq_ignore_ascii_case("quality"))
                    .and_then(|(_, v)| AppleQuality::from_key(&unescape(v)))
            })
            .unwrap_or(AppleQuality::Aac256);
        return Some((id, quality));
    }
    let mut body = text;
    if let Some((head, tail)) = body.split_once('_') {
        if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) {
            body = tail;
        }
    }
    let stripped = body
        .get(..GUID_PREFIX.len() + 1)
        .filter(|p| p[..GUID_PREFIX.len()].eq_ignore_ascii_case(GUID_PREFIX) && p.ends_with(':'))
        .map(|_| &body[GUID_PREFIX.len() + 1..])?;
    let parts: Vec<&str> = stripped.split(':').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    Some((
        parts[0].to_string(),
        AppleQuality::from_key(parts[1]).unwrap_or(AppleQuality::Aac256),
    ))
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn release_url(album_id: &str, quality: AppleQuality) -> String {
    format!(
        "{APPLE_SCHEME}album/{}?quality={}",
        pct(album_id),
        quality.key()
    )
}

fn release_guid(album_id: &str, quality: AppleQuality) -> String {
    format!("{GUID_PREFIX}:{album_id}:{}", quality.key())
}

// ---------------------------------------------------------------- settings

/// Connection and behaviour settings, shared by the indexer and the client (a
/// row carries only the half it needs). The names are the plugin's own, so a
/// migrated Lidarr row deserialises as it is.
///
/// `Debug` redacts both tokens.
#[derive(Clone, Deserialize)]
pub struct AppleMusicSettings {
    /// The Apple Music API base (the plugin stores its default here).
    #[serde(alias = "baseUrl", alias = "BaseUrl", alias = "base_url", default)]
    pub base_url: String,
    /// The MusicKit developer token (a JWT). Secret.
    #[serde(
        alias = "developerToken",
        alias = "DeveloperToken",
        alias = "developer_token",
        default
    )]
    pub developer_token: String,
    /// The Music-User-Token, needed to download. Secret.
    #[serde(
        alias = "musicUserToken",
        alias = "MusicUserToken",
        alias = "music_user_token",
        default
    )]
    pub music_user_token: String,
    /// Two-letter storefront.
    #[serde(alias = "Storefront", default = "default_storefront")]
    pub storefront: String,
    /// Ignore albums releasing further than this many days ahead (`null` = no window).
    #[serde(
        alias = "earlyReleaseLimit",
        alias = "EarlyReleaseLimit",
        default = "default_early"
    )]
    pub early_release_limit: Option<i64>,
    /// Albums requested per search query, 1 to 100.
    #[serde(
        alias = "searchLimit",
        alias = "SearchLimit",
        default = "default_limit"
    )]
    pub search_limit: i64,
    /// The decryptor's base URL.
    #[serde(
        alias = "decryptorUrl",
        alias = "DecryptorUrl",
        alias = "decryptor_url",
        default
    )]
    pub decryptor_url: String,
    /// Where the decryptor's albums are, as this process sees them.
    #[serde(
        alias = "downloadPath",
        alias = "DownloadPath",
        alias = "download_path",
        default
    )]
    pub download_path: String,
    /// The quality used when a release's link names none (a number or a key).
    #[serde(
        alias = "preferredQuality",
        alias = "PreferredQuality",
        default = "default_quality_value"
    )]
    pub preferred_quality: Value,
    /// How long to wait for one album, in minutes.
    #[serde(
        alias = "decryptorTimeoutMinutes",
        alias = "DecryptorTimeoutMinutes",
        default = "default_timeout_min"
    )]
    pub decryptor_timeout_minutes: i64,
}

fn default_storefront() -> String {
    "us".to_string()
}
#[allow(clippy::unnecessary_wraps)]
const fn default_early() -> Option<i64> {
    Some(14)
}
const fn default_limit() -> i64 {
    25
}
fn default_quality_value() -> Value {
    json!(0)
}
const fn default_timeout_min() -> i64 {
    180
}

impl fmt::Debug for AppleMusicSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redact = |s: &str| {
            if s.is_empty() {
                "<unset>"
            } else {
                "<redacted>"
            }
        };
        f.debug_struct("AppleMusicSettings")
            .field("base_url", &self.base_url)
            .field("developer_token", &redact(&self.developer_token))
            .field("music_user_token", &redact(&self.music_user_token))
            .field("storefront", &self.storefront)
            .field("early_release_limit", &self.early_release_limit)
            .field("search_limit", &self.search_limit)
            .field("decryptor_url", &self.decryptor_url)
            .field("download_path", &self.download_path)
            .field("preferred_quality", &self.preferred_quality)
            .field("decryptor_timeout_minutes", &self.decryptor_timeout_minutes)
            .finish()
    }
}

impl AppleMusicSettings {
    fn parse(v: &Value) -> Result<Self, String> {
        if v.get("native").and_then(Value::as_bool) == Some(false) {
            return Err("native=false (plugin path requested)".into());
        }
        let mut s: Self = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        s.developer_token = s.developer_token.trim().to_string();
        s.music_user_token = s.music_user_token.trim().to_string();
        s.storefront = s.storefront.trim().to_ascii_lowercase();
        if s.storefront.is_empty() {
            s.storefront = default_storefront();
        }
        if s.storefront.len() != 2 || !s.storefront.bytes().all(|b| b.is_ascii_lowercase()) {
            return Err("storefront must be a two-letter country code".into());
        }
        s.search_limit = s.search_limit.clamp(1, 100);
        s.decryptor_timeout_minutes = s.decryptor_timeout_minutes.clamp(1, 1440);
        if s.download_path.trim().is_empty() {
            s.download_path = DEFAULT_DOWNLOAD_DIR.to_string();
        }
        Ok(s)
    }

    /// Read the settings of an **indexer** row, or say why it cannot drive the
    /// native indexer (no developer token, or `native: false`). The caller falls
    /// back to the plugin path on `Err`.
    ///
    /// # Errors
    /// A readable reason; it never contains a token.
    pub fn from_json_indexer(v: &Value) -> Result<Self, String> {
        let s = Self::parse(v)?;
        if s.developer_token.is_empty() {
            return Err("no developerToken".into());
        }
        Ok(s)
    }

    /// Read the settings of a **download client** row: it needs the developer
    /// token (to look the album's tracks up), the Music-User-Token and the
    /// decryptor's URL.
    ///
    /// # Errors
    /// A readable reason; it never contains a token.
    pub fn from_json_client(v: &Value) -> Result<Self, String> {
        let s = Self::parse(v)?;
        if s.developer_token.is_empty() {
            return Err("no developerToken".into());
        }
        if s.music_user_token.is_empty() {
            return Err("no musicUserToken".into());
        }
        let u = s.decryptor_url.trim();
        if !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err("decryptorUrl is not an http(s) URL".into());
        }
        Ok(s)
    }

    fn preferred(&self) -> AppleQuality {
        match &self.preferred_quality {
            Value::Number(n) => match n.as_i64() {
                Some(1) => AppleQuality::Alac,
                Some(2) => AppleQuality::Atmos,
                _ => AppleQuality::Aac256,
            },
            Value::String(s) => AppleQuality::from_key(s).unwrap_or(AppleQuality::Aac256),
            _ => AppleQuality::Aac256,
        }
    }
}

// --------------------------------------------------------- developer token

fn jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = b64_decode(parts[1].trim_end_matches('='))?;
    serde_json::from_slice(&bytes).ok()
}

/// A readable problem with a developer token, judged locally (Apple answers a
/// bad one with a bare 401), or `None` when it looks usable. Never includes the
/// token itself.
#[must_use]
pub fn diagnose_developer_token(token: &str) -> Option<String> {
    let value = token.trim();
    if value.is_empty() {
        return Some("No developer token is configured.".into());
    }
    if !value.starts_with("eyJ") {
        return Some(
            "The developer token does not look like a JWT (it must start with 'eyJ'). \
             The media-user-token cookie value belongs in the Music-User-Token field."
                .into(),
        );
    }
    let segments = value.split('.').count();
    if segments != 3 {
        return Some(format!(
            "The developer token has {segments} dot-separated segments; a JWT has 3. \
             It was most likely truncated when copied."
        ));
    }
    let Some(payload) = jwt_payload(value) else {
        return Some("The developer token could not be decoded; it is probably truncated.".into());
    };
    if let Some(exp) = payload.get("exp").and_then(Value::as_i64) {
        let now = i64::try_from(unix_now()).unwrap_or(i64::MAX);
        if exp <= now {
            return Some(format!(
                "The developer token expired on {}. Get a fresh one.",
                civil_from_days(exp.div_euclid(86_400))
            ));
        }
    }
    None
}

fn is_web_player_token(token: &str) -> bool {
    jwt_payload(token)
        .and_then(|p| p.get("iss").and_then(Value::as_str).map(str::to_string))
        .is_some_and(|iss| iss == WEB_PLAYER_TEAM_ID)
}

// -------------------------------------------------------------- date math

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `YYYY-MM-DD` for a day count since 1970-01-01.
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The latest release date (`YYYY-MM-DD`) still inside an `days`-day window from
/// `now_secs`.
fn cutoff_date(now_secs: u64, days: i64) -> String {
    let today = i64::try_from(now_secs / 86_400).unwrap_or(0);
    civil_from_days(today + days)
}

// ------------------------------------------------------------- wire models

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SearchEnvelope {
    results: SearchResults,
}
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SearchResults {
    albums: DataList<AlbumDto>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct AlbumEnvelope {
    data: Vec<AlbumDto>,
}
#[derive(Debug, Deserialize)]
#[serde(default)]
struct DataList<T> {
    data: Vec<T>,
}
impl<T> Default for DataList<T> {
    fn default() -> Self {
        Self { data: Vec::new() }
    }
}
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct AlbumDto {
    id: String,
    attributes: Option<AlbumAttrs>,
    relationships: Option<AlbumRels>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct AlbumAttrs {
    name: String,
    artist_name: String,
    release_date: Option<String>,
    track_count: u32,
    url: Option<String>,
    audio_traits: Option<Vec<String>>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct AlbumRels {
    tracks: DataList<TrackDto>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TrackDto {
    id: String,
    attributes: Option<TrackAttrs>,
}
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct TrackAttrs {
    name: String,
    track_number: u32,
    disc_number: u32,
    duration_in_millis: u64,
}

/// One track of an album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleTrack {
    /// Apple's track id.
    pub id: String,
    /// Title.
    pub title: String,
    /// Track number on its disc.
    pub track_number: u32,
    /// Disc number.
    pub disc_number: u32,
    /// Duration in seconds (0 when Apple sent none).
    pub duration_secs: u64,
}

/// One album from the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleAlbum {
    /// Apple's album id.
    pub id: String,
    /// Title.
    pub title: String,
    /// Artist credit.
    pub artist: String,
    /// `YYYY-MM-DD`, or empty when Apple sent none (or only a year).
    pub release_date: String,
    /// Reported track count.
    pub track_count: u32,
    /// The album page.
    pub url: String,
    /// The tracks (empty for a search hit; present after a lookup).
    pub tracks: Vec<AppleTrack>,
    /// `Some(true)` when Apple lists an Atmos master, `Some(false)` when it sent
    /// its capability list without one, `None` when it sent no list at all.
    pub has_atmos: Option<bool>,
}

impl AppleAlbum {
    fn from_dto(dto: AlbumDto) -> Option<Self> {
        if dto.id.trim().is_empty() {
            return None;
        }
        let a = dto.attributes.unwrap_or_default();
        let tracks: Vec<AppleTrack> = dto
            .relationships
            .map(|r| r.tracks.data)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| {
                let at = t.attributes?;
                Some(AppleTrack {
                    id: t.id,
                    title: at.name,
                    track_number: at.track_number,
                    disc_number: at.disc_number,
                    duration_secs: (at.duration_in_millis + 500) / 1000,
                })
            })
            .collect();
        let release_date = a
            .release_date
            .as_deref()
            .and_then(|d| d.get(..10))
            .filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-')
            .unwrap_or_default()
            .to_string();
        Some(Self {
            id: dto.id,
            title: a.name,
            artist: a.artist_name,
            release_date,
            track_count: if a.track_count > 0 {
                a.track_count
            } else {
                u32::try_from(tracks.len()).unwrap_or(0)
            },
            url: a.url.unwrap_or_default(),
            has_atmos: a
                .audio_traits
                .map(|t| t.iter().any(|x| x.to_lowercase().contains("atmos"))),
            tracks,
        })
    }

    /// Estimated length: the sum of the tracks' durations (an unreported one
    /// counts the average), else the reported count (at least one) times it.
    #[must_use]
    pub fn estimated_secs(&self) -> u64 {
        if self.tracks.is_empty() {
            return u64::from(self.track_count.max(1)) * FALLBACK_TRACK_SECS;
        }
        self.tracks
            .iter()
            .map(|t| {
                if t.duration_secs > 0 {
                    t.duration_secs
                } else {
                    FALLBACK_TRACK_SECS
                }
            })
            .sum()
    }
}

// ---------------------------------------------------------------- matching

fn fold_char(c: char, out: &mut String) {
    match c {
        '\u{300}'..='\u{36f}' => {}
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
        'ç' | 'ć' | 'č' => out.push('c'),
        'ď' | 'đ' => out.push('d'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
        'ğ' => out.push('g'),
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
        'ł' => out.push('l'),
        'ñ' | 'ń' | 'ň' => out.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
        'ř' => out.push('r'),
        'ś' | 'š' | 'ş' => out.push('s'),
        'ť' => out.push('t'),
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => out.push('u'),
        'ý' | 'ÿ' => out.push('y'),
        'ź' | 'ż' | 'ž' => out.push('z'),
        'ß' => out.push_str("ss"),
        'æ' => out.push_str("ae"),
        'œ' => out.push_str("oe"),
        other => out.push(other),
    }
}

fn strip_edition_suffix(value: &str, suffix: &str) -> String {
    for wrapper in [
        format!(" {suffix}"),
        format!(" ({suffix})"),
        format!(" [{suffix}]"),
    ] {
        if let Some(head) = value.strip_suffix(wrapper.as_str()) {
            let rest = head.trim_end_matches([' ', '-', '(', ')', '[', ']']);
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    value.to_string()
}

/// A title reduced for comparison: accents folded, lowercased, Apple's own
/// ` - Single` / ` - EP` suffix dropped, apostrophes deleted, every other run of
/// non-alphanumerics one space.
#[must_use]
pub fn comparable_title(title: &str) -> String {
    let mut folded = String::with_capacity(title.len());
    for c in title.chars().flat_map(char::to_lowercase) {
        fold_char(c, &mut folded);
    }
    let folded = strip_edition_suffix(&strip_edition_suffix(&folded, "single"), "ep");
    let mut out = String::with_capacity(folded.len());
    let mut pending = false;
    for c in folded.chars() {
        if matches!(c, '\'' | '\u{2019}' | '`' | '\u{b4}') {
            continue;
        }
        if c.is_alphanumeric() {
            if pending && !out.is_empty() {
                out.push(' ');
            }
            out.push(c);
            pending = false;
        } else {
            pending = true;
        }
    }
    out
}

/// Keep only the albums that are the requested one: equal titles after
/// [`comparable_title`]. Containment is deliberately not used (`Playa Grande
/// (Uproot Andy remix)` belongs to a different album). A blank request means an
/// artist search and keeps everything. No match is a valid, empty answer.
#[must_use]
pub fn select_matching(albums: Vec<AppleAlbum>, requested: Option<&str>) -> Vec<AppleAlbum> {
    let Some(wanted) = requested.map(comparable_title).filter(|w| !w.is_empty()) else {
        return albums;
    };
    albums
        .into_iter()
        .filter(|a| comparable_title(&a.title) == wanted)
        .collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The query tiers for one search, most specific first: `artist album`,
/// `artist`, `album`.
#[must_use]
pub fn search_tiers(artist: &str, album: &str) -> Vec<String> {
    let (a, b) = (collapse_ws(artist), collapse_ws(album));
    let mut tiers = Vec::new();
    if !a.is_empty() && !b.is_empty() {
        tiers.push(format!("{a} {b}"));
    }
    if !a.is_empty() {
        tiers.push(a.clone());
    }
    if !b.is_empty() && !a.eq_ignore_ascii_case(&b) {
        tiers.push(b);
    }
    tiers
}

/// The releases one album is offered as: one per quality tier it can serve.
#[must_use]
pub fn releases_for(album: &AppleAlbum, indexer_id: IndexerId) -> Vec<Release> {
    if album.id.trim().is_empty() {
        return Vec::new();
    }
    let artist = if album.artist.trim().is_empty() {
        "Unknown Artist"
    } else {
        album.artist.trim()
    };
    let title = if album.title.trim().is_empty() {
        "Unknown Album"
    } else {
        album.title.trim()
    };
    let secs = album.estimated_secs();
    AppleQuality::ALL
        .iter()
        .filter(|q| !(**q == AppleQuality::Atmos && album.has_atmos == Some(false)))
        .map(|q| {
            let tokens: Vec<String> = q.title_tokens().iter().map(|t| format!("[{t}]")).collect();
            Release {
                indexer_id,
                title: format!("{artist} - {title} {}", tokens.join(" ")),
                download_url: release_url(&album.id, *q),
                guid: Some(release_guid(&album.id, *q)),
                protocol: Protocol::Torrent,
                size: Some(secs * q.bits_per_second() / 8),
                seeders: None,
                indexer_flags: Vec::new(),
            }
        })
        .collect()
}

// --------------------------------------------------------------- Apple API

/// The Apple Music catalogue API over the shared transport seam.
pub struct AppleCatalogApi {
    settings: AppleMusicSettings,
    transport: Box<dyn HttpTransport>,
}

impl fmt::Debug for AppleCatalogApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppleCatalogApi")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl AppleCatalogApi {
    /// Over a caller-supplied transport (the test seam).
    #[must_use]
    pub fn new(settings: AppleMusicSettings, transport: Box<dyn HttpTransport>) -> Self {
        Self {
            settings,
            transport,
        }
    }

    /// The API base: the row's own, except that the default base is swapped for
    /// the web player's host when the token is the web player's (Apple refuses
    /// that token on the public host).
    fn base(&self) -> String {
        let configured = self.settings.base_url.trim();
        let base = if configured.is_empty() {
            DEFAULT_API_BASE
        } else {
            configured
        };
        let base = if base.trim_end_matches('/') == DEFAULT_API_BASE.trim_end_matches('/')
            && is_web_player_token(&self.settings.developer_token)
        {
            AMP_API_BASE
        } else {
            base
        };
        format!("{}/", base.trim_end_matches('/'))
    }

    fn req(&self, url: String) -> HttpRequest {
        let mut r = HttpRequest::new("GET", url)
            .header(
                "authorization",
                format!("Bearer {}", self.settings.developer_token),
            )
            .header("accept", "application/json");
        if is_web_player_token(&self.settings.developer_token) {
            r = r
                .header("origin", HOMEPAGE)
                .header("referer", format!("{HOMEPAGE}/"));
            if !self.settings.music_user_token.is_empty() {
                r = r.header(
                    "cookie",
                    format!("media-user-token={}", self.settings.music_user_token),
                );
            }
        }
        r
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: String,
    ) -> Result<Option<T>, DownloadError> {
        let resp = self.transport.send(self.req(url)).await?;
        match resp.status {
            200..=299 => serde_json::from_str(&resp.body)
                .map(Some)
                .map_err(|e| DownloadError::UnexpectedResponse(format!("apple music: {e}"))),
            404 => Ok(None),
            401 | 403 => {
                let why =
                    diagnose_developer_token(&self.settings.developer_token).unwrap_or_else(|| {
                        "The token parses correctly, so it has most likely been revoked, or it \
                         is signed for a team not entitled to this storefront."
                            .to_string()
                    });
                Err(DownloadError::Auth(format!(
                    "Apple Music rejected the developer token (HTTP {}). {why} \
                     [storefront tried: {}]",
                    resp.status, self.settings.storefront
                )))
            }
            s => Err(DownloadError::Api(format!(
                "apple music answered {s}: {}",
                resp.body.chars().take(200).collect::<String>()
            ))),
        }
    }

    /// Full-text album search; an empty list when Apple matches nothing.
    ///
    /// # Errors
    /// Transport, auth (a rejected developer token) or API failures.
    pub async fn search_albums(&self, term: &str) -> Result<Vec<AppleAlbum>, DownloadError> {
        if term.trim().is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}catalog/{}/search?term={}&types=albums&limit={}",
            self.base(),
            self.settings.storefront,
            pct(term),
            self.settings.search_limit
        );
        let env: Option<SearchEnvelope> = self.get_json(url).await?;
        Ok(env
            .map(|e| e.results.albums.data)
            .unwrap_or_default()
            .into_iter()
            .filter_map(AppleAlbum::from_dto)
            .collect())
    }

    /// The cheap check behind the Test button: a one-result catalogue search in
    /// the row's storefront, the same call a real search makes, which needs a
    /// valid developer token and nothing else.
    ///
    /// # Errors
    /// The setting at fault and why; never a token.
    pub async fn check(&self) -> Result<(), crate::lifecycle::CheckFailure> {
        use crate::lifecycle::CheckFailure;
        let url = format!(
            "{}catalog/{}/search?term=a&types=albums&limit=1",
            self.base(),
            self.settings.storefront
        );
        match self.get_json::<Value>(url).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(CheckFailure::new(
                "storefront",
                format!(
                    "Apple Music has no storefront '{}'",
                    self.settings.storefront
                ),
            )),
            Err(DownloadError::Auth(m)) => Err(CheckFailure::new("developerToken", m)),
            Err(e) => Err(CheckFailure::new(
                "baseUrl",
                format!("Apple Music catalogue check failed: {e}"),
            )),
        }
    }

    /// One album with its tracks, or `None` when Apple has no such album.
    ///
    /// # Errors
    /// Transport, auth or API failures.
    pub async fn get_album(&self, album_id: &str) -> Result<Option<AppleAlbum>, DownloadError> {
        if album_id.trim().is_empty() {
            return Ok(None);
        }
        let url = format!(
            "{}catalog/{}/albums/{}?include=tracks",
            self.base(),
            self.settings.storefront,
            pct(album_id)
        );
        let env: Option<AlbumEnvelope> = self.get_json(url).await?;
        Ok(env
            .and_then(|e| e.data.into_iter().next())
            .and_then(AppleAlbum::from_dto))
    }
}

// ------------------------------------------------------------- the decryptor

/// What the decryptor says about one job.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DecryptorJob {
    /// `queued`, `running`, `completed`, `failed` or `cancelled`.
    pub status: String,
    /// Percent, 0 to 100.
    pub progress: f64,
    /// The album folder, relative to the decryptor's `OUTPUT_DIR`.
    pub directory: Option<String>,
    /// The audio files, relative to `OUTPUT_DIR`.
    pub files: Vec<String>,
    /// The failure (or, on a completed job, the partial-success note).
    pub error: Option<String>,
}

impl DecryptorJob {
    fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// The decryptor service's HTTP API.
pub struct DecryptorApi {
    base: String,
    transport: Box<dyn HttpTransport>,
}

impl DecryptorApi {
    /// Over a caller-supplied transport.
    #[must_use]
    pub fn new(base_url: &str, transport: Box<dyn HttpTransport>) -> Self {
        Self {
            base: base_url.trim().trim_end_matches('/').to_string(),
            transport,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Queue an album; returns the job id. The body is the plugin's: the album,
    /// the track list, the relative output directory, the quality key and the
    /// Music-User-Token (sent per request so rotating it is a settings change).
    ///
    /// # Errors
    /// The decryptor's own error text when it refuses, or transport failures.
    pub async fn enqueue(&self, body: &Value) -> Result<String, DownloadError> {
        let resp = self
            .transport
            .send(
                HttpRequest::new("POST", self.url("/v1/download"))
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .body(body.to_string()),
            )
            .await?;
        let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        if !resp.is_success() {
            let why = parsed["error"]
                .as_str()
                .map_or_else(|| format!("HTTP {}", resp.status), str::to_string);
            return Err(DownloadError::Api(format!(
                "decryptor rejected the job: {why}"
            )));
        }
        parsed["job_id"]
            .as_str()
            .filter(|j| !j.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                DownloadError::UnexpectedResponse(
                    "decryptor accepted the request but returned no job id".into(),
                )
            })
    }

    /// One job's state; `None` when the decryptor does not know it (it keeps
    /// jobs in memory, so a restart forgets them).
    ///
    /// # Errors
    /// Transport failures or an unreadable answer.
    pub async fn job(&self, id: &str) -> Result<Option<DecryptorJob>, DownloadError> {
        let resp = self
            .transport
            .send(
                HttpRequest::new("GET", self.url(&format!("/v1/job/{}", pct(id))))
                    .header("accept", "application/json"),
            )
            .await?;
        match resp.status {
            404 => Ok(None),
            200..=299 => serde_json::from_str(&resp.body)
                .map(Some)
                .map_err(|e| DownloadError::UnexpectedResponse(format!("decryptor: {e}"))),
            s => Err(DownloadError::Api(format!("decryptor answered {s}"))),
        }
    }

    /// Ask the decryptor to stop a job after its current track.
    ///
    /// # Errors
    /// Transport failures.
    pub async fn cancel(&self, id: &str) -> Result<(), DownloadError> {
        self.transport
            .send(HttpRequest::new(
                "DELETE",
                self.url(&format!("/v1/job/{}", pct(id))),
            ))
            .await
            .map(|_| ())
    }

    /// `GET /healthz`: `Ok` when the decryptor answers 2xx.
    ///
    /// # Errors
    /// Transport failures or a non-2xx answer.
    pub async fn probe(&self) -> Result<(), DownloadError> {
        let resp = self
            .transport
            .send(HttpRequest::new("GET", self.url("/healthz")))
            .await?;
        if resp.is_success() {
            Ok(())
        } else {
            Err(DownloadError::Api(format!(
                "decryptor health check answered {}",
                resp.status
            )))
        }
    }
}

/// The Test button for an Apple Music **indexer** row.
///
/// # Errors
/// The setting at fault and why.
pub async fn check_indexer(
    settings: AppleMusicSettings,
    transport: Box<dyn HttpTransport>,
) -> Result<(), crate::lifecycle::CheckFailure> {
    AppleCatalogApi::new(settings, transport).check().await
}

/// The Test button for an Apple Music **download client** row: the catalogue
/// check, then the decryptor's `/healthz`.
///
/// # Errors
/// The setting at fault and why.
pub async fn check_client(
    settings: AppleMusicSettings,
    apple: Box<dyn HttpTransport>,
    decryptor: Box<dyn HttpTransport>,
) -> Result<(), crate::lifecycle::CheckFailure> {
    use crate::lifecycle::CheckFailure;
    let url = settings.decryptor_url.clone();
    AppleCatalogApi::new(settings, apple).check().await?;
    DecryptorApi::new(&url, decryptor)
        .probe()
        .await
        .map_err(|e| CheckFailure::new("decryptorUrl", format!("decryptor not healthy: {e}")))
}

/// [`check_indexer`] over production transports with a short timeout.
///
/// # Errors
/// As [`check_indexer`].
pub async fn check_indexer_live(
    settings: AppleMusicSettings,
) -> Result<(), crate::lifecycle::CheckFailure> {
    check_indexer(
        settings,
        Box::new(ReqwestTransport::with_timeout(Duration::from_secs(8))),
    )
    .await
}

/// [`check_client`] over production transports with a short timeout.
///
/// # Errors
/// As [`check_client`].
pub async fn check_client_live(
    settings: AppleMusicSettings,
) -> Result<(), crate::lifecycle::CheckFailure> {
    check_client(
        settings,
        Box::new(ReqwestTransport::with_timeout(Duration::from_secs(8))),
        Box::new(ReqwestTransport::with_timeout(Duration::from_secs(8))),
    )
    .await
}

// ----------------------------------------------------------------- indexer

/// The Apple Music "indexer": searches the catalogue and returns releases.
pub struct AppleMusicIndexer {
    id: IndexerId,
    name: String,
    api: AppleCatalogApi,
}

impl AppleMusicIndexer {
    /// Over the production HTTP transport.
    #[must_use]
    pub fn new(id: IndexerId, name: impl Into<String>, settings: AppleMusicSettings) -> Self {
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
        settings: AppleMusicSettings,
        transport: Box<dyn HttpTransport>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            api: AppleCatalogApi::new(settings, transport),
        }
    }

    fn within_window(&self, a: &AppleAlbum, cutoff: Option<&str>) -> bool {
        match cutoff {
            None => true,
            Some(c) => a.release_date.is_empty() || a.release_date.as_str() <= c,
        }
    }
}

#[async_trait]
impl Indexer for AppleMusicIndexer {
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
        let artist = id("artist").unwrap_or_default();
        let album = id("album");
        let tiers = search_tiers(&artist, album.as_deref().unwrap_or_default());
        if tiers.is_empty() {
            // Searched by names; without them there is nothing to ask.
            return Ok(Vec::new());
        }
        let cutoff = self
            .api
            .settings
            .early_release_limit
            .map(|d| cutoff_date(unix_now(), d));
        let mut last_err = None;
        let mut any_ok = false;
        for q in tiers {
            let found = match self.api.search_albums(&q).await {
                Ok(f) => {
                    any_ok = true;
                    f
                }
                // A rejected token fails every later query the same way, and
                // silence would read as "no results".
                Err(e @ DownloadError::Auth(_)) => return Err(e),
                Err(e) => {
                    tracing::warn!(query = %q, error = %e, "apple music search failed");
                    last_err = Some(e);
                    continue;
                }
            };
            let mut seen = BTreeSet::new();
            let found: Vec<AppleAlbum> = found
                .into_iter()
                .filter(|a| seen.insert(a.id.clone()))
                .collect();
            let candidates = select_matching(found, album.as_deref());
            let releases: Vec<Release> = candidates
                .iter()
                .filter(|a| self.within_window(a, cutoff.as_deref()))
                .flat_map(|a| releases_for(a, self.id))
                .collect();
            if !releases.is_empty() {
                // First tier with results wins: later tiers match more loosely.
                return Ok(releases);
            }
        }
        match (any_ok, last_err) {
            (false, Some(e)) => Err(e),
            _ => Ok(Vec::new()),
        }
    }

    async fn latest(&self) -> Result<Vec<Release>, DownloadError> {
        // The catalogue has no feed.
        Ok(Vec::new())
    }
}

// ------------------------------------------------------------------ client

fn sanitize_segment(value: &str, fallback: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.chars().take(150).collect()
    }
}

/// What a download id carries: enough to follow the decryptor's job.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JobRef {
    album_id: String,
    job_id: String,
    directory: String,
    quality: AppleQuality,
    started: u64,
}

impl JobRef {
    fn encode(&self) -> String {
        let j = json!({"a": self.album_id, "j": self.job_id, "d": self.directory,
                       "q": self.quality.key(), "t": self.started});
        format!("{ID_PREFIX}{}", b64_encode(j.to_string().as_bytes()))
    }

    fn decode(id: &str) -> Option<Self> {
        let v: Value = serde_json::from_slice(&b64_decode(id.strip_prefix(ID_PREFIX)?)?).ok()?;
        Some(Self {
            album_id: v["a"].as_str()?.to_string(),
            job_id: v["j"].as_str()?.to_string(),
            directory: v["d"].as_str()?.to_string(),
            quality: AppleQuality::from_key(v["q"].as_str()?)?,
            started: v["t"].as_u64()?,
        })
    }
}

/// A relative path that stays inside its root: no root, no `..`.
fn safe_relative(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel.trim());
    if rel.trim().is_empty() {
        return None;
    }
    p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        .then(|| p.to_path_buf())
}

/// The Apple Music download client.
pub struct AppleMusicClient {
    name: String,
    settings: AppleMusicSettings,
    catalog: AppleCatalogApi,
    decryptor: DecryptorApi,
}

impl fmt::Debug for AppleMusicClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppleMusicClient")
            .field("name", &self.name)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl AppleMusicClient {
    /// Over the production HTTP transport.
    #[must_use]
    pub fn new(name: impl Into<String>, settings: AppleMusicSettings) -> Self {
        Self::with_transports(
            name,
            settings,
            Box::new(ReqwestTransport::with_timeout(Duration::from_secs(30))),
            Box::new(ReqwestTransport::with_timeout(Duration::from_secs(60))),
        )
    }

    /// Over caller-supplied transports: one for Apple, one for the decryptor.
    #[must_use]
    pub fn with_transports(
        name: impl Into<String>,
        settings: AppleMusicSettings,
        apple: Box<dyn HttpTransport>,
        decryptor: Box<dyn HttpTransport>,
    ) -> Self {
        Self {
            name: name.into(),
            catalog: AppleCatalogApi::new(settings.clone(), apple),
            decryptor: DecryptorApi::new(&settings.decryptor_url, decryptor),
            settings,
        }
    }

    /// Whether the decryptor answers `/healthz`.
    ///
    /// # Errors
    /// Why it does not.
    pub async fn probe(&self) -> Result<(), DownloadError> {
        self.decryptor.probe().await
    }

    fn root(&self) -> &Path {
        Path::new(&self.settings.download_path)
    }

    fn content_dir(&self, rel: &str) -> Option<PathBuf> {
        safe_relative(rel).map(|r| self.root().join(r))
    }

    /// The richer status the trait projects.
    ///
    /// # Errors
    /// [`DownloadError::NotFound`] when the decryptor no longer knows the job.
    pub async fn progress(&self, download_id: &str) -> Result<DownloadProgress, DownloadError> {
        let r = JobRef::decode(download_id)
            .ok_or_else(|| DownloadError::NotFound(download_id.to_string()))?;
        let job = self
            .decryptor
            .job(&r.job_id)
            .await?
            .ok_or_else(|| DownloadError::NotFound(download_id.to_string()))?;
        let mut p = DownloadProgress {
            state: DownloadState::Queued,
            progress: (job.progress / 100.0).clamp(0.0, 1.0),
            content_path: None,
            ratio: None,
            seeding_time_secs: None,
            peers: None,
            error_string: None,
            category: None,
        };
        match job.status.as_str() {
            "completed" => {
                let rel = job.directory.as_deref().unwrap_or(&r.directory);
                if let Some(dir) = self.content_dir(rel) {
                    p.state = DownloadState::Completed;
                    p.progress = 1.0;
                    p.content_path = Some(dir.to_string_lossy().into_owned());
                    if let Some(note) = job.error.as_deref().filter(|e| !e.is_empty()) {
                        // Partial success is still success: what is on disk is
                        // imported, the rest is re-searched.
                        tracing::warn!(album = %r.album_id, note, "apple music download completed with issues");
                    }
                } else {
                    p.state = DownloadState::Failed;
                    p.error_string =
                        Some(format!("decryptor reported an unusable directory: {rel:?}"));
                }
            }
            "failed" | "cancelled" => {
                p.state = DownloadState::Failed;
                p.error_string = Some(
                    job.error
                        .clone()
                        .filter(|e| !e.is_empty())
                        .unwrap_or_else(|| {
                            format!("decryptor finished with status '{}'", job.status)
                        }),
                );
            }
            "running" => p.state = DownloadState::Downloading,
            _ => {}
        }
        if !job.is_terminal() {
            let waited = unix_now().saturating_sub(r.started);
            let limit = u64::try_from(self.settings.decryptor_timeout_minutes).unwrap_or(180) * 60;
            if waited > limit {
                p.state = DownloadState::Failed;
                p.error_string = Some(format!(
                    "Timed out after {} minute(s) waiting for the decryptor (job {}).",
                    self.settings.decryptor_timeout_minutes, r.job_id
                ));
            }
        }
        Ok(p)
    }

    /// Cancel the job if it is still running; optionally delete the files it
    /// wrote.
    ///
    /// # Errors
    /// Transport failures reaching the decryptor.
    pub async fn remove_download(
        &self,
        download_id: &str,
        delete_data: bool,
    ) -> Result<(), DownloadError> {
        let Some(r) = JobRef::decode(download_id) else {
            return Err(DownloadError::NotFound(download_id.to_string()));
        };
        let job = self.decryptor.job(&r.job_id).await?;
        if let Some(j) = &job {
            if !j.is_terminal() {
                self.decryptor.cancel(&r.job_id).await?;
            }
        }
        if delete_data {
            if let Some(j) = &job {
                self.delete_files(&r, j);
            }
        }
        Ok(())
    }

    /// Delete only the audio files the job reports, then the folder if it is
    /// empty. Never leaves the download root. A job the decryptor has forgotten
    /// names no files, so nothing is deleted.
    fn delete_files(&self, r: &JobRef, job: &DecryptorJob) {
        let root = self.root();
        for f in &job.files {
            if let Some(rel) = safe_relative(f) {
                let _ = std::fs::remove_file(root.join(rel));
            }
        }
        let rel = job.directory.as_deref().unwrap_or(&r.directory);
        if let Some(dir) = self.content_dir(rel) {
            if dir != root {
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
}

#[async_trait]
impl DownloadClient for AppleMusicClient {
    type Error = DownloadError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn add(&self, grab: &GrabRequest) -> Result<String, DownloadError> {
        let (album_id, quality) = parse_release_link(&grab.release.download_url)
            .or_else(|| grab.release.guid.as_deref().and_then(parse_release_link))
            .ok_or_else(|| {
                DownloadError::Config(format!(
                    "not an Apple Music release link: {}",
                    grab.release
                        .download_url
                        .chars()
                        .take(40)
                        .collect::<String>()
                ))
            })?;
        // Catalogue lookups are this side's job: the decryptor is handed the
        // track list and never has to know how to search.
        let album = self
            .catalog
            .get_album(&album_id)
            .await?
            .filter(|a| !a.tracks.is_empty())
            .ok_or_else(|| {
                DownloadError::DownloadFailed(format!(
                    "Apple Music returned no tracks for album {album_id}."
                ))
            })?;
        let directory = format!(
            "{}/{}",
            sanitize_segment(&album.artist, "Unknown Artist"),
            sanitize_segment(&album.title, "Unknown Album")
        );
        let tracks: Vec<Value> = album
            .tracks
            .iter()
            .map(|t| {
                json!({"id": t.id, "title": t.title, "trackNumber": t.track_number,
                       "discNumber": t.disc_number})
            })
            .collect();
        let body = json!({
            "albumId": album_id,
            "artist": album.artist,
            "album": album.title,
            "storefront": self.settings.storefront,
            "directory": directory,
            "quality": quality.key(),
            "musicUserToken": self.settings.music_user_token,
            "tracks": tracks,
        });
        let job_id = self.decryptor.enqueue(&body).await?;
        tracing::debug!(album = %album_id, job = %job_id, quality = quality.key(), "decryptor accepted album");
        Ok(JobRef {
            album_id,
            job_id,
            directory,
            quality,
            started: unix_now(),
        }
        .encode())
    }

    async fn status(&self, download_id: &str) -> Result<DownloadStatus, DownloadError> {
        Ok(self.progress(download_id).await?.to_core_status())
    }

    async fn remove(&self, download_id: &str, delete_data: bool) -> Result<(), DownloadError> {
        self.remove_download(download_id, delete_data).await
    }
}

impl AppleMusicSettings {
    /// The quality a link without one falls back to.
    #[must_use]
    pub fn preferred_quality_tier(&self) -> AppleQuality {
        self.preferred()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, title: &str, artist: &str) -> AppleAlbum {
        AppleAlbum {
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            release_date: String::new(),
            track_count: 10,
            url: String::new(),
            tracks: Vec::new(),
            has_atmos: None,
        }
    }

    #[test]
    fn links_round_trip_in_the_url_and_guid_shapes() {
        let u = release_url("1667414099", AppleQuality::Atmos);
        assert_eq!(u, "applemusicarr://album/1667414099?quality=atmos");
        assert_eq!(
            parse_release_link(&u),
            Some(("1667414099".into(), AppleQuality::Atmos))
        );
        assert_eq!(
            parse_release_link("applemusicarr:1667414099:alac"),
            Some(("1667414099".into(), AppleQuality::Alac))
        );
        // Lidarr persists the guid behind the indexer id.
        assert_eq!(
            parse_release_link("49_applemusicarr:1667414099:atmos"),
            Some(("1667414099".into(), AppleQuality::Atmos))
        );
        assert_eq!(
            parse_release_link("applemusicarr://album/55"),
            Some(("55".into(), AppleQuality::Aac256))
        );
        assert!(parse_release_link("slskd:abc").is_none());
        assert!(parse_release_link("applemusicarr://album/").is_none());
        assert!(is_apple_url("applemusicarr://album/1"));
        assert!(!is_apple_url("magnet:?xt=x"));
    }

    #[test]
    fn kinds_cover_the_plugin_names() {
        for k in [
            "applemusicarr",
            "AppleMusicarrIndexer",
            "applemusicarrdownloadclient",
        ] {
            assert!(is_apple_kind(k), "{k}");
        }
        assert!(!is_apple_kind("slskd"));
    }

    #[test]
    fn titles_compare_by_identity_not_containment() {
        assert_eq!(
            comparable_title("Wandered To LA - Single"),
            "wandered to la"
        );
        assert_eq!(
            comparable_title("Sgt. Pepper's"),
            comparable_title("Sgt. Peppers")
        );
        assert_eq!(comparable_title("Beyoncé"), comparable_title("Beyonce"));
        assert_eq!(comparable_title("Some EP - EP"), "some ep");
        // A title that IS the suffix is not an edition marker.
        assert_eq!(comparable_title("Single"), "single");
        let kept = select_matching(
            vec![
                album("1", "Playa Grande", "A"),
                album("2", "Playa Grande (Uproot Andy remix)", "A"),
                album("3", "Playa Grande - Single", "A"),
            ],
            Some("Playa Grande"),
        );
        assert_eq!(
            kept.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["1", "3"]
        );
        // An artist search keeps everything.
        assert_eq!(select_matching(vec![album("1", "x", "a")], None).len(), 1);
    }

    #[test]
    fn tiers_degrade_from_specific_to_loose() {
        assert_eq!(
            search_tiers("Miles  Davis", "Kind of Blue"),
            ["Miles Davis Kind of Blue", "Miles Davis", "Kind of Blue"]
        );
        assert_eq!(search_tiers("", "Solo"), ["Solo"]);
        assert_eq!(search_tiers("Same", "same"), ["Same same", "Same"]);
        assert!(search_tiers("", "").is_empty());
    }

    #[test]
    fn a_release_per_tier_and_atmos_only_when_the_album_has_it() {
        let mut a = album("9", "Kind of Blue", "Miles Davis");
        a.track_count = 5;
        let all = releases_for(&a, IndexerId::new());
        assert_eq!(
            all.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
            [
                "Miles Davis - Kind of Blue [AAC] [256]",
                "Miles Davis - Kind of Blue [ALAC] [LOSSLESS]",
                "Miles Davis - Kind of Blue [AAC] [VBR] [ATMOS]",
            ]
        );
        // 5 tracks x 240 s at 256 kbps.
        assert_eq!(all[0].size, Some(5 * 240 * 256_000 / 8));
        assert_eq!(all[1].guid.as_deref(), Some("applemusicarr:9:alac"));
        a.has_atmos = Some(false);
        assert_eq!(releases_for(&a, IndexerId::new()).len(), 2);
        a.has_atmos = Some(true);
        assert_eq!(releases_for(&a, IndexerId::new()).len(), 3);
        // A count of zero still estimates one track, not a floor of twelve.
        a.track_count = 0;
        assert_eq!(a.estimated_secs(), 240);
    }

    #[test]
    fn the_early_release_window_counts_days() {
        // 2026-11-26 00:00:00 UTC
        let now = 1_795_651_200;
        assert_eq!(cutoff_date(now, 0), "2026-11-26");
        assert_eq!(cutoff_date(now, 14), "2026-12-10");
        assert_eq!(cutoff_date(now, 365), "2027-11-26");
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(11_016), "2000-02-29");
    }

    #[test]
    fn settings_need_a_token_and_the_client_needs_more_and_both_can_opt_out() {
        let ix = json!({"developerToken": "eyJa.b.c"});
        assert!(AppleMusicSettings::from_json_indexer(&ix).is_ok());
        assert!(AppleMusicSettings::from_json_indexer(&json!({})).is_err());
        assert!(AppleMusicSettings::from_json_indexer(
            &json!({"developerToken": "t", "native": false})
        )
        .is_err());
        assert!(AppleMusicSettings::from_json_indexer(
            &json!({"developerToken": "t", "storefront": "usa"})
        )
        .is_err());
        // The client also needs the user token and an http(s) decryptor URL.
        let mut c = json!({"developerToken": "t", "musicUserToken": "m",
                           "decryptorUrl": "http://d:8080"});
        let s = AppleMusicSettings::from_json_client(&c).unwrap();
        assert_eq!(s.download_path, DEFAULT_DOWNLOAD_DIR);
        assert_eq!(s.storefront, "us");
        assert_eq!(s.decryptor_timeout_minutes, 180);
        assert_eq!(s.early_release_limit, Some(14));
        c["decryptorUrl"] = json!("d:8080");
        assert!(AppleMusicSettings::from_json_client(&c).is_err());
        c["decryptorUrl"] = json!("http://d");
        c["musicUserToken"] = json!("");
        assert!(AppleMusicSettings::from_json_client(&c).is_err());
        // A null window means "no window"; the plugin's enum arrives as a number.
        let s = AppleMusicSettings::from_json_indexer(
            &json!({"DeveloperToken": "t", "EarlyReleaseLimit": null, "preferredQuality": 1}),
        )
        .unwrap();
        assert_eq!(s.early_release_limit, None);
        assert_eq!(s.preferred_quality_tier(), AppleQuality::Alac);
    }

    #[test]
    fn debug_never_shows_a_token() {
        let s = AppleMusicSettings::from_json_client(&json!({
            "developerToken": "SECRET-DEV-TOKEN", "musicUserToken": "SECRET-USER-TOKEN",
            "decryptorUrl": "http://d"
        }))
        .unwrap();
        let shown = format!("{s:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("<redacted>"));
        let c = AppleMusicClient::new("c", s);
        assert!(!format!("{c:?}").contains("SECRET"));
    }

    fn jwt(payload: &str) -> String {
        format!(
            "eyJhbGciOiJFUzI1NiJ9.{}.sig",
            b64_encode(payload.as_bytes())
        )
    }

    #[test]
    fn a_developer_token_is_diagnosed_without_being_repeated() {
        assert!(diagnose_developer_token("").is_some());
        let not_jwt = diagnose_developer_token("opaque-cookie-value").unwrap();
        assert!(not_jwt.contains("Music-User-Token"));
        assert!(!not_jwt.contains("opaque-cookie-value"));
        assert!(diagnose_developer_token("eyJonly.two")
            .unwrap()
            .contains("segments"));
        let expired = diagnose_developer_token(&jwt(r#"{"iss":"T","exp":86400}"#)).unwrap();
        assert!(expired.contains("expired on 1970-01-02"), "{expired}");
        assert!(diagnose_developer_token(&jwt(r#"{"iss":"T","exp":4102444800}"#)).is_none());
        assert!(is_web_player_token(&jwt(r#"{"iss":"AMPWebPlay"}"#)));
        assert!(!is_web_player_token(&jwt(r#"{"iss":"TEAM"}"#)));
    }

    #[test]
    fn a_download_id_survives_and_a_directory_cannot_escape() {
        let r = JobRef {
            album_id: "9".into(),
            job_id: "abc".into(),
            directory: "A/B".into(),
            quality: AppleQuality::Alac,
            started: 5,
        };
        assert_eq!(JobRef::decode(&r.encode()), Some(r));
        assert!(JobRef::decode("slskd-xx").is_none());
        assert!(safe_relative("A/B").is_some());
        assert!(safe_relative("../x").is_none());
        assert!(safe_relative("/etc").is_none());
        assert!(safe_relative("").is_none());
        assert_eq!(sanitize_segment("AC/DC: Live?", "x"), "ACDC Live");
        assert_eq!(sanitize_segment("..", "Unknown"), "Unknown");
    }
}

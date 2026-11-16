//! The MusicBrainz metadata source.
//!
//! MusicBrainz is the open, CC0 music encyclopedia and the identity space the
//! whole music ecosystem agrees on. cellarr talks to its public web service
//! directly — it needs no API key and no intermediary metadata server — and
//! caches what it learns in its own database, so a library that has been
//! fetched once keeps working with the network unplugged.
//!
//! Two rules from MusicBrainz shape this client: requests are limited to one
//! per second, and every request must identify the application in its
//! `User-Agent`. Both are enforced here rather than left to callers.
//!
//! The vocabulary: an **artist** has many **release groups** (the abstract
//! albums); a release group has many **releases** (the pressings: the 1997 CD,
//! the 2009 two-disc reissue, the vinyl); a release has **media** (discs), each
//! with **tracks**. cellarr's album is the release group, and its track list
//! comes from one representative release (see [`pick_release`]).

use serde::Deserialize;

use crate::error::MetaError;
use crate::http::Fetcher;
use crate::ratelimit::RateLimiter;

const SOURCE: &str = "musicbrainz";

/// The public MusicBrainz web-service root.
pub const DEFAULT_BASE_URL: &str = "https://musicbrainz.org/ws/2";

/// How many times a request refused with "server busy" (HTTP 503) or rate
/// limited (429) is retried before the error is reported.
const BUSY_RETRIES: u32 = 4;

/// An artist as a search hit or a lookup result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbArtist {
    /// MusicBrainz artist id.
    pub mbid: String,
    /// Display name.
    pub name: String,
    /// Sort name.
    pub sort_name: Option<String>,
    /// Disambiguation comment.
    pub disambiguation: Option<String>,
    /// `Person`, `Group`, ….
    pub artist_type: Option<String>,
    /// Whether the artist has ended.
    pub ended: bool,
    /// Alternative names (aliases), excluding the display name.
    pub aliases: Vec<String>,
    /// Genre names, most-voted first.
    pub genres: Vec<String>,
    /// Search relevance (0–100); 100 for a direct lookup.
    pub score: u32,
}

/// A release group: the abstract album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbReleaseGroup {
    /// MusicBrainz release-group id.
    pub mbid: String,
    /// Title.
    pub title: String,
    /// Primary type, defaulting to `Other` when MusicBrainz has none.
    pub primary_type: String,
    /// Secondary types.
    pub secondary_types: Vec<String>,
    /// First release date, as precise as known.
    pub first_release_date: Option<String>,
    /// Disambiguation comment.
    pub disambiguation: Option<String>,
    /// The credited artist, when the response carried one (search results do).
    pub artist: Option<(String, String)>,
}

/// One pressing of a release group, with its track list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbRelease {
    /// MusicBrainz release id.
    pub mbid: String,
    /// Title of this pressing.
    pub title: String,
    /// `Official`, `Promotion`, `Bootleg`, `Pseudo-Release`.
    pub status: Option<String>,
    /// Release date.
    pub date: Option<String>,
    /// Release country code.
    pub country: Option<String>,
    /// The discs.
    pub media: Vec<MbMedium>,
}

impl MbRelease {
    /// Total audio tracks across every disc.
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.media.iter().map(|m| m.tracks.len()).sum()
    }
}

/// One disc of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbMedium {
    /// Disc number, 1-based.
    pub position: u32,
    /// `CD`, `Digital Media`, `12" Vinyl`, ….
    pub format: Option<String>,
    /// The tracks on this disc.
    pub tracks: Vec<MbTrack>,
}

/// One track on a disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbTrack {
    /// Position on the disc, 1-based.
    pub position: u32,
    /// Track title.
    pub title: String,
    /// Length in milliseconds.
    pub length_ms: Option<u32>,
    /// MusicBrainz recording id.
    pub recording_mbid: Option<String>,
}

/// The MusicBrainz web-service client.
pub struct MusicBrainzSource<F: Fetcher> {
    fetcher: F,
    base_url: String,
    user_agent: String,
    limiter: RateLimiter,
}

impl<F: Fetcher> MusicBrainzSource<F> {
    /// A client against the public service.
    #[must_use]
    pub fn new(fetcher: F) -> Self {
        Self::with_base_url(fetcher, DEFAULT_BASE_URL)
    }

    /// A client against a mirror (or a test server).
    #[must_use]
    pub fn with_base_url(fetcher: F, base_url: &str) -> Self {
        Self {
            fetcher,
            base_url: base_url.trim_end_matches('/').to_string(),
            user_agent: concat!(
                "cellarr/",
                env!("CARGO_PKG_VERSION"),
                " ( https://github.com/poiley/cellarr )"
            )
            .to_string(),
            limiter: RateLimiter::per_second(1),
        }
    }

    /// Search artists by name, best match first.
    ///
    /// # Errors
    /// Returns a [`MetaError`] on transport, HTTP or decode failure.
    pub async fn search_artists(&self, query: &str, limit: u32) -> Result<Vec<MbArtist>, MetaError> {
        let lucene = format!("artist:\"{}\"", escape_lucene(query));
        let url = format!(
            "{}/artist?query={}&fmt=json&limit={limit}",
            self.base_url,
            encode(&lucene)
        );
        let body: ArtistSearch = self.get_json(&url).await?;
        Ok(body.artists.into_iter().map(MbArtist::from).collect())
    }

    /// Fetch one artist with its aliases and genres.
    ///
    /// # Errors
    /// Returns a [`MetaError`] on transport, HTTP or decode failure.
    pub async fn artist(&self, mbid: &str) -> Result<MbArtist, MetaError> {
        let url = format!(
            "{}/artist/{}?inc=aliases+genres&fmt=json",
            self.base_url,
            encode(mbid)
        );
        let raw: RawArtist = self.get_json(&url).await?;
        let mut artist = MbArtist::from(raw);
        artist.score = 100;
        Ok(artist)
    }

    /// Every release group of an artist, optionally narrowed to some primary
    /// types (the service filters, so an artist with hundreds of singles costs
    /// one page when only albums are wanted).
    ///
    /// # Errors
    /// Returns a [`MetaError`] on transport, HTTP or decode failure.
    pub async fn release_groups(
        &self,
        artist_mbid: &str,
        primary_types: &[String],
    ) -> Result<Vec<MbReleaseGroup>, MetaError> {
        const PAGE: usize = 100;
        let type_filter = if primary_types.is_empty() {
            String::new()
        } else {
            let joined = primary_types
                .iter()
                .map(|t| t.to_lowercase())
                .collect::<Vec<_>>()
                .join("|");
            format!("&type={}", encode(&joined))
        };
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let url = format!(
                "{}/release-group?artist={}{type_filter}&limit={PAGE}&offset={offset}&fmt=json",
                self.base_url,
                encode(artist_mbid),
            );
            let page: ReleaseGroupBrowse = self.get_json(&url).await?;
            let got = page.release_groups.len();
            out.extend(page.release_groups.into_iter().map(MbReleaseGroup::from));
            offset += got;
            if got == 0 || offset >= page.count {
                break;
            }
        }
        Ok(out)
    }

    /// Search release groups by title, optionally within one artist.
    ///
    /// # Errors
    /// Returns a [`MetaError`] on transport, HTTP or decode failure.
    pub async fn search_release_groups(
        &self,
        title: &str,
        artist: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MbReleaseGroup>, MetaError> {
        let mut lucene = format!("releasegroup:\"{}\"", escape_lucene(title));
        if let Some(artist) = artist {
            lucene.push_str(&format!(" AND artist:\"{}\"", escape_lucene(artist)));
        }
        let url = format!(
            "{}/release-group?query={}&fmt=json&limit={limit}",
            self.base_url,
            encode(&lucene)
        );
        let body: ReleaseGroupSearch = self.get_json(&url).await?;
        Ok(body
            .release_groups
            .into_iter()
            .map(MbReleaseGroup::from)
            .collect())
    }

    /// The pressings of a release group, each with its full track list.
    ///
    /// # Errors
    /// Returns a [`MetaError`] on transport, HTTP or decode failure.
    pub async fn releases(&self, release_group_mbid: &str) -> Result<Vec<MbRelease>, MetaError> {
        // With recordings included the service caps a page at 25 releases. Two
        // pages are plenty to find the representative pressing; an album with
        // more than fifty pressings does not have fifty track layouts.
        const PAGE: usize = 25;
        const MAX_PAGES: usize = 2;
        let mut out = Vec::new();
        for page_index in 0..MAX_PAGES {
            let url = format!(
                "{}/release?release-group={}&inc=recordings+media&limit={PAGE}&offset={}&fmt=json",
                self.base_url,
                encode(release_group_mbid),
                page_index * PAGE,
            );
            let page: ReleaseBrowse = self.get_json(&url).await?;
            let got = page.releases.len();
            out.extend(page.releases.into_iter().map(MbRelease::from));
            if got < PAGE || out.len() >= page.count {
                break;
            }
        }
        Ok(out)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, MetaError> {
        let headers = [
            ("User-Agent", self.user_agent.as_str()),
            ("Accept", "application/json"),
        ];
        let mut attempt = 0;
        loop {
            self.limiter.until_ready().await;
            let resp = self.fetcher.get(url, &headers).await?;
            if resp.is_success() {
                return serde_json::from_slice(&resp.body).map_err(|e| MetaError::Decode {
                    src: SOURCE,
                    detail: e.to_string(),
                });
            }
            // "Busy" and "slow down" are the service asking for a pause, not a
            // verdict on the request; anything else is final.
            let retryable = matches!(resp.status, 429 | 503);
            if !retryable || attempt >= BUSY_RETRIES {
                return Err(MetaError::Http {
                    src: SOURCE,
                    status: resp.status,
                });
            }
            attempt += 1;
            tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt))).await;
        }
    }
}

/// Choose the pressing whose track list an album should use.
///
/// A release group's pressings disagree: the original CD has twelve tracks, the
/// anniversary reissue adds a bonus disc, the vinyl splits the same songs over
/// two records. The album a listener means is the **common** one, so this picks
/// by majority and breaks ties toward the plainest, earliest pressing:
///
/// 1. only statuses the caller allows (when any pressing has one of them);
/// 2. the track count most pressings share;
/// 3. among those, the fewest discs (a single CD over the same songs on two LPs);
/// 4. then the earliest release date, then the id, so the choice is stable.
#[must_use]
pub fn pick_release<'a>(releases: &'a [MbRelease], allowed_statuses: &[String]) -> Option<&'a MbRelease> {
    let status_ok = |r: &MbRelease| {
        allowed_statuses.is_empty()
            || r.status.as_deref().is_some_and(|s| {
                allowed_statuses.iter().any(|a| a.eq_ignore_ascii_case(s))
            })
    };
    let with_tracks: Vec<&MbRelease> = releases.iter().filter(|r| r.track_count() > 0).collect();
    let mut pool: Vec<&MbRelease> = with_tracks.iter().copied().filter(|r| status_ok(r)).collect();
    if pool.is_empty() {
        pool = with_tracks;
    }
    let mut frequency: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for r in &pool {
        *frequency.entry(r.track_count()).or_default() += 1;
    }
    // The most common count; a tie goes to the smaller (the un-expanded album).
    let common = frequency
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(count, _)| *count)?;
    pool.into_iter()
        .filter(|r| r.track_count() == common)
        .min_by(|a, b| {
            a.media
                .len()
                .cmp(&b.media.len())
                // A missing date sorts last: an undated pressing is the least
                // trustworthy witness of when the album came out.
                .then_with(|| match (&a.date, &b.date) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.mbid.cmp(&b.mbid))
        })
}

/// Escape the characters Lucene query syntax gives meaning to, inside a quoted
/// phrase: the quote itself and the backslash.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Percent-encode a query-string value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

// --- wire shapes ---------------------------------------------------------------

#[derive(Deserialize)]
struct ArtistSearch {
    #[serde(default)]
    artists: Vec<RawArtist>,
}

#[derive(Deserialize)]
struct RawArtist {
    id: String,
    name: String,
    #[serde(rename = "sort-name")]
    sort_name: Option<String>,
    disambiguation: Option<String>,
    #[serde(rename = "type")]
    artist_type: Option<String>,
    #[serde(rename = "life-span")]
    life_span: Option<RawLifeSpan>,
    #[serde(default)]
    aliases: Vec<RawAlias>,
    #[serde(default)]
    genres: Vec<RawGenre>,
    score: Option<u32>,
}

#[derive(Deserialize)]
struct RawLifeSpan {
    ended: Option<bool>,
}

#[derive(Deserialize)]
struct RawAlias {
    name: String,
}

#[derive(Deserialize)]
struct RawGenre {
    name: String,
    #[serde(default)]
    count: i64,
}

impl From<RawArtist> for MbArtist {
    fn from(raw: RawArtist) -> Self {
        let mut genres = raw.genres;
        genres.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        let mut aliases: Vec<String> = raw
            .aliases
            .into_iter()
            .map(|a| a.name)
            .filter(|a| *a != raw.name)
            .collect();
        aliases.sort();
        aliases.dedup();
        MbArtist {
            mbid: raw.id,
            sort_name: non_empty(raw.sort_name).filter(|s| *s != raw.name),
            name: raw.name,
            disambiguation: non_empty(raw.disambiguation),
            artist_type: non_empty(raw.artist_type),
            ended: raw.life_span.and_then(|l| l.ended).unwrap_or(false),
            aliases,
            genres: genres.into_iter().map(|g| g.name).collect(),
            score: raw.score.unwrap_or(0),
        }
    }
}

#[derive(Deserialize)]
struct ReleaseGroupBrowse {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<RawReleaseGroup>,
    #[serde(rename = "release-group-count", default)]
    count: usize,
}

#[derive(Deserialize)]
struct ReleaseGroupSearch {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<RawReleaseGroup>,
}

#[derive(Deserialize)]
struct RawReleaseGroup {
    id: String,
    title: String,
    #[serde(rename = "primary-type")]
    primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Vec<String>,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
    disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<RawArtistCredit>,
}

#[derive(Deserialize)]
struct RawArtistCredit {
    artist: Option<RawCreditedArtist>,
}

#[derive(Deserialize)]
struct RawCreditedArtist {
    id: String,
    name: String,
}

impl From<RawReleaseGroup> for MbReleaseGroup {
    fn from(raw: RawReleaseGroup) -> Self {
        MbReleaseGroup {
            mbid: raw.id,
            title: raw.title,
            primary_type: non_empty(raw.primary_type).unwrap_or_else(|| "Other".to_string()),
            secondary_types: raw.secondary_types,
            first_release_date: non_empty(raw.first_release_date),
            disambiguation: non_empty(raw.disambiguation),
            artist: raw
                .artist_credit
                .into_iter()
                .find_map(|c| c.artist)
                .map(|a| (a.id, a.name)),
        }
    }
}

#[derive(Deserialize)]
struct ReleaseBrowse {
    #[serde(default)]
    releases: Vec<RawRelease>,
    #[serde(rename = "release-count", default)]
    count: usize,
}

#[derive(Deserialize)]
struct RawRelease {
    id: String,
    title: String,
    status: Option<String>,
    date: Option<String>,
    country: Option<String>,
    #[serde(default)]
    media: Vec<RawMedium>,
}

#[derive(Deserialize)]
struct RawMedium {
    #[serde(default)]
    position: u32,
    format: Option<String>,
    #[serde(default)]
    tracks: Vec<RawTrack>,
}

#[derive(Deserialize)]
struct RawTrack {
    #[serde(default)]
    position: u32,
    title: String,
    length: Option<u32>,
    recording: Option<RawRecording>,
}

#[derive(Deserialize)]
struct RawRecording {
    id: Option<String>,
    length: Option<u32>,
}

/// Disc formats that carry no audio album tracks. A deluxe set's DVD is listed
/// as a medium with "tracks" (its video chapters); they are not songs.
fn is_video_format(format: Option<&str>) -> bool {
    format.is_some_and(|f| {
        let f = f.to_lowercase();
        f.contains("dvd-video") || f.contains("blu-ray") || f == "vhs" || f == "laserdisc"
    })
}

impl From<RawRelease> for MbRelease {
    fn from(raw: RawRelease) -> Self {
        let media = raw
            .media
            .into_iter()
            .filter(|m| !is_video_format(m.format.as_deref()))
            .enumerate()
            .map(|(index, m)| MbMedium {
                // Discs are renumbered densely once video discs are dropped, and
                // a missing position falls back to its place in the list.
                position: u32::try_from(index + 1).unwrap_or(1),
                format: non_empty(m.format),
                tracks: m
                    .tracks
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| MbTrack {
                        position: if t.position > 0 {
                            t.position
                        } else {
                            u32::try_from(i + 1).unwrap_or(1)
                        },
                        title: t.title,
                        length_ms: t
                            .length
                            .or_else(|| t.recording.as_ref().and_then(|r| r.length)),
                        recording_mbid: t.recording.and_then(|r| r.id),
                    })
                    .collect(),
            })
            .collect();
        MbRelease {
            mbid: raw.id,
            title: raw.title,
            status: non_empty(raw.status),
            date: non_empty(raw.date),
            country: non_empty(raw.country),
            media,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(id: &str, status: &str, date: Option<&str>, discs: &[usize]) -> MbRelease {
        MbRelease {
            mbid: id.to_string(),
            title: "t".to_string(),
            status: Some(status.to_string()),
            date: date.map(str::to_string),
            country: None,
            media: discs
                .iter()
                .enumerate()
                .map(|(i, n)| MbMedium {
                    position: u32::try_from(i + 1).unwrap(),
                    format: Some("CD".to_string()),
                    tracks: (1..=*n)
                        .map(|p| MbTrack {
                            position: u32::try_from(p).unwrap(),
                            title: format!("track {p}"),
                            length_ms: None,
                            recording_mbid: None,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn official() -> Vec<String> {
        vec!["Official".to_string()]
    }

    #[test]
    fn the_common_track_count_wins_over_an_expanded_reissue() {
        let releases = vec![
            release("reissue", "Official", Some("2009"), &[12, 15]),
            release("cd-b", "Official", Some("1997-07-01"), &[12]),
            release("cd-a", "Official", Some("1997-05-21"), &[12]),
        ];
        assert_eq!(pick_release(&releases, &official()).unwrap().mbid, "cd-a");
    }

    #[test]
    fn the_same_songs_on_one_disc_beat_them_split_over_two() {
        let releases = vec![
            release("vinyl", "Official", Some("1997-01-01"), &[6, 6]),
            release("cd", "Official", Some("1997-06-01"), &[12]),
        ];
        assert_eq!(pick_release(&releases, &official()).unwrap().mbid, "cd");
    }

    #[test]
    fn a_disallowed_status_is_only_used_when_nothing_else_exists() {
        let releases = vec![
            release("boot", "Bootleg", Some("1990"), &[9]),
            release("real", "Official", Some("1997"), &[12]),
        ];
        assert_eq!(pick_release(&releases, &official()).unwrap().mbid, "real");
        let only_bootleg = vec![release("boot", "Bootleg", Some("1990"), &[9])];
        assert_eq!(pick_release(&only_bootleg, &official()).unwrap().mbid, "boot");
    }

    #[test]
    fn an_undated_pressing_loses_to_a_dated_one_and_ties_break_on_id() {
        let releases = vec![
            release("b", "Official", None, &[10]),
            release("c", "Official", Some("2001"), &[10]),
            release("a", "Official", Some("2001"), &[10]),
        ];
        assert_eq!(pick_release(&releases, &official()).unwrap().mbid, "a");
    }

    #[test]
    fn nothing_is_picked_when_no_pressing_has_tracks() {
        assert!(pick_release(&[release("empty", "Official", None, &[])], &official()).is_none());
        assert!(pick_release(&[], &official()).is_none());
    }

    #[test]
    fn query_values_are_percent_encoded_and_phrases_escaped() {
        assert_eq!(encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(escape_lucene("say \"hi\" \\ bye"), "say \\\"hi\\\" \\\\ bye");
    }
}

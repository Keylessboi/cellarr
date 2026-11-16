//! Lidarr's metadata service as a music source.
//!
//! The service is a MusicBrainz mirror that adds what MusicBrainz does not
//! have in one call: biographies, artwork, links and ratings. It keys everything
//! by the same MusicBrainz ids, so an artist added from here is the same artist
//! as one added from MusicBrainz. The response shapes are public interface facts,
//! read here with our own types.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::MetaError;
use crate::http::Fetcher;
use crate::musicbrainz::{
    MbArtist, MbImage, MbLink, MbMedium, MbRating, MbRelease, MbReleaseGroup, MbTrack, MusicMeta,
};
use crate::ratelimit::RateLimiter;

const SOURCE: &str = "lidarr-metadata";

/// The public service root.
pub const DEFAULT_BASE_URL: &str = "https://api.lidarr.audio/api/v0.4";

/// The metadata-service client.
pub struct LidarrMetaSource<F: Fetcher> {
    fetcher: F,
    base_url: String,
    limiter: RateLimiter,
}

impl<F: Fetcher> LidarrMetaSource<F> {
    /// A client against the public service.
    #[must_use]
    pub fn new(fetcher: F) -> Self {
        Self::with_base_url(fetcher, DEFAULT_BASE_URL)
    }

    /// A client against `base_url` (a mirror, or a test server).
    #[must_use]
    pub fn with_base_url(fetcher: F, base_url: &str) -> Self {
        Self {
            fetcher,
            base_url: base_url.trim_end_matches('/').to_string(),
            // Courtesy limit; the service is a shared cache, not ours.
            limiter: RateLimiter::per_second(5),
        }
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, MetaError> {
        self.limiter.until_ready().await;
        let resp = self
            .fetcher
            .get(url, &[("Accept", "application/json")])
            .await?;
        if !resp.is_success() {
            return Err(MetaError::Http {
                src: SOURCE,
                status: resp.status,
            });
        }
        serde_json::from_slice(&resp.body).map_err(|e| MetaError::Decode {
            src: SOURCE,
            detail: e.to_string(),
        })
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

#[derive(Deserialize, Default)]
struct RawLink {
    #[serde(default)]
    target: String,
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Deserialize, Default)]
struct RawImage {
    #[serde(default, rename = "CoverType")]
    cover_type: String,
    #[serde(default, rename = "Url")]
    url: String,
}

#[derive(Deserialize)]
struct RawRating {
    #[serde(default, rename = "Count")]
    count: Option<u32>,
    #[serde(default, rename = "Value")]
    value: Option<f64>,
}

#[derive(Deserialize)]
struct RawArtist {
    id: String,
    #[serde(default)]
    artistname: String,
    #[serde(default)]
    sortname: Option<String>,
    #[serde(default)]
    artistaliases: Vec<String>,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    links: Vec<RawLink>,
    #[serde(default)]
    images: Vec<RawImage>,
    #[serde(default)]
    rating: Option<RawRating>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, rename = "Albums")]
    albums: Vec<RawArtistAlbum>,
}

#[derive(Deserialize)]
struct RawArtistAlbum {
    #[serde(rename = "Id")]
    id: String,
    #[serde(default, rename = "Title")]
    title: String,
    #[serde(default, rename = "Type")]
    kind: Option<String>,
    #[serde(default, rename = "SecondaryTypes")]
    secondary: Vec<String>,
    #[serde(default, rename = "ReleaseDate")]
    date: Option<String>,
}

#[derive(Deserialize)]
struct RawAlbum {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    disambiguation: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    secondarytypes: Vec<String>,
    #[serde(default)]
    releasedate: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    links: Vec<RawLink>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    images: Vec<RawImage>,
    #[serde(default)]
    rating: Option<RawRating>,
    #[serde(default)]
    artists: Vec<RawAlbumArtist>,
    #[serde(default, rename = "Releases")]
    releases: Vec<RawRelease>,
}

#[derive(Deserialize)]
struct RawAlbumArtist {
    id: String,
    #[serde(default)]
    artistname: String,
}

#[derive(Deserialize)]
struct RawRelease {
    #[serde(rename = "Id")]
    id: String,
    #[serde(default, rename = "Title")]
    title: String,
    #[serde(default, rename = "Status")]
    status: Option<String>,
    #[serde(default, rename = "Country")]
    country: Vec<String>,
    #[serde(default, rename = "Media")]
    media: Vec<RawMedium>,
    #[serde(default, rename = "ReleaseDate")]
    date: Option<String>,
    #[serde(default, rename = "Tracks")]
    tracks: Vec<RawTrack>,
}

#[derive(Deserialize)]
struct RawMedium {
    #[serde(default, rename = "Position")]
    position: u32,
    #[serde(default, rename = "Format")]
    format: Option<String>,
}

#[derive(Deserialize)]
struct RawTrack {
    #[serde(default, rename = "RecordingId")]
    recording: Option<String>,
    #[serde(default, rename = "TrackName")]
    name: String,
    #[serde(default, rename = "TrackPosition")]
    position: u32,
    #[serde(default, rename = "DurationMs")]
    duration: Option<u32>,
    #[serde(default, rename = "MediumNumber")]
    medium: u32,
}

fn links(raw: Vec<RawLink>) -> Vec<MbLink> {
    raw.into_iter()
        .filter(|l| !l.target.is_empty())
        .map(|l| MbLink {
            name: l.kind,
            url: l.target,
        })
        .collect()
}

fn images(raw: Vec<RawImage>) -> Vec<MbImage> {
    raw.into_iter()
        .filter(|i| !i.url.is_empty())
        .map(|i| MbImage {
            cover_type: i.cover_type,
            url: i.url,
        })
        .collect()
}

fn rating(raw: Option<RawRating>) -> Option<MbRating> {
    raw.and_then(|r| match (r.count, r.value) {
        (Some(votes), Some(value)) if votes > 0 => Some(MbRating { votes, value }),
        _ => None,
    })
}

impl From<RawArtist> for MbArtist {
    fn from(raw: RawArtist) -> Self {
        let name = raw.artistname;
        let mut aliases: Vec<String> = raw
            .artistaliases
            .into_iter()
            .filter(|a| *a != name)
            .collect();
        aliases.sort();
        aliases.dedup();
        MbArtist {
            overview: non_empty(raw.overview),
            images: images_lower(raw.images),
            links: links(raw.links),
            rating: rating(raw.rating),
            mbid: raw.id,
            sort_name: non_empty(raw.sortname).filter(|s| *s != name),
            name,
            disambiguation: non_empty(raw.disambiguation),
            artist_type: non_empty(raw.kind),
            ended: raw.status.as_deref() == Some("ended"),
            aliases,
            genres: raw.genres,
            score: 100,
        }
    }
}

// Artist images use lowercase keys on this service, album images capitalised.
fn images_lower(raw: Vec<RawImage>) -> Vec<MbImage> {
    images(raw)
}

fn group_of(a: RawArtistAlbum, artist: &(String, String)) -> MbReleaseGroup {
    MbReleaseGroup {
        releases: Vec::new(),
        overview: None,
        images: Vec::new(),
        links: Vec::new(),
        genres: Vec::new(),
        rating: None,
        mbid: a.id,
        title: a.title,
        primary_type: non_empty(a.kind).unwrap_or_else(|| "Other".to_string()),
        secondary_types: a.secondary,
        first_release_date: non_empty(a.date),
        disambiguation: None,
        artist: Some(artist.clone()),
    }
}

impl From<RawAlbum> for MbReleaseGroup {
    fn from(a: RawAlbum) -> Self {
        let artist = a.artists.into_iter().next().map(|x| (x.id, x.artistname));
        MbReleaseGroup {
            releases: a.releases.into_iter().map(MbRelease::from).collect(),
            overview: non_empty(a.overview),
            images: images(a.images),
            links: links(a.links),
            genres: a.genres,
            rating: rating(a.rating),
            mbid: a.id,
            title: a.title,
            primary_type: non_empty(a.kind).unwrap_or_else(|| "Other".to_string()),
            secondary_types: a.secondarytypes,
            first_release_date: non_empty(a.releasedate),
            disambiguation: non_empty(a.disambiguation),
            artist,
        }
    }
}

impl From<RawRelease> for MbRelease {
    fn from(r: RawRelease) -> Self {
        let mut media: Vec<MbMedium> = r
            .media
            .iter()
            .map(|m| MbMedium {
                position: m.position,
                format: non_empty(m.format.clone()),
                tracks: Vec::new(),
            })
            .collect();
        for t in r.tracks {
            let disc = t.medium.max(1);
            let slot = match media.iter().position(|m| m.position == disc) {
                Some(i) => i,
                None => {
                    media.push(MbMedium {
                        position: disc,
                        format: None,
                        tracks: Vec::new(),
                    });
                    media.len() - 1
                }
            };
            media[slot].tracks.push(MbTrack {
                position: t.position,
                title: t.name,
                length_ms: t.duration,
                recording_mbid: t.recording,
            });
        }
        media.sort_by_key(|m| m.position);
        for m in &mut media {
            m.tracks.sort_by_key(|t| t.position);
        }
        MbRelease {
            mbid: r.id,
            title: r.title,
            status: non_empty(r.status),
            date: non_empty(r.date),
            country: r.country.into_iter().next(),
            media,
        }
    }
}

#[async_trait]
impl<F: Fetcher + 'static> MusicMeta for LidarrMetaSource<F> {
    async fn search_artists(&self, query: &str, limit: u32) -> Result<Vec<MbArtist>, MetaError> {
        let url = format!(
            "{}/search?type=artist&query={}",
            self.base_url,
            encode(query)
        );
        let hits: Vec<RawArtist> = self.get_json(&url).await?;
        let total = hits.len().max(1);
        Ok(hits
            .into_iter()
            .take(limit as usize)
            .enumerate()
            .map(|(i, h)| {
                let mut a = MbArtist::from(h);
                // The service ranks; it does not score. Spread 100..0 by rank.
                a.score = (100 - (i * 100 / total)) as u32;
                a
            })
            .collect())
    }

    async fn artist(&self, mbid: &str) -> Result<MbArtist, MetaError> {
        let url = format!("{}/artist/{}", self.base_url, encode(mbid));
        let raw: RawArtist = self.get_json(&url).await?;
        Ok(MbArtist::from(raw))
    }

    async fn release_groups(
        &self,
        artist_mbid: &str,
        primary_types: &[String],
    ) -> Result<Vec<MbReleaseGroup>, MetaError> {
        let url = format!("{}/artist/{}", self.base_url, encode(artist_mbid));
        let raw: RawArtist = self.get_json(&url).await?;
        let who = (raw.id.clone(), raw.artistname.clone());
        Ok(raw
            .albums
            .into_iter()
            .map(|a| group_of(a, &who))
            .filter(|g| {
                primary_types.is_empty()
                    || primary_types
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(&g.primary_type))
            })
            .collect())
    }

    async fn search_release_groups(
        &self,
        title: &str,
        artist: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MbReleaseGroup>, MetaError> {
        let query = match artist {
            Some(a) => format!("{a} {title}"),
            None => title.to_string(),
        };
        let url = format!(
            "{}/search?type=album&query={}",
            self.base_url,
            encode(&query)
        );
        let hits: Vec<RawAlbum> = self.get_json(&url).await?;
        Ok(hits
            .into_iter()
            .take(limit as usize)
            .map(MbReleaseGroup::from)
            .collect())
    }

    async fn releases(&self, release_group_mbid: &str) -> Result<Vec<MbRelease>, MetaError> {
        let url = format!("{}/album/{}", self.base_url, encode(release_group_mbid));
        let raw: RawAlbum = self.get_json(&url).await?;
        Ok(raw.releases.into_iter().map(MbRelease::from).collect())
    }
}

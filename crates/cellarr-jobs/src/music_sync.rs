//! Keeping the music library's identity in step with MusicBrainz.
//!
//! This is the one place music metadata is fetched. It adds an artist (its
//! identity and the albums a metadata profile keeps), refreshes an artist's
//! album list as new releases appear, and fetches an album's track list on
//! demand. Everything it learns is written to the database, so a library that
//! has been synced once keeps working with the network down; only adding or
//! refreshing needs MusicBrainz.

use async_trait::async_trait;
use cellarr_core::{
    Album, Artist, ContentId, LibraryId, MetadataProfile, QualityProfileId, Track,
};
use cellarr_db::{Database, DbError, NewAlbum, NewArtist, NewTrack};
use cellarr_meta::{
    pick_release, Fetcher, MbArtist, MbReleaseGroup, MetaError, MusicBrainzSource,
};

/// Which of an artist's albums start out monitored when the artist is added.
///
/// The names are the ones the Lidarr API's `addOptions.monitor` field uses, so a
/// client's choice maps straight through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorMode {
    /// Every album.
    #[default]
    All,
    /// Only albums released from today on.
    Future,
    /// Albums that have no files (every album, for a fresh artist).
    Missing,
    /// Albums that already have files (none, for a fresh artist).
    Existing,
    /// The earliest album only.
    FirstAlbum,
    /// The most recent album only.
    LatestAlbum,
    /// No albums.
    None,
}

impl MonitorMode {
    /// Parse the API's spelling, case-insensitively. Unknown values are `All`,
    /// the API's own default.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "future" => Self::Future,
            "missing" => Self::Missing,
            "existing" => Self::Existing,
            "first" | "firstalbum" => Self::FirstAlbum,
            "latest" | "latestalbum" => Self::LatestAlbum,
            "none" | "unknown" => Self::None,
            _ => Self::All,
        }
    }
}

/// Decide which albums start monitored. `dates` are each album's release date as
/// known; `today` is `yyyy-mm-dd`. The result is parallel to `dates`.
#[must_use]
pub fn monitored_for(mode: MonitorMode, dates: &[Option<String>], today: &str) -> Vec<bool> {
    // Dates compare as text: `yyyy`, `yyyy-mm` and `yyyy-mm-dd` order correctly
    // against each other, and a partial date sorts before any full one in its
    // period. An undated album is treated as the latest (it is not out yet).
    let key = |d: &Option<String>| d.clone().unwrap_or_else(|| "9999".to_string());
    match mode {
        MonitorMode::All | MonitorMode::Missing => vec![true; dates.len()],
        MonitorMode::None | MonitorMode::Existing => vec![false; dates.len()],
        MonitorMode::Future => dates
            .iter()
            .map(|d| d.as_deref().is_none_or(|d| d > today))
            .collect(),
        MonitorMode::FirstAlbum | MonitorMode::LatestAlbum => {
            let keys: Vec<String> = dates.iter().map(key).collect();
            let pick = if mode == MonitorMode::FirstAlbum {
                keys.iter().enumerate().min_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)))
            } else {
                keys.iter().enumerate().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(&a.0)))
            }
            .map(|(i, _)| i);
            (0..dates.len()).map(|i| Some(i) == pick).collect()
        }
    }
}

/// What it takes to add an artist.
#[derive(Debug, Clone)]
pub struct AddArtist {
    /// The library to add it to.
    pub library: LibraryId,
    /// MusicBrainz artist id.
    pub mbid: String,
    /// The folder the artist's albums live under.
    pub path: String,
    /// Quality profile for the artist's albums.
    pub quality_profile_id: Option<QualityProfileId>,
    /// Metadata profile deciding which albums are kept.
    pub metadata_profile_id: i64,
    /// Whether the artist is monitored.
    pub monitored: bool,
    /// Which albums start monitored.
    pub monitor: MonitorMode,
    /// Whether albums that appear later start monitored.
    pub monitor_new_albums: bool,
}

/// What a refresh found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Albums that were not known before (and passed the metadata profile).
    pub new_albums: Vec<Album>,
}

/// Errors from catalogue operations.
#[derive(Debug, thiserror::Error)]
pub enum CatalogueError {
    /// MusicBrainz could not be reached or answered badly.
    #[error("musicbrainz: {0}")]
    Metadata(#[from] MetaError),
    /// The database refused.
    #[error("database: {0}")]
    Db(#[from] DbError),
    /// The thing asked about does not exist.
    #[error("{0}")]
    NotFound(String),
}

/// The seam the API and the job handlers use to read and extend the music
/// catalogue.
#[async_trait]
pub trait MusicCatalogue: Send + Sync {
    /// Search MusicBrainz for artists by name.
    async fn search_artists(&self, term: &str) -> Result<Vec<MbArtist>, CatalogueError>;
    /// Search MusicBrainz for albums by title (optionally `artist - album`).
    async fn search_albums(&self, term: &str) -> Result<Vec<MbReleaseGroup>, CatalogueError>;
    /// Look up one artist and the albums a metadata profile would keep.
    async fn lookup_artist(
        &self,
        mbid: &str,
        profile: &MetadataProfile,
    ) -> Result<(MbArtist, Vec<MbReleaseGroup>), CatalogueError>;
    /// Add an artist with the albums its metadata profile keeps.
    async fn add_artist(&self, req: AddArtist) -> Result<Artist, CatalogueError>;
    /// Re-read an artist's albums from MusicBrainz.
    async fn refresh_artist(&self, artist: ContentId) -> Result<RefreshReport, CatalogueError>;
    /// Make sure an album has its track list, fetching it if it has none.
    async fn ensure_tracks(&self, album: ContentId) -> Result<Vec<Track>, CatalogueError>;
}

/// The MusicBrainz-backed catalogue.
pub struct MusicSync<F: Fetcher> {
    db: Database,
    mb: MusicBrainzSource<F>,
}

impl<F: Fetcher> MusicSync<F> {
    /// A catalogue over `db`, reading MusicBrainz through `mb`.
    pub fn new(db: Database, mb: MusicBrainzSource<F>) -> Self {
        Self { db, mb }
    }

    async fn profile(&self, id: i64) -> Result<MetadataProfile, CatalogueError> {
        Ok(self
            .db
            .music()
            .metadata_profile(id)
            .await?
            .unwrap_or_else(MetadataProfile::standard))
    }

    /// The release groups of `artist_mbid` that `profile` keeps, oldest first.
    async fn kept_groups(
        &self,
        artist_mbid: &str,
        profile: &MetadataProfile,
    ) -> Result<Vec<MbReleaseGroup>, CatalogueError> {
        let mut groups: Vec<MbReleaseGroup> = self
            .mb
            .release_groups(artist_mbid, &profile.primary_types)
            .await?
            .into_iter()
            .filter(|g| profile.allows(&g.primary_type, &g.secondary_types))
            .collect();
        groups.sort_by(|a, b| {
            a.first_release_date
                .cmp(&b.first_release_date)
                .then(a.title.cmp(&b.title))
        });
        Ok(groups)
    }
}

fn today() -> String {
    time::OffsetDateTime::now_utc().date().to_string()
}

fn new_album(g: &MbReleaseGroup, monitored: bool) -> NewAlbum {
    NewAlbum {
        id: None,
        mbid: g.mbid.clone(),
        title: g.title.clone(),
        disambiguation: g.disambiguation.clone(),
        album_type: g.primary_type.clone(),
        secondary_types: g.secondary_types.clone(),
        release_date: g.first_release_date.clone(),
        release_mbid: None,
        monitored,
        added_at: None,
    }
}

#[async_trait]
impl<F: Fetcher + 'static> MusicCatalogue for MusicSync<F> {
    async fn search_artists(&self, term: &str) -> Result<Vec<MbArtist>, CatalogueError> {
        Ok(self.mb.search_artists(term, 10).await?)
    }

    async fn search_albums(&self, term: &str) -> Result<Vec<MbReleaseGroup>, CatalogueError> {
        // `artist - album` narrows the search to that artist's albums.
        let (artist, title) = match term.split_once(" - ") {
            Some((a, t)) => (Some(a.trim()), t.trim()),
            None => (None, term.trim()),
        };
        Ok(self.mb.search_release_groups(title, artist, 10).await?)
    }

    async fn lookup_artist(
        &self,
        mbid: &str,
        profile: &MetadataProfile,
    ) -> Result<(MbArtist, Vec<MbReleaseGroup>), CatalogueError> {
        let artist = self.mb.artist(mbid).await?;
        let groups = self.kept_groups(mbid, profile).await?;
        Ok((artist, groups))
    }

    async fn add_artist(&self, req: AddArtist) -> Result<Artist, CatalogueError> {
        if let Some(existing) = self.db.music().artist_by_mbid(&req.mbid).await? {
            return Ok(existing);
        }
        let profile = self.profile(req.metadata_profile_id).await?;
        let (mb, groups) = self.lookup_artist(&req.mbid, &profile).await?;
        let artist = self
            .db
            .music()
            .add_artist(
                req.library,
                NewArtist {
                    id: None,
                    mbid: mb.mbid.clone(),
                    name: mb.name.clone(),
                    sort_name: mb.sort_name.clone(),
                    disambiguation: mb.disambiguation.clone(),
                    artist_type: mb.artist_type.clone(),
                    ended: mb.ended,
                    overview: None,
                    genres: mb.genres.clone(),
                    aliases: mb.aliases.clone(),
                    path: req.path,
                    quality_profile_id: req.quality_profile_id,
                    metadata_profile_id: req.metadata_profile_id,
                    monitored: req.monitored,
                    monitor_new_albums: req.monitor_new_albums,
                    added_at: None,
                },
            )
            .await?;
        let dates: Vec<Option<String>> = groups.iter().map(|g| g.first_release_date.clone()).collect();
        let flags = monitored_for(req.monitor, &dates, &today());
        let albums: Vec<NewAlbum> = groups
            .iter()
            .zip(flags)
            .map(|(g, monitored)| new_album(g, monitored))
            .collect();
        self.db
            .music()
            .upsert_albums(req.library, artist.content_id, albums)
            .await?;
        self.db.music().mark_artist_refreshed(artist.content_id).await?;
        Ok(artist)
    }

    async fn refresh_artist(&self, artist: ContentId) -> Result<RefreshReport, CatalogueError> {
        let music = self.db.music();
        let Some(mut known) = music.artist_by_content(artist).await? else {
            return Err(CatalogueError::NotFound(format!("no artist {artist}")));
        };
        let node = cellarr_core::repo::ContentRepository::get(&self.db.content(), artist)
            .await?
            .ok_or_else(|| CatalogueError::NotFound(format!("no artist node {artist}")))?;
        let profile = self.profile(known.metadata_profile_id).await?;
        let mb = self.mb.artist(&known.mbid).await?;
        let groups = self.kept_groups(&known.mbid, &profile).await?;

        known.name = mb.name.clone();
        known.sort_name = mb.sort_name.clone();
        known.disambiguation = mb.disambiguation.clone();
        known.artist_type = mb.artist_type.clone();
        known.ended = mb.ended;
        known.genres = mb.genres.clone();
        music.update_artist(&known, Some(mb.aliases.clone())).await?;

        // A new album starts monitored only if the artist asks for that, and
        // never if the artist itself is unmonitored. Existing albums keep the
        // monitored flag the user gave them (the repository guarantees that).
        let monitored = known.monitor_new_albums && known.monitored;
        let albums: Vec<NewAlbum> = groups.iter().map(|g| new_album(g, monitored)).collect();
        let inserted = music.upsert_albums(node.library_id, artist, albums).await?;
        music.mark_artist_refreshed(artist).await?;
        Ok(RefreshReport {
            new_albums: inserted,
        })
    }

    async fn ensure_tracks(&self, album: ContentId) -> Result<Vec<Track>, CatalogueError> {
        let music = self.db.music();
        let Some(stored) = music.album_by_content(album).await? else {
            return Err(CatalogueError::NotFound(format!("no album {album}")));
        };
        if stored.tracks_fetched_at.is_some() {
            return Ok(music.tracks_for_album(album).await?);
        }
        let node = cellarr_core::repo::ContentRepository::get(&self.db.content(), album)
            .await?
            .ok_or_else(|| CatalogueError::NotFound(format!("no album node {album}")))?;
        let releases = self.mb.releases(&stored.mbid).await?;
        let allowed = ["Official".to_string()];
        let Some(release) = pick_release(&releases, &allowed) else {
            return Ok(Vec::new());
        };
        let tracks: Vec<NewTrack> = release
            .media
            .iter()
            .flat_map(|m| {
                m.tracks.iter().map(move |t| NewTrack {
                    id: None,
                    mbid: t.recording_mbid.clone(),
                    title: t.title.clone(),
                    disc: m.position,
                    position: t.position,
                    duration_ms: t.length_ms,
                })
            })
            .collect();
        Ok(music
            .set_album_tracks(node.library_id, album, Some(release.mbid.clone()), tracks)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn every_mode_picks_the_albums_its_name_promises() {
        let dates = vec![d("1993"), d("1997-05-21"), d("2016-05-08"), None];
        let t = |m| monitored_for(m, &dates, "2020-01-01");
        assert_eq!(t(MonitorMode::All), [true; 4]);
        assert_eq!(t(MonitorMode::None), [false; 4]);
        assert_eq!(t(MonitorMode::Missing), [true; 4]);
        assert_eq!(t(MonitorMode::Existing), [false; 4]);
        assert_eq!(t(MonitorMode::Future), [false, false, false, true]);
        assert_eq!(t(MonitorMode::FirstAlbum), [true, false, false, false]);
        assert_eq!(t(MonitorMode::LatestAlbum), [false, false, false, true]);
    }

    #[test]
    fn the_api_spellings_parse() {
        assert_eq!(MonitorMode::parse("all"), MonitorMode::All);
        assert_eq!(MonitorMode::parse("firstAlbum"), MonitorMode::FirstAlbum);
        assert_eq!(MonitorMode::parse("latest"), MonitorMode::LatestAlbum);
        assert_eq!(MonitorMode::parse("future"), MonitorMode::Future);
        assert_eq!(MonitorMode::parse("none"), MonitorMode::None);
        assert_eq!(MonitorMode::parse("bogus"), MonitorMode::All);
    }
}

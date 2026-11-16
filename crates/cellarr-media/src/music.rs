//! The music module: how an album is searched for, how a release is matched to
//! an album, and how a track's file is named.
//!
//! Music is acquired an album at a time. The unit this module speaks about to
//! the pipeline is therefore the **album** node (coordinates
//! [`Coordinates::Album`]): it is what gets searched, what a release is matched
//! against, and what a grab is addressed to. Tracks appear only at naming time,
//! when the files inside a finished download are placed.

use async_trait::async_trait;
use cellarr_core::music::normalize_name;
use cellarr_core::{
    Confidence, ContentId, ContentMatch, ContentRef, Coordinates, MediaModule, MediaType,
    NamingTokens, ParsedRelease, SearchTerms,
};

use crate::error::MediaError;
use crate::module::AMBIGUOUS_CONFIDENCE;

/// The Torznab category range for audio.
const AUDIO_CATEGORY: u32 = 3000;

/// Confidence of a release whose artist and album both match as written.
const CONF_EXACT: f32 = 1.0;
/// Confidence of a release that matches only once an edition suffix is set
/// aside (`Nevermind (Deluxe Edition)` for `Nevermind`), or whose title named
/// artist and album without separating them.
const CONF_LOOSE: f32 = 0.85;

/// Who an album is by and what it is called — what a search and a file name are
/// built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumIdentity {
    /// The artist's display name.
    pub artist_name: String,
    /// The artist's folder, relative to the library root.
    pub artist_folder: String,
    /// The album title.
    pub album_title: String,
    /// The release year, when known.
    pub year: Option<u16>,
    /// How many discs the album's track list spans (0 when tracks are unknown).
    pub disc_count: u32,
}

/// A track within its album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackIdentity {
    /// The album the track is on.
    pub album: AlbumIdentity,
    /// The track title.
    pub title: String,
    /// Disc number.
    pub disc: u32,
    /// Position on the disc.
    pub position: u32,
}

/// An album a release title might be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumCandidate {
    /// The album node.
    pub content_ref: ContentRef,
    /// The release year, used to tell same-named albums apart.
    pub year: Option<u16>,
}

/// The library reads the music module needs. Implemented over the database by
/// the daemon and by an in-memory fake in tests.
#[async_trait]
pub trait MusicLookup: Send + Sync {
    /// The lookup's error type.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The identity of an album node, or `None` if it is not an album.
    async fn album(&self, album: ContentId) -> Result<Option<AlbumIdentity>, Self::Error>;

    /// The identity of a track node, or `None` if it is not a track.
    async fn track(&self, track: ContentId) -> Result<Option<TrackIdentity>, Self::Error>;

    /// Albums whose artist (name or alias) and title equal the given normalized
    /// names; any of `album_norms` may match.
    async fn albums_named(
        &self,
        artist_norm: &str,
        album_norms: &[String],
    ) -> Result<Vec<AlbumCandidate>, Self::Error>;

    /// Albums for which `"<artist> <album>"`, normalized, equals `blob`.
    async fn albums_named_jointly(&self, blob: &str)
        -> Result<Vec<AlbumCandidate>, Self::Error>;
}

/// Errors from the music module.
#[derive(Debug, thiserror::Error)]
pub enum MusicModuleError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    /// A media-layer error (wrong media type, unresolved identity).
    #[error(transparent)]
    Media(#[from] MediaError),
    /// The library lookup failed.
    #[error("music lookup failed: {0}")]
    Lookup(#[source] E),
}

/// The music [`MediaModule`].
pub struct MusicModule<L> {
    lookup: L,
}

impl<L> MusicModule<L> {
    /// Build the module over a library lookup.
    pub fn new(lookup: L) -> Self {
        Self { lookup }
    }
}

#[async_trait]
impl<L> MediaModule for MusicModule<L>
where
    L: MusicLookup,
{
    type Error = MusicModuleError<L::Error>;

    fn media_type(&self) -> MediaType {
        MediaType::Music
    }

    async fn search_terms(&self, content: &ContentRef) -> Result<SearchTerms, Self::Error> {
        ensure_music(content)?;
        let album = self
            .lookup
            .album(content.id)
            .await
            .map_err(MusicModuleError::Lookup)?
            .ok_or_else(|| MediaError::UnresolvedIdentity(content.id.to_string()))?;
        Ok(SearchTerms {
            // Free text for indexers without a typed music search.
            queries: vec![format!("{} {}", album.artist_name, album.album_title)],
            // The typed music search takes the two names in their own params.
            ids: vec![
                ("artist".to_string(), album.artist_name),
                ("album".to_string(), album.album_title),
            ],
            numbering: Vec::new(),
            categories: vec![AUDIO_CATEGORY],
        })
    }

    async fn match_release(
        &self,
        parsed: &ParsedRelease,
    ) -> Result<Vec<ContentMatch>, Self::Error> {
        // Only a title read as music can match an album, and a discography is
        // not one album: importing it against a single album would scatter an
        // artist's whole catalogue into one folder.
        let Some(music) = &parsed.music else {
            return Ok(Vec::new());
        };
        if music.discography {
            return Ok(Vec::new());
        }

        let mut raw: Vec<(AlbumCandidate, f32)> = Vec::new();
        match (&music.artist, &music.album) {
            (Some(artist), Some(album)) => {
                let exact = normalize_name(album);
                let loose = normalize_name(&strip_edition(album));
                let mut variants = vec![exact.clone()];
                if loose != exact && !loose.is_empty() {
                    variants.push(loose);
                }
                let found = self
                    .lookup
                    .albums_named(&normalize_name(artist), &variants)
                    .await
                    .map_err(MusicModuleError::Lookup)?;
                // A candidate found under the exact title outranks one found
                // only once the edition suffix was dropped. The lookup returns
                // exact-variant hits first, so the first sighting of a node is
                // its best.
                let exact_hits = self
                    .lookup
                    .albums_named(&normalize_name(artist), &variants[..1])
                    .await
                    .map_err(MusicModuleError::Lookup)?;
                for cand in found {
                    if raw.iter().any(|(c, _)| c.content_ref.id == cand.content_ref.id) {
                        continue;
                    }
                    let is_exact = exact_hits
                        .iter()
                        .any(|e| e.content_ref.id == cand.content_ref.id);
                    raw.push((cand, if is_exact { CONF_EXACT } else { CONF_LOOSE }));
                }
            }
            _ => {
                let blob = normalize_name(parsed.clean_title.as_deref().unwrap_or(&parsed.raw_title));
                if !blob.is_empty() {
                    for cand in self
                        .lookup
                        .albums_named_jointly(&blob)
                        .await
                        .map_err(MusicModuleError::Lookup)?
                    {
                        if !raw.iter().any(|(c, _)| c.content_ref.id == cand.content_ref.id) {
                            raw.push((cand, CONF_LOOSE));
                        }
                    }
                }
            }
        }
        Ok(finalize(raw, parsed.year))
    }

    async fn naming_tokens(&self, content: &ContentRef) -> Result<NamingTokens, Self::Error> {
        ensure_music(content)?;
        let Coordinates::Track { .. } = content.coords else {
            // An album has no file of its own; its tokens name the folder.
            let album = self
                .lookup
                .album(content.id)
                .await
                .map_err(MusicModuleError::Lookup)?
                .ok_or_else(|| MediaError::UnresolvedIdentity(content.id.to_string()))?;
            return Ok(NamingTokens {
                tokens: album_tokens(&album),
            });
        };
        let track = self
            .lookup
            .track(content.id)
            .await
            .map_err(MusicModuleError::Lookup)?
            .ok_or_else(|| MediaError::UnresolvedIdentity(content.id.to_string()))?;
        let mut tokens = album_tokens(&track.album);
        tokens.push(("Track Title".to_string(), track.title));
        tokens.push(("Track".to_string(), format!("{:02}", track.position)));
        // The disc is only a token on a multi-disc album: its presence is what
        // selects the format with a per-disc folder.
        if track.album.disc_count > 1 {
            tokens.push(("Medium".to_string(), format!("{:02}", track.disc)));
        }
        Ok(NamingTokens { tokens })
    }
}

fn album_tokens(album: &AlbumIdentity) -> Vec<(String, String)> {
    let mut tokens = vec![
        ("Artist Name".to_string(), album.artist_name.clone()),
        ("Artist Folder".to_string(), album.artist_folder.clone()),
        ("Album Title".to_string(), album.album_title.clone()),
    ];
    if let Some(year) = album.year {
        tokens.push(("Release Year".to_string(), year.to_string()));
    }
    tokens
}

fn ensure_music(content: &ContentRef) -> Result<(), MediaError> {
    if content.media_type == MediaType::Music {
        Ok(())
    } else {
        Err(MediaError::WrongMediaType {
            expected: MediaType::Music,
            coords: format!("{:?}", content.coords),
        })
    }
}

/// An album title without its trailing edition suffix.
///
/// Releases are often titled with the edition they are (`(Deluxe Edition)`,
/// `[Remastered]`, `(2011 Remaster)`); the album they are an edition *of* is the
/// title without it. Only trailing bracketed groups are removed, so a
/// parenthetical in the middle of a name, or a title that is nothing but a
/// bracketed group (Sigur Rós's `( )`), is left alone.
fn strip_edition(title: &str) -> String {
    let mut out = title.trim().to_string();
    loop {
        let trimmed = out.trim_end();
        let Some(close) = trimmed.chars().last() else {
            break;
        };
        let open = match close {
            ')' => '(',
            ']' => '[',
            _ => break,
        };
        let Some(start) = trimmed.rfind(open) else {
            break;
        };
        if start == 0 {
            break;
        }
        out = trimmed[..start].trim_end().to_string();
    }
    out
}

/// Turn candidates into matches, resolving same-named albums by year.
///
/// One candidate keeps its earned confidence. Several candidates are rivals —
/// an artist with two self-titled albums — and the release year is the only
/// evidence that separates them: a candidate released in the year the title
/// states keeps its confidence and the rest are demoted; with no year to go on,
/// every rival is demoted, so nothing is grabbed on a coin toss.
fn finalize(raw: Vec<(AlbumCandidate, f32)>, release_year: Option<u16>) -> Vec<ContentMatch> {
    let rivals = raw.len() > 1;
    let year_matches = |c: &AlbumCandidate| release_year.is_some() && c.year == release_year;
    let unique_year_match = raw.iter().filter(|(c, _)| year_matches(c)).count() == 1;
    raw.into_iter()
        .map(|(cand, conf)| {
            let confident = !rivals || (unique_year_match && year_matches(&cand));
            ContentMatch {
                content_ref: cand.content_ref,
                confidence: Confidence::new(if confident { conf } else { AMBIGUOUS_CONFIDENCE }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cellarr_core::LibraryId;
    use std::convert::Infallible;

    struct Shelf {
        albums: Vec<(ContentRef, AlbumIdentity)>,
    }

    fn album_ref() -> ContentRef {
        ContentRef {
            id: ContentId::new(),
            library_id: LibraryId::new(),
            media_type: MediaType::Music,
            coords: Coordinates::Album,
        }
    }

    impl Shelf {
        fn with(albums: &[(&str, &str, Option<u16>)]) -> Self {
            Self {
                albums: albums
                    .iter()
                    .map(|(artist, title, year)| {
                        (
                            album_ref(),
                            AlbumIdentity {
                                artist_name: (*artist).to_string(),
                                artist_folder: (*artist).to_string(),
                                album_title: (*title).to_string(),
                                year: *year,
                                disc_count: 1,
                            },
                        )
                    })
                    .collect(),
            }
        }

        fn id_of(&self, title: &str, year: Option<u16>) -> ContentId {
            self.albums
                .iter()
                .find(|(_, a)| a.album_title == title && a.year == year)
                .map(|(r, _)| r.id)
                .unwrap()
        }
    }

    #[async_trait]
    impl MusicLookup for Shelf {
        type Error = Infallible;

        async fn album(&self, album: ContentId) -> Result<Option<AlbumIdentity>, Infallible> {
            Ok(self
                .albums
                .iter()
                .find(|(r, _)| r.id == album)
                .map(|(_, a)| a.clone()))
        }

        async fn track(&self, _track: ContentId) -> Result<Option<TrackIdentity>, Infallible> {
            Ok(self.albums.first().map(|(_, a)| TrackIdentity {
                album: AlbumIdentity {
                    disc_count: 2,
                    ..a.clone()
                },
                title: "Airbag".to_string(),
                disc: 2,
                position: 3,
            }))
        }

        async fn albums_named(
            &self,
            artist_norm: &str,
            album_norms: &[String],
        ) -> Result<Vec<AlbumCandidate>, Infallible> {
            let mut out = Vec::new();
            for wanted in album_norms {
                for (r, a) in &self.albums {
                    if normalize_name(&a.artist_name) == artist_norm
                        && &normalize_name(&a.album_title) == wanted
                    {
                        out.push(AlbumCandidate {
                            content_ref: r.clone(),
                            year: a.year,
                        });
                    }
                }
            }
            Ok(out)
        }

        async fn albums_named_jointly(&self, blob: &str) -> Result<Vec<AlbumCandidate>, Infallible> {
            Ok(self
                .albums
                .iter()
                .filter(|(_, a)| {
                    format!("{} {}", normalize_name(&a.artist_name), normalize_name(&a.album_title))
                        == blob
                })
                .map(|(r, a)| AlbumCandidate {
                    content_ref: r.clone(),
                    year: a.year,
                })
                .collect())
        }
    }

    async fn matches(shelf: Shelf, title: &str) -> (Shelf, Vec<ContentMatch>) {
        let parsed = cellarr_parse_stub::parse(title);
        let module = MusicModule::new(shelf);
        let out = module.match_release(&parsed).await.unwrap();
        (module.lookup, out)
    }

    /// The module is tested without depending on the parser crate: these build
    /// the parse a music title would produce.
    mod cellarr_parse_stub {
        use cellarr_core::{Coordinates, MusicRelease, ParsedRelease};

        pub fn parse(title: &str) -> ParsedRelease {
            let mut p = ParsedRelease::new(title);
            let (names, year) = match title.rsplit_once(" @") {
                Some((names, year)) => (names, year.parse().ok()),
                None => (title, None),
            };
            p.year = year;
            let (artist, album) = match names.split_once(" - ") {
                Some((a, b)) => (Some(a.to_string()), Some(b.to_string())),
                None => (None, None),
            };
            p.clean_title = Some(names.to_string());
            let discography = names.contains("Discography");
            if !discography {
                p.coordinates.push(Coordinates::Album);
            }
            p.music = Some(MusicRelease {
                artist,
                album,
                quality: Some("FLAC".to_string()),
                discography,
            });
            p
        }
    }

    #[tokio::test]
    async fn an_exact_artist_and_album_is_a_certain_match() {
        let shelf = Shelf::with(&[("Radiohead", "OK Computer", Some(1997))]);
        let (shelf, out) = matches(shelf, "radiohead - ok.computer").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content_ref.id, shelf.id_of("OK Computer", Some(1997)));
        assert_eq!(out[0].confidence.value(), CONF_EXACT);
    }

    #[tokio::test]
    async fn an_edition_suffix_still_matches_but_less_confidently() {
        let shelf = Shelf::with(&[("Nirvana", "Nevermind", Some(1991))]);
        let (_, out) = matches(shelf, "Nirvana - Nevermind (Deluxe Edition) [Remastered]").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence.value(), CONF_LOOSE);
    }

    #[tokio::test]
    async fn another_artists_album_of_the_same_name_does_not_match() {
        let shelf = Shelf::with(&[("Weezer", "Weezer", Some(1994))]);
        let (_, out) = matches(shelf, "Somebody Else - Weezer").await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn same_named_albums_are_told_apart_by_the_release_year() {
        let shelf = Shelf::with(&[
            ("Weezer", "Weezer", Some(1994)),
            ("Weezer", "Weezer", Some(2001)),
        ]);
        let (shelf, out) = matches(shelf, "Weezer - Weezer @2001").await;
        let green = shelf.id_of("Weezer", Some(2001));
        for m in &out {
            if m.content_ref.id == green {
                assert_eq!(m.confidence.value(), CONF_EXACT);
            } else {
                assert_eq!(m.confidence.value(), AMBIGUOUS_CONFIDENCE);
            }
        }
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn same_named_albums_with_no_year_are_all_ambiguous() {
        let shelf = Shelf::with(&[
            ("Weezer", "Weezer", Some(1994)),
            ("Weezer", "Weezer", Some(2001)),
        ]);
        let (_, out) = matches(shelf, "Weezer - Weezer").await;
        assert!(out.iter().all(|m| m.confidence.value() == AMBIGUOUS_CONFIDENCE));
    }

    #[tokio::test]
    async fn a_title_without_a_separator_matches_jointly() {
        let shelf = Shelf::with(&[("Boards of Canada", "Geogaddi", Some(2002))]);
        let (_, out) = matches(shelf, "Boards of Canada Geogaddi").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence.value(), CONF_LOOSE);
    }

    #[tokio::test]
    async fn a_discography_matches_no_album() {
        let shelf = Shelf::with(&[("Radiohead", "Discography", None)]);
        let (_, out) = matches(shelf, "Radiohead - Discography").await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn a_video_parse_matches_nothing() {
        let shelf = Shelf::with(&[("Radiohead", "OK Computer", Some(1997))]);
        let module = MusicModule::new(shelf);
        let parsed = ParsedRelease::new("Radiohead - OK Computer");
        assert!(module.match_release(&parsed).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_terms_carry_typed_and_free_text_forms() {
        let shelf = Shelf::with(&[("Radiohead", "OK Computer", Some(1997))]);
        let id = shelf.id_of("OK Computer", Some(1997));
        let module = MusicModule::new(shelf);
        let mut content = album_ref();
        content.id = id;
        let terms = module.search_terms(&content).await.unwrap();
        assert_eq!(terms.queries, vec!["Radiohead OK Computer".to_string()]);
        assert!(terms.ids.contains(&("artist".to_string(), "Radiohead".to_string())));
        assert!(terms.ids.contains(&("album".to_string(), "OK Computer".to_string())));
        assert_eq!(terms.categories, vec![3000]);
    }

    #[tokio::test]
    async fn a_track_on_a_multi_disc_album_gets_a_medium_token() {
        let shelf = Shelf::with(&[("Radiohead", "OK Computer", Some(1997))]);
        let module = MusicModule::new(shelf);
        let track = ContentRef {
            coords: Coordinates::Track { disc: 2, track: 3 },
            ..album_ref()
        };
        let tokens = module.naming_tokens(&track).await.unwrap().tokens;
        let get = |k: &str| tokens.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Artist Name"), Some("Radiohead"));
        assert_eq!(get("Album Title"), Some("OK Computer"));
        assert_eq!(get("Release Year"), Some("1997"));
        assert_eq!(get("Track"), Some("03"));
        assert_eq!(get("Medium"), Some("02"));
        assert_eq!(get("Track Title"), Some("Airbag"));
    }

    #[test]
    fn only_trailing_edition_groups_are_stripped() {
        assert_eq!(strip_edition("Nevermind (Deluxe Edition)"), "Nevermind");
        assert_eq!(strip_edition("Nevermind (Deluxe) [Remastered]"), "Nevermind");
        assert_eq!(strip_edition("(What's the Story) Morning Glory?"), "(What's the Story) Morning Glory?");
        assert_eq!(strip_edition("( )"), "( )");
        assert_eq!(strip_edition("Plain"), "Plain");
    }
}

//! The music vocabulary: artists, albums, tracks and metadata profiles.
//!
//! The structural tree is generic (`artist → album → track` are ordinary content
//! nodes); these are the typed identity records that hang off those nodes. An
//! **album** here is a MusicBrainz *release group* — the abstract album, not one
//! particular pressing — because that is the unit a listener wants and a release
//! is named after. The pressing whose track list is used is recorded on the album
//! as its selected release.
//!
//! Every record carries a small **sequence id** as well as its content id. The
//! sequence id is the integer the Lidarr-compatible API speaks; it is assigned
//! once, never reused, and preserved across a migration so that anything keyed
//! on an existing install's ids keeps working.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::{ContentId, QualityProfileId};

/// Build a UUID that carries `seq` where the compatibility APIs read an integer
/// id from.
///
/// Those APIs expose integer ids, and project one from a UUID by reading its
/// first eight bytes (see [`seq_of_uuid`]). A random UUID therefore yields a
/// random 31-bit integer, which collides once a library holds tens of thousands
/// of albums. Music nodes instead get a UUID whose leading bytes are zero and
/// whose next four are the sequence id, so the projected integer **is** the
/// sequence id and is unique by construction. The remaining eight bytes are
/// random, so two kinds sharing a sequence number still get distinct UUIDs.
///
/// The textual form begins `00000000-HHHH-HHHH-`, which makes a lookup by
/// sequence id a primary-key range scan.
#[must_use]
pub fn seq_uuid(seq: u32) -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[..4].fill(0);
    bytes[4..8].copy_from_slice(&seq.to_be_bytes());
    Uuid::from_bytes(bytes)
}

/// The integer a compatibility API reports for a UUID: its first eight bytes,
/// big-endian, masked to a positive 32-bit value.
#[must_use]
pub fn seq_of_uuid(id: Uuid) -> i64 {
    let mut n: i64 = 0;
    for b in &id.as_bytes()[..8] {
        n = (n << 8) | i64::from(*b);
    }
    n & i64::from(i32::MAX)
}

/// Reduce a name to the form two spellings of it are compared in.
///
/// Lower-cased, with every run of non-alphanumeric characters collapsed to one
/// space and the ends trimmed, so `OK Computer`, `ok.computer` and
/// `OK_Computer!` are the same name. This one function is used by both the
/// stored, indexed form of a name and the release matcher, so the two can never
/// disagree about what counts as equal.
#[must_use]
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_space = true;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_was_space = false;
        } else if ch == '&' {
            // `&` and `and` are the same word in a band or album name.
            if !last_was_space {
                out.push(' ');
            }
            out.push_str("and ");
            last_was_space = true;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// An artist's identity record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    /// The sequence id (the Lidarr-face `id`).
    pub id: i64,
    /// The artist's content node.
    pub content_id: ContentId,
    /// MusicBrainz artist id.
    pub mbid: String,
    /// Display name.
    pub name: String,
    /// Sort name (`Beatles, The`), when it differs.
    pub sort_name: Option<String>,
    /// MusicBrainz disambiguation comment (`UK rock band`).
    pub disambiguation: Option<String>,
    /// `Person`, `Group`, …, as MusicBrainz types the artist.
    pub artist_type: Option<String>,
    /// Whether the artist has ended (disbanded / deceased).
    pub ended: bool,
    /// A short biography, when one is known.
    pub overview: Option<String>,
    /// Genre names.
    pub genres: Vec<String>,
    /// The artist's folder under the library root.
    pub path: String,
    /// The quality profile the artist's albums are graded with.
    pub quality_profile_id: Option<QualityProfileId>,
    /// The metadata profile that decides which of the artist's albums are kept.
    pub metadata_profile_id: i64,
    /// Whether the artist is monitored (mirrors the content node's flag).
    pub monitored: bool,
    /// Whether albums discovered on a later refresh start out monitored.
    pub monitor_new_albums: bool,
    /// When the artist was added (RFC 3339).
    pub added_at: String,
    /// When the album list was last refreshed from the metadata source.
    pub refreshed_at: Option<String>,
    /// Rich metadata (links, artwork, rating) as a JSON document, when a source
    /// supplied it.
    #[serde(default)]
    pub extras: Option<String>,
}

/// An album's identity record (a MusicBrainz release group).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    /// The sequence id (the Lidarr-face `id`).
    pub id: i64,
    /// The album's content node.
    pub content_id: ContentId,
    /// The owning artist's content node.
    pub artist_content_id: ContentId,
    /// MusicBrainz release-group id.
    pub mbid: String,
    /// Album title.
    pub title: String,
    /// MusicBrainz disambiguation comment.
    pub disambiguation: Option<String>,
    /// Primary type: `Album`, `EP`, `Single`, `Broadcast` or `Other`.
    pub album_type: String,
    /// Secondary types (`Live`, `Compilation`, `Soundtrack`, …); empty for a
    /// plain studio album.
    pub secondary_types: Vec<String>,
    /// First release date, as precise as known (`1997`, `1997-05`, `1997-05-21`).
    pub release_date: Option<String>,
    /// Whether the album is monitored (mirrors the content node's flag).
    pub monitored: bool,
    /// The MusicBrainz release whose track list this album uses, once chosen.
    pub release_mbid: Option<String>,
    /// When the album was added (RFC 3339).
    pub added_at: String,
    /// When the track list was fetched; `None` means tracks are not yet known.
    pub tracks_fetched_at: Option<String>,
}

impl Album {
    /// The release year, when the release date is known.
    #[must_use]
    pub fn year(&self) -> Option<u16> {
        self.release_date
            .as_deref()
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok())
    }
}

/// A track's identity record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    /// The sequence id (the Lidarr-face `id`).
    pub id: i64,
    /// The track's content node.
    pub content_id: ContentId,
    /// The owning album's content node.
    pub album_content_id: ContentId,
    /// MusicBrainz recording id, when known.
    pub mbid: Option<String>,
    /// Track title.
    pub title: String,
    /// Disc (medium) number, 1-based.
    pub disc: u32,
    /// Position on the disc, 1-based.
    pub position: u32,
    /// Length in milliseconds, when known.
    pub duration_ms: Option<u32>,
}

/// A file that satisfies a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackFile {
    /// The sequence id (the Lidarr-face track-file `id`).
    pub id: i64,
    /// The underlying media file.
    pub file: crate::media::MediaFile,
    /// The track it satisfies.
    pub track_content_id: ContentId,
    /// That track's album.
    pub album_content_id: ContentId,
    /// That album's artist.
    pub artist_content_id: ContentId,
    /// When the file was imported (RFC 3339).
    pub added_at: String,
}

/// Which of an artist's release groups are worth having.
///
/// MusicBrainz lists everything an artist ever put out: studio albums, but also
/// singles, live bootlegs, compilations and promos. A metadata profile is the
/// filter between that list and the library. A release group is kept when its
/// primary type is allowed **and** every one of its secondary types is allowed
/// (a group with no secondary type is a studio release and needs `Studio`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataProfile {
    /// The profile's integer id.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// Allowed primary types (`Album`, `EP`, `Single`, `Broadcast`, `Other`).
    pub primary_types: Vec<String>,
    /// Allowed secondary types, where `Studio` stands for "none".
    pub secondary_types: Vec<String>,
    /// Allowed release statuses (`Official`, `Promotion`, `Bootleg`,
    /// `Pseudo-Release`).
    pub release_statuses: Vec<String>,
}

impl MetadataProfile {
    /// The profile a fresh install starts with: official studio albums only.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            id: 1,
            name: "Standard".to_string(),
            primary_types: vec!["Album".to_string()],
            secondary_types: vec!["Studio".to_string()],
            release_statuses: vec!["Official".to_string()],
        }
    }

    /// Whether a release group of this primary type and these secondary types
    /// passes the profile.
    #[must_use]
    pub fn allows(&self, primary: &str, secondary: &[String]) -> bool {
        let has = |list: &[String], value: &str| list.iter().any(|v| v.eq_ignore_ascii_case(value));
        if !has(&self.primary_types, primary) {
            return false;
        }
        if secondary.is_empty() {
            return has(&self.secondary_types, "Studio");
        }
        secondary.iter().all(|s| has(&self.secondary_types, s))
    }
}

/// `(cellarr quality name, Lidarr quality id)`.
///
/// Lidarr numbers its qualities in its source, and stores those numbers in every
/// profile and file record of an install. cellarr identifies a quality by name,
/// so this is the bridge: the compatibility API speaks the numbers, and the
/// migration reads them out of an existing database. It lives here, below both,
/// so neither depends on the other for it.
pub const LIDARR_QUALITY_IDS: &[(&str, u32)] = &[
    ("Unknown", 0),
    ("MP3-192", 1),
    ("MP3-VBR-V0", 2),
    ("MP3-256", 3),
    ("MP3-320", 4),
    ("MP3-160", 5),
    ("FLAC", 6),
    ("ALAC", 7),
    ("MP3-VBR-V2", 8),
    ("AAC-192", 9),
    ("AAC-256", 10),
    ("AAC-320", 11),
    ("AAC-VBR", 12),
    ("WAV", 13),
    ("OGG Vorbis Q10", 14),
    ("OGG Vorbis Q9", 15),
    ("OGG Vorbis Q8", 16),
    ("OGG Vorbis Q7", 17),
    ("OGG Vorbis Q6", 18),
    ("OGG Vorbis Q5", 19),
    ("WMA", 20),
    ("FLAC 24bit", 21),
    ("MP3-128", 22),
    ("MP3-96", 23),
    ("MP3-80", 24),
    ("MP3-64", 25),
    ("MP3-56", 26),
    ("MP3-48", 27),
    ("MP3-40", 28),
    ("MP3-32", 29),
    ("MP3-24", 30),
    ("MP3-16", 31),
    ("MP3-8", 32),
    ("MP3-112", 33),
    ("MP3-224", 34),
    ("APE", 35),
    ("WavPack", 36),
    ("ALAC 24bit", 37),
];

/// The Lidarr id of a cellarr quality name (0 — Unknown — when Lidarr has none).
#[must_use]
pub fn lidarr_quality_id(name: &str) -> u32 {
    LIDARR_QUALITY_IDS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map_or(0, |(_, id)| *id)
}

/// The cellarr quality name a Lidarr quality id stands for.
#[must_use]
pub fn lidarr_quality_name(id: u32) -> Option<&'static str> {
    LIDARR_QUALITY_IDS
        .iter()
        .find(|(_, i)| *i == id)
        .map(|(n, _)| *n)
}

/// Every primary type MusicBrainz assigns a release group.
pub const PRIMARY_ALBUM_TYPES: &[&str] = &["Album", "EP", "Single", "Broadcast", "Other"];

/// Every secondary type, with `Studio` standing for "no secondary type".
pub const SECONDARY_ALBUM_TYPES: &[&str] = &[
    "Studio",
    "Compilation",
    "Soundtrack",
    "Spokenword",
    "Interview",
    "Audiobook",
    "Live",
    "Remix",
    "DJ-mix",
    "Mixtape/Street",
    "Demo",
    "Audio drama",
];

/// Every release status.
pub const RELEASE_STATUSES: &[&str] = &["Official", "Promotion", "Bootleg", "Pseudo-Release"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequenced_uuid_projects_back_to_its_sequence() {
        for seq in [1_u32, 2, 41_459, 1_032_753, i32::MAX as u32] {
            assert_eq!(seq_of_uuid(seq_uuid(seq)), i64::from(seq));
        }
    }

    #[test]
    fn two_sequenced_uuids_for_one_sequence_differ() {
        assert_ne!(seq_uuid(7), seq_uuid(7));
    }

    #[test]
    fn a_sequenced_uuid_has_a_fixed_textual_prefix() {
        assert!(seq_uuid(0x0001_02a3)
            .to_string()
            .starts_with("00000000-0001-02a3-"));
    }

    #[test]
    fn names_compare_equal_across_punctuation_case_and_ampersands() {
        assert_eq!(normalize_name("OK Computer"), "ok computer");
        assert_eq!(normalize_name("ok.computer!"), "ok computer");
        assert_eq!(
            normalize_name("Simon & Garfunkel"),
            normalize_name("Simon and Garfunkel")
        );
        assert_eq!(normalize_name("  AC/DC "), "ac dc");
        assert_eq!(normalize_name("Sigur Rós"), "sigur rós");
    }

    #[test]
    fn the_standard_profile_keeps_studio_albums_only() {
        let p = MetadataProfile::standard();
        assert!(p.allows("Album", &[]));
        assert!(!p.allows("Single", &[]));
        assert!(!p.allows("Album", &["Live".to_string()]));
    }

    #[test]
    fn every_secondary_type_must_be_allowed() {
        let mut p = MetadataProfile::standard();
        p.secondary_types.push("Live".to_string());
        assert!(p.allows("Album", &["Live".to_string()]));
        assert!(!p.allows("Album", &["Live".to_string(), "Compilation".to_string()]));
    }
}

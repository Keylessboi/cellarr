//! Lidarr's quality vocabulary, mapped onto cellarr's.
//!
//! cellarr ranks every quality on one scale (see `cellarr_core::QualityRanking`);
//! Lidarr names each by a small integer id. This table is the translation, and is
//! the only place Lidarr's numbering appears.
//!
//! The ids for the qualities every Lidarr install ships with a profile for are
//! Lidarr's own. The remaining bitrate and codec buckets use ids from `200` up,
//! which Lidarr clients treat opaquely (they echo an id back; they do not
//! interpret it). When an existing install is migrated, its profiles are read by
//! quality *name*, so nothing depends on these numbers matching beyond
//! round-tripping.

use cellarr_core::{QualityRanking, MUSIC_RANK_BASE};

/// `(cellarr quality name, Lidarr quality id)` for the qualities with a
/// well-known Lidarr id.
const KNOWN: &[(&str, u32)] = &[
    ("Unknown", 0),
    ("MP3-192", 1),
    ("MP3-VBR-V2", 2),
    ("MP3-256", 3),
    ("MP3-320", 4),
    ("MP3-160", 5),
    ("FLAC", 6),
    ("FLAC 24bit", 7),
];

/// The first id handed to a quality with no well-known Lidarr id.
const DERIVED_BASE: u32 = 200;

/// The Lidarr id of a cellarr quality, by catalogue name.
#[must_use]
pub fn lidarr_quality_id(name: &str) -> u32 {
    if let Some((_, id)) = KNOWN.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return *id;
    }
    QualityRanking::default()
        .by_name(name)
        .filter(|q| q.rank >= MUSIC_RANK_BASE)
        .map_or(0, |q| DERIVED_BASE + (q.rank - MUSIC_RANK_BASE))
}

/// The cellarr quality name a Lidarr id stands for.
#[must_use]
pub fn quality_name_for_id(id: u32) -> Option<String> {
    if let Some((name, _)) = KNOWN.iter().find(|(_, i)| *i == id) {
        return Some((*name).to_string());
    }
    let rank = id.checked_sub(DERIVED_BASE)? + MUSIC_RANK_BASE;
    QualityRanking::default()
        .definition_for_rank(rank)
        .map(|d| d.name.clone())
}

/// The cellarr rank of a Lidarr quality id.
#[must_use]
pub fn rank_for_id(id: u32) -> Option<u32> {
    let name = quality_name_for_id(id)?;
    QualityRanking::default().by_name(&name).map(|q| q.rank)
}

/// The Lidarr id for a cellarr quality rank.
#[must_use]
pub fn id_for_rank(rank: u32) -> u32 {
    QualityRanking::default()
        .definition_for_rank(rank)
        .map_or(0, |d| lidarr_quality_id(&d.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_ids_are_lidarrs_own() {
        assert_eq!(lidarr_quality_id("FLAC"), 6);
        assert_eq!(lidarr_quality_id("MP3-320"), 4);
        assert_eq!(quality_name_for_id(7).as_deref(), Some("FLAC 24bit"));
    }

    #[test]
    fn every_music_quality_round_trips_through_its_id() {
        for q in QualityRanking::default().for_media(cellarr_core::MediaType::Music) {
            if q.name == "Unknown" {
                continue;
            }
            let id = lidarr_quality_id(&q.name);
            assert_eq!(rank_for_id(id), Some(q.rank), "{}", q.name);
            assert_eq!(id_for_rank(q.rank), id, "{}", q.name);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for q in QualityRanking::default().for_media(cellarr_core::MediaType::Music) {
            assert!(seen.insert(lidarr_quality_id(&q.name)), "{}", q.name);
        }
    }

    #[test]
    fn video_qualities_have_no_lidarr_id() {
        assert_eq!(lidarr_quality_id("Bluray-1080p"), 0);
        assert_eq!(quality_name_for_id(199), None);
    }
}

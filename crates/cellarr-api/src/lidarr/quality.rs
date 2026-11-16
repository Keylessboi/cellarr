//! Lidarr's quality vocabulary, mapped onto cellarr's.
//!
//! cellarr ranks every quality on one scale (see `cellarr_core::QualityRanking`);
//! Lidarr names each by a small integer id, fixed in its source and stored in
//! every profile and file record of an existing install. The table itself is
//! `cellarr_core::LIDARR_QUALITY_IDS` (the migration needs it too); this module
//! adapts it to ranks. The tests check the table against the catalogue, so a
//! quality added on either side shows up as a failing test rather than a silent
//! `Unknown`.

use cellarr_core::QualityRanking;

pub use cellarr_core::lidarr_quality_id;

/// The cellarr quality name a Lidarr id stands for.
#[must_use]
pub fn quality_name_for_id(id: u32) -> Option<String> {
    cellarr_core::lidarr_quality_name(id).map(str::to_string)
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
    fn ids_are_lidarrs_own() {
        assert_eq!(lidarr_quality_id("FLAC"), 6);
        assert_eq!(lidarr_quality_id("MP3-320"), 4);
        assert_eq!(lidarr_quality_id("MP3-VBR-V0"), 2);
        assert_eq!(lidarr_quality_id("MP3-VBR-V2"), 8);
        assert_eq!(quality_name_for_id(21).as_deref(), Some("FLAC 24bit"));
    }

    #[test]
    fn every_music_quality_round_trips_through_its_id() {
        for q in QualityRanking::default().for_media(cellarr_core::MediaType::Music) {
            let id = lidarr_quality_id(&q.name);
            assert_eq!(rank_for_id(id), Some(q.rank), "{}", q.name);
            assert_eq!(id_for_rank(q.rank), id, "{}", q.name);
        }
    }

    #[test]
    fn every_music_quality_has_an_id_and_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for q in QualityRanking::default().for_media(cellarr_core::MediaType::Music) {
            let id = lidarr_quality_id(&q.name);
            assert!(
                q.name == "Unknown" || id != 0,
                "{} has no Lidarr id",
                q.name
            );
            assert!(seen.insert(id), "{}", q.name);
        }
        assert_eq!(
            cellarr_core::music::LIDARR_QUALITY_IDS.len(),
            seen.len(),
            "the table and the catalogue agree"
        );
    }

    #[test]
    fn video_qualities_have_no_lidarr_id() {
        assert_eq!(lidarr_quality_id("Bluray-1080p"), 0);
        assert_eq!(quality_name_for_id(199), None);
    }
}

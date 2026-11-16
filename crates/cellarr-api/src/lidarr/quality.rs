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

/// Lidarr's quality groups, worst tier first: `(group id, name, member quality ids)`.
///
/// A Lidarr profile does not list qualities one by one; it lists these tiers (plus
/// `Unknown` and `WAV`, which stand alone), and allows or disallows each tier. The
/// group ids are Lidarr's (`1000` up).
pub const GROUPS: &[(u32, &str, &[u32])] = &[
    (
        1000,
        "Trash Quality Lossy",
        &[32, 31, 30, 29, 28, 27, 26, 25, 24],
    ),
    (1001, "Poor Quality Lossy", &[23, 33, 22, 19, 5]),
    (1002, "Low Quality Lossy", &[1, 18, 9, 20, 34]),
    (1003, "Mid Quality Lossy", &[17, 8, 3, 16, 10]),
    (1004, "High Quality Lossy", &[2, 12, 4, 15, 11, 14]),
    (1005, "Lossless", &[6, 7, 35, 36, 21, 37]),
];

fn quality_item(id: u32, allowed: bool) -> serde_json::Value {
    serde_json::json!({
        "quality": { "id": id, "name": cellarr_core::lidarr_quality_name(id).unwrap_or("Unknown") },
        "items": [],
        "allowed": allowed,
    })
}

/// A profile's `items` array as Lidarr renders it: `Unknown`, then each tier as a
/// group (allowed when any of its members is), then `WAV`.
#[must_use]
pub fn profile_items(allowed_ranks: &[u32]) -> Vec<serde_json::Value> {
    let on = |id: u32| rank_for_id(id).is_some_and(|r| allowed_ranks.contains(&r));
    let mut items = vec![quality_item(0, on(0))];
    for (gid, name, members) in GROUPS {
        items.push(serde_json::json!({
            "name": name,
            "items": members.iter().map(|m| quality_item(*m, on(*m))).collect::<Vec<_>>(),
            "allowed": members.iter().any(|m| on(*m)),
            "id": gid,
        }));
    }
    items.push(quality_item(13, on(13)));
    items
}

/// The ranks a posted `items` array allows. A group that is allowed contributes
/// every member (the members' own flags are ignored, as in Lidarr, where a group is
/// allowed as a whole); a stand-alone quality contributes itself.
#[must_use]
pub fn allowed_ranks(items: &[serde_json::Value]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut push = |id: u64| {
        if let Some(r) = u32::try_from(id).ok().and_then(rank_for_id) {
            if !out.contains(&r) {
                out.push(r);
            }
        }
    };
    for item in items {
        if !item
            .get("allowed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let members = item
            .get("items")
            .and_then(serde_json::Value::as_array)
            .filter(|m| !m.is_empty());
        match members {
            Some(members) => {
                for m in members {
                    if let Some(id) = m
                        .get("quality")
                        .and_then(|q| q.get("id"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        push(id);
                    }
                }
            }
            None => {
                if let Some(id) = item
                    .get("quality")
                    .and_then(|q| q.get("id"))
                    .and_then(serde_json::Value::as_u64)
                {
                    push(id);
                }
            }
        }
    }
    out
}

/// The `cutoff` value for a cellarr cutoff rank: its group's id when the quality
/// belongs to a tier, else its own id.
#[must_use]
pub fn cutoff_id_for_rank(rank: u32) -> u32 {
    let id = id_for_rank(rank);
    GROUPS
        .iter()
        .find(|(_, _, members)| members.contains(&id))
        .map_or(id, |(gid, _, _)| *gid)
}

/// The cellarr cutoff rank for a posted `cutoff`: a group means its lowest member
/// (the first quality that satisfies it).
#[must_use]
pub fn rank_for_cutoff_id(cutoff: u32) -> Option<u32> {
    match GROUPS.iter().find(|(gid, _, _)| *gid == cutoff) {
        Some((_, _, members)) => members.iter().filter_map(|m| rank_for_id(*m)).min(),
        None => rank_for_id(cutoff),
    }
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
    fn every_music_quality_is_in_exactly_one_tier_or_stands_alone() {
        let mut seen = std::collections::HashSet::new();
        for (_, _, members) in GROUPS {
            for m in *members {
                assert!(seen.insert(*m), "quality {m} is in two groups");
                assert!(rank_for_id(*m).is_some(), "group member {m} is unknown");
            }
        }
        // Everything else is Unknown (0) or WAV (13), the two stand-alone items.
        for q in QualityRanking::default().for_media(cellarr_core::MediaType::Music) {
            let id = lidarr_quality_id(&q.name);
            assert!(
                seen.contains(&id) || id == 0 || id == 13,
                "{} is in no tier",
                q.name
            );
        }
    }

    #[test]
    fn a_tier_is_allowed_when_any_member_is_and_round_trips() {
        let ranks = allowed_ranks(&profile_items(&[
            rank_for_id(6).unwrap(),
            rank_for_id(4).unwrap(),
        ]));
        // FLAC allows the whole Lossless tier; 320 allows the whole High tier.
        assert!(ranks.contains(&rank_for_id(21).unwrap()));
        assert!(ranks.contains(&rank_for_id(2).unwrap()));
        assert!(!ranks.contains(&rank_for_id(1).unwrap()));
    }

    #[test]
    fn a_cutoff_is_a_tier_id_for_a_grouped_quality() {
        assert_eq!(cutoff_id_for_rank(rank_for_id(6).unwrap()), 1005);
        assert_eq!(cutoff_id_for_rank(rank_for_id(13).unwrap()), 13);
        assert_eq!(rank_for_cutoff_id(1005), rank_for_id(6));
    }

    #[test]
    fn video_qualities_have_no_lidarr_id() {
        assert_eq!(lidarr_quality_id("Bluray-1080p"), 0);
        assert_eq!(quality_name_for_id(199), None);
    }
}

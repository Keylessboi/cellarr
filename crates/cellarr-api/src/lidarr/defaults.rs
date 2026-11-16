//! What a fresh Lidarr ships with, so a fresh cellarr answers the same.
//!
//! A new Lidarr has three quality profiles (`Any`, `Lossless`, `Standard`), two
//! metadata profiles (`Standard`, `None`) and one delay profile, with ids 1–3, 1–2 and 1.
//! Clients rely on them: a script that adds an artist with `qualityProfileId: 1` works
//! against a new install, and Octo's setup page offers them. [`ensure`] creates what is
//! missing and never touches what is there, so it is safe to run on every start.

use cellarr_core::{
    DelayProfile, MetadataProfile, PreferredProtocol, QualityProfile, QualityProfileId,
};

use super::quality::{allowed_ranks, profile_items, rank_for_cutoff_id};
use crate::error::ApiResult;
use crate::state::AppState;

/// Create the default profiles that are absent.
///
/// # Errors
/// Returns the database error if a write fails.
pub async fn ensure(state: &AppState) -> ApiResult<()> {
    let music = state.db.music();
    // Quality profiles, as (id, name, allowed tiers by group id or quality id, cutoff).
    // `Any` allows everything but WAV, `Lossless` only the lossless tier, `Standard`
    // the three upper lossy tiers.
    if music.profile_by_seq(1).await?.is_none() && music.profile_by_seq(2).await?.is_none() {
        let all = |on: &[u64]| -> Vec<serde_json::Value> {
            profile_items(&[])
                .into_iter()
                .map(|mut i| {
                    let id = i
                        .get("id")
                        .or_else(|| i.get("quality").and_then(|q| q.get("id")))
                        .and_then(serde_json::Value::as_u64);
                    i["allowed"] = serde_json::json!(id.is_some_and(|id| on.contains(&id)));
                    i
                })
                .collect()
        };
        let defaults: [(i64, &str, Vec<u64>, u32); 3] = [
            (1, "Any", vec![0, 1000, 1001, 1002, 1003, 1004, 1005], 0),
            (2, "Lossless", vec![1005], 1005),
            (3, "Standard", vec![1002, 1003, 1004], 1002),
        ];
        for (seq, name, on, cutoff) in defaults {
            let ranks = allowed_ranks(&all(&on));
            let cutoff_rank = if cutoff == 0 {
                0
            } else {
                rank_for_cutoff_id(cutoff).unwrap_or(0)
            };
            let profile = QualityProfile {
                id: QualityProfileId::new(),
                name: name.to_string(),
                allowed_qualities: ranks,
                upgrades_allowed: false,
                cutoff_quality: cutoff_rank,
                min_custom_format_score: 0,
                upgrade_until_custom_format_score: 0,
                required_languages: Vec::new(),
            };
            state.db.profiles().upsert_profile(&profile).await?;
            music.claim_profile_seq(profile.id, seq).await?;
        }
    }
    // Metadata profiles: `Standard` exists implicitly; add `None`.
    if music.metadata_profile(2).await?.is_none() {
        music
            .upsert_metadata_profile(MetadataProfile {
                id: 1,
                ..MetadataProfile::standard()
            })
            .await?;
        music
            .upsert_metadata_profile(MetadataProfile {
                id: 2,
                name: "None".to_string(),
                primary_types: Vec::new(),
                secondary_types: Vec::new(),
                release_statuses: Vec::new(),
            })
            .await?;
    }
    // The catch-all delay profile.
    if state.db.profiles().list_delay_profiles().await?.is_empty() {
        let mut p = DelayProfile::default_catch_all();
        p.preferred_protocol = PreferredProtocol::Usenet;
        p.bypass_if_highest_quality = true;
        state.db.profiles().upsert_delay_profile(&p).await?;
    }
    Ok(())
}

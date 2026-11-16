//! Record/replay tests for the Lidarr metadata client: recorded responses in, the
//! normalized vocabulary out. The recordings are trimmed copies of real answers,
//! including the `null` ratings the service really sends.

use cellarr_meta::{LidarrMetaSource, MusicMeta, RecordedFetcher};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const BASE: &str = "https://meta.test/api/v0.4";
const RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
const KID_A: &str = "e75c0549-ad55-39e3-8025-c72c5d4a3c5d";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURES}/{name}")).expect("fixture present")
}

fn source(prefix: &str, file: &str) -> LidarrMetaSource<RecordedFetcher> {
    LidarrMetaSource::with_base_url(
        RecordedFetcher::new().with_body(&format!("{BASE}{prefix}"), fixture(file)),
        BASE,
    )
}

#[tokio::test]
async fn an_artist_search_carries_biography_links_and_artwork() {
    let src = source("/search?type=artist", "lm_search_artist.json");
    let hits = src.search_artists("radiohead", 5).await.unwrap();
    assert_eq!(hits[0].mbid, RADIOHEAD);
    assert!(hits[0].overview.as_deref().unwrap().contains("English rock band"));
    assert!(!hits[0].links.is_empty());
    assert!(hits[0].score >= hits[1].score);
}

#[tokio::test]
async fn an_artist_lists_its_release_groups_filtered_by_type() {
    let src = source(&format!("/artist/{RADIOHEAD}"), "lm_artist.json");
    let all = src.release_groups(RADIOHEAD, &[]).await.unwrap();
    assert_eq!(all.len(), 2);
    let none = src
        .release_groups(RADIOHEAD, &["Single".to_string()])
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn an_album_search_tolerates_null_ratings_and_embeds_releases() {
    let src = source("/search?type=album", "lm_search_album.json");
    let hits = src
        .search_release_groups("ok computer", Some("radiohead"), 5)
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert!(!hits[0].releases.is_empty());
}

#[tokio::test]
async fn an_album_yields_its_pressings_with_tracks() {
    let src = source(&format!("/album/{KID_A}"), "lm_album.json");
    let releases = src.releases(KID_A).await.unwrap();
    assert_eq!(releases.len(), 2);
    assert_eq!(releases[0].media[0].tracks[0].position, 1);
    assert!(releases[0].media[0].tracks[0].recording_mbid.is_some());
}

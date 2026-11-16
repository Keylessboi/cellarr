//! Record/replay tests for the MusicBrainz client: recorded responses in, the
//! normalized vocabulary out. No network.

use cellarr_meta::{pick_release, MusicBrainzSource, RecordedFetcher};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const BASE: &str = "https://mb.test/ws/2";
const RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
const OK_COMPUTER: &str = "b1392450-e666-3926-a536-22c65f834433";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURES}/{name}")).expect("fixture present")
}

fn source(fetcher: RecordedFetcher) -> MusicBrainzSource<RecordedFetcher> {
    MusicBrainzSource::with_base_url(fetcher, BASE)
}

#[tokio::test]
async fn an_artist_search_returns_the_best_match_first() {
    let mb = source(RecordedFetcher::new().with_body(
        &format!("{BASE}/artist?query="),
        fixture("mb_artist_search_radiohead.json"),
    ));
    let hits = mb.search_artists("radiohead", 3).await.unwrap();
    assert_eq!(hits[0].mbid, RADIOHEAD);
    assert_eq!(hits[0].name, "Radiohead");
    assert_eq!(hits[0].artist_type.as_deref(), Some("Group"));
    assert_eq!(hits[0].score, 100);
    assert!(hits[1].score < 100);
    assert_eq!(hits[1].disambiguation.as_deref(), Some("Capsmusic LTD. artist"));
}

#[tokio::test]
async fn an_artist_lookup_carries_aliases_and_genres() {
    let mb = source(RecordedFetcher::new().with_body(
        &format!("{BASE}/artist/{RADIOHEAD}"),
        fixture("mb_artist_radiohead.json"),
    ));
    let artist = mb.artist(RADIOHEAD).await.unwrap();
    assert_eq!(artist.name, "Radiohead");
    assert!(!artist.ended);
    assert!(artist.aliases.contains(&"Radio Head".to_string()));
    assert!(
        !artist.aliases.contains(&"Radiohead".to_string()),
        "the display name is not its own alias"
    );
    assert_eq!(artist.sort_name, None, "a sort name equal to the name is not stored");
    assert!(!artist.genres.is_empty());
}

#[tokio::test]
async fn release_groups_carry_their_types_so_a_profile_can_filter_them() {
    let mb = source(RecordedFetcher::new().with_body(
        &format!("{BASE}/release-group?artist="),
        fixture("mb_release_groups_radiohead.json"),
    ));
    let groups = mb.release_groups(RADIOHEAD, &[]).await.unwrap();
    let ok = groups.iter().find(|g| g.mbid == OK_COMPUTER).unwrap();
    assert_eq!(ok.title, "OK Computer");
    assert_eq!(ok.primary_type, "Album");
    assert!(ok.secondary_types.is_empty());
    assert_eq!(ok.first_release_date.as_deref(), Some("1997-05-21"));

    let standard = cellarr_core::MetadataProfile::standard();
    let kept: Vec<&str> = groups
        .iter()
        .filter(|g| standard.allows(&g.primary_type, &g.secondary_types))
        .map(|g| g.title.as_str())
        .collect();
    assert!(kept.contains(&"Kid A"));
    assert!(kept.contains(&"In Rainbows"));
    assert!(!kept.contains(&"Man of War"), "a single is not a studio album");
    assert!(!kept.contains(&"5 Album Set"), "a compilation is not a studio album");
    assert!(!kept.contains(&"Live at The Astoria, London, 27/05/1994"));
}

#[tokio::test]
async fn an_album_search_names_the_credited_artist() {
    let mb = source(RecordedFetcher::new().with_body(
        &format!("{BASE}/release-group?query="),
        fixture("mb_release_group_search_ok_computer.json"),
    ));
    let hits = mb
        .search_release_groups("ok computer", Some("radiohead"), 3)
        .await
        .unwrap();
    assert_eq!(hits[0].mbid, OK_COMPUTER);
    assert_eq!(
        hits[0].artist,
        Some((RADIOHEAD.to_string(), "Radiohead".to_string()))
    );
}

#[tokio::test]
async fn the_representative_pressing_is_the_plain_twelve_track_album() {
    let mb = source(RecordedFetcher::new().with_body(
        &format!("{BASE}/release?release-group="),
        fixture("mb_releases_ok_computer.json"),
    ));
    let releases = mb.releases(OK_COMPUTER).await.unwrap();
    assert!(releases.len() >= 5);

    // The recorded set includes a two-LP pressing, a CD with a bonus disc and a
    // deluxe set whose third disc is a DVD: the DVD's chapters are not tracks.
    let deluxe = releases
        .iter()
        .find(|r| r.media.len() == 2 && r.track_count() == 27)
        .expect("the 2CD+DVD set is read as two audio discs");
    assert_eq!(deluxe.media[1].position, 2);

    let picked = pick_release(&releases, &["Official".to_string()]).unwrap();
    assert_eq!(picked.track_count(), 12);
    assert_eq!(picked.media.len(), 1);
    assert_eq!(picked.date.as_deref(), Some("1997-05-21"));
    let first = &picked.media[0].tracks[0];
    assert_eq!((first.position, first.title.as_str()), (1, "Airbag"));
    assert!(first.length_ms.is_some());
    assert!(first.recording_mbid.is_some());
}

#[tokio::test]
async fn a_refusal_that_is_not_a_request_to_wait_is_reported_at_once() {
    let mb = source(RecordedFetcher::new().with_response(
        &format!("{BASE}/artist/"),
        404,
        br#"{"error":"Not Found"}"#.to_vec(),
    ));
    let err = mb.artist("nope").await.unwrap_err();
    assert!(matches!(err, cellarr_meta::MetaError::Http { status: 404, .. }));
}

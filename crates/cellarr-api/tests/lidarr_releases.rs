//! A client that adds an album (or an artist) and reads it straight back must find
//! its pressings, with one monitored: the lidarr-downloader app skips an album whose
//! `releases` is empty ("No valid releases").

mod lidarr_common;

use cellarr_api::AuthConfig;
use lidarr_common::*;
use reqwest::Method;
use serde_json::{json, Value};

const EP: &str = "11111111-2222-3333-4444-555555555555";

fn has_monitored_release(album: &Value) -> bool {
    album["releases"].as_array().is_some_and(|r| {
        !r.is_empty()
            && r.iter()
                .any(|x| x["monitored"] == json!(true) && x["id"].as_i64().is_some())
    })
}

async fn add_artist_with_no_albums_monitored(l: &Lidarr) {
    let profile = l.flac_profile().await;
    let hits = l.get("/artist/lookup?term=radiohead").await;
    let mut payload = hits[0].clone();
    payload["qualityProfileId"] = json!(profile);
    payload["metadataProfileId"] = json!(1);
    payload["rootFolderPath"] = json!("/data/media/music");
    payload["monitored"] = json!(true);
    payload["addOptions"] = json!({ "monitor": "none", "searchForMissingAlbums": false });
    let r = l.send(Method::POST, "/artist", &payload).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
}

fn ep_body() -> Value {
    json!({
        "foreignAlbumId": EP,
        "title": "Street Halo",
        "albumType": "EP",
        "artistId": 1,
        "monitored": true,
        "releaseDate": "2011-02-14",
        "addOptions": { "searchForNewAlbum": false },
    })
}

#[tokio::test]
async fn an_album_posted_then_read_back_has_a_monitored_release() {
    let l = start(AuthConfig::disabled()).await;
    add_artist_with_no_albums_monitored(&l).await;
    let r = l.send(Method::POST, "/album", &ep_body()).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let posted: Value = r.json().await.unwrap();
    assert!(has_monitored_release(&posted), "the POST answer: {posted}");
    let id = posted["id"].as_i64().unwrap();
    let read = l.get(&format!("/album/{id}")).await;
    assert!(has_monitored_release(&read), "the GET answer: {read}");
}

#[tokio::test]
async fn an_album_of_an_added_artist_has_releases_on_its_first_read() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;
    let id = l.get(&format!("/album?foreignAlbumId={OK_COMPUTER}")).await[0]["id"]
        .as_i64()
        .unwrap();
    // No /track call first: the read itself supplies the pressings.
    let read = l.get(&format!("/album/{id}")).await;
    assert!(has_monitored_release(&read), "the GET answer: {read}");
    // And they are stored, not refetched: the same ids come back.
    let again = l.get(&format!("/album/{id}")).await;
    assert_eq!(read["releases"], again["releases"]);
}

#[tokio::test]
async fn a_metadata_source_that_cannot_supply_pressings_does_not_fail_the_add() {
    let l = start_with(AuthConfig::disabled(), false).await;
    add_artist_with_no_albums_monitored(&l).await;
    let r = l.send(Method::POST, "/album", &ep_body()).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let posted: Value = r.json().await.unwrap();
    assert_eq!(posted["releases"], json!([]));
    assert_eq!(posted["foreignAlbumId"], EP);
    let id = posted["id"].as_i64().unwrap();
    let read = l.get(&format!("/album/{id}")).await;
    assert_eq!(read["releases"], json!([]));
}

//! The Lidarr face, driven the way Lidarr's real clients drive it: look an artist
//! up, add it, queue a search, poll the command, read the wanted list.

mod lidarr_common;

use cellarr_api::AuthConfig;
use lidarr_common::{start, KEY, OK_COMPUTER, RADIOHEAD};
use serde_json::{json, Value};

#[tokio::test]
async fn it_identifies_itself_as_lidarr_and_answers_ping_without_a_key() {
    let l = start(AuthConfig::with_key(KEY)).await;
    let ping = l
        .client
        .get(format!("{}/ping", l.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ping.status(), 200);

    let status = l
        .req(reqwest::Method::GET, "/system/status")
        .send()
        .await
        .unwrap();
    assert!(status.headers().contains_key("x-application-version"));
    let v: Value = status.json().await.unwrap();
    assert_eq!(v["appName"], "Lidarr");
    assert!(v["version"].as_str().unwrap().starts_with("3."));
}

#[tokio::test]
async fn an_unknown_path_is_json_404() {
    let l = start(AuthConfig::disabled()).await;
    let r = l
        .client
        .get(format!("{}/api/v1/nope", l.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(r.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("json"));
}

#[tokio::test]
async fn an_artist_is_looked_up_added_and_listed_with_statistics() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    assert_eq!(profile, 4, "ids 1-3 are the seeded defaults");

    let hits = l.get("/artist/lookup?term=radiohead").await;
    assert_eq!(hits[0]["foreignArtistId"], RADIOHEAD);
    assert!(
        hits[0].get("id").is_none(),
        "not in the library yet: Lidarr omits the id"
    );

    let added = l.add_radiohead(profile).await;
    assert_eq!(added["id"], 1);
    assert_eq!(added["artistName"], "Radiohead");
    assert_eq!(added["qualityProfileId"], profile);
    assert_eq!(added["path"], "/data/media/music/Radiohead");
    assert_eq!(added["rootFolderPath"], "/data/media/music");
    assert!(added["statistics"]["albumCount"].as_i64().unwrap() >= 8);

    let list = l.get("/artist").await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let again = l
        .get("/artist?mbId=a74b1b7f-71a5-4011-9441-d0b5e4122711")
        .await;
    assert_eq!(again.as_array().unwrap().len(), 1);

    let hits = l.get("/artist/lookup?term=radiohead").await;
    assert_eq!(hits[0]["id"], 1, "a lookup knows what the library holds");
}

#[tokio::test]
async fn albums_are_listed_by_artist_and_by_musicbrainz_id() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;

    let by_artist = l.get("/album?artistId=1").await;
    assert!(by_artist.as_array().unwrap().len() >= 8);
    let by_mbid = l.get(&format!("/album?foreignAlbumId={OK_COMPUTER}")).await;
    let album = &by_mbid[0];
    assert_eq!(album["title"], "OK Computer");
    assert_eq!(album["albumType"], "Album");
    assert_eq!(album["artistId"], 1);
    assert_eq!(album["artist"]["artistName"], "Radiohead");
    assert_eq!(album["releaseDate"], "1997-05-21T00:00:00Z");
    assert!(album["monitored"].as_bool().unwrap());
    assert_eq!(album["statistics"]["trackFileCount"], 0);
}

#[tokio::test]
async fn albums_of_an_unknown_artist_are_an_empty_list_not_a_404() {
    let l = start(AuthConfig::disabled()).await;
    let r = l
        .req(reqwest::Method::GET, "/album?artistId=999999")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "Lidarr answers [] for an unknown artist");
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn tracks_are_fetched_the_first_time_they_are_asked_for() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;
    let id = l.get(&format!("/album?foreignAlbumId={OK_COMPUTER}")).await[0]["id"]
        .as_i64()
        .unwrap();

    let tracks = l.get(&format!("/track?albumId={id}")).await;
    assert_eq!(tracks.as_array().unwrap().len(), 12);
    assert_eq!(tracks[0]["title"], "Airbag");
    assert_eq!(tracks[0]["trackNumber"], "1");
    assert_eq!(tracks[0]["hasFile"], false);
    assert_eq!(tracks[0]["albumId"], id);
    assert_eq!(tracks[0]["artist"]["artistName"], "Radiohead");

    let album = l.get(&format!("/album/{id}")).await;
    assert_eq!(album["statistics"]["trackCount"], 12);
    assert_eq!(album["mediumCount"], 1);
}

#[tokio::test]
async fn an_album_search_command_is_polled_to_completion() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;
    let id = l.get(&format!("/album?foreignAlbumId={OK_COMPUTER}")).await[0]["id"]
        .as_i64()
        .unwrap();

    let r = l
        .send(
            reqwest::Method::POST,
            "/command",
            &json!({ "name": "AlbumSearch", "albumIds": [id] }),
        )
        .await;
    assert!(r.status().is_success());
    let cmd: Value = r.json().await.unwrap();
    assert_eq!(cmd["name"], "AlbumSearch");
    let cid = cmd["id"].as_i64().expect("command ids are integers");
    assert!(["queued", "started", "completed"].contains(&cmd["status"].as_str().unwrap()));

    let polled = l.get(&format!("/command/{cid}")).await;
    assert_eq!(polled["id"], cid);
    assert!(polled["status"].is_string());
    assert!(polled["queued"].is_string());

    let bad = l
        .send(
            reqwest::Method::POST,
            "/command",
            &json!({ "name": "NoSuchCommand" }),
        )
        .await;
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn the_wanted_list_pages_monitored_albums_without_files() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;
    // Lidarr only wants albums that have a track list, and cellarr fetches one
    // the first time it is asked for, so ask for every album's tracks first.
    for album in l.get("/album?artistId=1").await.as_array().unwrap() {
        let id = album["id"].as_i64().unwrap();
        l.get(&format!("/track?albumId={id}")).await;
    }

    let page = l.get("/wanted/missing?page=1&pageSize=3&sortKey=albums.releaseDate&sortDirection=descending&includeArtist=true").await;
    assert_eq!(page["page"], 1);
    assert_eq!(page["pageSize"], 3);
    assert!(page["totalRecords"].as_i64().unwrap() >= 8);
    let records = page["records"].as_array().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["artist"]["artistName"], "Radiohead");
    let dates: Vec<&str> = records
        .iter()
        .map(|r| r["releaseDate"].as_str().unwrap())
        .collect();
    let mut sorted = dates.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(dates, sorted, "descending by release date");

    // Unmonitoring an album removes it from the list.
    let first = records[0]["id"].as_i64().unwrap();
    let total = page["totalRecords"].as_i64().unwrap();
    let r = l
        .send(
            reqwest::Method::PUT,
            "/album/monitor",
            &json!({ "albumIds": [first], "monitored": false }),
        )
        .await;
    assert!(r.status().is_success());
    let after = l.get("/wanted/missing?pageSize=100").await;
    assert_eq!(after["totalRecords"].as_i64().unwrap(), total - 1);
}

#[tokio::test]
async fn history_and_queue_are_empty_but_well_formed() {
    let l = start(AuthConfig::disabled()).await;
    let history = l.get("/history?pageSize=1&albumId=0&eventType=8").await;
    assert_eq!(history["totalRecords"], 0);
    assert!(history["records"].as_array().unwrap().is_empty());
    let queue = l.get("/queue?page=1&pageSize=1000&includeAlbum=true").await;
    assert_eq!(queue["totalRecords"], 0);
    let status = l.get("/queue/status").await;
    assert_eq!(status["totalCount"], 0);
}

#[tokio::test]
async fn profiles_round_trip_and_the_default_metadata_profile_exists() {
    let l = start(AuthConfig::disabled()).await;
    let meta = l.get("/metadataprofile").await;
    assert_eq!(meta[0]["id"], 1);
    assert_eq!(meta[0]["name"], "Standard");
    let primary = meta[0]["primaryAlbumTypes"].as_array().unwrap();
    assert!(primary
        .iter()
        .any(|t| t["albumType"]["name"] == "Album" && t["allowed"] == true));
    assert!(primary
        .iter()
        .any(|t| t["albumType"]["name"] == "Single" && t["allowed"] == false));

    let id = l.flac_profile().await;
    let profile = l.get(&format!("/qualityprofile/{id}")).await;
    let allowed: Vec<&str> = profile["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["allowed"] == true)
        .map(|i| {
            i.get("name")
                .and_then(|n| n.as_str())
                .or_else(|| i["quality"]["name"].as_str())
                .unwrap()
        })
        .collect();
    assert!(allowed.contains(&"Lossless"), "{allowed:?}");
    assert!(allowed.contains(&"High Quality Lossy"), "{allowed:?}");
    assert_eq!(profile["cutoff"], 1005);

    let list = l.get("/qualityprofile").await;
    assert_eq!(
        list.as_array().unwrap().len(),
        4,
        "the three seeded defaults plus ours; video profiles are not listed to a music client"
    );
}

#[tokio::test]
async fn deleting_an_artist_removes_its_albums() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;
    let r = l
        .req(reqwest::Method::DELETE, "/artist/1?deleteFiles=false")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert!(l.get("/artist").await.as_array().unwrap().is_empty());
    assert!(l.get("/album").await.as_array().unwrap().is_empty());
    let gone = l
        .req(reqwest::Method::GET, "/artist/1")
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
    let _ = &l.state;
}

#[tokio::test]
async fn an_album_is_added_with_its_artist_the_way_octo_does_it() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    let hits = l
        .get("/album/lookup?term=radiohead%20-%20ok%20computer")
        .await;
    assert_eq!(hits[0]["foreignAlbumId"], OK_COMPUTER);
    assert_eq!(hits[0]["artist"]["foreignArtistId"], RADIOHEAD);

    let mut body = hits[0].clone();
    body.as_object_mut().unwrap().remove("id");
    body["monitored"] = json!(true);
    body["addOptions"] = json!({ "searchForNewAlbum": false });
    body["artist"]["qualityProfileId"] = json!(profile);
    body["artist"]["metadataProfileId"] = json!(1);
    body["artist"]["rootFolderPath"] = json!("/data/media/music");
    body["artist"]["monitored"] = json!(true);
    body["artist"]["monitorNewItems"] = json!("none");
    body["artist"]["addOptions"] = json!({ "monitor": "none", "searchForMissingAlbums": false });
    let r = l.send(reqwest::Method::POST, "/album", &body).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let album: Value = r.json().await.unwrap();
    assert_eq!(album["foreignAlbumId"], OK_COMPUTER);
    assert_eq!(album["monitored"], true);

    // The artist was added with no other album monitored.
    let all = l.get("/album?artistId=1").await;
    let monitored: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["monitored"] == true)
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(monitored, ["OK Computer"]);
}

#[tokio::test]
async fn adding_an_artist_to_monitor_no_albums_leaves_it_unmonitored_like_lidarr() {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    let hits = l.get("/artist/lookup?term=radiohead").await;
    let mut payload = hits[0].clone();
    payload["qualityProfileId"] = json!(profile);
    payload["metadataProfileId"] = json!(1);
    payload["rootFolderPath"] = json!("/data/media/music");
    payload["monitored"] = json!(true);
    payload["addOptions"] = json!({ "monitor": "none", "searchForMissingAlbums": false });
    let r = l.send(reqwest::Method::POST, "/artist", &payload).await;
    assert!(r.status().is_success());
    let added: Value = r.json().await.unwrap();
    assert_eq!(
        added["monitored"], false,
        "monitor=none adds the artist unmonitored"
    );
    let albums = l.get("/album?artistId=1").await;
    assert!(albums
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["monitored"] == false));
}

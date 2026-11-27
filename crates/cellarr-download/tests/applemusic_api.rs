//! The Apple Music indexer and client against a fake catalogue and a fake
//! decryptor over real HTTP. No real Apple host, token or decryptor is involved.

#[path = "common/fake_apple.rs"]
mod fake_apple;

use cellarr_core::{
    ContentRef, DownloadClient, DownloadState, GrabRequest, Indexer, IndexerId, Protocol, Release,
    SearchTerms,
};
use cellarr_download::{AppleMusicClient, AppleMusicIndexer, AppleMusicSettings};
use fake_apple::{album, start_catalog, start_decryptor, DEV_TOKEN, USER_TOKEN};
use serde_json::{json, Value};

fn settings(api: &str, decryptor: &str, root: &str) -> Value {
    json!({
        "baseUrl": api, "developerToken": DEV_TOKEN, "musicUserToken": USER_TOKEN,
        "storefront": "us", "decryptorUrl": decryptor, "downloadPath": root,
    })
}

fn terms(artist: &str, album: &str) -> SearchTerms {
    SearchTerms {
        queries: vec![format!("{artist} {album}")],
        ids: vec![
            ("artist".into(), artist.into()),
            ("album".into(), album.into()),
        ],
        ..SearchTerms::default()
    }
}

fn indexer(s: &Value) -> AppleMusicIndexer {
    AppleMusicIndexer::new(
        IndexerId::new(),
        "apple",
        AppleMusicSettings::from_json_indexer(s).unwrap(),
    )
}

fn grab(url: String) -> GrabRequest {
    GrabRequest {
        content_ref: ContentRef {
            id: cellarr_core::ContentId::new(),
            library_id: cellarr_core::LibraryId::new(),
            media_type: cellarr_core::MediaType::Music,
            coords: cellarr_core::Coordinates::Album,
        },
        release: Release {
            indexer_id: IndexerId::new(),
            title: "t".into(),
            download_url: url,
            guid: None,
            protocol: Protocol::Torrent,
            size: None,
            seeders: None,
            indexer_flags: vec![],
        },
        indexer_id: IndexerId::new(),
        client_id: cellarr_core::DownloadClientId::new(),
        category: "music".into(),
        release_type: None,
    }
}

#[tokio::test]
async fn a_search_keeps_only_the_requested_album_and_offers_every_tier() {
    let cat = start_catalog(vec![
        album("100", "Kind of Blue", "Miles Davis", 5, Some(&["lossless"])),
        album(
            "101",
            "Kind of Blue (Legacy Edition)",
            "Miles Davis",
            9,
            None,
        ),
        album(
            "102",
            "Kind of Blue - Single",
            "Miles Davis",
            1,
            Some(&["atmos"]),
        ),
        album("200", "Bitches Brew", "Miles Davis", 4, None),
    ])
    .await;
    let ix = indexer(&settings(&cat.url, "http://unused", "/dl"));
    let found = ix
        .search(&terms("Miles Davis", "Kind of Blue"))
        .await
        .unwrap();
    let titles: Vec<&str> = found.iter().map(|r| r.title.as_str()).collect();
    // 100: no atmos trait -> two tiers; 102: edition suffix dropped, atmos -> three.
    assert_eq!(
        titles,
        [
            "Miles Davis - Kind of Blue [AAC] [256]",
            "Miles Davis - Kind of Blue [ALAC] [LOSSLESS]",
            "Miles Davis - Kind of Blue - Single [AAC] [256]",
            "Miles Davis - Kind of Blue - Single [ALAC] [LOSSLESS]",
            "Miles Davis - Kind of Blue - Single [AAC] [VBR] [ATMOS]",
        ]
    );
    assert_eq!(
        found[0].download_url,
        "applemusicarr://album/100?quality=aac256"
    );
    assert_eq!(found[1].guid.as_deref(), Some("applemusicarr:100:alac"));
    let c = cat.state.lock().unwrap();
    // First tier (artist + album) wins; no loose tiers were needed.
    assert_eq!(c.searches.len(), 1, "{:?}", c.searches);
    assert_eq!(c.searches[0].0, "us");
    assert_eq!(c.searches[0].1, "Miles Davis Kind of Blue");
    assert_eq!(c.searches[0].2, "25");
    assert_eq!(c.seen[0].0, format!("Bearer {DEV_TOKEN}"));
    assert_eq!(c.seen[0].1, "", "no cookie for a developer token");
}

#[tokio::test]
async fn a_miss_walks_the_tiers_and_nothing_matching_is_empty() {
    let cat = start_catalog(vec![album("200", "Bitches Brew", "Miles Davis", 4, None)]).await;
    let ix = indexer(&settings(&cat.url, "http://unused", "/dl"));
    let found = ix
        .search(&terms("Miles Davis", "Kind of Blue"))
        .await
        .unwrap();
    assert!(found.is_empty(), "a neighbouring album is never offered");
    let c = cat.state.lock().unwrap();
    let q: Vec<&str> = c.searches.iter().map(|s| s.1.as_str()).collect();
    assert_eq!(
        q,
        ["Miles Davis Kind of Blue", "Miles Davis", "Kind of Blue"]
    );
    drop(c);
    assert!(ix.search(&SearchTerms::default()).await.unwrap().is_empty());
}

#[tokio::test]
async fn far_future_releases_are_dropped_and_a_rejected_token_is_an_auth_error() {
    let mut future = album("300", "Next Year", "Soon", 3, None);
    future["attributes"]["releaseDate"] = json!("2099-01-01");
    let cat = start_catalog(vec![future]).await;
    let ix = indexer(&settings(&cat.url, "http://unused", "/dl"));
    assert!(ix
        .search(&terms("Soon", "Next Year"))
        .await
        .unwrap()
        .is_empty());
    let mut s = settings(&cat.url, "http://unused", "/dl");
    s["earlyReleaseLimit"] = Value::Null;
    assert_eq!(
        indexer(&s)
            .search(&terms("Soon", "Next Year"))
            .await
            .unwrap()
            .len(),
        3
    );

    cat.state.lock().unwrap().fail_status = Some(401);
    let err = ix.search(&terms("Soon", "Next Year")).await.unwrap_err();
    assert!(
        matches!(err, cellarr_download::error::DownloadError::Auth(_)),
        "{err}"
    );
    // The diagnosis names the problem and never repeats the token.
    assert!(!err.to_string().contains(DEV_TOKEN), "{err}");
    cat.state.lock().unwrap().fail_status = Some(500);
    let err = ix.search(&terms("Soon", "Next Year")).await.unwrap_err();
    assert!(!err.to_string().contains(DEV_TOKEN), "{err}");
}

#[tokio::test]
async fn a_web_player_token_goes_with_its_cookie_and_origin() {
    let cat = start_catalog(vec![album("100", "Kind of Blue", "Miles Davis", 5, None)]).await;
    // header.payload.signature, payload {"iss":"AMPWebPlay"}; not a real token.
    let web = "eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJBTVBXZWJQbGF5In0.sig";
    let mut s = settings(&cat.url, "http://unused", "/dl");
    s["developerToken"] = json!(web);
    indexer(&s)
        .search(&terms("Miles Davis", "Kind of Blue"))
        .await
        .unwrap();
    let c = cat.state.lock().unwrap();
    assert_eq!(c.seen[0].1, format!("media-user-token={USER_TOKEN}"));
    assert_eq!(c.seen[0].2, "https://music.apple.com");
}

fn download_client(cat: &str, dec: &str, root: &str) -> AppleMusicClient {
    AppleMusicClient::new(
        "apple",
        AppleMusicSettings::from_json_client(&settings(cat, dec, root)).unwrap(),
    )
}

#[tokio::test]
async fn grab_status_lifecycle_and_removal() {
    let root = tempfile::tempdir().unwrap();
    let cat = start_catalog(vec![album("100", "Kind of Blue", "Miles Davis", 3, None)]).await;
    let dec = start_decryptor(root.path().to_path_buf()).await;
    dec.state.lock().unwrap().materialize = Some(std::sync::Arc::new(|out, req| {
        let dir = req["directory"].as_str().unwrap().to_string();
        std::fs::create_dir_all(out.join(&dir)).unwrap();
        let mut files = Vec::new();
        for t in req["tracks"].as_array().unwrap() {
            let rel = format!("{dir}/{}.m4a", t["title"].as_str().unwrap());
            std::fs::write(out.join(&rel), b"x").unwrap();
            files.push(rel);
        }
        files
    }));
    let client = download_client(&cat.url, &dec.url, root.path().to_str().unwrap());
    client.probe().await.unwrap();

    let id = client
        .add(&grab("applemusicarr://album/100?quality=alac".into()))
        .await
        .unwrap();
    {
        let d = dec.state.lock().unwrap();
        assert_eq!(d.jobs.len(), 1);
        let r = &d.jobs[0].request;
        assert_eq!(r["albumId"], "100");
        assert_eq!(r["artist"], "Miles Davis");
        assert_eq!(r["album"], "Kind of Blue");
        assert_eq!(r["storefront"], "us");
        assert_eq!(r["quality"], "alac");
        assert_eq!(r["directory"], "Miles Davis/Kind of Blue");
        assert_eq!(r["musicUserToken"], USER_TOKEN);
        let t = r["tracks"].as_array().unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[1]["id"], "10002");
        assert_eq!(t[1]["trackNumber"], 2);
        assert_eq!(t[1]["discNumber"], 1);
    }
    assert_eq!(
        client.status(&id).await.unwrap().state,
        DownloadState::Queued
    );
    dec.set_status("running");
    dec.state.lock().unwrap().jobs[0].progress = 50.0;
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Downloading);
    assert!((st.progress - 0.5).abs() < 1e-6);
    assert!(st.content_path.is_none());

    dec.set_status("completed");
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Completed);
    let want = root.path().join("Miles Davis/Kind of Blue");
    assert_eq!(st.content_path.as_deref(), Some(want.to_str().unwrap()));
    assert!(want.join("Track 1.m4a").exists());

    // delete_data removes exactly what the job reported, not foreign files.
    std::fs::write(want.join("keep-me.txt"), b"x").unwrap();
    client.remove(&id, true).await.unwrap();
    assert!(!want.join("Track 1.m4a").exists());
    assert!(want.join("keep-me.txt").exists());
    assert!(
        dec.state.lock().unwrap().cancelled.is_empty(),
        "a finished job is not cancelled"
    );
}

#[tokio::test]
async fn an_unfinished_job_is_cancelled_on_removal_and_failures_say_why() {
    let root = tempfile::tempdir().unwrap();
    let cat = start_catalog(vec![album("100", "Kind of Blue", "Miles Davis", 2, None)]).await;
    let dec = start_decryptor(root.path().to_path_buf()).await;
    let client = download_client(&cat.url, &dec.url, root.path().to_str().unwrap());
    let url = "applemusicarr://album/100?quality=aac256".to_string();

    let id = client.add(&grab(url.clone())).await.unwrap();
    {
        let mut d = dec.state.lock().unwrap();
        d.jobs[0].status = "failed".into();
        d.jobs[0].error = Some("amdl exited 1".into());
    }
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Failed);
    assert_eq!(st.error_string.as_deref(), Some("amdl exited 1"));

    let id2 = client.add(&grab(url.clone())).await.unwrap();
    client.remove(&id2, false).await.unwrap();
    assert_eq!(dec.state.lock().unwrap().cancelled, ["job2"]);

    // A decryptor that forgot the job: not found, nothing deleted.
    dec.state.lock().unwrap().jobs.clear();
    assert!(matches!(
        client.status(&id).await,
        Err(cellarr_download::DownloadError::NotFound(_))
    ));

    // Refusals and unusable links are errors.
    dec.state.lock().unwrap().refuse = Some("queue full".into());
    let err = client.add(&grab(url)).await.unwrap_err();
    assert!(err.to_string().contains("queue full"), "{err}");
    assert!(client.add(&grab("magnet:?xt=x".into())).await.is_err());
    // An album Apple does not have.
    dec.state.lock().unwrap().refuse = None;
    let err = client
        .add(&grab("applemusicarr://album/999?quality=aac256".into()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no tracks"), "{err}");
}

#[tokio::test]
async fn a_hostile_album_name_cannot_escape_the_output_directory() {
    let root = tempfile::tempdir().unwrap();
    let cat = start_catalog(vec![album("100", "../../etc/passwd", "..", 2, None)]).await;
    let dec = start_decryptor(root.path().to_path_buf()).await;
    let client = download_client(&cat.url, &dec.url, root.path().to_str().unwrap());
    let id = client
        .add(&grab("applemusicarr://album/100?quality=aac256".into()))
        .await
        .unwrap();
    let dir = dec.state.lock().unwrap().jobs[0].request["directory"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!dir.contains(".."), "{dir}");
    assert_eq!(
        client.status(&id).await.unwrap().state,
        DownloadState::Queued
    );
}

#[tokio::test]
async fn the_connection_checks_name_the_setting_at_fault_and_never_a_token() {
    use cellarr_download::applemusic::{check_client, check_indexer};
    use cellarr_download::ReqwestTransport;
    let cat = start_catalog(vec![]).await;
    let dec = start_decryptor(std::env::temp_dir()).await;
    let s = settings(&cat.url, &dec.url, "/data");
    let ix = || AppleMusicSettings::from_json_indexer(&s).unwrap();
    let cl = || AppleMusicSettings::from_json_client(&s).unwrap();
    let t = || Box::new(ReqwestTransport::new()) as Box<dyn cellarr_download::HttpTransport>;

    assert!(check_indexer(ix(), t()).await.is_ok());
    assert!(check_client(cl(), t(), t()).await.is_ok());

    // The decryptor is unhealthy: the client fails on decryptorUrl, the indexer does not care.
    dec.state.lock().unwrap().health_ok = false;
    let f = check_client(cl(), t(), t()).await.unwrap_err();
    assert_eq!(f.property, "decryptorUrl");
    assert!(check_indexer(ix(), t()).await.is_ok());
    dec.state.lock().unwrap().health_ok = true;

    // Apple refuses the token.
    cat.state.lock().unwrap().fail_status = Some(401);
    for f in [
        check_indexer(ix(), t()).await.unwrap_err(),
        check_client(cl(), t(), t()).await.unwrap_err(),
    ] {
        assert_eq!(f.property, "developerToken");
        assert!(!f.message.contains(DEV_TOKEN), "{}", f.message);
        assert!(!f.message.contains(USER_TOKEN), "{}", f.message);
    }
    // An unreachable catalogue is a baseUrl problem.
    let mut dead = s.clone();
    dead["baseUrl"] = json!("http://127.0.0.1:1/v1");
    let f = check_indexer(
        AppleMusicSettings::from_json_indexer(&dead).unwrap(),
        Box::new(ReqwestTransport::with_timeout(
            std::time::Duration::from_secs(2),
        )),
    )
    .await
    .unwrap_err();
    assert_eq!(f.property, "baseUrl");
}

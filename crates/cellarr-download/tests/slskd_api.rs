//! The slskd client and indexer against a fake slskd over real HTTP.

#[path = "common/fake_slskd.rs"]
mod fake_slskd;

use cellarr_core::{
    ContentRef, DownloadClient, DownloadState, GrabRequest, Indexer, IndexerId, Protocol, Release,
    SearchTerms,
};
use cellarr_download::{SlskdClient, SlskdIndexer, SlskdSettings, SlskdTarget};
use fake_slskd::{peer, start, API_KEY};
use serde_json::json;

fn settings(url: &str, root: &str) -> SlskdSettings {
    SlskdSettings::from_json(&json!({
        "baseUrl": url, "apiKey": API_KEY, "downloadRoot": root,
        "pollIntervalMs": 5, "searchTimeoutSecs": 1,
    }))
    .unwrap()
}

fn terms() -> SearchTerms {
    SearchTerms {
        queries: vec!["Radiohead OK Computer".into()],
        ids: vec![
            ("artist".into(), "Radiohead".into()),
            ("album".into(), "OK Computer".into()),
        ],
        ..SearchTerms::default()
    }
}

const DIR: &str = "@@u1\\Music\\Radiohead\\OK Computer (1997)";

#[tokio::test]
async fn a_search_polls_until_complete_and_returns_ranked_releases() {
    let srv = start(vec![
        peer("mp3guy", DIR, 12, "mp3", Some(320)),
        peer("flacguy", DIR, 12, "flac", None),
        peer("other", "x\\Coldplay\\Parachutes", 10, "flac", None),
    ])
    .await;
    srv.state.lock().unwrap().incomplete_polls = 2;
    let ix = SlskdIndexer::new(IndexerId::new(), "slsk", settings(&srv.url, "/dl"));
    let found = ix.search(&terms()).await.unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].title, "Radiohead - OK Computer [FLAC]");
    assert_eq!(found[1].title, "Radiohead - OK Computer [MP3 320]");
    let t = SlskdTarget::from_url(&found[0].download_url).unwrap();
    assert_eq!(t.username, "flacguy");
    assert_eq!(t.files.len(), 12);
    assert_eq!(found[0].size, Some(t.files.iter().map(|f| f.size).sum()));
    let f = srv.state.lock().unwrap();
    assert_eq!(f.searches, ["radiohead ok computer"]);
    assert!(f.polls >= 3, "polled until complete");
    assert_eq!(f.searches_deleted, 1, "the search is cleaned up");
}

#[tokio::test]
async fn a_wrong_key_is_an_auth_error_and_missing_names_search_nothing() {
    let srv = start(vec![]).await;
    let mut s = settings(&srv.url, "/dl");
    s.api_key = "nope".into();
    let ix = SlskdIndexer::new(IndexerId::new(), "slsk", s);
    let err = ix.search(&terms()).await.unwrap_err();
    assert!(
        matches!(err, cellarr_download::error::DownloadError::Auth(_)),
        "{err}"
    );
    let ix = SlskdIndexer::new(IndexerId::new(), "slsk", settings(&srv.url, "/dl"));
    assert!(ix.search(&SearchTerms::default()).await.unwrap().is_empty());
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
async fn grab_status_lifecycle_and_removal() {
    let root = tempfile::tempdir().unwrap();
    let srv = start(vec![peer("flacguy", DIR, 3, "flac", None)]).await;
    let s = settings(&srv.url, root.path().to_str().unwrap());
    let ix = SlskdIndexer::new(IndexerId::new(), "slsk", s.clone());
    let release = ix.search(&terms()).await.unwrap().remove(0);
    let client = SlskdClient::new("slsk", s);

    let id = client
        .add(&grab(release.download_url.clone()))
        .await
        .unwrap();
    {
        let f = srv.state.lock().unwrap();
        assert_eq!(f.enqueued.len(), 1);
        assert_eq!(f.enqueued[0].0, "flacguy");
        assert_eq!(f.enqueued[0].1.as_array().unwrap().len(), 3);
    }
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Queued);
    assert!(st.content_path.is_none());

    srv.set_state_all("InProgress");
    assert_eq!(
        client.status(&id).await.unwrap().state,
        DownloadState::Downloading
    );

    srv.set_state_all("Completed, Succeeded");
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Completed);
    // Derived from the transfer + root, never from slskd's /options.
    let want = root.path().join("flacguy").join("OK Computer (1997)");
    assert_eq!(st.content_path.as_deref(), Some(want.to_str().unwrap()));

    // Files on disk: delete_data removes exactly what was fetched.
    std::fs::create_dir_all(&want).unwrap();
    for i in 1..=3 {
        std::fs::write(want.join(format!("{i:02} - Track {i}.flac")), b"x").unwrap();
    }
    std::fs::write(want.join("keep-me.txt"), b"x").unwrap();
    client.remove(&id, true).await.unwrap();
    assert_eq!(srv.state.lock().unwrap().deleted.len(), 3);
    assert!(
        want.join("keep-me.txt").exists(),
        "foreign files are left alone"
    );
    assert!(!want.join("01 - Track 1.flac").exists());
    assert!(matches!(
        client.status(&id).await,
        Err(cellarr_download::DownloadError::NotFound(_))
    ));
}

#[tokio::test]
async fn the_flat_layout_is_used_when_only_it_exists() {
    let root = tempfile::tempdir().unwrap();
    let flat = root.path().join("OK Computer (1997)");
    std::fs::create_dir_all(&flat).unwrap();
    let srv = start(vec![peer("flacguy", DIR, 3, "flac", None)]).await;
    let s = settings(&srv.url, root.path().to_str().unwrap());
    let release = SlskdIndexer::new(IndexerId::new(), "i", s.clone())
        .search(&terms())
        .await
        .unwrap()
        .remove(0);
    let client = SlskdClient::new("c", s);
    let id = client.add(&grab(release.download_url)).await.unwrap();
    srv.set_state_all("Completed, Succeeded");
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.content_path.as_deref(), Some(flat.to_str().unwrap()));
}

#[tokio::test]
async fn a_failed_file_fails_the_download_with_a_reason_and_refusals_are_errors() {
    let srv = start(vec![peer("flacguy", DIR, 3, "flac", None)]).await;
    let s = settings(&srv.url, "/dl");
    let release = SlskdIndexer::new(IndexerId::new(), "i", s.clone())
        .search(&terms())
        .await
        .unwrap()
        .remove(0);
    let client = SlskdClient::new("c", s);
    let id = client
        .add(&grab(release.download_url.clone()))
        .await
        .unwrap();
    srv.state.lock().unwrap().transfers[1].4 = "Completed, TimedOut".into();
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Failed);
    assert!(st.error_string.unwrap().contains("1 of 3 files failed"));

    srv.state.lock().unwrap().refuse = Some("Too many files".into());
    let err = client.add(&grab(release.download_url)).await.unwrap_err();
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(client.add(&grab("magnet:?xt=x".into())).await.is_err());
}

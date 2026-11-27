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
        peer(
            "u_a_search_polls_until_complete_and_returns_ranked_releases",
            DIR,
            12,
            "flac",
            None,
        ),
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
    assert_eq!(
        t.username,
        "u_a_search_polls_until_complete_and_returns_ranked_releases"
    );
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
    let srv = start(vec![peer(
        "u_grab_status_lifecycle_and_removal",
        DIR,
        3,
        "flac",
        None,
    )])
    .await;
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
        assert_eq!(f.enqueued[0].0, "u_grab_status_lifecycle_and_removal");
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
    let want = root
        .path()
        .join("u_grab_status_lifecycle_and_removal")
        .join("OK Computer (1997)");
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
    let srv = start(vec![peer(
        "u_the_flat_layout_is_used_when_only_it_exists",
        DIR,
        3,
        "flac",
        None,
    )])
    .await;
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
    let srv = start(vec![peer(
        "u_a_failed_file_fails_the_download_with_a_reason_and_refusals_are_errors",
        DIR,
        3,
        "flac",
        None,
    )])
    .await;
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

/// Enqueue a 3-track grab and hand back the client, id and server.
async fn grabbed(root: &str, user: &str) -> (SlskdClient, String, fake_slskd::Server) {
    let srv = start(vec![peer(user, DIR, 3, "flac", None)]).await;
    let s = settings(&srv.url, root);
    let release = SlskdIndexer::new(IndexerId::new(), "i", s.clone())
        .search(&terms())
        .await
        .unwrap()
        .remove(0);
    let client = SlskdClient::new("c", s);
    let id = client.add(&grab(release.download_url)).await.unwrap();
    (client, id, srv)
}

#[tokio::test]
async fn status_never_goes_back_to_queued_once_a_file_progressed() {
    let (client, id, srv) = grabbed(
        "/dl",
        "u_status_never_goes_back_to_queued_once_a_file_progressed",
    )
    .await;
    assert_eq!(
        client.status(&id).await.unwrap().state,
        DownloadState::Queued
    );

    // One file finishes, one is mid-transfer, one waits remotely: Downloading.
    {
        let mut f = srv.state.lock().unwrap();
        f.transfers[0].4 = "Completed, Succeeded".into();
        f.transfers[0].5 = f.transfers[0].3;
        f.transfers[1].4 = "InProgress".into();
        f.transfers[1].5 = 500;
    }
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Downloading);
    let before = st.progress;
    assert!(before > 0.0 && before < 1.0, "{before}");

    // The in-flight file moves on to the next one: between files every
    // remaining transfer is Requested / Queued, Remotely. Still Downloading.
    {
        let mut f = srv.state.lock().unwrap();
        f.transfers[1].4 = "Requested".into();
        f.transfers[1].5 = 0;
        f.transfers[2].4 = "Queued, Remotely".into();
    }
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Downloading);
    assert!(st.progress >= before, "progress never goes back");

    // A listing that fails, then one that is empty: the last status stays.
    srv.state.lock().unwrap().fail_listing = true;
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Downloading);
    assert!(st.progress >= before);
    srv.state.lock().unwrap().fail_listing = false;
    srv.state.lock().unwrap().empty_listing = true;
    assert_eq!(
        client.status(&id).await.unwrap().state,
        DownloadState::Downloading
    );

    // Back to normal, finishing: Completed, with every file counted.
    {
        let mut f = srv.state.lock().unwrap();
        f.empty_listing = false;
    }
    srv.set_state_all("Completed, Succeeded");
    let st = client.status(&id).await.unwrap();
    assert_eq!(st.state, DownloadState::Completed);
    assert_eq!(st.progress, 1.0);
}

#[tokio::test]
async fn a_read_error_before_anything_is_known_is_an_error_not_queued() {
    let (client, id, srv) = grabbed(
        "/dl",
        "u_a_read_error_before_anything_is_known_is_an_error_not_queued",
    )
    .await;
    srv.state.lock().unwrap().fail_listing = true;
    // Nothing has been read successfully for this grab yet.
    assert!(client.status(&id).await.is_err());
}

#[tokio::test]
async fn an_empty_listing_is_forgiven_a_while_then_the_grab_is_gone() {
    let (client, id, srv) = grabbed(
        "/dl",
        "u_an_empty_listing_is_forgiven_a_while_then_the_grab_is_gone",
    )
    .await;
    srv.set_state_all("InProgress");
    assert_eq!(
        client.status(&id).await.unwrap().state,
        DownloadState::Downloading
    );
    srv.state.lock().unwrap().empty_listing = true;
    for _ in 0..cellarr_download::slskd::EMPTY_LISTING_GRACE {
        assert_eq!(
            client.status(&id).await.unwrap().state,
            DownloadState::Downloading
        );
    }
    assert!(matches!(
        client.status(&id).await,
        Err(cellarr_download::DownloadError::NotFound(_))
    ));
}

#[tokio::test]
async fn progress_counts_bytes_across_all_files() {
    let (client, id, srv) = grabbed("/dl", "u_progress_counts_bytes_across_all_files").await;
    {
        let mut f = srv.state.lock().unwrap();
        let sizes: Vec<u64> = f.transfers.iter().map(|t| t.3).collect();
        for (t, half) in f.transfers.iter_mut().zip(sizes) {
            t.4 = "InProgress".into();
            t.5 = half / 2;
        }
    }
    let st = client.status(&id).await.unwrap();
    assert!((st.progress - 0.5).abs() < 0.01, "{}", st.progress);
}
#[tokio::test]
async fn removal_deletes_exactly_this_grabs_transfer_records_and_keeps_files() {
    let root = tempfile::tempdir().unwrap();
    let (client, id, srv) = grabbed(
        root.path().to_str().unwrap(),
        "u_removal_deletes_exactly_this_grabs_transfer_records_and_keeps_files",
    )
    .await;
    // A foreign transfer from the same peer that is not part of this grab.
    srv.state.lock().unwrap().transfers.push((
        "u_removal_deletes_exactly_this_grabs_transfer_records_and_keeps_files".into(),
        "foreign".into(),
        "@@u1\\Music\\Other\\01.flac".into(),
        10,
        "Completed, Succeeded".into(),
        10,
    ));
    srv.set_state_all("Completed, Succeeded");
    let want = root
        .path()
        .join("u_removal_deletes_exactly_this_grabs_transfer_records_and_keeps_files")
        .join("OK Computer (1997)");
    std::fs::create_dir_all(&want).unwrap();
    std::fs::write(want.join("01 - Track 1.flac"), b"x").unwrap();

    client.remove(&id, false).await.unwrap();
    let f = srv.state.lock().unwrap();
    assert_eq!(f.delete_calls.len(), 3, "{:?}", f.delete_calls);
    assert!(f.delete_calls.iter().all(|c| c.0
        == "u_removal_deletes_exactly_this_grabs_transfer_records_and_keeps_files"
        && c.1 != "foreign"));
    assert!(f.transfers.iter().any(|t| t.1 == "foreign"));
    assert!(want.join("01 - Track 1.flac").exists(), "files stay");
}

#[tokio::test]
async fn the_indexer_uses_the_wanted_track_list_to_pick_the_folder() {
    // A 6-track and a 3-track folder for a 3-track album: only the 3-track one
    // matches, the 6-track one cannot be this album's tracks.
    let srv = start(vec![
        peer("six", DIR, 6, "flac", None),
        peer("three", DIR, 3, "mp3", Some(320)),
    ])
    .await;
    let ix = SlskdIndexer::new(IndexerId::new(), "slsk", settings(&srv.url, "/dl"));
    let mut t = terms();
    t.track_titles = (1..=3).map(|i| format!("Track {i}")).collect();
    let found = ix.search(&t).await.unwrap();
    let users: Vec<String> = found
        .iter()
        .map(|r| SlskdTarget::from_url(&r.download_url).unwrap().username)
        .collect();
    assert_eq!(
        users.first().map(String::as_str),
        Some("three"),
        "{users:?}"
    );
    // And a two-track folder is not offered for it at all.
    let srv = start(vec![peer("two", DIR, 2, "flac", None)]).await;
    let mut s = settings(&srv.url, "/dl");
    s.minimum_files = 1;
    let ix = SlskdIndexer::new(IndexerId::new(), "slsk", s);
    assert!(ix.search(&t).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_connection_check_tells_a_bad_key_from_a_bad_url() {
    let srv = start(vec![]).await;
    // The fake answers /application only through its auth layer; a 404 body is
    // a reachable slskd, which is not what the check asks, so use the good path
    // against a route that exists: the key check is what matters here.
    let mut bad = settings(&srv.url, "/dl");
    bad.api_key = "wrong".into();
    let f = cellarr_download::slskd::check_connection(
        bad,
        Box::new(cellarr_download::ReqwestTransport::new()),
    )
    .await
    .unwrap_err();
    assert_eq!(f.property, "apiKey");
    assert!(!f.message.contains("wrong"));

    let mut gone = settings("http://127.0.0.1:1", "/dl");
    gone.api_key = "secret-key-value".into();
    let f = cellarr_download::slskd::check_connection(
        gone,
        Box::new(cellarr_download::ReqwestTransport::with_timeout(
            std::time::Duration::from_secs(2),
        )),
    )
    .await
    .unwrap_err();
    assert_eq!(f.property, "baseUrl");
    assert!(!f.message.contains("secret-key-value"));

    let ok = cellarr_download::slskd::check_connection(
        settings(&srv.url, "/dl"),
        Box::new(cellarr_download::ReqwestTransport::new()),
    )
    .await;
    assert!(ok.is_ok(), "{ok:?}");
}

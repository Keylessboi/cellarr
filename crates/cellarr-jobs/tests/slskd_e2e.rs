//! End to end through the pipeline with a fake slskd: the configured `slskd`
//! indexer searches Soulseek, the album is grabbed through the native client, the
//! transfers report complete, and the files that landed under slskd's download
//! root are imported into a temp library.

#[path = "../../cellarr-download/tests/common/fake_slskd.rs"]
mod fake_slskd;

use std::path::PathBuf;

use cellarr_core::repo::ContentRepository;
use cellarr_core::{
    ContentRef, Coordinates, CustomFormat, ExtraFileImport, IndexerConfig, IndexerId, Library,
    LibraryId, MediaType, Protocol, QualityProfile, QualityProfileId, QualityRanking,
};
use cellarr_db::{Database, NewAlbum, NewArtist};
use cellarr_decide::ProperRepackPolicy;
use cellarr_download::{SlskdClient, SlskdSettings};
use cellarr_jobs::clock::LogicalClock;
use cellarr_jobs::music_lookup::DbMusicLookup;
use cellarr_jobs::runner::{PipelineRunner, RunOutcome, RunnerConfig};
use cellarr_jobs::DbIndexerSet;
use cellarr_media::{MediaRegistry, MusicModule};
use serde_json::json;

fn profile() -> QualityProfile {
    let ranking = QualityRanking::default();
    QualityProfile {
        id: QualityProfileId::new(),
        name: "any".into(),
        allowed_qualities: ranking
            .qualities
            .iter()
            .map(|q| q.rank)
            .filter(|r| *r != 0)
            .collect(),
        upgrades_allowed: true,
        cutoff_quality: ranking.by_name("FLAC").unwrap().rank,
        min_custom_format_score: 0,
        upgrade_until_custom_format_score: 100,
        required_languages: Vec::new(),
    }
}

fn config(root: PathBuf) -> RunnerConfig {
    RunnerConfig {
        content_tag_ids: Vec::new(),
        profile: profile(),
        custom_formats: Vec::<CustomFormat>::new(),
        ranking: QualityRanking::default(),
        proper_repack_policy: ProperRepackPolicy::default(),
        library_root: root,
        naming_format: "{Title}.{Extension}".into(),
        anime_naming_format: String::new(),
        series_type: cellarr_core::SeriesType::Standard,
        indexer_id: IndexerId::new(),
        client_id: cellarr_core::DownloadClientId::new(),
        category: "cellarr".into(),
        max_track_polls: 5,
        stall_grace_polls: 0,
        track_poll_interval: std::time::Duration::ZERO,
        client_host: String::new(),
        remote_path_mappings: Vec::new(),
        write_nfo: false,
        delay_profiles: Vec::new(),
        release_profiles: Vec::new(),
        content_tags: Vec::new(),
        permissions: cellarr_core::ImportPermissions::default(),
        extra_files: ExtraFileImport::default(),
        indexer_criteria: std::collections::HashMap::new(),
    }
}

async fn shelf() -> (Database, ContentRef) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.keep().join("t.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let lib = LibraryId::new();
    db.config()
        .upsert_library(&Library {
            id: lib,
            media_type: MediaType::Music,
            name: "music".into(),
            root_folders: vec!["/music".into()],
            default_quality_profile: QualityProfileId::new(),
        })
        .await
        .unwrap();
    let artist = db
        .music()
        .add_artist(
            lib,
            NewArtist {
                mbid: "mb-a".into(),
                name: "Radiohead".into(),
                path: "/music/Radiohead".into(),
                metadata_profile_id: 1,
                monitored: true,
                monitor_new_albums: true,
                ..NewArtist::default()
            },
        )
        .await
        .unwrap();
    let album = db
        .music()
        .upsert_albums(
            lib,
            artist.content_id,
            vec![NewAlbum {
                mbid: "mb-rg".into(),
                title: "OK Computer".into(),
                album_type: "Album".into(),
                release_date: Some("1997-05-21".into()),
                monitored: true,
                ..NewAlbum::default()
            }],
        )
        .await
        .unwrap()
        .remove(0);
    let r = db.content().get(album.content_id).await.unwrap().unwrap();
    (db, r)
}

fn peer(user: &str, dir: &str, ext: &str, names: &[&str]) -> serde_json::Value {
    let files: Vec<_> = names
        .iter()
        .map(|n| json!({"filename": format!("{dir}\\{n}.{ext}"), "size": 30_000_000, "isLocked": false}))
        .collect();
    json!({"username": user, "files": files, "hasFreeUploadSlot": true,
           "uploadSpeed": 1, "queueLength": 0})
}

#[tokio::test]
async fn a_soulseek_album_is_searched_grabbed_and_imported() {
    let (db, album) = shelf().await;
    assert_eq!(album.coords, Coordinates::Album);

    let dir = "@@h\\Music\\Radiohead\\OK Computer (1997)";
    let names = ["01 - Airbag", "02 - Paranoid Android"];
    let srv = fake_slskd::start(vec![
        peer("mp3guy", dir, "mp3", &names),
        peer("flacguy", dir, "flac", &names),
    ])
    .await;
    srv.state.lock().unwrap().auto_complete = true;

    // slskd has "downloaded" the folder under its root: <root>/<user>/<folder>.
    let root = tempfile::tempdir().unwrap();
    let landed = root.path().join("flacguy").join("OK Computer (1997)");
    std::fs::create_dir_all(&landed).unwrap();
    for (i, (n, title)) in [
        ("01 - Airbag", "Airbag"),
        ("02 - Paranoid Android", "Paranoid Android"),
    ]
    .iter()
    .enumerate()
    {
        cellarr_fs::audio::testing::write_flac(
            &landed.join(format!("{n}.flac")),
            16,
            &[
                ("TITLE", title),
                ("ARTIST", "Radiohead"),
                ("ALBUM", "OK Computer"),
                ("TRACKNUMBER", &(i + 1).to_string()),
                ("DISCNUMBER", "1"),
            ],
        )
        .unwrap();
    }

    // The indexer is a configured `slskd` row, resolved like any other.
    let settings = json!({
        "baseUrl": srv.url, "apiKey": fake_slskd::API_KEY,
        "downloadRoot": root.path(), "pollIntervalMs": 5, "searchTimeoutSecs": 1,
        "minimumFiles": 2,
    });
    db.config()
        .upsert_indexer(&IndexerConfig {
            id: IndexerId::new(),
            name: "Soulseek".into(),
            kind: "slskd".into(),
            protocol: Protocol::Torrent,
            enabled: true,
            enable_rss: false,
            enable_automatic_search: true,
            enable_interactive_search: true,
            priority: 25,
            criteria: Default::default(),
            tags: vec![],
            settings: settings.clone(),
        })
        .await
        .unwrap();
    let indexer = DbIndexerSet::new(db.clone());
    let client = SlskdClient::new("Soulseek", SlskdSettings::from_json(&settings).unwrap());

    let library = tempfile::tempdir().unwrap();
    let cfg = config(library.path().to_path_buf());
    let mut registry = MediaRegistry::new();
    registry.register(MusicModule::new(DbMusicLookup::new(db.clone())));
    let clock = LogicalClock::default();
    let runner = PipelineRunner::new(&indexer, &client, &registry, &db, &clock, &cfg);

    let outcome = runner.run(&album).await.unwrap();
    assert!(
        matches!(outcome, RunOutcome::Imported { .. }),
        "{outcome:?}"
    );

    {
        let f = srv.state.lock().unwrap();
        assert_eq!(f.searches, ["radiohead ok computer"]);
        assert_eq!(f.enqueued.len(), 1, "one folder enqueued");
        assert_eq!(f.enqueued[0].0, "flacguy", "FLAC preferred over MP3");
        assert_eq!(f.enqueued[0].1.as_array().unwrap().len(), 2);
    }
    let base = library.path().join("Radiohead/OK Computer (1997)");
    assert!(
        base.join("01 - Airbag.flac").exists(),
        "{:?}",
        std::fs::read_dir(library.path())
    );
    assert!(base.join("02 - Paranoid Android.flac").exists());
    let files = db.music().track_files_for_album(album.id).await.unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.file.quality.name == "FLAC"));
    assert!(db.content().monitored_missing().await.unwrap().is_empty());
}

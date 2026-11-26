//! End to end through the pipeline with a fake Apple Music catalogue and a fake
//! decryptor: the configured `applemusicarr` indexer searches the catalogue, the
//! album is grabbed through the native client (track list handed to the
//! decryptor), the job completes, and the files that landed under the decryptor's
//! output directory are imported into a temp library. Nothing here touches Apple.

#[path = "../../cellarr-download/tests/common/fake_apple.rs"]
mod fake_apple;

use std::path::PathBuf;

use cellarr_core::repo::ContentRepository;
use cellarr_core::{
    ContentRef, Coordinates, CustomFormat, ExtraFileImport, IndexerConfig, IndexerId, Library,
    LibraryId, MediaType, Protocol, QualityProfile, QualityProfileId, QualityRanking,
};
use cellarr_db::{Database, NewAlbum, NewArtist};
use cellarr_decide::ProperRepackPolicy;
use cellarr_download::{AppleMusicClient, AppleMusicSettings};
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

#[tokio::test]
async fn an_apple_music_album_is_searched_grabbed_and_imported() {
    let (db, album) = shelf().await;
    assert_eq!(album.coords, Coordinates::Album);

    let cat = fake_apple::start_catalog(vec![
        fake_apple::album("500", "OK Computer", "Radiohead", 2, Some(&["lossless"])),
        fake_apple::album("501", "OK Computer OKNOTOK 1997 2017", "Radiohead", 3, None),
        fake_apple::album("502", "Kid A", "Radiohead", 4, None),
    ])
    .await;

    // The decryptor's OUTPUT_DIR: it writes the album folder the client asked for.
    let out = tempfile::tempdir().unwrap();
    let dec = fake_apple::start_decryptor(out.path().to_path_buf()).await;
    {
        let mut d = dec.state.lock().unwrap();
        d.auto_advance = true;
        // The real decryptor writes .m4a; the repo's tag writer makes FLAC, so the
        // fake decryptor "produces" tagged FLAC files named after the tracks.
        d.materialize = Some(std::sync::Arc::new(|out, req| {
            let dir = req["directory"].as_str().unwrap().to_string();
            std::fs::create_dir_all(out.join(&dir)).unwrap();
            let mut files = Vec::new();
            for t in req["tracks"].as_array().unwrap() {
                let n = t["trackNumber"].as_u64().unwrap();
                let title = t["title"].as_str().unwrap();
                let rel = format!("{dir}/{n:02} {title}.flac");
                cellarr_fs::audio::testing::write_flac(
                    &out.join(&rel),
                    16,
                    &[
                        ("TITLE", title),
                        ("ARTIST", req["artist"].as_str().unwrap()),
                        ("ALBUM", req["album"].as_str().unwrap()),
                        ("TRACKNUMBER", &n.to_string()),
                        ("DISCNUMBER", "1"),
                    ],
                )
                .unwrap();
                files.push(rel);
            }
            files
        }));
    }
    // Track titles come from the catalogue ("Track 1", ...); the tags carry them.

    // Both halves are rows of the migrated kind, built like any other row.
    let indexer_settings = json!({
        "baseUrl": cat.url, "developerToken": fake_apple::DEV_TOKEN, "storefront": "us",
    });
    let client_settings = json!({
        "baseUrl": cat.url, "developerToken": fake_apple::DEV_TOKEN,
        "musicUserToken": fake_apple::USER_TOKEN, "storefront": "us",
        "decryptorUrl": dec.url, "downloadPath": out.path(),
    });
    db.config()
        .upsert_indexer(&IndexerConfig {
            id: IndexerId::new(),
            name: "AppleMusicarr".into(),
            kind: "applemusicarrindexer".into(),
            protocol: Protocol::Torrent,
            enabled: true,
            enable_rss: false,
            enable_automatic_search: true,
            enable_interactive_search: true,
            priority: 25,
            criteria: Default::default(),
            tags: vec![],
            settings: indexer_settings,
        })
        .await
        .unwrap();
    let indexer = DbIndexerSet::new(db.clone());
    let client = AppleMusicClient::new(
        "AppleMusicarr",
        AppleMusicSettings::from_json_client(&client_settings).unwrap(),
    );

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
        let c = cat.state.lock().unwrap();
        assert_eq!(c.searches[0].1, "Radiohead OK Computer");
        assert_eq!(c.lookups, [("us".to_string(), "500".to_string())]);
        assert!(c
            .seen
            .iter()
            .all(|s| s.0 == format!("Bearer {}", fake_apple::DEV_TOKEN)));
        let d = dec.state.lock().unwrap();
        assert_eq!(d.jobs.len(), 1, "one album queued");
        let r = &d.jobs[0].request;
        // Only the exact title was grabbed, never the "OKNOTOK" neighbour.
        assert_eq!(r["albumId"], "500");
        assert_eq!(r["directory"], "Radiohead/OK Computer");
        assert_eq!(r["musicUserToken"], fake_apple::USER_TOKEN);
        assert_eq!(r["tracks"].as_array().unwrap().len(), 2);
        assert!(["aac256", "alac", "atmos"].contains(&r["quality"].as_str().unwrap()));
    }
    let base = library.path().join("Radiohead/OK Computer (1997)");
    assert!(
        base.join("01 - Track 1.flac").exists(),
        "{:?}",
        std::process::Command::new("find")
            .arg(library.path())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    );
    assert!(base.join("02 - Track 2.flac").exists());
    let files = db.music().track_files_for_album(album.id).await.unwrap();
    assert_eq!(files.len(), 2);
    assert!(db.content().monitored_missing().await.unwrap().is_empty());
}

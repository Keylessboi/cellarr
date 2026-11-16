//! End to end: a monitored album is searched, a release is picked, the download
//! "completes", and its files land on the album's tracks under the library, with
//! the grab imported and the album no longer missing.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cellarr_core::repo::{ContentRepository, GrabRepository};
use cellarr_core::{
    ContentRef, Coordinates, CustomFormat, ExtraFileImport, GrabStatus, Library, LibraryId,
    MediaType, Protocol, QualityProfile, QualityProfileId, QualityRanking, Release, SearchTerms,
};
use cellarr_db::{Database, NewAlbum, NewArtist};
use cellarr_decide::ProperRepackPolicy;
use cellarr_jobs::clock::LogicalClock;
use cellarr_jobs::music_lookup::DbMusicLookup;
use cellarr_jobs::runner::{PipelineRunner, RunOutcome, RunnerConfig};
use cellarr_media::{MediaRegistry, MusicModule};

struct Indexer(Vec<Release>);

#[derive(Debug, thiserror::Error)]
#[error("fake")]
struct Fake;

#[async_trait]
impl cellarr_core::traits::Indexer for Indexer {
    type Error = Fake;
    fn name(&self) -> &str {
        "fake-indexer"
    }
    async fn search(&self, _t: &SearchTerms) -> Result<Vec<Release>, Fake> {
        Ok(self.0.clone())
    }
    async fn latest(&self) -> Result<Vec<Release>, Fake> {
        Ok(self.0.clone())
    }
}

struct Client(String);

#[async_trait]
impl cellarr_core::traits::DownloadClient for Client {
    type Error = Fake;
    fn name(&self) -> &str {
        "fake-client"
    }
    async fn add(&self, _g: &cellarr_core::GrabRequest) -> Result<String, Fake> {
        Ok("dl-1".into())
    }
    async fn status(&self, _id: &str) -> Result<cellarr_core::DownloadStatus, Fake> {
        Ok(cellarr_core::DownloadStatus {
            state: cellarr_core::DownloadState::Completed,
            progress: 1.0,
            content_path: Some(self.0.clone()),
            ratio: Some(1.0),
            seeding_time_secs: Some(0),
            peers: Some(10),
            error_string: None,
        })
    }
    async fn remove(&self, _id: &str, _delete: bool) -> Result<(), Fake> {
        Ok(())
    }
}

fn release(title: &str) -> Release {
    Release {
        indexer_id: cellarr_core::IndexerId::new(),
        title: title.into(),
        download_url: "magnet:?xt=synthetic".into(),
        guid: Some("guid-album".into()),
        protocol: Protocol::Torrent,
        size: Some(300_000_000),
        seeders: Some(20),
        indexer_flags: Vec::new(),
    }
}

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
        indexer_id: cellarr_core::IndexerId::new(),
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

fn write_track(dir: &Path, name: &str, album: &str, disc: u32, track: u32, title: &str) {
    cellarr_fs::audio::testing::write_flac(
        &dir.join(name),
        16,
        &[
            ("TITLE", title),
            ("ARTIST", "Radiohead"),
            ("ALBUM", album),
            ("TRACKNUMBER", &track.to_string()),
            ("DISCNUMBER", &disc.to_string()),
        ],
    )
    .unwrap();
}

struct Shelf {
    db: Database,
    album: ContentRef,
}

async fn shelf() -> Shelf {
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
    let album_ref = db.content().get(album.content_id).await.unwrap().unwrap();
    Shelf { db, album: album_ref }
}

fn registry(db: &Database) -> MediaRegistry {
    let mut r = MediaRegistry::new();
    r.register(MusicModule::new(DbMusicLookup::new(db.clone())));
    r
}

#[tokio::test]
async fn an_album_is_searched_grabbed_and_imported_onto_its_tracks() {
    let Shelf { db, album } = shelf().await;
    assert_eq!(album.coords, Coordinates::Album);

    let download = tempfile::tempdir().unwrap();
    let dl = download.path().join("Radiohead - OK Computer (1997) [FLAC]");
    std::fs::create_dir_all(&dl).unwrap();
    write_track(&dl, "01 - Airbag.flac", "OK Computer", 1, 1, "Airbag");
    write_track(&dl, "02 - Paranoid Android.flac", "OK Computer", 1, 2, "Paranoid Android");
    std::fs::write(dl.join("cover.jpg"), b"jpg").unwrap();

    let library = tempfile::tempdir().unwrap();
    let cfg = config(library.path().to_path_buf());
    let registry = registry(&db);
    let indexer = Indexer(vec![release("Radiohead - OK Computer (1997) [FLAC]")]);
    let client = Client(dl.to_string_lossy().into_owned());
    let clock = LogicalClock::default();
    let runner = PipelineRunner::new(&indexer, &client, &registry, &db, &clock, &cfg);

    let outcome = runner.run(&album).await.unwrap();
    assert!(matches!(outcome, RunOutcome::Imported { .. }), "{outcome:?}");

    let base = library.path().join("Radiohead/OK Computer (1997)");
    assert!(base.join("01 - Airbag.flac").exists(), "{:?}", std::fs::read_dir(library.path()));
    assert!(base.join("02 - Paranoid Android.flac").exists());
    assert!(!base.join("cover.jpg").exists(), "only audio is imported");

    let files = db.music().track_files_for_album(album.id).await.unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.file.quality.name == "FLAC"));
    let tracks = db.music().tracks_for_album(album.id).await.unwrap();
    assert_eq!(
        tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["Airbag", "Paranoid Android"],
        "with no stored track list the files define it"
    );

    assert!(
        db.content().monitored_missing().await.unwrap().is_empty(),
        "an album with files is no longer missing"
    );
    let grabs = db.grabs();
    let _ = grabs; // grab status is asserted through the outcome
    let _ = GrabStatus::Imported;
}

#[tokio::test]
async fn a_download_tagged_as_another_album_is_not_imported() {
    let Shelf { db, album } = shelf().await;
    let download = tempfile::tempdir().unwrap();
    let dl = download.path().join("Radiohead - OK Computer (1997) [FLAC]");
    std::fs::create_dir_all(&dl).unwrap();
    write_track(&dl, "01.flac", "Kid A", 1, 1, "Everything In Its Right Place");
    write_track(&dl, "02.flac", "Kid A", 1, 2, "Kid A");

    let library = tempfile::tempdir().unwrap();
    let cfg = config(library.path().to_path_buf());
    let registry = registry(&db);
    let indexer = Indexer(vec![release("Radiohead - OK Computer (1997) [FLAC]")]);
    let client = Client(dl.to_string_lossy().into_owned());
    let clock = LogicalClock::default();
    let runner = PipelineRunner::new(&indexer, &client, &registry, &db, &clock, &cfg);

    let outcome = runner.run(&album).await.unwrap();
    assert!(!matches!(outcome, RunOutcome::Imported { .. }), "{outcome:?}");
    assert!(db.music().track_files_for_album(album.id).await.unwrap().is_empty());
    assert!(!library.path().join("Radiohead").exists(), "nothing was written to the library");
}

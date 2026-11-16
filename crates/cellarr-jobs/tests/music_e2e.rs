//! End to end: a monitored album is searched, a release is picked, the download
//! "completes", and its files land on the album's tracks under the library, with
//! the grab imported and the album no longer missing.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cellarr_core::repo::ContentRepository;
use cellarr_core::{
    ContentRef, Coordinates, CustomFormat, ExtraFileImport, Library, LibraryId, MediaType,
    Protocol, QualityProfile, QualityProfileId, QualityRanking, Release, SearchTerms,
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
    Shelf {
        db,
        album: album_ref,
    }
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
    let dl = download
        .path()
        .join("Radiohead - OK Computer (1997) [FLAC]");
    std::fs::create_dir_all(&dl).unwrap();
    write_track(&dl, "01 - Airbag.flac", "OK Computer", 1, 1, "Airbag");
    write_track(
        &dl,
        "02 - Paranoid Android.flac",
        "OK Computer",
        1,
        2,
        "Paranoid Android",
    );
    std::fs::write(dl.join("cover.jpg"), b"jpg").unwrap();

    let library = tempfile::tempdir().unwrap();
    let cfg = config(library.path().to_path_buf());
    let registry = registry(&db);
    let indexer = Indexer(vec![release("Radiohead - OK Computer (1997) [FLAC]")]);
    let client = Client(dl.to_string_lossy().into_owned());
    let clock = LogicalClock::default();
    let runner = PipelineRunner::new(&indexer, &client, &registry, &db, &clock, &cfg);

    let outcome = runner.run(&album).await.unwrap();
    assert!(
        matches!(outcome, RunOutcome::Imported { .. }),
        "{outcome:?}"
    );

    let base = library.path().join("Radiohead/OK Computer (1997)");
    assert!(
        base.join("01 - Airbag.flac").exists(),
        "{:?}",
        std::fs::read_dir(library.path())
    );
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
}

#[tokio::test]
async fn a_download_tagged_as_another_album_is_not_imported() {
    let Shelf { db, album } = shelf().await;
    let download = tempfile::tempdir().unwrap();
    let dl = download
        .path()
        .join("Radiohead - OK Computer (1997) [FLAC]");
    std::fs::create_dir_all(&dl).unwrap();
    write_track(
        &dl,
        "01.flac",
        "Kid A",
        1,
        1,
        "Everything In Its Right Place",
    );
    write_track(&dl, "02.flac", "Kid A", 1, 2, "Kid A");

    let library = tempfile::tempdir().unwrap();
    let cfg = config(library.path().to_path_buf());
    let registry = registry(&db);
    let indexer = Indexer(vec![release("Radiohead - OK Computer (1997) [FLAC]")]);
    let client = Client(dl.to_string_lossy().into_owned());
    let clock = LogicalClock::default();
    let runner = PipelineRunner::new(&indexer, &client, &registry, &db, &clock, &cfg);

    let outcome = runner.run(&album).await.unwrap();
    assert!(
        !matches!(outcome, RunOutcome::Imported { .. }),
        "{outcome:?}"
    );
    assert!(db
        .music()
        .track_files_for_album(album.id)
        .await
        .unwrap()
        .is_empty());
    assert!(
        !library.path().join("Radiohead").exists(),
        "nothing was written to the library"
    );
}

mod catalogue {
    use super::*;
    use cellarr_jobs::music_sync::{AddArtist, MonitorMode, MusicCatalogue, MusicSync};
    use cellarr_meta::{MusicBrainzSource, RecordedFetcher};

    const FIX: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../cellarr-meta/tests/fixtures"
    );
    const BASE: &str = "https://mb.test/ws/2";
    const RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";

    fn fixture(n: &str) -> Vec<u8> {
        std::fs::read(format!("{FIX}/{n}")).unwrap()
    }

    fn sync(db: &Database) -> MusicSync<RecordedFetcher> {
        let fetcher = RecordedFetcher::new()
            .with_body(
                &format!("{BASE}/artist/{RADIOHEAD}"),
                fixture("mb_artist_radiohead.json"),
            )
            .with_body(
                &format!("{BASE}/release-group?artist="),
                fixture("mb_release_groups_radiohead.json"),
            )
            .with_body(
                &format!("{BASE}/release?release-group="),
                fixture("mb_releases_ok_computer.json"),
            );
        MusicSync::new(db.clone(), MusicBrainzSource::with_base_url(fetcher, BASE))
    }

    async fn empty_db() -> (Database, LibraryId) {
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
        (db, lib)
    }

    fn req(lib: LibraryId, monitor: MonitorMode) -> AddArtist {
        AddArtist {
            library: lib,
            mbid: RADIOHEAD.into(),
            path: "/music/Radiohead".into(),
            quality_profile_id: None,
            metadata_profile_id: 1,
            monitored: true,
            monitor,
            monitor_new_albums: true,
        }
    }

    #[tokio::test]
    async fn adding_an_artist_keeps_only_the_albums_its_profile_allows() {
        let (db, lib) = empty_db().await;
        let artist = sync(&db)
            .add_artist(req(lib, MonitorMode::All))
            .await
            .unwrap();
        assert_eq!(artist.name, "Radiohead");
        let titles: Vec<String> = db
            .music()
            .albums_for_artist(artist.content_id)
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.title)
            .collect();
        assert!(titles.contains(&"OK Computer".to_string()));
        assert!(titles.contains(&"Kid A".to_string()));
        assert!(
            !titles.contains(&"Man of War".to_string()),
            "singles are not kept"
        );
        assert!(
            !titles.iter().any(|t| t.contains("Astoria")),
            "live albums are not kept"
        );
    }

    #[tokio::test]
    async fn adding_the_same_artist_twice_is_one_artist() {
        let (db, lib) = empty_db().await;
        let a = sync(&db)
            .add_artist(req(lib, MonitorMode::All))
            .await
            .unwrap();
        let b = sync(&db)
            .add_artist(req(lib, MonitorMode::All))
            .await
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(db.music().artists().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn monitoring_the_latest_album_monitors_exactly_one() {
        let (db, lib) = empty_db().await;
        let a = sync(&db)
            .add_artist(req(lib, MonitorMode::LatestAlbum))
            .await
            .unwrap();
        let monitored: Vec<String> = db
            .music()
            .albums_for_artist(a.content_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|al| al.monitored)
            .map(|al| al.title)
            .collect();
        assert_eq!(monitored, ["A Moon Shaped Pool"]);
    }

    #[tokio::test]
    async fn a_refresh_finds_nothing_new_when_nothing_changed_and_keeps_user_choices() {
        let (db, lib) = empty_db().await;
        let s = sync(&db);
        let a = s.add_artist(req(lib, MonitorMode::All)).await.unwrap();
        let ok = db
            .music()
            .album_by_mbid("b1392450-e666-3926-a536-22c65f834433")
            .await
            .unwrap()
            .unwrap();
        db.music()
            .set_monitored(vec![ok.content_id], false)
            .await
            .unwrap();
        let report = s.refresh_artist(a.content_id).await.unwrap();
        assert!(report.new_albums.is_empty());
        assert!(
            !db.music()
                .album_by_content(ok.content_id)
                .await
                .unwrap()
                .unwrap()
                .monitored
        );
    }

    #[tokio::test]
    async fn an_albums_tracks_are_fetched_once_from_the_representative_pressing() {
        let (db, lib) = empty_db().await;
        let s = sync(&db);
        s.add_artist(req(lib, MonitorMode::All)).await.unwrap();
        let ok = db
            .music()
            .album_by_mbid("b1392450-e666-3926-a536-22c65f834433")
            .await
            .unwrap()
            .unwrap();
        let tracks = s.ensure_tracks(ok.content_id).await.unwrap();
        assert_eq!(tracks.len(), 12);
        assert_eq!(tracks[0].title, "Airbag");
        let again = s.ensure_tracks(ok.content_id).await.unwrap();
        assert_eq!(
            again.iter().map(|t| t.id).collect::<Vec<_>>(),
            tracks.iter().map(|t| t.id).collect::<Vec<_>>()
        );
    }
}

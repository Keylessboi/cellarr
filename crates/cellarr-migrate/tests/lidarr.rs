//! Importing a Lidarr database. The source is built here, table by table, with
//! the column names and embedded-JSON shapes Lidarr persists, so the test pins
//! what the importer reads rather than a recorded file that might drift.

use cellarr_core::repo::ProfileRepository;
use cellarr_db::Database;
use cellarr_migrate::{detect_source, import, preview, SourceKind};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

async fn lidarr_db(dir: &std::path::Path) -> String {
    let path = dir.join("lidarr.db");
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let statements = [
        "CREATE TABLE RootFolders (Id INTEGER PRIMARY KEY, Path TEXT, Name TEXT, DefaultMetadataProfileId INTEGER, DefaultQualityProfileId INTEGER, DefaultMonitorOption INTEGER, DefaultNewItemMonitorOption INTEGER, DefaultTags TEXT)",
        "CREATE TABLE Indexers (Id INTEGER PRIMARY KEY, Name TEXT, Implementation TEXT, Settings TEXT, ConfigContract TEXT, EnableRss INTEGER, EnableAutomaticSearch INTEGER, EnableInteractiveSearch INTEGER, Priority INTEGER, Tags TEXT, DownloadClientId INTEGER)",
        "CREATE TABLE DownloadClients (Id INTEGER PRIMARY KEY, Enable INTEGER, Name TEXT, Implementation TEXT, Settings TEXT, ConfigContract TEXT, Priority INTEGER, RemoveCompletedDownloads INTEGER, RemoveFailedDownloads INTEGER, Tags TEXT)",
        "CREATE TABLE RemotePathMappings (Id INTEGER PRIMARY KEY, Host TEXT, RemotePath TEXT, LocalPath TEXT)",
        "CREATE TABLE QualityProfiles (Id INTEGER PRIMARY KEY, Name TEXT, Cutoff INTEGER, Items TEXT, UpgradeAllowed INTEGER, MinFormatScore INTEGER, CutoffFormatScore INTEGER, FormatItems TEXT)",
        "CREATE TABLE MetadataProfiles (Id INTEGER PRIMARY KEY, Name TEXT, PrimaryAlbumTypes TEXT, SecondaryAlbumTypes TEXT, ReleaseStatuses TEXT)",
        "CREATE TABLE ArtistMetadata (Id INTEGER PRIMARY KEY, ForeignArtistId TEXT, Name TEXT, Overview TEXT, Disambiguation TEXT, Type TEXT, Status INTEGER, Images TEXT, Links TEXT, Genres TEXT, Ratings TEXT, Members TEXT, Aliases TEXT, OldForeignArtistIds TEXT)",
        "CREATE TABLE Artists (Id INTEGER PRIMARY KEY, ArtistMetadataId INTEGER, CleanName TEXT, SortName TEXT, Path TEXT, Monitored INTEGER, MonitorNewItems INTEGER, LastInfoSync TEXT, QualityProfileId INTEGER, MetadataProfileId INTEGER, Added TEXT, AddOptions TEXT, Tags TEXT)",
        "CREATE TABLE Albums (Id INTEGER PRIMARY KEY, ArtistMetadataId INTEGER, ForeignAlbumId TEXT, OldForeignAlbumIds TEXT, Title TEXT, Overview TEXT, Disambiguation TEXT, ReleaseDate TEXT, Images TEXT, Links TEXT, Genres TEXT, AlbumType TEXT, SecondaryTypes TEXT, Ratings TEXT, CleanTitle TEXT, ProfileId INTEGER, Monitored INTEGER, AnyReleaseOk INTEGER, LastInfoSync TEXT, Added TEXT, AddOptions TEXT, LastSearchTime TEXT)",
        "CREATE TABLE AlbumReleases (Id INTEGER PRIMARY KEY, AlbumId INTEGER, ForeignReleaseId TEXT, Title TEXT, Status TEXT, Duration INTEGER, Label TEXT, Disambiguation TEXT, Country TEXT, ReleaseDate TEXT, Media TEXT, TrackCount INTEGER, Monitored INTEGER)",
        "CREATE TABLE Tracks (Id INTEGER PRIMARY KEY, ForeignTrackId TEXT, ForeignRecordingId TEXT, AlbumReleaseId INTEGER, ArtistMetadataId INTEGER, TrackNumber TEXT, AbsoluteTrackNumber INTEGER, Title TEXT, Duration INTEGER, Explicit INTEGER, Ratings TEXT, MediumNumber INTEGER, TrackFileId INTEGER)",
        "CREATE TABLE TrackFiles (Id INTEGER PRIMARY KEY, AlbumId INTEGER, Quality TEXT, Size INTEGER, SceneName TEXT, DateAdded TEXT, ReleaseGroup TEXT, MediaInfo TEXT, Modified TEXT, Path TEXT, OriginalFilePath TEXT, IndexerFlags INTEGER)",
        "CREATE TABLE Blocklist (Id INTEGER PRIMARY KEY, ArtistId INTEGER, AlbumIds TEXT, SourceTitle TEXT, Quality TEXT, Date TEXT, PublishedDate TEXT, Size INTEGER, Protocol TEXT, Indexer TEXT, Message TEXT, TorrentInfoHash TEXT, IndexerFlags INTEGER)",
        "CREATE TABLE History (Id INTEGER PRIMARY KEY, AlbumId INTEGER, ArtistId INTEGER, TrackId INTEGER, SourceTitle TEXT, Quality TEXT, Date TEXT, EventType INTEGER, Data TEXT, DownloadId TEXT)",
        // Config
        "INSERT INTO RootFolders VALUES (1, '/data/media/music', 'Music', 1, 4, 2, 1, '[3]')",
        "INSERT INTO RemotePathMappings VALUES (1, 'qbit', '/downloads/', '/data/torrents/')",
        r#"INSERT INTO Indexers VALUES (1, 'Prowlarr TPB', 'Torznab', '{"baseUrl":"http://prowlarr/1","apiKey":"k","categories":[3000]}', 'TorznabSettings', 1, 1, 1, 25, '[]', 0)"#,
        r#"INSERT INTO DownloadClients VALUES (1, 1, 'qBittorrent', 'QBittorrent', '{"host":"qbit","port":8080,"musicCategory":"music"}', 'QBittorrentSettings', 1, 1, 1, '[]')"#,
        r#"INSERT INTO DownloadClients VALUES (2, 1, 'Plugin client', 'PluginProvidedClient', '{"baseUrl":"http://slskd:5030","apiKey":"k"}', 'PluginProvidedClientSettings', 2, 1, 1, '[]')"#,
        // Profiles. Items reference qualities by bare id, as Lidarr stores them.
        r#"INSERT INTO QualityProfiles VALUES (1, 'Lossless', 1001, '[{"id":1001,"name":"Lossless","items":[{"quality":6,"items":[],"allowed":true},{"quality":21,"items":[],"allowed":true}],"allowed":true},{"quality":4,"items":[],"allowed":true},{"quality":1,"items":[],"allowed":false}]', 1, 0, 0, '[]')"#,
        r#"INSERT INTO QualityProfiles VALUES (4, 'Any', 4, '[{"quality":4,"items":[],"allowed":true}]', 0, 0, 0, '[]')"#,
        r#"INSERT INTO MetadataProfiles VALUES (1, 'Standard', '[{"primaryAlbumType":{"id":0,"name":"Album"},"allowed":true},{"primaryAlbumType":{"id":2,"name":"Single"},"allowed":false}]', '[{"secondaryAlbumType":{"id":0,"name":"Studio"},"allowed":true},{"secondaryAlbumType":{"id":6,"name":"Live"},"allowed":false}]', '[{"releaseStatus":{"id":0,"name":"Official"},"allowed":true}]')"#,
        // Library: 2 artists, 3 albums.
        r#"INSERT INTO ArtistMetadata VALUES (10, 'a-radiohead', 'Radiohead', 'English rock band', 'UK rock band', 'Group', 0, '[]', '[]', '["Alternative rock"]', '{}', '[]', '["Radio Head"]', '[]')"#,
        r#"INSERT INTO ArtistMetadata VALUES (11, 'a-burial', 'Burial', '', '', 'Person', 0, '[]', '[]', '[]', '{}', '[]', '[]', '[]')"#,
        "INSERT INTO Artists VALUES (907, 10, 'radiohead', 'Radiohead', '/data/media/music/Radiohead', 1, 0, '2026-01-01 00:00:00', 1, 1, '2025-06-01 12:00:00', '{}', '[]')",
        "INSERT INTO Artists VALUES (908, 11, 'burial', 'Burial', '/data/media/music/Burial', 0, 1, NULL, 4, 1, '2025-06-02 12:00:00', '{}', '[]')",
        r#"INSERT INTO Albums VALUES (30001, 10, 'rg-okc', '[]', 'OK Computer', '', '', '1997-05-21 00:00:00', '[]', '[]', '[]', 'Album', '[]', '{}', 'okcomputer', 0, 1, 1, NULL, '2025-06-01 12:00:00', '{}', NULL)"#,
        r#"INSERT INTO Albums VALUES (30002, 10, 'rg-live', '[]', 'Live Thing', '', '', '2001-01-01 00:00:00', '[]', '[]', '[]', 'Album', '[{"id":6,"name":"Live"}]', '{}', 'livething', 0, 0, 1, NULL, '2025-06-01 12:00:00', '{}', NULL)"#,
        r#"INSERT INTO Albums VALUES (30003, 11, 'rg-untrue', '[]', 'Untrue', '', '', '2007-11-05 00:00:00', '[]', '[]', '[]', 'Album', '[]', '{}', 'untrue', 0, 1, 1, NULL, '2025-06-02 12:00:00', '{}', NULL)"#,
        // Two pressings of OK Computer; the monitored one (2) is the album's.
        "INSERT INTO AlbumReleases VALUES (501, 30001, 'rel-a', 'OK Computer', 'Official', 0, '[]', '', '[]', NULL, '[]', 13, 0)",
        "INSERT INTO AlbumReleases VALUES (502, 30001, 'rel-b', 'OK Computer', 'Official', 0, '[]', '', '[]', NULL, '[]', 2, 1)",
        "INSERT INTO AlbumReleases VALUES (503, 30002, 'rel-live', 'Live Thing', 'Official', 0, '[]', '', '[]', NULL, '[]', 1, 1)",
        "INSERT INTO AlbumReleases VALUES (504, 30003, 'rel-untrue', 'Untrue', 'Official', 0, '[]', '', '[]', NULL, '[]', 2, 1)",
        // Tracks of the selected releases (and one of the unselected, to be ignored).
        "INSERT INTO Tracks VALUES (700001, 'ft1', 'rec1', 502, 10, '1', 1, 'Airbag', 284000, 0, '{}', 1, 91)",
        "INSERT INTO Tracks VALUES (700002, 'ft2', 'rec2', 502, 10, '2', 2, 'Paranoid Android', 387000, 0, '{}', 1, 0)",
        "INSERT INTO Tracks VALUES (700003, 'ftx', 'recx', 501, 10, '1', 1, 'Airbag (other pressing)', 284000, 0, '{}', 1, 0)",
        "INSERT INTO Tracks VALUES (700004, 'ft4', 'rec4', 503, 10, '1', 1, 'Live Opener', 100000, 0, '{}', 1, 0)",
        "INSERT INTO Tracks VALUES (700005, 'ft5', 'rec5', 504, 11, '1', 1, 'Untrue', 200000, 0, '{}', 1, 92)",
        "INSERT INTO Tracks VALUES (700006, 'ft6', 'rec6', 504, 11, 'A2', 2, 'Shell of Light', 200000, 0, '{}', 1, 0)",
        // Files. 93 is an orphan no track points at.
        r#"INSERT INTO TrackFiles VALUES (91, 30001, '{"quality":6,"revision":{"version":1,"real":0,"isRepack":false}}', 31000000, '', '2025-07-01 10:00:00', '', '{"audioFormat":"Flac","audioBitrate":"1000 kbps","audioBits":"16bit","audioSampleRate":"44100"}', '2025-07-01 10:00:00', '/data/media/music/Radiohead/OK Computer (1997)/01 - Airbag.flac', '', 0)"#,
        r#"INSERT INTO TrackFiles VALUES (92, 30003, '{"quality":4,"revision":{"version":1,"real":0,"isRepack":false}}', 11000000, '', '2025-07-02 10:00:00', '', '{}', '2025-07-02 10:00:00', '/data/media/music/Burial/Untrue/01 - Untrue.mp3', '', 0)"#,
        r#"INSERT INTO TrackFiles VALUES (93, 30003, '{"quality":4}', 1, '', '2025-07-02 10:00:00', '', '{}', '2025-07-02 10:00:00', '/data/media/music/Burial/Untrue/orphan.mp3', '', 0)"#,
        r#"INSERT INTO Blocklist VALUES (1, 907, '[30001]', 'Radiohead - OK Computer [DEAD]', '{}', '2025-08-01 00:00:00', NULL, 1, 'TorrentDownloadProtocol', 'TPB', 'dead torrent', 'abc', 0)"#,
        "INSERT INTO History VALUES (1, 30001, 907, 0, 'x', '{}', '2025-08-01 00:00:00', 1, '{}', 'd')",
    ];
    for sql in statements {
        sqlx::query(sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    pool.close().await;
    path.to_str().unwrap().to_string()
}

async fn dest(dir: &std::path::Path) -> Database {
    Database::open(dir.join("cellarr.sqlite").to_str().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_lidarr_database_is_recognized() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    assert_eq!(detect_source(&src).await.unwrap(), SourceKind::Lidarr);
}

#[tokio::test]
async fn the_preview_counts_what_would_be_imported() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let p = preview(&[&src]).await.unwrap();
    let m = p.music.unwrap();
    assert_eq!((m.artists, m.albums, m.track_files), (2, 3, 3));
    assert_eq!(m.tracks, 5, "only each album's selected release counts");
    assert_eq!((m.quality_profiles, m.metadata_profiles), (2, 1));
    assert_eq!(p.indexer_count, 1);
    assert_eq!(p.scheduled_file_operations, 0);
}

#[tokio::test]
async fn the_library_is_imported_with_lidarrs_own_ids() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    let report = import(&[&src], &db).await.unwrap();
    let m = report.music.unwrap();
    assert_eq!((m.artists, m.albums, m.tracks, m.track_files), (2, 3, 5, 2));
    assert_eq!(m.unmapped_files, 1);

    let music = db.music();
    let radiohead = music
        .artist(907)
        .await
        .unwrap()
        .expect("artist keeps its id");
    assert_eq!(radiohead.name, "Radiohead");
    assert_eq!(radiohead.mbid, "a-radiohead");
    assert_eq!(radiohead.path, "/data/media/music/Radiohead");
    assert!(radiohead.monitored && radiohead.monitor_new_albums);
    assert_eq!(radiohead.genres, ["Alternative rock"]);
    let burial = music.artist(908).await.unwrap().unwrap();
    assert!(!burial.monitored, "an unmonitored artist stays unmonitored");
    assert!(
        !burial.monitor_new_albums,
        "MonitorNewItems=None carries over"
    );

    let okc = music
        .album(30001)
        .await
        .unwrap()
        .expect("album keeps its id");
    assert_eq!(okc.title, "OK Computer");
    assert_eq!(okc.release_date.as_deref(), Some("1997-05-21"));
    assert_eq!(
        okc.release_mbid.as_deref(),
        Some("rel-b"),
        "the monitored pressing"
    );
    assert!(okc.monitored);
    let live = music.album(30002).await.unwrap().unwrap();
    assert_eq!(live.secondary_types, ["Live"]);
    assert!(!live.monitored);

    let tracks = music.tracks_for_album(okc.content_id).await.unwrap();
    assert_eq!(
        tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["Airbag", "Paranoid Android"]
    );
    assert_eq!(tracks[0].id, 700001, "tracks keep their ids");
    assert_eq!(tracks[0].duration_ms, Some(284_000));
    let untrue = music
        .tracks_for_album(music.album(30003).await.unwrap().unwrap().content_id)
        .await
        .unwrap();
    assert_eq!(
        untrue[1].position, 2,
        "a vinyl-style `A2` is position 2 on its disc"
    );
}

#[tokio::test]
async fn files_are_recognized_in_place_with_their_quality() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();

    let music = db.music();
    let flac = music
        .track_file(91)
        .await
        .unwrap()
        .expect("file keeps its id");
    assert_eq!(
        flac.file.path,
        "/data/media/music/Radiohead/OK Computer (1997)/01 - Airbag.flac"
    );
    assert_eq!(flac.file.quality.name, "FLAC");
    assert_eq!(flac.file.size, 31_000_000);
    assert_eq!(flac.added_at, "2025-07-01T10:00:00Z");
    let info = flac.file.media_info.unwrap();
    assert_eq!(info["bitrateKbps"], 1000);
    assert_eq!(info["bitDepth"], 16);
    let mp3 = music.track_file(92).await.unwrap().unwrap();
    assert_eq!(mp3.file.quality.name, "MP3-320");
    assert!(
        music.track_file(93).await.unwrap().is_none(),
        "an orphan file is left out"
    );

    let stats = music.artist_stats().await.unwrap();
    let radiohead = music.artist(907).await.unwrap().unwrap();
    assert_eq!(stats[&radiohead.content_id].track_file_count, 1);
}

#[tokio::test]
async fn profiles_keep_their_integer_ids_and_meaning() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();

    let music = db.music();
    let lossless = music
        .profile_by_seq(1)
        .await
        .unwrap()
        .expect("profile id 1");
    let any = music
        .profile_by_seq(4)
        .await
        .unwrap()
        .expect("profile id 4");
    let profiles = db.profiles().list_profiles().await.unwrap();
    let p = profiles.iter().find(|p| p.id == lossless).unwrap();
    assert_eq!(p.name, "Lossless");
    let ranking = cellarr_core::QualityRanking::default();
    let names: Vec<String> = p
        .allowed_qualities
        .iter()
        .map(|r| ranking.definition_for_rank(*r).unwrap().name.clone())
        .collect();
    assert!(names.contains(&"FLAC".to_string()) && names.contains(&"FLAC 24bit".to_string()));
    assert!(names.contains(&"MP3-320".to_string()));
    assert!(
        !names.contains(&"MP3-192".to_string()),
        "a disallowed quality stays out"
    );
    // The cutoff was set on the "Lossless" group; it resolves to that group's
    // lowest member.
    assert_eq!(
        ranking.definition_for_rank(p.cutoff_quality).unwrap().name,
        "FLAC"
    );
    let _ = any;

    // Each artist is graded by the profile it had.
    let burial = music.artist(908).await.unwrap().unwrap();
    assert_eq!(burial.quality_profile_id, Some(any));
    let mp = music.metadata_profile(1).await.unwrap().unwrap();
    assert_eq!(mp.primary_types, ["Album"]);
    assert_eq!(mp.secondary_types, ["Studio"]);
}

#[tokio::test]
async fn config_blocklist_and_mappings_come_across() {
    use cellarr_core::BlocklistRepository;
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    let report = import(&[&src], &db).await.unwrap();
    let m = report.music.unwrap();
    assert_eq!(
        (
            m.blocklist,
            m.remote_path_mappings,
            m.history,
            m.history_skipped
        ),
        (1, 1, 1, 0)
    );

    let entries = BlocklistRepository::list(&db.blocklist()).await.unwrap();
    assert_eq!(entries[0].title, "Radiohead - OK Computer [DEAD]");
    assert_eq!(entries[0].protocol.as_deref(), Some("torrent"));
    let okc = db.music().album(30001).await.unwrap().unwrap();
    assert_eq!(entries[0].content_id, okc.content_id);

    // A release the indexers return under that name is refused for that album.
    let release = cellarr_core::Release {
        indexer_id: cellarr_core::IndexerId::new(),
        title: "Radiohead - OK Computer [DEAD]".into(),
        download_url: "magnet:?xt=urn:btih:zzz".into(),
        guid: Some("some-guid".into()),
        protocol: cellarr_core::Protocol::Torrent,
        size: Some(1),
        seeders: Some(1),
        indexer_flags: vec![],
    };
    assert!(
        BlocklistRepository::is_blocklisted(&db.blocklist(), okc.content_id, &release)
            .await
            .unwrap()
    );

    assert_eq!(
        db.config().list_root_folders().await.unwrap()[0].path,
        "/data/media/music"
    );
    assert_eq!(db.config().list_indexers().await.unwrap().len(), 1);
    assert_eq!(db.config().list_download_clients().await.unwrap().len(), 2);
    assert_eq!(
        db.config().list_remote_path_mappings().await.unwrap()[0].local_path,
        "/data/torrents/"
    );
}

#[tokio::test]
async fn the_source_database_is_never_modified() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let before = std::fs::read(&src).unwrap();
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), before);
}

#[tokio::test]
async fn every_pressing_keeps_its_lidarr_id_and_the_pin_survives() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();
    let music = db.music();
    let album = music.album(30001).await.unwrap().unwrap();
    assert_eq!(
        album.release_mbid.as_deref(),
        Some("rel-b"),
        "the monitored pressing stays pinned"
    );
    let pressings = music
        .releases_for_albums(&[album.content_id])
        .await
        .unwrap()
        .remove(&album.content_id)
        .unwrap();
    let ids: Vec<(i64, &str, u32)> = pressings
        .iter()
        .map(|r| (r.id, r.mbid.as_str(), r.track_count))
        .collect();
    assert_eq!(ids, [(501, "rel-a", 13), (502, "rel-b", 2)]);
}

#[tokio::test]
async fn an_album_with_no_monitored_release_is_imported_unwanted() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    // Radiohead gets a monitored album whose only release is unmonitored.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::from_str(&format!("sqlite://{src}")).unwrap())
        .await
        .unwrap();
    for sql in [
        r#"INSERT INTO Albums VALUES (30004, 10, 'rg-chlorine', '[]', 'Chlorine', '', '', '2020-01-01 00:00:00', '[]', '[]', '[]', 'Album', '[]', '{}', 'chlorine', 0, 1, 1, NULL, '2025-06-01 12:00:00', '{}', NULL)"#,
        "INSERT INTO AlbumReleases VALUES (505, 30004, 'rel-chlorine', 'Chlorine', 'Official', 0, '[]', '', '[]', NULL, '[]', 2, 0)",
        "INSERT INTO Tracks VALUES (700007, 'ft7', 'rec7', 505, 10, '1', 1, 'One', 100000, 0, '{}', 1, 0)",
        "INSERT INTO Tracks VALUES (700008, 'ft8', 'rec8', 505, 10, '2', 2, 'Two', 100000, 0, '{}', 1, 0)",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    pool.close().await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();
    let music = db.music();
    let chlorine = music.album(30004).await.unwrap().unwrap();
    assert!(!chlorine.release_monitored);
    assert!(music.album(30001).await.unwrap().unwrap().release_monitored);
    let (_, wanted) = music
        .wanted_albums("2026-01-01", cellarr_db::AlbumSort::Id, true, 0, 50)
        .await
        .unwrap();
    assert!(
        !wanted.iter().any(|a| a.id == 30004),
        "Lidarr does not want it"
    );
}

#[tokio::test]
async fn download_clients_keep_their_own_category_and_plugin_implementations_carry_over() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();
    let clients = db.config().list_download_clients().await.unwrap();
    let qbit = clients.iter().find(|c| c.name == "qBittorrent").unwrap();
    assert_eq!(qbit.kind, "qbittorrent");
    assert_eq!(
        qbit.category, "music",
        "the client's own category, not the app's name"
    );
    // A plugin's client keeps its implementation, so the plugin sidecar (which
    // offers the same implementation) serves it once it is running.
    let plugin = clients.iter().find(|c| c.name == "Plugin client").unwrap();
    assert_eq!(plugin.kind, "pluginprovidedclient");
    assert_eq!(plugin.settings["baseUrl"], "http://slskd:5030");
}

#[tokio::test]
async fn history_is_carried_over_row_for_row() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();
    let (total, rows) = db
        .music()
        .music_history(&cellarr_db::MusicHistoryFilter::default(), false, 0, 10)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert!(rows[0].id.is_some(), "Lidarr's id is kept");
}

#[tokio::test]
async fn a_root_folders_defaults_are_imported_with_the_profile_id_kept() {
    let dir = tempfile::tempdir().unwrap();
    let src = lidarr_db(dir.path()).await;
    let db = dest(dir.path()).await;
    import(&[&src], &db).await.unwrap();

    let defaults = db.root_folder_defaults().list().await.unwrap();
    assert_eq!(defaults.len(), 1);
    let d = &defaults[0];
    assert_eq!(d.path, "/data/media/music");
    assert_eq!(d.name, "Music");
    assert_eq!(d.metadata_profile_id, 1);
    assert_eq!(d.monitor_option, "missing");
    assert_eq!(d.new_item_monitor_option, "none");
    assert_eq!(d.tags, vec![3]);
    // Lidarr's quality profile 4 is still 4 in cellarr.
    let profile = d.quality_profile.expect("profile resolved");
    assert_eq!(db.music().profile_seq(profile).await.unwrap(), 4);
}

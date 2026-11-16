//! Integration tests for the music repository against a real migrated database.

use cellarr_core::profile::Quality;
use cellarr_core::repo::ContentRepository;
use cellarr_core::{
    music::seq_of_uuid, ContentId, Coordinates, Library, LibraryId, MediaFile, MediaFileId,
    MediaType, MetadataProfile, QualityProfileId,
};
use cellarr_db::{AlbumSort, Database, NewAlbum, NewArtist, NewTrack};

mod common;

async fn music_library(db: &Database) -> LibraryId {
    let id = LibraryId::new();
    db.config()
        .upsert_library(&Library {
            id,
            media_type: MediaType::Music,
            name: "music".into(),
            root_folders: vec!["/data/media/music".into()],
            default_quality_profile: QualityProfileId::new(),
        })
        .await
        .unwrap();
    id
}

fn artist(name: &str, mbid: &str) -> NewArtist {
    NewArtist {
        mbid: mbid.into(),
        name: name.into(),
        path: format!("/data/media/music/{name}"),
        metadata_profile_id: 1,
        monitored: true,
        monitor_new_albums: true,
        ..NewArtist::default()
    }
}

fn album(title: &str, mbid: &str, date: &str) -> NewAlbum {
    NewAlbum {
        mbid: mbid.into(),
        title: title.into(),
        album_type: "Album".into(),
        release_date: Some(date.into()),
        monitored: true,
        ..NewAlbum::default()
    }
}

fn track(disc: u32, position: u32, title: &str) -> NewTrack {
    NewTrack {
        title: title.into(),
        disc,
        position,
        duration_ms: Some(200_000),
        ..NewTrack::default()
    }
}

fn file(path: &str, size: u64) -> MediaFile {
    MediaFile {
        id: MediaFileId::new(),
        path: path.into(),
        size,
        quality: Quality::new("FLAC", 131),
        languages: Vec::new(),
        media_info: None,
        custom_format_score: None,
        release_type: None,
    }
}

#[tokio::test]
async fn ids_are_sequential_and_the_content_id_carries_the_same_number() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    let b = db
        .music()
        .add_artist(lib, artist("Burial", "mb-a2"))
        .await
        .unwrap();
    assert_eq!((a.id, b.id), (1, 2));
    assert_eq!(seq_of_uuid(a.content_id.as_uuid()), 1);
    assert_eq!(seq_of_uuid(b.content_id.as_uuid()), 2);

    let albums = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![album("OK Computer", "mb-rg1", "1997-05-21")],
        )
        .await
        .unwrap();
    assert_eq!(albums[0].id, 1, "albums have their own sequence");
    assert_eq!(seq_of_uuid(albums[0].content_id.as_uuid()), 1);
}

#[tokio::test]
async fn a_preset_id_is_kept_and_later_ids_continue_above_it() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let mut migrated = artist("Radiohead", "mb-a1");
    migrated.id = Some(907);
    let a = db.music().add_artist(lib, migrated).await.unwrap();
    assert_eq!(a.id, 907);
    let b = db
        .music()
        .add_artist(lib, artist("Burial", "mb-a2"))
        .await
        .unwrap();
    assert_eq!(b.id, 908);
    assert_eq!(
        db.music().artist(907).await.unwrap().unwrap().name,
        "Radiohead"
    );
}

#[tokio::test]
async fn an_artist_is_added_once_per_musicbrainz_id() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    db.music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    assert!(db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .is_err());
    assert_eq!(db.music().artists().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_refresh_updates_metadata_but_never_the_monitored_flag() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    let first = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![album("OK Computr", "mb-rg1", "1997")],
        )
        .await
        .unwrap();
    db.music()
        .set_monitored(vec![first[0].content_id], false)
        .await
        .unwrap();

    let again = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![
                album("OK Computer", "mb-rg1", "1997-05-21"),
                album("Kid A", "mb-rg2", "2000-10-02"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(again.len(), 1, "only the new album is reported as inserted");
    assert_eq!(again[0].title, "Kid A");

    let stored = db.music().album_by_mbid("mb-rg1").await.unwrap().unwrap();
    assert_eq!(stored.title, "OK Computer");
    assert_eq!(stored.release_date.as_deref(), Some("1997-05-21"));
    assert!(
        !stored.monitored,
        "the user's unmonitor survives the refresh"
    );
    assert_eq!(stored.id, first[0].id, "the album keeps its id");
}

#[tokio::test]
async fn albums_are_found_by_normalized_artist_and_title_or_alias() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let mut sr = artist("Sigur Rós", "mb-a1");
    sr.aliases = vec!["Sigur Ros".into()];
    let a = db.music().add_artist(lib, sr).await.unwrap();
    db.music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![
                album("( )", "mb-rg0", "2002"),
                album("Ágætis byrjun", "mb-rg1", "1999"),
            ],
        )
        .await
        .unwrap();

    let by_name = db
        .music()
        .albums_named("sigur rós", &["ágætis byrjun".to_string()])
        .await
        .unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].artist_name, "Sigur Rós");

    let by_alias = db
        .music()
        .albums_named("sigur ros", &["ágætis byrjun".to_string()])
        .await
        .unwrap();
    assert_eq!(by_alias.len(), 1);

    assert!(db
        .music()
        .albums_named("somebody else", &["ágætis byrjun".to_string()])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_title_naming_artist_and_album_without_a_separator_is_matched_jointly() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Boards of Canada", "mb-a1"))
        .await
        .unwrap();
    db.music()
        .upsert_albums(lib, a.content_id, vec![album("Geogaddi", "mb-rg1", "2002")])
        .await
        .unwrap();
    let hits = db
        .music()
        .albums_named_jointly("boards of canada geogaddi")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].album.title, "Geogaddi");
}

#[tokio::test]
async fn refetching_tracks_keeps_ids_and_never_orphans_a_file() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    let al = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![album("OK Computer", "mb-rg1", "1997")],
        )
        .await
        .unwrap()
        .remove(0);

    let first = db
        .music()
        .set_album_tracks(
            lib,
            al.content_id,
            Some("mb-rel1".into()),
            vec![
                track(1, 1, "Airbag"),
                track(1, 2, "Paranoid Android"),
                track(1, 3, "Bonus"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(first.len(), 3);
    db.music()
        .record_track_file(first[2].content_id, file("/m/03.flac", 10), None, None)
        .await
        .unwrap();

    // A different pressing: track 2 retitled, track 3 gone.
    let second = db
        .music()
        .set_album_tracks(
            lib,
            al.content_id,
            Some("mb-rel2".into()),
            vec![
                track(1, 1, "Airbag"),
                track(1, 2, "Paranoid Android (Remaster)"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(second[0].id, first[0].id, "a kept track keeps its id");
    assert_eq!(second[1].title, "Paranoid Android (Remaster)");
    assert_eq!(
        second.len(),
        3,
        "the dropped track survives because it has a file"
    );

    let stored = db
        .music()
        .album_by_content(al.content_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.release_mbid.as_deref(), Some("mb-rel2"));
    assert!(stored.tracks_fetched_at.is_some());
}

#[tokio::test]
async fn an_album_is_missing_until_a_track_file_lands_and_the_sweep_agrees() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    let albums = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![
                album("OK Computer", "mb-rg1", "1997-05-21"),
                album("Kid A", "mb-rg2", "2000-10-02"),
                album("Not Out Yet", "mb-rg3", "2999-01-01"),
            ],
        )
        .await
        .unwrap();
    let tracks = db
        .music()
        .set_album_tracks(lib, albums[0].content_id, None, vec![track(1, 1, "Airbag")])
        .await
        .unwrap();

    let sweep = |db: Database| async move {
        let mut ids: Vec<ContentId> = db
            .content()
            .monitored_missing()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        ids.sort();
        ids
    };
    let mut both = vec![albums[0].content_id, albums[1].content_id];
    both.sort();
    assert_eq!(
        sweep(db.clone()).await,
        both,
        "released albums are units; the unreleased album and the track are not"
    );
    let refs = db.content().monitored_missing().await.unwrap();
    assert!(refs.iter().all(|r| r.coords == Coordinates::Album));

    let (total, page) = db
        .music()
        .missing_albums("2026-01-01", AlbumSort::Id, true, 0, 10)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(page.iter().map(|a| a.id).collect::<Vec<_>>(), vec![1, 2]);

    let tf = db
        .music()
        .record_track_file(tracks[0].content_id, file("/m/01.flac", 1234), None, None)
        .await
        .unwrap();
    assert_eq!(tf, 1);
    assert_eq!(sweep(db.clone()).await, vec![albums[1].content_id]);
    let (total, _) = db
        .music()
        .missing_albums("2026-01-01", AlbumSort::Id, true, 0, 10)
        .await
        .unwrap();
    assert_eq!(total, 1);

    // Unmonitoring the artist silences what is left.
    db.music()
        .set_monitored(vec![a.content_id], false)
        .await
        .unwrap();
    assert!(sweep(db.clone()).await.is_empty());
}

#[tokio::test]
async fn statistics_count_albums_tracks_files_and_bytes() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    let albums = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![
                album("OK Computer", "mb-rg1", "1997"),
                album("Kid A", "mb-rg2", "2000"),
            ],
        )
        .await
        .unwrap();
    let tracks = db
        .music()
        .set_album_tracks(
            lib,
            albums[0].content_id,
            None,
            vec![track(1, 1, "Airbag"), track(1, 2, "Paranoid Android")],
        )
        .await
        .unwrap();
    db.music()
        .record_track_file(tracks[0].content_id, file("/m/01.flac", 1000), None, None)
        .await
        .unwrap();

    let artist_stats = db.music().artist_stats().await.unwrap();
    let s = artist_stats[&a.content_id];
    assert_eq!(
        (
            s.album_count,
            s.track_count,
            s.track_file_count,
            s.size_on_disk
        ),
        (2, 2, 1, 1000)
    );

    let album_stats = db.music().album_stats(Some(a.content_id)).await.unwrap();
    let s = album_stats[&albums[0].content_id];
    assert_eq!(
        (s.track_count, s.track_file_count, s.size_on_disk),
        (2, 1, 1000)
    );
    assert!(!album_stats.contains_key(&albums[1].content_id));

    let files = db
        .music()
        .track_files_for_album(albums[0].content_id)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file.quality.name, "FLAC");
    assert_eq!(files[0].artist_content_id, a.content_id);
}

#[tokio::test]
async fn deleting_an_artist_removes_the_subtree_and_reports_its_files() {
    let db = common::test_database().await;
    let lib = music_library(&db).await;
    let a = db
        .music()
        .add_artist(lib, artist("Radiohead", "mb-a1"))
        .await
        .unwrap();
    let al = db
        .music()
        .upsert_albums(
            lib,
            a.content_id,
            vec![album("OK Computer", "mb-rg1", "1997")],
        )
        .await
        .unwrap()
        .remove(0);
    let tracks = db
        .music()
        .set_album_tracks(lib, al.content_id, None, vec![track(1, 1, "Airbag")])
        .await
        .unwrap();
    db.music()
        .record_track_file(tracks[0].content_id, file("/m/01.flac", 1), None, None)
        .await
        .unwrap();

    let paths = db.music().delete_artist(a.content_id).await.unwrap();
    assert_eq!(paths, vec!["/m/01.flac".to_string()]);
    assert!(db.music().artists().await.unwrap().is_empty());
    assert!(db.music().albums().await.unwrap().is_empty());
    assert!(db.music().track(tracks[0].id).await.unwrap().is_none());
    assert!(db.music().track_file(1).await.unwrap().is_none());
    assert!(db.content().search("Airbag").await.unwrap().is_empty());
}

#[tokio::test]
async fn the_standard_metadata_profile_exists_before_any_is_stored() {
    let db = common::test_database().await;
    let profiles = db.music().metadata_profiles().await.unwrap();
    assert_eq!(profiles, vec![MetadataProfile::standard()]);

    let mut everything = MetadataProfile::standard();
    everything.id = 0;
    everything.name = "Everything".into();
    everything.primary_types = vec!["Album".into(), "EP".into(), "Single".into()];
    let stored = db
        .music()
        .upsert_metadata_profile(everything)
        .await
        .unwrap();
    assert_eq!(
        stored.id, 2,
        "a new profile never takes the standard profile's id"
    );
    assert!(db.music().metadata_profile(2).await.unwrap().is_some());
}

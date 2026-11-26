//! A download client that cannot be built (an unsupported plugin kind such as
//! `applemusicarrdownloadclient`) must not stop a library rescan: the rescan never
//! grabs, so it resolves its environment with an unavailable stand-in client. The
//! acquisition path keeps reporting the build error.

use cellarr_cli::pipeline::{LivePipelineEnv, PipelineEnv};
use cellarr_core::{
    ContentId, ContentRef, Coordinates, DownloadClientConfig, DownloadClientId, Library, LibraryId,
    MediaType, Protocol, QualityProfile, QualityProfileId, QualityRanking,
};
use cellarr_db::Database;

async fn seed(db: &Database) -> ContentRef {
    let ranking = QualityRanking::default();
    let profile = QualityProfile {
        id: QualityProfileId::new(),
        name: "p".into(),
        allowed_qualities: ranking.qualities.iter().map(|q| q.rank).collect(),
        upgrades_allowed: true,
        cutoff_quality: 14,
        min_custom_format_score: 0,
        upgrade_until_custom_format_score: 100,
        required_languages: Vec::new(),
    };
    db.profiles().upsert_profile(&profile).await.unwrap();
    let library_id = LibraryId::new();
    db.config()
        .upsert_library(&Library {
            id: library_id,
            media_type: MediaType::Movie,
            name: "Movies".into(),
            root_folders: vec!["/tmp/synthetic-library".into()],
            default_quality_profile: profile.id,
        })
        .await
        .unwrap();
    db.config()
        .upsert_download_client(&DownloadClientConfig {
            tags: Vec::new(),
            id: DownloadClientId::new(),
            name: "Apple Music".into(),
            kind: "applemusicarrdownloadclient".into(),
            protocol: Protocol::Usenet,
            enabled: true,
            priority: 1,
            category: "cellarr".into(),
            settings: serde_json::json!({}),
        })
        .await
        .unwrap();
    ContentRef::new(
        ContentId::new(),
        library_id,
        MediaType::Movie,
        Coordinates::Movie,
    )
    .unwrap()
}

#[tokio::test]
async fn an_unbuildable_client_does_not_stop_the_rescan_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::open(tmp.path().join("c.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let probe = seed(&db).await;
    let env = LivePipelineEnv::new(db);

    let err = env
        .resolve(&probe)
        .await
        .err()
        .expect("acquisition still errors");
    assert!(err.contains("unsupported download client kind"), "{err}");

    let resolved = env.resolve_for_rescan(&probe).await.unwrap();
    assert!(
        resolved.is_some(),
        "the rescan environment resolves without a client"
    );
}

//! cellarr's Lidarr face, checked against Lidarr's own API contract.
//!
//! `tests/contracts/lidarr-v1.json` is generated from Lidarr's OpenAPI document by
//! `scripts/lidarr-contract`. This test holds the face to it in two ways:
//!
//! - **Paths.** Every path and method Lidarr serves must be served by cellarr, or be
//!   listed in `lidarr-gaps.json` with the reason it is not. A path that is
//!   neither fails the test, and so does a gap that is no longer one.
//! - **Fields.** For every resource clients read, each field Lidarr's schema names must
//!   be present in cellarr's response with a compatible JSON type, or be listed as a gap.
//!
//! When Lidarr releases, regenerate the contract and run this test: the failures are the
//! list of what changed. See `docs/19-lidarr-compat.md`.

mod lidarr_common;

use std::collections::BTreeMap;

use cellarr_api::AuthConfig;
use cellarr_core::repo::{GrabRepository, HistoryRepository};
use lidarr_common::{start, Lidarr, OK_COMPUTER};
use serde_json::{json, Value};

fn contract() -> Value {
    let path = format!(
        "{}/tests/contracts/lidarr-v1.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("the contract is committed"))
        .expect("the contract parses")
}

fn gaps() -> Value {
    let path = format!(
        "{}/tests/contracts/lidarr-gaps.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("the gaps file is committed"))
        .expect("the gaps file parses")
}

fn gap_set(section: &str) -> BTreeMap<String, String> {
    gaps()[section]
        .as_object()
        .unwrap_or_else(|| panic!("lidarr-gaps.json has no `{section}` object"))
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str().expect("a gap's reason is a string").to_string(),
            )
        })
        .collect()
}

/// Whether the face answers `method path` at all (as opposed to "no such endpoint").
async fn served(l: &Lidarr, method: &str, path: &str) -> bool {
    let concrete: String = path
        .split('/')
        .map(|seg| if seg.starts_with('{') { "1" } else { seg })
        .collect::<Vec<_>>()
        .join("/");
    let url = format!("{}{concrete}", l.base);
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut req = l.client.request(method.clone(), url);
    if path.starts_with("/api/") {
        req = req.header("X-Api-Key", lidarr_common::KEY);
    }
    if matches!(method.as_str(), "POST" | "PUT" | "DELETE") {
        req = req.json(&json!({}));
    }
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    if status == 405 {
        return false;
    }
    if status == 404 {
        let body = r.text().await.unwrap_or_default();
        return !body.contains("unknown api endpoint");
    }
    true
}

#[tokio::test]
async fn every_lidarr_endpoint_is_served_or_a_documented_gap() {
    let l = start(AuthConfig::disabled()).await;
    let contract = contract();
    let gaps = gap_set("paths");
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for (path, methods) in contract["paths"].as_object().unwrap() {
        for method in methods.as_array().unwrap() {
            let method = method.as_str().unwrap();
            let key = format!("{method} {path}");
            seen.insert(key.clone());
            let ok = served(&l, method, path).await;
            match (ok, gaps.contains_key(&key)) {
                (false, false) => unexpected.push(key),
                (true, true) => stale.push(key),
                _ => {}
            }
        }
    }
    let unknown_gaps: Vec<_> = gaps
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();

    assert!(
        unexpected.is_empty(),
        "Lidarr serves these and cellarr does not (implement them, or add them to \
         tests/contracts/lidarr-gaps.json with a reason):\n  {}",
        unexpected.join("\n  ")
    );
    assert!(
        stale.is_empty(),
        "these are listed as gaps but cellarr now serves them; remove them from \
         lidarr-gaps.json:\n  {}",
        stale.join("\n  ")
    );
    assert!(
        unknown_gaps.is_empty(),
        "lidarr-gaps.json names endpoints Lidarr no longer has; remove them:\n  {}",
        unknown_gaps.join("\n  ")
    );
}

/// The JSON types a contract type accepts.
fn type_matches(want: &str, nullable: bool, got: &Value) -> bool {
    match got {
        Value::Null => nullable,
        Value::Bool(_) => want == "boolean",
        Value::Number(n) => match want {
            "integer" => n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0),
            "number" => true,
            _ => false,
        },
        Value::String(_) => want == "string",
        Value::Array(_) => want == "array",
        Value::Object(_) => want == "object",
    }
}

/// Compare one response object to a resource schema; return the problems.
fn check_resource(
    name: &str,
    got: &Value,
    contract: &Value,
    gaps: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(obj) = got.as_object() else {
        return vec![format!("{name}: expected an object, got {got}")];
    };
    for (field, spec) in contract["resources"][name].as_object().unwrap() {
        let key = format!("{name}.{field}");
        let want = spec["type"].as_str().unwrap();
        let nullable = spec["nullable"].as_bool().unwrap_or(false);
        let gap = gaps.contains_key(&key);
        let optional = spec["optional"].as_bool().unwrap_or(false);
        match obj.get(field) {
            None if gap || optional => {}
            None => problems.push(format!("{key}: missing (Lidarr sends a {want})")),
            // A nested resource Lidarr only sends when asked may be null when not.
            Some(Value::Null) if optional => {}
            Some(v) if type_matches(want, nullable, v) => {
                if gap {
                    problems.push(format!("{key}: listed as a gap but present and correct; remove it from lidarr-gaps.json"));
                }
            }
            Some(_) if gap => {}
            Some(v) => problems.push(format!(
                "{key}: Lidarr sends {}{want}, cellarr sent {v}",
                if nullable { "nullable " } else { "" }
            )),
        }
    }
    problems
}

/// A library with every kind of record the resources describe.
async fn seeded() -> Lidarr {
    let l = start(AuthConfig::disabled()).await;
    let profile = l.flac_profile().await;
    l.add_radiohead(profile).await;
    let state = &l.state;
    let album = state
        .db
        .music()
        .album_by_mbid(OK_COMPUTER)
        .await
        .unwrap()
        .unwrap();

    // A track file, so tracks and files have something to show.
    let tracks = l.get(&format!("/track?albumId={}", album.id)).await;
    let first = state
        .db
        .music()
        .track(tracks[0]["id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    state
        .db
        .music()
        .record_track_file(
            first.content_id,
            cellarr_core::MediaFile {
                id: cellarr_core::MediaFileId::new(),
                path: "/data/media/music/Radiohead/OK Computer (1997)/01 - Airbag.flac".into(),
                size: 31_000_000,
                quality: cellarr_core::QualityRanking::default().by_name("FLAC").unwrap(),
                languages: vec![],
                media_info: Some(json!({ "codec": "flac", "bitrateKbps": 1000, "bitDepth": 16, "sampleRate": 44100 })),
                custom_format_score: None,
                release_type: None,
            },
            None,
            None,
        )
        .await
        .unwrap();

    // History, a queue row and a blocklist entry for the same album.
    let album_ref =
        cellarr_core::repo::ContentRepository::get(&state.db.content(), album.content_id)
            .await
            .unwrap()
            .unwrap();
    let release = cellarr_core::Release {
        indexer_id: cellarr_core::IndexerId::new(),
        title: "Radiohead - OK Computer (1997) [FLAC]".into(),
        download_url: "magnet:?xt=urn:btih:abc".into(),
        guid: Some("g1".into()),
        protocol: cellarr_core::Protocol::Torrent,
        size: Some(300_000_000),
        seeders: Some(5),
        indexer_flags: vec![],
    };
    let grab = state
        .db
        .grabs()
        .create(&cellarr_core::GrabRequest {
            content_ref: album_ref.clone(),
            release: release.clone(),
            indexer_id: release.indexer_id,
            client_id: cellarr_core::DownloadClientId::new(),
            category: "lidarr".into(),
            release_type: None,
        })
        .await
        .unwrap();
    state
        .db
        .history()
        .append(&cellarr_core::HistoryRecord {
            at: time::OffsetDateTime::now_utc(),
            content_id: album.content_id,
            run_id: cellarr_core::PipelineRunId::new(),
            event: cellarr_core::HistoryEvent::Grabbed {
                grab_id: grab,
                release_type: None,
            },
        })
        .await
        .unwrap();
    // The Lidarr face reads the Lidarr-shaped music history the runner writes.
    state
        .db
        .music()
        .append_music_history(vec![cellarr_db::MusicHistoryRow {
            id: None,
            album_id: album.id,
            artist_id: 1,
            track_id: 0,
            source_title: release.title.clone(),
            quality: serde_json::json!({"quality": 6, "revision": {"version": 1, "real": 0, "isRepack": false}}),
            date: "2026-11-18T10:00:00Z".into(),
            event_type: 1,
            data: serde_json::json!({"indexer": "Prowlarr", "guid": "g1", "size": 300_000_000}),
            download_id: Some("ABC".into()),
        }])
        .await
        .unwrap();
    cellarr_core::BlocklistRepository::add(
        &state.db.blocklist(),
        &cellarr_core::BlocklistEntry::from_release(
            album.content_id,
            &release,
            "dead",
            time::OffsetDateTime::now_utc(),
        ),
    )
    .await
    .unwrap();
    let _ = l
        .send(
            reqwest::Method::POST,
            "/rootfolder",
            &json!({ "path": "/data/media/music" }),
        )
        .await;
    let _ = l
        .send(reqwest::Method::POST, "/tag", &json!({ "label": "x" }))
        .await;
    l
}

#[tokio::test]
async fn the_resources_clients_read_carry_every_field_lidarr_sends() {
    let l = seeded().await;
    let contract = contract();
    let gaps = gap_set("fields");
    let album_id = l
        .state
        .db
        .music()
        .album_by_mbid(OK_COMPUTER)
        .await
        .unwrap()
        .unwrap()
        .id;

    // (resource, a response that is one, and any nested resources to check as well)
    let artist = l.get("/artist/1").await;
    let album = l.get(&format!("/album/{album_id}")).await;
    let tracks = l.get(&format!("/track?albumId={album_id}")).await;
    let files = l.get(&format!("/trackfile?albumId={album_id}")).await;
    let queue = l.get("/queue?includeAlbum=true&includeArtist=true").await;
    let history = l.get("/history?includeAlbum=true").await;
    let blocklist = l.get("/blocklist").await;
    let command = l
        .send(
            reqwest::Method::POST,
            "/command",
            &json!({ "name": "CheckHealth" }),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();

    let mut checks: Vec<(&str, Value)> = vec![
        ("ArtistResource", artist.clone()),
        ("ArtistStatisticsResource", artist["statistics"].clone()),
        ("AlbumResource", album.clone()),
        ("AlbumStatisticsResource", album["statistics"].clone()),
        ("TrackResource", tracks[0].clone()),
        ("TrackFileResource", files[0].clone()),
        (
            "QualityProfileResource",
            l.get("/qualityprofile").await[0].clone(),
        ),
        (
            "MetadataProfileResource",
            l.get("/metadataprofile").await[0].clone(),
        ),
        ("QueueResource", queue["records"][0].clone()),
        ("HistoryResource", history["records"][0].clone()),
        ("BlocklistResource", blocklist["records"][0].clone()),
        ("CommandResource", command),
        ("SystemResource", l.get("/system/status").await),
        (
            "QualityDefinitionResource",
            l.get("/qualitydefinition").await[0].clone(),
        ),
    ];
    let roots = l.get("/rootfolder").await;
    if let Some(r) = roots.as_array().and_then(|a| a.first()) {
        checks.push(("RootFolderResource", r.clone()));
    }
    if let Some(t) = l.get("/tag").await.as_array().and_then(|a| a.first()) {
        checks.push(("TagResource", t.clone()));
    }

    let mut problems = Vec::new();
    for (name, got) in &checks {
        problems.extend(check_resource(name, got, &contract, &gaps));
    }
    assert!(
        problems.is_empty(),
        "cellarr's Lidarr resources differ from Lidarr's contract (fix the resource, or list \
         the field in tests/contracts/lidarr-gaps.json with a reason):\n  {}",
        problems.join("\n  ")
    );
}

#[tokio::test]
async fn whole_history_answers_walk_every_row_across_pages_in_order() {
    let l = seeded().await;
    let album_id = l
        .state
        .db
        .music()
        .album_by_mbid(OK_COMPUTER)
        .await
        .unwrap()
        .unwrap()
        .id;
    // More than two pages' worth, with runs of rows sharing one timestamp so
    // the cursor has to break ties by id.
    let rows: Vec<_> = (0..4500)
        .map(|i| cellarr_db::MusicHistoryRow {
            id: None,
            album_id,
            artist_id: 7,
            track_id: 0,
            source_title: format!("row {i}"),
            quality: json!({"quality": 6}),
            date: format!("2026-01-01T00:{:02}:00Z", i / 100),
            event_type: if i % 3 == 0 { 3 } else { 1 },
            data: json!({}),
            download_id: None,
        })
        .collect();
    l.state.db.music().append_music_history(rows).await.unwrap();

    let newest_first = l.get("/history/artist?artistId=7").await;
    let titles: Vec<&str> = newest_first
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["sourceTitle"].as_str().unwrap())
        .collect();
    assert_eq!(titles.len(), 4500, "every row, once");
    let unique: std::collections::BTreeSet<_> = titles.iter().collect();
    assert_eq!(unique.len(), 4500, "no row repeated at a page boundary");
    let dates: Vec<&str> = newest_first
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["date"].as_str().unwrap())
        .collect();
    assert!(dates.windows(2).all(|w| w[0] >= w[1]), "newest first");

    let imported = l.get("/history/artist?artistId=7&eventType=3").await;
    assert_eq!(imported.as_array().unwrap().len(), 1500);

    let since = l
        .get("/history/since?date=2026-01-01T00:30:00Z&artistId=7")
        .await;
    let since = since.as_array().unwrap();
    assert_eq!(since.len(), 1500, "rows on or after the date");
    let d: Vec<&str> = since.iter().map(|r| r["date"].as_str().unwrap()).collect();
    assert!(d.windows(2).all(|w| w[0] <= w[1]), "oldest first");

    let none = l.get("/history/artist?artistId=999").await;
    assert_eq!(none, json!([]));
}

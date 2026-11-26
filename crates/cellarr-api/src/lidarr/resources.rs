//! The JSON shapes the Lidarr API speaks, built from cellarr's records.
//!
//! Every resource here is a plain function from cellarr data to a
//! `serde_json::Value` with Lidarr's field names. They are deliberately free of
//! I/O: a handler gathers what a resource needs (statistics, profile ids) in a
//! fixed number of queries and passes it in, so listing a thousand artists never
//! costs a thousand queries.

use std::collections::HashMap;

use cellarr_core::{Album, Artist, ContentId, MetadataProfile, Track, TrackFile};
use cellarr_db::MusicStats;
use serde_json::{json, Value};

use super::quality::lidarr_quality_id;

/// Where an album's cover art is served from when cellarr holds none locally.
pub(crate) const COVER_ART_BASE: &str = "https://coverartarchive.org/release-group";

/// The quality object a file or release carries.
#[must_use]
pub fn quality_object(name: &str) -> Value {
    json!({
        "quality": { "id": lidarr_quality_id(name), "name": name },
        "revision": { "version": 1, "real": 0, "isRepack": false },
    })
}

/// The statistics block of an artist or album.
#[must_use]
pub fn statistics(stats: MusicStats, artist: bool) -> Value {
    let percent = if stats.track_count > 0 {
        (stats.track_file_count as f64 / stats.track_count as f64) * 100.0
    } else {
        0.0
    };
    let mut v = json!({
        "trackFileCount": stats.track_file_count,
        "trackCount": stats.track_count,
        "totalTrackCount": stats.track_count,
        "sizeOnDisk": stats.size_on_disk,
        "percentOfTracks": percent,
    });
    if artist {
        v["albumCount"] = json!(stats.album_count);
    }
    v
}

/// The last path component, used as an artist's `folder`.
#[must_use]
pub fn folder_of(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The path above an artist's folder: the root folder it lives under.
#[must_use]
pub fn root_of(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => String::new(),
    }
}

/// The lowercase, punctuation-free name Lidarr calls `cleanName`.
#[must_use]
pub fn clean_name(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase()
}

/// `links`, `images` and `ratings` for an artist, from its stored extras
/// document, in the shapes Lidarr serves.
fn artist_extras(extras: Option<&str>) -> (Value, Value, Value) {
    let doc: Value = extras
        .and_then(|e| serde_json::from_str(e).ok())
        .unwrap_or(Value::Null);
    let links = doc.get("links").cloned().unwrap_or_else(|| json!([]));
    let images = images_of(doc.get("images"));
    let ratings = match doc.get("rating") {
        Some(r) if r.is_object() => r.clone(),
        _ => json!({ "votes": 0, "value": 0.0 }),
    };
    (links, images, ratings)
}

/// Render stored `{coverType, url}` pairs as Lidarr image resources.
#[must_use]
pub fn images_of(raw: Option<&Value>) -> Value {
    let list = raw.and_then(Value::as_array).cloned().unwrap_or_default();
    Value::Array(
        list.iter()
            .filter_map(|i| {
                let url = i.get("url")?.as_str()?;
                let ext = url
                    .rsplit('.')
                    .next()
                    .filter(|e| e.len() <= 4 && !e.contains('/'))
                    .map(|e| format!(".{e}"))
                    .unwrap_or_default();
                Some(json!({
                    "url": url,
                    "coverType": i.get("coverType").and_then(Value::as_str).unwrap_or("cover").to_lowercase(),
                    "extension": ext,
                    "remoteUrl": url,
                }))
            })
            .collect(),
    )
}

/// An artist resource.
#[must_use]
pub fn artist(a: &Artist, stats: Option<MusicStats>, profile_seq: Option<i64>) -> Value {
    let (links, images, ratings) = artist_extras(a.extras.as_deref());
    let mut v = json!({
        "id": a.id,
        "status": if a.ended { "ended" } else { "continuing" },
        "ended": a.ended,
        "artistName": a.name,
        "foreignArtistId": a.mbid,
        "mbId": a.mbid,
        "tadbId": 0,
        "discogsId": 0,
        "allMusicId": "",
        "overview": a.overview.clone().unwrap_or_default(),
        "artistType": a.artist_type.clone().unwrap_or_default(),
        "disambiguation": a.disambiguation.clone().unwrap_or_default(),
        "links": links,
        "nextAlbum": null,
        "lastAlbum": null,
        "images": images,
        "members": [],
        "remotePoster": null,
        "addOptions": {
            "monitor": "none",
            "albumsToMonitor": [],
            "monitored": false,
            "searchForMissingAlbums": false,
        },
        "path": a.path,
        "qualityProfileId": profile_seq.unwrap_or(0),
        "metadataProfileId": a.metadata_profile_id,
        "monitored": a.monitored,
        "monitorNewItems": if a.monitor_new_albums { "all" } else { "none" },
        "rootFolderPath": root_of(&a.path),
        "folder": folder_of(&a.path),
        "genres": a.genres,
        "cleanName": clean_name(&a.name),
        "sortName": a.sort_name.clone().unwrap_or_else(|| a.name.to_lowercase()),
        "tags": [],
        "added": a.added_at,
        "ratings": ratings,
    });
    if let Some(stats) = stats {
        v["statistics"] = statistics(stats, true);
    }
    v
}

/// A bare artist resource for embedding in an album or a track: no statistics.
#[must_use]
pub fn artist_ref(a: &Artist, profile_seq: Option<i64>) -> Value {
    artist(a, None, profile_seq)
}

/// An album resource. `artist` is embedded when given.
#[must_use]
pub fn album(
    al: &Album,
    artist_id: i64,
    artist: Option<Value>,
    stats: Option<MusicStats>,
    profile_seq: Option<i64>,
    tracks: &[Track],
) -> Value {
    let discs = tracks.iter().map(|t| t.disc).max().unwrap_or(1);
    let duration: u64 = tracks
        .iter()
        .filter_map(|t| t.duration_ms)
        .map(u64::from)
        .sum();
    let media: Vec<Value> = (1..=discs.max(1))
        .map(|n| json!({ "mediumNumber": n, "mediumName": "", "mediumFormat": "CD" }))
        .collect();
    let releases = match &al.release_mbid {
        Some(rel) => vec![json!({
            "id": al.id,
            "albumId": al.id,
            "foreignReleaseId": rel,
            "title": al.title,
            "status": "Official",
            "duration": duration,
            "trackCount": tracks.len(),
            "media": media,
            "mediaCount": discs,
            "disambiguation": "",
            "country": [],
            "label": [],
            "format": "CD",
            "monitored": true,
        })],
        None => Vec::new(),
    };
    let mut v = json!({
        "id": al.id,
        "title": al.title,
        "disambiguation": al.disambiguation.clone().unwrap_or_default(),
        "overview": "",
        "artistId": artist_id,
        "foreignAlbumId": al.mbid,
        "monitored": al.monitored,
        "anyReleaseOk": al.any_release_ok,
        "profileId": profile_seq.unwrap_or(0),
        "duration": duration,
        "albumType": al.album_type,
        "secondaryTypes": al.secondary_types,
        "mediumCount": discs,
        "ratings": { "votes": 0, "value": 0.0 },
        "releaseDate": release_date_iso(al.release_date.as_deref()),
        "releases": releases,
        "genres": [],
        "media": media,
        "images": [{
            "coverType": "cover",
            "url": format!("{COVER_ART_BASE}/{}/front-500", al.mbid),
            "remoteUrl": format!("{COVER_ART_BASE}/{}/front-500", al.mbid),
        }],
        "links": [],
        "added": al.added_at,
        "grabbed": false,
        "cleanTitle": clean_name(&al.title),
        "lastSearchTime": null,
        "remoteCover": null,
        "addOptions": { "addType": "automatic", "searchForNewAlbum": false },
    });
    if let Some(a) = artist {
        v["artist"] = a;
    }
    // An album with no monitored release has no tracks in Lidarr, so no
    // statistics: Lidarr serializes them as null.
    if let Some(stats) = stats.filter(|_| al.release_monitored) {
        v["statistics"] = statistics(stats, false);
    }
    v
}

/// Replace an album resource's releases with every stored pressing. The pinned
/// one (the album's `release_mbid`) is monitored; with none pinned, the first;
/// none at all when the album's release is flagged unmonitored.
pub fn set_releases(v: &mut Value, al: &Album, pressings: &[cellarr_core::AlbumRelease]) {
    if pressings.is_empty() {
        return;
    }
    let pinned = al
        .release_mbid
        .as_deref()
        .filter(|p| pressings.iter().any(|r| r.mbid == *p))
        .unwrap_or(pressings[0].mbid.as_str());
    let releases: Vec<Value> = pressings
        .iter()
        .map(|r| {
            let media: Vec<Value> = r
                .media
                .iter()
                .map(|(n, name, format)| {
                    json!({ "mediumNumber": n, "mediumName": name, "mediumFormat": format })
                })
                .collect();
            json!({
                "id": r.id,
                "albumId": al.id,
                "foreignReleaseId": r.mbid,
                "title": r.title,
                "status": r.status.clone().unwrap_or_default(),
                "duration": r.duration_ms,
                "trackCount": r.track_count,
                "media": media,
                "mediumCount": r.media.len(),
                "disambiguation": r.disambiguation.clone().unwrap_or_default(),
                "country": r.country,
                "label": r.label,
                "format": r.media.first().map(|m| m.2.clone()).unwrap_or_default(),
                "monitored": al.release_monitored && r.mbid == pinned,
            })
        })
        .collect();
    v["releases"] = Value::Array(releases);
    v["anyReleaseOk"] = json!(al.any_release_ok);
}

/// A release date as the ISO timestamp Lidarr serializes. A partial date is
/// completed to its first day, which is how a year-only release date compares in
/// Lidarr too.
#[must_use]
pub fn release_date_iso(date: Option<&str>) -> Value {
    let Some(d) = date.filter(|d| d.len() >= 4) else {
        return Value::Null;
    };
    let full = match d.len() {
        4 => format!("{d}-01-01"),
        7 => format!("{d}-01"),
        _ => d.chars().take(10).collect(),
    };
    json!(format!("{full}T00:00:00Z"))
}

/// A track resource.
#[must_use]
pub fn track(
    t: &Track,
    album_id: i64,
    artist_id: i64,
    file: Option<&TrackFile>,
    artist: Option<Value>,
) -> Value {
    let mut v = json!({
        "id": t.id,
        "artistId": artist_id,
        "foreignTrackId": t.mbid.clone().unwrap_or_default(),
        "foreignRecordingId": t.mbid.clone().unwrap_or_default(),
        "trackFileId": file.map_or(0, |f| f.id),
        "albumId": album_id,
        "explicit": false,
        "absoluteTrackNumber": t.position,
        "trackNumber": t.position.to_string(),
        "title": t.title,
        "duration": t.duration_ms.unwrap_or(0),
        "mediumNumber": t.disc,
        "hasFile": file.is_some(),
        "ratings": { "votes": 0, "value": 0.0 },
    });
    if let Some(a) = artist {
        v["artist"] = a;
    }
    v
}

/// A track-file resource.
#[must_use]
pub fn track_file(f: &TrackFile, artist_id: i64, album_id: i64, cutoff_rank: Option<u32>) -> Value {
    let info = f.file.media_info.clone().unwrap_or(Value::Null);
    let get = |k: &str| info.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "id": f.id,
        "artistId": artist_id,
        "albumId": album_id,
        "path": f.file.path,
        "size": f.file.size,
        "dateAdded": f.added_at,
        "sceneName": "",
        "releaseGroup": "",
        "quality": quality_object(&f.file.quality.name),
        "qualityWeight": f.file.quality.rank,
        "customFormats": [],
        "customFormatScore": f.file.custom_format_score.unwrap_or(0),
        "indexerFlags": 0,
        "releaseType": "unknown",
        "mediaInfo": {
            "audioFormat": get("codec"),
            "audioBitrate": info.get("bitrateKbps").and_then(Value::as_u64).map_or(Value::Null, |k| json!(format!("{k} kbps"))),
            "audioChannels": 2,
            "audioBits": get("bitDepth"),
            "audioSampleRate": get("sampleRate"),
        },
        "qualityCutoffNotMet": cutoff_rank.is_some_and(|c| f.file.quality.rank < c),
    })
}

/// The order Lidarr lists a metadata profile's types in, with their ids.
const PRIMARY_ORDER: &[(usize, &str)] = &[
    (2, "Single"),
    (4, "Other"),
    (1, "EP"),
    (3, "Broadcast"),
    (0, "Album"),
];
const SECONDARY_ORDER: &[(usize, &str)] = &[
    (0, "Studio"),
    (3, "Spokenword"),
    (2, "Soundtrack"),
    (7, "Remix"),
    (9, "Mixtape/Street"),
    (6, "Live"),
    (4, "Interview"),
    (10, "Demo"),
    (8, "DJ-mix"),
    (1, "Compilation"),
    (11, "Audio drama"),
];
const STATUS_ORDER: &[(usize, &str)] = &[
    (3, "Pseudo-Release"),
    (1, "Promotion"),
    (0, "Official"),
    (2, "Bootleg"),
];

/// A metadata profile resource.
#[must_use]
pub fn metadata_profile(p: &MetadataProfile) -> Value {
    let list = |order: &[(usize, &str)], key: &str, allowed: &[String]| -> Vec<Value> {
        order
            .iter()
            .map(|(id, name)| {
                json!({
                    key: { "id": id, "name": name },
                    "allowed": allowed.iter().any(|t| t.eq_ignore_ascii_case(name)),
                })
            })
            .collect()
    };
    json!({
        "name": p.name,
        "primaryAlbumTypes": list(PRIMARY_ORDER, "albumType", &p.primary_types),
        "secondaryAlbumTypes": list(SECONDARY_ORDER, "albumType", &p.secondary_types),
        "releaseStatuses": list(STATUS_ORDER, "releaseStatus", &p.release_statuses),
        "id": p.id,
    })
}

/// Read a metadata profile back from the resource a client PUTs or POSTs.
#[must_use]
pub fn metadata_profile_from(body: &Value, id: i64) -> MetadataProfile {
    let allowed_names = |key: &str, inner: &str| -> Vec<String> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i.get("allowed").and_then(Value::as_bool).unwrap_or(false))
                    .filter_map(|i| i.get(inner)?.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    MetadataProfile {
        id,
        name: body
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Profile")
            .to_string(),
        primary_types: allowed_names("primaryAlbumTypes", "albumType"),
        secondary_types: allowed_names("secondaryAlbumTypes", "albumType"),
        release_statuses: allowed_names("releaseStatuses", "releaseStatus"),
    }
}

/// An index of tracks' album/artist sequence ids, so a list of tracks or files
/// is rendered without a query each.
#[derive(Default)]
pub struct IdIndex {
    /// Album content id → (album seq id, artist content id).
    pub albums: HashMap<ContentId, (i64, ContentId)>,
    /// Artist content id → artist seq id.
    pub artists: HashMap<ContentId, i64>,
}

impl IdIndex {
    /// The `(album id, artist id)` a content id of an album belongs to.
    #[must_use]
    pub fn of_album(&self, album: ContentId) -> (i64, i64) {
        self.albums
            .get(&album)
            .map_or((0, 0), |(album_id, artist)| {
                (*album_id, self.artists.get(artist).copied().unwrap_or(0))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_release_dates_complete_to_their_first_day() {
        assert_eq!(
            release_date_iso(Some("1997")),
            json!("1997-01-01T00:00:00Z")
        );
        assert_eq!(
            release_date_iso(Some("1997-05")),
            json!("1997-05-01T00:00:00Z")
        );
        assert_eq!(
            release_date_iso(Some("1997-05-21")),
            json!("1997-05-21T00:00:00Z")
        );
        assert_eq!(release_date_iso(None), Value::Null);
        assert_eq!(release_date_iso(Some("")), Value::Null);
    }

    #[test]
    fn an_artist_path_splits_into_root_and_folder() {
        assert_eq!(root_of("/data/media/music/Radiohead"), "/data/media/music");
        assert_eq!(folder_of("/data/media/music/Radiohead/"), "Radiohead");
        assert_eq!(root_of("/Radiohead"), "/");
        assert_eq!(root_of("Radiohead"), "");
    }

    #[test]
    fn a_metadata_profile_round_trips_through_its_resource() {
        let mut p = MetadataProfile::standard();
        p.primary_types.push("EP".into());
        p.secondary_types.push("Live".into());
        let mut back = metadata_profile_from(&metadata_profile(&p), p.id);
        // Resources list types in Lidarr's display order, not insertion order.
        for v in [&mut back.primary_types, &mut back.secondary_types] {
            v.sort();
        }
        p.primary_types.sort();
        p.secondary_types.sort();
        assert_eq!(back, p);
    }

    #[test]
    fn an_album_with_no_monitored_release_has_no_statistics() {
        let mut al: Album = serde_json::from_value(json!({
            "id": 7,
            "content_id": ContentId::new(),
            "artist_content_id": ContentId::new(),
            "mbid": "mb-rg1",
            "title": "Kid A",
            "disambiguation": null,
            "album_type": "Album",
            "secondary_types": [],
            "release_date": "2000",
            "monitored": true,
            "release_mbid": null,
            "added_at": "2026-01-01T00:00:00Z",
            "tracks_fetched_at": null,
        }))
        .unwrap();
        let stats = MusicStats {
            album_count: 1,
            track_count: 10,
            ..MusicStats::default()
        };
        assert!(album(&al, 1, None, Some(stats), None, &[])["statistics"].is_object());
        al.release_monitored = false;
        assert!(album(&al, 1, None, Some(stats), None, &[])
            .get("statistics")
            .is_none());
    }

    #[test]
    fn percent_of_tracks_is_zero_for_an_album_with_no_known_tracks() {
        let v = statistics(MusicStats::default(), false);
        assert_eq!(v["percentOfTracks"], json!(0.0));
        assert!(v.get("albumCount").is_none());
    }
}

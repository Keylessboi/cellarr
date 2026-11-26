//! Reading the config tables Sonarr and Radarr share: root folders, indexers,
//! and download clients.
//!
//! These tables have the same shape in both apps. Connection settings are
//! carried across verbatim (in the `settings` JSON blob) so the user can re-test
//! them on import; the migration spec is explicit that connections are
//! re-tested, not trusted blind, so cellarr stores rather than validates here.

use cellarr_core::{
    DownloadClientConfig, DownloadClientId, IndexerConfig, IndexerId, Protocol, RootFolder,
};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::error::Result;
use crate::source::{opt_text, SourceKind};

/// Read `RootFolders` rows.
///
/// # Errors
/// Returns a [`crate::MigrationError`] on query failure.
pub(crate) async fn read_root_folders(pool: &SqlitePool) -> Result<Vec<RootFolder>> {
    let rows = sqlx::query("SELECT Id, Path FROM RootFolders ORDER BY Id ASC")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let id: i64 = row.try_get("Id").unwrap_or(0);
            let path: String = row.try_get("Path").unwrap_or_default();
            RootFolder {
                id: format!("rootfolder-{id}"),
                path,
                name: None,
                enabled: true,
            }
        })
        .collect())
}

/// Read `Indexers` rows. The source's `Implementation` becomes the cellarr
/// adapter `kind`; `Settings` (the JSON connection blob) is carried verbatim.
///
/// # Errors
/// Returns a [`crate::MigrationError`] on query failure.
pub(crate) async fn read_indexers(
    pool: &SqlitePool,
    kind: SourceKind,
) -> Result<Vec<IndexerConfig>> {
    let rows = sqlx::query(
        // `*`, not a column list: Radarr/Sonarr store `Protocol`, Lidarr derives it
        // from the implementation and has no such column.
        "SELECT * FROM Indexers ORDER BY Id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let name: String = row.try_get("Name").unwrap_or_default();
            let implementation = opt_text(row, "Implementation").unwrap_or_default();
            let settings = native_apple(
                &implementation,
                native_slskd(&implementation, parse_settings(opt_text(row, "Settings"))),
            );
            let priority: i64 = row.try_get("Priority").unwrap_or(0);
            // All three capabilities, not just RSS. The source models them
            // independently, and reading one to stand for all of them silently
            // drops the operator's choice — an indexer they kept for searches but
            // out of the RSS cadence would come across switched off entirely.
            let flag = |col: &str| row.try_get::<i64, _>(col).unwrap_or(1) != 0;
            let enable_rss = flag("EnableRss");
            let enable_automatic_search = flag("EnableAutomaticSearch");
            let enable_interactive_search = flag("EnableInteractiveSearch");
            let enabled = enable_rss || enable_automatic_search || enable_interactive_search;
            IndexerConfig {
                id: IndexerId::new(),
                name,
                kind: adapter_kind(&implementation, kind),
                protocol: protocol_of(row),
                enabled,
                enable_rss,
                enable_automatic_search,
                enable_interactive_search,
                priority: priority as i32,
                // The legacy Sonarr/Radarr minimumSeeders/seedCriteria live in the
                // indexer's Settings JSON; carrying them across is long-tail and
                // the criteria default (gate nothing) preserves prior behaviour.
                // TODO(deferred): lift minimumSeeders/seedCriteria/required flags
                // out of the legacy Settings JSON into IndexerCriteria.
                criteria: cellarr_core::IndexerCriteria::default(),
                // Legacy per-indexer Tags (a JSON array of source tag ids) are not
                // remapped to cellarr tag ids here; default to untagged (global),
                // preserving prior behaviour.
                // TODO(deferred): remap the source `Tags` column to migrated
                // cellarr tag ids once the tag vocabulary is migrated across.
                tags: Vec::new(),
                settings,
            }
        })
        .collect())
}

/// Read `DownloadClients` rows.
///
/// # Errors
/// Returns a [`crate::MigrationError`] on query failure.
pub(crate) async fn read_download_clients(
    pool: &SqlitePool,
    kind: SourceKind,
) -> Result<Vec<DownloadClientConfig>> {
    let rows = sqlx::query("SELECT * FROM DownloadClients ORDER BY Id ASC")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let name: String = row.try_get("Name").unwrap_or_default();
            let implementation = opt_text(row, "Implementation").unwrap_or_default();
            let settings = native_apple(
                &implementation,
                native_slskd(&implementation, parse_settings(opt_text(row, "Settings"))),
            );
            let priority: i64 = row.try_get("Priority").unwrap_or(0);
            let enabled = row.try_get::<i64, _>("Enable").unwrap_or(1) != 0;
            DownloadClientConfig {
                id: DownloadClientId::new(),
                name,
                kind: adapter_kind(&implementation, kind),
                protocol: protocol_of(row),
                enabled,
                priority: priority as i32,
                // The category the source app tagged its downloads with. It is
                // what the client's per-category save path, and any tooling
                // around the client (cleanup jobs keyed on it), depend on, so it
                // carries over unchanged; only a client that set none falls back
                // to the app's name.
                category: category_of(&settings).unwrap_or_else(|| kind.app_name().to_string()),
                // Legacy per-client Tags are not remapped here; default to
                // untagged (global), preserving prior behaviour.
                // TODO(deferred): remap the source `Tags` column to migrated
                // cellarr tag ids once the tag vocabulary is migrated across.
                tags: Vec::new(),
                settings,
            }
        })
        .collect())
}

/// Map the source `Protocol` column to a cellarr [`Protocol`].
///
/// The originals store either a string (`"torrent"`/`"usenet"`) or an integer
/// enum (0 = usenet, 1 = torrent in the apps' `DownloadProtocol`); handle both.
fn protocol_of(row: &sqlx::sqlite::SqliteRow) -> Protocol {
    if let Some(s) = opt_text(row, "Protocol") {
        return match s.to_ascii_lowercase().as_str() {
            "torrent" => Protocol::Torrent,
            _ => Protocol::Usenet,
        };
    }
    if let Ok(n) = row.try_get::<i64, _>("Protocol") {
        return if n == 1 {
            Protocol::Torrent
        } else {
            Protocol::Usenet
        };
    }
    // No `Protocol` column (Lidarr): it follows from what the entry is.
    protocol_of_implementation(&opt_text(row, "Implementation").unwrap_or_default())
}

/// The protocol an indexer or download-client implementation speaks. The
/// usenet ones are the short, well-known list; everything else (Torznab, every
/// torrent client, torrent blackhole) is a torrent.
fn protocol_of_implementation(implementation: &str) -> Protocol {
    const USENET: &[&str] = &[
        "newznab",
        "sabnzbd",
        "nzbget",
        "nzbvortex",
        "usenetblackhole",
        "pneumatic",
        "usenetdownloadstation",
    ];
    if USENET.contains(&implementation.to_ascii_lowercase().as_str()) {
        Protocol::Usenet
    } else {
        Protocol::Torrent
    }
}

/// The download category a client's settings name (`musicCategory` in Lidarr,
/// `tvCategory` in Sonarr, `movieCategory` in Radarr, or a plain `category`).
fn category_of(settings: &serde_json::Value) -> Option<String> {
    ["musicCategory", "tvCategory", "movieCategory", "category"]
        .iter()
        .find_map(|k| settings.get(*k).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// A Tubifarry `Slskd` indexer or client row, mapped to the settings cellarr's
/// native Soulseek kind reads (`baseUrl`, `apiKey`). The kind stays `slskd`: the
/// same name resolves to the native client when the row has a URL and key, and to
/// the sidecar plugin when it does not. Everything else in the row is kept.
fn native_slskd(implementation: &str, settings: serde_json::Value) -> serde_json::Value {
    if !implementation.eq_ignore_ascii_case("slskd") {
        return settings;
    }
    let serde_json::Value::Object(mut map) = settings else {
        return settings;
    };
    for (want, aliases) in [
        ("baseUrl", ["BaseUrl", "baseURL", "url"]),
        ("apiKey", ["ApiKey", "apikey", "api_key"]),
    ] {
        if !map.contains_key(want) {
            if let Some(v) = aliases.iter().find_map(|a| map.get(*a).cloned()) {
                map.insert(want.to_string(), v);
            }
        }
    }
    if let Some(u) = map.get("baseUrl").and_then(|v| v.as_str()) {
        let trimmed = u.trim().trim_end_matches('/').to_string();
        map.insert("baseUrl".to_string(), serde_json::Value::String(trimmed));
    }
    serde_json::Value::Object(map)
}

/// An AppleMusicarr plugin row (indexer or download client), mapped to the
/// settings cellarr's native Apple Music kind reads. The plugin's own field names
/// are camelCase already; this accepts a PascalCase spelling too and trims the
/// decryptor URL. The kind stays what the plugin called itself (lowercased), so a
/// row the native kind cannot be built from (no developer token, no decryptor
/// URL, or `native: false`) still resolves to the sidecar plugin.
fn native_apple(implementation: &str, settings: serde_json::Value) -> serde_json::Value {
    if !implementation
        .to_ascii_lowercase()
        .starts_with("applemusicarr")
    {
        return settings;
    }
    let serde_json::Value::Object(mut map) = settings else {
        return settings;
    };
    for want in [
        "developerToken",
        "musicUserToken",
        "storefront",
        "earlyReleaseLimit",
        "searchLimit",
        "decryptorUrl",
        "downloadPath",
        "preferredQuality",
        "decryptorTimeoutMinutes",
        "baseUrl",
    ] {
        if !map.contains_key(want) {
            let pascal = format!("{}{}", want[..1].to_ascii_uppercase(), &want[1..]);
            if let Some(v) = map.get(&pascal).cloned() {
                map.insert(want.to_string(), v);
            }
        }
    }
    if let Some(u) = map.get("decryptorUrl").and_then(|v| v.as_str()) {
        let trimmed = u.trim().trim_end_matches('/').to_string();
        map.insert(
            "decryptorUrl".to_string(),
            serde_json::Value::String(trimmed),
        );
    }
    serde_json::Value::Object(map)
}

fn parse_settings(raw: Option<String>) -> serde_json::Value {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

/// Lowercase the source `Implementation` into a cellarr adapter kind, prefixing
/// the source app so re-test/round-trip can tell where a config originated.
fn adapter_kind(implementation: &str, _kind: SourceKind) -> String {
    if implementation.is_empty() {
        "unknown".to_string()
    } else {
        implementation.to_ascii_lowercase()
    }
}

//! What plugins are loaded, in a form the API can render without the WASM
//! runtime: each plugin's implementation name, kind, protocol and settings.
//!
//! Boot fills it after loading the plugin directory; the *arr faces read it to
//! list plugin implementations in their schemas beside the built-in ones.

use std::sync::{OnceLock, RwLock};

/// Indexer or download client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// An indexer.
    Indexer,
    /// A download client.
    DownloadClient,
}

/// One setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryField {
    /// Settings key.
    pub name: String,
    /// Label.
    pub label: String,
    /// *arr field type.
    pub kind: String,
    /// Default, as JSON.
    pub default_value: String,
    /// Help text.
    pub help_text: String,
    /// Advanced.
    pub advanced: bool,
    /// Secret.
    pub privacy: bool,
}

/// One loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Implementation name.
    pub name: String,
    /// Version.
    pub version: String,
    /// Description.
    pub description: String,
    /// Kind.
    pub kind: EntryKind,
    /// `torrent` or `usenet`.
    pub protocol: String,
    /// Settings.
    pub fields: Vec<EntryField>,
    /// `wasm` or `dotnet` (a native Lidarr plugin hosted by the sidecar).
    pub runtime: String,
}

static CATALOG: OnceLock<RwLock<Vec<Entry>>> = OnceLock::new();

fn cell() -> &'static RwLock<Vec<Entry>> {
    CATALOG.get_or_init(|| RwLock::new(Vec::new()))
}

/// Replace the entries of one runtime.
pub fn set_runtime(runtime: &str, entries: Vec<Entry>) {
    if let Ok(mut g) = cell().write() {
        g.retain(|e| e.runtime != runtime);
        g.extend(entries);
    }
}

/// Every loaded plugin.
#[must_use]
pub fn entries() -> Vec<Entry> {
    cell().read().map(|g| g.clone()).unwrap_or_default()
}

/// Loaded plugins of one kind.
#[must_use]
pub fn of_kind(kind: EntryKind) -> Vec<Entry> {
    entries().into_iter().filter(|e| e.kind == kind).collect()
}

/// The entry implementing `name` (case-insensitive).
#[must_use]
pub fn find(name: &str) -> Option<Entry> {
    entries()
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
}

/// Render an entry as an *arr schema resource (`implementation`, `fields`, ...).
#[must_use]
pub fn schema_resource(e: &Entry) -> serde_json::Value {
    let fields: Vec<serde_json::Value> = e
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let value: serde_json::Value =
                serde_json::from_str(&f.default_value).unwrap_or(serde_json::Value::Null);
            let mut v = serde_json::json!({
                "order": i,
                "name": f.name,
                "label": f.label,
                "helpText": f.help_text,
                "value": value,
                "type": f.kind,
                "advanced": f.advanced,
                "isFloat": false,
            });
            if f.privacy {
                v["privacy"] = serde_json::json!(if f.kind == "password" {
                    "password"
                } else {
                    "apiKey"
                });
            }
            v
        })
        .collect();
    serde_json::json!({
        "name": "",
        "implementation": e.name,
        "implementationName": e.name,
        "configContract": format!("{}Settings", e.name),
        "infoLink": "",
        "protocol": e.protocol,
        "priority": 1,
        "enable": true,
        "fields": fields,
        "tags": [],
        "presets": [],
        "message": { "message": format!("Plugin ({}) v{}: {}", e.runtime, e.version, e.description), "type": "info" },
    })
}

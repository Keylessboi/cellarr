//! Loading plugins from a directory, and the process-wide registry of them.
//!
//! Layout, under the plugin directory (`<data>/plugins` by default):
//!
//! ```text
//! plugins/
//!   my-indexer.wasm                 a plugin on its own
//!   my-client/plugin.wasm           or in a folder, with an optional manifest:
//!   my-client/plugin.json           {"allowHosts": ["https://api.example"],
//!                                    "fuel": 2000000000, "memoryMib": 64}
//! ```
//!
//! A plugin that fails to load is reported and skipped; the others still load.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use serde::Deserialize;

use crate::config::HostConfig;
use crate::error::{PluginError, Result};
use crate::host::{Plugin, PluginKind, Transport};

/// Default fuel for a loaded plugin's call: enough to parse a large RSS feed.
pub const PLUGIN_FUEL: u64 = 5_000_000_000;
/// Default memory cap for a loaded plugin.
pub const PLUGIN_MEMORY_MIB: usize = 64;
/// Core instances per call (a componentized module and its shims).
pub const PLUGIN_INSTANCES: usize = 8;
/// Default wall-clock budget for one call (HTTP included).
pub const PLUGIN_CALL_SECS: u64 = 60;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Manifest {
    allow_hosts: Vec<String>,
    fuel: Option<u64>,
    memory_mib: Option<usize>,
}

/// A plugin that did not load, and why.
#[derive(Debug, Clone)]
pub struct LoadFailure {
    /// The file.
    pub path: PathBuf,
    /// What went wrong.
    pub reason: String,
}

/// The loaded plugins, by implementation name.
#[derive(Default)]
pub struct Registry {
    plugins: BTreeMap<String, Arc<Plugin>>,
    failures: Vec<LoadFailure>,
    dir: Option<PathBuf>,
}

impl Registry {
    /// Load every plugin under `dir`. A missing directory is an empty registry.
    #[must_use]
    pub fn load_dir(dir: &Path, transport: Transport) -> Self {
        let mut reg = Self {
            dir: Some(dir.to_path_buf()),
            ..Self::default()
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return reg;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            let (wasm, manifest) = if p.is_dir() {
                (p.join("plugin.wasm"), Some(p.join("plugin.json")))
            } else if p.extension().is_some_and(|e| e == "wasm") {
                (p.clone(), None)
            } else {
                continue;
            };
            if !wasm.is_file() {
                continue;
            }
            match load_one(&wasm, manifest.as_deref(), Arc::clone(&transport)) {
                Ok(plugin) => {
                    let name = plugin.info().name.clone();
                    match reg.plugins.entry(name) {
                        Entry::Occupied(taken) => reg.failures.push(LoadFailure {
                            path: wasm,
                            reason: format!("another plugin is already named {}", taken.key()),
                        }),
                        Entry::Vacant(slot) => {
                            tracing::info!(
                                plugin = %slot.key(),
                                version = %plugin.info().version,
                                kind = ?plugin.kind(),
                                "plugin loaded"
                            );
                            slot.insert(Arc::new(plugin));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %wasm.display(), error = %e, "plugin not loaded");
                    reg.failures.push(LoadFailure {
                        path: wasm,
                        reason: e.to_string(),
                    });
                }
            }
        }
        reg
    }

    /// A registry holding the given plugins (tests).
    #[must_use]
    pub fn from_plugins(plugins: Vec<Plugin>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|p| (p.info().name.clone(), Arc::new(p)))
                .collect(),
            ..Self::default()
        }
    }

    /// The plugin implementing `name` (case-insensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<Plugin>> {
        self.plugins
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, p)| Arc::clone(p))
    }

    /// Every loaded plugin of a kind.
    #[must_use]
    pub fn of_kind(&self, kind: PluginKind) -> Vec<Arc<Plugin>> {
        self.plugins
            .values()
            .filter(|p| p.kind() == kind)
            .cloned()
            .collect()
    }

    /// Every loaded plugin.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<Plugin>> {
        self.plugins.values().cloned().collect()
    }

    /// The plugins that did not load.
    #[must_use]
    pub fn failures(&self) -> &[LoadFailure] {
        &self.failures
    }

    /// Where plugins are loaded from.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

fn load_one(wasm: &Path, manifest: Option<&Path>, transport: Transport) -> Result<Plugin> {
    let bytes = std::fs::read(wasm).map_err(|e| PluginError::Load(e.to_string()))?;
    let manifest: Manifest = match manifest.filter(|m| m.is_file()) {
        Some(m) => {
            let text = std::fs::read(m).map_err(|e| PluginError::Load(e.to_string()))?;
            serde_json::from_slice(&text)
                .map_err(|e| PluginError::Load(format!("{}: {e}", m.display())))?
        }
        None => Manifest::default(),
    };
    let config = HostConfig::default()
        .with_http(true)
        .with_max_instances(PLUGIN_INSTANCES)
        .with_fuel(Some(manifest.fuel.unwrap_or(PLUGIN_FUEL)))
        .with_max_memory_bytes(manifest.memory_mib.unwrap_or(PLUGIN_MEMORY_MIB) * 1024 * 1024)
        .with_epoch_timeout(Some(std::time::Duration::from_secs(PLUGIN_CALL_SECS)));
    let origins = manifest
        .allow_hosts
        .iter()
        .filter_map(|h| crate::capability::origin_of(h))
        .collect();
    Ok(Plugin::from_bytes(&bytes, config)?
        .with_transport(transport)
        .with_allowed_origins(origins))
}

static REGISTRY: OnceLock<RwLock<Arc<Registry>>> = OnceLock::new();

/// Install the process-wide registry (boot does this once; tests may replace it).
pub fn install(reg: Registry) {
    let cell = REGISTRY.get_or_init(|| RwLock::new(Arc::new(Registry::default())));
    if let Ok(mut g) = cell.write() {
        *g = Arc::new(reg);
    }
}

/// The process-wide registry (empty until [`install`]).
#[must_use]
pub fn global() -> Arc<Registry> {
    REGISTRY
        .get_or_init(|| RwLock::new(Arc::new(Registry::default())))
        .read()
        .map(|g| Arc::clone(&g))
        .unwrap_or_default()
}

/// The production transport: a blocking HTTP client with a timeout. Calls run
/// on a blocking thread, never on the async runtime.
#[must_use]
pub fn http_transport() -> Transport {
    // Built on first use, never here: a blocking client owns a runtime, which
    // must not be created or dropped on an async thread (boot is one). Plugin
    // calls always run on blocking threads.
    let client: Arc<OnceLock<std::result::Result<reqwest::blocking::Client, String>>> =
        Arc::new(OnceLock::new());
    Arc::new(move |req: &crate::capability::HttpRequest| {
        let client = client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .user_agent(concat!("cellarr-plugin-host/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| PluginError::Host(e.clone()))?;
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| PluginError::Guest(format!("bad method: {e}")))?;
        let mut rb = client.request(method, &req.url);
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if !req.body.is_empty() {
            rb = rb.body(req.body.clone());
        }
        let resp = rb.send().map_err(|e| PluginError::Host(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
            .collect();
        let body = resp
            .bytes()
            .map_err(|e| PluginError::Host(e.to_string()))?
            .to_vec();
        Ok(crate::capability::HttpResponse {
            status,
            headers,
            body,
        })
    })
}

/// The catalog entries for this registry's plugins.
#[must_use]
pub fn catalog_entries(reg: &Registry) -> Vec<crate::catalog::Entry> {
    use crate::catalog::{Entry, EntryField, EntryKind};
    use crate::host::PluginProtocol;
    reg.all()
        .iter()
        .map(|p| Entry {
            name: p.info().name.clone(),
            version: p.info().version.clone(),
            description: p.info().description.clone(),
            kind: match p.kind() {
                PluginKind::Indexer => EntryKind::Indexer,
                PluginKind::DownloadClient => EntryKind::DownloadClient,
            },
            protocol: match p.info().protocol {
                PluginProtocol::Torrent => "torrent".into(),
                PluginProtocol::Usenet => "usenet".into(),
            },
            fields: p
                .fields()
                .iter()
                .map(|f| EntryField {
                    name: f.name.clone(),
                    label: f.label.clone(),
                    kind: f.kind.clone(),
                    default_value: f.default_value.clone(),
                    help_text: f.help_text.clone(),
                    advanced: f.advanced,
                    privacy: f.privacy,
                })
                .collect(),
            runtime: "wasm".into(),
        })
        .collect()
}

/// Load `dir`, install it as the process-wide registry, and publish its catalog.
pub fn load_and_install(dir: &Path) -> (usize, Vec<LoadFailure>) {
    let reg = Registry::load_dir(dir, http_transport());
    let n = reg.all().len();
    let failures = reg.failures().to_vec();
    crate::catalog::set_runtime("wasm", catalog_entries(&reg));
    install(reg);
    (n, failures)
}

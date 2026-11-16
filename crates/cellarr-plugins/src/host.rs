//! The wasmtime host (behind the `wasm` feature).
//!
//! A [`Plugin`] is one compiled component implementing either the
//! `indexer-plugin` or the `download-client-plugin` world of `wit/plugins.wit`.
//! Compilation happens once; every call gets a fresh store and instance, under
//! fuel, an epoch deadline and memory limits, with an HTTP capability scoped to
//! the hosts that call's settings name. Nothing survives between calls.
//!
//! A plugin may be shipped as a component or as a plain core module built with
//! `wit-bindgen` for `wasm32-unknown-unknown`; a core module is turned into a
//! component at load time, so authors need nothing beyond `cargo build`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

use crate::capability::{
    origins_in_settings, AllowListHttp, DenyAllHttp, HttpCapability, HttpRequest, HttpResponse,
};
use crate::config::HostConfig;
use crate::error::{PluginError, Result};

mod idx {
    wasmtime::component::bindgen!({
        path: "wit/plugins.wit",
        world: "indexer-plugin",
    });
}

mod dlc {
    wasmtime::component::bindgen!({
        path: "wit/plugins.wit",
        world: "download-client-plugin",
    });
}

/// The network a plugin's HTTP capability uses once a request is allowed.
pub type Transport = Arc<dyn Fn(&HttpRequest) -> Result<HttpResponse> + Send + Sync>;

/// Which interface a plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// An indexer.
    Indexer,
    /// A download client.
    DownloadClient,
}

/// The protocol a plugin's releases or downloads use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginProtocol {
    /// BitTorrent.
    Torrent,
    /// Usenet.
    Usenet,
}

/// Who a plugin says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// The implementation name *arr clients see.
    pub name: String,
    /// The plugin's version.
    pub version: String,
    /// One line about it.
    pub description: String,
    /// Torrent or Usenet.
    pub protocol: PluginProtocol,
}

/// One setting the plugin takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginField {
    /// Field name (the settings key).
    pub name: String,
    /// Label.
    pub label: String,
    /// *arr field type.
    pub kind: String,
    /// Default, as JSON.
    pub default_value: String,
    /// Help text.
    pub help_text: String,
    /// Hidden behind "advanced".
    pub advanced: bool,
    /// A secret.
    pub privacy: bool,
}

/// A release a plugin indexer returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRelease {
    /// Title.
    pub title: String,
    /// Where to get it.
    pub download_url: String,
    /// Stable id.
    pub guid: Option<String>,
    /// Protocol.
    pub protocol: PluginProtocol,
    /// Bytes.
    pub size: Option<u64>,
    /// Seeders.
    pub seeders: Option<u32>,
    /// Flags (`freeleech`, ...).
    pub indexer_flags: Vec<String>,
}

/// Search terms handed to a plugin indexer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginSearch {
    /// Free-text queries.
    pub queries: Vec<String>,
    /// Structured terms (`artist`, `album`, ...).
    pub ids: Vec<(String, String)>,
    /// Numbering (`season`, `ep`, ...).
    pub numbering: Vec<(String, String)>,
}

/// A download's state, from a plugin client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDownloadState {
    /// Waiting.
    Queued,
    /// In progress.
    Downloading,
    /// Done and importable.
    Completed,
    /// Failed.
    Failed,
}

/// A download's status, from a plugin client.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDownloadStatus {
    /// State.
    pub state: PluginDownloadState,
    /// 0..=1.
    pub progress: f32,
    /// Where the finished content is.
    pub content_path: Option<String>,
    /// Seed ratio.
    pub ratio: Option<f32>,
    /// Seconds seeded.
    pub seeding_time_secs: Option<u64>,
    /// Connected peers.
    pub peers: Option<u32>,
    /// Why it failed.
    pub error_message: Option<String>,
}

struct HostState {
    limits: StoreLimits,
    http: Arc<dyn HttpCapability>,
}

impl HostState {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
        self.http.send(req).map_err(|e| e.to_string())
    }
}

impl idx::cellarr::plugins::http::Host for HostState {
    fn send(
        &mut self,
        req: idx::cellarr::plugins::http::HttpRequest,
    ) -> std::result::Result<
        idx::cellarr::plugins::http::HttpResponse,
        idx::cellarr::plugins::types::PluginError,
    > {
        let r = HttpRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
        };
        HostState::send(self, &r)
            .map(|r| idx::cellarr::plugins::http::HttpResponse {
                status: r.status,
                headers: r.headers,
                body: r.body,
            })
            .map_err(|message| idx::cellarr::plugins::types::PluginError { message })
    }
}
impl idx::cellarr::plugins::types::Host for HostState {}

impl dlc::cellarr::plugins::http::Host for HostState {
    fn send(
        &mut self,
        req: dlc::cellarr::plugins::http::HttpRequest,
    ) -> std::result::Result<
        dlc::cellarr::plugins::http::HttpResponse,
        dlc::cellarr::plugins::types::PluginError,
    > {
        let r = HttpRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
        };
        HostState::send(self, &r)
            .map(|r| dlc::cellarr::plugins::http::HttpResponse {
                status: r.status,
                headers: r.headers,
                body: r.body,
            })
            .map_err(|message| dlc::cellarr::plugins::types::PluginError { message })
    }
}
impl dlc::cellarr::plugins::types::Host for HostState {}

enum Linked {
    Indexer(Linker<HostState>),
    DownloadClient(Linker<HostState>),
}

/// A compiled plugin.
pub struct Plugin {
    engine: Engine,
    component: Component,
    linked: Linked,
    config: HostConfig,
    info: PluginInfo,
    fields: Vec<PluginField>,
    extra_origins: Vec<String>,
    transport: Option<Transport>,
    epoch_stop: Arc<AtomicBool>,
}

/// Turn a core module carrying `wit-bindgen` metadata into a component; pass a
/// component through untouched.
fn as_component(bytes: &[u8]) -> Result<Vec<u8>> {
    if wasmparser::Parser::is_core_wasm(bytes) {
        wit_component::ComponentEncoder::default()
            .module(bytes)
            .and_then(|e| e.validate(true).encode())
            .map_err(|e| PluginError::Load(format!("not a cellarr plugin module: {e:#}")))
    } else {
        Ok(bytes.to_vec())
    }
}

fn kind_of(engine: &Engine, component: &Component) -> Result<PluginKind> {
    let ty = component.component_type();
    let exports: Vec<String> = ty.exports(engine).map(|(n, _)| n.to_string()).collect();
    let has = |iface: &str| {
        exports
            .iter()
            .any(|e| e.starts_with(&format!("cellarr:plugins/{iface}@0.2")))
    };
    if !has("plugin") {
        return Err(PluginError::Load(format!(
            "the component does not export cellarr:plugins/plugin@0.2 (exports: {exports:?})"
        )));
    }
    if has("indexer") {
        Ok(PluginKind::Indexer)
    } else if has("download-client") {
        Ok(PluginKind::DownloadClient)
    } else {
        Err(PluginError::Load(
            "the component exports neither an indexer nor a download client".into(),
        ))
    }
}

impl Plugin {
    /// Compile a plugin from a component or a `wit-bindgen` core module.
    ///
    /// # Errors
    /// [`PluginError::Load`] when it is not a cellarr plugin, or its `info`/`fields`
    /// calls fail.
    pub fn from_bytes(bytes: &[u8], config: HostConfig) -> Result<Self> {
        let bytes = as_component(bytes)?;
        let mut wasm_config = Config::new();
        wasm_config.wasm_component_model(true);
        if config.fuel.is_some() {
            wasm_config.consume_fuel(true);
        }
        if config.epoch_timeout.is_some() {
            wasm_config.epoch_interruption(true);
        }
        let engine = Engine::new(&wasm_config).map_err(|e| PluginError::Load(e.to_string()))?;
        let component =
            Component::new(&engine, &bytes).map_err(|e| PluginError::Load(format!("{e:#}")))?;
        let kind = kind_of(&engine, &component)?;
        // Only the http import (and the types it uses) is linked. No WASI: a
        // guest asking for anything else fails to instantiate.
        let mut linker: Linker<HostState> = Linker::new(&engine);
        let linked = match kind {
            PluginKind::Indexer => {
                idx::IndexerPlugin::add_to_linker::<HostState, HasSelf<HostState>>(
                    &mut linker,
                    |s| s,
                )
                .map_err(|e| PluginError::Host(e.to_string()))?;
                Linked::Indexer(linker)
            }
            PluginKind::DownloadClient => {
                dlc::DownloadClientPlugin::add_to_linker::<HostState, HasSelf<HostState>>(
                    &mut linker,
                    |s| s,
                )
                .map_err(|e| PluginError::Host(e.to_string()))?;
                Linked::DownloadClient(linker)
            }
        };
        let epoch_stop = Arc::new(AtomicBool::new(false));
        if let Some(timeout) = config.epoch_timeout {
            spawn_epoch_ticker(&engine, timeout, Arc::clone(&epoch_stop));
        }
        let mut plugin = Self {
            engine,
            component,
            linked,
            config,
            info: PluginInfo {
                name: String::new(),
                version: String::new(),
                description: String::new(),
                protocol: PluginProtocol::Torrent,
            },
            fields: Vec::new(),
            extra_origins: Vec::new(),
            transport: None,
            epoch_stop,
        };
        let (info, fields) = plugin.describe()?;
        if info.name.trim().is_empty() {
            return Err(PluginError::Load("the plugin reports an empty name".into()));
        }
        plugin.info = info;
        plugin.fields = fields;
        Ok(plugin)
    }

    /// The network requests go out on, once allowed.
    #[must_use]
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Origins the plugin may reach whatever its settings say (from its manifest).
    #[must_use]
    pub fn with_allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.extra_origins = origins;
        self
    }

    /// Indexer or download client.
    #[must_use]
    pub fn kind(&self) -> PluginKind {
        match self.linked {
            Linked::Indexer(_) => PluginKind::Indexer,
            Linked::DownloadClient(_) => PluginKind::DownloadClient,
        }
    }

    /// Who the plugin says it is.
    #[must_use]
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// The settings it takes.
    #[must_use]
    pub fn fields(&self) -> &[PluginField] {
        &self.fields
    }

    fn store(&self, settings: Option<&serde_json::Value>) -> Result<Store<HostState>> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.config.max_memory_bytes)
            .tables(self.config.max_tables)
            .table_elements(self.config.max_table_elements)
            .instances(self.config.max_instances)
            .memories(self.config.max_instances.max(1))
            .build();
        let http: Arc<dyn HttpCapability> =
            match (settings, &self.transport, self.config.grant_http) {
                (Some(s), Some(t), true) => {
                    let mut origins = origins_in_settings(s);
                    origins.extend(self.extra_origins.iter().cloned());
                    let t = Arc::clone(t);
                    Arc::new(AllowListHttp::new(origins, move |r: &HttpRequest| t(r)))
                }
                _ => Arc::new(DenyAllHttp),
            };
        let mut store = Store::new(&self.engine, HostState { limits, http });
        store.limiter(|state| &mut state.limits);
        if let Some(fuel) = self.config.fuel {
            store
                .set_fuel(fuel)
                .map_err(|e| PluginError::Host(e.to_string()))?;
        }
        if self.config.epoch_timeout.is_some() {
            store.set_epoch_deadline(1);
        }
        Ok(store)
    }

    fn describe(&self) -> Result<(PluginInfo, Vec<PluginField>)> {
        let mut store = self.store(None)?;
        match &self.linked {
            Linked::Indexer(linker) => {
                let inst = idx::IndexerPlugin::instantiate(&mut store, &self.component, linker)
                    .map_err(map_trap)?;
                let p = inst.cellarr_plugins_plugin();
                let info = p.call_info(&mut store).map_err(map_trap)?;
                let fields = p.call_fields(&mut store).map_err(map_trap)?;
                Ok((
                    PluginInfo {
                        name: info.name,
                        version: info.version,
                        description: info.description,
                        protocol: match info.protocol {
                            idx::cellarr::plugins::types::Protocol::Torrent => {
                                PluginProtocol::Torrent
                            }
                            idx::cellarr::plugins::types::Protocol::Usenet => {
                                PluginProtocol::Usenet
                            }
                        },
                    },
                    fields
                        .into_iter()
                        .map(|f| PluginField {
                            name: f.name,
                            label: f.label,
                            kind: f.kind,
                            default_value: f.default_value,
                            help_text: f.help_text,
                            advanced: f.advanced,
                            privacy: f.privacy,
                        })
                        .collect(),
                ))
            }
            Linked::DownloadClient(linker) => {
                let inst =
                    dlc::DownloadClientPlugin::instantiate(&mut store, &self.component, linker)
                        .map_err(map_trap)?;
                let p = inst.cellarr_plugins_plugin();
                let info = p.call_info(&mut store).map_err(map_trap)?;
                let fields = p.call_fields(&mut store).map_err(map_trap)?;
                Ok((
                    PluginInfo {
                        name: info.name,
                        version: info.version,
                        description: info.description,
                        protocol: match info.protocol {
                            dlc::cellarr::plugins::types::Protocol::Torrent => {
                                PluginProtocol::Torrent
                            }
                            dlc::cellarr::plugins::types::Protocol::Usenet => {
                                PluginProtocol::Usenet
                            }
                        },
                    },
                    fields
                        .into_iter()
                        .map(|f| PluginField {
                            name: f.name,
                            label: f.label,
                            kind: f.kind,
                            default_value: f.default_value,
                            help_text: f.help_text,
                            advanced: f.advanced,
                            privacy: f.privacy,
                        })
                        .collect(),
                ))
            }
        }
    }

    fn indexer(
        &self,
        settings: &serde_json::Value,
    ) -> Result<(Store<HostState>, idx::IndexerPlugin)> {
        let Linked::Indexer(linker) = &self.linked else {
            return Err(PluginError::Host(format!(
                "{} is not an indexer plugin",
                self.info.name
            )));
        };
        let mut store = self.store(Some(settings))?;
        let inst = idx::IndexerPlugin::instantiate(&mut store, &self.component, linker)
            .map_err(map_trap)?;
        Ok((store, inst))
    }

    fn client(
        &self,
        settings: &serde_json::Value,
    ) -> Result<(Store<HostState>, dlc::DownloadClientPlugin)> {
        let Linked::DownloadClient(linker) = &self.linked else {
            return Err(PluginError::Host(format!(
                "{} is not a download-client plugin",
                self.info.name
            )));
        };
        let mut store = self.store(Some(settings))?;
        let inst = dlc::DownloadClientPlugin::instantiate(&mut store, &self.component, linker)
            .map_err(map_trap)?;
        Ok((store, inst))
    }

    /// Search.
    ///
    /// # Errors
    /// A guest error, a denied capability, or a resource limit.
    pub fn search(
        &self,
        settings: &serde_json::Value,
        terms: &PluginSearch,
    ) -> Result<Vec<PluginRelease>> {
        let (mut store, inst) = self.indexer(settings)?;
        let terms = idx::cellarr::plugins::types::SearchTerms {
            queries: terms.queries.clone(),
            ids: terms.ids.clone(),
            numbering: terms.numbering.clone(),
        };
        let r = inst
            .cellarr_plugins_indexer()
            .call_search(&mut store, &settings.to_string(), &terms)
            .map_err(map_trap)?;
        r.map(|v| v.into_iter().map(release_from).collect())
            .map_err(|e| PluginError::Guest(e.message))
    }

    /// Recent releases (RSS).
    ///
    /// # Errors
    /// As [`Plugin::search`].
    pub fn latest(&self, settings: &serde_json::Value) -> Result<Vec<PluginRelease>> {
        let (mut store, inst) = self.indexer(settings)?;
        let r = inst
            .cellarr_plugins_indexer()
            .call_latest(&mut store, &settings.to_string())
            .map_err(map_trap)?;
        r.map(|v| v.into_iter().map(release_from).collect())
            .map_err(|e| PluginError::Guest(e.message))
    }

    /// Check the settings work.
    ///
    /// # Errors
    /// What the plugin reports as wrong.
    pub fn test(&self, settings: &serde_json::Value) -> Result<()> {
        match self.kind() {
            PluginKind::Indexer => {
                let (mut store, inst) = self.indexer(settings)?;
                inst.cellarr_plugins_indexer()
                    .call_test(&mut store, &settings.to_string())
                    .map_err(map_trap)?
                    .map_err(|e| PluginError::Guest(e.message))
            }
            PluginKind::DownloadClient => {
                let (mut store, inst) = self.client(settings)?;
                inst.cellarr_plugins_download_client()
                    .call_test(&mut store, &settings.to_string())
                    .map_err(map_trap)?
                    .map_err(|e| PluginError::Guest(e.message))
            }
        }
    }

    /// Hand a download to the client; returns its id.
    ///
    /// # Errors
    /// As [`Plugin::search`].
    pub fn add(&self, settings: &serde_json::Value, url: &str, category: &str) -> Result<String> {
        let (mut store, inst) = self.client(settings)?;
        inst.cellarr_plugins_download_client()
            .call_add(&mut store, &settings.to_string(), url, category)
            .map_err(map_trap)?
            .map_err(|e| PluginError::Guest(e.message))
    }

    /// A download's status.
    ///
    /// # Errors
    /// As [`Plugin::search`].
    pub fn status(&self, settings: &serde_json::Value, id: &str) -> Result<PluginDownloadStatus> {
        use dlc::exports::cellarr::plugins::download_client::DownloadState as S;
        let (mut store, inst) = self.client(settings)?;
        let s = inst
            .cellarr_plugins_download_client()
            .call_status(&mut store, &settings.to_string(), id)
            .map_err(map_trap)?
            .map_err(|e| PluginError::Guest(e.message))?;
        Ok(PluginDownloadStatus {
            state: match s.state {
                S::Queued => PluginDownloadState::Queued,
                S::Downloading => PluginDownloadState::Downloading,
                S::Completed => PluginDownloadState::Completed,
                S::Failed => PluginDownloadState::Failed,
            },
            progress: s.progress,
            content_path: s.content_path,
            ratio: s.ratio,
            seeding_time_secs: s.seeding_time_secs,
            peers: s.peers,
            error_message: s.error_message,
        })
    }

    /// Remove a download.
    ///
    /// # Errors
    /// As [`Plugin::search`].
    pub fn remove(&self, settings: &serde_json::Value, id: &str, delete_data: bool) -> Result<()> {
        let (mut store, inst) = self.client(settings)?;
        inst.cellarr_plugins_download_client()
            .call_remove(&mut store, &settings.to_string(), id, delete_data)
            .map_err(map_trap)?
            .map_err(|e| PluginError::Guest(e.message))
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        self.epoch_stop.store(true, Ordering::Relaxed);
    }
}

fn release_from(r: idx::cellarr::plugins::types::Release) -> PluginRelease {
    PluginRelease {
        title: r.title,
        download_url: r.download_url,
        guid: r.guid,
        protocol: match r.protocol {
            idx::cellarr::plugins::types::Protocol::Torrent => PluginProtocol::Torrent,
            idx::cellarr::plugins::types::Protocol::Usenet => PluginProtocol::Usenet,
        },
        size: r.size,
        seeders: r.seeders,
        indexer_flags: r.indexer_flags,
    }
}

fn spawn_epoch_ticker(engine: &Engine, timeout: Duration, stop: Arc<AtomicBool>) {
    let engine = engine.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(timeout);
            engine.increment_epoch();
        }
    });
}

/// A fuel/epoch/memory trap is a [`PluginError::ResourceLimit`]; anything else a
/// [`PluginError::Host`].
fn map_trap(e: wasmtime::Error) -> PluginError {
    let msg = format!("{e:#}");
    if let Some(trap) = e.downcast_ref::<wasmtime::Trap>() {
        if matches!(
            trap,
            wasmtime::Trap::OutOfFuel
                | wasmtime::Trap::Interrupt
                | wasmtime::Trap::MemoryOutOfBounds
        ) {
            return PluginError::ResourceLimit(msg);
        }
    }
    let lower = msg.to_lowercase();
    if lower.contains("fuel")
        || lower.contains("epoch")
        || lower.contains("interrupt")
        || lower.contains("limit")
    {
        PluginError::ResourceLimit(msg)
    } else {
        PluginError::Host(msg)
    }
}

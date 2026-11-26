//! The live download-client factory: build a [`DownloadClient`] from a persisted
//! [`DownloadClientConfig`].
//!
//! The pipeline's Grab/Track stages take a single [`cellarr_core::DownloadClient`].
//! A deployment persists [`DownloadClientConfig`] rows (kind + open-ended
//! `settings` JSON: host, credentials, paths) via the db `ConfigRepo`. This module
//! reads the *enabled* client of highest priority at run time and constructs the
//! matching native adapter (qBittorrent / Transmission / Deluge / rTorrent /
//! SABnzbd / NZBGet / Blackhole), wrapping
//! it in [`ConfiguredDownloadClient`] so the runner is driven over one concrete
//! `DownloadClient` whose error type is unified (mirroring the indexer set's
//! `NabAdapter`).
//!
//! Reading the config when the handler is built (rather than caching a live
//! socket) keeps the chosen client in step with CRUD writes: a client added or
//! reconfigured through the API is visible to the next pipeline run with no code
//! change here. Each adapter routes its I/O through the real `reqwest` transport.

use async_trait::async_trait;
use cellarr_core::{DownloadClient, DownloadClientConfig, DownloadStatus, GrabRequest, Protocol};
use cellarr_download::{
    AppleMusicClient, AppleMusicSettings, BlackholeClient, BlackholeSettings, DelugeClient,
    DelugeSettings, DownloadError, NzbgetClient, NzbgetSettings, QbittorrentClient,
    QbittorrentSettings, RtorrentClient, RtorrentSettings, SabnzbdClient, SabnzbdSettings,
    SlskdClient, SlskdSettings, TransmissionClient, TransmissionSettings,
};

/// A failure building a download client from its persisted configuration.
#[derive(Debug, thiserror::Error)]
pub enum DownloadClientFactoryError {
    /// The configured client's `kind` is not one this build supports.
    #[error("unsupported download client kind '{kind}'")]
    UnsupportedKind {
        /// The unrecognized kind string.
        kind: String,
    },

    /// The client's `settings` JSON was missing or malformed for its kind (e.g.
    /// no `baseUrl`/`apiKey`), so no adapter could be built from it.
    #[error("download client '{name}' is misconfigured: {reason}")]
    Misconfigured {
        /// The configured client's name.
        name: String,
        /// Why the adapter could not be built.
        reason: String,
    },
}

/// One built native download-client adapter, dispatched dynamically by kind.
///
/// Implements [`DownloadClient`] with a single unified [`DownloadError`], so the
/// [`PipelineRunner`](cellarr_jobs::PipelineRunner) (generic over `D:
/// DownloadClient`) is driven over this one type regardless of which client was
/// configured.
pub enum ConfiguredDownloadClient {
    /// qBittorrent (torrent, WebUI v2).
    Qbittorrent(QbittorrentClient),
    /// A WASM plugin download client.
    #[cfg(feature = "plugins")]
    Plugin(cellarr_plugins::PluginDownloadClient),
    /// A native Lidarr plugin download client, hosted by the sidecar.
    #[cfg(feature = "plugins")]
    Sidecar(cellarr_plugins::sidecar::SidecarDownloadClient),
    /// Transmission (torrent, RPC).
    Transmission(TransmissionClient),
    /// Deluge (torrent, JSON-RPC WebUI).
    Deluge(DelugeClient),
    /// rTorrent (torrent, XML-RPC).
    Rtorrent(RtorrentClient),
    /// SABnzbd (Usenet).
    Sabnzbd(SabnzbdClient),
    /// NZBGet (Usenet, JSON-RPC).
    Nzbget(NzbgetClient),
    /// Blackhole / watch-folder (universal).
    Blackhole(BlackholeClient),
    /// Soulseek through slskd's own HTTP API (native; no sidecar).
    Slskd(SlskdClient),
    /// Apple Music: catalogue lookups natively, audio through the decryptor service.
    AppleMusic(AppleMusicClient),
    /// Several eligible clients, in priority order; each release is handed to
    /// the one that fits it (see [`choose_client`]).
    Routed(Box<RoutedClients>),
    /// Stands in when every eligible client failed to build (an unsupported or
    /// misconfigured kind). Jobs that never grab (a library rescan) still run;
    /// anything that does call it gets the build error back.
    Unavailable(String),
}

/// What decides whether a client can take a given release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTraits {
    /// The protocol the client is configured for.
    pub protocol: Protocol,
    /// `Some(protocol as the sidecar names it)` for a sidecar-hosted plugin
    /// client (empty when unknown), `None` for a native client.
    pub sidecar_wire: Option<String>,
}

/// What decides which client a release goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseTraits {
    /// The release's protocol as cellarr models it.
    pub protocol: Protocol,
    /// `None` for a release from an ordinary indexer; `Some(protocol as the
    /// sidecar names it)` (empty when unknown) for one only the sidecar can fetch.
    pub sidecar_wire: Option<String>,
}

fn is_standard_wire(w: &str) -> bool {
    w.eq_ignore_ascii_case("torrent") || w.eq_ignore_ascii_case("usenet")
}

/// Pick the client for one release from `clients`, given in priority order.
///
/// A release in a plugin's own protocol goes to a sidecar client of that
/// protocol; a release only the sidecar can fetch goes to any sidecar client; an
/// ordinary release goes to the first client whose protocol matches (a sidecar
/// client in a non-standard protocol never takes one). When nothing fits, the
/// highest-priority client is the fallback, so a single-client setup behaves as
/// before. `None` only for an empty list.
#[must_use]
pub fn choose_client(clients: &[ClientTraits], release: &ReleaseTraits) -> Option<usize> {
    let first = |f: &dyn Fn(&ClientTraits) -> bool| clients.iter().position(f);
    let by_wire = |c: &ClientTraits, w: &str| {
        c.sidecar_wire
            .as_deref()
            .is_some_and(|cw| cw.eq_ignore_ascii_case(w))
    };
    let standard_fit = |c: &ClientTraits| match c.sidecar_wire.as_deref() {
        Some(w) if is_standard_wire(w) => {
            let p = if w.eq_ignore_ascii_case("usenet") {
                Protocol::Usenet
            } else {
                Protocol::Torrent
            };
            p == release.protocol
        }
        Some(w) if w.is_empty() => c.protocol == release.protocol,
        Some(_) => false,
        None => c.protocol == release.protocol,
    };
    let fit = match release.sidecar_wire.as_deref() {
        Some(w) if !w.is_empty() && !is_standard_wire(w) => first(&|c| by_wire(c, w)),
        Some(w) if w.is_empty() => first(&|c| c.sidecar_wire.is_some()),
        _ => first(&standard_fit),
    };
    fit.or(if clients.is_empty() { None } else { Some(0) })
}

/// Clients tried per release by [`choose_client`]; `status`/`remove` ask them in
/// priority order, skipping those that do not know the download.
pub struct RoutedClients {
    clients: Vec<(ClientTraits, ConfiguredDownloadClient)>,
}

impl RoutedClients {
    fn pick(&self, release: &cellarr_core::Release) -> &ConfiguredDownloadClient {
        let traits: Vec<ClientTraits> = self.clients.iter().map(|(t, _)| t.clone()).collect();
        let want = ReleaseTraits {
            protocol: release.protocol,
            sidecar_wire: release_sidecar_wire(release),
        };
        let i = choose_client(&traits, &want).unwrap_or(0);
        &self.clients[i].1
    }
}

/// A Soulseek release (a `slskd:` link) routes by its own protocol name, the same
/// way a plugin protocol does, so only a Soulseek client takes it.
#[cfg(feature = "plugins")]
fn release_sidecar_wire(release: &cellarr_core::Release) -> Option<String> {
    if cellarr_download::slskd::is_slskd_url(&release.download_url) {
        return Some(cellarr_download::slskd::SOULSEEK_PROTOCOL.to_string());
    }
    if cellarr_download::applemusic::is_apple_url(&release.download_url) {
        return Some(cellarr_download::applemusic::APPLE_PROTOCOL.to_string());
    }
    cellarr_plugins::sidecar::global().and_then(|s| s.release_wire_protocol(release))
}

#[cfg(not(feature = "plugins"))]
fn release_sidecar_wire(release: &cellarr_core::Release) -> Option<String> {
    if cellarr_download::applemusic::is_apple_url(&release.download_url) {
        return Some(cellarr_download::applemusic::APPLE_PROTOCOL.to_string());
    }
    cellarr_download::slskd::is_slskd_url(&release.download_url)
        .then(|| cellarr_download::slskd::SOULSEEK_PROTOCOL.to_string())
}

impl ConfiguredDownloadClient {
    /// Build every config (given in priority order) and route per release. A
    /// config that cannot be built is skipped with a warning while another can;
    /// when none can, the first build error is returned. One usable client is
    /// returned bare.
    ///
    /// # Errors
    /// The first build error when no config builds.
    pub fn routed(configs: &[DownloadClientConfig]) -> Result<Self, DownloadClientFactoryError> {
        let mut built = Vec::new();
        let mut first_err = None;
        for c in configs {
            match Self::from_config(c) {
                Ok(client) => built.push((client.traits(c), client)),
                Err(e) => {
                    tracing::warn!(client = %c.name, error = %e, "skipping download client that cannot be built");
                    first_err.get_or_insert(e);
                }
            }
        }
        match built.len() {
            0 => Err(
                first_err.unwrap_or(DownloadClientFactoryError::UnsupportedKind {
                    kind: "none".to_string(),
                }),
            ),
            1 => Ok(built.remove(0).1),
            _ => Ok(Self::Routed(Box::new(RoutedClients { clients: built }))),
        }
    }

    /// Like [`routed`](Self::routed), but when no config builds, return a
    /// [`Unavailable`](Self::Unavailable) stand-in (after a warning) instead of an
    /// error, for jobs that never grab.
    #[must_use]
    pub fn routed_or_unavailable(configs: &[DownloadClientConfig]) -> Self {
        match Self::routed(configs) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "no download client could be built; continuing without one");
                Self::Unavailable(e.to_string())
            }
        }
    }

    fn traits(&self, config: &DownloadClientConfig) -> ClientTraits {
        if matches!(self, Self::Slskd(_)) {
            return ClientTraits {
                protocol: config.protocol,
                sidecar_wire: Some(cellarr_download::slskd::SOULSEEK_PROTOCOL.to_string()),
            };
        }
        if matches!(self, Self::AppleMusic(_)) {
            return ClientTraits {
                protocol: config.protocol,
                sidecar_wire: Some(cellarr_download::applemusic::APPLE_PROTOCOL.to_string()),
            };
        }
        #[cfg(feature = "plugins")]
        if let Self::Sidecar(c) = self {
            return ClientTraits {
                protocol: config.protocol,
                sidecar_wire: Some(c.wire_protocol().unwrap_or_default()),
            };
        }
        ClientTraits {
            protocol: config.protocol,
            sidecar_wire: None,
        }
    }

    /// Build the adapter for one [`DownloadClientConfig`], reading its open-ended
    /// `settings` JSON (the shape the API shim persists) into the kind's typed
    /// settings struct.
    ///
    /// # Errors
    /// [`DownloadClientFactoryError::UnsupportedKind`] for a kind this build does
    /// not ship; [`DownloadClientFactoryError::Misconfigured`] when the settings
    /// JSON does not deserialize for the chosen kind.
    pub fn from_config(config: &DownloadClientConfig) -> Result<Self, DownloadClientFactoryError> {
        let kind = config.kind.to_ascii_lowercase();
        let name = config.name.clone();
        let category = config.category.clone();
        let misconfigured = |reason: String| DownloadClientFactoryError::Misconfigured {
            name: name.clone(),
            reason,
        };
        match kind.as_str() {
            "qbittorrent" | "qbit" => {
                let settings: QbittorrentSettings =
                    parse_settings(&with_qbittorrent_url(config)).map_err(&misconfigured)?;
                Ok(Self::Qbittorrent(QbittorrentClient::new(
                    name, settings, category,
                )))
            }
            "transmission" | "transmissionbt" => {
                let settings: TransmissionSettings =
                    parse_settings(config).map_err(&misconfigured)?;
                Ok(Self::Transmission(TransmissionClient::new(
                    name, settings, category,
                )))
            }
            "deluge" => {
                let settings: DelugeSettings = parse_settings(config).map_err(&misconfigured)?;
                Ok(Self::Deluge(DelugeClient::new(name, settings, category)))
            }
            "rtorrent" => {
                let settings: RtorrentSettings = parse_settings(config).map_err(&misconfigured)?;
                Ok(Self::Rtorrent(RtorrentClient::new(
                    name, settings, category,
                )))
            }
            "sabnzbd" | "sab" => {
                let settings: SabnzbdSettings = parse_settings(config).map_err(&misconfigured)?;
                Ok(Self::Sabnzbd(SabnzbdClient::new(name, settings, category)))
            }
            "nzbget" => {
                let settings: NzbgetSettings = parse_settings(config).map_err(&misconfigured)?;
                Ok(Self::Nzbget(NzbgetClient::new(name, settings, category)))
            }
            "blackhole" => {
                let settings: BlackholeSettings = parse_settings(config).map_err(&misconfigured)?;
                // The blackhole writes either `.torrent`/`.nzb`/`.magnet` jobs; the
                // protocol it stamps onto a job comes from the client config (a
                // torrent blackhole vs a usenet one).
                Ok(Self::Blackhole(BlackholeClient::new(
                    name,
                    settings,
                    category,
                    config.protocol,
                )))
            }
            // Native Soulseek, when the row names slskd's URL and key. A row the
            // native client cannot be built from (or one that says `native: false`)
            // falls through to the Tubifarry plugin in the sidecar below.
            other
                if cellarr_download::slskd::is_slskd_kind(other)
                    && SlskdSettings::from_json(&config.settings).is_ok() =>
            {
                let settings =
                    SlskdSettings::from_json(&config.settings).map_err(&misconfigured)?;
                Ok(Self::Slskd(SlskdClient::new(name, settings)))
            }
            // Native Apple Music, when the row names a developer token, a
            // Music-User-Token and the decryptor's URL. Anything less (or
            // `native: false`) falls through to the AppleMusicarr plugin.
            other
                if cellarr_download::applemusic::is_apple_kind(other)
                    && AppleMusicSettings::from_json_client(&config.settings).is_ok() =>
            {
                let settings = AppleMusicSettings::from_json_client(&config.settings)
                    .map_err(&misconfigured)?;
                Ok(Self::AppleMusic(AppleMusicClient::new(name, settings)))
            }
            #[cfg(feature = "plugins")]
            other
                if cellarr_plugins::sidecar::global().is_some()
                    && cellarr_plugins::catalog::find(other).is_some_and(|e| {
                        e.runtime == "dotnet"
                            && e.kind == cellarr_plugins::catalog::EntryKind::DownloadClient
                    }) =>
            {
                let entry = cellarr_plugins::catalog::find(other).expect("checked above");
                let sidecar = cellarr_plugins::sidecar::global().expect("checked above");
                Ok(Self::Sidecar(
                    cellarr_plugins::sidecar::SidecarDownloadClient::new(
                        sidecar,
                        entry.name,
                        config.settings.clone(),
                        format!("cellarr:{}", config.id),
                        name,
                    ),
                ))
            }
            #[cfg(feature = "plugins")]
            other
                if cellarr_plugins::registry::global()
                    .get(other)
                    .is_some_and(|p| p.kind() == cellarr_plugins::PluginKind::DownloadClient) =>
            {
                let plugin = cellarr_plugins::registry::global()
                    .get(other)
                    .expect("checked above");
                Ok(Self::Plugin(cellarr_plugins::PluginDownloadClient::new(
                    plugin,
                    config.settings.clone(),
                    category,
                    name,
                )))
            }
            other => Err(DownloadClientFactoryError::UnsupportedKind {
                kind: other.to_string(),
            }),
        }
    }
}

/// Deserialize a client's open-ended `settings` JSON into its typed struct,
/// rendering a readable reason on failure.
/// The settings of a qBittorrent client with `base_url` filled in.
///
/// The *arr apps (Lidarr included) describe a qBittorrent client as
/// `host`/`port`/`useSsl`/`urlBase`, and that is how a config written through the
/// compatible API is stored. The adapter wants one URL; derive it here so the stored
/// form stays exactly what the app's own client would read back.
fn with_qbittorrent_url(config: &DownloadClientConfig) -> DownloadClientConfig {
    let mut config = config.clone();
    if let Some(o) = config.settings.as_object_mut() {
        if !o.contains_key("base_url") {
            if let Some(host) = o
                .get("host")
                .and_then(|v| v.as_str())
                .filter(|h| !h.is_empty())
            {
                let scheme = if o.get("useSsl").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "https"
                } else {
                    "http"
                };
                let port = o.get("port").and_then(|v| v.as_u64()).unwrap_or(8080);
                let base = o
                    .get("urlBase")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim_matches('/');
                let url = if base.is_empty() {
                    format!("{scheme}://{host}:{port}")
                } else {
                    format!("{scheme}://{host}:{port}/{base}")
                };
                o.insert("base_url".into(), serde_json::Value::String(url));
            }
        }
    }
    config
}

fn parse_settings<T: serde::de::DeserializeOwned>(
    config: &DownloadClientConfig,
) -> Result<T, String> {
    serde_json::from_value(config.settings.clone())
        .map_err(|e| format!("settings JSON does not match {} schema: {e}", config.kind))
}

#[async_trait]
impl DownloadClient for ConfiguredDownloadClient {
    type Error = DownloadError;

    fn name(&self) -> &str {
        match self {
            Self::Qbittorrent(c) => <QbittorrentClient as DownloadClient>::name(c),
            Self::Transmission(c) => <TransmissionClient as DownloadClient>::name(c),
            Self::Deluge(c) => <DelugeClient as DownloadClient>::name(c),
            Self::Rtorrent(c) => <RtorrentClient as DownloadClient>::name(c),
            Self::Sabnzbd(c) => <SabnzbdClient as DownloadClient>::name(c),
            Self::Nzbget(c) => <NzbgetClient as DownloadClient>::name(c),
            Self::Blackhole(c) => <BlackholeClient as DownloadClient>::name(c),
            Self::Slskd(c) => <SlskdClient as DownloadClient>::name(c),
            Self::AppleMusic(c) => <AppleMusicClient as DownloadClient>::name(c),
            Self::Routed(r) => r.clients[0].1.name(),
            Self::Unavailable(_) => "unavailable",
            #[cfg(feature = "plugins")]
            Self::Plugin(c) => DownloadClient::name(c),
            #[cfg(feature = "plugins")]
            Self::Sidecar(c) => DownloadClient::name(c),
        }
    }

    async fn add(&self, grab: &GrabRequest) -> Result<String, Self::Error> {
        // Fully-qualified trait calls: each concrete client has inherent methods
        // of the same name (`status`/`remove` with a richer signature), so we must
        // dispatch through the `DownloadClient` trait impl in `cellarr-download`
        // (which projects the rich progress onto the core `DownloadStatus`).
        match self {
            Self::Qbittorrent(c) => DownloadClient::add(c, grab).await,
            Self::Transmission(c) => DownloadClient::add(c, grab).await,
            Self::Deluge(c) => DownloadClient::add(c, grab).await,
            Self::Rtorrent(c) => DownloadClient::add(c, grab).await,
            Self::Sabnzbd(c) => DownloadClient::add(c, grab).await,
            Self::Nzbget(c) => DownloadClient::add(c, grab).await,
            Self::Blackhole(c) => DownloadClient::add(c, grab).await,
            Self::Slskd(c) => DownloadClient::add(c, grab).await,
            Self::AppleMusic(c) => DownloadClient::add(c, grab).await,
            Self::Routed(r) => Box::pin(DownloadClient::add(r.pick(&grab.release), grab)).await,
            Self::Unavailable(why) => Err(DownloadError::Config(why.clone())),
            #[cfg(feature = "plugins")]
            Self::Plugin(c) => DownloadClient::add(c, grab)
                .await
                .map_err(|e| DownloadError::Api(format!("plugin: {e}"))),
            #[cfg(feature = "plugins")]
            Self::Sidecar(c) => DownloadClient::add(c, grab)
                .await
                .map_err(|e| DownloadError::Api(format!("lidarr sidecar: {e}"))),
        }
    }

    async fn status(&self, download_id: &str) -> Result<DownloadStatus, Self::Error> {
        match self {
            Self::Qbittorrent(c) => DownloadClient::status(c, download_id).await,
            Self::Transmission(c) => DownloadClient::status(c, download_id).await,
            Self::Deluge(c) => DownloadClient::status(c, download_id).await,
            Self::Rtorrent(c) => DownloadClient::status(c, download_id).await,
            Self::Sabnzbd(c) => DownloadClient::status(c, download_id).await,
            Self::Nzbget(c) => DownloadClient::status(c, download_id).await,
            Self::Blackhole(c) => DownloadClient::status(c, download_id).await,
            Self::Slskd(c) => DownloadClient::status(c, download_id).await,
            Self::AppleMusic(c) => DownloadClient::status(c, download_id).await,
            Self::Routed(r) => {
                let mut missing = None;
                for (_, c) in &r.clients {
                    match Box::pin(DownloadClient::status(c, download_id)).await {
                        Err(DownloadError::NotFound(id)) => missing = Some(id),
                        other => return other,
                    }
                }
                Err(DownloadError::NotFound(missing.unwrap_or_default()))
            }
            Self::Unavailable(why) => Err(DownloadError::Config(why.clone())),
            #[cfg(feature = "plugins")]
            Self::Plugin(c) => DownloadClient::status(c, download_id)
                .await
                .map_err(|e| DownloadError::Api(format!("plugin: {e}"))),
            #[cfg(feature = "plugins")]
            Self::Sidecar(c) => DownloadClient::status(c, download_id)
                .await
                .map_err(|e| DownloadError::Api(format!("lidarr sidecar: {e}"))),
        }
    }

    async fn remove(&self, download_id: &str, delete_data: bool) -> Result<(), Self::Error> {
        match self {
            Self::Qbittorrent(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Transmission(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Deluge(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Rtorrent(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Sabnzbd(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Nzbget(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Blackhole(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Slskd(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::AppleMusic(c) => DownloadClient::remove(c, download_id, delete_data).await,
            Self::Routed(r) => {
                let mut missing = None;
                for (_, c) in &r.clients {
                    match Box::pin(DownloadClient::remove(c, download_id, delete_data)).await {
                        Err(DownloadError::NotFound(id)) => missing = Some(id),
                        other => return other,
                    }
                }
                Err(DownloadError::NotFound(missing.unwrap_or_default()))
            }
            Self::Unavailable(why) => Err(DownloadError::Config(why.clone())),
            #[cfg(feature = "plugins")]
            Self::Plugin(c) => DownloadClient::remove(c, download_id, delete_data)
                .await
                .map_err(|e| DownloadError::Api(format!("plugin: {e}"))),
            #[cfg(feature = "plugins")]
            Self::Sidecar(c) => DownloadClient::remove(c, download_id, delete_data)
                .await
                .map_err(|e| DownloadError::Api(format!("lidarr sidecar: {e}"))),
        }
    }
}

/// A download client that does nothing and is never driven.
///
/// The interactive release-search preview (`GET /api/v3/release`) runs the
/// read-only Discover→Decide path through the [`PipelineRunner`](cellarr_jobs::PipelineRunner),
/// which is generic over a `D: DownloadClient` but **never calls** it for a
/// preview (the preview stops before Grab). The runner still needs *a* client
/// value to construct, so the search path supplies this no-op rather than
/// building a live adapter — which means a misconfigured/unreachable download
/// client can never fail an interactive search (a search never grabs).
///
/// Every method returns a [`DownloadError::Config`] error so that, were it ever
/// driven by mistake, the failure is loud and self-describing rather than a
/// silent success.
pub struct NoopDownloadClient;

#[async_trait]
impl DownloadClient for NoopDownloadClient {
    type Error = DownloadError;

    fn name(&self) -> &str {
        "noop (interactive search; never grabs)"
    }

    async fn add(&self, _grab: &GrabRequest) -> Result<String, Self::Error> {
        Err(DownloadError::Config(
            "interactive release search must not grab".into(),
        ))
    }

    async fn status(&self, _download_id: &str) -> Result<DownloadStatus, Self::Error> {
        Err(DownloadError::Config(
            "interactive release search must not track".into(),
        ))
    }

    async fn remove(&self, _download_id: &str, _delete_data: bool) -> Result<(), Self::Error> {
        Err(DownloadError::Config(
            "interactive release search must not remove a download".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cellarr_core::{DownloadClientId, Protocol};
    use serde_json::json;

    fn config(kind: &str, settings: serde_json::Value) -> DownloadClientConfig {
        DownloadClientConfig {
            tags: Vec::new(),
            id: DownloadClientId::new(),
            name: format!("test-{kind}"),
            kind: kind.to_string(),
            protocol: Protocol::Torrent,
            enabled: true,
            priority: 0,
            category: "cellarr".to_string(),
            settings,
        }
    }

    #[test]
    fn builds_qbittorrent_from_settings() {
        let c = config(
            "qbittorrent",
            json!({"base_url": "http://localhost:8080", "username": "u", "password": "p"}),
        );
        let built = ConfiguredDownloadClient::from_config(&c).expect("qbit builds");
        assert!(matches!(built, ConfiguredDownloadClient::Qbittorrent(_)));
        assert_eq!(DownloadClient::name(&built), "test-qbittorrent");
    }

    #[test]
    fn builds_transmission_from_settings() {
        let c = config(
            "transmission",
            json!({"host": "localhost", "port": 9091, "category": "cellarr-tv"}),
        );
        let built = ConfiguredDownloadClient::from_config(&c).expect("transmission builds");
        assert!(matches!(built, ConfiguredDownloadClient::Transmission(_)));
        assert_eq!(DownloadClient::name(&built), "test-transmission");
    }

    #[test]
    fn builds_transmission_from_base_url() {
        // The simpler base_url shape (no host/port) is also accepted.
        let c = config("transmission", json!({"base_url": "http://localhost:9091"}));
        let built = ConfiguredDownloadClient::from_config(&c).expect("transmission builds");
        assert!(matches!(built, ConfiguredDownloadClient::Transmission(_)));
    }

    #[test]
    fn builds_blackhole_carrying_protocol() {
        let c = config(
            "blackhole",
            json!({"watch_folder": "/w", "completed_folder": "/c"}),
        );
        let built = ConfiguredDownloadClient::from_config(&c).expect("blackhole builds");
        assert!(matches!(built, ConfiguredDownloadClient::Blackhole(_)));
    }

    #[test]
    fn builds_deluge_from_settings() {
        let c = config(
            "deluge",
            json!({"host": "localhost", "port": 8112, "password": "secret"}),
        );
        let built = ConfiguredDownloadClient::from_config(&c).expect("deluge builds");
        assert!(matches!(built, ConfiguredDownloadClient::Deluge(_)));
        assert_eq!(DownloadClient::name(&built), "test-deluge");
    }

    #[test]
    fn builds_rtorrent_from_settings() {
        let c = config(
            "rtorrent",
            json!({"host": "localhost", "port": 8080, "urlBase": "/RPC2"}),
        );
        let built = ConfiguredDownloadClient::from_config(&c).expect("rtorrent builds");
        assert!(matches!(built, ConfiguredDownloadClient::Rtorrent(_)));
        assert_eq!(DownloadClient::name(&built), "test-rtorrent");
    }

    #[test]
    fn unsupported_kind_is_rejected() {
        let c = config("aria2", json!({}));
        // `ConfiguredDownloadClient` is not `Debug` (it holds live transports), so
        // match the result directly rather than `expect_err`.
        assert!(matches!(
            ConfiguredDownloadClient::from_config(&c),
            Err(DownloadClientFactoryError::UnsupportedKind { .. })
        ));
    }

    #[tokio::test]
    async fn only_unbuildable_clients_give_an_unavailable_stand_in() {
        let bad = config("applemusicarrdownloadclient", json!({}));
        let c = ConfiguredDownloadClient::routed_or_unavailable(&[bad]);
        assert!(matches!(c, ConfiguredDownloadClient::Unavailable(_)));
        assert!(DownloadClient::status(&c, "x").await.is_err());
    }

    #[test]
    fn an_unbuildable_client_is_skipped_beside_a_good_one() {
        let bad = config("applemusicarrdownloadclient", json!({}));
        let good = config(
            "blackhole",
            json!({"watch_folder": "/tmp/w", "completed_folder": "/tmp/c"}),
        );
        let c = ConfiguredDownloadClient::routed_or_unavailable(&[bad, good]);
        assert!(!matches!(c, ConfiguredDownloadClient::Unavailable(_)));
    }

    #[test]
    fn malformed_settings_are_misconfigured() {
        // qbittorrent settings require base_url/username/password.
        let c = config("qbittorrent", json!({"base_url": "http://x"}));
        assert!(matches!(
            ConfiguredDownloadClient::from_config(&c),
            Err(DownloadClientFactoryError::Misconfigured { .. })
        ));
    }

    fn native(p: Protocol) -> ClientTraits {
        ClientTraits {
            protocol: p,
            sidecar_wire: None,
        }
    }
    fn plugin(w: &str) -> ClientTraits {
        ClientTraits {
            protocol: Protocol::Torrent,
            sidecar_wire: Some(w.to_string()),
        }
    }
    fn rel(p: Protocol, w: Option<&str>) -> ReleaseTraits {
        ReleaseTraits {
            protocol: p,
            sidecar_wire: w.map(str::to_string),
        }
    }

    #[test]
    fn a_usenet_release_skips_a_higher_priority_torrent_client() {
        let c = [native(Protocol::Torrent), native(Protocol::Usenet)];
        assert_eq!(choose_client(&c, &rel(Protocol::Usenet, None)), Some(1));
        assert_eq!(choose_client(&c, &rel(Protocol::Torrent, None)), Some(0));
    }

    #[test]
    fn a_slskd_row_with_url_and_key_builds_the_native_client() {
        let c = ConfiguredDownloadClient::from_config(&config(
            "slskd",
            json!({"baseUrl": "http://slskd:5030", "apiKey": "k"}),
        ))
        .unwrap();
        assert!(matches!(c, ConfiguredDownloadClient::Slskd(_)));
        let t = c.traits(&config("slskd", json!({})));
        assert_eq!(t.sidecar_wire.as_deref(), Some("SoulseekDownloadProtocol"));
    }

    #[test]
    fn a_tubifarry_class_name_row_builds_the_native_client() {
        let c = ConfiguredDownloadClient::from_config(&config(
            "slskdclient",
            json!({"baseUrl": "http://slskd:5030", "apiKey": "k"}),
        ))
        .unwrap();
        assert!(matches!(c, ConfiguredDownloadClient::Slskd(_)));
    }

    #[test]
    fn a_slskd_row_without_a_key_or_asking_for_the_plugin_is_not_native() {
        for settings in [
            json!({"baseUrl": "http://slskd:5030"}),
            json!({"baseUrl": "http://slskd:5030", "apiKey": "k", "native": false}),
        ] {
            // No sidecar in this process: it is unsupported, i.e. it took the
            // plugin path rather than the native one.
            let r = ConfiguredDownloadClient::from_config(&config("slskd", settings));
            assert!(
                matches!(r, Err(DownloadClientFactoryError::UnsupportedKind { .. })),
                "{:?}",
                r.err()
            );
        }
    }

    #[test]
    fn a_slskd_link_routes_to_the_native_client_below_a_torrent_client() {
        let rel = cellarr_core::Release {
            indexer_id: cellarr_core::IndexerId::new(),
            title: "A - B [FLAC]".into(),
            download_url: "slskd:abc".into(),
            guid: None,
            protocol: Protocol::Torrent,
            size: None,
            seeders: None,
            indexer_flags: vec![],
        };
        let wire = release_sidecar_wire(&rel);
        assert_eq!(wire.as_deref(), Some("SoulseekDownloadProtocol"));
        let c = [
            native(Protocol::Torrent),
            plugin("SoulseekDownloadProtocol"),
        ];
        let r = ReleaseTraits {
            protocol: Protocol::Torrent,
            sidecar_wire: wire,
        };
        assert_eq!(choose_client(&c, &r), Some(1));
        // And an ordinary torrent never goes to the Soulseek client.
        assert_eq!(choose_client(&c, &rel_plain()), Some(0));
    }

    fn apple_client_row() -> serde_json::Value {
        json!({"developerToken": "dev-placeholder", "musicUserToken": "user-placeholder",
               "decryptorUrl": "http://decryptor:8080"})
    }

    #[test]
    fn an_apple_row_with_its_fields_builds_the_native_client_under_any_plugin_kind() {
        for kind in [
            "applemusicarr",
            "applemusicarrdownloadclient",
            "AppleMusicarr",
        ] {
            let c =
                ConfiguredDownloadClient::from_config(&config(kind, apple_client_row())).unwrap();
            assert!(
                matches!(c, ConfiguredDownloadClient::AppleMusic(_)),
                "{kind}"
            );
            let t = c.traits(&config(kind, json!({})));
            assert_eq!(
                t.sidecar_wire.as_deref(),
                Some("AppleMusicarrDownloadProtocol")
            );
        }
    }

    #[test]
    fn an_apple_row_missing_a_field_or_asking_for_the_plugin_is_not_native() {
        let mut no_decryptor = apple_client_row();
        no_decryptor["decryptorUrl"] = json!("");
        let mut no_user = apple_client_row();
        no_user["musicUserToken"] = json!("");
        let mut plugin = apple_client_row();
        plugin["native"] = json!(false);
        for settings in [json!({}), no_decryptor, no_user, plugin] {
            // No sidecar in this process: unsupported means it took the plugin
            // path rather than the native one.
            let r = ConfiguredDownloadClient::from_config(&config(
                "applemusicarrdownloadclient",
                settings,
            ));
            assert!(
                matches!(r, Err(DownloadClientFactoryError::UnsupportedKind { .. })),
                "{:?}",
                r.err()
            );
        }
    }

    #[test]
    fn an_apple_link_routes_only_to_an_apple_client() {
        let rel = cellarr_core::Release {
            indexer_id: cellarr_core::IndexerId::new(),
            title: "A - B [AAC] [256]".into(),
            download_url: "applemusicarr://album/1?quality=aac256".into(),
            guid: None,
            protocol: Protocol::Torrent,
            size: None,
            seeders: None,
            indexer_flags: vec![],
        };
        let wire = release_sidecar_wire(&rel);
        assert_eq!(wire.as_deref(), Some("AppleMusicarrDownloadProtocol"));
        let c = [
            native(Protocol::Torrent),
            plugin("AppleMusicarrDownloadProtocol"),
        ];
        let r = ReleaseTraits {
            protocol: Protocol::Torrent,
            sidecar_wire: wire,
        };
        assert_eq!(choose_client(&c, &r), Some(1));
        assert_eq!(choose_client(&c, &rel_plain()), Some(0));
    }

    fn rel_plain() -> ReleaseTraits {
        rel(Protocol::Torrent, None)
    }

    #[test]
    fn a_plugin_protocol_release_goes_to_the_matching_sidecar_client() {
        let c = [
            native(Protocol::Torrent),
            plugin("AppleMusicDownloadProtocol"),
            plugin("SoulseekDownloadProtocol"),
        ];
        let r = rel(Protocol::Torrent, Some("SoulseekDownloadProtocol"));
        assert_eq!(choose_client(&c, &r), Some(2));
    }

    #[test]
    fn an_ordinary_release_never_goes_to_a_plugin_protocol_client() {
        let c = [
            plugin("SoulseekDownloadProtocol"),
            native(Protocol::Torrent),
        ];
        assert_eq!(choose_client(&c, &rel(Protocol::Torrent, None)), Some(1));
    }

    #[test]
    fn a_sidecar_only_release_with_unknown_protocol_takes_any_sidecar_client() {
        let c = [
            native(Protocol::Torrent),
            plugin("SoulseekDownloadProtocol"),
        ];
        assert_eq!(
            choose_client(&c, &rel(Protocol::Torrent, Some(""))),
            Some(1)
        );
    }

    #[test]
    fn sidecar_torrent_client_takes_a_torrent_release_by_priority() {
        let c = [plugin("torrent"), native(Protocol::Torrent)];
        assert_eq!(
            choose_client(&c, &rel(Protocol::Torrent, Some("torrent"))),
            Some(0)
        );
    }

    #[test]
    fn nothing_fitting_falls_back_to_the_top_priority_client() {
        let c = [native(Protocol::Torrent)];
        assert_eq!(choose_client(&c, &rel(Protocol::Usenet, None)), Some(0));
        assert_eq!(
            choose_client(&c, &rel(Protocol::Torrent, Some("YoutubeDownloadProtocol"))),
            Some(0)
        );
        assert_eq!(choose_client(&[], &rel(Protocol::Torrent, None)), None);
    }

    #[test]
    fn routed_builds_all_skips_unbuildable_and_returns_one_bare() {
        let ok = |p: Protocol, name: &str| {
            let mut c = config(
                "blackhole",
                json!({"watch_folder": "/tmp/w", "completed_folder": "/tmp/c"}),
            );
            c.protocol = p;
            c.name = name.to_string();
            c
        };
        let bad = config("nonsense-kind", json!({}));
        let one = ConfiguredDownloadClient::routed(&[bad.clone(), ok(Protocol::Torrent, "a")]);
        assert!(matches!(one, Ok(ConfiguredDownloadClient::Blackhole(_))));
        let two = ConfiguredDownloadClient::routed(&[
            ok(Protocol::Torrent, "a"),
            ok(Protocol::Usenet, "b"),
        ])
        .unwrap();
        assert!(matches!(two, ConfiguredDownloadClient::Routed(_)));
        assert!(ConfiguredDownloadClient::routed(&[bad]).is_err());
    }
}

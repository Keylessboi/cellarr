//! Plugins as the core seams: a plugin indexer *is* an [`Indexer`], a plugin
//! download client *is* a [`DownloadClient`], so the pipeline needs no special
//! case for them. Guest calls are synchronous and may block on HTTP, so each runs
//! on a blocking thread.

use std::sync::Arc;

use async_trait::async_trait;
use cellarr_core::ids::IndexerId;
use cellarr_core::release::{Protocol, Release};
use cellarr_core::traits::{DownloadClient, DownloadState, DownloadStatus, Indexer, SearchTerms};
use cellarr_core::GrabRequest;

use crate::error::PluginError;
use crate::host::{Plugin, PluginDownloadState, PluginProtocol, PluginSearch};

async fn blocking<T, F>(f: F) -> Result<T, PluginError>
where
    F: FnOnce() -> Result<T, PluginError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| PluginError::Host(format!("plugin call panicked: {e}")))?
}

/// A configured plugin indexer.
pub struct PluginIndexer {
    plugin: Arc<Plugin>,
    settings: serde_json::Value,
    indexer_id: IndexerId,
    name: String,
}

impl PluginIndexer {
    /// `plugin` with one indexer's `settings`; releases are stamped with `indexer_id`.
    #[must_use]
    pub fn new(
        plugin: Arc<Plugin>,
        settings: serde_json::Value,
        indexer_id: IndexerId,
        name: String,
    ) -> Self {
        Self {
            plugin,
            settings,
            indexer_id,
            name,
        }
    }

    fn release(&self, r: crate::host::PluginRelease) -> Release {
        Release {
            indexer_id: self.indexer_id,
            title: r.title,
            download_url: r.download_url,
            guid: r.guid,
            protocol: match r.protocol {
                PluginProtocol::Torrent => Protocol::Torrent,
                PluginProtocol::Usenet => Protocol::Usenet,
            },
            size: r.size,
            seeders: r.seeders,
            indexer_flags: r.indexer_flags,
        }
    }
}

#[async_trait]
impl Indexer for PluginIndexer {
    type Error = PluginError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, terms: &SearchTerms) -> Result<Vec<Release>, PluginError> {
        let plugin = Arc::clone(&self.plugin);
        let settings = self.settings.clone();
        let mut ids = terms.ids.clone();
        ids.extend(
            terms
                .categories
                .iter()
                .map(|c| ("cat".to_string(), c.to_string())),
        );
        let search = PluginSearch {
            queries: terms.queries.clone(),
            ids,
            numbering: terms.numbering.clone(),
        };
        let found = blocking(move || plugin.search(&settings, &search)).await?;
        Ok(found.into_iter().map(|r| self.release(r)).collect())
    }

    async fn latest(&self) -> Result<Vec<Release>, PluginError> {
        let plugin = Arc::clone(&self.plugin);
        let settings = self.settings.clone();
        let found = blocking(move || plugin.latest(&settings)).await?;
        Ok(found.into_iter().map(|r| self.release(r)).collect())
    }
}

/// A configured plugin download client.
pub struct PluginDownloadClient {
    plugin: Arc<Plugin>,
    settings: serde_json::Value,
    category: String,
    name: String,
}

impl PluginDownloadClient {
    /// `plugin` with one client's `settings`, tagging downloads with `category`.
    #[must_use]
    pub fn new(
        plugin: Arc<Plugin>,
        settings: serde_json::Value,
        category: String,
        name: String,
    ) -> Self {
        Self {
            plugin,
            settings,
            category,
            name,
        }
    }
}

#[async_trait]
impl DownloadClient for PluginDownloadClient {
    type Error = PluginError;

    fn name(&self) -> &str {
        &self.name
    }

    async fn add(&self, grab: &GrabRequest) -> Result<String, PluginError> {
        let plugin = Arc::clone(&self.plugin);
        let settings = self.settings.clone();
        let url = grab.release.download_url.clone();
        let category = if grab.category.is_empty() {
            self.category.clone()
        } else {
            grab.category.clone()
        };
        blocking(move || plugin.add(&settings, &url, &category)).await
    }

    async fn status(&self, download_id: &str) -> Result<DownloadStatus, PluginError> {
        let plugin = Arc::clone(&self.plugin);
        let settings = self.settings.clone();
        let id = download_id.to_string();
        let s = blocking(move || plugin.status(&settings, &id)).await?;
        Ok(DownloadStatus {
            state: match s.state {
                PluginDownloadState::Queued => DownloadState::Queued,
                PluginDownloadState::Downloading => DownloadState::Downloading,
                PluginDownloadState::Completed => DownloadState::Completed,
                PluginDownloadState::Failed => DownloadState::Failed,
            },
            progress: s.progress.clamp(0.0, 1.0),
            content_path: s.content_path,
            ratio: s.ratio,
            seeding_time_secs: s.seeding_time_secs,
            peers: s.peers,
            error_string: s.error_message,
        })
    }

    async fn remove(&self, download_id: &str, delete_data: bool) -> Result<(), PluginError> {
        let plugin = Arc::clone(&self.plugin);
        let settings = self.settings.clone();
        let id = download_id.to_string();
        blocking(move || plugin.remove(&settings, &id, delete_data)).await
    }
}

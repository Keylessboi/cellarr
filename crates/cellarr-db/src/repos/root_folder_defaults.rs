//! Per-root-folder defaults for new artists, as Lidarr's `RootFolders` table
//! carries them (quality and metadata profile, monitor options, tags).

use cellarr_core::QualityProfileId;
use sqlx::Row;

use crate::convert::parse_uuid;
use crate::dialect::{pq, DbPool};
use crate::error::Result;
use crate::writer::WriterHandle;

/// The defaults of one root folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootFolderDefaults {
    /// The folder's path (its identity).
    pub path: String,
    /// Display name.
    pub name: String,
    /// The quality profile new artists get; `None` when unset or since deleted.
    pub quality_profile: Option<QualityProfileId>,
    /// The metadata profile's integer id.
    pub metadata_profile_id: i64,
    /// Lidarr's `defaultMonitorOption` (`all`, `future`, `missing`, ...).
    pub monitor_option: String,
    /// Lidarr's `defaultNewItemMonitorOption` (`all`, `none`, `new`).
    pub new_item_monitor_option: String,
    /// Tag ids.
    pub tags: Vec<i64>,
}

/// Reads and writes of [`RootFolderDefaults`].
#[derive(Clone)]
pub struct RootFolderDefaultsRepo {
    pool: DbPool,
    writer: WriterHandle,
}

impl RootFolderDefaultsRepo {
    pub(crate) fn new(pool: DbPool, writer: WriterHandle) -> Self {
        Self { pool, writer }
    }

    /// Insert or replace the defaults of a root folder.
    ///
    /// # Errors
    /// Returns a [`crate::DbError`] on write failure.
    pub async fn upsert(&self, d: RootFolderDefaults) -> Result<()> {
        self.writer
            .submit(move |conn| {
                Box::pin(async move {
                    sqlx::query(&pq(
                        "INSERT INTO root_folder_defaults (path, name, quality_profile_id, \
                         metadata_profile_id, monitor_option, new_item_monitor_option, tags) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                         ON CONFLICT (path) DO UPDATE SET name = excluded.name, \
                         quality_profile_id = excluded.quality_profile_id, \
                         metadata_profile_id = excluded.metadata_profile_id, \
                         monitor_option = excluded.monitor_option, \
                         new_item_monitor_option = excluded.new_item_monitor_option, \
                         tags = excluded.tags",
                    ))
                    .bind(d.path)
                    .bind(d.name)
                    .bind(d.quality_profile.map(|p| p.to_string()))
                    .bind(d.metadata_profile_id)
                    .bind(d.monitor_option)
                    .bind(d.new_item_monitor_option)
                    .bind(serde_json::to_string(&d.tags).unwrap_or_else(|_| "[]".into()))
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
    }

    /// Every stored root folder's defaults.
    ///
    /// # Errors
    /// Returns a [`crate::DbError`] on query failure.
    pub async fn list(&self) -> Result<Vec<RootFolderDefaults>> {
        let rows = sqlx::query(&pq(
            "SELECT path, name, quality_profile_id, metadata_profile_id, monitor_option, \
             new_item_monitor_option, tags FROM root_folder_defaults ORDER BY path",
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let profile: Option<String> = r.try_get("quality_profile_id")?;
                let tags: String = r.try_get("tags")?;
                Ok(RootFolderDefaults {
                    path: r.try_get("path")?,
                    name: r.try_get("name")?,
                    quality_profile: profile
                        .map(|s| {
                            parse_uuid("quality_profile_id", &s).map(QualityProfileId::from_uuid)
                        })
                        .transpose()?,
                    metadata_profile_id: r.try_get("metadata_profile_id")?,
                    monitor_option: r.try_get("monitor_option")?,
                    new_item_monitor_option: r.try_get("new_item_monitor_option")?,
                    tags: serde_json::from_str(&tags).unwrap_or_default(),
                })
            })
            .collect()
    }
}

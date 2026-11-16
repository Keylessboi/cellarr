//! The database-backed [`MusicLookup`]: what the music media module reads the
//! library through.

use async_trait::async_trait;
use cellarr_core::repo::ContentRepository;
use cellarr_core::ContentId;
use cellarr_db::{Database, DbError};
use cellarr_media::{AlbumCandidate, AlbumIdentity, MusicLookup, TrackIdentity};

/// A [`MusicLookup`] over the cellarr database.
pub struct DbMusicLookup {
    db: Database,
}

impl DbMusicLookup {
    /// Build a lookup over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    async fn identity(
        &self,
        album: &cellarr_core::Album,
    ) -> Result<Option<AlbumIdentity>, DbError> {
        let music = self.db.music();
        let Some(artist) = music.artist_by_content(album.artist_content_id).await? else {
            return Ok(None);
        };
        let disc_count = music
            .tracks_for_album(album.content_id)
            .await?
            .iter()
            .map(|t| t.disc)
            .max()
            .unwrap_or(0);
        Ok(Some(AlbumIdentity {
            artist_mbid: Some(artist.mbid.clone()),
            album_mbid: Some(album.mbid.clone()),
            artist_folder: folder_name(&artist.path, &artist.name),
            artist_name: artist.name,
            album_title: album.title.clone(),
            year: album.year(),
            disc_count,
        }))
    }

    async fn candidates(
        &self,
        found: Vec<cellarr_db::AlbumWithArtist>,
    ) -> Result<Vec<AlbumCandidate>, DbError> {
        let mut out = Vec::with_capacity(found.len());
        for hit in found {
            if let Some(content_ref) = self.db.content().get(hit.album.content_id).await? {
                out.push(AlbumCandidate {
                    content_ref,
                    year: hit.album.year(),
                });
            }
        }
        Ok(out)
    }
}

/// The folder an artist lives in: the last component of its stored path, so a
/// library already laid out by another manager keeps its layout. An artist with
/// no usable path is filed under its name.
fn folder_name(path: &str, fallback: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[async_trait]
impl MusicLookup for DbMusicLookup {
    type Error = DbError;

    async fn album(&self, album: ContentId) -> Result<Option<AlbumIdentity>, DbError> {
        match self.db.music().album_by_content(album).await? {
            Some(a) => self.identity(&a).await,
            None => Ok(None),
        }
    }

    async fn track(&self, track: ContentId) -> Result<Option<TrackIdentity>, DbError> {
        let music = self.db.music();
        let Some(t) = music.track_by_content(track).await? else {
            return Ok(None);
        };
        let Some(album) = music.album_by_content(t.album_content_id).await? else {
            return Ok(None);
        };
        Ok(self.identity(&album).await?.map(|album| TrackIdentity {
            album,
            title: t.title,
            disc: t.disc,
            position: t.position,
        }))
    }

    async fn albums_named(
        &self,
        artist_norm: &str,
        album_norms: &[String],
    ) -> Result<Vec<AlbumCandidate>, DbError> {
        let found = self
            .db
            .music()
            .albums_named(artist_norm, album_norms)
            .await?;
        self.candidates(found).await
    }

    async fn albums_named_jointly(&self, blob: &str) -> Result<Vec<AlbumCandidate>, DbError> {
        let found = self.db.music().albums_named_jointly(blob).await?;
        self.candidates(found).await
    }
}

#[cfg(test)]
mod tests {
    use super::folder_name;

    #[test]
    fn an_artist_keeps_the_folder_it_already_has() {
        assert_eq!(folder_name("/data/media/music/Radiohead", "x"), "Radiohead");
        assert_eq!(
            folder_name("/data/media/music/Radiohead/", "x"),
            "Radiohead"
        );
        assert_eq!(folder_name("", "Radiohead"), "Radiohead");
    }
}

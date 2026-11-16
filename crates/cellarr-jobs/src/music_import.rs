//! Placing the files of a finished album download onto the album's tracks.
//!
//! A music download is a folder of audio files, and what each file *is* comes
//! from inside it: the disc and track number in its tags, or failing that its
//! file name. This module turns that folder into a list of `(file, track)`
//! placements for the generic import to move, and refuses — rather than guesses
//! — when the folder is not the album it was grabbed for.

use std::path::{Path, PathBuf};

use cellarr_core::music::normalize_name;
use cellarr_core::{ContentRef, Coordinates, MediaType, Track};
use cellarr_db::{Database, NewTrack};
use cellarr_fs::{is_audio_file, read_audio_tags, AudioTags};

/// How deep into a download folder audio files are looked for (`Artist/Album/CD1/`).
const MAX_DEPTH: usize = 5;

/// Every audio file under `src` (or `src` itself if it is one), sorted.
///
/// Symlinks are not followed, and folders that hold extras rather than the
/// album are skipped.
pub fn collect_audio_sources(src: &Path) -> std::io::Result<Vec<PathBuf>> {
    if src.is_file() {
        return Ok(if is_audio_file(src) {
            vec![src.to_path_buf()]
        } else {
            Vec::new()
        });
    }
    let mut out = Vec::new();
    walk(src, 0, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !matches!(
                name.as_str(),
                "scans" | "artwork" | "covers" | "extras" | "video"
            ) {
                walk(&path, depth + 1, out)?;
            }
        } else if kind.is_file() && is_audio_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// What a file says about which track it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileClaim {
    /// Disc number, when stated.
    pub disc: Option<u32>,
    /// Track number, when stated.
    pub track: Option<u32>,
    /// Title, when stated.
    pub title: Option<String>,
}

/// The disc/track/title a file claims: its tags first, its name where the tags
/// are silent. A multi-disc download often keeps each disc in its own folder
/// (`CD2/05 - Song.flac`), so a disc folder name is read too.
#[must_use]
pub fn claim_of(path: &Path, tags: Option<&AudioTags>) -> FileClaim {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let (name_disc, name_track, name_title) = parse_file_name(stem);
    let folder_disc = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .and_then(disc_from_folder);
    FileClaim {
        disc: tags.and_then(|t| t.disc).or(name_disc).or(folder_disc),
        track: tags.and_then(|t| t.track).or(name_track),
        title: tags.and_then(|t| t.title.clone()).or(name_title),
    }
}

/// `03 - Title`, `1-03 Title`, `103. Title`, `Artist - 03 - Title`.
fn parse_file_name(stem: &str) -> (Option<u32>, Option<u32>, Option<String>) {
    let parts: Vec<&str> = stem.split(" - ").collect();
    // Find the first segment that is just a number (or disc-track).
    for (i, seg) in parts.iter().enumerate() {
        let seg = seg.trim();
        if let Some((d, t)) = seg.split_once('-') {
            if let (Ok(d), Ok(t)) = (d.trim().parse::<u32>(), t.trim().parse::<u32>()) {
                if d > 0 && t > 0 && d < 100 {
                    return (Some(d), Some(t), rest_title(&parts, i));
                }
            }
        }
        if let Ok(t) = seg.parse::<u32>() {
            if t > 0 && t < 1000 {
                return (None, Some(t), rest_title(&parts, i));
            }
        }
    }
    // `1-03 Title`: disc, track, then a title with no ` - ` separator.
    if let Some((d, rest)) = stem.split_once('-') {
        let t_digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let (Ok(d), Ok(t)) = (d.trim().parse::<u32>(), t_digits.parse::<u32>()) {
            let after = rest.trim_start()[t_digits.len()..]
                .trim_start_matches(['.', ' ', '_', '-'])
                .trim();
            if d > 0 && d < 100 && t > 0 && !t_digits.is_empty() && t_digits.len() <= 3 {
                return (
                    Some(d),
                    Some(t),
                    (!after.is_empty()).then(|| after.to_string()),
                );
            }
        }
    }
    // `03 Title` / `03. Title` / `03_Title`
    let digits: String = stem.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let rest = stem[digits.len()..]
            .trim_start_matches(['.', ' ', '_', '-'])
            .trim();
        if let Ok(t) = digits.parse::<u32>() {
            if t > 0 {
                return (None, Some(t), (!rest.is_empty()).then(|| rest.to_string()));
            }
        }
    }
    (None, None, (!stem.is_empty()).then(|| stem.to_string()))
}

fn rest_title(parts: &[&str], number_at: usize) -> Option<String> {
    let rest = parts[number_at + 1..].join(" - ");
    let rest = rest.trim();
    (!rest.is_empty()).then(|| rest.to_string())
}

fn disc_from_folder(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    for prefix in ["cd", "disc", "disk", "d"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let digits: String = rest
                .trim_start_matches([' ', '_', '-'])
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                if n > 0 && n < 50 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Place each audio file on one of the album's tracks.
///
/// - The album's track list is used as stored. If it has none (the metadata
///   service was never reached) the list is built from the files themselves, so
///   an offline import still works; a later refresh reconciles it by disc and
///   position.
/// - A file is placed by `(disc, track)`; a file that states only a track number
///   is placed on that track of disc 1, or of the only disc; a file that states
///   no number is placed by its title.
/// - A file that matches no track is left behind rather than forced onto one.
/// - The folder must be the album: if the files' own album tags are present and
///   none of them is this album, the import is refused.
pub async fn plan_album_placements(
    db: &Database,
    album: &ContentRef,
    album_title: &str,
    sources: &[PathBuf],
) -> Result<Vec<(PathBuf, ContentRef)>, String> {
    let read: Vec<(PathBuf, Option<AudioTags>)> = sources
        .iter()
        .map(|p| (p.clone(), read_audio_tags(p)))
        .collect();
    guard_identity(album_title, &read)?;

    let music = db.music();
    let mut tracks = music
        .tracks_for_album(album.id)
        .await
        .map_err(|e| format!("read tracks: {e}"))?;
    let claims: Vec<FileClaim> = read.iter().map(|(p, t)| claim_of(p, t.as_ref())).collect();

    if tracks.is_empty() {
        let mut derived: Vec<NewTrack> = Vec::new();
        let mut taken = std::collections::BTreeSet::new();
        for claim in &claims {
            let (Some(track), disc) = (claim.track, claim.disc.unwrap_or(1)) else {
                continue;
            };
            if taken.insert((disc, track)) {
                derived.push(NewTrack {
                    title: claim
                        .title
                        .clone()
                        .unwrap_or_else(|| format!("Track {track}")),
                    disc,
                    position: track,
                    ..NewTrack::default()
                });
            }
        }
        if derived.is_empty() {
            return Err("the album has no track list and the files state no track numbers".into());
        }
        tracks = music
            .set_album_tracks(album.library_id, album.id, None, derived)
            .await
            .map_err(|e| format!("store tracks: {e}"))?;
    }

    let only_disc = {
        let mut discs: Vec<u32> = tracks.iter().map(|t| t.disc).collect();
        discs.sort_unstable();
        discs.dedup();
        (discs.len() == 1).then(|| discs[0])
    };
    let mut used = std::collections::HashSet::new();
    let mut placements = Vec::new();
    for ((path, _), claim) in read.iter().zip(&claims) {
        let target = locate(&tracks, claim, only_disc).filter(|t| used.insert(t.id));
        if let Some(t) = target {
            placements.push((path.clone(), track_ref(album, t)));
        }
    }
    if placements.is_empty() {
        return Err(format!(
            "none of the {} audio files could be matched to a track of the album",
            sources.len()
        ));
    }
    Ok(placements)
}

fn locate<'a>(tracks: &'a [Track], claim: &FileClaim, only_disc: Option<u32>) -> Option<&'a Track> {
    if let Some(number) = claim.track {
        let disc = claim.disc.or(only_disc).unwrap_or(1);
        if let Some(t) = tracks
            .iter()
            .find(|t| t.disc == disc && t.position == number)
        {
            return Some(t);
        }
        // A disc-less number on a multi-disc album may be a continuous count.
        if claim.disc.is_none() {
            let mut ordered: Vec<&Track> = tracks.iter().collect();
            ordered.sort_by_key(|t| (t.disc, t.position));
            if let Some(t) = ordered.get(usize::try_from(number).ok()?.checked_sub(1)?) {
                if claim.title.as_deref().map(normalize_name) == Some(normalize_name(&t.title)) {
                    return Some(t);
                }
            }
        }
    }
    let wanted = normalize_name(claim.title.as_deref()?);
    if wanted.is_empty() {
        return None;
    }
    tracks.iter().find(|t| normalize_name(&t.title) == wanted)
}

fn track_ref(album: &ContentRef, track: &Track) -> ContentRef {
    ContentRef {
        id: track.content_id,
        library_id: album.library_id,
        media_type: MediaType::Music,
        coords: Coordinates::Track {
            disc: track.disc,
            track: track.position,
        },
    }
}

/// Refuse a folder whose own tags say it is a different album.
///
/// Only decisive when at least half the files carry an album tag, and none of
/// those tags is, or contains, the album's title once edition suffixes are set
/// aside. Files with no tags at all are not evidence either way.
fn guard_identity(album_title: &str, read: &[(PathBuf, Option<AudioTags>)]) -> Result<(), String> {
    let wanted = normalize_name(album_title);
    let tagged: Vec<String> = read
        .iter()
        .filter_map(|(_, t)| t.as_ref().and_then(|t| t.album.as_deref()))
        .map(normalize_name)
        .filter(|a| !a.is_empty())
        .collect();
    if wanted.is_empty() || tagged.len() * 2 < read.len() {
        return Ok(());
    }
    let agrees = tagged
        .iter()
        .any(|a| *a == wanted || a.contains(&wanted) || wanted.contains(a.as_str()));
    if agrees {
        Ok(())
    } else {
        Err(format!(
            "the files are tagged as album {:?}, not {album_title:?}",
            tagged[0]
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_state_their_track_in_the_common_shapes() {
        let t = |s: &str| parse_file_name(s);
        assert_eq!(t("03 - Airbag"), (None, Some(3), Some("Airbag".into())));
        assert_eq!(t("03. Airbag"), (None, Some(3), Some("Airbag".into())));
        assert_eq!(t("03_Airbag"), (None, Some(3), Some("Airbag".into())));
        assert_eq!(t("1-03 Airbag"), (Some(1), Some(3), Some("Airbag".into())));
        assert_eq!(
            t("2-07 - Airbag"),
            (Some(2), Some(7), Some("Airbag".into()))
        );
        assert_eq!(
            t("Radiohead - 03 - Airbag"),
            (None, Some(3), Some("Airbag".into()))
        );
        assert_eq!(t("Airbag"), (None, None, Some("Airbag".into())));
    }

    #[test]
    fn a_disc_folder_names_the_disc() {
        assert_eq!(disc_from_folder("CD2"), Some(2));
        assert_eq!(disc_from_folder("Disc 1"), Some(1));
        assert_eq!(disc_from_folder("disk_03"), Some(3));
        assert_eq!(disc_from_folder("OK Computer"), None);
    }

    fn track(id: i64, disc: u32, position: u32, title: &str) -> Track {
        Track {
            id,
            content_id: cellarr_core::ContentId::new(),
            album_content_id: cellarr_core::ContentId::new(),
            mbid: None,
            title: title.into(),
            disc,
            position,
            duration_ms: None,
        }
    }

    #[test]
    fn a_file_is_placed_by_disc_and_track_then_by_title() {
        let tracks = vec![
            track(1, 1, 1, "Airbag"),
            track(2, 1, 2, "Lucky"),
            track(3, 2, 1, "Bonus"),
        ];
        let by_number = FileClaim {
            disc: Some(2),
            track: Some(1),
            title: None,
        };
        assert_eq!(locate(&tracks, &by_number, None).unwrap().id, 3);
        let by_title = FileClaim {
            disc: None,
            track: None,
            title: Some("LUCKY!".into()),
        };
        assert_eq!(locate(&tracks, &by_title, None).unwrap().id, 2);
        let unknown = FileClaim {
            disc: Some(1),
            track: Some(9),
            title: Some("Nothing".into()),
        };
        assert!(locate(&tracks, &unknown, None).is_none());
    }

    #[test]
    fn a_track_number_alone_uses_the_only_disc() {
        let tracks = vec![track(1, 1, 1, "A"), track(2, 1, 2, "B")];
        let claim = FileClaim {
            disc: None,
            track: Some(2),
            title: None,
        };
        assert_eq!(locate(&tracks, &claim, Some(1)).unwrap().id, 2);
    }

    fn tagged(album: Option<&str>) -> (PathBuf, Option<AudioTags>) {
        (
            PathBuf::from("x.flac"),
            Some(AudioTags {
                album: album.map(String::from),
                codec: "flac".into(),
                ..AudioTags::default()
            }),
        )
    }

    #[test]
    fn a_folder_tagged_as_another_album_is_refused() {
        let wrong = vec![tagged(Some("Kid A")), tagged(Some("Kid A"))];
        assert!(guard_identity("OK Computer", &wrong).is_err());
        let right = vec![
            tagged(Some("OK Computer (Remastered)")),
            tagged(Some("OK Computer")),
        ];
        assert!(guard_identity("OK Computer", &right).is_ok());
        let untagged = vec![tagged(None), tagged(None)];
        assert!(guard_identity("OK Computer", &untagged).is_ok());
    }
}

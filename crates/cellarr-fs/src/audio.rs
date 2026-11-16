//! Reading what an audio file says about itself.
//!
//! A music download is a folder of track files, and the only reliable statement
//! of which track each file is comes from inside the file: its tags (title,
//! track and disc number) and its stream properties (codec, bitrate, bit depth).
//! This module reads both, in pure Rust — no ffmpeg or external tool — so music
//! import works in the same single static binary as everything else.
//!
//! Nothing here is trusted blindly: a file with no tags, or one that is not
//! audio at all, simply yields `None`, and the importer falls back to the file
//! name or leaves the file alone.

use std::path::Path;

use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey};

/// File extensions treated as audio tracks.
pub const AUDIO_EXTS: &[&str] = &[
    "flac", "mp3", "m4a", "ogg", "oga", "opus", "wav", "ape", "wv", "wma", "aiff", "aif", "alac",
    "aac",
];

/// Whether `path` has an audio-track extension.
#[must_use]
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// What an audio file's tags and stream say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioTags {
    /// Track title.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Album artist.
    pub album_artist: Option<String>,
    /// Album title.
    pub album: Option<String>,
    /// Track number on its disc.
    pub track: Option<u32>,
    /// Disc number.
    pub disc: Option<u32>,
    /// Container/codec family: `flac`, `mp3`, `aac`, `vorbis`, `opus`, `wav`,
    /// `ape`, `wavpack`, `aiff`, or `unknown`.
    pub codec: String,
    /// Length in milliseconds.
    pub duration_ms: Option<u32>,
    /// Audio bitrate in kbps.
    pub bitrate_kbps: Option<u32>,
    /// Sample rate in Hz.
    pub sample_rate: Option<u32>,
    /// Bits per sample (lossless formats).
    pub bit_depth: Option<u8>,
}

/// Read a file's tags and stream properties, or `None` if it cannot be read as
/// audio.
#[must_use]
pub fn read_audio_tags(path: &Path) -> Option<AudioTags> {
    let tagged = lofty::read_from_path(path).ok()?;
    let properties = tagged.properties();
    let codec = match tagged.file_type() {
        FileType::Flac => "flac",
        FileType::Mpeg => "mp3",
        FileType::Mp4 | FileType::Aac => "aac",
        FileType::Vorbis => "vorbis",
        FileType::Opus => "opus",
        FileType::Wav => "wav",
        FileType::Ape => "ape",
        FileType::WavPack => "wavpack",
        FileType::Aiff => "aiff",
        _ => "unknown",
    }
    .to_string();
    let mut out = AudioTags {
        codec,
        duration_ms: u32::try_from(properties.duration().as_millis())
            .ok()
            .filter(|ms| *ms > 0),
        bitrate_kbps: properties.audio_bitrate().filter(|b| *b > 0),
        sample_rate: properties.sample_rate().filter(|s| *s > 0),
        bit_depth: properties.bit_depth().filter(|b| *b > 0),
        ..AudioTags::default()
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let clean = |s: std::borrow::Cow<'_, str>| {
            let s = s.trim().to_string();
            (!s.is_empty()).then_some(s)
        };
        out.title = tag.title().and_then(clean);
        out.artist = tag.artist().and_then(clean);
        out.album = tag.album().and_then(clean);
        out.album_artist = tag
            .get_string(&ItemKey::AlbumArtist)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        out.track = tag.track().filter(|t| *t > 0);
        out.disc = tag.disk().filter(|d| *d > 0);
    }
    Some(out)
}

/// The quality-catalogue bucket a file's own stream properties put it in, or
/// `None` when they do not say (an unknown codec, or a lossy file whose bitrate
/// could not be read).
///
/// This grades the *file*, so it is only consulted when the release title
/// stated no quality.
#[must_use]
pub fn audio_quality_name(tags: &AudioTags) -> Option<&'static str> {
    let hi_res = tags.bit_depth.is_some_and(|b| b > 16);
    match tags.codec.as_str() {
        "flac" => Some(if hi_res { "FLAC 24bit" } else { "FLAC" }),
        "wav" | "aiff" => Some("WAV"),
        "ape" => Some("APE"),
        "wavpack" => Some("WavPack"),
        "mp3" => tags.bitrate_kbps.map(mp3_bucket),
        "aac" => {
            let kbps = tags.bitrate_kbps?;
            // An MP4 container holding several hundred kbps is Apple Lossless,
            // not AAC, which tops out well below that.
            Some(if kbps >= 500 {
                if hi_res {
                    "ALAC 24bit"
                } else {
                    "ALAC"
                }
            } else if near(kbps, 320) {
                "AAC-320"
            } else if near(kbps, 256) {
                "AAC-256"
            } else if near(kbps, 192) {
                "AAC-192"
            } else {
                "AAC-VBR"
            })
        }
        "vorbis" => {
            let kbps = tags.bitrate_kbps?;
            Some(match kbps {
                k if k >= 400 => "OGG Vorbis Q10",
                k if k >= 290 => "OGG Vorbis Q9",
                k if k >= 240 => "OGG Vorbis Q8",
                k if k >= 208 => "OGG Vorbis Q7",
                k if k >= 176 => "OGG Vorbis Q6",
                _ => "OGG Vorbis Q5",
            })
        }
        _ => None,
    }
}

/// Whether a measured bitrate is a nominal rate, allowing for container
/// overhead in the measurement.
fn near(kbps: u32, nominal: u32) -> bool {
    kbps.abs_diff(nominal) <= 4
}

/// The MP3 bucket for a measured bitrate. A rate that sits on a standard
/// constant-bitrate step is that step; one that falls between steps is a
/// variable-bitrate encode, graded by which preset averages that high.
fn mp3_bucket(kbps: u32) -> &'static str {
    const STEPS: &[(u32, &str)] = &[
        (320, "MP3-320"),
        (256, "MP3-256"),
        (224, "MP3-224"),
        (192, "MP3-192"),
        (160, "MP3-160"),
        (128, "MP3-128"),
        (112, "MP3-112"),
        (96, "MP3-96"),
        (80, "MP3-80"),
        (64, "MP3-64"),
        (56, "MP3-56"),
        (48, "MP3-48"),
        (40, "MP3-40"),
        (32, "MP3-32"),
        (24, "MP3-24"),
        (16, "MP3-16"),
        (8, "MP3-8"),
    ];
    if let Some((_, name)) = STEPS.iter().find(|(rate, _)| kbps.abs_diff(*rate) <= 1) {
        return name;
    }
    match kbps {
        k if k >= 215 => "MP3-VBR-V0",
        k if k >= 165 => "MP3-VBR-V2",
        k => STEPS
            .iter()
            .find(|(rate, _)| k >= *rate)
            .map_or("MP3-8", |(_, name)| name),
    }
}

/// Test support: write a minimal, valid FLAC file carrying the given tags.
///
/// The file has a stream-info header and a comment block but no audio frames,
/// which is all a tag reader needs. Used by this crate's tests and by the import
/// pipeline's, so both exercise the real reader instead of a mock.
#[doc(hidden)]
pub mod testing {
    use std::io::Write;
    use std::path::Path;

    /// Write a taggable FLAC at `path`. `bits` is the bit depth (16 or 24).
    ///
    /// # Errors
    /// Returns any I/O error from creating or writing the file.
    pub fn write_flac(path: &Path, bits: u8, comments: &[(&str, &str)]) -> std::io::Result<()> {
        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0), 34 bytes.
        out.push(0x00);
        out.extend_from_slice(&[0x00, 0x00, 34]);
        out.extend_from_slice(&4096u16.to_be_bytes()); // min block size
        out.extend_from_slice(&4096u16.to_be_bytes()); // max block size
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // min/max frame size (unknown)
                                                    // 20 bits sample rate, 3 bits channels-1, 5 bits bps-1, 36 bits samples.
        let sample_rate: u64 = 44_100;
        let total_samples: u64 = 44_100 * 180;
        let packed: u64 =
            (sample_rate << 44) | (1u64 << 41) | ((u64::from(bits) - 1) << 36) | total_samples;
        out.extend_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&[0u8; 16]); // md5
                                           // VORBIS_COMMENT (type 4), last block.
        let vendor = b"cellarr-test";
        let mut body = Vec::new();
        body.extend_from_slice(&u32::try_from(vendor.len()).unwrap_or(0).to_le_bytes());
        body.extend_from_slice(vendor);
        body.extend_from_slice(&u32::try_from(comments.len()).unwrap_or(0).to_le_bytes());
        for (key, value) in comments {
            let entry = format!("{key}={value}");
            body.extend_from_slice(&u32::try_from(entry.len()).unwrap_or(0).to_le_bytes());
            body.extend_from_slice(entry.as_bytes());
        }
        out.push(0x84);
        let len = u32::try_from(body.len()).unwrap_or(0).to_be_bytes();
        out.extend_from_slice(&len[1..]);
        out.extend_from_slice(&body);
        // A little padding so the file is not suspiciously tiny.
        out.extend_from_slice(&[0u8; 64]);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(path)?.write_all(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(codec: &str, kbps: Option<u32>, bits: Option<u8>) -> AudioTags {
        AudioTags {
            codec: codec.to_string(),
            bitrate_kbps: kbps,
            bit_depth: bits,
            ..AudioTags::default()
        }
    }

    #[test]
    fn a_flac_file_yields_its_tags_and_bit_depth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("03 - Subterranean Homesick Alien.flac");
        testing::write_flac(
            &path,
            16,
            &[
                ("TITLE", "Subterranean Homesick Alien"),
                ("ARTIST", "Radiohead"),
                ("ALBUMARTIST", "Radiohead"),
                ("ALBUM", "OK Computer"),
                ("TRACKNUMBER", "3"),
                ("DISCNUMBER", "1"),
            ],
        )
        .unwrap();
        let read = read_audio_tags(&path).expect("a valid flac reads");
        assert_eq!(read.title.as_deref(), Some("Subterranean Homesick Alien"));
        assert_eq!(read.artist.as_deref(), Some("Radiohead"));
        assert_eq!(read.album_artist.as_deref(), Some("Radiohead"));
        assert_eq!(read.album.as_deref(), Some("OK Computer"));
        assert_eq!((read.track, read.disc), (Some(3), Some(1)));
        assert_eq!(read.codec, "flac");
        assert_eq!(read.bit_depth, Some(16));
        assert_eq!(read.sample_rate, Some(44_100));
        assert_eq!(audio_quality_name(&read), Some("FLAC"));
    }

    #[test]
    fn a_24_bit_flac_grades_as_hi_res() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        testing::write_flac(&path, 24, &[("TITLE", "x")]).unwrap();
        assert_eq!(
            audio_quality_name(&read_audio_tags(&path).unwrap()),
            Some("FLAC 24bit")
        );
    }

    #[test]
    fn a_file_that_is_not_audio_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cover.flac");
        std::fs::write(&path, b"this is not a flac file").unwrap();
        assert!(read_audio_tags(&path).is_none());
        assert!(read_audio_tags(&dir.path().join("missing.flac")).is_none());
    }

    #[test]
    fn audio_extensions_are_recognized_case_insensitively() {
        assert!(is_audio_file(Path::new("a/01.FLAC")));
        assert!(is_audio_file(Path::new("a/01.mp3")));
        assert!(!is_audio_file(Path::new("a/cover.jpg")));
        assert!(!is_audio_file(Path::new("a/album.cue")));
        assert!(!is_audio_file(Path::new("a/noext")));
    }

    #[test]
    fn mp3_grades_by_constant_step_or_vbr_preset() {
        assert_eq!(
            audio_quality_name(&tags("mp3", Some(320), None)),
            Some("MP3-320")
        );
        assert_eq!(
            audio_quality_name(&tags("mp3", Some(192), None)),
            Some("MP3-192")
        );
        assert_eq!(
            audio_quality_name(&tags("mp3", Some(245), None)),
            Some("MP3-VBR-V0")
        );
        assert_eq!(
            audio_quality_name(&tags("mp3", Some(183), None)),
            Some("MP3-VBR-V2")
        );
        assert_eq!(
            audio_quality_name(&tags("mp3", Some(140), None)),
            Some("MP3-128")
        );
        assert_eq!(audio_quality_name(&tags("mp3", None, None)), None);
    }

    #[test]
    fn an_mp4_container_is_aac_or_apple_lossless_by_bitrate() {
        assert_eq!(
            audio_quality_name(&tags("aac", Some(256), None)),
            Some("AAC-256")
        );
        assert_eq!(
            audio_quality_name(&tags("aac", Some(271), None)),
            Some("AAC-VBR")
        );
        assert_eq!(
            audio_quality_name(&tags("aac", Some(900), Some(16))),
            Some("ALAC")
        );
        assert_eq!(
            audio_quality_name(&tags("aac", Some(2800), Some(24))),
            Some("ALAC 24bit")
        );
    }

    #[test]
    fn an_ungraded_codec_states_no_quality() {
        assert_eq!(audio_quality_name(&tags("opus", Some(128), None)), None);
        assert_eq!(audio_quality_name(&tags("unknown", None, None)), None);
    }
}

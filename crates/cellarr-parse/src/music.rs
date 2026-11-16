//! Reading a release title as a music release.
//!
//! A music title has a different grammar from a video one: there are two names
//! (artist and album) rather than one, no season/episode numbering, and the
//! quality is an audio codec plus a bitrate or bit depth rather than a
//! source/resolution pair. Running the video extractors over one produces noise
//! (a `24bit` reads as nothing, an album called `1989` reads as a year), so music
//! gets its own reader and the video pipeline is not involved.
//!
//! Two layouts cover what indexers and peers publish:
//!
//! - the **spaced** layout, `Artist - Album (2019) [FLAC]`, where ` - ` separates
//!   the names and brackets carry the tags; a year may sit between them
//!   (`Artist - 2019 - Album`); and
//! - the **scene** layout, `Artist_Name-Album_Title-WEB-FLAC-2019-GROUP`, with no
//!   spaces at all: hyphens separate fields, underscores stand for spaces, and
//!   the trailing fields are tags, the year and the group.
//!
//! The reader never guesses a quality the title does not state: a bare `MP3`
//! with no bitrate stays unresolved, and the decision engine treats it as an
//! unknown quality the user's profile may or may not allow.

use cellarr_core::parsed::{Confidence, MusicRelease, ParsedField, ParsedRelease};
use cellarr_core::Coordinates;

/// Read `input` as a music release.
///
/// Always returns a parse marked as music (its `music` block is present), with
/// whatever of artist, album, year, quality and group the title carries. An
/// album release is addressed to [`Coordinates::Album`].
#[must_use]
pub fn parse_music_title(input: &str) -> ParsedRelease {
    let mut out = ParsedRelease::new(input);
    let quality = quality_of(input);
    let year = year_of(input);
    let discography = is_discography(input);

    let (artist, album, group) = if is_scene_layout(input) {
        scene_names(input)
    } else {
        let (artist, album) = spaced_names(input);
        (artist, album, None)
    };

    if let Some(year) = year {
        out.year = Some(year);
        out.set_confidence(ParsedField::Year, Confidence::new(0.9));
    }
    if let Some(group) = group {
        out.group = Some(group);
        out.set_confidence(ParsedField::Group, Confidence::new(0.8));
    }
    if quality.is_some() {
        out.set_confidence(ParsedField::Audio, Confidence::CERTAIN);
    }
    out.clean_title = album.clone().or_else(|| Some(strip_tags(input)));
    if !discography {
        out.coordinates.push(Coordinates::Album);
        out.set_confidence(ParsedField::Coordinates, Confidence::new(0.9));
    }
    out.music = Some(MusicRelease {
        artist,
        album,
        quality: quality.map(str::to_string),
        discography,
    });
    out
}

/// The audio-quality bucket a string advertises, as its catalogue name.
///
/// Exposed for the import side, which grades a file from its own name or its
/// folder's when the release title said nothing useful.
#[must_use]
pub fn quality_of(input: &str) -> Option<&'static str> {
    let tokens = tokenize(input);
    let has = |t: &str| tokens.iter().any(|x| x == t);
    let any = |ts: &[&str]| ts.iter().any(|t| has(t));

    // Bit depth: a `24` only counts next to a lossless marker or an explicit
    // `bit`, so a track count or a sample rate is not mistaken for it.
    let hi_res = any(&["24bit", "24b", "24bits", "hires", "hi-res"])
        || adjacent(&tokens, "24", &["bit", "bits", "44", "48", "88", "96", "176", "192"])
        || tokens.iter().any(|t| {
            t.starts_with("24-") && t[3..].chars().all(|c| c.is_ascii_digit() || c == '.')
        });

    if has("flac") {
        return Some(if hi_res { "FLAC 24bit" } else { "FLAC" });
    }
    if has("alac") {
        return Some(if hi_res { "ALAC 24bit" } else { "ALAC" });
    }
    if any(&["wavpack", "wv"]) {
        return Some("WavPack");
    }
    if any(&["ape", "monkey"]) && !has("mp3") {
        return Some("APE");
    }
    if has("wav") {
        return Some("WAV");
    }

    let bitrate = bitrate_of(&tokens);
    let vbr_preset = if any(&["v0", "v0vbr", "vbr0"]) {
        Some(0)
    } else if any(&["v2", "v2vbr", "vbr2"]) {
        Some(2)
    } else {
        None
    };

    if has("mp3") || vbr_preset.is_some() {
        if let Some(preset) = vbr_preset {
            return Some(if preset == 0 { "MP3-VBR-V0" } else { "MP3-VBR-V2" });
        }
        return bitrate.and_then(mp3_bucket);
    }
    if any(&["aac", "m4a", "itunes"]) {
        return match bitrate {
            Some(320) => Some("AAC-320"),
            Some(256) => Some("AAC-256"),
            Some(192) => Some("AAC-192"),
            // An iTunes Plus purchase is 256 kbps AAC by definition.
            None if has("itunes") => Some("AAC-256"),
            None if has("vbr") => Some("AAC-VBR"),
            _ => None,
        };
    }
    if any(&["ogg", "vorbis"]) {
        return ["q10", "q9", "q8", "q7", "q6", "q5"]
            .iter()
            .zip([
                "OGG Vorbis Q10",
                "OGG Vorbis Q9",
                "OGG Vorbis Q8",
                "OGG Vorbis Q7",
                "OGG Vorbis Q6",
                "OGG Vorbis Q5",
            ])
            .find(|(q, _)| has(q))
            .map(|(_, name)| name);
    }
    if has("wma") {
        return Some("WMA");
    }
    // A bare bitrate with a `kbps` unit and no codec is MP3 in practice: it is the
    // only lossy format routinely advertised by bitrate alone.
    if tokens.iter().any(|t| t.ends_with("kbps") || t.ends_with("kbit")) {
        return bitrate.and_then(mp3_bucket);
    }
    None
}

/// Lower-cased tokens, split on everything that is not a letter, a digit, or a
/// hyphen/dot inside a number (`24-96`, `44.1`).
fn tokenize(input: &str) -> Vec<String> {
    let lower = input.to_lowercase();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = lower.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let joins_number = (c == '-' || c == '.')
            && i > 0
            && i + 1 < chars.len()
            && chars[i - 1].is_ascii_digit()
            && chars[i + 1].is_ascii_digit();
        if c.is_alphanumeric() || joins_number {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Whether `first` is immediately followed by one of `seconds`.
fn adjacent(tokens: &[String], first: &str, seconds: &[&str]) -> bool {
    tokens
        .windows(2)
        .any(|w| w[0] == first && seconds.iter().any(|s| w[1] == *s || w[1].starts_with(s)))
}

/// A stated bitrate in kbps: `320`, `320k`, `320kbps`, `320kbit`.
fn bitrate_of(tokens: &[String]) -> Option<u32> {
    const KNOWN: &[u32] = &[
        320, 256, 224, 192, 160, 128, 112, 96, 80, 64, 56, 48, 40, 32, 24, 16, 8,
    ];
    for (i, token) in tokens.iter().enumerate() {
        let digits: String = token.chars().take_while(char::is_ascii_digit).collect();
        let rest = &token[digits.len()..];
        let Ok(value) = digits.parse::<u32>() else {
            continue;
        };
        if !KNOWN.contains(&value) {
            continue;
        }
        let unit_attached = matches!(rest, "k" | "kbps" | "kbit" | "kbs" | "kb");
        let unit_follows = tokens
            .get(i + 1)
            .is_some_and(|n| matches!(n.as_str(), "kbps" | "kbit" | "kbs" | "kb" | "cbr"));
        let codec_precedes = i > 0
            && matches!(
                tokens[i - 1].as_str(),
                "mp3" | "aac" | "cbr" | "m4a" | "lame"
            );
        // A bare number is only a bitrate at the common high rates, where it
        // cannot be confused with a year fragment, a track count or a disc count.
        let unambiguous = rest.is_empty() && value >= 128;
        if unit_attached || unit_follows || codec_precedes || unambiguous {
            return Some(value);
        }
    }
    None
}

fn mp3_bucket(kbps: u32) -> Option<&'static str> {
    Some(match kbps {
        320 => "MP3-320",
        256 => "MP3-256",
        224 => "MP3-224",
        192 => "MP3-192",
        160 => "MP3-160",
        128 => "MP3-128",
        112 => "MP3-112",
        96 => "MP3-96",
        80 => "MP3-80",
        64 => "MP3-64",
        56 => "MP3-56",
        48 => "MP3-48",
        40 => "MP3-40",
        32 => "MP3-32",
        24 => "MP3-24",
        16 => "MP3-16",
        8 => "MP3-8",
        _ => return None,
    })
}

/// The release year: a four-digit 19xx/20xx standing on its own. When a title
/// carries several (an album named after a year, a reissue date), the one inside
/// brackets wins, then the last one, which is where a release year is written.
fn year_of(input: &str) -> Option<u16> {
    let mut bracketed = None;
    let mut last = None;
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let window: String = bytes[i..i + 4].iter().collect();
        let boundary_before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let boundary_after = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric();
        if boundary_before && boundary_after && window.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(year) = window.parse::<u16>() {
                if (1900..=2099).contains(&year) {
                    let opened = i > 0 && matches!(bytes[i - 1], '(' | '[' | '{');
                    if opened && bracketed.is_none() {
                        bracketed = Some(year);
                    }
                    last = Some(year);
                }
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    bracketed.or(last)
}

fn is_discography(input: &str) -> bool {
    let lower = input.to_lowercase();
    [
        "discography",
        "discografia",
        "diskografie",
        "discographie",
        "complete albums",
        "albums collection",
        "complete collection",
        "studio albums",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// The scene layout has no spaces: fields are hyphen-separated and words are
/// joined by underscores or dots.
fn is_scene_layout(input: &str) -> bool {
    !input.contains(' ') && input.matches('-').count() >= 2
}

/// Tags a scene title carries between the album and the group. None of them is
/// part of a name, so the album ends at the first one.
fn is_scene_tag(field: &str) -> bool {
    let f = field.to_lowercase();
    let f = f.trim_matches(|c| c == '(' || c == ')' || c == '[' || c == ']');
    matches!(
        f,
        "web" | "cd" | "cds" | "cdm" | "cdr" | "cdep" | "ep" | "lp" | "vinyl" | "vls" | "flac"
            | "mp3" | "retail" | "promo" | "proper" | "repack" | "reissue" | "remastered"
            | "deluxe" | "bootleg" | "dvd" | "sat" | "fm" | "cable" | "tape" | "mag" | "single"
            | "digital" | "limited" | "advance" | "bonus" | "ost" | "16bit" | "24bit" | "2cd"
            | "3cd" | "4cd" | "2lp" | "it" | "de" | "fr" | "es" | "nl" | "se" | "jp"
    ) || (f.len() == 4 && f.chars().all(|c| c.is_ascii_digit()))
        || f.starts_with("24bit")
        || f.starts_with("16bit")
        || f.ends_with("khz")
        || f.ends_with("kbps")
}

fn scene_names(input: &str) -> (Option<String>, Option<String>, Option<String>) {
    let fields: Vec<&str> = input.split('-').collect();
    let unjoin = |s: &str| {
        s.replace(['_', '.'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let artist = fields.first().map(|f| unjoin(f)).filter(|s| !s.is_empty());
    // The album runs from the second field up to the first tag field; a hyphen
    // inside an album name therefore survives as long as no tag interrupts it.
    let mut album_fields = Vec::new();
    let mut tags_at = fields.len();
    for (i, field) in fields.iter().enumerate().skip(1) {
        if is_scene_tag(field) {
            tags_at = i;
            break;
        }
        album_fields.push(*field);
    }
    // With no tag at all the last field is the group, not part of the album.
    let group = if tags_at == fields.len() {
        album_fields.pop()
    } else {
        fields.last().copied().filter(|g| !is_scene_tag(g))
    };
    let album = Some(unjoin(&album_fields.join("-"))).filter(|s| !s.is_empty());
    (
        artist,
        album,
        group.map(str::to_string).filter(|g| !g.is_empty()),
    )
}

fn spaced_names(input: &str) -> (Option<String>, Option<String>) {
    let normalized = input.replace(['–', '—'], "-");
    let segments: Vec<&str> = normalized.split(" - ").collect();
    if segments.len() < 2 {
        return (None, None);
    }
    let artist = clean_name(segments[0]);
    // `Artist - 2019 - Album`: a segment that is only a year is a tag, not a name.
    // An album that is itself a year (`1989`) has no other candidate, so a
    // year-only segment is the album when nothing else is.
    let cleaned: Vec<String> = segments[1..].iter().map(|s| clean_name(s)).collect();
    let album = cleaned
        .iter()
        .find(|s| !s.is_empty() && !is_year_only(s))
        .or_else(|| cleaned.iter().find(|s| !s.is_empty()))
        .cloned()
        .unwrap_or_default();
    (
        Some(artist).filter(|s| !s.is_empty()),
        Some(album).filter(|s| !s.is_empty()),
    )
}

fn is_year_only(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_digit())
}

/// A name with its tags removed: bracketed groups that hold a year or a format
/// go, as do trailing bare format words. A parenthetical that is part of the
/// name (`(Deluxe Edition)`, `(Live)`) stays; matching decides what it means.
fn clean_name(segment: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut group = String::new();
    for c in segment.chars() {
        match c {
            '(' | '[' | '{' => {
                if depth == 0 {
                    group.clear();
                }
                depth += 1;
                group.push(c);
            }
            ')' | ']' | '}' if depth > 0 => {
                group.push(c);
                depth -= 1;
                if depth == 0 {
                    if !is_tag_group(&group) {
                        out.push_str(&group);
                    }
                    group.clear();
                }
            }
            _ if depth > 0 => group.push(c),
            _ => out.push(c),
        }
    }
    // An unclosed bracket is kept verbatim rather than silently dropped.
    out.push_str(&group);
    let mut words: Vec<&str> = out.split_whitespace().collect();
    // The last remaining word is never popped: a name that is only a year or a
    // format word (`1989`, `CD`) is still the name.
    while let Some(last) = words.last() {
        if words.len() > 1 && (is_format_word(last) || is_year_only(last)) {
            words.pop();
        } else {
            break;
        }
    }
    words.join(" ").trim_matches(|c| c == '-' || c == ' ').to_string()
}

fn is_format_word(word: &str) -> bool {
    let w = word.to_lowercase();
    matches!(
        w.as_str(),
        "flac" | "mp3" | "aac" | "alac" | "ogg" | "wav" | "ape" | "m4a" | "320" | "v0" | "v2"
            | "320kbps" | "cbr" | "vbr" | "web" | "cd" | "vinyl" | "24bit" | "16bit" | "lossless"
            | "kbps"
    )
}

/// Whether a bracketed group is a tag (year, format, source) rather than part of
/// the name.
fn is_tag_group(group: &str) -> bool {
    let inner = group
        .trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | '{' | '}'))
        .trim();
    if inner.is_empty() {
        return true;
    }
    if quality_of(inner).is_some() {
        return true;
    }
    inner
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '-' | '/' | '+' | '.'))
        .filter(|w| !w.is_empty())
        .all(|w| {
            is_format_word(w)
                || is_year_only(w)
                || w.chars().all(|c| c.is_ascii_digit())
                || w.to_lowercase().ends_with("khz")
                || w.to_lowercase().ends_with("bit")
        })
}

/// The title with every bracketed group removed, as a last-resort clean title
/// when no artist/album split was found.
fn strip_tags(input: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in input.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.replace(['_', '.'], " ")
        .split_whitespace()
        .filter(|w| !is_format_word(w) && !is_year_only(w))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(input: &str) -> (Option<String>, Option<String>) {
        let p = parse_music_title(input);
        let m = p.music.expect("a music parse always has its music block");
        (m.artist, m.album)
    }

    fn quality(input: &str) -> Option<String> {
        parse_music_title(input).music.and_then(|m| m.quality)
    }

    #[test]
    fn the_spaced_layout_splits_artist_and_album_and_drops_tags() {
        assert_eq!(
            names("Radiohead - OK Computer (1997) [FLAC]"),
            (Some("Radiohead".into()), Some("OK Computer".into()))
        );
        assert_eq!(
            names("Boards of Canada - Music Has the Right to Children [1998] [MP3 320]"),
            (
                Some("Boards of Canada".into()),
                Some("Music Has the Right to Children".into())
            )
        );
    }

    #[test]
    fn a_year_between_the_names_is_a_tag_not_the_album() {
        assert_eq!(
            names("Burial - 2007 - Untrue [FLAC]"),
            (Some("Burial".into()), Some("Untrue".into()))
        );
        assert_eq!(parse_music_title("Burial - 2007 - Untrue [FLAC]").year, Some(2007));
    }

    #[test]
    fn a_parenthetical_that_is_part_of_the_name_is_kept() {
        assert_eq!(
            names("Nirvana - Nevermind (Deluxe Edition) (2011) FLAC"),
            (
                Some("Nirvana".into()),
                Some("Nevermind (Deluxe Edition)".into())
            )
        );
    }

    #[test]
    fn the_scene_layout_reads_underscores_as_spaces_and_the_tail_as_tags() {
        let p = parse_music_title("Aphex_Twin-Selected_Ambient_Works_85-92-CD-FLAC-1992-GRP");
        let m = p.music.clone().unwrap();
        assert_eq!(m.artist.as_deref(), Some("Aphex Twin"));
        assert_eq!(m.album.as_deref(), Some("Selected Ambient Works 85-92"));
        assert_eq!(m.quality.as_deref(), Some("FLAC"));
        assert_eq!(p.year, Some(1992));
        assert_eq!(p.group.as_deref(), Some("GRP"));
    }

    #[test]
    fn a_scene_title_with_no_format_states_no_quality() {
        let p = parse_music_title("Four_Tet-Rounds-WEB-2003-GRP");
        assert_eq!(names("Four_Tet-Rounds-WEB-2003-GRP").1.as_deref(), Some("Rounds"));
        assert_eq!(p.music.unwrap().quality, None);
    }

    #[test]
    fn an_album_named_after_a_year_keeps_its_name_and_the_release_year_wins() {
        let p = parse_music_title("Taylor Swift - 1989 (2014) [FLAC]");
        assert_eq!(p.music.as_ref().unwrap().album.as_deref(), Some("1989"));
        assert_eq!(p.year, Some(2014));
    }

    #[test]
    fn lossless_qualities_resolve_with_bit_depth() {
        assert_eq!(quality("A - B [FLAC]").as_deref(), Some("FLAC"));
        assert_eq!(quality("A - B [FLAC 24bit 96kHz]").as_deref(), Some("FLAC 24bit"));
        assert_eq!(quality("A - B (2020) [24-96 FLAC]").as_deref(), Some("FLAC 24bit"));
        assert_eq!(quality("A - B [FLAC 24 bit]").as_deref(), Some("FLAC 24bit"));
        assert_eq!(quality("A - B [ALAC]").as_deref(), Some("ALAC"));
        assert_eq!(quality("A - B [WAV]").as_deref(), Some("WAV"));
    }

    #[test]
    fn a_track_count_next_to_flac_is_not_a_bit_depth() {
        assert_eq!(quality("A - 24 Greatest Hits [FLAC]").as_deref(), Some("FLAC"));
    }

    #[test]
    fn mp3_resolves_only_when_the_bitrate_or_preset_is_stated() {
        assert_eq!(quality("A - B [MP3 320]").as_deref(), Some("MP3-320"));
        assert_eq!(quality("A - B (2001) 320kbps").as_deref(), Some("MP3-320"));
        assert_eq!(quality("A - B [MP3 V0]").as_deref(), Some("MP3-VBR-V0"));
        assert_eq!(quality("A - B [V2]").as_deref(), Some("MP3-VBR-V2"));
        assert_eq!(quality("A - B [MP3 192kbps]").as_deref(), Some("MP3-192"));
        assert_eq!(quality("A - B [MP3]"), None);
    }

    #[test]
    fn aac_and_vorbis_resolve_by_rate() {
        assert_eq!(quality("A - B [AAC 256]").as_deref(), Some("AAC-256"));
        assert_eq!(quality("A - B [iTunes M4A]").as_deref(), Some("AAC-256"));
        assert_eq!(quality("A - B [OGG Q8]").as_deref(), Some("OGG Vorbis Q8"));
    }

    #[test]
    fn a_discography_is_flagged_and_addresses_no_album() {
        let p = parse_music_title("Radiohead - Discography (1993-2016) [FLAC]");
        assert!(p.music.unwrap().discography);
        assert!(p.coordinates.is_empty());
    }

    #[test]
    fn an_album_release_addresses_the_album_unit() {
        let p = parse_music_title("Radiohead - OK Computer (1997) [FLAC]");
        assert_eq!(p.coordinates, vec![Coordinates::Album]);
    }

    #[test]
    fn a_title_with_no_separator_still_parses_without_names() {
        let p = parse_music_title("Radiohead OK Computer 1997 FLAC");
        let m = p.music.clone().unwrap();
        assert_eq!((m.artist, m.album), (None, None));
        assert_eq!(p.clean_title.as_deref(), Some("Radiohead OK Computer"));
        assert_eq!(m.quality.as_deref(), Some("FLAC"));
    }

    #[test]
    fn arbitrary_input_never_panics() {
        for input in ["", "-", " - ", "((((", "]]]", "a-b-c", "é - ü (", "24-", "-----"] {
            let _ = parse_music_title(input);
        }
    }
}

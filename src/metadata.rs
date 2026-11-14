use crate::config::{Config, MediaState};

/// Searches `haystack` for `needle` ignoring ASCII letter case (e.g., "FEAT" == "feat"),
/// returning the starting byte index without allocating any new strings on the heap.
pub fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Checks if `haystack` ends with `suffix` ignoring ASCII case.
/// Operates on `.as_bytes()` so it can never panic on multi-byte UTF-8 characters.
fn ends_with_ascii_case_insensitive(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack.as_bytes()[haystack.len() - suffix.len()..]
            .eq_ignore_ascii_case(suffix.as_bytes())
}

/// Cleans up common clutter added to track titles and artist names by web browsers
/// and streaming platforms (like YouTube's "- Topic" or "(Official Music Video)").
pub fn sanitize_metadata(raw_title: &str, raw_artist: &str) -> (String, String) {
    let mut title = raw_title.trim().to_string();
    let artist = raw_artist
        .trim()
        .strip_suffix(" - Topic")
        .unwrap_or(raw_artist)
        .trim()
        .to_string();

    // Remove browser tab platform suffixes from the end of the title.
    const PLATFORM_SUFFIXES: [&str; 4] = [
        " - YouTube Music",
        " - YouTube",
        " - Twitch",
        " | Spotify",
    ];
    for suf in PLATFORM_SUFFIXES {
        if title.len() > suf.len() && ends_with_ascii_case_insensitive(&title, suf) {
            title.truncate(title.len() - suf.len());
            let trimmed_len = title.trim_end().len();
            title.truncate(trimmed_len);
        }
    }

    // Common video tags to strip out of song titles.
    const NOISE_TAGS: [&str; 23] = [
        "(official music video)",
        "[official music video]",
        "(official video)",
        "[official video]",
        "(official audio)",
        "[official audio]",
        "(official lyric video)",
        "[official lyric video]",
        "(lyric video)",
        "[lyric video]",
        "(lyrics)",
        "[lyrics]",
        "(official visualizer)",
        "[official visualizer]",
        "(visualizer)",
        "[visualizer]",
        "(audio)",
        "[audio]",
        "(hd)",
        "[hd]",
        "(4k)",
        "[4k]",
        "[monstercat release]",
    ];

    // Repeatedly scan and remove noise tags until none remain.
    while let Some((idx, tag_len)) = NOISE_TAGS
        .iter()
        .find_map(|tag| find_ascii_case_insensitive(&title, tag).map(|i| (i, tag.len())))
    {
        let before = title[..idx].trim_end();
        let after = title[idx + tag_len..].trim_start();
        title = match (before.is_empty(), after.is_empty()) {
            (true, _) => after.to_string(),
            (_, true) => before.to_string(),
            (false, false) => format!("{before} {after}"),
        };
    }

    // If stripping tags left the title blank, fall back to the original title.
    if title.is_empty() {
        title = raw_title.trim().to_string();
    }
    (title, artist)
}

/// Formats seconds into `MM:SS` or `H:MM:SS` if the track is an hour or longer.
fn format_clock(sec: f64) -> String {
    let total = sec.max(0.0).floor() as u64;
    let hrs = total / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    if hrs > 0 {
        format!("{hrs}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

/// Replaces `{title}`, `{artist}`, etc. in the user's format string for the `/text` endpoint.
pub fn format_status_text(state: &MediaState, cfg: &Config, custom_fmt: Option<&str>) -> String {
    if !state.is_playing && !cfg.always_show {
        return cfg.text_idle.clone();
    }
    let template = custom_fmt.unwrap_or(&cfg.text_format);
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('}') {
            let key = &after_open[..close];
            match key {
                "title" => out.push_str(&state.title),
                "artist" => out.push_str(&state.artist),
                "album" => out.push_str(&state.album),
                "app" => out.push_str(&state.source_app),
                "pos" => out.push_str(&format_clock(state.position)),
                "dur" => out.push_str(&format_clock(state.duration)),
                "status" => out.push_str(if state.is_playing { "Playing" } else { "Paused" }),
                _ => {
                    // Unknown placeholder: leave it intact (e.g. "{custom}")
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            }
            rest = &after_open[close + 1..];
        } else {
            // Unclosed '{' bracket: copy the rest of the string as-is
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Extracts the lead artist before collaboration separators like "feat.", "&", or ",".
fn primary_artist_token(artist: &str) -> &str {
    let mut base = artist.trim();
    for sep in [",", "&", ";", "/", " feat.", " feat ", " ft.", " ft ", " with "] {
        if let Some(idx) = find_ascii_case_insensitive(base, sep) {
            let head = base[..idx].trim();
            if !head.is_empty() {
                base = head;
            }
        }
    }
    base
}

/// Checks if two artist strings refer to the same lead artist so album art doesn't
/// flicker during track transitions on the same album/artist.
pub fn artists_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.eq_ignore_ascii_case(b) || {
        let pa = primary_artist_token(a);
        !pa.is_empty() && pa.eq_ignore_ascii_case(primary_artist_token(b))
    }
}
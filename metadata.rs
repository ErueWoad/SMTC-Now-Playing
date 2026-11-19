use std::fmt::Write;

use crate::config::{Config, MediaState};

/// Searches `haystack` for `needle` ignoring ASCII letter case without heap allocations.
pub fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn ends_with_ascii_case_insensitive(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack.as_bytes()[haystack.len() - suffix.len()..]
            .eq_ignore_ascii_case(suffix.as_bytes())
}

/// Cleans common clutter added to track titles and artist names by web browsers
/// and streaming platforms.
pub fn sanitize_metadata(raw_title: &str, raw_artist: &str) -> (String, String) {
    let mut title = raw_title.trim().to_string();
    let artist = raw_artist
        .trim()
        .strip_suffix(" - Topic")
        .unwrap_or(raw_artist)
        .trim()
        .to_string();

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

    // Mutate `title` in-place via `replace_range` to avoid repeated reallocations.
    while let Some((idx, tag_len)) = NOISE_TAGS
        .iter()
        .find_map(|tag| find_ascii_case_insensitive(&title, tag).map(|i| (i, tag.len())))
    {
        let start = title[..idx].trim_end().len();
        let after_start = idx + tag_len;
        let ws_after = title[after_start..].len() - title[after_start..].trim_start().len();
        let end = after_start + ws_after;
        let replacement = if start > 0 && end < title.len() { " " } else { "" };
        title.replace_range(start..end, replacement);
    }

    if title.is_empty() {
        title = raw_title.trim().to_string();
    }
    (title, artist)
}

/// Appends formatted `MM:SS` or `H:MM:SS` directly into `out` with zero extra allocations.
fn write_clock(out: &mut String, sec: f64) {
    let total = sec.max(0.0).floor() as u64;
    let hrs = total / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    if hrs > 0 {
        let _ = write!(out, "{hrs}:{mins:02}:{secs:02}");
    } else {
        let _ = write!(out, "{mins:02}:{secs:02}");
    }
}

/// Replaces `{title}`, `{artist}`, etc. in the user's format string for `/text`.
pub fn format_status_text(
    state: &MediaState,
    live_pos: f64,
    cfg: &Config,
    custom_fmt: Option<&str>,
) -> String {
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
                "pos" => write_clock(&mut out, live_pos),
                "dur" => write_clock(&mut out, state.duration),
                "status" => out.push_str(if state.is_playing { "Playing" } else { "Paused" }),
                _ => {
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            }
            rest = &after_open[close + 1..];
        } else {
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

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
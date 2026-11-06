use crate::config::{Config, MediaState};

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

pub fn sanitize_metadata(raw_title: &str, raw_artist: &str) -> (String, String) {
    let mut title = raw_title.trim().to_string();
    let mut artist = raw_artist.trim().to_string();

    if let Some(stripped) = artist.strip_suffix(" - Topic") {
        artist = stripped.trim().to_string();
    }

    let platform_suffixes = [
        " - YouTube Music",
        " - YouTube",
        " - Twitch",
        " | Spotify",
    ];
    for suf in platform_suffixes {
        if title.len() > suf.len() && ends_with_ascii_case_insensitive(&title, suf) {
            title.truncate(title.len() - suf.len());
            title = title.trim().to_string();
        }
    }

    let noise_tags = [
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

    loop {
        let mut changed = false;
        for tag in noise_tags {
            if let Some(idx) = find_ascii_case_insensitive(&title, tag) {
                let before = title[..idx].trim_end();
                let after = title[idx + tag.len()..].trim_start();
                title = if before.is_empty() {
                    after.to_string()
                } else if after.is_empty() {
                    before.to_string()
                } else {
                    format!("{before} {after}")
                };
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }

    if title.is_empty() {
        title = raw_title.trim().to_string();
    }
    (title, artist)
}

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
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            }
            rest = &after_open[close + 1..];
        } else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
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
    let a = a.trim();
    let b = b.trim();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    let pa = primary_artist_token(a);
    let pb = primary_artist_token(b);
    !pa.is_empty() && pa.eq_ignore_ascii_case(pb)
}
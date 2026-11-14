use std::borrow::Cow;
use std::io::Cursor;

use axum::body::Bytes;
use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};
use tray_icon::Icon;

/// Converts Red/Green/Blue (0.0..=1.0) into Hue/Saturation/Value (0.0..=1.0).
/// HSV makes it easy to separate colorful pixels from dark/gray pixels.
fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0) / 6.0
    } else if max == g {
        (((b - r) / delta) + 2.0) / 6.0
    } else {
        (((r - g) / delta) + 4.0) / 6.0
    };

    let s = if max == 0.0 { 0.0 } else { delta / max };
    (h, s, max)
}

/// Converts Hue/Saturation/Value (0.0..=1.0) back into Red/Green/Blue (0.0..=1.0).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (v, v, v);
    }
    let h6 = h * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

fn to_u8_rgb(r: f32, g: f32, b: f32) -> [u8; 3] {
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

/// Analyzes album artwork by grouping pixels into 36 Hue buckets (10 degrees each)
/// and picking a vibrant primary and contrasting secondary accent color.
fn extract_vibrant_palette(img: &DynamicImage) -> ([u8; 3], [u8; 3]) {
    // Downscale to 64x64 (4,096 pixels) so color analysis finishes in microseconds.
    let small = img.resize_exact(64, 64, FilterType::Triangle).to_rgb8();

    #[derive(Clone, Copy, Default)]
    struct Bin {
        area_weight: f32,
        color_weight: f32,
        r: f32,
        g: f32,
        b: f32,
        sat_sum: f32,
    }

    let mut bins = [Bin::default(); 36];
    let (mut neutral_weight, mut neutral_r, mut neutral_g, mut neutral_b) = (0.0f32, 0.0, 0.0, 0.0);

    for pixel in small.pixels() {
        let (rf, gf, bf) = (
            pixel[0] as f32 / 255.0,
            pixel[1] as f32 / 255.0,
            pixel[2] as f32 / 255.0,
        );
        let (h, s, v) = rgb_to_hsv(rf, gf, bf);

        // Ignore nearly black pixels.
        if v < 0.15 {
            continue;
        }

        // Track low-saturation (grayscale/monochrome) pixels separately.
        if s < 0.16 {
            let nw = v.clamp(0.25, 0.90);
            neutral_weight += nw;
            neutral_r += rf * nw;
            neutral_g += gf * nw;
            neutral_b += bf * nw;
            continue;
        }

        let area_w = s.powf(0.8) * v.powf(0.9);
        let color_w = s.powf(2.0) * v.powf(1.8);

        let bin = &mut bins[((h * 36.0) as usize) % 36];
        bin.area_weight += area_w;
        bin.color_weight += color_w;
        bin.r += rf * color_w;
        bin.g += gf * color_w;
        bin.b += bf * color_w;
        bin.sat_sum += s * area_w;
    }

    let total_chroma_area: f32 = bins.iter().map(|b| b.area_weight).sum();

    // Smooth neighboring hue bins (wrapping around 0°/360°) to find dominant color clusters.
    let mut smoothed_area = [0.0f32; 36];
    for i in 0..36 {
        smoothed_area[i] = bins[(i + 35) % 36].area_weight * 0.25
            + bins[i].area_weight * 0.50
            + bins[(i + 1) % 36].area_weight * 0.25;
    }

    let (best_idx, best_area) = smoothed_area
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or((0, 0.0));

    // If the cover art is mostly black-and-white/grayscale, return soft monochrome accents.
    if best_area <= 0.0 || total_chroma_area < neutral_weight * 0.06 {
        if neutral_weight > 0.0 {
            let nr = (neutral_r / neutral_weight).clamp(0.65, 0.92);
            let ng = (neutral_g / neutral_weight).clamp(0.65, 0.92);
            let nb = (neutral_b / neutral_weight).clamp(0.65, 0.92);
            return (
                to_u8_rgb(nr, ng, nb),
                to_u8_rgb(nr * 0.68, ng * 0.68, nb * 0.68),
            );
        }
        return ([215, 215, 225], [145, 145, 160]);
    }

    // Averages a 3-bin cluster and boosts saturation/brightness so UI bars always pop.
    let sample_cluster = |center: usize| -> ([u8; 3], f32, f32, f32) {
        let (mut cw_sum, mut r_sum, mut g_sum, mut b_sum) = (0.0f32, 0.0, 0.0, 0.0);
        for i in [(center + 35) % 36, center, (center + 1) % 36] {
            cw_sum += bins[i].color_weight;
            r_sum += bins[i].r;
            g_sum += bins[i].g;
            b_sum += bins[i].b;
        }
        let denom = cw_sum.max(1e-5);
        let (h, s, v) = rgb_to_hsv(r_sum / denom, g_sum / denom, b_sum / denom);
        let out_s = (s * 1.15).clamp(0.42, 0.86);
        let out_v = (v * 1.18).clamp(0.68, 0.92);
        let (ro, go, bo) = hsv_to_rgb(h, out_s, out_v);
        (to_u8_rgb(ro, go, bo), h, out_s, out_v)
    };

    let (primary_rgb, h1, s1, v1) = sample_cluster(best_idx);

    // Find a secondary hue bin that is visually distinct from the primary hue.
    let second = (0..36)
        .filter_map(|idx| {
            let diff = (idx as i32 - best_idx as i32).abs();
            let circular_dist = diff.min(36 - diff);
            let area = smoothed_area[idx];

            if circular_dist < 4 || area < best_area * 0.05 || bins[idx].color_weight <= 0.0 {
                return None;
            }

            let avg_sat = (bins[idx].sat_sum / bins[idx].area_weight.max(1e-5)).clamp(0.2, 1.0);
            let contrast_bonus =
                0.45 + 0.55 * ((circular_dist as f32 / 18.0) * std::f32::consts::FRAC_PI_2).sin();
            Some((idx, area.powf(0.60) * avg_sat.powf(1.3) * contrast_bonus))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));

    let secondary_rgb = if let Some((sec_idx, _)) = second {
        sample_cluster(sec_idx).0
    } else {
        // Fallback: shift hue slightly and offset brightness to create a smooth gradient.
        let h2 = (h1 + 0.06).fract();
        let v2 = if v1 > 0.76 {
            (v1 - 0.20).max(0.52)
        } else {
            (v1 + 0.18).min(0.94)
        };
        let (r2, g2, b2) = hsv_to_rgb(h2, (s1 * 0.92).max(0.38), v2);
        to_u8_rgb(r2, g2, b2)
    };

    (primary_rgb, secondary_rgb)
}

/// Resizes album artwork to at most 256x256, extracts dual accent colors, encodes
/// the image as a PNG in memory, and computes a 64-bit FNV-1a hash for browser caching.
pub fn process_artwork(img: &DynamicImage) -> ([u8; 3], [u8; 3], Option<(Bytes, u64)>) {
    let (w, h) = img.dimensions();
    let resized: Cow<DynamicImage> = if w > 256 || h > 256 {
        Cow::Owned(img.resize(256, 256, FilterType::Lanczos3))
    } else {
        Cow::Borrowed(img)
    };

    let (a1, a2) = extract_vibrant_palette(&resized);

    let mut buf = Cursor::new(Vec::with_capacity(32 * 1024));
    if resized.write_to(&mut buf, ImageFormat::Png).is_err() {
        return (a1, a2, None);
    }
    let bytes = buf.into_inner();

    // FNV-1a 64-bit hash: fast, deterministic fingerprint of the PNG bytes.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in &bytes {
        hash = (hash ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }

    (a1, a2, Some((Bytes::from(bytes), hash.max(1))))
}

/// Procedurally draws a 64x64 pink rounded-square icon with a white play triangle in memory,
/// avoiding the need to ship external `.ico` or `.png` files alongside the executable.
pub fn create_tray_icon() -> Icon {
    const SIZE: u32 = 64;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);

            // Check if pixel lies inside a rounded rectangle with corner radius 14px.
            let in_rect = (6.0..=58.0).contains(&fx) && (6.0..=58.0).contains(&fy) && {
                let dx = fx - fx.clamp(20.0, 44.0);
                let dy = fy - fy.clamp(20.0, 44.0);
                (dx * dx + dy * dy) <= 196.0 // 14.0^2 = 196.0 (avoids `.sqrt()`)
            };

            if !in_rect {
                continue;
            }

            // Check if pixel lies inside the centered play triangle.
            let in_triangle =
                (24.0..=46.0).contains(&fx) && (fy - 32.0).abs() <= 14.0 * (46.0 - fx) / 22.0;

            let idx = ((y * SIZE + x) * 4) as usize;
            let color = if in_triangle {
                [255, 255, 255, 255]
            } else {
                [255, 15, 135, 255]
            };
            rgba[idx..idx + 4].copy_from_slice(&color);
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("Failed to create tray icon")
}
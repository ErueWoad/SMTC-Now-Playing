use std::borrow::Cow;
use std::io::Cursor;
use std::sync::Arc;

use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat};
use tray_icon::Icon;

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

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (v, v, v);
    }
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
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

fn extract_vibrant_palette(img: &DynamicImage) -> ([u8; 3], [u8; 3]) {
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
    let mut neutral_weight = 0.0f32;
    let mut neutral_r = 0.0f32;
    let mut neutral_g = 0.0f32;
    let mut neutral_b = 0.0f32;

    for pixel in small.pixels() {
        let rf = pixel[0] as f32 / 255.0;
        let gf = pixel[1] as f32 / 255.0;
        let bf = pixel[2] as f32 / 255.0;
        let (h, s, v) = rgb_to_hsv(rf, gf, bf);

        if v < 0.15 {
            continue;
        }

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

        let idx = ((h * 36.0) as usize) % 36;
        bins[idx].area_weight += area_w;
        bins[idx].color_weight += color_w;
        bins[idx].r += rf * color_w;
        bins[idx].g += gf * color_w;
        bins[idx].b += bf * color_w;
        bins[idx].sat_sum += s * area_w;
    }

    let total_chroma_area: f32 = bins.iter().map(|b| b.area_weight).sum();

    let mut smoothed_area = [0.0f32; 36];
    for i in 0..36 {
        let prev = bins[(i + 35) % 36].area_weight * 0.25;
        let curr = bins[i].area_weight * 0.50;
        let next = bins[(i + 1) % 36].area_weight * 0.25;
        smoothed_area[i] = prev + curr + next;
    }

    let (best_idx, best_area) = smoothed_area
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0, 0.0));

    if best_area <= 0.0 || total_chroma_area < neutral_weight * 0.06 {
        if neutral_weight > 0.0 {
            let nr = (neutral_r / neutral_weight).clamp(0.65, 0.92);
            let ng = (neutral_g / neutral_weight).clamp(0.65, 0.92);
            let nb = (neutral_b / neutral_weight).clamp(0.65, 0.92);
            let primary = [(nr * 255.0) as u8, (ng * 255.0) as u8, (nb * 255.0) as u8];
            let secondary = [
                (nr * 0.68 * 255.0) as u8,
                (ng * 0.68 * 255.0) as u8,
                (nb * 0.68 * 255.0) as u8,
            ];
            return (primary, secondary);
        }
        return ([215, 215, 225], [145, 145, 160]);
    }

    let sample_cluster = |center: usize| -> ([u8; 3], f32, f32, f32) {
        let indices = [(center + 35) % 36, center, (center + 1) % 36];
        let mut cw_sum = 0.0f32;
        let mut r_sum = 0.0f32;
        let mut g_sum = 0.0f32;
        let mut b_sum = 0.0f32;

        for &i in &indices {
            cw_sum += bins[i].color_weight;
            r_sum += bins[i].r;
            g_sum += bins[i].g;
            b_sum += bins[i].b;
        }

        let avg_r = r_sum / cw_sum.max(1e-5);
        let avg_g = g_sum / cw_sum.max(1e-5);
        let avg_b = b_sum / cw_sum.max(1e-5);
        let (h, s, v) = rgb_to_hsv(avg_r, avg_g, avg_b);

        let out_s = (s * 1.15).clamp(0.42, 0.86);
        let out_v = (v * 1.18).clamp(0.68, 0.92);

        let (ro, go, bo) = hsv_to_rgb(h, out_s, out_v);
        (
            [(ro * 255.0) as u8, (go * 255.0) as u8, (bo * 255.0) as u8],
            h,
            out_s,
            out_v,
        )
    };

    let (primary_rgb, h1, s1, v1) = sample_cluster(best_idx);

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

            let score = area.powf(0.60) * avg_sat.powf(1.3) * contrast_bonus;
            Some((idx, score))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let secondary_rgb = if let Some((sec_idx, _)) = second {
        sample_cluster(sec_idx).0
    } else {
        let h2 = (h1 + 0.06).fract();
        let v2 = if v1 > 0.76 {
            (v1 - 0.20).max(0.52)
        } else {
            (v1 + 0.18).min(0.94)
        };
        let (r2, g2, b2) = hsv_to_rgb(h2, (s1 * 0.92).max(0.38), v2);
        [(r2 * 255.0) as u8, (g2 * 255.0) as u8, (b2 * 255.0) as u8]
    };

    (primary_rgb, secondary_rgb)
}

pub fn process_artwork(img: &DynamicImage) -> ([u8; 3], [u8; 3], Option<(Arc<[u8]>, u64)>) {
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

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in &bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    if hash == 0 {
        hash = 1;
    }

    (a1, a2, Some((Arc::from(bytes), hash)))
}

pub fn create_tray_icon() -> Icon {
    let size = 64u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;

            let in_rect = if (6.0..=58.0).contains(&fx) && (6.0..=58.0).contains(&fy) {
                let cx = fx.clamp(20.0, 44.0);
                let cy = fy.clamp(20.0, 44.0);
                ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= 14.0
            } else {
                false
            };

            if !in_rect {
                continue;
            }

            let in_triangle = (24.0..=46.0).contains(&fx)
                && (fy - 32.0).abs() <= 14.0 * (46.0 - fx) / 22.0;

            let idx = ((y * size + x) * 4) as usize;
            if in_triangle {
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            } else {
                rgba[idx] = 255;
                rgba[idx + 1] = 15;
                rgba[idx + 2] = 135;
                rgba[idx + 3] = 255;
            }
        }
    }

    Icon::from_rgba(rgba, size, size).expect("Failed to create tray icon")
}
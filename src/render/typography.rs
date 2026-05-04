// render::typography -- title block, legends, metadata text.
//
// Emits a single <g> containing the lower data-row band and the
// top-corner attribution. The data row is anchored to the inner
// border: city name on the left, coordinates and meta (activity
// count + date range) on the right, separated by hairline rules. A
// scale bar sits in the lower-left corner as a cartographic legend.
// Positioning is derived from the viewBox so the block scales with
// poster size.

use svg::node::element::{Group, Line, Text};

use crate::render::theme::Theme;

/// Uppercase the string and let SVG `letter-spacing` handle the gaps.
fn uppercase_for_title(s: &str) -> String {
    s.to_uppercase()
}

/// Format `42.96°N · 85.67°W` from signed decimals. Two-decimal
/// precision is enough at hometown scale and reads cleaner than four
/// in the data row.
fn format_coords(lat: f64, lng: f64) -> String {
    let ns = if lat >= 0.0 { 'N' } else { 'S' };
    let ew = if lng >= 0.0 { 'E' } else { 'W' };
    format!(
        "{:.2}\u{00B0}{} \u{00B7} {:.2}\u{00B0}{}",
        lat.abs(),
        ns,
        lng.abs(),
        ew
    )
}

/// Mix two `#rrggbb` colors evenly. Used to derive a "secondary" text
/// color halfway between the theme's text and bg, so labels read as
/// supportive rather than competing with the title.
fn mix_hex(a: &str, b: &str) -> String {
    let parse = |s: &str| -> Option<(u8, u8, u8)> {
        let s = s.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let bl = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some((r, g, bl))
    };
    match (parse(a), parse(b)) {
        (Some((r1, g1, b1)), Some((r2, g2, b2))) => {
            let r = ((r1 as u16 + r2 as u16) / 2) as u8;
            let g = ((g1 as u16 + g2 as u16) / 2) as u8;
            let bl = ((b1 as u16 + b2 as u16) / 2) as u8;
            format!("#{r:02x}{g:02x}{bl:02x}")
        }
        _ => a.to_string(),
    }
}

/// Metadata for the data row. Built by `compose::render` from the
/// activity slice + projection so `typography::build` does not need to
/// reach back into those.
pub struct TypoMeta {
    /// Right-column secondary line. Typical: "30 ACTIVITIES · 2024".
    pub meta_text: String,
    /// SVG units per real-world meter at the projection's center
    /// latitude. Drives the scale-bar length.
    pub scale_per_meter: f64,
}

#[allow(clippy::too_many_arguments)]
pub fn build(
    city_name: &str,
    _country: &str,
    center_lat: f64,
    center_lng: f64,
    theme: &Theme,
    viewbox_w: f64,
    viewbox_h: f64,
    meta: &TypoMeta,
) -> Group {
    let text_color = theme.text.as_str();
    let bg_color = theme.bg.as_str();
    let secondary = mix_hex(text_color, bg_color);

    // Inset matches the border drawn in `compose.rs::render`. Keep these
    // two numbers in sync if either changes; the typography is anchored
    // to the inner edge of the border.
    let inset = (viewbox_w * 0.015).max(12.0);
    let inner_l = inset + viewbox_w * 0.030;
    let inner_r = viewbox_w - inset - viewbox_w * 0.030;

    // Sizes scale with viewbox so a 1200x1600 poster and a 600x800 card
    // both look balanced.
    let title_size = viewbox_w / 20.0;
    let data_size = viewbox_w / 67.0;
    let label_size = viewbox_w / 92.0;
    let attribution_size = viewbox_w / 133.0;

    // Lower data row.
    let title_y = viewbox_h * 0.93;
    let data_top_y = title_y - data_size - 6.0;
    let rule_y = title_y - title_size + 8.0;

    let fam = "'IBM Plex Sans', system-ui, sans-serif";

    // Hairline rule above the data band.
    let rule = Line::new()
        .set("x1", inner_l)
        .set("y1", rule_y)
        .set("x2", inner_r)
        .set("y2", rule_y)
        .set("stroke", secondary.as_str())
        .set("stroke-width", 0.6)
        .set("opacity", 0.5);

    // City title, left-aligned.
    let city = Text::new(uppercase_for_title(city_name))
        .set("x", inner_l)
        .set("y", title_y)
        .set("text-anchor", "start")
        .set("font-family", fam)
        .set("font-weight", 300)
        .set("font-size", title_size)
        .set("letter-spacing", "0.42em")
        .set("fill", text_color);

    // Coordinates, right-aligned, top of the right column.
    let coords = Text::new(format_coords(center_lat, center_lng))
        .set("x", inner_r)
        .set("y", data_top_y)
        .set("text-anchor", "end")
        .set("font-family", fam)
        .set("font-weight", 400)
        .set("font-size", data_size)
        .set("letter-spacing", "0.18em")
        .set("fill", text_color);

    // Meta line (e.g. "543 ACTIVITIES · 2024-2026"), right-aligned,
    // bottom of the right column.
    let meta_line = Text::new(meta.meta_text.clone())
        .set("x", inner_r)
        .set("y", title_y)
        .set("text-anchor", "end")
        .set("font-family", fam)
        .set("font-weight", 400)
        .set("font-size", label_size)
        .set("letter-spacing", "0.25em")
        .set("fill", secondary.as_str());

    // Scale bar: lower-left, just above the data row. Length picks the
    // largest "round" km value that fits in roughly 10% of viewbox_w.
    let bar_max_units = viewbox_w * 0.10;
    let candidate_kms: [u32; 7] = [1, 2, 5, 10, 20, 25, 50];
    let mut bar_km: u32 = 1;
    for &k in candidate_kms.iter().rev() {
        let len = meta.scale_per_meter * 1000.0 * (k as f64);
        if len <= bar_max_units {
            bar_km = k;
            break;
        }
    }
    let bar_len = (meta.scale_per_meter * 1000.0 * (bar_km as f64)) as f32;
    let bar_y = title_y - title_size * 1.2;
    let bar_x0 = inner_l;
    let tick = (data_size * 0.4) as f32;

    let bar_baseline = Line::new()
        .set("x1", bar_x0)
        .set("y1", bar_y)
        .set("x2", bar_x0 + bar_len as f64)
        .set("y2", bar_y)
        .set("stroke", text_color)
        .set("stroke-width", 1.4);
    let bar_tick_l = Line::new()
        .set("x1", bar_x0)
        .set("y1", bar_y - tick as f64)
        .set("x2", bar_x0)
        .set("y2", bar_y + tick as f64)
        .set("stroke", text_color)
        .set("stroke-width", 1.4);
    let bar_tick_r = Line::new()
        .set("x1", bar_x0 + bar_len as f64)
        .set("y1", bar_y - tick as f64)
        .set("x2", bar_x0 + bar_len as f64)
        .set("y2", bar_y + tick as f64)
        .set("stroke", text_color)
        .set("stroke-width", 1.4);
    let bar_label = Text::new(format!("{bar_km}\u{202F}KM"))
        .set("x", bar_x0 + bar_len as f64)
        .set("y", bar_y - tick as f64 - data_size * 0.3)
        .set("text-anchor", "end")
        .set("font-family", fam)
        .set("font-weight", 400)
        .set("font-size", label_size)
        .set("letter-spacing", "0.18em")
        .set("fill", text_color);
    let bar_zero = Text::new("0")
        .set("x", bar_x0)
        .set("y", bar_y - tick as f64 - data_size * 0.3)
        .set("text-anchor", "start")
        .set("font-family", fam)
        .set("font-weight", 400)
        .set("font-size", label_size)
        .set("fill", text_color);

    // Top-right attribution corner mark.
    let attribution = Text::new("PATINATE \u{00B7} OPENSTREETMAP")
        .set("x", inner_r)
        .set("y", inset + attribution_size * 1.5)
        .set("text-anchor", "end")
        .set("font-family", fam)
        .set("font-weight", 400)
        .set("font-size", attribution_size)
        .set("letter-spacing", "0.25em")
        .set("fill", secondary.as_str())
        .set("opacity", 0.85);

    Group::new()
        .set("class", "typography")
        .add(rule)
        .add(city)
        .add(coords)
        .add(meta_line)
        .add(bar_baseline)
        .add(bar_tick_l)
        .add(bar_tick_r)
        .add(bar_zero)
        .add(bar_label)
        .add(attribution)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercases_for_title() {
        assert_eq!(uppercase_for_title("ab"), "AB");
        assert_eq!(uppercase_for_title("Grand Rapids"), "GRAND RAPIDS");
    }

    #[test]
    fn formats_coords_with_hemispheres() {
        let s = format_coords(42.9634, -85.6681);
        assert!(s.contains("42.96"));
        assert!(s.ends_with("W"));
        assert!(s.contains("N"));
        let s2 = format_coords(-33.8688, 151.2093);
        assert!(s2.contains("S"));
        assert!(s2.ends_with("E"));
    }

    #[test]
    fn mix_hex_averages_channels() {
        assert_eq!(mix_hex("#000000", "#ffffff"), "#7f7f7f");
        // Pure red + pure blue mix to a magenta with both channels at half.
        assert_eq!(mix_hex("#ff0000", "#0000ff"), "#7f007f");
        // Idempotent on identical inputs.
        assert_eq!(mix_hex("#2c2417", "#2c2417"), "#2c2417");
    }
}

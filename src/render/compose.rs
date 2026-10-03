// render::compose — layer assembly: basemap, tracks, overlays.
//
// One pass over the OSM basemap, one pass over the activities. Output
// is a single SVG document string. Z-order (bottom to top): bg, water,
// parks, roads (residential -> motorway), heat, fades, typography.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::Result;
use svg::Document;
use svg::node::element::{
    Definitions, Description, Group, LinearGradient, Path, Rectangle, Stop, Title, Use,
};

use crate::config::ValidatedConfig;
use crate::obfuscation::ObfuscatedActivity;
use crate::osm::{Basemap, LatLon};
use crate::render::path::{Compact, signed_area2};
use crate::render::projection::{Fit, Projection};
use crate::render::theme::{HeatBlend, RoadTier, Theme};
use crate::render::typography;

/// Per-render overrides on top of the theme's baked values.
///
/// `bloom` and `alpha` both default to 1.0 = use the theme's baked
/// values exactly. Set `bloom = 0.0` to render sharp lines only (no
/// halo stack); set `bloom > 1.0` to amplify the soft halos for a
/// cloudier feel. The `alpha` multiplier scales just the sharp-core
/// layer's stroke opacity, useful when a theme reads too dim or too
/// dense at a given activity count.
///
/// `anonymize`: when set, suppress the per-activity `data-*` attrs on
/// heat paths and scrub the SVG `<desc>` to city+country only (no
/// exact center coords). Visible image is unchanged. Use whenever the
/// SVG itself will be published. See `CONTRIBUTING.md` for the
/// privacy threat model.
#[derive(Debug, Clone, Copy)]
pub struct HeatTuning {
    pub bloom: f32,
    pub alpha: f32,
    pub anonymize: bool,
}

impl Default for HeatTuning {
    fn default() -> Self {
        HeatTuning {
            bloom: 1.0,
            alpha: 1.0,
            anonymize: false,
        }
    }
}

/// Top-level entry. Returns the SVG document as a string.
/// - `heat_only`: skip basemap (water, parks, roads).
/// - `web`: web-embed preset — a lighter glow, compact merged paths
///   (`render::path::Compact`), no own typography. Targets a small
///   inline SVG for a consumer page.
/// - `transparent_bg`: skip the bg rect so the SVG paints over the
///   consumer page's own background. Combine with `web` for inline.
/// - `fit`: letterbox (`Contain`) or crop (`Cover`) the radius circle
///   into the viewbox.
/// - `tuning`: runtime knobs that scale the theme's bloom and core
///   alpha. See `HeatTuning` for semantics.
#[allow(clippy::too_many_arguments)]
pub fn render(
    config: &ValidatedConfig,
    basemap: &Basemap,
    activities: &[ObfuscatedActivity],
    theme: &Theme,
    heat_only: bool,
    web: bool,
    transparent_bg: bool,
    fit: Fit,
    tuning: HeatTuning,
) -> Result<String> {
    let viewbox_w = config.viewbox_width as f64;
    let viewbox_h = config.viewbox_height as f64;

    let proj = Projection::fit_radius(
        config.center_lat,
        config.center_lng,
        config.radius_m,
        config.viewbox_width,
        config.viewbox_height,
        fit,
    )?;

    // The web preset's compact path writer; posters keep plain paths.
    let compact = web.then(|| Compact::for_viewbox(viewbox_w, viewbox_h));
    let compact = compact.as_ref();

    // Defs: fade gradients.
    let defs = build_defs(theme);

    // Background.
    let bg = Rectangle::new()
        .set("x", 0)
        .set("y", 0)
        .set("width", viewbox_w)
        .set("height", viewbox_h)
        .set("fill", theme.bg.as_str());

    // Water polygons (lakes, ponds — fill).
    let water_polys = build_polygon_group(
        &basemap.water_polygons,
        theme.water.as_str(),
        "water",
        &proj,
        compact,
    );

    // Water lines (rivers, canals, streams — stroke). The Grand River is
    // here. The web preset leaves streams out: they are most of the
    // water bytes and read as noise at embed size.
    let streams = if web { &[][..] } else { &basemap.streams[..] };
    let water_lines = build_line_group(
        basemap.water_lines.iter().chain(streams),
        theme.water.as_str(),
        theme.water_line_width,
        "water-lines",
        &proj,
        compact,
    );

    // Parks (filled polygons).
    let parks = build_polygon_group(
        &basemap.parks,
        theme.parks.as_str(),
        "parks",
        &proj,
        compact,
    );

    // Roads (one sub-group per tier, drawn residential first so motorways
    // sit on top). Web mode drops tertiary + residential to keep the
    // inline SVG inside its file-size budget.
    let roads = build_roads(basemap, theme, &proj, web, compact);

    // Heat: one path group drawn by a three-layer <use> glow stack.
    // Web variant lightens the glow and writes compact paths.
    let heat = build_heat(activities, theme, &proj, web, compact, tuning);

    // Fades (top + bottom rects pointing at the gradients).
    let fades = build_fades(viewbox_w, viewbox_h, theme);

    // Typography. Build the meta line from the activity slice so the
    // data row reflects what actually rendered, not just what was
    // requested. Year span: earliest start_date year through latest.
    let activity_count = activities.len();
    let year_span = year_span(activities);
    let meta_text = match (activity_count, year_span) {
        (0, _) => "0 ACTIVITIES".to_string(),
        (n, Some((y0, y1))) if y0 == y1 => format!("{n} ACTIVITIES \u{00B7} {y0}"),
        (n, Some((y0, y1))) => format!("{n} ACTIVITIES \u{00B7} {y0}-{y1}"),
        (n, None) => format!("{n} ACTIVITIES"),
    };
    let typo = typography::build(
        &config.city_name,
        &config.country,
        config.center_lat,
        config.center_lng,
        theme,
        viewbox_w,
        viewbox_h,
        &typography::TypoMeta {
            meta_text,
            scale_per_meter: proj.scale_per_meter(),
        },
    );

    // Inset hairline border. Frames the basemap as a designed plate
    // rather than a raw screenshot. Inset matches the typography
    // inner-gutter calculation in `typography::build`.
    let inset = (viewbox_w * 0.015).max(12.0);
    let border_color = theme.text.as_str();
    let border = Rectangle::new()
        .set("x", inset)
        .set("y", inset)
        .set("width", viewbox_w - 2.0 * inset)
        .set("height", viewbox_h - 2.0 * inset)
        .set("fill", "none")
        .set("stroke", border_color)
        .set("stroke-width", 1.5)
        .set("stroke-opacity", 0.6);

    let title = Title::new(format!("{} heatmap", config.city_name));
    // city_name + country are user-supplied; the svg crate escapes
    // their contents when serialized (verified against svg-0.18 source),
    // so XSS exposure is nil even when they reach inline web embeds.
    //
    // Under `anonymize`, drop the precise center coords + radius so a
    // published SVG cannot be used to localize home via the <desc>.
    let desc_text = if tuning.anonymize {
        format!(
            "patinate poster for {}, {}.",
            config.city_name, config.country
        )
    } else {
        format!(
            "patinate poster for {}, {}. Center {:.4}, {:.4}, radius {:.0} m.",
            config.city_name, config.country, config.center_lat, config.center_lng, config.radius_m
        )
    };
    let desc = Description::new().add(svg::node::Text::new(desc_text));

    let mut doc = Document::new()
        .set("xmlns:xlink", "http://www.w3.org/1999/xlink")
        .set(
            "viewBox",
            (0i64, 0i64, config.viewbox_width, config.viewbox_height),
        )
        .set("width", config.viewbox_width)
        .set("height", config.viewbox_height)
        .add(title)
        .add(desc)
        .add(defs);
    if !transparent_bg {
        doc = doc.add(bg);
    }
    if !heat_only {
        doc = doc.add(water_polys).add(water_lines).add(parks).add(roads);
    }
    let mut doc = doc.add(heat).add(fades);
    if !web {
        // Web variant skips its own typography + border. The consumer
        // page (cycle.dunn.dev's hero, etc.) provides its own framing.
        doc = doc.add(border).add(typo);
    }
    let doc = doc;

    Ok(doc.to_string())
}

/// Earliest and latest year in the activity slice, or `None` if empty.
fn year_span(activities: &[ObfuscatedActivity]) -> Option<(i32, i32)> {
    let years = activities
        .iter()
        .map(|a| a.activity().start_date.format("%Y").to_string());
    let mut lo: Option<i32> = None;
    let mut hi: Option<i32> = None;
    for ys in years {
        if let Ok(y) = ys.parse::<i32>() {
            lo = Some(lo.map_or(y, |v| v.min(y)));
            hi = Some(hi.map_or(y, |v| v.max(y)));
        }
    }
    match (lo, hi) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

// --- helpers ----------------------------------------------------------

/// Resolve a `FadeStyle.to` ("transparent" or hex) into (color, opacity).
fn resolve_fade_to(s: &str) -> (&str, f32) {
    if s.eq_ignore_ascii_case("transparent") {
        // Color must still parse; pick the from-side color or black.
        ("#000000", 0.0)
    } else {
        (s, 1.0)
    }
}

fn build_defs(theme: &Theme) -> Definitions {
    let (top_to_color, top_to_opacity) = resolve_fade_to(&theme.fade_top.to);
    let (bot_to_color, bot_to_opacity) = resolve_fade_to(&theme.fade_bottom.to);

    let top_grad = LinearGradient::new()
        .set("id", "fadeTop")
        .set("x1", "0%")
        .set("y1", "0%")
        .set("x2", "0%")
        .set("y2", "100%")
        .add(
            Stop::new()
                .set("offset", "0%")
                .set("stop-color", theme.fade_top.from.as_str())
                .set("stop-opacity", "1"),
        )
        .add(
            Stop::new()
                .set("offset", "100%")
                .set("stop-color", top_to_color)
                .set("stop-opacity", top_to_opacity.to_string()),
        );

    let bot_grad = LinearGradient::new()
        .set("id", "fadeBottom")
        .set("x1", "0%")
        .set("y1", "100%")
        .set("x2", "0%")
        .set("y2", "0%")
        .add(
            Stop::new()
                .set("offset", "0%")
                .set("stop-color", theme.fade_bottom.from.as_str())
                .set("stop-opacity", "1"),
        )
        .add(
            Stop::new()
                .set("offset", "100%")
                .set("stop-color", bot_to_color)
                .set("stop-opacity", bot_to_opacity.to_string()),
        );

    Definitions::new().add(top_grad).add(bot_grad)
}

fn build_fades(viewbox_w: f64, viewbox_h: f64, theme: &Theme) -> Group {
    let top_h = viewbox_h * (theme.fade_top.height_pct as f64) / 100.0;
    let bot_h = viewbox_h * (theme.fade_bottom.height_pct as f64) / 100.0;

    let top_rect = Rectangle::new()
        .set("x", 0)
        .set("y", 0)
        .set("width", viewbox_w)
        .set("height", top_h)
        .set("fill", "url(#fadeTop)");

    let bot_rect = Rectangle::new()
        .set("x", 0)
        .set("y", viewbox_h - bot_h)
        .set("width", viewbox_w)
        .set("height", bot_h)
        .set("fill", "url(#fadeBottom)");

    Group::new()
        .set("class", "fades")
        .add(top_rect)
        .add(bot_rect)
}

/// Add one `<path>` per line to `g`, or under `compact` (the web
/// preset) a single merged path. Merging is lossless here: every line
/// in a basemap group shares one opaque style, and rings are turned to
/// one winding first so the nonzero rule fills them as a union.
fn add_lines<'a>(
    mut g: Group,
    lines: impl IntoIterator<Item = &'a Vec<LatLon>>,
    closed: bool,
    proj: &Projection,
    compact: Option<&Compact>,
) -> Group {
    let Some(c) = compact else {
        for line in lines {
            if line.len() < 2 {
                continue;
            }
            let d = polyline_to_path(line, proj, closed);
            if !d.is_empty() {
                g = g.add(Path::new().set("d", d));
            }
        }
        return g;
    };
    let mut d = String::new();
    for line in lines {
        let mut pts: Vec<(f64, f64)> = line.iter().map(|p| proj.project_latlon(*p)).collect();
        if closed && signed_area2(&pts) < 0.0 {
            pts.reverse();
        }
        c.write(&mut d, &pts, closed);
    }
    if !d.is_empty() {
        g = g.add(Path::new().set("d", d));
    }
    g
}

/// Build a closed-polygon group from a list of boundary rings.
fn build_polygon_group(
    rings: &[Vec<LatLon>],
    fill: &str,
    class: &str,
    proj: &Projection,
    compact: Option<&Compact>,
) -> Group {
    let g = Group::new()
        .set("class", class)
        .set("fill", fill)
        .set("stroke", "none");
    add_lines(g, rings, true, proj, compact)
}

/// Build one tier's stroke layer. Used by both the solid road pass
/// and the glow halo pass for major tiers.
fn build_road_layer(
    geoms: &[&Vec<LatLon>],
    stroke: &str,
    width: f32,
    class: &str,
    opacity: Option<f32>,
    proj: &Projection,
    compact: Option<&Compact>,
) -> Group {
    let mut g = Group::new()
        .set("class", class.to_string())
        .set("fill", "none")
        .set("stroke", stroke)
        .set("stroke-width", width)
        .set("stroke-linecap", "round")
        .set("stroke-linejoin", "round");
    if let Some(o) = opacity {
        g = g.set("opacity", o);
    }
    add_lines(g, geoms.iter().copied(), false, proj, compact)
}

/// Build an open-line group from a list of polylines (rivers, streams).
fn build_line_group<'a>(
    lines: impl IntoIterator<Item = &'a Vec<LatLon>>,
    stroke: &str,
    width: f32,
    class: &str,
    proj: &Projection,
    compact: Option<&Compact>,
) -> Group {
    let g = Group::new()
        .set("class", class)
        .set("fill", "none")
        .set("stroke", stroke)
        .set("stroke-width", width)
        .set("stroke-linecap", "round")
        .set("stroke-linejoin", "round");
    add_lines(g, lines, false, proj, compact)
}

/// Build the road group: one sub-group per tier, residential first.
/// `web` skips the two minor tiers (tertiary + residential) so all
/// `--web` semantics live in one place.
fn build_roads(
    basemap: &Basemap,
    theme: &Theme,
    proj: &Projection,
    web: bool,
    compact: Option<&Compact>,
) -> Group {
    // Bucket roads by tier in a single pass.
    let mut buckets: HashMap<RoadTier, Vec<&Vec<LatLon>>> = HashMap::new();
    for road in &basemap.roads {
        if web && matches!(road.tier, RoadTier::Tertiary | RoadTier::Residential) {
            continue;
        }
        buckets.entry(road.tier).or_default().push(&road.geometry);
    }

    let order = [
        RoadTier::Residential,
        RoadTier::Tertiary,
        RoadTier::Secondary,
        RoadTier::Primary,
        RoadTier::Trunk,
        RoadTier::Motorway,
    ];

    let mut roads = Group::new().set("class", "roads");
    for tier in order {
        let style = theme.road_style(tier);
        let class = tier_class(tier);
        if let Some(geoms) = buckets.get(&tier) {
            // Glow halo for major tiers: same color, ~2x width, ~25%
            // opacity. Renders BEFORE the solid stroke so the dark line
            // sits on top of its own halo. Halo bridges OSM data gaps
            // visually (e.g. Paul B Henry Freeway, M-6 GR) without
            // implying false geometry — the halo just softens the edge,
            // it doesn't draw a line where there isn't one.
            // Major-road halo: dark themes only. Cream-paper themes get
            // no halo because the dark sepia road color is high-contrast
            // enough on cream that an extra glow reads as a shadow rather
            // than a presence-amplifier.
            if matches!(tier, RoadTier::Motorway | RoadTier::Trunk) && !theme.bg.is_light() {
                roads = roads.add(build_road_layer(
                    geoms,
                    style.color.as_str(),
                    style.width * 1.1,
                    &format!("{class}-glow"),
                    Some(0.05),
                    proj,
                    compact,
                ));
            }
            // Solid stroke.
            roads = roads.add(build_road_layer(
                geoms,
                style.color.as_str(),
                style.width,
                class,
                None,
                proj,
                compact,
            ));
        }
    }
    roads
}

fn tier_class(tier: RoadTier) -> &'static str {
    match tier {
        RoadTier::Motorway => "motorway",
        RoadTier::Trunk => "trunk",
        RoadTier::Primary => "primary",
        RoadTier::Secondary => "secondary",
        RoadTier::Tertiary => "tertiary",
        RoadTier::Residential => "residential",
    }
}

/// Build the heat group: the ride paths once under `#heat-paths`, then
/// three `<use>` layers of it for the painterly glow stack:
///   outer halo (`heat.glow_outer_ratio` x width, `heat.glow_outer_alpha`)
///   inner halo (`heat.glow_inner_ratio` x width, `heat.glow_inner_alpha`)
///   sharp core (1.0 x width, `heat.alpha`)
/// Defaults are tuned for dark themes; cream-paper themes (cycle_heat,
/// warm_beige) lift the halo opacity in their JSON so the bloom registers
/// against a warm background.
///
/// **Web (web=true):** halo opacity halved and paths written by
/// `Compact`, each ride still its own path so overlaps build heat.
/// `bloom = 0` keeps only the core.
///
/// **Privacy invariant:** activities with empty `clipped_segments` are
/// dropped, never falling back to the raw `summary_polyline`. The
/// renderer only ever paints from the obfuscation-pipeline's output.
fn build_heat(
    activities: &[ObfuscatedActivity],
    theme: &Theme,
    proj: &Projection,
    web: bool,
    compact: Option<&Compact>,
    tuning: HeatTuning,
) -> Group {
    // Path-break threshold in viewbox units, derived from the projection
    // so the value scales with viewport and radius. 600 m is wider than
    // any plausible bike-trail bridge but narrower than the Grand River
    // crossings that tripped the old fixed-30-unit threshold at smaller
    // radii.
    const MAX_GAP_METERS: f64 = 600.0;
    let max_gap_units = proj.scale_per_meter() * MAX_GAP_METERS;

    let mut decoded: Vec<(&ObfuscatedActivity, String)> = Vec::with_capacity(activities.len());
    for obf in activities {
        // Privacy gate: the obfuscation pipeline is the only source of
        // truth for what enters the SVG. An empty segment list means
        // either "activity entirely inside the home circle" or "no
        // polyline at all"; both must be dropped, not silently replaced
        // with the raw polyline.
        if obf.segments().is_empty() {
            continue;
        }

        // L segments with sparse-polyline guard. The guard breaks the
        // path (M instead of L) when consecutive Strava points jump
        // farther than rides could realistically cover, which keeps heat
        // off rivers and lakes that the polyline accidentally bridges.
        // Each clipped segment also becomes its own M sub-path so the
        // renderer never bridges across an obfuscation gap.
        let mut d = String::new();
        for seg in obf.segments() {
            if seg.len() < 2 {
                continue;
            }
            let projected: Vec<(f64, f64)> = seg
                .iter()
                .map(|&(lat, lng)| proj.project(lat, lng))
                .collect();
            if let Some(c) = compact {
                for run in split_at_gaps(&projected, max_gap_units) {
                    c.write(&mut d, run, false);
                }
                continue;
            }
            let piece = points_to_heat_path(&projected, max_gap_units);
            if piece.is_empty() {
                continue;
            }
            if !d.is_empty() {
                d.push(' ');
            }
            d.push_str(&piece);
        }
        if !d.is_empty() {
            decoded.push((obf, d));
        }
    }

    // Apply the runtime tuning. `bloom` scales BOTH the halo widths and
    // their alphas (so bloom=0 collapses to no-halo, bloom=2 doubles
    // both extent and presence). `alpha` scales just the sharp-core
    // opacity. Tuning factors clamp at 0 to keep stroke-opacity sane.
    // The web preset keeps the halos at `WEB_GLOW` of their opacity: a
    // lighter glow that costs a few hundred bytes, since every layer
    // reuses the one path group.
    const WEB_GLOW: f32 = 0.5;
    let bloom = tuning.bloom.max(0.0);
    let halo_alpha = |a: f32| (a * bloom * if web { WEB_GLOW } else { 1.0 }).min(1.0);
    let core_alpha = (theme.heat.alpha * tuning.alpha.max(0.0)).min(1.0);
    let core_layer = ("heat-core", theme.heat.width, core_alpha);

    let layers: Vec<(&str, f32, f32)> = if bloom == 0.0 {
        vec![core_layer]
    } else {
        vec![
            (
                "heat-outer",
                theme.heat.width * theme.heat.glow_outer_ratio * bloom,
                halo_alpha(theme.heat.glow_outer_alpha),
            ),
            (
                "heat-inner",
                theme.heat.width * theme.heat.glow_inner_ratio * bloom,
                halo_alpha(theme.heat.glow_inner_alpha),
            ),
            core_layer,
        ]
    };

    // The ride paths are written once, bare, under `#heat-paths`; each
    // layer is a `<use>` whose stroke, width and opacity the clone
    // inherits. A path that set any of those itself would pin every
    // layer to one value, so paths carry only `d` and data attrs.
    let mut paths = Group::new().set("id", "heat-paths").set("class", "heat");
    for (obf, d) in &decoded {
        let act = obf.activity();
        let mut p = Path::new().set("d", d.clone());
        // data-* attrs feed consumer JS (`.heat path[data-type]`).
        // `anonymize` suppresses them entirely so a published SVG
        // cannot be linked back to the operator's Strava profile via
        // athlete_id or gear_id.
        if !tuning.anonymize {
            p = p
                .set("data-rider", act.athlete_id.to_string())
                .set("data-bike", act.gear_id.clone().unwrap_or_default())
                .set("data-year", act.start_date.format("%Y").to_string())
                .set("data-type", act.activity_type.as_data_attr());
        }
        paths = paths.add(p);
    }

    let mut stack = Group::new()
        .set("class", "heat-stack")
        .set("fill", "none")
        .set("stroke", theme.heat.color.as_str())
        .set("stroke-linecap", "round")
        .set("stroke-linejoin", "round")
        .add(Definitions::new().add(paths));
    for (class, width, alpha) in layers {
        let mut u = Use::new()
            .set("href", "#heat-paths")
            .set("class", class)
            .set("stroke-width", width)
            .set("stroke-opacity", alpha);
        // Halos composite normally (a wash, no darkening). The sharp
        // core takes the theme's blend: multiply presses rides into
        // cream paper, screen lifts them off a dark ground.
        if class == "heat-core" {
            let blend = theme.heat_blend();
            if blend != HeatBlend::Normal {
                u = u.set("style", format!("mix-blend-mode: {}", blend.as_css()));
            }
        }
        stack = stack.add(u);
    }
    stack
}

/// Project a lat/lon ring to an SVG path-`d` string. If `closed`,
/// appends `Z` so the resulting path can be filled cleanly. We hand-roll
/// the string for both speed and consistent rounding (matches the heat
/// layer's formatter).
fn polyline_to_path(ring: &[LatLon], proj: &Projection, closed: bool) -> String {
    let pts: Vec<(f64, f64)> = ring.iter().map(|p| proj.project_latlon(*p)).collect();
    if pts.len() < 2 {
        return String::new();
    }
    let mut s = points_to_path_string(&pts);
    if closed {
        s.push_str(" Z");
    }
    s
}

/// Build an SVG path-`d` string from already-projected (x, y) points.
/// Used for basemap layers (roads, water, parks) where every segment
/// is real geometry that we want preserved as-is.
fn points_to_path_string(pts: &[(f64, f64)]) -> String {
    if pts.len() < 2 {
        return String::new();
    }
    let mut s = String::with_capacity(pts.len() * 16);
    let (x0, y0) = pts[0];
    let _ = write!(s, "M{:.2} {:.2}", x0, y0);
    for &(x, y) in &pts[1..] {
        let _ = write!(s, " L{:.2} {:.2}", x, y);
    }
    s
}

/// Split a projected ride into runs wherever consecutive points sit
/// farther apart than `max_gap_units` (see `points_to_heat_path`).
fn split_at_gaps(pts: &[(f64, f64)], max_gap_units: f64) -> Vec<&[(f64, f64)]> {
    let max_gap_sq = max_gap_units * max_gap_units;
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..pts.len() {
        let (dx, dy) = (pts[i].0 - pts[i - 1].0, pts[i].1 - pts[i - 1].1);
        if dx * dx + dy * dy > max_gap_sq {
            runs.push(&pts[start..i]);
            start = i;
        }
    }
    runs.push(&pts[start..]);
    runs
}

/// Heat-only variant. Strava `summary_polyline` is heavily decimated;
/// for some long rides two consecutive points end up far apart and a
/// straight `L` between them draws across geography the ride doesn't
/// actually cross. When a segment exceeds `max_gap_units`, emit `M`
/// to break the path instead of bridging the gap.
///
/// Poster output only, at two decimals; the web preset splits rides
/// with `split_at_gaps` and writes them with `Compact`.
///
/// `max_gap_units` is the threshold in projected SVG units beyond which
/// consecutive points are treated as discontinuous. The caller derives
/// this from the projection (units-per-meter * threshold-in-meters) so
/// the value scales with viewport and radius rather than being a magic
/// number tuned to a single resolution.
fn points_to_heat_path(pts: &[(f64, f64)], max_gap_units: f64) -> String {
    let max_gap_sq = max_gap_units * max_gap_units;
    if pts.len() < 2 {
        return String::new();
    }
    let mut s = String::with_capacity(pts.len() * 16);
    let (x0, y0) = pts[0];
    let _ = write!(s, "M{x0:.2} {y0:.2}");
    let mut prev = pts[0];
    for &(x, y) in &pts[1..] {
        let dx = x - prev.0;
        let dy = y - prev.1;
        let cmd = if dx * dx + dy * dy > max_gap_sq {
            'M'
        } else {
            'L'
        };
        let _ = write!(s, " {cmd}{x:.2} {y:.2}");
        prev = (x, y);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::obfuscation::{self, ObfuscationParams};
    use crate::osm;
    use crate::render::theme;
    use crate::strava::Activity;

    use crate::strava::ActivityType;
    use chrono::Utc;

    fn synthetic_activity(id: i64, polyline: String) -> Activity {
        Activity {
            id,
            name: format!("synthetic ride {id}"),
            activity_type: ActivityType::Ride,
            start_date: Utc::now(),
            start_lat: 42.95,
            start_lng: -85.73,
            distance_m: 5000.0,
            moving_time_s: 600,
            summary_polyline: polyline,
            athlete_id: 1,
            gear_id: Some("bSCRUBBED1".to_string()),
        }
    }

    /// End-to-end SVG validity check that runs by default (no #[ignore]).
    /// Uses synthetic inputs (empty basemap, two short rides) so it
    /// finishes in milliseconds while still exercising the render →
    /// document → string path. Validates the output is well-formed XML
    /// and carries the expected class structure.
    #[test]
    fn render_e2e_synthetic_is_well_formed() {
        let cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load_named("noir_heat", None).expect("embedded theme loads");
        let basemap = Basemap::default();

        let coords = vec![
            geo_types::Coord {
                x: -85.73_f64,
                y: 42.95_f64,
            },
            geo_types::Coord {
                x: -85.72,
                y: 42.955,
            },
            geo_types::Coord {
                x: -85.71,
                y: 42.98,
            },
        ];
        let polyline_str = polyline::encode_coordinates(coords, 5).expect("encode");
        let activities = vec![
            synthetic_activity(1, polyline_str.clone()),
            synthetic_activity(2, polyline_str),
        ];
        let obf = obfuscation::apply(
            activities,
            ObfuscationParams {
                home_lat: cfg.home_lat,
                home_lng: cfg.home_lng,
                radius_m: 0.0,
                salt: None,
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        assert_eq!(obf.len(), 2, "synthetic activities should both survive");

        let svg = render(
            &cfg,
            &basemap,
            &obf,
            &theme,
            false,
            false,
            false,
            Fit::Contain,
            HeatTuning::default(),
        )
        .expect("render ok");

        // Structural sanity: open + close tags, expected layers.
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));
        assert!(svg.contains("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("class=\"heat-stack\""));
        assert!(svg.contains("class=\"heat\""));
        assert!(svg.contains("class=\"fades\""));
        // data-* attrs are emitted on the sharp-core heat layer.
        assert!(svg.contains("data-rider=\"1\""));
        assert!(svg.contains("data-bike=\"bSCRUBBED1\""));
        assert!(svg.contains("data-type=\"Ride\""));

        // Web variant: no typography, and a lighter glow. Every layer
        // is a <use> of the one path group, so each ride's path is
        // written once whatever the layer count.
        let svg_web = render(
            &cfg,
            &basemap,
            &obf,
            &theme,
            false,
            true,
            true,
            Fit::Contain,
            HeatTuning::default(),
        )
        .expect("render web");
        for layer in ["heat-outer", "heat-inner", "heat-core"] {
            assert_eq!(
                svg_web.matches(&format!("class=\"{layer}\"")).count(),
                1,
                "{layer} layer in the web variant"
            );
        }
        assert_eq!(svg.matches("data-rider=\"1\"").count(), 2);
        // Every hop in this sparse ride breaks at the 600 m gap, so it
        // draws nothing; the web writer drops it rather than ship a
        // path of bare movetos.
        assert_eq!(svg_web.matches("data-rider=\"1\"").count(), 0);
        let outer_alpha = |s: &str| -> f32 {
            let tail = &s[s.find("class=\"heat-outer\"").expect("outer")..];
            let at = tail.find("stroke-opacity=\"").expect("opacity") + 16;
            tail[at..at + tail[at..].find('"').unwrap()]
                .parse()
                .unwrap()
        };
        assert!(
            outer_alpha(&svg_web) < outer_alpha(&svg),
            "web glow should be lighter than the poster's"
        );
    }

    /// Posters stroke streams with the rivers; the web preset leaves
    /// them out and keeps the rivers.
    #[test]
    fn web_drops_streams_keeps_rivers() {
        let cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load_named("noir_heat", None).expect("embedded theme loads");
        let line = |lng: f64| {
            vec![
                LatLon {
                    lat: 42.95,
                    lon: lng,
                },
                LatLon {
                    lat: 42.97,
                    lon: lng,
                },
            ]
        };
        let basemap = Basemap {
            water_lines: vec![line(-85.74)],
            streams: vec![line(-85.72)],
            ..Basemap::default()
        };
        let water_lines = |web: bool| -> String {
            let svg = render(
                &cfg,
                &basemap,
                &[],
                &theme,
                false,
                web,
                false,
                Fit::Contain,
                HeatTuning::default(),
            )
            .expect("render ok");
            let start = svg
                .find("class=\"water-lines\"")
                .expect("water-lines group");
            let end = start + svg[start..].find("</g>").expect("group close");
            svg[start..end].to_string()
        };
        // Each line is one M..L run, merged or not.
        assert_eq!(water_lines(false).matches('M').count(), 2, "poster");
        assert_eq!(water_lines(true).matches('M').count(), 1, "web");
    }

    /// The SVG between the `#heat-paths` group's open and close tags.
    fn heat_paths_inner(svg: &str) -> &str {
        let open = "<g class=\"heat\" id=\"heat-paths\">";
        let start = svg.find(open).expect("heat-paths group") + open.len();
        let end = start + svg[start..].find("</g>").expect("group close");
        &svg[start..end]
    }

    /// Small synthetic heat-only render on a 300x300 viewbox.
    fn small_heat_render(web: bool) -> String {
        let mut cfg = config::load("fixtures/config.toml").expect("config loads");
        cfg.viewbox_width = 300;
        cfg.viewbox_height = 300;
        cfg.radius_m = 4000.0;
        let theme = theme::load_named("cycle_heat", None).expect("embedded theme loads");
        // Densify each leg to ~100 m steps so no hop trips the 600 m
        // gap break and the rides draw as continuous lines.
        let coords = |pts: &[(f64, f64)]| {
            let mut c = Vec::new();
            for w in pts.windows(2) {
                for i in 0..20 {
                    let t = i as f64 / 20.0;
                    c.push(geo_types::Coord {
                        x: w[0].1 + (w[1].1 - w[0].1) * t,
                        y: w[0].0 + (w[1].0 - w[0].0) * t,
                    });
                }
            }
            let &(y, x) = pts.last().unwrap();
            c.push(geo_types::Coord { x, y });
            polyline::encode_coordinates(c, 5).expect("encode")
        };
        let activities = vec![
            synthetic_activity(
                1,
                coords(&[(42.95, -85.74), (42.96, -85.73), (42.97, -85.72)]),
            ),
            synthetic_activity(
                2,
                coords(&[(42.97, -85.76), (42.95, -85.72), (42.94, -85.70)]),
            ),
        ];
        let obf = obfuscation::apply(
            activities,
            ObfuscationParams {
                home_lat: cfg.home_lat,
                home_lng: cfg.home_lng,
                radius_m: 0.0,
                salt: None,
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        let tuning = HeatTuning {
            bloom: 1.8,
            ..HeatTuning::default()
        };
        render(
            &cfg,
            &Basemap::default(),
            &obf,
            &theme,
            true,
            web,
            false,
            Fit::Contain,
            tuning,
        )
        .expect("render ok")
    }

    #[test]
    fn frame_ignores_activities_and_home() {
        // Rides cluster around home, so a frame fitted to them would
        // point at it. Everything drawn before the heat (viewBox,
        // basemap) must come out the same whatever the rides and home.
        let cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load_named("cycle_heat", None).expect("theme loads");
        let basemap = osm::load("fixtures/grand-rapids.osm.json.gz").expect("osm loads");
        let raw = std::fs::read_to_string("fixtures/activities.json").expect("fixture");
        let rides: Vec<Activity> = serde_json::from_str(&raw).expect("activities parse");
        let frame = |cfg: &ValidatedConfig, rides: Vec<Activity>| -> String {
            let obf = obfuscation::apply(
                rides,
                ObfuscationParams {
                    home_lat: cfg.home_lat,
                    home_lng: cfg.home_lng,
                    radius_m: 0.0,
                    salt: None,
                    offset_m: 750.0,
                },
            )
            .expect("obfuscate");
            let svg = render(
                cfg,
                &basemap,
                &obf,
                &theme,
                false,
                true,
                true,
                Fit::Cover,
                HeatTuning::default(),
            )
            .expect("render ok");
            svg[..svg.find("<g class=\"heat-stack\"").expect("heat")].to_string()
        };
        let base = frame(&cfg, rides.clone());
        assert!(base.contains("class=\"roads\""), "basemap must draw");

        let mut moved = cfg.clone();
        moved.home_lat += 0.2;
        moved.home_lng -= 0.3;
        let east: Vec<Activity> = rides[..rides.len() / 4].to_vec();
        assert_eq!(frame(&moved, Vec::new()), base, "no rides, home moved");
        assert_eq!(frame(&cfg, east), base, "a quarter of the rides");
    }

    #[test]
    fn heat_paths_set_no_presentation_attrs() {
        for web in [false, true] {
            let svg = small_heat_render(web);
            let inner = heat_paths_inner(&svg);
            assert_eq!(inner.matches("<path").count(), 2);
            for attr in ["stroke", "fill", "opacity", "style"] {
                assert!(!inner.contains(attr), "a heat path sets {attr}: {inner}");
            }
            assert_eq!(svg.matches("<use ").count(), 3);
        }
    }

    /// Rasterizes the use-stack render and the same render with every
    /// `<use>` expanded into a group holding a copy of the paths (the
    /// shape the stack had before), and compares the pixels. Needs
    /// `rsvg-convert` on PATH; `just check-pixels` runs it.
    #[test]
    #[ignore = "needs rsvg-convert; run with `just check-pixels`"]
    fn use_stack_matches_inlined_layers_in_pixels() {
        let svg = small_heat_render(false);
        let inner = heat_paths_inner(&svg).to_string();
        let defs_start = svg.find("<defs>\n<g class=\"heat\"").expect("heat defs");
        let defs_end = defs_start + svg[defs_start..].find("</defs>").unwrap() + "</defs>".len();
        let mut inlined = format!("{}{}", &svg[..defs_start], &svg[defs_end..]);
        while let Some(at) = inlined.find("<use ") {
            let end = at + inlined[at..].find("/>").unwrap();
            let attrs = inlined[at + 5..end].replace("href=\"#heat-paths\" ", "");
            inlined.replace_range(at..end + 2, &format!("<g {attrs}>{inner}</g>"));
        }
        assert!(!inlined.contains("heat-paths"));

        let dir = std::env::temp_dir().join(format!("patinate-pixels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raster = |name: &str, body: &str| -> Vec<u8> {
            let src = dir.join(format!("{name}.svg"));
            let png = dir.join(format!("{name}.png"));
            std::fs::write(&src, body).unwrap();
            let ok = std::process::Command::new("rsvg-convert")
                .arg(&src)
                .arg("-o")
                .arg(&png)
                .status()
                .expect("rsvg-convert runs")
                .success();
            assert!(ok, "rsvg-convert failed on {name}");
            std::fs::read(&png).unwrap()
        };
        let a = raster("use", &svg);
        let b = raster("inlined", &inlined);
        let no_heat: String = svg.lines().filter(|l| !l.starts_with("<use ")).collect();
        let bare = raster("bare", &no_heat);
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            a != bare,
            "the heat must draw, or the comparison proves nothing"
        );
        assert!(a == b, "use stack and inlined layers rasterize differently");
    }

    #[test]
    fn render_never_emits_privacy_salt() {
        // The salt seeds the hidden zone; it must not reach the SVG.
        let cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load_named("noir_heat", None).expect("embedded theme loads");
        let secret = "synthetic-salt-must-not-render";
        let coords = (0..=120).map(|i| geo_types::Coord {
            x: -85.78 + 0.001 * i as f64,
            y: 42.95,
        });
        let polyline_str = polyline::encode_coordinates(coords, 5).expect("encode");
        let obf = obfuscation::apply(
            vec![synthetic_activity(1, polyline_str)],
            ObfuscationParams {
                home_lat: 42.95,
                home_lng: -85.72,
                radius_m: 250.0,
                salt: Some(obfuscation::PrivacySalt::new(secret).expect("salt")),
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        assert_eq!(obf[0].segments().len(), 2, "route should be cut in two");
        for anonymize in [false, true] {
            let tuning = HeatTuning {
                anonymize,
                ..HeatTuning::default()
            };
            let svg = render(
                &cfg,
                &Basemap::default(),
                &obf,
                &theme,
                false,
                false,
                false,
                Fit::Contain,
                tuning,
            )
            .expect("render ok");
            assert!(
                !svg.contains(secret),
                "salt leaked (anonymize = {anonymize})"
            );
        }
    }

    #[test]
    fn anonymize_strips_data_attrs_and_scrubs_desc() {
        let cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load_named("noir_heat", None).expect("embedded theme loads");
        let basemap = Basemap::default();
        let coords = vec![
            geo_types::Coord {
                x: -85.73_f64,
                y: 42.95_f64,
            },
            geo_types::Coord {
                x: -85.72,
                y: 42.955,
            },
        ];
        let polyline_str = polyline::encode_coordinates(coords, 5).expect("encode");
        let activities = vec![synthetic_activity(1, polyline_str)];
        let obf = obfuscation::apply(
            activities,
            ObfuscationParams {
                home_lat: cfg.home_lat,
                home_lng: cfg.home_lng,
                radius_m: 0.0,
                salt: None,
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        let tuning = HeatTuning {
            anonymize: true,
            ..HeatTuning::default()
        };
        let svg = render(
            &cfg,
            &basemap,
            &obf,
            &theme,
            false,
            false,
            false,
            Fit::Contain,
            tuning,
        )
        .expect("render");
        assert!(
            !svg.contains("data-rider"),
            "data-rider must be stripped under --anonymize"
        );
        assert!(!svg.contains("data-bike"), "data-bike must be stripped");
        assert!(!svg.contains("data-year"));
        assert!(!svg.contains("data-type"));
        // <desc> must NOT contain the precise center coords.
        assert!(
            !svg.contains("Center"),
            "<desc> coord scrubbing failed: {svg}"
        );
        assert!(
            svg.contains("Grand Rapids"),
            "city name should still appear"
        );
    }

    /// Smoke test: load every fixture, render, and assert the SVG has
    /// the expected layers and weight. Needs the LFS basemap fixture.
    #[test]
    fn render_smoke() {
        let cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load("themes/noir_heat.json").expect("theme loads");
        let basemap = osm::load("fixtures/grand-rapids.osm.json.gz").expect("osm loads");

        let raw = std::fs::read_to_string("fixtures/activities.json")
            .expect("activities fixture readable");
        let activities: Vec<Activity> = serde_json::from_str(&raw).expect("activities parse");

        let obf = obfuscation::apply(
            activities,
            ObfuscationParams {
                home_lat: cfg.home_lat,
                home_lng: cfg.home_lng,
                radius_m: cfg.obfuscation_radius_m,
                salt: cfg.privacy_salt.clone(),
                offset_m: cfg.privacy_offset_m,
            },
        )
        .expect("obfuscate");

        let svg = render(
            &cfg,
            &basemap,
            &obf,
            &theme,
            false,
            false,
            false,
            Fit::Contain,
            HeatTuning::default(),
        )
        .expect("render ok");

        assert!(svg.contains("<svg"), "missing <svg open tag");
        assert!(
            svg.trim_end().ends_with("</svg>"),
            "doesn't end with </svg>"
        );
        assert!(svg.contains("class=\"heat\""), "missing heat layer");
        assert!(svg.contains("class=\"roads\""), "missing roads layer");
        assert!(svg.contains("class=\"water\""), "missing water layer");
        assert!(svg.len() >= 500_000, "svg too small: {} bytes", svg.len());

        eprintln!("render_smoke: svg size = {} bytes", svg.len());
    }

    /// cycle.dunn.dev's landing render (`--theme cycle_heat --web
    /// --transparent-bg --anonymize --obfuscation-radius-m 1000
    /// --cycling --heat-alpha 4.0 --fit cover`) on the fixtures, held
    /// to the 300 KB raw target at both crops. Measured 260 KB and
    /// 286 KB when set.
    #[test]
    fn web_payload_stays_in_budget() {
        let mut cfg = config::load("fixtures/config.toml").expect("config loads");
        let theme = theme::load_named("cycle_heat", None).expect("theme loads");
        let mut basemap = osm::load("fixtures/grand-rapids.osm.json.gz").expect("osm loads");
        osm::filter::refine(&mut basemap);
        let raw = std::fs::read_to_string("fixtures/activities.json").expect("fixture");
        let mut rides: Vec<Activity> = serde_json::from_str(&raw).expect("activities parse");
        rides.retain(|a| {
            matches!(
                a.activity_type,
                ActivityType::Ride | ActivityType::EBikeRide
            ) && a.distance_m >= 1000.0
        });
        let obf = obfuscation::apply(
            rides,
            ObfuscationParams {
                home_lat: cfg.home_lat,
                home_lng: cfg.home_lng,
                radius_m: 1000.0,
                salt: cfg.privacy_salt.clone(),
                offset_m: cfg.privacy_offset_m,
            },
        )
        .expect("obfuscate");
        let tuning = HeatTuning {
            alpha: 4.0,
            anonymize: true,
            ..HeatTuning::default()
        };
        for (w, h, budget) in [(1600, 900, 300_000), (1200, 1600, 300_000)] {
            cfg.viewbox_width = w;
            cfg.viewbox_height = h;
            let svg = render(
                &cfg,
                &basemap,
                &obf,
                &theme,
                false,
                true,
                true,
                Fit::Cover,
                tuning,
            )
            .expect("render ok");
            assert!(svg.contains("class=\"heat\""), "{w}x{h}: heat missing");
            assert!(svg.contains("class=\"water\""), "{w}x{h}: water missing");
            assert!(
                svg.len() <= budget,
                "{w}x{h}: {} bytes, over the {budget} byte budget",
                svg.len()
            );
        }
    }
}

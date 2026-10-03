// render::theme — JSON theme loader. Bundled enforcement: invalid
// theme files can't deserialize into Theme, so the renderer never
// sees malformed colors.
//
// Themes ship two ways:
//   * Embedded — the four shipped themes are baked into the binary
//     via `include_str!`. `cargo install --git` users get them at
//     zero ceremony.
//   * On-disk — pass `--themes-dir` to a directory of `*.json` files
//     to override or extend. A name resolves via `dir/{name}.json`
//     first, falling back to the embedded set if not found.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

const NOIR_HEAT: &str = include_str!("../../themes/noir_heat.json");
const BLUEPRINT_HEAT: &str = include_str!("../../themes/blueprint_heat.json");
const WARM_BEIGE: &str = include_str!("../../themes/warm_beige.json");
const CYCLE_HEAT: &str = include_str!("../../themes/cycle_heat.json");

const EMBEDDED_THEMES: &[(&str, &str)] = &[
    ("noir_heat", NOIR_HEAT),
    ("blueprint_heat", BLUEPRINT_HEAT),
    ("warm_beige", WARM_BEIGE),
    ("cycle_heat", CYCLE_HEAT),
];

#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    pub name: String,
    pub bg: HexColor,
    pub text: HexColor,
    pub water: HexColor,
    /// Stroke width for line water (rivers, streams). Defaults to 1.0
    /// if omitted from the theme JSON for backward compat.
    #[serde(default = "default_water_line_width")]
    pub water_line_width: f32,
    pub parks: HexColor,
    pub road_motorway: RoadStyle,
    pub road_trunk: RoadStyle,
    pub road_primary: RoadStyle,
    pub road_secondary: RoadStyle,
    pub road_tertiary: RoadStyle,
    pub road_residential: RoadStyle,
    pub heat: HeatStyle,
    pub fade_top: FadeStyle,
    pub fade_bottom: FadeStyle,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoadStyle {
    pub color: HexColor,
    pub width: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeatStyle {
    pub color: HexColor,
    pub width: f32,
    pub alpha: f32,
    /// Outer glow halo width as a multiple of `width`. Tuned for dark
    /// themes by default; cream-paper themes can dial this up so the
    /// halo registers visually against a warm background.
    #[serde(default = "default_glow_outer_ratio")]
    pub glow_outer_ratio: f32,
    /// Outer glow halo stroke-opacity. Default is faint enough to read
    /// on black; cream-paper themes lift this for visible bloom.
    #[serde(default = "default_glow_outer_alpha")]
    pub glow_outer_alpha: f32,
    /// Inner glow (mid-bloom) width as a multiple of `width`.
    #[serde(default = "default_glow_inner_ratio")]
    pub glow_inner_ratio: f32,
    /// Inner glow stroke-opacity.
    #[serde(default = "default_glow_inner_alpha")]
    pub glow_inner_alpha: f32,
    /// Blend mode for the sharp core. Omitted: derived from `bg` via
    /// `Theme::heat_blend` (multiply on light grounds, screen on dark).
    #[serde(default)]
    pub blend: Option<HeatBlend>,
}

/// `mix-blend-mode` for the heat core. Multiply presses ink into paper
/// but crushes heat to black on a dark ground; screen is its mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeatBlend {
    Normal,
    Multiply,
    Screen,
}

impl HeatBlend {
    pub fn as_css(self) -> &'static str {
        match self {
            HeatBlend::Normal => "normal",
            HeatBlend::Multiply => "multiply",
            HeatBlend::Screen => "screen",
        }
    }
}

fn default_glow_outer_ratio() -> f32 {
    3.5
}
fn default_glow_outer_alpha() -> f32 {
    0.025
}
fn default_glow_inner_ratio() -> f32 {
    1.7
}
fn default_glow_inner_alpha() -> f32 {
    0.06
}

#[derive(Debug, Clone, Deserialize)]
pub struct FadeStyle {
    pub from: HexColor,
    /// "transparent" or a 6-digit hex.
    pub to: String,
    pub height_pct: f32,
}

/// 6-digit hex color, validated at deserialize time.
#[derive(Debug, Clone)]
pub struct HexColor(String);

impl HexColor {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// WCAG relative luminance, 0 (black) to 1 (white).
    pub fn relative_luminance(&self) -> f64 {
        let channel = |i: usize| {
            let c = u8::from_str_radix(&self.0[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
    }

    /// Lighter than mid grey (L* 50, relative luminance about 0.18).
    pub fn is_light(&self) -> bool {
        self.relative_luminance() > 0.18
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        if !is_valid_hex6(&s) {
            return Err(serde::de::Error::custom(format!(
                "invalid hex color {s:?} — expected #rrggbb (e.g. #ff4500). \
                 Edit the theme file and supply a 6-digit hex value."
            )));
        }
        Ok(HexColor(s))
    }
}

fn default_water_line_width() -> f32 {
    1.0
}

fn is_valid_hex6(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

impl Theme {
    /// The heat core's blend: the theme's `heat.blend`, else multiply
    /// on a light `bg` and screen on a dark one.
    pub fn heat_blend(&self) -> HeatBlend {
        self.heat.blend.unwrap_or(if self.bg.is_light() {
            HeatBlend::Multiply
        } else {
            HeatBlend::Screen
        })
    }

    pub fn road_style(&self, tier: RoadTier) -> &RoadStyle {
        match tier {
            RoadTier::Motorway => &self.road_motorway,
            RoadTier::Trunk => &self.road_trunk,
            RoadTier::Primary => &self.road_primary,
            RoadTier::Secondary => &self.road_secondary,
            RoadTier::Tertiary => &self.road_tertiary,
            RoadTier::Residential => &self.road_residential,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoadTier {
    Motorway,
    Trunk,
    Primary,
    Secondary,
    Tertiary,
    Residential,
}

impl RoadTier {
    pub fn from_osm_highway(tag: &str) -> Option<Self> {
        match tag {
            "motorway" | "motorway_link" => Some(RoadTier::Motorway),
            "trunk" | "trunk_link" => Some(RoadTier::Trunk),
            "primary" | "primary_link" => Some(RoadTier::Primary),
            "secondary" | "secondary_link" => Some(RoadTier::Secondary),
            "tertiary" | "tertiary_link" => Some(RoadTier::Tertiary),
            "residential" | "unclassified" | "living_street" => Some(RoadTier::Residential),
            _ => None,
        }
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<Theme> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "could not read theme file {path:?}. \
             Run `patinate list-themes` to see what's available."
        )
    })?;
    let theme: Theme = serde_json::from_str(&text)
        .with_context(|| format!("theme file {path:?} failed to parse. Fix the JSON and retry."))?;
    Ok(theme)
}

/// Resolve `name` to a `Theme`. If `dir` is supplied and contains
/// `{name}.json`, load from there. Otherwise fall back to the embedded
/// set baked at compile time. Errors only when `name` matches neither.
pub fn load_named(name: &str, dir: Option<&Path>) -> Result<Theme> {
    if let Some(d) = dir {
        let p = d.join(format!("{name}.json"));
        if p.exists() {
            return load(&p);
        }
    }
    for (n, content) in EMBEDDED_THEMES {
        if *n == name {
            let theme: Theme = serde_json::from_str(content).with_context(|| {
                format!("embedded theme {name:?} failed to parse (this is a patinate bug)")
            })?;
            return Ok(theme);
        }
    }
    let available: Vec<&str> = EMBEDDED_THEMES.iter().map(|(n, _)| *n).collect();
    bail!(
        "no theme named {name:?}. Embedded themes: {}. \
         Pass `--themes-dir` to add custom themes alongside the embedded set.",
        available.join(", ")
    );
}

/// Names of every theme baked into the binary.
pub fn embedded_names() -> impl Iterator<Item = &'static str> {
    EMBEDDED_THEMES.iter().map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_hex() {
        let s = r##"{"name":"x","bg":"#fff","text":"#000000","water":"#000000",
                    "parks":"#000000",
                    "road_motorway":{"color":"#000000","width":1.0},
                    "road_trunk":{"color":"#000000","width":1.0},
                    "road_primary":{"color":"#000000","width":1.0},
                    "road_secondary":{"color":"#000000","width":1.0},
                    "road_tertiary":{"color":"#000000","width":1.0},
                    "road_residential":{"color":"#000000","width":1.0},
                    "heat":{"color":"#000000","width":1.0,"alpha":0.1},
                    "fade_top":{"from":"#000000","to":"transparent","height_pct":1.0},
                    "fade_bottom":{"from":"#000000","to":"transparent","height_pct":1.0}}"##;
        let err = serde_json::from_str::<Theme>(s).unwrap_err().to_string();
        assert!(err.contains("invalid hex color"), "got: {err}");
    }

    #[test]
    fn loads_shipped_themes() {
        let noir = load("themes/noir_heat.json").expect("noir_heat loads");
        let bp = load("themes/blueprint_heat.json").expect("blueprint_heat loads");
        assert_eq!(noir.name, "noir_heat");
        assert_eq!(bp.name, "blueprint_heat");
    }

    #[test]
    fn blend_defaults_from_bg_luminance() {
        for (name, want) in [
            ("noir_heat", HeatBlend::Screen),
            ("blueprint_heat", HeatBlend::Screen),
            ("warm_beige", HeatBlend::Multiply),
            ("cycle_heat", HeatBlend::Multiply),
        ] {
            let t = load_named(name, None).expect("embedded theme loads");
            assert_eq!(t.heat.blend, None, "{name} ships without a blend");
            assert_eq!(t.heat_blend(), want, "{name}");
        }
    }

    #[test]
    fn blend_field_overrides_the_default() {
        let mut t = load_named("noir_heat", None).expect("embedded theme loads");
        t.heat.blend = serde_json::from_str("\"normal\"").expect("parses");
        assert_eq!(t.heat_blend(), HeatBlend::Normal);
        assert!(serde_json::from_str::<HeatBlend>("\"overlay\"").is_err());
    }
}

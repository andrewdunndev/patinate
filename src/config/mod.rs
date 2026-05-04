// config — load + validate config from TOML file or environment.
//
// Bundled enforcement: there is no way to obtain a `ValidatedConfig`
// except through `load()`, which runs invariant checks in one place.
// Both file and env paths flow through that constructor.

use anyhow::{Context, Result, bail};
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct RawConfig {
    general: General,
    render: Render,
    privacy: Privacy,
    #[serde(default)]
    strava: Strava,
}

#[derive(Debug, Deserialize)]
struct General {
    city_name: String,
    country: String,
    center_lat: f64,
    center_lng: f64,
    radius_m: f64,
}

#[derive(Debug, Deserialize)]
struct Render {
    theme: String,
    viewbox_width: u32,
    viewbox_height: u32,
}

#[derive(Debug, Deserialize)]
struct Privacy {
    home_lat: f64,
    home_lng: f64,
    obfuscation_radius_m: f64,
}

#[derive(Debug, Default, Deserialize)]
struct Strava {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// Validated, render-ready config. Construct via `load()` only.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    pub city_name: String,
    pub country: String,
    pub center_lat: f64,
    pub center_lng: f64,
    pub radius_m: f64,
    pub theme: String,
    pub viewbox_width: u32,
    pub viewbox_height: u32,
    pub home_lat: f64,
    pub home_lng: f64,
    pub obfuscation_radius_m: f64,
    pub strava_client_id: Option<String>,
    pub strava_client_secret: Option<String>,
    pub strava_refresh_token: Option<String>,
    /// Short-lived (24h) Strava access token. Optional — if absent or
    /// expired, the sync flow exchanges `refresh_token` for a fresh one.
    pub strava_access_token: Option<String>,
}

/// Load + validate config. File path is required.
///
/// Two env-var conventions, both supported:
///
/// 1. Generic figment nested env: `PATINATE_*__*` with `__` separator,
///    useful for overriding any non-secret field from CI.
/// 2. Strava-specific names that match GitLab group variables:
///    `STRAVA_PATINATE_CLIENT_SECRET`, `STRAVA_PATINATE_REFRESH_TOKEN`,
///    `STRAVA_PATINATE_ACCESS_TOKEN`. These take precedence over the
///    file and the figment env. Their readable prefix groups Strava
///    secrets in the GitLab UI.
pub fn load(path: impl AsRef<Path>) -> Result<ValidatedConfig> {
    let path = path.as_ref();

    // Merge order (later wins): project file -> user XDG file -> env.
    // The user file is optional and partial: typical use is just a
    // [strava] block holding secrets that shouldn't live in any repo.
    let mut figment = Figment::new().merge(Toml::file(path));
    if let Some(user_path) = user_config_path() {
        if user_path.exists() {
            tracing::debug!(user_config = %user_path.display(), "merging user config");
            figment = figment.merge(Toml::file(user_path));
        }
    }
    figment = figment.merge(Env::prefixed("PATINATE_").split("__"));

    let raw: RawConfig = figment.extract().with_context(|| {
        format!(
            "could not load config from {path:?} (+ user override + env). \
             Check the file syntax and any PATINATE_* env vars."
        )
    })?;
    let mut cfg = validate(raw)?;
    apply_strava_env_overrides(&mut cfg);
    Ok(cfg)
}

/// Resolve the per-user XDG config base directory. Returns `None` if
/// neither `XDG_CONFIG_HOME` nor `HOME` is set (or both are empty,
/// per XDG Base Directory spec). Shared between the figment user-config
/// merge in this module and the CLI default-resolver in `cli.rs` so
/// the two layers can never silently disagree.
pub fn xdg_config_dir() -> Option<PathBuf> {
    let nonempty_var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    nonempty_var("XDG_CONFIG_HOME").or_else(|| nonempty_var("HOME").map(|h| h.join(".config")))
}

/// Resolve `$XDG_CONFIG_HOME/patinate/config.toml`, falling back to
/// `$HOME/.config/patinate/config.toml`. Returns `None` if neither
/// env var is set (or both are empty, per XDG Base Directory spec).
fn user_config_path() -> Option<PathBuf> {
    Some(xdg_config_dir()?.join("patinate").join("config.toml"))
}

/// Layer in the `STRAVA_PATINATE_*` env vars on top of whatever the
/// file/figment provided. Lets CI inject secrets without touching the
/// repo or the figment naming convention.
fn apply_strava_env_overrides(cfg: &mut ValidatedConfig) {
    if let Ok(s) = std::env::var("STRAVA_PATINATE_CLIENT_SECRET") {
        cfg.strava_client_secret = Some(s);
    }
    if let Ok(s) = std::env::var("STRAVA_PATINATE_REFRESH_TOKEN") {
        cfg.strava_refresh_token = Some(s);
    }
    if let Ok(s) = std::env::var("STRAVA_PATINATE_ACCESS_TOKEN") {
        cfg.strava_access_token = Some(s);
    }
}

/// Reject NaN / infinity before any range check. Range comparisons
/// against NaN evaluate to false, which would let `obfuscation_radius_m
/// = nan` skate through validation, then silently disable obfuscation
/// downstream (every distance comparison against NaN is false, so every
/// point classifies as "outside the home circle"). Catch it here.
fn require_finite(name: &str, v: f64) -> Result<()> {
    if v.is_finite() {
        return Ok(());
    }
    bail!(
        "{name} = {v} is not a finite number. \
         Use a real decimal value; NaN and infinity are not accepted."
    );
}

fn validate(raw: RawConfig) -> Result<ValidatedConfig> {
    require_finite("[general].center_lat", raw.general.center_lat)?;
    require_finite("[general].center_lng", raw.general.center_lng)?;
    require_finite("[general].radius_m", raw.general.radius_m)?;
    require_finite("[privacy].home_lat", raw.privacy.home_lat)?;
    require_finite("[privacy].home_lng", raw.privacy.home_lng)?;
    require_finite(
        "[privacy].obfuscation_radius_m",
        raw.privacy.obfuscation_radius_m,
    )?;
    if !(-90.0..=90.0).contains(&raw.general.center_lat) {
        bail!(
            "[general].center_lat = {} is out of range. \
             Use a value in [-90.0, 90.0].",
            raw.general.center_lat
        );
    }
    if !(-180.0..=180.0).contains(&raw.general.center_lng) {
        bail!(
            "[general].center_lng = {} is out of range. \
             Use a value in [-180.0, 180.0].",
            raw.general.center_lng
        );
    }
    if raw.general.radius_m <= 0.0 {
        bail!(
            "[general].radius_m = {} must be > 0. \
             Try 25000 (25km) for a hometown poster.",
            raw.general.radius_m
        );
    }
    if !(-90.0..=90.0).contains(&raw.privacy.home_lat) {
        bail!(
            "[privacy].home_lat = {} is out of range. \
             Use a value in [-90.0, 90.0].",
            raw.privacy.home_lat
        );
    }
    if !(-180.0..=180.0).contains(&raw.privacy.home_lng) {
        bail!(
            "[privacy].home_lng = {} is out of range. \
             Use a value in [-180.0, 180.0].",
            raw.privacy.home_lng
        );
    }
    if raw.privacy.obfuscation_radius_m < 0.0 {
        bail!(
            "[privacy].obfuscation_radius_m = {} must be >= 0. \
             Set 0 to disable obfuscation; default 250 matches Strava.",
            raw.privacy.obfuscation_radius_m
        );
    }
    if raw.render.viewbox_width == 0 || raw.render.viewbox_height == 0 {
        bail!(
            "[render].viewbox_width and viewbox_height must be > 0. \
             Try 1200 x 1600 for a 3:4 poster aspect."
        );
    }
    Ok(ValidatedConfig {
        city_name: raw.general.city_name,
        country: raw.general.country,
        center_lat: raw.general.center_lat,
        center_lng: raw.general.center_lng,
        radius_m: raw.general.radius_m,
        theme: raw.render.theme,
        viewbox_width: raw.render.viewbox_width,
        viewbox_height: raw.render.viewbox_height,
        home_lat: raw.privacy.home_lat,
        home_lng: raw.privacy.home_lng,
        obfuscation_radius_m: raw.privacy.obfuscation_radius_m,
        strava_client_id: raw.strava.client_id,
        strava_client_secret: raw.strava.client_secret,
        strava_refresh_token: raw.strava.refresh_token,
        strava_access_token: raw.strava.access_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(home_lat: f64, home_lng: f64, radius: f64) -> RawConfig {
        RawConfig {
            general: General {
                city_name: "T".into(),
                country: "T".into(),
                center_lat: 0.0,
                center_lng: 0.0,
                radius_m: 1000.0,
            },
            render: Render {
                theme: "noir_heat".into(),
                viewbox_width: 1200,
                viewbox_height: 1600,
            },
            privacy: Privacy {
                home_lat,
                home_lng,
                obfuscation_radius_m: radius,
            },
            strava: Strava::default(),
        }
    }

    #[test]
    fn nan_obfuscation_radius_rejected() {
        // Privacy bypass footgun: NaN slips past `< 0.0` and then every
        // distance comparison downstream is false, treating every point
        // as "outside the home circle." Validator must reject.
        let err = validate(raw(42.0, -85.0, f64::NAN)).unwrap_err();
        assert!(
            err.to_string().contains("not a finite number"),
            "got: {err}"
        );
    }

    #[test]
    fn nan_home_lat_rejected() {
        let err = validate(raw(f64::NAN, -85.0, 250.0)).unwrap_err();
        assert!(err.to_string().contains("not a finite number"));
    }

    #[test]
    fn infinity_home_lng_rejected() {
        let err = validate(raw(42.0, f64::INFINITY, 250.0)).unwrap_err();
        assert!(err.to_string().contains("not a finite number"));
    }
}

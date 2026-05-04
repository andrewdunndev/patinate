//! cli — clap entrypoint and subcommand dispatch.
//!
//! v0 surface:
//!   patinate render        — render a heatmap SVG (fixture-driven for now)
//!   patinate sync          — Strava sync into the local cache (stub)
//!   patinate auth          — interactive OAuth: write a fresh refresh_token
//!                             into ~/.config/patinate/config.toml
//!   patinate list-themes   — list available themes
//!   patinate fetch-osm     — pull OSM basemap data from Overpass
//!
//! Each subcommand returns Result so errors propagate with the
//! prescriptive-failure messages defined in lower modules.

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};
use oauth2::{AuthorizationCode, ClientId, ClientSecret};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use crate::cache::{self, queries::StoredTokens};
use crate::config::{self, ValidatedConfig};
use crate::obfuscation::{self, ObfuscationParams};
use crate::osm;
use crate::render::{compose, theme};
use crate::strava::{self, Activity, auth::AuthClient, client::StravaClient};

#[derive(Debug, Parser)]
#[command(
    name = "patinate",
    about = "Render Strava activity into stylized heatmap posters and interactive SVG.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Render a heatmap SVG from config + OSM basemap + activities.
    Render(RenderArgs),

    /// Pull new activities from Strava into the local cache.
    Sync(SyncArgs),

    /// List available themes. Always shows the four embedded themes;
    /// `--themes-dir` adds anything from a directory of `*.json` files
    /// on top.
    ListThemes(ListThemesArgs),

    /// Fetch OSM basemap data for a city/region from the Overpass API
    /// and write it to a JSON fixture suitable for `patinate render`.
    FetchOsm(FetchOsmArgs),

    /// Interactive OAuth: open a browser to Strava, capture the
    /// authorization code on a localhost callback, and write a fresh
    /// refresh_token into ~/.config/patinate/config.toml.
    Auth(AuthArgs),
}

#[derive(Debug, clap::Args)]
struct ListThemesArgs {
    /// Optional directory of `*.json` theme files to list alongside the
    /// embedded set. Themes here override embedded names of the same
    /// stem.
    #[arg(long, env = "PATINATE_THEMES_DIR")]
    themes_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct AuthArgs {
    /// Path to the TOML config file (holds client_id + client_secret).
    /// Defaults to `~/.config/patinate/config.toml`.
    #[arg(long, env = "PATINATE_CONFIG")]
    config: Option<PathBuf>,

    /// Localhost port to bind for the OAuth callback. Strava's app
    /// settings accept any port on `localhost`. Default 8765.
    #[arg(long, default_value = "8765")]
    port: u16,

    /// Override the cache database path. Defaults to the per-user
    /// XDG/Application-Support location (see `cache::default_cache_path`).
    #[arg(long, env = "PATINATE_CACHE")]
    cache: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct FetchOsmArgs {
    /// City / place name to geocode via Nominatim. Required if
    /// --center-lat/--center-lng are not both supplied. Coords win
    /// when both forms are passed (explicit beats implicit).
    #[arg(long)]
    city: Option<String>,

    /// Center latitude in decimal degrees. Pair with --center-lng.
    #[arg(long)]
    center_lat: Option<f64>,

    /// Center longitude in decimal degrees. Pair with --center-lat.
    #[arg(long)]
    center_lng: Option<f64>,

    /// Search radius in meters around the center point. 25000 is a
    /// good default for a mid-size city's heatmap-relevant area.
    #[arg(long, default_value = "25000")]
    radius_m: u32,

    /// Output JSON path. The Overpass response is written verbatim
    /// here; `patinate render --osm <this>` will consume it. Defaults
    /// to the per-user cache location (`~/.cache/patinate/basemap.osm.json`
    /// on Linux; `~/Library/Caches/dev.dunn.patinate/basemap.osm.json`
    /// on macOS).
    #[arg(long, short, env = "PATINATE_OSM")]
    out: Option<PathBuf>,

    /// Overpass endpoint URL. Defaults to the public instance; point
    /// at a private mirror for heavier workloads.
    #[arg(
        long,
        env = "PATINATE_OVERPASS_ENDPOINT",
        default_value = crate::osm::overpass::DEFAULT_OVERPASS_ENDPOINT
    )]
    endpoint: String,
}

#[derive(Debug, clap::Args)]
struct RenderArgs {
    /// Path to the TOML config file. Defaults to
    /// `~/.config/patinate/config.toml`.
    #[arg(long, env = "PATINATE_CONFIG")]
    config: Option<PathBuf>,

    /// Path to the pre-fetched OSM JSON (Overpass `out geom` format).
    /// Defaults to the per-user cache path written by `patinate fetch-osm`.
    #[arg(long, env = "PATINATE_OSM")]
    osm: Option<PathBuf>,

    /// Path to a JSON array of activities. If omitted, the cache is read.
    #[arg(long, env = "PATINATE_ACTIVITIES")]
    activities: Option<PathBuf>,

    /// Output SVG path.
    #[arg(long, short, default_value = "heatmap.svg")]
    out: PathBuf,

    /// Theme name (overrides [render].theme in the config).
    #[arg(long)]
    theme: Option<String>,

    /// Directory of `*.json` theme files. If omitted, only the four
    /// embedded themes are searched. When set, on-disk themes override
    /// embedded ones with matching names.
    #[arg(long, env = "PATINATE_THEMES_DIR")]
    themes_dir: Option<PathBuf>,

    /// Skip the OSM filter pipeline (min_way_length + connected
    /// components + simplify). Useful for diagnosing whether a visual
    /// artifact comes from filtering or from the underlying data.
    #[arg(long)]
    no_refine: bool,

    /// Only render activities of these types. Repeat or comma-separate.
    /// Common values: Ride, EBikeRide, VirtualRide, Run, Walk, Hike.
    /// Default: include all types.
    #[arg(long = "type", value_delimiter = ',')]
    types: Vec<crate::strava::ActivityType>,

    /// Only render activities with these Strava gear IDs (e.g. bSCRUBBED1).
    /// Repeat or comma-separate. Default: include all gear.
    #[arg(long = "gear", value_delimiter = ',')]
    gears: Vec<String>,

    /// Convenience flag: same as `--type Ride,EBikeRide`. Outdoor
    /// cycling only — drops walks, runs, virtual rides.
    #[arg(long)]
    cycling: bool,

    /// Minimum activity distance in meters. Drops short stubs that
    /// would render as visual noise. Default 1000m.
    #[arg(long, default_value = "1000")]
    min_distance_m: f64,

    /// Override the render radius from the config. Smaller = tighter
    /// crop on the riding area. Default: from config.
    #[arg(long)]
    radius_m: Option<f64>,

    /// Drop the basemap (water, parks, roads). Render heat + typography
    /// only — the Cadence/Glowmap minimal-poster aesthetic.
    #[arg(long)]
    heat_only: bool,

    /// Web-embed preset: strip detail for inline-on-page use. Drops
    /// tertiary + residential roads, drops typography (consumer page
    /// adds its own), uses single-layer heat (no glow stack), coord
    /// precision 1. Targets ~500KB SVG.
    #[arg(long)]
    web: bool,

    /// Override viewbox width.
    #[arg(long)]
    viewbox_width: Option<u32>,

    /// Override viewbox height.
    #[arg(long)]
    viewbox_height: Option<u32>,

    /// Render only this single Strava activity ID (single-ride poster).
    #[arg(long)]
    activity_id: Option<i64>,

    /// Override the SQLite cache path. Used only when `--activities` is
    /// omitted (otherwise the activities file is read directly).
    /// Defaults to the per-user data location matching `patinate sync`.
    #[arg(long, env = "PATINATE_CACHE")]
    cache: Option<PathBuf>,

    /// Skip the background rect — SVG paints over whatever background
    /// the consumer page has. Use for inline-on-page heros.
    #[arg(long)]
    transparent_bg: bool,

    /// Heat halo strength multiplier. 0 = sharp lines only (collapse the
    /// glow stack to a single core layer); 1 = the theme's baked default;
    /// values above 1 amplify the halo for a softer, cloudier feel.
    /// Useful when a theme reads too crisp at low activity counts.
    #[arg(long, default_value = "1.0")]
    heat_bloom: f32,

    /// Heat sharp-core alpha multiplier. 1 = theme default; under 1 dims
    /// the rideline cores; over 1 saturates them. Independent of bloom.
    #[arg(long, default_value = "1.0")]
    heat_alpha: f32,

    /// Strip identifying metadata before writing the SVG: drop the
    /// `data-rider`, `data-bike`, `data-year`, `data-type` attributes
    /// from heat paths, and scrub the `<desc>` element down to city +
    /// country (no exact center coords). Use whenever the SVG itself
    /// will be published. Has no effect on the visible image; rasterized
    /// PNG/WebP outputs were never carrying these attrs to begin with.
    #[arg(long)]
    anonymize: bool,

    /// Override `[privacy].obfuscation_radius_m` from the config. Useful
    /// when you want a tighter privacy circle for one render without
    /// editing the config file. Pair with `--anonymize` for any render
    /// destined for a public README or social card; 1000 m or larger
    /// is the recommended floor for public publication.
    #[arg(long)]
    obfuscation_radius_m: Option<f64>,
}

#[derive(Debug, clap::Args)]
struct SyncArgs {
    /// Path to the TOML config file. Defaults to
    /// `~/.config/patinate/config.toml`.
    #[arg(long, env = "PATINATE_CONFIG")]
    config: Option<PathBuf>,

    /// Override the cache database path. Defaults to the per-user
    /// XDG/Application-Support location (see `cache::default_cache_path`).
    #[arg(long, env = "PATINATE_CACHE")]
    cache: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Render(args) => render_cmd(args),
        Command::Sync(args) => sync_cmd(args),
        Command::ListThemes(args) => list_themes_cmd(args),
        Command::FetchOsm(args) => fetch_osm_cmd(args),
        Command::Auth(args) => auth_cmd(args),
    }
}

/// Resolve `~/.config/patinate/config.toml` (or `$XDG_CONFIG_HOME`).
/// Wraps `config::xdg_config_dir` so the CLI default and the figment
/// merge layer can never silently disagree about where the user
/// config lives.
fn default_user_config_path() -> Result<PathBuf> {
    let base = config::xdg_config_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "neither HOME nor XDG_CONFIG_HOME is set (or both are empty), \
             so patinate cannot resolve the default config path. Pass \
             `--config <path>` to choose one explicitly."
        )
    })?;
    Ok(base.join("patinate").join("config.toml"))
}

/// Resolve the per-user cache path for the OSM basemap JSON. Uses the
/// same `directories` crate as the SQLite cache so both blobs land
/// under one app-specific subtree.
fn default_osm_path() -> Result<PathBuf> {
    use directories::ProjectDirs;
    let dirs = ProjectDirs::from("dev", "dunn", "patinate").ok_or_else(|| {
        anyhow::anyhow!(
            "could not resolve a per-user cache directory for the OSM \
             basemap. Pass `--osm <path>` (render) or `--out <path>` \
             (fetch-osm) to choose an explicit location."
        )
    })?;
    Ok(dirs.cache_dir().join("basemap.osm.json"))
}

fn fetch_osm_cmd(args: FetchOsmArgs) -> Result<()> {
    let out = match args.out {
        Some(p) => p,
        None => default_osm_path()?,
    };
    // Like sync_cmd, build a current-thread tokio runtime locally so
    // the rest of run() stays sync. fetch-osm is a one-shot: geocode
    // (maybe) + one POST + a write — no need to ripple async upward.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the tokio runtime for `patinate fetch-osm`")?;

    rt.block_on(crate::osm::overpass::run(
        args.city,
        args.center_lat,
        args.center_lng,
        args.radius_m,
        out,
        args.endpoint,
    ))?;
    Ok(())
}

fn render_cmd(args: RenderArgs) -> Result<()> {
    let config_path = match args.config {
        Some(p) => p,
        None => default_user_config_path()?,
    };
    let osm_path = match args.osm {
        Some(p) => p,
        None => default_osm_path()?,
    };
    tracing::info!(config = %config_path.display(), "loading config");
    let mut cfg = config::load(&config_path)?;
    if let Some(r) = args.radius_m {
        cfg.radius_m = r;
    }
    if let Some(w) = args.viewbox_width {
        cfg.viewbox_width = w;
    }
    if let Some(h) = args.viewbox_height {
        cfg.viewbox_height = h;
    }

    let theme_name = args.theme.as_deref().unwrap_or(&cfg.theme);
    tracing::info!(theme = theme_name, "loading theme");
    let theme = theme::load_named(theme_name, args.themes_dir.as_deref())?;

    tracing::info!(osm = %osm_path.display(), "loading OSM basemap");
    let mut basemap = osm::load(&osm_path)?;
    tracing::info!(
        roads = basemap.roads.len(),
        water_polygons = basemap.water_polygons.len(),
        water_lines = basemap.water_lines.len(),
        parks = basemap.parks.len(),
        "OSM basemap loaded (raw)"
    );

    if args.no_refine {
        tracing::info!("OSM refine SKIPPED via --no-refine");
    } else {
        osm::filter::refine(&mut basemap);
        tracing::info!(
            roads = basemap.roads.len(),
            "OSM basemap refined (min_way_length + connected_components + simplify)"
        );
    }

    // Resolve the requested activity-type set. --cycling is sugar for
    // Ride + EBikeRide; otherwise honor explicit --type. Empty set = all.
    let type_filter: Vec<strava::ActivityType> = if args.cycling {
        use std::collections::HashSet;
        let mut set: HashSet<strava::ActivityType> = args.types.iter().copied().collect();
        set.insert(strava::ActivityType::Ride);
        set.insert(strava::ActivityType::EBikeRide);
        let mut v: Vec<_> = set.into_iter().collect();
        v.sort_by_key(|t| t.as_data_attr());
        v
    } else {
        args.types.clone()
    };

    let mut activities = match &args.activities {
        Some(path) => load_activities_json(path)?,
        None => {
            let cache_path = match &args.cache {
                Some(p) => p.clone(),
                None => cache::default_cache_path()?,
            };
            tracing::info!(cache = %cache_path.display(), "loading activities from cache");
            let conn = cache::open(&cache_path)?;
            let types_opt = if type_filter.is_empty() {
                None
            } else {
                Some(type_filter.as_slice())
            };
            cache::queries::list_activities(&conn, None, types_opt, None, None)?
        }
    };

    // Apply type filter to the JSON path too (cache path already
    // filtered above).
    if args.activities.is_some() && !type_filter.is_empty() {
        activities.retain(|a| type_filter.contains(&a.activity_type));
    }

    // Gear filter (post-load; queries layer doesn't support it yet).
    if !args.gears.is_empty() {
        activities.retain(|a| {
            a.gear_id
                .as_deref()
                .map(|g| args.gears.iter().any(|want| want == g))
                .unwrap_or(false)
        });
    }

    // Minimum-distance filter: drops short noise stubs.
    if args.min_distance_m > 0.0 {
        activities.retain(|a| a.distance_m >= args.min_distance_m);
    }

    // Single-activity filter (highest precedence).
    if let Some(id) = args.activity_id {
        activities.retain(|a| a.id == id);
    }

    tracing::info!(
        count = activities.len(),
        types = ?type_filter.iter().map(|t| t.as_data_attr()).collect::<Vec<_>>(),
        gears = ?args.gears,
        "activities loaded after filters"
    );

    let obfuscation_radius_m = args
        .obfuscation_radius_m
        .unwrap_or(cfg.obfuscation_radius_m);
    if let Some(override_r) = args.obfuscation_radius_m {
        tracing::info!(
            cfg_radius_m = cfg.obfuscation_radius_m,
            override_radius_m = override_r,
            "obfuscation radius overridden via --obfuscation-radius-m"
        );
    }
    let obfuscated = obfuscation::apply(
        activities,
        ObfuscationParams {
            home_lat: cfg.home_lat,
            home_lng: cfg.home_lng,
            radius_m: obfuscation_radius_m,
        },
    );
    tracing::info!(kept = obfuscated.len(), "after obfuscation");

    // `--web` semantics (single-layer heat, precision-1 coords, dropped
    // minor road tiers, no own typography) all live inside compose::render.
    // The CLI just passes the flag through.
    let svg = compose::render(
        &cfg,
        &basemap,
        &obfuscated,
        &theme,
        args.heat_only,
        args.web,
        args.transparent_bg,
        compose::HeatTuning {
            bloom: args.heat_bloom,
            alpha: args.heat_alpha,
            anonymize: args.anonymize,
        },
    )?;

    std::fs::write(&args.out, &svg)
        .with_context(|| format!("could not write SVG to {:?}", args.out))?;
    tracing::info!(out = %args.out.display(), bytes = svg.len(), "wrote SVG");

    Ok(())
}

fn load_activities_json(path: &std::path::Path) -> Result<Vec<Activity>> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "could not read activities file {path:?}. \
             Try `--activities fixtures/activities.json` to use the bundled fixture."
        )
    })?;
    serde_json::from_str(&text)
        .with_context(|| format!("activities file {path:?} is not valid JSON"))
}

fn sync_cmd(args: SyncArgs) -> Result<()> {
    let config_path = match args.config {
        Some(p) => p,
        None => default_user_config_path()?,
    };
    tracing::info!(config = %config_path.display(), "loading config");
    let cfg = config::load(&config_path)?;

    let cache_path = match args.cache {
        Some(p) => p,
        None => cache::default_cache_path()?,
    };
    tracing::info!(cache = %cache_path.display(), "opening cache");
    let conn = cache::open(&cache_path)?;

    // Required secrets — fail with prescriptive next-step messages.
    let client_id = cfg.strava_client_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "no Strava client_id configured. \
             Set [strava].client_id in fixtures/config.toml or \
             ~/.config/patinate/config.toml; the value is the numeric \
             ID from https://www.strava.com/settings/api."
        )
    })?;
    let client_secret = cfg.strava_client_secret.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "no Strava client_secret configured. \
             Set [strava].client_secret in ~/.config/patinate/config.toml \
             or STRAVA_PATINATE_CLIENT_SECRET env var; the value is the \
             secret from https://www.strava.com/settings/api."
        )
    })?;

    // Build a current-thread tokio runtime locally so the rest of run()
    // stays sync. Refactoring run() to async would ripple into main.rs
    // for no benefit (sync_cmd is the only async caller right now).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the tokio runtime for `patinate sync`")?;

    let access_token = rt.block_on(resolve_access_token(&conn, &cfg, client_id, client_secret))?;

    // Athlete identity: prefer whatever ended up in the cache after
    // resolve_access_token. If the env-token shortcut was taken on
    // first run there will be no row yet — we can't proceed without
    // an identity, so force a refresh in that case.
    let athlete_id = match any_cached_athlete(&conn)? {
        Some(id) => id,
        None => bail!(
            "no athlete identity cached yet, and the env-provided \
             access token does not include one. Unset \
             STRAVA_PATINATE_ACCESS_TOKEN and rerun `patinate sync` \
             so the refresh flow can capture athlete.id from Strava."
        ),
    };

    let strava_client = StravaClient::new(access_token)?;

    let stats = rt
        .block_on(strava::sync::sync(&conn, &strava_client, athlete_id))
        .with_context(|| {
            "synced 0 of unknown activities before failing; \
             rerun `patinate sync` to resume from the watermark."
        })?;

    tracing::info!(
        fetched = stats.fetched,
        upserted = stats.upserted,
        "synced {} activities",
        stats.fetched
    );

    Ok(())
}

/// Decide which access token to send to Strava, refreshing via OAuth
/// if nothing in the cache or env is usable. Persists the resulting
/// tokens (including any rotated refresh_token) back to the cache.
async fn resolve_access_token(
    conn: &Connection,
    cfg: &ValidatedConfig,
    client_id: &str,
    client_secret: &str,
) -> Result<String> {
    let cached = any_cached_athlete(conn)?
        .map(|id| cache::queries::get_oauth(conn, id))
        .transpose()?
        .flatten();

    // 1. Cached access token still comfortably valid → reuse it.
    if let Some(c) = &cached {
        if let (Some(tok), Some(exp)) = (&c.access_token, c.access_expires_at) {
            if exp > Utc::now() + Duration::minutes(5) {
                tracing::debug!(athlete_id = c.athlete_id, "using cached access token");
                return Ok(tok.clone());
            }
        }
    }

    // 2. Env-supplied access token shortcut. Only applies when we
    //    already have a cached oauth row (so we know the athlete_id
    //    out of band) but the cached access_token is stale. On a cold
    //    cache we always go through refresh, since refresh is the
    //    only thing that returns athlete.id.
    if let (Some(c), Some(env_tok)) = (cached.as_ref(), cfg.strava_access_token.as_ref()) {
        let stale = c
            .access_expires_at
            .map(|exp| exp <= Utc::now() + Duration::minutes(5))
            .unwrap_or(true);
        if stale {
            tracing::debug!(
                athlete_id = c.athlete_id,
                "using STRAVA_PATINATE_ACCESS_TOKEN; will refresh on next run"
            );
            return Ok(env_tok.clone());
        }
    }

    // 3. Fall back to the refresh-token exchange. Refresh token comes
    //    from the cache if we have one, otherwise from config/env.
    let refresh_token = cached
        .as_ref()
        .map(|c| c.refresh_token.clone())
        .or_else(|| cfg.strava_refresh_token.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no Strava refresh_token configured. \
                 Set [strava].refresh_token in ~/.config/patinate/config.toml \
                 or STRAVA_PATINATE_REFRESH_TOKEN env var; \
                 visit https://www.strava.com/settings/api to get one."
            )
        })?;

    tracing::info!("refreshing Strava access token");
    let auth = AuthClient::new(
        ClientId::new(client_id.to_string()),
        ClientSecret::new(client_secret.to_string()),
    )?;
    let tokens = auth
        .refresh(client_id, client_secret, &refresh_token)
        .await
        .context(
            "Strava OAuth refresh failed; the refresh token may be revoked. \
             Re-authorize at https://www.strava.com/settings/api and update \
             STRAVA_PATINATE_REFRESH_TOKEN.",
        )?;

    // Strava's refresh response does NOT include athlete details (the
    // agent's earlier spec was wrong). Make a second call to /athlete
    // to capture the id, which keys our cache.
    let athlete_id = match tokens.athlete_id {
        Some(id) => id,
        None => fetch_athlete_id(&tokens.access_token).await?,
    };

    cache::queries::upsert_oauth(
        conn,
        &StoredTokens {
            athlete_id,
            refresh_token: tokens.refresh_token.clone(),
            access_token: Some(tokens.access_token.clone()),
            access_expires_at: Some(tokens.expires_at),
        },
    )?;

    Ok(tokens.access_token)
}

/// One-shot Strava /athlete call to capture athlete.id after a
/// refresh-token swap. Strava's refresh endpoint does not include
/// athlete info; this fills the gap.
async fn fetch_athlete_id(access_token: &str) -> Result<i64> {
    let body: serde_json::Value = reqwest::Client::new()
        .get("https://www.strava.com/api/v3/athlete")
        .bearer_auth(access_token)
        .send()
        .await
        .context("could not call Strava /athlete to capture athlete.id")?
        .error_for_status()
        .context("Strava /athlete returned non-2xx; access_token may be invalid")?
        .json()
        .await
        .context("Strava /athlete response was not valid JSON")?;
    body.get("id").and_then(|v| v.as_i64()).ok_or_else(|| {
        anyhow::anyhow!(
            "Strava /athlete response missing `id` field. \
                 Re-authorize at https://www.strava.com/settings/api \
                 and update STRAVA_PATINATE_REFRESH_TOKEN."
        )
    })
}

/// Return any athlete_id we already have an oauth row for. v0 is
/// single-user, so we just take the first one we see.
fn any_cached_athlete(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT athlete_id FROM oauth ORDER BY athlete_id LIMIT 1",
        params![],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .context("could not read oauth table from cache")
}

/// Interactive OAuth flow. Replaces the manual code-paste dance: the
/// operator clicks Authorize in the browser and `patinate auth` writes
/// the resulting refresh_token to the user XDG config without ever
/// echoing the secret to stdout.
fn auth_cmd(args: AuthArgs) -> Result<()> {
    let config_path = match args.config {
        Some(p) => p,
        None => default_user_config_path()?,
    };
    tracing::info!(config = %config_path.display(), "loading config");
    let cfg = config::load(&config_path)?;

    let client_id = cfg.strava_client_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "no Strava client_id configured. \
             Register an app at https://www.strava.com/settings/api \
             (set the Authorization Callback Domain to `localhost`) \
             and put the numeric Client ID under [strava].client_id \
             in fixtures/config.toml or ~/.config/patinate/config.toml."
        )
    })?;
    let client_secret = cfg.strava_client_secret.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "no Strava client_secret configured. \
             Copy the Client Secret from https://www.strava.com/settings/api \
             into [strava].client_secret in ~/.config/patinate/config.toml \
             (or set STRAVA_PATINATE_CLIENT_SECRET)."
        )
    })?;

    let cache_path = match &args.cache {
        Some(p) => p.clone(),
        None => cache::default_cache_path()?,
    };
    tracing::info!(cache = %cache_path.display(), "opening cache");
    let conn = cache::open(&cache_path)?;

    let auth = strava::auth::AuthClient::new(
        ClientId::new(client_id.to_string()),
        ClientSecret::new(client_secret.to_string()),
    )?;
    let redirect = strava::auth::local_redirect_url(args.port);
    let (authorize_url, csrf) = auth.authorize_url(&redirect)?;

    // Strava's authorize endpoint honors `approval_prompt=force`, which
    // makes sure the consent screen renders even if the user already
    // approved patinate. The oauth2 crate doesn't expose this knob, so
    // we append it ourselves; the URL builder above keeps it idempotent.
    let mut authorize_url = authorize_url;
    authorize_url
        .query_pairs_mut()
        .append_pair("approval_prompt", "force");

    println!("Opening browser to authorize patinate against Strava...");
    println!("If the browser does not open, paste this URL manually:");
    println!("  {authorize_url}");

    // Best-effort browser open. We never depend on this succeeding —
    // the URL is printed above so the user can copy it if `open`/
    // `xdg-open` is missing or returns nonzero.
    spawn_browser(authorize_url.as_str());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the tokio runtime for `patinate auth`")?;

    // 5-minute timeout: long enough that a distracted user can still
    // finish the click-through, short enough that a forgotten run
    // doesn't leak a listener forever.
    let timeout = StdDuration::from_secs(300);
    let callback = rt.block_on(strava::auth::await_callback(args.port, timeout))?;

    // Constant-time compare on the CSRF state. See
    // `strava::auth::csrf_states_match` for the timing-safe primitive.
    if !strava::auth::csrf_states_match(callback.state.as_str(), csrf.secret()) {
        bail!(
            "OAuth state mismatch. Expected the CSRF token patinate \
             generated, got something else. This usually means the \
             redirect was tampered with: re-run `patinate auth` from a \
             trusted browser session."
        );
    }
    if let Some(err) = callback.error.as_deref() {
        bail!(
            "Strava authorization rejected with error={err}. \
             Re-run `patinate auth` and confirm you clicked Authorize \
             (and that the requested scope `activity:read_all` is allowed)."
        );
    }

    let tokens = rt
        .block_on(auth.exchange_code(AuthorizationCode::new(callback.code), &redirect))
        .context(
            "Strava rejected the authorization code. The code expires \
             quickly (~10 minutes) and is single-use; re-run \
             `patinate auth` and approve in the browser, or check the \
             client_id/client_secret at https://www.strava.com/settings/api.",
        )?;

    // Strava's auth-code response goes through the typed oauth2
    // pipeline which doesn't surface athlete.id; capture it via a
    // /athlete call against the new access token.
    let athlete_id = match tokens.athlete_id {
        Some(id) => id,
        None => rt.block_on(fetch_athlete_id(&tokens.access_token))?,
    };

    cache::queries::upsert_oauth(
        &conn,
        &StoredTokens {
            athlete_id,
            refresh_token: tokens.refresh_token.clone(),
            access_token: Some(tokens.access_token.clone()),
            access_expires_at: Some(tokens.expires_at),
        },
    )?;

    // Refresh-token persistence always lands in the user XDG config so
    // that `patinate sync` (which loads from there by default) finds it
    // without further setup. If the operator passed `--config` on the
    // command line, we still write to the XDG path: a project-local
    // config shouldn't have a refresh token committed alongside it.
    let user_config = default_user_config_path()?;
    write_strava_section(
        &user_config,
        client_id,
        client_secret,
        &tokens.refresh_token,
    )?;

    println!(
        "OK. Athlete {athlete_id} authorized. Refresh token written to {}.",
        user_config.display()
    );
    if std::env::var_os("STRAVA_PATINATE_REFRESH_TOKEN").is_some() {
        println!(
            "Note: STRAVA_PATINATE_REFRESH_TOKEN is set in the current shell \
             and will shadow the file value on the next run. Unset it to \
             pick up the new refresh_token."
        );
    }
    println!("Run `patinate sync`.");

    Ok(())
}

/// Best-effort spawn of the user's default browser. Failures are
/// swallowed: the caller has already printed the URL so the operator
/// can fall back to copy-paste.
fn spawn_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        let (program, args) = cmd;
        if let Err(e) = std::process::Command::new(program).args(args).spawn() {
            tracing::debug!(error = %e, program, "could not spawn browser; URL printed above");
        }
    }
}

/// Upsert just the `[strava]` block of the user config TOML, preserving
/// every other section verbatim (round-tripped via the `toml` crate's
/// edit-friendly Value model). If the file doesn't exist, create it
/// with parents and write a minimal `[strava]` block.
fn write_strava_section(
    path: &Path,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create config directory {parent:?}. \
                 Check filesystem permissions or pre-create the path."
            )
        })?;
        // The directory holds Strava client_secret + refresh_token. On
        // a multi-user host any reader of `~/.config/patinate/` could
        // inspect the file; lock the dir down so even listing requires
        // ownership.
        set_user_only_dir_mode(parent)?;
    }

    // Parse-or-default. We deliberately use `toml::Table` (the value
    // model) rather than a typed deserialize: every other section in
    // the file is opaque to `patinate auth`, and we want to round-trip
    // it without dropping fields.
    let mut table: toml::Table = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read existing user config at {path:?}"))?;
        text.parse().with_context(|| {
            format!(
                "user config at {path:?} is not valid TOML. \
                 Fix or remove it, then re-run `patinate auth`."
            )
        })?
    } else {
        toml::Table::new()
    };

    let mut strava = match table.remove("strava") {
        Some(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    strava.insert(
        "client_id".to_string(),
        toml::Value::String(client_id.to_string()),
    );
    strava.insert(
        "client_secret".to_string(),
        toml::Value::String(client_secret.to_string()),
    );
    strava.insert(
        "refresh_token".to_string(),
        toml::Value::String(refresh_token.to_string()),
    );
    table.insert("strava".to_string(), toml::Value::Table(strava));

    let serialized = toml::to_string_pretty(&table).context(
        "could not serialize updated user config. \
         Open an issue against patinate with the offending TOML file.",
    )?;
    // Atomic write: tmp + chmod + rename. A kill mid-write would
    // otherwise leave the user with an empty or half-written TOML and a
    // confusing "is not valid TOML" error on the next sync. POSIX
    // rename is atomic on the same filesystem, so the user either sees
    // the old config or the new one, never a partial.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, serialized).with_context(|| {
        format!(
            "could not write {tmp:?}. Check filesystem permissions and \
             that the parent directory exists."
        )
    })?;
    // Set 0600 on the tmp BEFORE rename so there's no narrow window
    // where the world can read the secrets through the final path.
    set_user_only_file_mode(&tmp)?;
    std::fs::rename(&tmp, path).with_context(|| {
        format!("could not rename {tmp:?} to {path:?}. Filesystems must match.")
    })?;
    Ok(())
}

#[cfg(unix)]
fn set_user_only_file_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("could not set 0600 mode on {path:?} (file holds OAuth secrets)"))
}

#[cfg(not(unix))]
fn set_user_only_file_mode(_path: &Path) -> Result<()> {
    // Windows ACL hardening is out of scope for v0.1. The caller
    // should verify file permissions manually on non-Unix targets.
    Ok(())
}

#[cfg(unix)]
fn set_user_only_dir_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Always tightens to 0700, even when the operator had set a custom
    // mode (e.g. 0750 for a backup user). Log so the operator can
    // diagnose unexpected permission resets without grepping the source.
    tracing::debug!(path = %path.display(), "tightening directory mode to 0700");
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, perms).with_context(|| {
        format!("could not set 0700 mode on {path:?} (directory holds OAuth secrets)")
    })
}

#[cfg(not(unix))]
fn set_user_only_dir_mode(_path: &Path) -> Result<()> {
    Ok(())
}

fn list_themes_cmd(args: ListThemesArgs) -> Result<()> {
    // Embedded set first — these always work regardless of cwd or a
    // `--themes-dir` flag. They form the baseline a fresh
    // `cargo install --git` user gets.
    for name in theme::embedded_names() {
        println!("{name:<24} OK   embedded");
    }

    let Some(themes_dir) = args.themes_dir else {
        return Ok(());
    };
    if !themes_dir.exists() {
        bail!(
            "themes_dir {:?} does not exist. Drop the flag to list only \
             the embedded set, or point it at a directory of *.json themes.",
            themes_dir
        );
    }
    for entry in std::fs::read_dir(&themes_dir).with_context(|| {
        format!("could not read themes_dir {themes_dir:?}. Check filesystem permissions.")
    })? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match theme::load(&path) {
            Ok(t) => println!("{:<24} OK   {}", t.name, path.display()),
            Err(e) => println!("{:<24} ERR  {}: {e}", "?", path.display()),
        }
    }
    Ok(())
}

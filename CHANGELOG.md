# Changelog

## v0.1.0 -- first public release

End-to-end story for a single rider in a single city: install,
authenticate, sync, fetch a basemap, render.

### Added
- `--anonymize` flag on `patinate render` strips the per-activity
  `data-rider` / `data-bike` / `data-year` / `data-type` attributes
  from heat paths and scrubs the SVG `<desc>` to city + country only
  (no exact center coords or radius). Visible image is unchanged. Use
  whenever the SVG itself will be published. Threat model + per-attr
  audit lives in CONTRIBUTING.md.
- `--obfuscation-radius-m` flag overrides the config's
  `[privacy].obfuscation_radius_m` for a single render. Useful for
  publishing one-off art at a larger radius without editing the
  config file. The README recommends 1000 m or larger as a floor for
  public publication.

- `patinate auth`: interactive OAuth dance with Strava. Opens a
  browser, captures the authorization code on a localhost callback,
  exchanges it for a fresh refresh_token with `activity:read_all`
  scope, writes tokens to `~/.config/patinate/config.toml` `[strava]`
  block (preserving other sections via TOML round-trip). Five-minute
  outer timeout, two-second per-read idle timeout. The local HTTP
  reader drains until `\r\n\r\n` or 8 KiB cap, so requests that split
  across packets parse correctly. Prescriptive errors on bind /
  timeout / scope failures.
- `patinate fetch-osm`: fetch OSM basemap data for any city / region
  from the Overpass API. Supports city-name geocoding via Nominatim
  or explicit `--center-lat` / `--center-lng` / `--radius-m`. Polite
  single-request: a 429 or 5xx bails immediately with a prescriptive
  error (Overpass is a shared community resource). Endpoint
  configurable via `--endpoint` or `PATINATE_OVERPASS_ENDPOINT`;
  rejects non-https schemes to keep lat/lng off cleartext channels.
  200 MB streamed-response cap.
- Real polyline-circle obfuscation. Each activity polyline is decoded,
  walked segment-by-segment, and split at the obfuscation circle's
  edge. Inside-circle portions are removed. Chord-through-circle case
  handled (segment passes across the circle without either endpoint
  inside). Sub-meter precision via bisection. Activities entirely
  inside the circle are dropped. The renderer never sees the raw
  `summary_polyline` for activities that came through `apply()` with
  a positive radius.
- `--type`, `--gear`, `--cycling`, `--activity-id`, `--min-distance-m`
  filters on `patinate render`.
- `--web`, `--transparent-bg`, `--heat-only`, `--radius-m`,
  `--viewbox-width/-height` overrides. `--web` does single-layer heat
  (no glow stack), drops the visual typography group + tertiary +
  residential road tiers, and trims coord precision to one digit.
  The SVG `<title>` and `<desc>` accessibility metadata is preserved.
  File size on a 25 km metro is ~2.5 MB (dominated by OSM geometry,
  not heat); use `magick`/WebP for tighter inline budgets.
- Four shipped themes baked into the binary via `include_str!`:
  `noir_heat`, `blueprint_heat`, `warm_beige`, `cycle_heat`.
  `--themes-dir` adds custom themes alongside the embedded set.
- Per-theme glow tuning: `HeatStyle.glow_outer_ratio`,
  `glow_outer_alpha`, `glow_inner_ratio`, `glow_inner_alpha`. Defaults
  match the previous hardcoded numbers (3.5 / 0.025 / 1.7 / 0.06) so
  dark themes are byte-equivalent. Cream-paper themes (`warm_beige`,
  `cycle_heat`) lift these so the bloom registers against a warm
  background.
- Three-layer glow heat (outer halo / inner bloom / sharp core) with
  mixed blend modes for the poster variant.
- Heat path gap-break: if two consecutive Strava polyline points are
  >600 m apart in projected space (parameterized from the projection's
  scale-per-meter), break the path with `M` instead of drawing across
  the gap. Removes diagonal-line artifacts across rivers and lakes.
- OSM filter pipeline: tier-aware connected-components (keeps all
  major roads regardless of component size; minor-tier orphans
  dropped), tier-tuned min-way-length, Douglas-Peucker simplify.
- Cache: `PRAGMA user_version = 1` for v0.2 migrations to land
  cleanly. `busy_timeout(5s)` so a cron sync racing a manual run
  doesn't hit `database is locked`.
- Token persistence: `~/.config/patinate/config.toml` chmod 0600 with
  the parent directory chmod 0700 on Unix. Default umask was leaving
  the file group-readable.
- Strava rate limit handling: 429 honors `Retry-After` once with a 60s
  cap before bailing; documented in CONTRIBUTING and the
  known-limitations section.
- Config validation rejects NaN and infinity for every f64 field. Was
  silently bypassing obfuscation if `obfuscation_radius_m = nan` slipped
  through (range checks evaluate to false against NaN).
- CSRF state comparison via `subtle::ConstantTimeEq`. Exposed as
  `strava::auth::csrf_states_match` with unit tests for both the
  equal and length-mismatch paths.
- Web Mercator projection guards: bails with a prescriptive error
  for `|lat| >= 85.0`. Antimeridian crossing documented as a known
  limitation.
- 200 MB cap on Overpass response streaming. Replaces unbounded
  `resp.bytes().await`.
- Gzip-aware OSM fixture loader. `osm::load` sniffs the first two
  bytes for the gzip magic and decompresses via `flate2`
  transparently; both `basemap.osm.json` and `basemap.osm.json.gz`
  round-trip through one code path.
  `fixtures/grand-rapids.osm.json` (35 MB) compressed to 5.5 MB.
- README rewrite: install via `cargo install --git`, complete config
  example, full env-var reference, embed-in-page recipe, privacy,
  Strava ToS, security, known limitations.
- CONTRIBUTING.md: build prereqs, test layout, what's tested vs
  deferred, visual-iteration loop, voice rules.
- `Makefile` with `make example` (regenerate the README hero gallery)
  and `make site` (build `public/` for GitLab Pages).
- `.gitlab-ci.yml` runs `cargo fmt --check`, `cargo clippy -D warnings`,
  build, and `cargo test --lib` on push. `pages:` job deploys the
  `--web` demo to GitLab Pages on every main push.
- Lib test suite covering cache round-trip, polyline-circle clipping,
  OSM filtering, render compose, OAuth flow, Strava client. Includes
  a load-bearing privacy invariant test
  (`apply_privacy_invariant_loop_through_home`), an end-to-end SVG
  validity test on synthetic input
  (`render_e2e_synthetic_is_well_formed`), CSRF state-mismatch tests,
  and NaN-rejection tests for every f64 in the config validator.

### Changed

- Comprehensive design refresh on the poster output:
  - Top + bottom fade gradients dropped on every shipped theme. They
    were either invisible (cream-on-cream washed to nothing) or
    heavy-handed (dark themes covered ~18% of the canvas in solid
    color). Replaced with negative space + intentional typography.
  - Hairline inset border (1.5px stroke at theme text color, 0.6
    opacity) frames the basemap as a designed plate rather than a
    raw screenshot. Web preset skips the border, since the consumer
    page provides its own framing.
  - Typography restructured from a vertically-stacked center block to
    a horizontal data row pinned to the inner border: city name
    left-aligned, coordinates and a meta line (activity count + year
    span derived from the rendered slice) right-aligned, hairline
    rule above. Reads as cartographic legend, not poster caption.
  - Small attribution mark moves to the top-right corner.
  - Scale bar (smart 1/2/5/10/20/25/50 km picker, sized to about 10%
    of viewbox width) sits in the lower-left.
  - Major-road halo is now gated on background luminance: dark themes
    keep a toned halo (1.1x width, 0.05 alpha) so light-on-dark
    freeways stand out from the residential mesh; light-bg themes
    skip the halo entirely because on cream paper it reads as a
    shadow without adding signal.
- `--heat-bloom` and `--heat-alpha` runtime knobs let an operator
  scale the glow stack without forking a theme JSON. Default 1.0
  preserves theme-baked behavior.
- All CLI defaults that resolved to repo-relative paths
  (`fixtures/config.toml`, `fixtures/grand-rapids.osm.json`,
  `themes/`) now resolve at runtime to the per-user XDG paths or
  fall through to the embedded set. `cargo install --git` produces a
  binary that works from any cwd.
- `Tokens.athlete_id` populated inline from the auth-code response
  (no separate `/athlete` round trip on the auth path; the refresh
  path already had this).
- Doc comments and README updated to honest descriptions of what
  shipped (no claims drift from prior pre-release iterations).
- Toolchain pinned to rust 1.94 via `rust-toolchain.toml`. CI image
  matches. MSRV stays at 1.85.

### Removed

- Privacy fallback in `compose::build_heat`. Empty segments now drop
  the activity instead of decoding the raw `summary_polyline`.
- 35 MB committed `fixtures/grand-rapids.osm.json` (replaced with
  the 5.5 MB gzip variant).
- `pretty_assertions` dev-dep (was unused).
- `tokio` `rt-multi-thread` feature (we use `new_current_thread`
  everywhere).
- Dead `heat_blur` SVG filter that was defined in `<defs>` but never
  referenced.

### Known limitations (v0.2 candidates)

- Single-rider only. Multi-rider rendering deferred.
- No built-in PNG output. The README documents `rsvg-convert` and
  `magick` recipes.
- No JS interactivity layer. `data-rider` / `data-bike` / `data-year`
  / `data-type` attrs are emitted on heat paths; consumer pages can
  build hover / filter behavior on top.
- No schema migrations. `cache.db` is rebuildable from the Strava
  API; the upgrade path is `rm cache.db && patinate sync`.
- Sync is at-least-once on partial failure. The watermark only
  advances on a full successful sweep.
- No antimeridian handling. Cities within
  `radius_m / (111 km * cos(lat))` of longitude 180 produce a
  degenerate frame.

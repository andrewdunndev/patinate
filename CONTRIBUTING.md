# Contributing to patinate

## Build prerequisites

- Rust toolchain pinned in `rust-toolchain.toml` (1.94 at the time of
  v0.1). `rustup` reads that file on first build.
- `librsvg`'s `rsvg-convert` and ImageMagick's `magick` are required
  for `make example` and `make site`. The crate itself doesn't need
  them.
- macOS / Linux are tested. Windows is supported in code but not
  exercised by CI.

## Day-to-day commands

```bash
make release        # build optimized binary at target/release/patinate
make example        # rebuild the README hero gallery (3 themes)
make site           # build public/ for local preview of the demo page
make clean          # remove generated example + site artifacts

cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test --lib
cargo test --lib -- --ignored render_smoke   # heavy fixture-based
```

## Test layout

Lib tests organized per module:

| Module | What it covers |
|---|---|
| `cache::queries` | Round-trip and filter semantics on the SQLite store. |
| `cache::schema` | Table creation, idempotency, `PRAGMA user_version`. |
| `cache` | `open()` creates parents and initializes the schema. |
| `obfuscation` | Polyline-circle clipping correctness, including the privacy invariant test. |
| `osm::filter` | min-length, connected-component, simplify behaviors. |
| `osm::overpass` | Query template, Nominatim parsing, prescriptive errors. |
| `render::theme` | Hex-color validation, embedded-theme load. |
| `render::typography` | Title casing and coordinate formatting. |
| `render::compose` | End-to-end well-formedness on synthetic data. |
| `strava::auth` | Query parsing, URL decoding, CSRF compare, redirect URL. |
| `strava::client` | Wire-format decode, rate-limit parsing, Retry-After. |

The single `#[ignore]` test is `render::compose::tests::render_smoke`,
which loads the 5.5 MB gzipped Grand Rapids fixture and renders the
full poster. It runs in CI when explicitly invoked but is gated off
the default loop because the debug-mode render is a few seconds. The
default-running `render_e2e_synthetic_is_well_formed` exercises the
same code path on a synthetic basemap and finishes in milliseconds.

## What we test, what we don't

**Tested:**

- Privacy invariant: `obfuscation::apply()` followed by clipped-segment
  inspection guarantees no point inside the home circle reaches the
  output. `apply_privacy_invariant_loop_through_home` is the load-bearing
  test.
- SVG output well-formedness on synthetic and fixture inputs.
- Cache round-trip, including the OAuth token lifecycle.
- OAuth wire-format decode (refresh + auth-code paths), including the
  inline `athlete.id` extraction.
- Strava rate-limit handling: the `parse_retry_after` helper has its
  own test; the surrounding control flow is exercised manually against
  the live API.
- CSRF state comparison via `subtle::ConstantTimeEq`, including the
  short-circuit on mismatched length.
- Overpass query template generation, including the `(_link)?` regex
  that's load-bearing for interchange rendering.

**Deferred:**

- Live-network tests against Strava, Overpass, or Nominatim. CI does
  not have credentials and these endpoints are shared community
  resources we don't want to hit from automated runs.
- Schema-migration tests. v0.1 uses `PRAGMA user_version = 1` and
  ships no migrations; v0.2 will add idempotent ALTER paths and tests
  for each step.
- Multi-rider rendering. The data model already supports multiple
  athletes, but the renderer currently keys the data-rider attribute
  off a single athlete_id; the multi-rider path will land with v0.2.
- Antimeridian handling in the projection. Documented as a known
  limitation; affected geographies are vanishingly rare.

## Adding a fixture-based test

If a new test needs the real Grand Rapids basemap or the synthetic
activities fixture, mirror the pattern in
`render::compose::tests::render_smoke`: read the file directly under
`fixtures/`, deserialize, and run the rendering or filter stage.
Use `osm::load("fixtures/grand-rapids.osm.json.gz")` to get the
gzipped basemap; the loader transparently decompresses.

If the test depends on the 35 MB raw JSON expansion, mark it
`#[ignore]` so default runs stay fast.

## Reproducing visual changes

The visual tuning loop is:

```bash
cargo build --release
./target/release/patinate render \
    --config fixtures/config.toml \
    --osm fixtures/grand-rapids.osm.json.gz \
    --activities fixtures/activities.json \
    --theme noir_heat \
    --out /tmp/out.svg
rsvg-convert -w 600 /tmp/out.svg -o /tmp/out.png
```

When iterating on glow/halo ratios, render against `cycle_heat` or
`warm_beige` (cream-paper themes) at the same time so you don't
silently regress the high-contrast cases.

## Voice rules in patches

These apply to user-facing prose: README, CHANGELOG, error messages,
log lines, CLI help text, commit messages, and external comms (PR
descriptions, issues). Code comments are exempt; the source tree
predates the rule and a sweep would inflate diffs without adding
clarity.

- No em-dashes (`—`) in user-facing prose. Use `--`, parentheses,
  semicolons, or rewrite.
- No filler words: comprehensive, robust, leverage, straightforward.
- State things directly. Prefer "this fixes X" over "this might fix X".
- Keep comments to the WHY, not the WHAT. Function and variable names
  should explain themselves.

## Releasing

Tag releases on `main` after CI is green. The pipeline auto-deploys
the demo site to GitLab Pages on every `main` push.

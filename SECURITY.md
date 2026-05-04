# Security policy

## Supported versions

| Version | Supported          |
|---------|--------------------|
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

## Reporting a vulnerability

Email `andrew@dunn.dev` with `[patinate security]` in the subject
line. Expect an acknowledgement within 72 hours.

If the issue involves a privacy bypass (a render exposes data that
the obfuscation circle should have removed; the renderer reaches the
raw `summary_polyline` for an activity that came through `apply()`
with a positive radius; the `--anonymize` flag fails to strip a
`data-*` attribute or the `<desc>` element), include:

- The exact `patinate render` command line used.
- The relevant section of the resulting SVG (you can scrub real
  coordinates, but please preserve the structure).
- Your config's `[privacy]` section (lat / lng can be redacted; the
  `obfuscation_radius_m` value matters).

Do not file a public GitLab or GitHub issue for privacy bypasses.
Novel privacy bypasses warrant a patch release before disclosure.

## Out of scope

- Strava OAuth abuse: report to Strava via
  https://developers.strava.com.
- OpenStreetMap / Overpass / Nominatim availability or accuracy:
  upstream community resources, not patinate.
- Generic dependency CVEs: `cargo audit` runs clean at
  release. Subscribe to RustSec advisories for the dependency tree
  if you want eager notification.

// obfuscation — privacy clipping for activity polylines.
//
// Bundled enforcement: `ObfuscatedActivity` is the only type the
// renderer accepts; raw `Activity` cannot reach the render layer
// without passing through `apply()`. The constructor is the gate.
//
// Every polyline is decoded, clipped against a hidden zone, and the
// inside portions dropped; the renderer consumes the outside-only
// segment list via `segments()` and never sees `summary_polyline`.
//
// The hidden zone is not the home circle. Clipping at one exact circle
// on home puts every cut endpoint on that circle, and a circle fit over
// the render recovers home to metres. Instead, with `r` the configured
// radius, `rho` the configured offset and a secret salt:
//   - the zone's centre is drawn once from the salt, uniform over the
//     disk of radius rho around home, and its radius is r + rho + 1 m,
//     so the home disk stays strictly inside whatever offset is drawn;
//   - each cut end is then trimmed a further 0..r along the track, an
//     amount seeded by salt + activity id + crossing index, so the cut
//     endpoints scatter over a band instead of lying on a circle.
// A circle fit lands on the zone's centre, not on home. Tracks still
// converge toward home inside the gap; this hides the address within
// the zone, it does not erase the neighbourhood. All randomness comes
// from SHA-256 over the salt, so repeated renders are identical and
// cannot be averaged against each other.

use std::fmt;

use anyhow::{Result, anyhow, bail};
use geo::LineString;
use sha2::{Digest, Sha256};

use crate::strava::Activity;

/// Shown when a positive radius is requested without a salt. Loud by
/// design: the only salt-free fallback is the exact home circle.
pub const MISSING_SALT: &str = "\
[privacy].obfuscation_radius_m > 0 needs [privacy].salt, a private random \
string that decides where the hidden zone around home sits. Without it the \
zone would be an exact circle on home, which a circle fit over the render \
recovers to a few metres. Add to ~/.config/patinate/config.toml (never a \
public repo):

  [privacy]
  salt = \"<output of: openssl rand -hex 16>\"

or set PATINATE_PRIVACY__SALT. Keep it stable: a new salt moves the zone, and \
two renders with different salts narrow down home.";

/// Floor on `offset_m` for any positive radius. At 0 a circle fit
/// finds home within metres; at 50 m, within about 30 m.
pub const MIN_OFFSET_M: f64 = 250.0;

/// Operator secret seeding the hidden zone. Never rendered, never logged.
#[derive(Clone)]
pub struct PrivacySalt(Vec<u8>);

impl PrivacySalt {
    pub const MIN_LEN: usize = 16;

    pub fn new(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() < Self::MIN_LEN {
            bail!(
                "[privacy].salt is {} characters; use at least {}. \
                 Generate one with: openssl rand -hex 16",
                s.len(),
                Self::MIN_LEN
            );
        }
        Ok(Self(s.as_bytes().to_vec()))
    }

    /// Keyed PRF: a uniform `f64` in [0, 1) for `(domain, words)`.
    fn unit(&self, domain: &[u8], words: &[u64]) -> f64 {
        let mut h = Sha256::new();
        h.update((self.0.len() as u64).to_le_bytes());
        h.update(&self.0);
        h.update((domain.len() as u64).to_le_bytes());
        h.update(domain);
        for w in words {
            h.update(w.to_le_bytes());
        }
        let d = h.finalize();
        let mut b = [0u8; 8];
        b.copy_from_slice(&d[..8]);
        (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl fmt::Debug for PrivacySalt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivacySalt(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct ObfuscationParams {
    pub home_lat: f64,
    pub home_lng: f64,
    pub radius_m: f64,
    /// Bound on the secret offset of the zone centre from home, metres.
    /// At least `MIN_OFFSET_M` whenever `radius_m > 0`.
    pub offset_m: f64,
    /// Required when `radius_m > 0`; `apply()` refuses to run without it.
    pub salt: Option<PrivacySalt>,
}

/// An activity that has passed obfuscation. Carries the wrapped
/// `Activity` plus the clipped polyline split into 0+ outside-only
/// segments as `(lat, lng)` pairs. The renderer consumes
/// `segments()`; for radius-0 (obfuscation disabled) the segment
/// list contains a single segment that is the full decoded polyline.
#[derive(Debug, Clone)]
pub struct ObfuscatedActivity {
    activity: Activity,
    clipped_segments: Vec<Vec<(f64, f64)>>,
}

impl ObfuscatedActivity {
    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Outside-zone polyline pieces. Each inner `Vec` is one
    /// continuous run of `(lat, lng)` points that the renderer should
    /// stroke as a single sub-path.
    pub fn segments(&self) -> &[Vec<(f64, f64)>] {
        &self.clipped_segments
    }
}

/// The region removed from every render: a disk that contains the
/// home disk but is not centred on it, plus per-crossing trims.
struct HiddenZone<'a> {
    center: (f64, f64),
    radius_m: f64,
    max_trim_m: f64,
    salt: &'a PrivacySalt,
}

impl<'a> HiddenZone<'a> {
    fn derive(params: &ObfuscationParams, salt: &'a PrivacySalt) -> Self {
        // Drawn once from the salt alone, uniform over the disk of
        // radius `offset_m`: every render with this salt shares the
        // centre, whatever its radius.
        let theta = std::f64::consts::TAU * salt.unit(b"zone-direction", &[]);
        let dist_m = params.offset_m * salt.unit(b"zone-offset", &[]).sqrt();
        let (hlat, hlng) = (params.home_lat, params.home_lng);
        const EARTH_R: f64 = 6_371_000.0;
        let center = (
            hlat + (dist_m * theta.cos() / EARTH_R).to_degrees(),
            hlng + (dist_m * theta.sin() / (EARTH_R * hlat.to_radians().cos())).to_degrees(),
        );
        // Sized for the largest possible offset, not the drawn one, so
        // the radius says nothing about where home sits inside the
        // disk. The 1 m covers bisection slack.
        Self {
            center,
            radius_m: params.radius_m + params.offset_m + 1.0,
            max_trim_m: params.radius_m,
            salt,
        }
    }

    fn contains(&self, lat: f64, lng: f64) -> bool {
        haversine_m(lat, lng, self.center.0, self.center.1) <= self.radius_m
    }

    fn trim_m(&self, activity_id: i64, crossing: u64) -> f64 {
        self.max_trim_m
            * self
                .salt
                .unit(b"crossing-trim", &[activity_id as u64, crossing])
    }
}

/// Apply obfuscation to a slice of activities. Each polyline is
/// clipped against the hidden zone (see the module comment); inside
/// portions are removed and every cut end is trimmed further along
/// the track. Activities with nothing left outside the zone (or with
/// no polyline and a start inside it) are dropped. `radius_m = 0.0`
/// disables clipping and keeps the full decoded polyline as one
/// segment. A positive radius without a salt is an error, and so is
/// a radius or offset that is negative or not finite: every caller,
/// the CLI override included, passes through here.
pub fn apply(
    activities: impl IntoIterator<Item = Activity>,
    params: ObfuscationParams,
) -> Result<Vec<ObfuscatedActivity>> {
    for (name, v) in [
        ("obfuscation radius", params.radius_m),
        ("[privacy].offset_m", params.offset_m),
    ] {
        if !v.is_finite() || v < 0.0 {
            bail!("{name} = {v} must be a finite number >= 0");
        }
    }
    let zone = if params.radius_m > 0.0 {
        if params.offset_m < MIN_OFFSET_M {
            bail!(
                "[privacy].offset_m = {} is below the {MIN_OFFSET_M} m floor. A \
                 smaller offset leaves the zone nearly centred on home, which \
                 a circle fit finds. Raise it or leave it out for the default.",
                params.offset_m
            );
        }
        let salt = params.salt.as_ref().ok_or_else(|| anyhow!(MISSING_SALT))?;
        Some(HiddenZone::derive(&params, salt))
    } else {
        None
    };
    Ok(activities
        .into_iter()
        .filter_map(|a| build_obfuscated(a, zone.as_ref()))
        .collect())
}

fn build_obfuscated(activity: Activity, zone: Option<&HiddenZone>) -> Option<ObfuscatedActivity> {
    // No polyline: fall back to start-point distance. A polyline-less
    // activity can't be split, so the only honest behavior is to drop
    // it when it starts inside the zone.
    if activity.summary_polyline.is_empty() {
        if zone.is_some_and(|z| z.contains(activity.start_lat, activity.start_lng)) {
            return None;
        }
        return Some(ObfuscatedActivity {
            activity,
            clipped_segments: Vec::new(),
        });
    }

    let line = match polyline::decode_polyline(&activity.summary_polyline, 5) {
        Ok(l) => l,
        // Polyline decode failed: drop. Better than rendering a
        // possibly-misplaced trace through the privacy zone.
        Err(_) => return None,
    };

    let segments = match zone {
        None => {
            // Obfuscation disabled: emit the full polyline as one segment.
            let pts: Vec<(f64, f64)> = line.0.iter().map(|c| (c.y, c.x)).collect();
            if pts.len() < 2 {
                return None;
            }
            vec![pts]
        }
        Some(z) => clip_and_trim(&line, z, activity.id),
    };

    if segments.is_empty() {
        return None;
    }
    Some(ObfuscatedActivity {
        activity,
        clipped_segments: segments,
    })
}

/// Clip against the zone's disk, then trim each cut end (not the
/// activity's own start or finish) a seeded distance along the run.
/// Runs too short to survive their trims are dropped.
fn clip_and_trim(
    line: &LineString<f64>,
    zone: &HiddenZone,
    activity_id: i64,
) -> Vec<Vec<(f64, f64)>> {
    let runs = clip_to_outside_circle(line, zone.center, zone.radius_m);
    let starts_outside = line.0.first().is_some_and(|c| !zone.contains(c.y, c.x));
    let ends_outside = line.0.last().is_some_and(|c| !zone.contains(c.y, c.x));
    let last = runs.len().saturating_sub(1);
    let mut crossing = 0u64;
    let mut next_trim = || {
        let t = zone.trim_m(activity_id, crossing);
        crossing += 1;
        t
    };
    let mut out = Vec::with_capacity(runs.len());
    for (i, mut run) in runs.into_iter().enumerate() {
        if !(i == 0 && starts_outside) {
            match trim_front(&run, next_trim()) {
                Some(r) => run = r,
                None => continue,
            }
        }
        if !(i == last && ends_outside) {
            run.reverse();
            match trim_front(&run, next_trim()) {
                Some(r) => run = r,
                None => continue,
            }
            run.reverse();
        }
        out.push(run);
    }
    out
}

/// Drop the first `trim_m` metres of a run, interpolating the new
/// first point. `None` when the run is not longer than `trim_m`.
fn trim_front(run: &[(f64, f64)], trim_m: f64) -> Option<Vec<(f64, f64)>> {
    let mut walked = 0.0;
    for j in 0..run.len().saturating_sub(1) {
        let (a, b) = (run[j], run[j + 1]);
        let seg = haversine_m(a.0, a.1, b.0, b.1);
        if walked + seg > trim_m {
            let t = if seg > 0.0 {
                (trim_m - walked) / seg
            } else {
                0.0
            };
            let mut out = Vec::with_capacity(run.len() - j);
            out.push(lerp(a, b, t));
            out.extend_from_slice(&run[j + 1..]);
            return Some(out);
        }
        walked += seg;
    }
    None
}

/// Clip a polyline against a circle, keeping only the portions that
/// lie entirely outside. The output is a list of `(lat, lng)` runs;
/// each run is a continuous sub-path with all points outside the
/// circle (intersection points sit exactly on the boundary, treated
/// as outside). Inside-circle portions are dropped.
///
/// Algorithm: walk consecutive point pairs. Classify by inside/outside
/// status of each endpoint, then handle the four cases:
///   - both outside, no chord: append to current run
///   - both inside: flush current run, drop segment
///   - one in / one out: bisect to the boundary, keep the outside half
///   - both outside but chord-through: bisect both crossings, emit
///     the front-side run as a complete piece, start a new run from
///     the back-side crossing
///
/// "Inside" uses haversine distance to the center compared against
/// `radius_m`. Segment parametrization is linear in `(lat, lng)`
/// space, which is fine at the obfuscation scale (sub-kilometer)
/// where lat/lng degrees are nearly cartesian.
pub fn clip_to_outside_circle(
    line: &LineString<f64>,
    center: (f64, f64),
    radius_m: f64,
) -> Vec<Vec<(f64, f64)>> {
    let pts: Vec<(f64, f64)> = line.0.iter().map(|c| (c.y, c.x)).collect();
    if pts.len() < 2 {
        return Vec::new();
    }

    let (clat, clng) = center;
    let dist = |p: (f64, f64)| haversine_m(p.0, p.1, clat, clng);

    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();

    for i in 0..pts.len() - 1 {
        let a = pts[i];
        let b = pts[i + 1];
        let da = dist(a);
        let db = dist(b);
        let a_in = da <= radius_m;
        let b_in = db <= radius_m;

        match (a_in, b_in) {
            (false, false) => {
                // Both endpoints outside — but the segment may still
                // chord through the circle. Find the closest point
                // on the (lat, lng)-linear segment to the center; if
                // that's inside, we have a chord case.
                let t_min = closest_t(a, b, center);
                let p_min = lerp(a, b, t_min);
                if dist(p_min) < radius_m {
                    // Chord case: bisect [0, t_min] for entry, then
                    // [t_min, 1] for exit. Each bisection is a 1D
                    // root-find on f(t) = dist(lerp(a,b,t)) - radius.
                    let t_in = bisect_boundary(a, b, center, radius_m, 0.0, t_min);
                    let t_out = bisect_boundary(a, b, center, radius_m, t_min, 1.0);
                    let p_in = lerp(a, b, t_in);
                    let p_out = lerp(a, b, t_out);
                    // Front piece: a -> p_in. Stitch onto current run
                    // if non-empty (current's last point should be a).
                    if current.is_empty() {
                        current.push(a);
                    }
                    current.push(p_in);
                    out.push(std::mem::take(&mut current));
                    // Back piece begins at p_out and continues to b.
                    current.push(p_out);
                    current.push(b);
                } else {
                    // Pure outside segment.
                    if current.is_empty() {
                        current.push(a);
                    }
                    current.push(b);
                }
            }
            (true, true) => {
                // Both inside: flush whatever was in flight and skip.
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            (true, false) => {
                // Crossing outward: trim from a to the boundary, then
                // start a new outside run at the boundary.
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                let t = bisect_boundary(a, b, center, radius_m, 0.0, 1.0);
                let p = lerp(a, b, t);
                current.push(p);
                current.push(b);
            }
            (false, true) => {
                // Crossing inward: extend current run up to the
                // boundary, then flush.
                if current.is_empty() {
                    current.push(a);
                }
                let t = bisect_boundary(a, b, center, radius_m, 0.0, 1.0);
                let p = lerp(a, b, t);
                current.push(p);
                out.push(std::mem::take(&mut current));
            }
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    // Drop any degenerate single-point runs that can sneak in if the
    // very first point is exactly on the boundary.
    out.retain(|run| run.len() >= 2);
    out
}

/// Linear interpolation in (lat, lng) space. Valid for short segments
/// where the great-circle deviation from a straight line is below
/// our sub-meter precision target.
fn lerp(a: (f64, f64), b: (f64, f64), t: f64) -> (f64, f64) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// Find `t` in [`lo`, `hi`] such that `dist(lerp(a, b, t)) == radius`.
/// Assumes `f(lo)` and `f(hi)` straddle zero (one inside, one
/// outside, or the chord-case half-intervals which both straddle by
/// construction). 24 iterations of bisection over a sub-kilometer
/// segment gets well below sub-meter precision.
fn bisect_boundary(
    a: (f64, f64),
    b: (f64, f64),
    center: (f64, f64),
    radius_m: f64,
    mut lo: f64,
    mut hi: f64,
) -> f64 {
    let f = |t: f64| {
        let p = lerp(a, b, t);
        haversine_m(p.0, p.1, center.0, center.1) - radius_m
    };
    let f_lo = f(lo);
    let f_hi = f(hi);
    // If the interval doesn't straddle (numerical edge cases on the
    // boundary), pick whichever endpoint is closer to zero.
    if f_lo == 0.0 {
        return lo;
    }
    if f_hi == 0.0 {
        return hi;
    }
    if f_lo.signum() == f_hi.signum() {
        return if f_lo.abs() < f_hi.abs() { lo } else { hi };
    }
    // Track sign of the "lo" side; bisect toward the opposite sign.
    let lo_negative = f_lo < 0.0;
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        let f_mid = f(mid);
        if f_mid == 0.0 {
            return mid;
        }
        let mid_negative = f_mid < 0.0;
        if mid_negative == lo_negative {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Approximate `t` in [0, 1] that minimizes haversine distance from
/// `lerp(a, b, t)` to `center`. Uses ternary search; 32 iterations
/// converges to ~1e-10 in `t`, far below the precision needed for
/// chord detection.
fn closest_t(a: (f64, f64), b: (f64, f64), center: (f64, f64)) -> f64 {
    let f = |t: f64| {
        let p = lerp(a, b, t);
        haversine_m(p.0, p.1, center.0, center.1)
    };
    let mut lo = 0.0_f64;
    let mut hi = 1.0_f64;
    for _ in 0..32 {
        let m1 = lo + (hi - lo) / 3.0;
        let m2 = hi - (hi - lo) / 3.0;
        if f(m1) < f(m2) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    0.5 * (lo + hi)
}

/// Great-circle distance in meters between two lat/lng points.
fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lng2 - lng1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    R * c
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use geo_types::{Coord, LineString};

    fn act(lat: f64, lng: f64) -> Activity {
        Activity {
            id: 1,
            name: "t".into(),
            activity_type: crate::strava::ActivityType::Ride,
            start_date: Utc::now(),
            start_lat: lat,
            start_lng: lng,
            distance_m: 0.0,
            moving_time_s: 0,
            summary_polyline: String::new(),
            athlete_id: 1,
            gear_id: None,
        }
    }

    fn line_from(pts: &[(f64, f64)]) -> LineString<f64> {
        // (lat, lng) tuples → LineString of (x=lng, y=lat) coords,
        // matching what `polyline::decode_polyline` produces.
        LineString::from(
            pts.iter()
                .map(|(lat, lng)| Coord { x: *lng, y: *lat })
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn drops_activities_within_radius() {
        // Both rides are polyline-less, so apply() falls back to the
        // start-point check. `near` is inside, `far` is outside.
        let near = act(42.9619, -85.6218);
        let far = act(42.94, -85.60);
        let kept = apply(
            vec![near, far],
            ObfuscationParams {
                home_lat: 42.96,
                home_lng: -85.622,
                radius_m: 250.0,
                salt: Some(salt()),
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        assert_eq!(kept.len(), 1);
        assert!((kept[0].activity().start_lat - 42.94).abs() < 1e-6);
    }

    #[test]
    fn radius_zero_disables_obfuscation() {
        let kept = apply(
            vec![act(42.96, -85.622)],
            ObfuscationParams {
                home_lat: 42.96,
                home_lng: -85.622,
                radius_m: 0.0,
                salt: None,
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn clip_drops_segment_entirely_inside() {
        // 4 points within ~50m of center, radius 500m.
        let center = (42.96, -85.622);
        let line = line_from(&[
            (42.9619, -85.6218),
            (42.9620, -85.6217),
            (42.9618, -85.6216),
            (42.9617, -85.6219),
        ]);
        let segs = clip_to_outside_circle(&line, center, 500.0);
        assert!(
            segs.is_empty(),
            "all-inside polyline should clip to nothing"
        );
    }

    #[test]
    fn clip_keeps_segment_entirely_outside() {
        // 4 points all >2km from center, radius 500m. The segments
        // also don't chord through.
        let center = (42.96, -85.622);
        let line = line_from(&[
            (42.94, -85.60),
            (42.943, -85.598),
            (42.944, -85.599),
            (42.945, -85.600),
        ]);
        let segs = clip_to_outside_circle(&line, center, 500.0);
        assert_eq!(segs.len(), 1, "single contiguous outside run expected");
        assert_eq!(segs[0].len(), 4, "all 4 input points preserved");
    }

    #[test]
    fn clip_one_in_one_out() {
        // a inside, b outside. Expect a single 2-point segment:
        // [boundary_point, b].
        let center = (42.96, -85.622);
        let radius = 500.0;
        let a = (42.9619, -85.6218); // ~15m from center, inside
        let b = (42.94, -85.60); // ~3km from center, outside
        let line = line_from(&[a, b]);
        let segs = clip_to_outside_circle(&line, center, radius);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].len(), 2);
        // First point of the kept run should sit on the circle.
        let p = segs[0][0];
        let d = haversine_m(p.0, p.1, center.0, center.1);
        assert!(
            (d - radius).abs() < 1.0,
            "boundary precision: {} m off",
            (d - radius).abs()
        );
        // Second point is b unchanged.
        assert!((segs[0][1].0 - b.0).abs() < 1e-9);
        assert!((segs[0][1].1 - b.1).abs() < 1e-9);
    }

    #[test]
    fn apply_privacy_invariant_loop_through_home() {
        // Integration test for the README's privacy claim: a ride that
        // starts across town, loops through the home circle, and
        // returns. After apply(), no `(lat, lng)` in any segment may
        // sit inside the home circle. This is the load-bearing test.
        let home = (42.96_f64, -85.622_f64);
        let radius = 250.0_f64;
        // Hand-pick 12 points that trace a there-and-back route through
        // home: starts 4km west, jogs north, dips south *through* the
        // home circle, and exits east. Several consecutive points are
        // inside the circle.
        let pts: Vec<(f64, f64)> = vec![
            (42.96, -85.66),     // start, ~3.3km W
            (42.964, -85.65),    // approaching
            (42.966, -85.64),    // approaching
            (42.965, -85.63),    // approaching
            (42.963, -85.624),   // ~25m N of home, INSIDE
            (42.9616, -85.6222), // ~25m S of home, INSIDE
            (42.961, -85.621),   // ~125m S, INSIDE
            (42.964, -85.619),   // ~325m NE, OUTSIDE
            (42.966, -85.612),   // departing
            (42.963, -85.602),   // departing
            (42.96, -85.592),    // end, ~2.4km E
            (42.96, -85.59),     // end, ~2.8km E
        ];
        let polyline_str = polyline::encode_coordinates(
            pts.iter()
                .map(|&(lat, lng)| geo_types::Coord { x: lng, y: lat }),
            5,
        )
        .expect("encode polyline");
        let mut a = act(42.96, -85.66);
        a.summary_polyline = polyline_str;
        let kept = apply(
            vec![a],
            ObfuscationParams {
                home_lat: home.0,
                home_lng: home.1,
                radius_m: radius,
                salt: Some(salt()),
                offset_m: 750.0,
            },
        )
        .expect("obfuscate");
        assert_eq!(kept.len(), 1, "ride that exits the circle must be kept");
        // No point in any kept segment may sit inside the home circle.
        // Boundary points (within sub-meter) are the bisection output
        // and count as on-circle, not inside.
        for seg in kept[0].segments() {
            for &(lat, lng) in seg {
                let d = haversine_m(lat, lng, home.0, home.1);
                assert!(
                    d >= radius - 1.0,
                    "privacy invariant violated: point ({lat}, {lng}) is {} m from home, radius {}",
                    d,
                    radius
                );
            }
        }
        // Also: the resulting segment list must split into 2+ runs
        // because the polyline straddles the circle.
        assert!(
            kept[0].segments().len() >= 2,
            "expected at least one inside-the-circle gap; got {} segments",
            kept[0].segments().len()
        );
    }

    #[test]
    fn clip_chord_through_circle() {
        // Two points on opposite sides of the circle; the straight
        // segment between them passes through the center. Expect two
        // outside runs, each with the original endpoint plus a
        // boundary intersection.
        let center = (42.96, -85.622);
        let radius = 200.0;
        // ~1.1km west and ~1.1km east of center along the same
        // latitude. The midpoint is the center, so the chord goes
        // straight through.
        let a = (42.96, -85.64);
        let b = (42.96, -85.61);
        let line = line_from(&[a, b]);
        let segs = clip_to_outside_circle(&line, center, radius);
        assert_eq!(segs.len(), 2, "chord-through should yield two runs");
        assert_eq!(segs[0].len(), 2);
        assert_eq!(segs[1].len(), 2);

        // First run: [a, entry_boundary]
        assert!((segs[0][0].0 - a.0).abs() < 1e-9);
        assert!((segs[0][0].1 - a.1).abs() < 1e-9);
        let d_in = haversine_m(segs[0][1].0, segs[0][1].1, center.0, center.1);
        assert!((d_in - radius).abs() < 1.0);

        // Second run: [exit_boundary, b]
        let d_out = haversine_m(segs[1][0].0, segs[1][0].1, center.0, center.1);
        assert!((d_out - radius).abs() < 1.0);
        assert!((segs[1][1].0 - b.0).abs() < 1e-9);
        assert!((segs[1][1].1 - b.1).abs() < 1e-9);
    }

    // ---- hidden-zone tests: synthetic data only ----

    const HOME: (f64, f64) = (42.96, -85.622);
    const EARTH_R: f64 = 6_371_000.0;
    const RHO: f64 = 750.0;

    fn salt() -> PrivacySalt {
        PrivacySalt::new("test-salt-not-a-secret").expect("salt")
    }

    fn salt_n(i: u64) -> PrivacySalt {
        PrivacySalt::new(&format!("synthetic-test-salt-{i:04}")).expect("salt")
    }

    fn params(radius_m: f64, salt: PrivacySalt) -> ObfuscationParams {
        ObfuscationParams {
            home_lat: HOME.0,
            home_lng: HOME.1,
            radius_m,
            salt: Some(salt),
            offset_m: RHO,
        }
    }

    /// Local metres (east, north) from HOME to (lat, lng).
    fn to_latlng(e: f64, n: f64) -> (f64, f64) {
        (
            HOME.0 + (n / EARTH_R).to_degrees(),
            HOME.1 + (e / (EARTH_R * HOME.0.to_radians().cos())).to_degrees(),
        )
    }

    fn to_local(p: (f64, f64)) -> (f64, f64) {
        (
            (p.1 - HOME.1).to_radians() * EARTH_R * HOME.0.to_radians().cos(),
            (p.0 - HOME.0).to_radians() * EARTH_R,
        )
    }

    /// Tiny deterministic LCG so the synthetic set needs no RNG crate.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    fn activity_from(id: i64, pts: &[(f64, f64)]) -> Activity {
        let mut a = act(pts[0].0, pts[0].1);
        a.id = id;
        a.summary_polyline =
            polyline::encode_coordinates(pts.iter().map(|&(lat, lng)| Coord { x: lng, y: lat }), 5)
                .expect("encode polyline");
        a
    }

    /// `n` activities radiating from home: most leave from near the
    /// door and head out 4r..10r; every fourth is a through route that
    /// passes within 0.3r of home. Returns the decoded natural
    /// endpoints alongside, so tests can pick out the cut ends.
    fn radiating(n: usize, r: f64) -> Vec<Activity> {
        let mut rng = Lcg(0x5eed);
        let mut out = Vec::new();
        for k in 0..n {
            let theta = std::f64::consts::TAU * (k as f64 + rng.next()) / n as f64;
            let len = r * (4.0 + 6.0 * rng.next());
            let (c, s) = (theta.cos(), theta.sin());
            let mut pts = Vec::new();
            if k % 4 == 3 {
                // Through route: far side, past home, out the other way.
                let miss = r * 0.3 * (rng.next() - 0.5);
                let steps = 60;
                for i in 0..=steps {
                    let t = -len + 2.0 * len * i as f64 / steps as f64;
                    pts.push(to_latlng(t * c - miss * s, t * s + miss * c));
                }
            } else {
                let start = r * 0.3 * rng.next();
                let steps = (len / (r / 3.0)) as usize;
                for i in 0..=steps {
                    let t = start + (len - start) * i as f64 / steps as f64;
                    let wiggle = r * 0.05 * (rng.next() - 0.5);
                    pts.push(to_latlng(t * c - wiggle * s, t * s + wiggle * c));
                }
            }
            out.push(activity_from(1000 + k as i64, &pts));
        }
        out
    }

    /// Segment endpoints that are not an activity's own start or
    /// finish: exactly what the clip and trim produced.
    fn cut_endpoints(kept: &[ObfuscatedActivity]) -> Vec<(f64, f64)> {
        let mut pts = Vec::new();
        for ob in kept {
            let line = polyline::decode_polyline(&ob.activity().summary_polyline, 5).unwrap();
            let first = (line.0[0].y, line.0[0].x);
            let last = (line.0[line.0.len() - 1].y, line.0[line.0.len() - 1].x);
            for seg in ob.segments() {
                for p in [seg[0], seg[seg.len() - 1]] {
                    if p != first && p != last {
                        pts.push(to_local(p));
                    }
                }
            }
        }
        pts
    }

    /// Algebraic (Kasa) least-squares circle fit: (cx, cy, radius).
    fn lsq_circle(pts: &[(f64, f64)]) -> (f64, f64, f64) {
        let (mut sxx, mut sxy, mut syy, mut sx, mut sy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut sxz, mut syz, mut sz) = (0.0, 0.0, 0.0);
        let n = pts.len() as f64;
        for &(x, y) in pts {
            let z = x * x + y * y;
            sxx += x * x;
            sxy += x * y;
            syy += y * y;
            sx += x;
            sy += y;
            sxz += x * z;
            syz += y * z;
            sz += z;
        }
        // Solve [sxx sxy sx; sxy syy sy; sx sy n] [a b c] = -[sxz syz sz].
        let m = [[sxx, sxy, sx], [sxy, syy, sy], [sx, sy, n]];
        let rhs = [-sxz, -syz, -sz];
        let det = |m: [[f64; 3]; 3]| {
            m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
                - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
                + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
        };
        let d = det(m);
        let col = |i: usize| {
            let mut mi = m;
            for (row, v) in mi.iter_mut().zip(rhs) {
                row[i] = v;
            }
            det(mi) / d
        };
        let (a, b, c) = (col(0), col(1), col(2));
        let (cx, cy) = (-a / 2.0, -b / 2.0);
        (cx, cy, (cx * cx + cy * cy - c).max(0.0).sqrt())
    }
    /// Kasa fit over the cut endpoints of one render: (centre, radius)
    /// in local metres around home.
    fn fit_render(r: f64, salt: PrivacySalt) -> ((f64, f64), f64) {
        let kept = apply(radiating(120, r), params(r, salt)).expect("obfuscate");
        let pts = cut_endpoints(&kept);
        assert!(pts.len() >= 120, "too few cut endpoints: {}", pts.len());
        let (cx, cy, rad) = lsq_circle(&pts);
        ((cx, cy), rad)
    }

    #[test]
    fn circle_fit_misses_home_across_salts() {
        // I2, statistically. Across 64 salts the fitted centre must sit
        // well away from home on average: a uniform draw over the
        // offset disk gives about 0.67 rho, the exact home-circle clip
        // gives 0. No fitted radius may land near r.
        let r = 250.0;
        let mut total = 0.0;
        for i in 0..64 {
            let (c, rad) = fit_render(r, salt_n(i));
            total += c.0.hypot(c.1);
            assert!(
                (rad - r).abs() > 0.25 * r,
                "salt {i}: fit radius {rad:.1} m reveals r"
            );
        }
        let mean = total / 64.0;
        assert!(mean >= 0.4 * RHO, "mean fit miss {mean:.1} m < 0.4 rho");
    }

    #[test]
    fn offset_direction_spreads_across_salts() {
        // A fixed direction would let one render's fit point at home
        // for every user. Mean resultant length of the fitted-centre
        // bearings: about 0.1 for uniform bearings, 1 for a constant.
        let (mut sx, mut sy) = (0.0, 0.0);
        for i in 0..64 {
            let (c, _) = fit_render(250.0, salt_n(i));
            let d = c.0.hypot(c.1);
            sx += c.0 / d;
            sy += c.1 / d;
        }
        let resultant = sx.hypot(sy) / 64.0;
        assert!(
            resultant < 0.3,
            "bearings cluster: resultant {resultant:.2}"
        );
    }

    #[test]
    fn renders_at_two_radii_share_the_centre() {
        // Renders with one salt at r = 250 and r = 1000 must not offer
        // two different centres to intersect, and neither zone may
        // touch the home disk (no tangency puts home on a known ring).
        for i in 0..64 {
            let s = salt_n(i);
            let z1 = HiddenZone::derive(&params(250.0, s.clone()), &s);
            let z2 = HiddenZone::derive(&params(1000.0, s.clone()), &s);
            assert_eq!(z1.center, z2.center, "salt {i}: centre moved with r");
            // The radius is fixed by r and offset_m alone: one that moved
            // with the drawn offset would place home on a known ring.
            assert_eq!(z1.radius_m, 250.0 + RHO + 1.0, "salt {i}: radius moved");
            assert_eq!(z2.radius_m, 1000.0 + RHO + 1.0, "salt {i}: radius moved");
            for (z, r) in [(&z1, 250.0), (&z2, 1000.0)] {
                let off = haversine_m(HOME.0, HOME.1, z.center.0, z.center.1);
                assert!(off <= RHO + 0.01, "salt {i}: offset {off:.2} > rho");
                assert!(
                    z.radius_m - (off + r) >= 1.0 - 1e-6,
                    "salt {i}, r {r}: zone touches the home disk"
                );
            }
        }
    }

    #[test]
    fn cut_ends_do_not_trace_a_circle() {
        // Without trims every cut end sits on the zone's edge. No 5 m
        // band around the zone centre may hold more than 15% of them.
        let r = 250.0;
        for i in 0..8 {
            let s = salt_n(i);
            let z = HiddenZone::derive(&params(r, s.clone()), &s);
            let kept = apply(radiating(240, r), params(r, s.clone())).expect("obfuscate");
            let c = to_local(z.center);
            let mut d: Vec<f64> = cut_endpoints(&kept)
                .iter()
                .map(|p| (p.0 - c.0).hypot(p.1 - c.1))
                .collect();
            d.sort_by(f64::total_cmp);
            let mut densest = 0;
            for (j, &lo) in d.iter().enumerate() {
                densest = densest.max(d[j..].partition_point(|&x| x < lo + 0.02 * r));
            }
            let frac = densest as f64 / d.len() as f64;
            assert!(
                frac < 0.15,
                "salt {i}: {:.0}% of cut ends on one ring",
                frac * 100.0
            );
        }
    }

    #[test]
    fn trims_vary_per_activity() {
        // The trim is seeded per activity: one shared trim per crossing
        // index would line the cut ends up again.
        let s = salt();
        let z = HiddenZone::derive(&params(250.0, s.clone()), &s);
        let t: Vec<f64> = (1..=200).map(|id| z.trim_m(id, 0)).collect();
        let mean = t.iter().sum::<f64>() / t.len() as f64;
        let sd = (t.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / t.len() as f64).sqrt();
        assert!(sd > 0.2 * 250.0, "trim spread {sd:.1} m across activities");
    }

    #[test]
    fn no_output_vertex_within_radius_of_home() {
        // I1 over many salts and radii, on routes that start at the
        // door and routes that pass straight through.
        for r in [100.0, 250.0, 1000.0] {
            let acts = radiating(80, r);
            for i in 0..8 {
                let kept = apply(acts.clone(), params(r, salt_n(i))).expect("obfuscate");
                assert!(!kept.is_empty());
                for ob in &kept {
                    for &(lat, lng) in ob.segments().iter().flatten() {
                        let d = haversine_m(lat, lng, HOME.0, HOME.1);
                        assert!(d >= r, "salt {i}: vertex {d:.2} m from home, r = {r}");
                    }
                }
            }
        }
    }

    #[test]
    fn hidden_zone_contains_home_disk() {
        // I1 at the zone level: every point within r of home, the rim
        // included, sits inside the offset disk.
        for r in [50.0, 250.0, 1000.0] {
            for i in 0..32 {
                let s = salt_n(i);
                let z = HiddenZone::derive(&params(r, s.clone()), &s);
                for k in 0..360 {
                    let a = (k as f64).to_radians();
                    let (lat, lng) = to_latlng(r * a.cos(), r * a.sin());
                    assert!(
                        z.contains(lat, lng),
                        "salt {i}: rim point {k} deg outside zone"
                    );
                }
            }
        }
    }

    #[test]
    fn output_is_deterministic_and_seeded_per_activity() {
        // I3. Same config, same output, so renders can't be averaged.
        // The cut moves with the activity id and with the salt.
        let r = 250.0;
        let acts = radiating(40, r);
        let run = |s: PrivacySalt| {
            apply(acts.clone(), params(r, s))
                .expect("obfuscate")
                .iter()
                .map(|o| o.segments().to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(run(salt()), run(salt()));
        assert_ne!(run(salt()), run(salt_n(1)));

        let pts: Vec<(f64, f64)> = (0..=80).map(|i| to_latlng(50.0 * i as f64, 0.0)).collect();
        let twins = vec![activity_from(1, &pts), activity_from(2, &pts)];
        let kept = apply(twins, params(r, salt())).expect("obfuscate");
        assert_eq!(kept.len(), 2);
        assert_ne!(kept[0].segments()[0][0], kept[1].segments()[0][0]);
    }

    #[test]
    fn positive_radius_without_salt_is_an_error() {
        // I5: no silent fallback to the exact home circle.
        let mut p = params(250.0, salt());
        p.salt = None;
        let err = apply(vec![act(42.94, -85.60)], p).unwrap_err();
        assert!(err.to_string().contains("[privacy].salt"), "got: {err}");
    }

    #[test]
    fn short_salt_rejected_and_debug_redacted() {
        assert!(PrivacySalt::new("too-short").is_err());
        let p = params(250.0, salt());
        let shown = format!("{p:?}");
        assert!(!shown.contains("test-salt-not-a-secret"), "{shown}");
    }

    #[test]
    fn offset_below_floor_is_refused() {
        // A small offset quietly brings back the home-centred circle.
        let ride = activity_from(1, &[to_latlng(5000.0, 0.0), to_latlng(6000.0, 0.0)]);
        for small in [0.0, 50.0, MIN_OFFSET_M - 1.0] {
            let mut p = params(250.0, salt());
            p.offset_m = small;
            let err = apply(vec![ride.clone()], p).unwrap_err().to_string();
            assert!(
                err.contains("[privacy].offset_m") && err.contains("250 m floor"),
                "{err}"
            );
        }
        let mut p = params(250.0, salt());
        p.offset_m = MIN_OFFSET_M;
        assert!(apply(vec![ride.clone()], p).is_ok());
        let mut off = params(0.0, salt());
        off.offset_m = 0.0;
        assert!(apply(vec![ride], off).is_ok(), "radius 0 needs no offset");
    }
}

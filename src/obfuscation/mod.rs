// obfuscation — privacy clipping for activity polylines.
//
// Bundled enforcement: `ObfuscatedActivity` is the only type the
// renderer accepts; raw `Activity` cannot reach the render layer
// without passing through `apply()`. The constructor is the gate.
//
// v0.1: real polyline-circle clipping. Every input polyline is decoded,
// walked segment by segment, and split at the obfuscation circle's
// edge. Inside-circle portions are dropped. The renderer consumes the
// resulting outside-only segment list directly via `segments()`; it
// never sees the raw `summary_polyline` for activities that came
// through `apply()` with a positive radius. Activities whose entire
// trace falls inside the circle are dropped.

use geo::LineString;

use crate::strava::Activity;

#[derive(Debug, Clone, Copy)]
pub struct ObfuscationParams {
    pub home_lat: f64,
    pub home_lng: f64,
    pub radius_m: f64,
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

    /// Outside-circle polyline pieces. Each inner `Vec` is one
    /// continuous run of `(lat, lng)` points that the renderer should
    /// stroke as a single sub-path.
    pub fn segments(&self) -> &[Vec<(f64, f64)>] {
        &self.clipped_segments
    }
}

/// Apply obfuscation to a slice of activities. Each polyline is
/// clipped against the circle of radius `params.radius_m` centered on
/// `(home_lat, home_lng)`; inside-circle portions are removed.
/// Activities whose polyline lies entirely inside the circle (or that
/// have no polyline at all and start within the circle) are dropped.
/// Pass `radius_m = 0.0` to disable clipping; in that case the full
/// decoded polyline is preserved as a single segment.
pub fn apply(
    activities: impl IntoIterator<Item = Activity>,
    params: ObfuscationParams,
) -> Vec<ObfuscatedActivity> {
    activities
        .into_iter()
        .filter_map(|a| build_obfuscated(a, &params))
        .collect()
}

fn build_obfuscated(activity: Activity, params: &ObfuscationParams) -> Option<ObfuscatedActivity> {
    // No polyline: fall back to start-point distance. A polyline-less
    // activity can't be split, so the only honest behavior is to drop
    // it when it starts inside the circle.
    if activity.summary_polyline.is_empty() {
        if params.radius_m > 0.0
            && haversine_m(
                activity.start_lat,
                activity.start_lng,
                params.home_lat,
                params.home_lng,
            ) <= params.radius_m
        {
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

    let segments = if params.radius_m <= 0.0 {
        // Obfuscation disabled: emit the full polyline as one segment.
        let pts: Vec<(f64, f64)> = line.0.iter().map(|c| (c.y, c.x)).collect();
        if pts.len() < 2 {
            return None;
        }
        vec![pts]
    } else {
        clip_to_outside_circle(&line, (params.home_lat, params.home_lng), params.radius_m)
    };

    if segments.is_empty() {
        return None;
    }
    Some(ObfuscatedActivity {
        activity,
        clipped_segments: segments,
    })
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
            },
        );
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
            },
        );
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
            },
        );
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
}

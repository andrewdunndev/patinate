// render::path — compact path-`d` writer for the `--web` preset.
//
// Posters keep the plain absolute `M x y L x y` strings written in
// compose. The web preset spends bytes only on what shows at the
// viewbox's own scale: coordinates quantized to a step sized to the
// viewbox, Douglas-Peucker at a fraction of a unit, no zero-length
// steps, nothing wholly off-canvas, and relative commands.
//
// Both moves shift lines after the privacy clip: a chord across a bend
// cuts inside it, and a quantized vertex can step toward home. Ride
// lines are written against a `KeepOut` disc so neither move takes a
// drawn segment into the home radius.

use std::fmt::Write as _;

use geo::algorithm::SimplifyIdx;
use geo::{Coord, LineString};

/// Margin, in viewbox units, past the edge before a line counts as
/// off-canvas. Wider than any stroke or halo the themes draw.
const CULL_MARGIN: f64 = 10.0;

/// Rings under this area, in square viewbox units, are dropped: a
/// pond smaller than one pixel at the viewbox's own size.
const MIN_RING_AREA: f64 = 1.0;

/// A disc, in viewbox units, that no drawn segment may enter. Input
/// lines already clear it by `slack`; the writer spends at most half
/// of that on simplification and an eighth on quantization.
#[derive(Debug, Clone, Copy)]
pub struct KeepOut {
    pub center: (f64, f64),
    pub radius: f64,
    pub slack: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Compact {
    /// Decimal digits kept per coordinate.
    precision: usize,
    /// Douglas-Peucker tolerance in viewbox units.
    tolerance: f64,
    viewbox_w: f64,
    viewbox_h: f64,
}

impl Compact {
    /// Sized to the viewbox: the coordinate step is at most a ten
    /// thousandth of the long side (0.1 units at 1600), and the
    /// simplification tolerance is five steps, half a unit: half a
    /// pixel at the viewbox's own size, one device pixel shown
    /// full-width at 2x density.
    pub fn for_viewbox(viewbox_w: f64, viewbox_h: f64) -> Self {
        let long = viewbox_w.max(viewbox_h).max(1.0);
        let precision = (10_000.0 / long).log10().ceil().max(0.0) as usize;
        let step = 10f64.powi(-(precision as i32));
        Compact {
            precision,
            tolerance: 5.0 * step,
            viewbox_w,
            viewbox_h,
        }
    }

    /// Append `pts` to `d` as one sub-path. Returns false and writes
    /// nothing when the line is off-canvas or collapses to a point, or
    /// when a ring covers less than `MIN_RING_AREA`.
    pub fn write(&self, d: &mut String, pts: &[(f64, f64)], closed: bool) -> bool {
        self.write_with(d, pts, closed, None)
    }

    /// `write` for an open line whose segments clear `keep` by its
    /// slack: every drawn segment stays out of `keep`. A chord that
    /// would come within half the slack keeps the vertices it skipped,
    /// and a line near the disc is quantized on a finer step.
    pub fn write_clear_of(&self, d: &mut String, pts: &[(f64, f64)], keep: KeepOut) -> bool {
        self.write_with(d, pts, false, Some(keep))
    }

    fn write_with(
        &self,
        d: &mut String,
        pts: &[(f64, f64)],
        closed: bool,
        keep: Option<KeepOut>,
    ) -> bool {
        let min_pts = if closed { 3 } else { 2 };
        if pts.len() < min_pts || self.off_canvas(pts) {
            return false;
        }
        let pts = self.simplify(pts, keep);
        let precision = match keep {
            Some(k) if near(&pts, k, max_snap(self.precision)) => {
                // Smallest step whose snap spends an eighth of the slack.
                let fine = (8.0 * max_snap(0) / k.slack).log10().ceil();
                (fine.clamp(0.0, 9.0) as usize).max(self.precision)
            }
            _ => self.precision,
        };

        // Quantize to integer steps, then drop zero-length steps. A
        // closed ring's repeated first point goes too; `z` closes it.
        let scale = 10f64.powi(precision as i32);
        let mut q: Vec<(i64, i64)> = Vec::with_capacity(pts.len());
        for &(x, y) in &pts {
            let p = ((x * scale).round() as i64, (y * scale).round() as i64);
            if q.last() != Some(&p) {
                q.push(p);
            }
        }
        if closed && q.len() > 1 && q.first() == q.last() {
            q.pop();
        }
        if q.len() < min_pts {
            return false;
        }
        if closed {
            let area2: i64 = q
                .iter()
                .zip(q.iter().cycle().skip(1))
                .map(|(a, b)| a.0 * b.1 - b.0 * a.1)
                .sum();
            if (area2.abs() as f64) / 2.0 < MIN_RING_AREA * scale * scale {
                return false;
            }
        }

        let (x0, y0) = q[0];
        d.push('M');
        Self::push_num(d, x0, false, precision);
        Self::push_num(d, y0, true, precision);
        d.push('l');
        let mut prev = q[0];
        for (i, &(x, y)) in q[1..].iter().enumerate() {
            Self::push_num(d, x - prev.0, i > 0, precision);
            Self::push_num(d, y - prev.1, true, precision);
            prev = (x, y);
        }
        if closed {
            d.push('z');
        }
        true
    }

    /// Douglas-Peucker at the tolerance. Against `keep`, a kept chord
    /// that comes within half the slack is split at the skipped vertex
    /// nearest the centre, down to the input's own segments if need be.
    fn simplify(&self, pts: &[(f64, f64)], keep: Option<KeepOut>) -> Vec<(f64, f64)> {
        if self.tolerance <= 0.0 || pts.len() <= 2 {
            return pts.to_vec();
        }
        let ls: LineString<f64> = pts.iter().map(|&(x, y)| Coord { x, y }).collect();
        let idx = ls.simplify_idx(self.tolerance);
        let mut out = vec![pts[idx[0]]];
        for w in idx.windows(2) {
            push_clear(pts, w[0], w[1], keep, &mut out);
        }
        out
    }

    fn off_canvas(&self, pts: &[(f64, f64)]) -> bool {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in pts {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        x1 < -CULL_MARGIN
            || y1 < -CULL_MARGIN
            || x0 > self.viewbox_w + CULL_MARGIN
            || y0 > self.viewbox_h + CULL_MARGIN
    }

    /// Write a quantized number in its shortest form: no trailing
    /// zeros, no leading `0` before the point, and no separator before
    /// a minus sign.
    fn push_num(d: &mut String, v: i64, sep: bool, precision: usize) {
        let neg = v < 0;
        let mut digits = v.unsigned_abs().to_string();
        if precision > 0 {
            if digits.len() <= precision {
                digits = format!("{}{digits}", "0".repeat(precision + 1 - digits.len()));
            }
            digits.insert(digits.len() - precision, '.');
            let trimmed = digits.trim_end_matches('0').trim_end_matches('.');
            digits = trimmed.strip_prefix('0').unwrap_or(trimmed).to_string();
            if digits.is_empty() || digits == "." {
                digits = "0".to_string();
            }
        }
        if neg {
            d.push('-');
        } else if sep {
            d.push(' ');
        }
        let _ = write!(d, "{digits}");
    }
}

/// The farthest rounding moves a point on a grid of `precision`
/// decimals: half a cell's diagonal.
fn max_snap(precision: usize) -> f64 {
    10f64.powi(-(precision as i32)) * std::f64::consts::FRAC_1_SQRT_2
}

/// Whether rounding by up to `snap` could take a segment of `pts` past
/// the slack's outer edge, toward `keep`.
fn near(pts: &[(f64, f64)], keep: KeepOut, snap: f64) -> bool {
    let reach = keep.radius + keep.slack + snap;
    pts.windows(2)
        .any(|w| seg_dist(w[0], w[1], keep.center) < reach)
}

/// Push `pts[j]` onto `out`, after whatever vertices between `i` and
/// `j` keep each chord half the slack clear of `keep`.
fn push_clear(
    pts: &[(f64, f64)],
    i: usize,
    j: usize,
    keep: Option<KeepOut>,
    out: &mut Vec<(f64, f64)>,
) {
    if let Some(k) = keep {
        if j > i + 1 && seg_dist(pts[i], pts[j], k.center) < k.radius + k.slack / 2.0 {
            let dist = |m: &usize| {
                let p = pts[*m];
                (p.0 - k.center.0).hypot(p.1 - k.center.1)
            };
            let m = (i + 1..j)
                .min_by(|a, b| dist(a).total_cmp(&dist(b)))
                .expect("j > i + 1");
            push_clear(pts, i, m, keep, out);
            push_clear(pts, m, j, keep, out);
            return;
        }
    }
    out.push(pts[j]);
}

/// Distance from `c` to the segment `a`-`b`.
fn seg_dist(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((c.0 - a.0) * dx + (c.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (a.0 + t * dx - c.0).hypot(a.1 + t * dy - c.1)
}

/// Twice the signed area of a ring (shoelace). The sign gives its
/// winding; equal signs let merged rings fill as a union.
pub fn signed_area2(ring: &[(f64, f64)]) -> f64 {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(a, b)| a.0 * b.1 - b.0 * a.1)
        .sum()
}

/// Sub-paths of a `Compact` path as absolute points, for tests that
/// check where lines land.
#[cfg(test)]
pub(crate) fn parse_d(d: &str) -> Vec<Vec<(f64, f64)>> {
    let mut nums: Vec<f64> = Vec::new();
    let mut cmds: Vec<(char, usize)> = Vec::new();
    let mut tok = String::new();
    let flush = |tok: &mut String, nums: &mut Vec<f64>| {
        if !tok.is_empty() {
            nums.push(tok.parse().expect("number"));
            tok.clear();
        }
    };
    for ch in d.chars() {
        match ch {
            'M' | 'l' | 'z' => {
                flush(&mut tok, &mut nums);
                cmds.push((ch, nums.len()));
            }
            ' ' => flush(&mut tok, &mut nums),
            '-' => {
                flush(&mut tok, &mut nums);
                tok.push(ch);
            }
            '.' if tok.contains('.') => {
                flush(&mut tok, &mut nums);
                tok.push(ch);
            }
            _ => tok.push(ch),
        }
    }
    flush(&mut tok, &mut nums);
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    for (i, &(cmd, at)) in cmds.iter().enumerate() {
        let end = cmds.get(i + 1).map_or(nums.len(), |c| c.1);
        let args = &nums[at..end];
        match cmd {
            'M' => out.push(vec![(args[0], args[1])]),
            'l' => {
                let sub = out.last_mut().expect("l after M");
                for step in args.chunks(2) {
                    let &(x, y) = sub.last().expect("point");
                    sub.push((x + step[0], y + step[1]));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt() -> Compact {
        Compact::for_viewbox(1600.0, 900.0)
    }

    #[test]
    fn precision_and_tolerance_track_the_viewbox() {
        assert_eq!(fmt().precision, 1);
        assert_eq!(Compact::for_viewbox(1200.0, 1600.0).precision, 1);
        assert_eq!(Compact::for_viewbox(800.0, 600.0).precision, 2);
        assert_eq!(Compact::for_viewbox(12_000.0, 9_000.0).precision, 0);
        assert!((fmt().tolerance - 0.5).abs() < 1e-9);
    }

    #[test]
    fn writes_relative_shortest_numbers() {
        let mut d = String::new();
        assert!(fmt().write(&mut d, &[(10.04, 20.0), (10.54, 19.0), (12.0, 19.0)], false));
        assert_eq!(d, "M10 20l.5-1 1.5 0");
    }

    #[test]
    fn drops_zero_length_steps_and_collapsed_lines() {
        let mut d = String::new();
        assert!(!fmt().write(&mut d, &[(5.01, 5.0), (5.02, 5.04)], false));
        assert!(d.is_empty());
        assert!(fmt().write(&mut d, &[(5.0, 5.0), (5.01, 5.0), (7.0, 5.0)], false));
        assert_eq!(d, "M5 5l2 0");
    }

    #[test]
    fn culls_off_canvas_lines_only() {
        let mut d = String::new();
        assert!(!fmt().write(&mut d, &[(-50.0, 10.0), (-20.0, 40.0)], false));
        assert!(!fmt().write(&mut d, &[(100.0, 950.0), (300.0, 990.0)], false));
        // Crossing the canvas with both ends outside still draws.
        assert!(fmt().write(&mut d, &[(-50.0, 10.0), (1700.0, 10.0)], false));
    }

    #[test]
    fn simplifies_below_tolerance_only() {
        let mut d = String::new();
        let wiggle = [(0.0, 0.0), (5.0, 0.4), (10.0, 0.0), (15.0, 3.0)];
        fmt().write(&mut d, &wiggle, false);
        assert_eq!(d, "M0 0l10 0 5 3", "the 0.4 wiggle goes, the 3 bend stays");
    }

    #[test]
    fn closes_rings_without_repeating_the_start() {
        let mut d = String::new();
        let ring = [(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 0.0)];
        assert!(fmt().write(&mut d, &ring, true));
        assert_eq!(d, "M0 0l4 0 0 4z");
        assert!(signed_area2(&ring) > 0.0);
    }

    #[test]
    fn drops_sub_pixel_rings() {
        let mut d = String::new();
        let speck = [(0.0, 0.0), (0.9, 0.0), (0.9, 0.9)];
        assert!(!fmt().write(&mut d, &speck, true), "0.4 square units");
        let pond = [(0.0, 0.0), (2.0, 0.0), (2.0, 2.0)];
        assert!(fmt().write(&mut d, &pond, true), "2 square units");
    }

    /// Nearest any drawn segment of `d` comes to `c`.
    fn nearest(d: &str, c: (f64, f64)) -> f64 {
        parse_d(d)
            .iter()
            .flat_map(|sub| sub.windows(2).map(|w| seg_dist(w[0], w[1], c)))
            .fold(f64::MAX, f64::min)
    }

    #[test]
    fn chords_stay_out_of_the_keep_out_disc() {
        // A line circling the disc just outside its slack: plain
        // simplification cuts across the bends and into the disc.
        let keep = KeepOut {
            center: (500.0, 500.0),
            radius: 30.0,
            slack: 0.032,
        };
        let rho = keep.radius + keep.slack;
        let ring: Vec<(f64, f64)> = (0..=720)
            .map(|k| {
                let a = (k as f64 / 2.0).to_radians();
                (500.0 + rho * a.cos(), 500.0 + rho * a.sin())
            })
            .collect();
        let (mut plain, mut kept) = (String::new(), String::new());
        assert!(fmt().write(&mut plain, &ring, false));
        assert!(
            nearest(&plain, keep.center) < keep.radius,
            "plain write cuts in"
        );
        assert!(fmt().write_clear_of(&mut kept, &ring, keep));
        let min = nearest(&kept, keep.center);
        assert!(
            min >= keep.radius,
            "a chord passes {min:.4} from the centre"
        );
    }

    #[test]
    fn lines_near_the_disc_snap_finer() {
        // On the 0.1 grid, y = 89.96 rounds to 90, inside the disc.
        let keep = KeepOut {
            center: (100.0, 100.0),
            radius: 10.01,
            slack: 0.03,
        };
        let line = [(90.0, 89.96), (110.0, 89.96)];
        let (mut plain, mut kept) = (String::new(), String::new());
        assert!(fmt().write(&mut plain, &line, false));
        assert!(
            nearest(&plain, keep.center) < keep.radius,
            "coarse grid steps in"
        );
        assert!(fmt().write_clear_of(&mut kept, &line, keep));
        let min = nearest(&kept, keep.center);
        assert!(
            min >= keep.radius,
            "a segment passes {min:.4} from the centre"
        );
        // Lines away from the disc keep the coarse grid.
        let mut far = String::new();
        assert!(fmt().write_clear_of(&mut far, &[(0.0, 0.04), (20.0, 0.04)], keep));
        assert_eq!(far, "M0 0l20 0");
    }
}

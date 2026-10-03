// render::path — compact path-`d` writer for the `--web` preset.
//
// Posters keep the plain absolute `M x y L x y` strings written in
// compose. The web preset spends bytes only on what shows at the
// viewbox's own scale: coordinates quantized to a step sized to the
// viewbox, Douglas-Peucker at a fraction of a unit, no zero-length
// steps, nothing wholly off-canvas, and relative commands.

use std::fmt::Write as _;

use geo::algorithm::Simplify;
use geo::{Coord, LineString};

/// Margin, in viewbox units, past the edge before a line counts as
/// off-canvas. Wider than any stroke or halo the themes draw.
const CULL_MARGIN: f64 = 10.0;

/// Rings under this area, in square viewbox units, are dropped: a
/// pond smaller than one pixel at the viewbox's own size.
const MIN_RING_AREA: f64 = 1.0;

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
        let min_pts = if closed { 3 } else { 2 };
        if pts.len() < min_pts || self.off_canvas(pts) {
            return false;
        }
        let simplified;
        let pts = if self.tolerance > 0.0 && pts.len() > 2 {
            let ls: LineString<f64> = pts.iter().map(|&(x, y)| Coord { x, y }).collect();
            simplified = ls.simplify(self.tolerance).into_inner();
            simplified.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>()
        } else {
            pts.to_vec()
        };

        // Quantize to integer steps, then drop zero-length steps. A
        // closed ring's repeated first point goes too; `z` closes it.
        let scale = 10f64.powi(self.precision as i32);
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
        self.push_num(d, x0, false);
        self.push_num(d, y0, true);
        d.push('l');
        let mut prev = q[0];
        for (i, &(x, y)) in q[1..].iter().enumerate() {
            self.push_num(d, x - prev.0, i > 0);
            self.push_num(d, y - prev.1, true);
            prev = (x, y);
        }
        if closed {
            d.push('z');
        }
        true
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
    fn push_num(&self, d: &mut String, v: i64, sep: bool) {
        let neg = v < 0;
        let mut digits = v.unsigned_abs().to_string();
        if self.precision > 0 {
            if digits.len() <= self.precision {
                digits = format!("{}{digits}", "0".repeat(self.precision + 1 - digits.len()));
            }
            digits.insert(digits.len() - self.precision, '.');
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

/// Twice the signed area of a ring (shoelace). The sign gives its
/// winding; equal signs let merged rings fill as a union.
pub fn signed_area2(ring: &[(f64, f64)]) -> f64 {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(a, b)| a.0 * b.1 - b.0 * a.1)
        .sum()
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
}

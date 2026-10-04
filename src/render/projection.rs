// render::projection — Web Mercator + viewport mapping.
//
// We project lat/lng into a square Mercator space, then fit the
// caller's bbox into the SVG viewBox. Web Mercator is industry
// standard and matches OSM-derived maps.

use anyhow::{Result, bail};

use crate::osm::LatLon;

/// Web Mercator's `tan(pi/4 + lat/2)` term diverges as `|lat|` approaches
/// 90 degrees. Beyond this cutoff the projection produces unusable output;
/// we bail with a prescriptive error rather than letting the renderer
/// emit infinities.
const MAX_ABS_LAT_DEGREES: f64 = 85.0;

/// How the radius circle's bounding box meets a viewbox of another
/// aspect. `Contain` letterboxes the short axis so the whole circle
/// shows; `Cover` fills the viewbox and crops the long axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Fit {
    #[default]
    Contain,
    Cover,
}

pub struct Projection {
    /// Mercator-projected min x, min y of the bbox we render.
    min_mx: f64,
    min_my: f64,
    /// Scale factor: SVG units per Mercator unit.
    scale: f64,
    /// Cached viewbox-units per real-world meter at the projection's
    /// center latitude. Derived once at fit time so callers (e.g. the
    /// heat-path gap detector) can express thresholds in meters and
    /// translate to SVG units without duplicating the math.
    scale_per_meter: f64,
    /// Viewbox height in SVG units. Cached at fit time so `project()`
    /// can flip Y from Mercator's bottom-up origin to SVG's top-down.
    viewbox_h: f64,
}

impl Projection {
    /// Build a projection that fits a circle of radius `radius_m`
    /// centered on `(center_lat, center_lng)` into a viewBox of
    /// `viewbox_w x viewbox_h` SVG units. Aspect is preserved; `fit`
    /// picks letterboxing (`Contain`) or cropping (`Cover`).
    ///
    /// The frame depends on these public inputs alone, never on
    /// activity geometry or home: rides cluster around home, so a frame
    /// fitted to them would point at it.
    ///
    /// **Known limitation: antimeridian crossing is not handled.**
    /// Centers within `radius_m / (111_320 * cos(lat))` degrees of
    /// `+/-180` longitude will produce a degenerate bbox (the Mercator
    /// `min_mx`/`max_mx` straddle the +/-pi seam without wrapping).
    /// Affected users are vanishingly rare (Fiji, eastern Russia,
    /// the Aleutians); the bbox is not split across the seam.
    pub fn fit_radius(
        center_lat: f64,
        center_lng: f64,
        radius_m: f64,
        viewbox_w: u32,
        viewbox_h: u32,
        fit: Fit,
    ) -> Result<Self> {
        if center_lat.abs() >= MAX_ABS_LAT_DEGREES {
            bail!(
                "center latitude {center_lat} is too close to the poles \
                 for Web Mercator; patinate supports |lat| < {MAX_ABS_LAT_DEGREES}. \
                 Pick a center within the supported band or wait for a \
                 polar projection in a future release."
            );
        }

        // Approximate degrees per meter at this latitude. Small-bbox
        // shortcut: fine for hometown scale.
        let m_per_deg_lat = 111_320.0;
        let m_per_deg_lng = 111_320.0 * center_lat.to_radians().cos();
        let dlat = radius_m / m_per_deg_lat;
        let dlng = radius_m / m_per_deg_lng;

        let min_lat = center_lat - dlat;
        let max_lat = center_lat + dlat;
        let min_lng = center_lng - dlng;
        let max_lng = center_lng + dlng;

        let (min_mx, min_my) = mercator(min_lat, min_lng);
        let (max_mx, max_my) = mercator(max_lat, max_lng);
        let bbox_w = max_mx - min_mx;
        let bbox_h = max_my - min_my;

        let scale_x = viewbox_w as f64 / bbox_w;
        let scale_y = viewbox_h as f64 / bbox_h;
        let scale = match fit {
            Fit::Contain => scale_x.min(scale_y),
            Fit::Cover => scale_x.max(scale_y),
        };

        let used_w = bbox_w * scale;
        let used_h = bbox_h * scale;
        // Negative under `Cover`: the bbox overflows and is cropped.
        let pad_x = (viewbox_w as f64 - used_w) / 2.0;
        let pad_y = (viewbox_h as f64 - used_h) / 2.0;

        // Derive viewbox-units per meter at the center latitude. The
        // bbox spans `2 * dlng` degrees of longitude horizontally; one
        // meter is `1 / m_per_deg_lng` degrees, and one degree maps to
        // `bbox_w * scale / (2 * dlng)` viewbox units after projection.
        let viewbox_units_per_deg_lng = if dlng > 0.0 {
            (bbox_w * scale) / (2.0 * dlng)
        } else {
            0.0
        };
        let scale_per_meter = viewbox_units_per_deg_lng / m_per_deg_lng;

        Ok(Projection {
            min_mx: min_mx - pad_x / scale,
            min_my: min_my - pad_y / scale,
            scale,
            scale_per_meter,
            viewbox_h: viewbox_h as f64,
        })
    }

    /// Viewbox units per real-world meter at the projection's center
    /// latitude. Used to translate meter-denominated thresholds (e.g.
    /// "break the heat path on gaps wider than 600 m") into SVG-unit
    /// values that scale correctly with viewport and radius.
    pub fn scale_per_meter(&self) -> f64 {
        self.scale_per_meter
    }

    /// Project a lat/lng to (svg_x, svg_y). Y is flipped because SVG
    /// origin is top-left while Mercator origin is bottom-left.
    pub fn project(&self, lat: f64, lng: f64) -> (f64, f64) {
        let (mx, my) = mercator(lat, lng);
        let x = (mx - self.min_mx) * self.scale;
        let y = self.viewbox_h - (my - self.min_my) * self.scale;
        (x, y)
    }

    pub fn project_latlon(&self, p: LatLon) -> (f64, f64) {
        self.project(p.lat, p.lon)
    }

    /// Viewbox units per meter at `lat`, on the 6,371 km sphere the
    /// obfuscation pipeline measures its radii on.
    pub fn units_per_meter_at(&self, lat: f64) -> f64 {
        self.scale / (6_371_000.0 * lat.to_radians().cos())
    }

    /// Inverse of `project`.
    #[cfg(test)]
    pub fn unproject(&self, x: f64, y: f64) -> (f64, f64) {
        let mx = x / self.scale + self.min_mx;
        let my = (self.viewbox_h - y) / self.scale + self.min_my;
        let lat = 2.0 * my.exp().atan() - std::f64::consts::FRAC_PI_2;
        (lat.to_degrees(), mx.to_degrees())
    }
}

/// Web Mercator forward projection. Output is in radians-on-a-unit-sphere
/// units; the caller normalizes via the bbox so we skip the radius factor.
fn mercator(lat: f64, lng: f64) -> (f64, f64) {
    let x = lng.to_radians();
    let y = ((std::f64::consts::PI / 4.0 + lat.to_radians() / 2.0).tan()).ln();
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAT: f64 = 42.96;
    const LNG: f64 = -85.67;
    const R: f64 = 25_000.0;

    /// Projected extent of the radius circle's bounding box.
    fn bbox_extent(p: &Projection) -> (f64, f64, f64, f64) {
        let dlat = R / 111_320.0;
        let dlng = R / (111_320.0 * LAT.to_radians().cos());
        let (x0, y0) = p.project(LAT + dlat, LNG - dlng);
        let (x1, y1) = p.project(LAT - dlat, LNG + dlng);
        (x0, y0, x1, y1)
    }

    #[test]
    fn cover_fills_the_viewbox() {
        for (w, h) in [(1600, 900), (1200, 1600), (900, 900)] {
            let p = Projection::fit_radius(LAT, LNG, R, w, h, Fit::Cover).unwrap();
            let (x0, y0, x1, y1) = bbox_extent(&p);
            let (w, h) = (w as f64, h as f64);
            assert!(x0 <= 1e-6 && x1 >= w - 1e-6, "{w}x{h}: x {x0}..{x1}");
            assert!(y0 <= 1e-6 && y1 >= h - 1e-6, "{w}x{h}: y {y0}..{y1}");
            // One axis meets the edges exactly; the other overflows.
            let tight_x = x0.abs() < 1e-6 && (x1 - w).abs() < 1e-6;
            let tight_y = y0.abs() < 1e-6 && (y1 - h).abs() < 1e-6;
            assert!(tight_x || tight_y, "{w}x{h}: no axis fits exactly");
            let (cx, cy) = p.project(LAT, LNG);
            // Mercator stretches y, so the center sits a hair off middle.
            assert!((cx - w / 2.0).abs() < 1e-6 && (cy - h / 2.0).abs() < 0.003 * w.max(h));
        }
    }

    #[test]
    fn contain_letterboxes_inside_the_viewbox() {
        let p = Projection::fit_radius(LAT, LNG, R, 1600, 900, Fit::Contain).unwrap();
        let (x0, y0, x1, y1) = bbox_extent(&p);
        assert!(x0 > 1.0 && x1 < 1599.0, "wide viewbox letterboxes x");
        assert!(y0.abs() < 1e-6 && (y1 - 900.0).abs() < 1e-6);
    }
}

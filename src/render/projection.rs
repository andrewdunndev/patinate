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
    /// `viewbox_w x viewbox_h` SVG units. Aspect is preserved by
    /// letterboxing the shorter axis.
    ///
    /// **Known limitation: antimeridian crossing is not handled.**
    /// Centers within `radius_m / (111_320 * cos(lat))` degrees of
    /// `+/-180` longitude will produce a degenerate bbox (the Mercator
    /// `min_mx`/`max_mx` straddle the +/-pi seam without wrapping).
    /// Affected users are vanishingly rare (Fiji, eastern Russia,
    /// the Aleutians); patinate v0.1 does not split the bbox across
    /// the seam.
    pub fn fit_radius(
        center_lat: f64,
        center_lng: f64,
        radius_m: f64,
        viewbox_w: u32,
        viewbox_h: u32,
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
        let scale = scale_x.min(scale_y);

        let used_w = bbox_w * scale;
        let used_h = bbox_h * scale;
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
}

/// Web Mercator forward projection. Output is in radians-on-a-unit-sphere
/// units; the caller normalizes via the bbox so we skip the radius factor.
fn mercator(lat: f64, lng: f64) -> (f64, f64) {
    let x = lng.to_radians();
    let y = ((std::f64::consts::PI / 4.0 + lat.to_radians() / 2.0).tan()).ln();
    (x, y)
}

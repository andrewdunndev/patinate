// osm::filter — pre-render filters that clean up the road graph.
//
// Three independent passes, applied in order:
//   1. min_way_length — drop ways shorter than a tier-tuned threshold
//      (kills the suburban-stub orphan lines)
//   2. keep_largest_components — keep only ways belonging to the
//      largest N connected components of the road network
//      (tier-aware components fix; see CONTRIBUTING.md)
//   3. simplify — Douglas-Peucker per way, single epsilon
//      (file size + visual noise reduction)
//
// All three operate on `Basemap.roads`; water and parks pass through
// unchanged. Pure functions over the data.

use std::collections::HashMap;

use geo::algorithm::Simplify;
use geo::{Coord, LineString};

#[cfg(test)]
use crate::osm::Road;
use crate::osm::{Basemap, LatLon};
use crate::render::theme::RoadTier;

// ---- min_way_length -------------------------------------------------

/// Drop roads whose total haversine length is below the tier-tuned
/// threshold (in meters). Tuned for a ~25km city poster at 1200×1600.
///
/// Motorway and trunk are never filtered here — they're a small finite
/// set, every segment is meaningful (interchange links, short connector
/// ramps), and dropping any of them creates visible gaps that read as
/// errors against Google/Apple maps where users have a mental model.
pub fn min_way_length(basemap: &mut Basemap) {
    basemap.roads.retain(|r| {
        if is_major(r.tier) {
            return true;
        }
        haversine_length_m(&r.geometry) >= min_meters_for(r.tier)
    });
}

fn min_meters_for(tier: RoadTier) -> f64 {
    match tier {
        // Motorway/trunk: keep nearly everything. Short segments are
        // usually interchange links between ramps; dropping them
        // creates visible gaps at cloverleafs (e.g. I-96 / I-196 in
        // Grand Rapids). 20m floor only weeds out truly degenerate
        // stubs (single-node ways with one duplicated point).
        RoadTier::Motorway => 20.0,
        RoadTier::Trunk => 20.0,
        RoadTier::Primary => 60.0,
        RoadTier::Secondary => 80.0,
        RoadTier::Tertiary => 50.0,
        RoadTier::Residential => 30.0,
    }
}

fn haversine_length_m(geometry: &[LatLon]) -> f64 {
    if geometry.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    for w in geometry.windows(2) {
        total += haversine_m(w[0].lat, w[0].lon, w[1].lat, w[1].lon);
    }
    total
}

fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lng2 - lng1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    R * c
}

// ---- keep_largest_components ----------------------------------------

/// Build the road graph (nodes = unique endpoints, edges = each way),
/// run union-find, and keep only roads belonging to the largest `n`
/// connected components.
///
/// **Tier-aware:** major roads (motorway, trunk, primary) are always
/// kept regardless of component size — they're the city's skeleton and
/// "isolated" major segments are usually divided-highway carriageways
/// that legitimately don't share nodes with their counterpart.
///
/// Endpoint matching uses 1e-7-degree quantized integer keys. Two
/// ways sharing an OSM node almost always produce identical keys, but
/// float-to-int rounding at the LSB boundary can occasionally split
/// a shared node. Documented limitation; in practice the failure mode
/// is invisible because Overpass `out geom` is deterministic per query.
/// Two ways are connected iff their endpoints share a node; interior
/// points are not considered, which matches OSM convention (ways are
/// split at junctions).
pub fn keep_largest_components(basemap: &mut Basemap, n: usize) {
    if basemap.roads.is_empty() || n == 0 {
        return;
    }

    // 1. Map each unique endpoint to a node index. Build the graph
    //    using ALL roads so divided-highway parallel ways still
    //    contribute to component sizing via shared ramp endpoints.
    let mut node_idx: HashMap<(i64, i64), usize> = HashMap::new();
    let id = |p: LatLon, m: &mut HashMap<(i64, i64), usize>| -> usize {
        let key = (
            (p.lat * 1.0e7).round() as i64,
            (p.lon * 1.0e7).round() as i64,
        );
        let next = m.len();
        *m.entry(key).or_insert(next)
    };

    let triples: Vec<(usize, usize, usize)> = basemap
        .roads
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let geom = &r.geometry;
            if geom.len() < 2 {
                return None;
            }
            let start = id(geom[0], &mut node_idx);
            let end = id(*geom.last().unwrap(), &mut node_idx);
            Some((i, start, end))
        })
        .collect();

    let mut uf = UnionFind::new(node_idx.len());
    for &(_, a, b) in &triples {
        uf.union(a, b);
    }

    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for i in 0..node_idx.len() {
        *sizes.entry(uf.find(i)).or_insert(0) += 1;
    }
    let mut ranked: Vec<(usize, usize)> = sizes.into_iter().collect();
    ranked.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    let kept_roots: std::collections::HashSet<usize> =
        ranked.into_iter().take(n).map(|(root, _)| root).collect();

    // 4. Filter roads: keep major-tier roads unconditionally; minor-
    //    tier roads only if their component is in the top N.
    let kept_set: std::collections::HashSet<usize> = triples
        .into_iter()
        .filter_map(|(i, start, _)| {
            let tier = basemap.roads[i].tier;
            if is_major(tier) || kept_roots.contains(&uf.find(start)) {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    let mut idx = 0usize;
    basemap.roads.retain(|_| {
        let keep = kept_set.contains(&idx);
        idx += 1;
        keep
    });
}

fn is_major(tier: RoadTier) -> bool {
    matches!(
        tier,
        RoadTier::Motorway | RoadTier::Trunk | RoadTier::Primary
    )
}

// ---- simplify -------------------------------------------------------

/// Douglas-Peucker each road's geometry. Epsilon is in degrees; ~0.0001
/// (≈ 8m at mid-latitudes) is conservative for poster scale.
pub fn simplify(basemap: &mut Basemap, epsilon_deg: f64) {
    for road in basemap.roads.iter_mut() {
        if road.geometry.len() <= 2 {
            continue;
        }
        let ls: LineString<f64> = road
            .geometry
            .iter()
            .map(|p| Coord { x: p.lon, y: p.lat })
            .collect();
        let simplified = ls.simplify(epsilon_deg);
        road.geometry = simplified
            .into_inner()
            .into_iter()
            .map(|c| LatLon { lat: c.y, lon: c.x })
            .collect();
    }
}

// ---- convenience entrypoint -----------------------------------------

/// Apply all three filters in the recommended order. Defaults tuned for
/// a hometown-scale poster (Grand Rapids, 25km radius, 1200×1600).
pub fn refine(basemap: &mut Basemap) {
    keep_largest_components(basemap, 12);
    min_way_length(basemap);
    simplify(basemap, 0.0001);
}

// ---- union-find -----------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            let r = self.find(self.parent[x]);
            self.parent[x] = r;
        }
        self.parent[x]
    }

    fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return;
        }
        if self.rank[rx] < self.rank[ry] {
            self.parent[rx] = ry;
        } else if self.rank[rx] > self.rank[ry] {
            self.parent[ry] = rx;
        } else {
            self.parent[ry] = rx;
            self.rank[rx] += 1;
        }
    }
}

// ---- tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn road(tier: RoadTier, pts: &[(f64, f64)]) -> Road {
        Road {
            tier,
            geometry: pts.iter().map(|&(lat, lon)| LatLon { lat, lon }).collect(),
        }
    }

    #[test]
    fn min_length_drops_short_residentials() {
        let mut bm = Basemap::default();
        // ~10m segment, residential threshold is 30m
        bm.roads.push(road(
            RoadTier::Residential,
            &[(42.96, -85.66), (42.9601, -85.66)],
        ));
        // ~110m segment, residential threshold is 30m
        bm.roads.push(road(
            RoadTier::Residential,
            &[(42.96, -85.66), (42.961, -85.66)],
        ));
        min_way_length(&mut bm);
        assert_eq!(bm.roads.len(), 1, "expected the longer way to survive");
    }

    #[test]
    fn keep_largest_components_drops_minor_orphans() {
        let mut bm = Basemap::default();
        // Big component: 4 residential ways sharing endpoints.
        let pts = [
            (42.96, -85.66),
            (42.961, -85.66),
            (42.962, -85.66),
            (42.963, -85.66),
            (42.964, -85.66),
        ];
        for w in pts.windows(2) {
            bm.roads.push(road(RoadTier::Residential, &[w[0], w[1]]));
        }
        // Orphan residential: standalone way with no shared endpoints.
        bm.roads.push(road(
            RoadTier::Residential,
            &[(43.10, -85.50), (43.11, -85.50)],
        ));
        keep_largest_components(&mut bm, 1);
        assert_eq!(bm.roads.len(), 4, "minor-tier orphan should be dropped");
    }

    #[test]
    fn keep_largest_components_keeps_major_orphans() {
        let mut bm = Basemap::default();
        // Big component: 4 residential ways forming the main mesh.
        let pts = [
            (42.96, -85.66),
            (42.961, -85.66),
            (42.962, -85.66),
            (42.963, -85.66),
            (42.964, -85.66),
        ];
        for w in pts.windows(2) {
            bm.roads.push(road(RoadTier::Residential, &[w[0], w[1]]));
        }
        // Orphan trunk (e.g. one carriageway of a divided highway).
        bm.roads
            .push(road(RoadTier::Trunk, &[(43.10, -85.50), (43.11, -85.50)]));
        keep_largest_components(&mut bm, 1);
        assert_eq!(
            bm.roads.len(),
            5,
            "major-tier orphan must survive (skeleton-preservation rule)"
        );
    }

    #[test]
    fn simplify_collapses_collinear_intermediate_points() {
        let mut bm = Basemap::default();
        // 3 collinear points: middle one should drop with any sane epsilon.
        bm.roads.push(road(
            RoadTier::Primary,
            &[(42.95, -85.66), (42.955, -85.66), (42.96, -85.66)],
        ));
        simplify(&mut bm, 0.0001);
        assert_eq!(
            bm.roads[0].geometry.len(),
            2,
            "collinear point should collapse"
        );
    }
}

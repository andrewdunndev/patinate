// module: osm — Overpass API JSON parser + tag classifier.
//
// In v0 we read pre-fetched fixture JSON; live Overpass calls are
// out of scope to be a good citizen of a shared community resource.

pub mod filter;
pub mod overpass;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::render::theme::RoadTier;

/// Top-level Overpass response. We only consume `elements`.
#[derive(Debug, Clone, Deserialize)]
pub struct OverpassResponse {
    pub elements: Vec<OverpassElement>,
}

/// One element from an Overpass `out geom` query. We accept relations
/// and nodes (we ignore them) so the deserializer doesn't fail on
/// data we asked for but don't render.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OverpassElement {
    Node(NodeElement),
    Way(WayElement),
    Relation(RelationElement),
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeElement {
    pub id: i64,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WayElement {
    pub id: i64,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    /// Inline geometry from `out geom`. Each entry is { lat, lon }.
    #[serde(default)]
    pub geometry: Vec<LatLon>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelationElement {
    pub id: i64,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    /// Multipolygon relations contain members with inline geometry.
    #[serde(default)]
    pub members: Vec<RelationMember>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelationMember {
    #[serde(rename = "type")]
    pub member_type: String,
    pub role: String,
    #[serde(default)]
    pub geometry: Vec<LatLon>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

/// Classified, render-ready features after tag interpretation.
///
/// Water comes in two flavors: polygons (lakes, ponds — `natural=water`)
/// get filled, lines (rivers, streams — `waterway=*`) get stroked. Same
/// distinction matters less for parks (almost all polygons in OSM).
#[derive(Debug, Default, Clone)]
pub struct Basemap {
    pub roads: Vec<Road>,
    pub water_polygons: Vec<Vec<LatLon>>,
    pub water_lines: Vec<Vec<LatLon>>,
    pub parks: Vec<Vec<LatLon>>,
}

#[derive(Debug, Clone)]
pub struct Road {
    pub tier: RoadTier,
    pub geometry: Vec<LatLon>,
}

pub fn load(path: impl AsRef<Path>) -> Result<Basemap> {
    let path = path.as_ref();
    let text = read_osm_file(path)?;
    let resp: OverpassResponse = serde_json::from_str(&text)
        .with_context(|| format!("OSM file {path:?} is not valid Overpass JSON"))?;
    Ok(classify(&resp))
}

/// Read an OSM fixture as a UTF-8 string, transparently decompressing
/// gzip if the file's first two bytes are `1f 8b`. We sniff bytes
/// rather than rely on `.gz` suffix so both `basemap.osm.json` and
/// `basemap.osm.json.gz` round-trip through the same code path.
fn read_osm_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "could not read OSM fixture {path:?}. \
             Run `patinate fetch-osm` to populate the per-user cache, \
             or pass `--osm <path>` to point at an existing Overpass JSON."
        )
    })?;
    let is_gzip = bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b;
    if !is_gzip {
        return String::from_utf8(bytes).with_context(|| {
            format!("OSM file {path:?} is not valid UTF-8 (and not a gzip stream)")
        });
    }
    let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut out = String::new();
    decoder.read_to_string(&mut out).with_context(|| {
        format!("OSM file {path:?} looked like gzip but failed to decode. Re-fetch the fixture.")
    })?;
    Ok(out)
}

pub fn classify(resp: &OverpassResponse) -> Basemap {
    let mut bm = Basemap::default();
    for el in &resp.elements {
        match el {
            OverpassElement::Way(way) => classify_way(way, &mut bm),
            OverpassElement::Relation(rel) => classify_relation(rel, &mut bm),
            OverpassElement::Node(_) => {}
        }
    }
    bm
}

fn classify_way(way: &WayElement, bm: &mut Basemap) {
    if way.geometry.is_empty() {
        return;
    }
    if let Some(highway) = way.tags.get("highway") {
        if let Some(tier) = RoadTier::from_osm_highway(highway) {
            bm.roads.push(Road {
                tier,
                geometry: way.geometry.clone(),
            });
            return;
        }
    }
    if way.tags.get("natural").map(|s| s.as_str()) == Some("water") {
        bm.water_polygons.push(way.geometry.clone());
        return;
    }
    if way.tags.contains_key("waterway") {
        bm.water_lines.push(way.geometry.clone());
        return;
    }
    if matches!(
        way.tags.get("leisure").map(|s| s.as_str()),
        Some("park" | "garden")
    ) || way.tags.get("landuse").map(|s| s.as_str()) == Some("grass")
        || way.tags.get("boundary").map(|s| s.as_str()) == Some("national_park")
    {
        bm.parks.push(way.geometry.clone());
    }
}

fn classify_relation(rel: &RelationElement, bm: &mut Basemap) {
    let is_water = rel.tags.get("natural").map(|s| s.as_str()) == Some("water");
    if !is_water {
        return;
    }
    for m in &rel.members {
        if !m.geometry.is_empty() && (m.role == "outer" || m.role.is_empty()) {
            bm.water_polygons.push(m.geometry.clone());
        }
    }
}

//! Generate fully synthetic demo activities for fixtures/activities.json.
//!
//! Each activity is a seeded random walk over the OSM fixture's road
//! graph (motorways and trunks excluded). Starts are stratified over a
//! grid covering the map so they spread across the whole city; the walk
//! keeps a heading with random turns, so ends land far from starts.
//!
//!     cargo run --example synth_activities -- \
//!         fixtures/grand-rapids.osm.json.gz > fixtures/activities.json

use std::collections::HashMap;

use chrono::{Duration, TimeZone, Utc};
use geo::{LineString, Simplify};
use patinate::render::theme::RoadTier;
use patinate::strava::{Activity, ActivityType};

const SEED: u64 = 0x7061_7469_6e61_7465; // "patinate"
const COUNT: usize = 320;
const CENTER: (f64, f64) = (42.9634, -85.6681);
/// Keep walks inside the render area, with margin under radius_m.
const MAX_FROM_CENTER_M: f64 = 18_000.0;
const GRID: usize = 8;

/// splitmix64: tiny, deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Local equirectangular metres from the map center.
fn to_xy(lat: f64, lon: f64) -> (f64, f64) {
    let k = 111_320.0;
    (
        (lon - CENTER.1) * k * CENTER.0.to_radians().cos(),
        (lat - CENTER.0) * k,
    )
}

struct Graph {
    pts: Vec<(f64, f64)>, // (lat, lon)
    xy: Vec<(f64, f64)>,
    adj: Vec<Vec<usize>>,
}

fn build_graph(basemap: &patinate::osm::Basemap) -> Graph {
    let mut index: HashMap<(i64, i64), usize> = HashMap::new();
    let mut g = Graph {
        pts: Vec::new(),
        xy: Vec::new(),
        adj: Vec::new(),
    };
    for road in &basemap.roads {
        if matches!(road.tier, RoadTier::Motorway | RoadTier::Trunk) {
            continue;
        }
        let mut prev: Option<usize> = None;
        for p in &road.geometry {
            let xy = to_xy(p.lat, p.lon);
            if xy.0.hypot(xy.1) > MAX_FROM_CENTER_M {
                prev = None;
                continue;
            }
            let key = ((p.lat * 1e7).round() as i64, (p.lon * 1e7).round() as i64);
            let id = *index.entry(key).or_insert_with(|| {
                g.pts.push((p.lat, p.lon));
                g.xy.push(xy);
                g.adj.push(Vec::new());
                g.pts.len() - 1
            });
            if let Some(q) = prev
                && q != id
                && !g.adj[q].contains(&id)
            {
                g.adj[q].push(id);
                g.adj[id].push(q);
            }
            prev = Some(id);
        }
    }
    g
}

/// Nodes of the largest connected component, so every walk can roam.
fn largest_component(g: &Graph) -> Vec<usize> {
    let mut comp = vec![usize::MAX; g.pts.len()];
    let mut best: Vec<usize> = Vec::new();
    for s in 0..g.pts.len() {
        if comp[s] != usize::MAX {
            continue;
        }
        let mut members = vec![s];
        comp[s] = s;
        let mut i = 0;
        while i < members.len() {
            for &n in &g.adj[members[i]] {
                if comp[n] == usize::MAX {
                    comp[n] = s;
                    members.push(n);
                }
            }
            i += 1;
        }
        if members.len() > best.len() {
            best = members;
        }
    }
    best
}

fn dist(g: &Graph, a: usize, b: usize) -> f64 {
    let (ax, ay) = g.xy[a];
    let (bx, by) = g.xy[b];
    (bx - ax).hypot(by - ay)
}

/// Heading-persistent random walk until `target_m` is covered.
fn walk(g: &Graph, rng: &mut Rng, start: usize, target_m: f64) -> (Vec<usize>, f64) {
    let mut path = vec![start];
    let mut covered = 0.0;
    let mut prev: Option<usize> = None;
    let mut cur = start;
    let mut heading = rng.range(0.0, std::f64::consts::TAU);
    while covered < target_m && path.len() < 20_000 {
        let options: Vec<usize> = g.adj[cur]
            .iter()
            .copied()
            .filter(|&n| Some(n) != prev || g.adj[cur].len() == 1)
            .collect();
        let weights: Vec<f64> = options
            .iter()
            .map(|&n| {
                let (dx, dy) = (g.xy[n].0 - g.xy[cur].0, g.xy[n].1 - g.xy[cur].1);
                (2.5 * (dy.atan2(dx) - heading).cos()).exp()
            })
            .collect();
        let mut pick = rng.unit() * weights.iter().sum::<f64>();
        let mut next = options[options.len() - 1];
        for (&n, &w) in options.iter().zip(&weights) {
            if pick < w {
                next = n;
                break;
            }
            pick -= w;
        }
        let (dx, dy) = (g.xy[next].0 - g.xy[cur].0, g.xy[next].1 - g.xy[cur].1);
        heading = dy.atan2(dx) + rng.range(-0.15, 0.15);
        covered += dist(g, cur, next);
        prev = Some(cur);
        cur = next;
        path.push(cur);
    }
    (path, covered)
}

fn main() -> anyhow::Result<()> {
    let osm = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fixtures/grand-rapids.osm.json.gz".into());
    let basemap = patinate::osm::load(&osm)?;
    let g = build_graph(&basemap);
    let nodes = largest_component(&g);

    // Stratify starts over a GRID x GRID raster of the map so they
    // cover every part of the city instead of following road density.
    let cell = 2.0 * MAX_FROM_CENTER_M / GRID as f64;
    let mut cells: Vec<Vec<usize>> = vec![Vec::new(); GRID * GRID];
    for &n in &nodes {
        let (x, y) = g.xy[n];
        let cx = (((x + MAX_FROM_CENTER_M) / cell) as usize).min(GRID - 1);
        let cy = (((y + MAX_FROM_CENTER_M) / cell) as usize).min(GRID - 1);
        cells[cy * GRID + cx].push(n);
    }
    let cells: Vec<Vec<usize>> = cells.into_iter().filter(|c| c.len() > 50).collect();

    // (type, share, km range, km/h range, gear choices)
    let mix: [(ActivityType, f64, (f64, f64), (f64, f64), &[&str]); 5] = [
        (
            ActivityType::Ride,
            0.35,
            (5.0, 40.0),
            (20.0, 28.0),
            &["g_demo_road", "g_demo_gravel", "g_demo_hybrid"],
        ),
        (
            ActivityType::Run,
            0.25,
            (2.0, 16.0),
            (9.0, 13.0),
            &["g_demo_running"],
        ),
        (ActivityType::Walk, 0.18, (1.0, 6.0), (4.5, 5.8), &[]),
        (
            ActivityType::EBikeRide,
            0.12,
            (4.0, 35.0),
            (22.0, 30.0),
            &["g_demo_ebike"],
        ),
        (ActivityType::Hike, 0.10, (3.0, 14.0), (3.5, 4.8), &[]),
    ];

    let mut rng = Rng(SEED);
    let epoch = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let mut out = Vec::with_capacity(COUNT);
    let mut order: Vec<usize> = (0..cells.len()).collect();
    for i in 0..COUNT {
        if i % cells.len() == 0 {
            for j in (1..order.len()).rev() {
                order.swap(j, rng.below(j + 1));
            }
        }
        let bucket = &cells[order[i % cells.len()]];
        let start = bucket[rng.below(bucket.len())];

        let mut roll = rng.unit();
        let mut kind = &mix[0];
        for m in &mix {
            if roll < m.1 {
                kind = m;
                break;
            }
            roll -= m.1;
        }
        let (atype, _, km, kmh, gear) = kind;
        let target_m = rng.range(km.0, km.1) * 1000.0;
        let (path, covered) = walk(&g, &mut rng, start, target_m);
        let line: LineString<f64> = path
            .iter()
            .map(|&n| geo::coord! { x: g.pts[n].1, y: g.pts[n].0 })
            .collect();
        let line = line.simplify(1e-5);
        let speed = rng.range(kmh.0, kmh.1) / 3.6;
        let when = epoch
            + Duration::days((i * 731 / COUNT) as i64)
            + Duration::minutes(rng.range(6.0 * 60.0, 19.0 * 60.0) as i64);
        let first = g.pts[start];
        out.push(Activity {
            id: 9_000_000_000 + i as i64,
            name: format!("demo activity {}", i + 1),
            activity_type: *atype,
            start_date: when,
            start_lat: (first.0 * 1e6).round() / 1e6,
            start_lng: (first.1 * 1e6).round() / 1e6,
            distance_m: (covered * 10.0).round() / 10.0,
            moving_time_s: (covered / speed).round() as i64,
            summary_polyline: polyline::encode_coordinates(line, 5)
                .map_err(|e| anyhow::anyhow!(e))?,
            athlete_id: 0,
            gear_id: (!gear.is_empty()).then(|| gear[rng.below(gear.len())].to_string()),
        });
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

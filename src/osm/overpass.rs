//! osm::overpass — fetch OSM basemap data from the Overpass API.
//!
//! v0 fixture-based workflow seeded `fixtures/grand-rapids.osm.json`
//! by hand. To support cities beyond Grand Rapids, this module wraps
//! that one-off into a real subcommand: build the same Overpass QL
//! query around a (lat, lng, radius), POST it to the public Overpass
//! endpoint, and stream the JSON response to disk.
//!
//! Optional Nominatim geocoding lets the user pass `--city "Detroit"`
//! instead of explicit coords. Both Overpass and Nominatim are shared
//! community resources, so we send a descriptive User-Agent, never
//! retry aggressively, and bail with a prescriptive error on 429/5xx.
//!
//! The `(_link)?` regex on the highway filter is load-bearing: without
//! it, every interchange in the rendered map looks broken (motorway
//! ramps are tagged `motorway_link`, etc.). Don't drop it.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

/// Hard cap on Overpass response size. Public Overpass instances
/// already cap server-side, but a malicious or misconfigured private
/// mirror could blast unbounded bytes into our buffer. 200 MB is
/// generous for a metro-scale `out geom` payload while still keeping
/// us out of swap on a small machine.
const MAX_OVERPASS_BYTES: usize = 200 * 1024 * 1024;

/// Default public Overpass endpoint. Configurable via CLI / env so
/// users on a bigger budget can point at a private mirror.
pub const DEFAULT_OVERPASS_ENDPOINT: &str = "https://overpass-api.de/api/interpreter";

/// Public Nominatim endpoint for `--city` geocoding. Same caveat as
/// Overpass: shared community resource; one-shot only.
pub const NOMINATIM_ENDPOINT: &str = "https://nominatim.openstreetmap.org/search";

/// User-Agent string sent on every outbound request. Both Overpass
/// and Nominatim ask for a descriptive UA pointing at the source.
pub const USER_AGENT: &str = "patinate/0.1.0 (https://gitlab.com/dunn.dev/patinate)";

/// All inputs needed to fetch + write a basemap fixture.
#[derive(Debug, Clone)]
pub struct FetchParams {
    pub center_lat: f64,
    pub center_lng: f64,
    pub radius_m: u32,
    pub out: PathBuf,
    pub endpoint: String,
}

/// Build the Overpass QL query that recreates the overnight bring-up
/// fixture at a given (lat, lng, radius). The shape of this string is
/// part of the contract: a unit test asserts the `around` clause and
/// the `(_link)?` highway suffix.
pub fn build_query(center_lat: f64, center_lng: f64, radius_m: u32) -> String {
    format!(
        "[out:json][timeout:120];\n\
         (\n\
         \x20\x20way(around:{r},{lat},{lng})[\"highway\"~\"^(motorway|trunk|primary|secondary|tertiary|residential)(_link)?$\"];\n\
         \x20\x20way(around:{r},{lat},{lng})[\"natural\"=\"water\"];\n\
         \x20\x20way(around:{r},{lat},{lng})[\"waterway\"~\"^(river|stream|canal)$\"];\n\
         \x20\x20relation(around:{r},{lat},{lng})[\"natural\"=\"water\"];\n\
         \x20\x20way(around:{r},{lat},{lng})[\"leisure\"~\"^(park|garden)$\"];\n\
         \x20\x20way(around:{r},{lat},{lng})[\"landuse\"=\"grass\"];\n\
         );\n\
         out geom;\n",
        r = radius_m,
        lat = center_lat,
        lng = center_lng,
    )
}

/// Geocode a free-form city/place query against Nominatim and return
/// the first result's (lat, lng).
///
/// Nominatim policy: descriptive User-Agent, low request volume, no
/// retry storms. We send one request and bail on the first failure.
pub async fn geocode_city(client: &reqwest::Client, query: &str) -> Result<(f64, f64)> {
    let resp = client
        .get(NOMINATIM_ENDPOINT)
        .query(&[("q", query), ("format", "json"), ("limit", "1")])
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach Nominatim at {NOMINATIM_ENDPOINT}. \
                 Check your network, or pass explicit --center-lat/--center-lng \
                 to skip geocoding."
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        bail!(
            "Nominatim returned HTTP {status} for query {query:?}. \
             It's a shared community resource — wait a few minutes and retry, \
             or pass explicit --center-lat/--center-lng."
        );
    }

    let body = resp
        .text()
        .await
        .context("could not read Nominatim response body")?;

    parse_nominatim_response(&body, query)
}

/// Parse a Nominatim search response and pull the first (lat, lng)
/// out of it. Nominatim returns lat/lon as JSON strings, not numbers.
pub fn parse_nominatim_response(body: &str, query: &str) -> Result<(f64, f64)> {
    #[derive(Deserialize)]
    struct Hit {
        lat: String,
        lon: String,
    }

    let hits: Vec<Hit> = serde_json::from_str(body)
        .with_context(|| format!("Nominatim response for {query:?} is not valid JSON"))?;

    let first = hits.into_iter().next().ok_or_else(|| {
        anyhow!(
            "Nominatim returned zero results for {query:?}. \
             Try a more specific query (e.g. \"Grand Rapids, USA\"), \
             or pass explicit --center-lat/--center-lng."
        )
    })?;

    let lat: f64 = first
        .lat
        .parse()
        .with_context(|| format!("Nominatim lat field {:?} is not a number", first.lat))?;
    let lng: f64 = first
        .lon
        .parse()
        .with_context(|| format!("Nominatim lon field {:?} is not a number", first.lon))?;
    Ok((lat, lng))
}

/// Diagnostic counters printed after a successful fetch. Mirrors the
/// shape we eyeballed during the overnight bring-up.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchSummary {
    pub bytes: u64,
    pub elements: usize,
    pub highway_ways: usize,
    pub waterway_ways: usize,
    pub park_ways: usize,
}

/// POST the Overpass query, validate the response is a real Overpass
/// JSON document (not an HTML error page), write it to `params.out`,
/// and return diagnostic counts.
pub async fn fetch(client: &reqwest::Client, params: &FetchParams) -> Result<FetchSummary> {
    let query = build_query(params.center_lat, params.center_lng, params.radius_m);

    // Reject non-https endpoints up front. Overpass requests carry
    // exact lat/lng in the query body; cleartext exposes location
    // intent to any MITM listener on the path.
    let parsed_endpoint = Url::parse(&params.endpoint).with_context(|| {
        format!(
            "PATINATE_OVERPASS_ENDPOINT is not a valid URL: {:?}",
            params.endpoint
        )
    })?;
    if parsed_endpoint.scheme() != "https" {
        bail!(
            "PATINATE_OVERPASS_ENDPOINT must use https:// to prevent leaking lat/lng to MITM listeners. Got: {scheme}",
            scheme = parsed_endpoint.scheme()
        );
    }

    let mut resp = client
        .post(&params.endpoint)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .form(&[("data", query.as_str())])
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach Overpass at {endpoint}. \
                 Check your network, or override with --endpoint / \
                 PATINATE_OVERPASS_ENDPOINT.",
                endpoint = params.endpoint
            )
        })?;

    let status = resp.status();
    if status.as_u16() == 429 || status.is_server_error() {
        bail!(
            "Overpass returned HTTP {status}. It's a shared community resource — \
             wait a few minutes and retry. Do not loop on this; aggressive retries \
             are how endpoints get rate-limited for everyone."
        );
    }
    if !status.is_success() {
        bail!(
            "Overpass returned HTTP {status} for the query at \
             ({lat},{lng}) r={r}m. The query may be malformed; \
             check the build_query template.",
            lat = params.center_lat,
            lng = params.center_lng,
            r = params.radius_m,
        );
    }

    // Stream the response in chunks and bail if it would exceed the
    // 200 MB cap. Avoids `resp.bytes()` which would buffer unbounded
    // and double the peak memory by holding the original `Bytes`
    // alongside any later copy.
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .context("could not read Overpass response body")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_OVERPASS_BYTES {
            bail!("Overpass response exceeded 200 MB; reduce --radius_m or use a private mirror");
        }
        body.extend_from_slice(&chunk);
    }

    let summary = inspect_response(&body).with_context(|| {
        format!(
            "Overpass response is not a valid JSON document with an `elements` \
             array. The endpoint may have returned an HTML error page. \
             First 200 bytes: {:?}",
            String::from_utf8_lossy(&body[..body.len().min(200)])
        )
    })?;

    if let Some(parent) = params.out.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("could not create parent directory {:?}", parent))?;
        }
    }
    // Atomic write: tmp + rename. The OSM payload is multi-MB; a kill
    // mid-write would leave a truncated file that the loader's gzip
    // sniffer would mis-classify or that serde would parse as invalid
    // JSON. POSIX rename is atomic on the same filesystem.
    let tmp = params.out.with_extension("json.tmp");
    tokio::fs::write(&tmp, &body)
        .await
        .with_context(|| format!("could not write OSM JSON to {tmp:?}"))?;
    tokio::fs::rename(&tmp, &params.out)
        .await
        .with_context(|| {
            format!(
                "could not rename {tmp:?} to {:?}. Filesystems must match.",
                params.out
            )
        })?;

    Ok(FetchSummary {
        bytes: body.len() as u64,
        ..summary
    })
}

/// Parse the response just enough to confirm it's Overpass JSON and
/// to count classified elements for the diagnostic line. Tags map 1:1
/// to the query above; if the query changes, update these tag checks.
fn inspect_response(body: &[u8]) -> Result<FetchSummary> {
    let parsed: super::OverpassResponse =
        serde_json::from_slice(body).context("response did not deserialize as OverpassResponse")?;

    let mut s = FetchSummary {
        elements: parsed.elements.len(),
        ..FetchSummary::default()
    };
    for el in &parsed.elements {
        if let super::OverpassElement::Way(w) = el {
            if w.tags.contains_key("highway") {
                s.highway_ways += 1;
            }
            if w.tags.contains_key("waterway") {
                s.waterway_ways += 1;
            }
            if matches!(
                w.tags.get("leisure").map(|s| s.as_str()),
                Some("park" | "garden")
            ) {
                s.park_ways += 1;
            }
        }
    }
    Ok(s)
}

/// Build a reqwest client suitable for polite Overpass / Nominatim
/// usage: explicit timeout, no aggressive retry behavior. Kept as a
/// helper so the CLI can share the same client across geocode + fetch.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .user_agent(USER_AGENT)
        .build()
        .context("could not build reqwest client for Overpass")
}

/// Top-level orchestration used by the CLI dispatcher. Resolves
/// (city → lat/lng) if needed, fetches, writes, and prints the
/// diagnostic summary line.
pub async fn run(
    city: Option<String>,
    explicit_lat: Option<f64>,
    explicit_lng: Option<f64>,
    radius_m: u32,
    out: PathBuf,
    endpoint: String,
) -> Result<FetchSummary> {
    let client = http_client()?;

    // Coords win when both are set (explicit). City-only triggers a
    // geocode. Neither = bail with prescriptive guidance.
    let (lat, lng) = match (explicit_lat, explicit_lng, city.as_deref()) {
        (Some(la), Some(ln), _) => (la, ln),
        (Some(_), None, _) | (None, Some(_), _) => bail!(
            "--center-lat and --center-lng must be passed together. \
             Pass both, or use --city to geocode by name."
        ),
        (None, None, Some(name)) => {
            tracing::info!(query = %name, "geocoding city via Nominatim");
            geocode_city(&client, name).await?
        }
        (None, None, None) => bail!(
            "no center provided. Pass either --city \"Grand Rapids, USA\" \
             or --center-lat/--center-lng explicitly."
        ),
    };

    tracing::info!(
        center_lat = lat,
        center_lng = lng,
        radius_m,
        endpoint = %endpoint,
        out = %out.display(),
        "fetching OSM basemap from Overpass"
    );

    let params = FetchParams {
        center_lat: lat,
        center_lng: lng,
        radius_m,
        out: out.clone(),
        endpoint,
    };
    let summary = fetch(&client, &params).await?;

    println!(
        "wrote {bytes} bytes to {out}\n\
         elements={el} highway_ways={hw} waterway_ways={ww} park_ways={pw}",
        bytes = summary.bytes,
        out = out.display(),
        el = summary.elements,
        hw = summary.highway_ways,
        ww = summary.waterway_ways,
        pw = summary.park_ways,
    );

    Ok(summary)
}

/// Helper used by the CLI: resolve the effective `out` path. Kept
/// here so the CLI module doesn't need to know our defaults.
pub fn default_out_path() -> PathBuf {
    Path::new("fixtures").join("basemap.osm.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_query_contains_around_clause_and_link_suffix() {
        let q = build_query(42.9634, -85.6681, 25_000);

        // around clause for the requested radius + center
        assert!(
            q.contains("around:25000,42.9634,-85.6681"),
            "expected `around:25000,42.9634,-85.6681` in query, got: {q}"
        );

        // load-bearing `(_link)?` on the highway regex — without it,
        // motorway/trunk ramps render as broken interchanges.
        assert!(
            q.contains(
                r#"highway"~"^(motorway|trunk|primary|secondary|tertiary|residential)(_link)?$"#
            ),
            "expected `(_link)?` highway suffix in query, got: {q}"
        );

        // out geom is the format the parser expects
        assert!(q.contains("out geom;"), "expected `out geom;` in query");

        // timeout 120 — be a polite citizen
        assert!(q.contains("timeout:120"), "expected `timeout:120` in query");

        // water + parks + relations all present (regression guard for
        // the classification path).
        assert!(q.contains(r#"natural"="water"#));
        assert!(q.contains("relation(around"));
        assert!(q.contains(r#"leisure"~"^(park|garden)$"#));
    }

    #[test]
    fn parse_nominatim_response_pulls_first_hit() {
        // Trimmed shape of a real Nominatim search response. Lat/lon
        // arrive as strings; the parser must coerce to f64.
        let body = r#"[
            {
                "place_id": 12345,
                "lat": "42.9633956",
                "lon": "-85.6680863",
                "display_name": "Grand Rapids, Kent County, Michigan, United States",
                "class": "place",
                "type": "city"
            },
            {
                "place_id": 67890,
                "lat": "0.0",
                "lon": "0.0",
                "display_name": "ignored — only the first hit is used"
            }
        ]"#;

        let (lat, lng) = parse_nominatim_response(body, "Grand Rapids").unwrap();
        assert!((lat - 42.9633956).abs() < 1e-9);
        assert!((lng - -85.6680863).abs() < 1e-9);
    }

    #[test]
    fn parse_nominatim_response_empty_array_errors_with_guidance() {
        let err = parse_nominatim_response("[]", "Atlantis").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero results"),
            "expected prescriptive error, got: {msg}"
        );
    }

    #[test]
    fn parse_nominatim_response_rejects_non_numeric_lat() {
        let body = r#"[{"lat": "not-a-number", "lon": "0.0"}]"#;
        let err = parse_nominatim_response(body, "broken").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a number"), "got: {msg}");
    }
}

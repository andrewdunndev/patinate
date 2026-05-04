// strava::client — reqwest-based HTTP client with rate limiting.
//
// Strava enforces two budgets per developer app:
//   * 100 requests / 15 minutes
//   * 1000 requests / day
//
// Each response carries `X-RateLimit-Usage` and `X-RateLimit-Limit`
// headers (both formatted as `15min,daily`). We parse them and back
// off for 60s if we're within 5 requests of the 15-minute ceiling.
// That's coarse but safe — a poster-grade sync rarely needs more
// than a few pages.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::Duration;

use crate::strava::{Activity, ActivityType};

const STRAVA_API_BASE: &str = "https://www.strava.com/api/v3";
const PER_PAGE: u32 = 200;
/// If our 15-min usage gets within this many requests of the limit,
/// sleep until the window rolls over.
const RATE_LIMIT_HEADROOM: u32 = 5;
/// Sleep this long when we hit the headroom threshold.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
/// Cap on `Retry-After` we are willing to honor automatically. Beyond
/// this we surface the 429 to the operator instead of blocking.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

pub struct StravaClient {
    http: reqwest::Client,
    access_token: String,
}

impl StravaClient {
    /// Construct a client bound to one access token. Refresh handling
    /// lives in `strava::auth`; this type assumes the token is fresh.
    ///
    /// Bubbles up reqwest builder failures (in practice, broken TLS
    /// roots) so the operator gets a prescriptive message instead of
    /// an opaque panic.
    pub fn new(access_token: String) -> Result<Self> {
        let http = reqwest::Client::builder().build().context(
            "could not build reqwest client for the Strava API. \
             This usually means the system TLS root store is broken; \
             reinstall ca-certificates (or the platform equivalent) and retry.",
        )?;
        Ok(Self { http, access_token })
    }

    /// Page through `/athlete/activities`, returning every activity
    /// with `start_date > after` (Unix seconds). Pages of 200 are
    /// requested until Strava returns an empty page. Honors the
    /// 15-minute rate limit; see module docs.
    pub async fn list_activities(&self, after: Option<i64>) -> Result<Vec<Activity>> {
        // Belt-and-braces upper bound on pagination. The loop normally
        // terminates on an empty page; this guard catches the case where
        // a server-side cursor bug returns a full page forever. 500
        // pages at 200 activities each is 100k activities, well above
        // any legitimate Strava history (the most prolific public
        // profiles top out around 30k).
        const MAX_PAGES: u32 = 500;

        let mut out: Vec<Activity> = Vec::new();
        let mut page: u32 = 1;
        loop {
            if page > MAX_PAGES {
                bail!(
                    "paged through {MAX_PAGES} pages of /athlete/activities \
                     without an empty page. Strava's cursor is likely broken: \
                     stop the sync and open an issue with the response shape."
                );
            }
            let resp = self.fetch_page(page, after).await?;

            let status = resp.status();
            // Pull rate-limit hints before consuming the body.
            let usage = parse_rate_limit_pair(resp.headers().get("x-ratelimit-usage"));
            let limit = parse_rate_limit_pair(resp.headers().get("x-ratelimit-limit"));

            // 429 honors `Retry-After` (in seconds) up to a 60s cap and
            // retries once. Strava's documented limits are 100/15min and
            // 1000/day, and most 429s arrive before the 15-minute
            // window has rolled over. A bounded retry recovers from a
            // single bursty page without making the operator wait the
            // full 15 minutes; if Strava 429s again, we surface the
            // original "wait 15 minutes" message.
            let resp = if status.as_u16() == 429 {
                if let Some(secs) = parse_retry_after(resp.headers().get("retry-after")) {
                    let wait = secs.min(RETRY_AFTER_CAP);
                    tracing::warn!(
                        retry_after_s = wait.as_secs(),
                        "Strava returned 429; honoring Retry-After and retrying once"
                    );
                    tokio::time::sleep(wait).await;
                    drop(resp);
                    self.fetch_page(page, after).await?
                } else {
                    resp
                }
            } else {
                resp
            };

            let status = resp.status();

            if !status.is_success() {
                // `text()` may fail (mid-stream truncation, decoding
                // error). Bubble that case rather than swallowing it
                // with `unwrap_or_default()`: a silently empty body on
                // a non-2xx response would make every error look like
                // "Strava returned HTTP 500: " with no signal.
                let body = resp.text().await.with_context(|| {
                    format!(
                        "Strava returned HTTP {status} but the response body \
                         could not be read. Re-run; if it persists, check \
                         network stability."
                    )
                })?;
                if status.as_u16() == 401 {
                    bail!(
                        "Strava returned 401 Unauthorized. \
                         The access token has expired or been revoked: \
                         re-run `patinate sync` to refresh, or `patinate auth` \
                         to re-issue. Body: {body}"
                    );
                }
                if status.as_u16() == 429 {
                    bail!(
                        "Strava returned 429 Too Many Requests. \
                         Wait 15 minutes for the rate-limit window to roll over, \
                         then retry. Body: {body}"
                    );
                }
                bail!("Strava returned HTTP {status}: {body}");
            }

            let page_activities: Vec<WireActivity> = resp
                .json()
                .await
                .with_context(|| format!("could not decode activities JSON from page {page}"))?;
            if page_activities.is_empty() {
                break;
            }
            for w in page_activities {
                out.push(w.into_activity());
            }

            // Back off if we're close to the 15-minute ceiling.
            if let (Some((used15, _)), Some((limit15, _))) = (usage, limit) {
                if limit15 > 0 && used15 + RATE_LIMIT_HEADROOM >= limit15 {
                    tracing::warn!(
                        used = used15,
                        limit = limit15,
                        "approaching Strava 15-min rate limit; sleeping {:?}",
                        RATE_LIMIT_BACKOFF
                    );
                    tokio::time::sleep(RATE_LIMIT_BACKOFF).await;
                }
            }

            page += 1;
        }
        Ok(out)
    }

    /// Issue one GET to `/athlete/activities` for the given page. Used
    /// by the main loop and by the 429 retry path.
    async fn fetch_page(&self, page: u32, after: Option<i64>) -> Result<reqwest::Response> {
        let url = format!("{STRAVA_API_BASE}/athlete/activities");
        let mut req = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .query(&[("per_page", PER_PAGE), ("page", page)]);
        if let Some(after) = after {
            req = req.query(&[("after", after)]);
        }
        req.send().await.with_context(|| {
            format!(
                "request to {url} failed. \
                 Check network connectivity and that \
                 api.strava.com is reachable from this host."
            )
        })
    }
}

/// Parse a `Retry-After` header value into a `Duration`. Per RFC 7231
/// the value can be either an integer number of seconds or an HTTP
/// date; Strava (and most APIs) emit seconds, so we only handle that
/// shape and ignore HTTP-date values.
fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let s = value?.to_str().ok()?;
    let secs: u64 = s.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Parse Strava's `15min,daily` header pair, e.g. `"42,500"`.
fn parse_rate_limit_pair(value: Option<&reqwest::header::HeaderValue>) -> Option<(u32, u32)> {
    let s = value?.to_str().ok()?;
    let mut parts = s.split(',');
    let a: u32 = parts.next()?.trim().parse().ok()?;
    let b: u32 = parts.next()?.trim().parse().ok()?;
    Some((a, b))
}

// ---- Wire format ----
//
// Strava's response shape doesn't quite match our flat `Activity`. We
// decode into this private DTO and then map. This keeps the public
// type free of the `start_latlng` / `map.summary_polyline` quirks.

#[derive(Debug, Deserialize)]
struct WireActivity {
    id: i64,
    name: String,
    #[serde(rename = "type")]
    activity_type: WireActivityType,
    start_date: DateTime<Utc>,
    /// `[lat, lng]`. Strava emits `[]` (an empty array) for activities
    /// recorded without GPS — treat that as (0.0, 0.0) and let
    /// downstream filtering throw them out.
    #[serde(default)]
    start_latlng: Vec<f64>,
    #[serde(default)]
    distance: f64,
    #[serde(default)]
    moving_time: i64,
    athlete: WireAthlete,
    #[serde(default)]
    gear_id: Option<String>,
    #[serde(default)]
    map: WireMap,
}

#[derive(Debug, Deserialize)]
struct WireAthlete {
    id: i64,
}

#[derive(Debug, Default, Deserialize)]
struct WireMap {
    #[serde(default)]
    summary_polyline: Option<String>,
}

/// Mirror of `ActivityType` decoded straight off the wire. We
/// keep this separate so that any future wire-only quirks (custom
/// renames, extra variants) don't leak into the public type.
#[derive(Debug, Deserialize)]
enum WireActivityType {
    Ride,
    EBikeRide,
    VirtualRide,
    Run,
    VirtualRun,
    Walk,
    Hike,
    #[serde(other)]
    Other,
}

impl From<WireActivityType> for ActivityType {
    fn from(w: WireActivityType) -> Self {
        match w {
            WireActivityType::Ride => ActivityType::Ride,
            WireActivityType::EBikeRide => ActivityType::EBikeRide,
            WireActivityType::VirtualRide => ActivityType::VirtualRide,
            WireActivityType::Run => ActivityType::Run,
            WireActivityType::VirtualRun => ActivityType::VirtualRun,
            WireActivityType::Walk => ActivityType::Walk,
            WireActivityType::Hike => ActivityType::Hike,
            WireActivityType::Other => ActivityType::Other,
        }
    }
}

impl WireActivity {
    fn into_activity(self) -> Activity {
        let (lat, lng) = match self.start_latlng.as_slice() {
            [lat, lng, ..] => (*lat, *lng),
            _ => (0.0, 0.0),
        };
        Activity {
            id: self.id,
            name: self.name,
            activity_type: self.activity_type.into(),
            start_date: self.start_date,
            start_lat: lat,
            start_lng: lng,
            distance_m: self.distance,
            moving_time_s: self.moving_time,
            summary_polyline: self.map.summary_polyline.unwrap_or_default(),
            athlete_id: self.athlete.id,
            gear_id: self.gear_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_strava_wire_format() {
        let json = r#"
        {
          "id": 1234567890,
          "name": "Morning Ride",
          "type": "Ride",
          "start_date": "2024-06-01T12:30:00Z",
          "start_latlng": [42.96, -85.67],
          "distance": 12500.0,
          "moving_time": 2400,
          "athlete": { "id": 42 },
          "gear_id": "b1234",
          "map": { "summary_polyline": "}_p~iF~ps|U_ulLnnqC" }
        }
        "#;

        let wire: WireActivity = serde_json::from_str(json).expect("decode");
        let a = wire.into_activity();
        assert_eq!(a.id, 1234567890);
        assert_eq!(a.name, "Morning Ride");
        assert_eq!(a.activity_type, ActivityType::Ride);
        assert!((a.start_lat - 42.96).abs() < 1e-9);
        assert!((a.start_lng - -85.67).abs() < 1e-9);
        assert!((a.distance_m - 12500.0).abs() < 1e-9);
        assert_eq!(a.moving_time_s, 2400);
        assert_eq!(a.athlete_id, 42);
        assert_eq!(a.gear_id.as_deref(), Some("b1234"));
        assert_eq!(a.summary_polyline, "}_p~iF~ps|U_ulLnnqC");
    }

    #[test]
    fn handles_missing_optional_fields() {
        // No start_latlng, no map, no gear_id, unknown type.
        let json = r#"
        {
          "id": 1,
          "name": "Indoor",
          "type": "Workout",
          "start_date": "2024-06-01T12:00:00Z",
          "distance": 0.0,
          "moving_time": 0,
          "athlete": { "id": 99 }
        }
        "#;
        let wire: WireActivity = serde_json::from_str(json).expect("decode");
        let a = wire.into_activity();
        assert_eq!(a.id, 1);
        assert_eq!(a.activity_type, ActivityType::Other);
        assert_eq!(a.start_lat, 0.0);
        assert_eq!(a.start_lng, 0.0);
        assert_eq!(a.gear_id, None);
        assert_eq!(a.summary_polyline, "");
    }

    #[test]
    fn parses_rate_limit_pair() {
        use reqwest::header::HeaderValue;
        let v = HeaderValue::from_static("42,500");
        assert_eq!(parse_rate_limit_pair(Some(&v)), Some((42, 500)));
        assert_eq!(parse_rate_limit_pair(None), None);
        let bad = HeaderValue::from_static("not-a-pair");
        assert_eq!(parse_rate_limit_pair(Some(&bad)), None);
    }

    #[test]
    fn parses_retry_after_seconds() {
        use reqwest::header::HeaderValue;
        let v = HeaderValue::from_static("30");
        assert_eq!(parse_retry_after(Some(&v)), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after(None), None);
        // HTTP-date form is intentionally not parsed; we treat it as
        // absent and fall through to the existing 15-min message.
        let date = HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT");
        assert_eq!(parse_retry_after(Some(&date)), None);
    }
}

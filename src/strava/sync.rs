// strava::sync — paginated activity sync into the local cache.
//
// The flow is intentionally minimal:
//   1. Look up the per-athlete watermark (`last_synced_at`).
//   2. Ask Strava for everything strictly newer than that timestamp.
//   3. Upsert each activity (so re-runs of an interrupted sync are safe).
//   4. Bump the watermark to "now" (not to the newest activity's
//      start_date, so manually-added old uploads still get picked up
//      on the next pass — Strava sorts by upload time).
//
// SyncStats is returned for the CLI to print a one-line summary.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;

use crate::cache::queries;
use crate::strava::client::StravaClient;

/// Counts returned from a single sync run.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncStats {
    /// Activities returned by the Strava API.
    pub fetched: usize,
    /// Activities written into the cache (insert or update).
    pub upserted: usize,
}

/// Run an incremental sync for one athlete.
///
/// Errors carry the original API or DB failure plus a hint for the
/// user (re-auth, retry, check connectivity). The watermark is only
/// advanced after every upsert succeeds, so a partial run leaves the
/// cache in a state that the next invocation will resume from.
pub async fn sync(conn: &Connection, client: &StravaClient, athlete_id: i64) -> Result<SyncStats> {
    let watermark = queries::last_synced_at(conn, athlete_id)?;
    let after = watermark.map(|d| d.timestamp());

    tracing::info!(athlete_id, ?watermark, "starting Strava sync");

    let activities = client.list_activities(after).await.with_context(|| {
        "could not fetch activities from Strava. \
             If this is a credential failure, run `patinate auth`; \
             otherwise wait a moment and retry."
    })?;

    let fetched = activities.len();
    let mut upserted = 0usize;
    for a in &activities {
        queries::upsert_activity(conn, a)?;
        upserted += 1;
    }

    queries::set_last_synced_at(conn, athlete_id, Utc::now())?;

    tracing::info!(athlete_id, fetched, upserted, "sync complete");
    Ok(SyncStats { fetched, upserted })
}

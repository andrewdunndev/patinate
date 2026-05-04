// cache::queries — typed read/write helpers over rusqlite
//
// All time values cross the SQLite boundary as RFC 3339 strings so
// that they sort lexicographically and stay human-readable in the
// shell. Activity types are stored as the same strings the Strava API
// emits (matching `ActivityType::as_data_attr()`).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params, params_from_iter, types::Value};

use crate::strava::{Activity, ActivityType};

fn parse_activity_type(s: &str) -> ActivityType {
    match s {
        "Ride" => ActivityType::Ride,
        "EBikeRide" => ActivityType::EBikeRide,
        "VirtualRide" => ActivityType::VirtualRide,
        "Run" => ActivityType::Run,
        "VirtualRun" => ActivityType::VirtualRun,
        "Walk" => ActivityType::Walk,
        "Hike" => ActivityType::Hike,
        // Strava adds new activity types over time. Unknown ones bucket
        // into Other so we don't break sync; emit a warn so the operator
        // knows a future patinate release should add it explicitly.
        other => {
            tracing::warn!(
                activity_type = other,
                "unknown Strava activity type bucketed as Other"
            );
            ActivityType::Other
        }
    }
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .with_context(|| {
            format!(
                "could not parse timestamp {s:?} as RFC 3339. \
                 The cache may have been written by a different \
                 patinate version: remove cache.db and re-sync."
            )
        })
}

/// Insert an activity, or update every column on conflict by id.
pub fn upsert_activity(conn: &Connection, a: &Activity) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO activities (
            id, name, activity_type, start_date,
            start_lat, start_lng, distance_m, moving_time_s,
            summary_polyline, athlete_id, gear_id, fetched_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
         )
         ON CONFLICT(id) DO UPDATE SET
            name             = excluded.name,
            activity_type    = excluded.activity_type,
            start_date       = excluded.start_date,
            start_lat        = excluded.start_lat,
            start_lng        = excluded.start_lng,
            distance_m       = excluded.distance_m,
            moving_time_s    = excluded.moving_time_s,
            summary_polyline = excluded.summary_polyline,
            athlete_id       = excluded.athlete_id,
            gear_id          = excluded.gear_id,
            fetched_at       = excluded.fetched_at
         ",
        params![
            a.id,
            a.name,
            a.activity_type.as_data_attr(),
            a.start_date.to_rfc3339(),
            a.start_lat,
            a.start_lng,
            a.distance_m,
            a.moving_time_s,
            a.summary_polyline,
            a.athlete_id,
            a.gear_id,
            now,
        ],
    )
    .with_context(|| {
        format!(
            "could not upsert activity id={} into cache. \
             The cache.db may be locked by another process; \
             close other patinate runs and retry.",
            a.id
        )
    })?;
    Ok(())
}

/// Fetch all activities matching the supplied filters. Any filter
/// passed as `None` is omitted from the WHERE clause; passing all
/// `None` returns every row, ordered newest-first.
pub fn list_activities(
    conn: &Connection,
    athlete_id: Option<i64>,
    types: Option<&[ActivityType]>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Vec<Activity>> {
    let mut sql = String::from(
        "SELECT id, name, activity_type, start_date, start_lat, start_lng, \
         distance_m, moving_time_s, summary_polyline, athlete_id, gear_id \
         FROM activities WHERE 1=1",
    );
    let mut bound: Vec<Value> = Vec::new();

    if let Some(id) = athlete_id {
        sql.push_str(" AND athlete_id = ?");
        bound.push(Value::Integer(id));
    }
    if let Some(types) = types {
        if !types.is_empty() {
            sql.push_str(" AND activity_type IN (");
            for (i, t) in types.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push('?');
                bound.push(Value::Text(t.as_data_attr().to_string()));
            }
            sql.push(')');
        }
    }
    if let Some(since) = since {
        sql.push_str(" AND start_date >= ?");
        bound.push(Value::Text(since.to_rfc3339()));
    }
    if let Some(until) = until {
        sql.push_str(" AND start_date <= ?");
        bound.push(Value::Text(until.to_rfc3339()));
    }
    sql.push_str(" ORDER BY start_date DESC");

    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("could not prepare list query: {sql}"))?;
    let rows = stmt
        .query_map(params_from_iter(bound.iter()), |row| {
            let type_str: String = row.get(2)?;
            let date_str: String = row.get(3)?;
            Ok((
                Activity {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    activity_type: parse_activity_type(&type_str),
                    // Placeholder — rewritten below once we can return Result.
                    start_date: Utc::now(),
                    start_lat: row.get(4)?,
                    start_lng: row.get(5)?,
                    distance_m: row.get(6)?,
                    moving_time_s: row.get(7)?,
                    summary_polyline: row.get(8)?,
                    athlete_id: row.get(9)?,
                    gear_id: row.get(10)?,
                },
                date_str,
            ))
        })
        .with_context(|| "could not execute list_activities query")?;

    let mut out = Vec::new();
    for row in rows {
        let (mut a, date_str) = row.with_context(|| "row decode failed")?;
        a.start_date = parse_rfc3339(&date_str)?;
        out.push(a);
    }
    Ok(out)
}

/// Read the recorded "last sync" watermark for an athlete, if any.
pub fn last_synced_at(conn: &Connection, athlete_id: i64) -> Result<Option<DateTime<Utc>>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT last_synced_at FROM sync_state WHERE athlete_id = ?1",
            params![athlete_id],
            |r| r.get(0),
        )
        .optional()
        .with_context(|| {
            format!(
                "could not read sync_state for athlete {athlete_id}. \
                 If cache.db is corrupt, remove it and re-run `patinate sync`."
            )
        })?;
    row.map(|s| parse_rfc3339(&s)).transpose()
}

/// Upsert the "last sync" watermark for an athlete.
pub fn set_last_synced_at(conn: &Connection, athlete_id: i64, when: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (athlete_id, last_synced_at)
         VALUES (?1, ?2)
         ON CONFLICT(athlete_id) DO UPDATE SET last_synced_at = excluded.last_synced_at",
        params![athlete_id, when.to_rfc3339()],
    )
    .with_context(|| format!("could not record sync watermark for athlete {athlete_id}"))?;
    Ok(())
}

/// Cached OAuth credentials for one athlete.
///
/// `refresh_token` is always present once the row is written; the
/// access token + its expiry are tracked together so callers can decide
/// whether to reuse the stored access token or trade the refresh token
/// for a fresh one.
#[derive(Debug, Clone)]
pub struct StoredTokens {
    pub athlete_id: i64,
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub access_expires_at: Option<DateTime<Utc>>,
}

/// Insert or replace the OAuth credentials for one athlete.
///
/// Strava rotates the refresh token on most refreshes; callers should
/// always pass through whatever the API last returned so the cache
/// stays in lockstep with Strava's view of the world.
pub fn upsert_oauth(conn: &Connection, t: &StoredTokens) -> Result<()> {
    let expires = t.access_expires_at.map(|d| d.to_rfc3339());
    conn.execute(
        "INSERT INTO oauth (
            athlete_id, refresh_token, access_token, access_expires_at
         ) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(athlete_id) DO UPDATE SET
            refresh_token     = excluded.refresh_token,
            access_token      = excluded.access_token,
            access_expires_at = excluded.access_expires_at
         ",
        params![t.athlete_id, t.refresh_token, t.access_token, expires],
    )
    .with_context(|| {
        format!(
            "could not upsert oauth row for athlete {}. \
             The cache.db may be locked by another process; \
             close other patinate runs and retry.",
            t.athlete_id
        )
    })?;
    Ok(())
}

/// Read the cached OAuth credentials for one athlete, if any.
pub fn get_oauth(conn: &Connection, athlete_id: i64) -> Result<Option<StoredTokens>> {
    let row: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT refresh_token, access_token, access_expires_at
             FROM oauth WHERE athlete_id = ?1",
            params![athlete_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .with_context(|| {
            format!(
                "could not read oauth row for athlete {athlete_id}. \
                 If cache.db is corrupt, remove it and re-run `patinate sync`."
            )
        })?;
    let Some((refresh_token, access_token, expires_str)) = row else {
        return Ok(None);
    };
    let access_expires_at = expires_str.map(|s| parse_rfc3339(&s)).transpose()?;
    Ok(Some(StoredTokens {
        athlete_id,
        refresh_token,
        access_token,
        access_expires_at,
    }))
}

// rusqlite's `OptionalExtension` brings `.optional()` into scope.
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::schema;
    use chrono::TimeZone;

    fn fixture(id: i64, t: ActivityType, when: DateTime<Utc>) -> Activity {
        Activity {
            id,
            name: format!("activity-{id}"),
            activity_type: t,
            start_date: when,
            start_lat: 42.96,
            start_lng: -85.67,
            distance_m: 12500.0,
            moving_time_s: 2400,
            summary_polyline: "abc".to_string(),
            athlete_id: 1,
            gear_id: None,
        }
    }

    #[test]
    fn upsert_then_filter_by_type() {
        let conn = Connection::open_in_memory().expect("open db");
        schema::init(&conn).expect("init schema");

        let t0 = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2024, 6, 2, 12, 0, 0).unwrap();

        upsert_activity(&conn, &fixture(1, ActivityType::Ride, t0)).unwrap();
        upsert_activity(&conn, &fixture(2, ActivityType::Run, t1)).unwrap();

        let rides = list_activities(&conn, None, Some(&[ActivityType::Ride]), None, None)
            .expect("list rides");
        assert_eq!(rides.len(), 1);
        assert_eq!(rides[0].id, 1);
        assert_eq!(rides[0].activity_type, ActivityType::Ride);

        // Without filter we get both, newest-first.
        let all = list_activities(&conn, None, None, None, None).expect("list all");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, 2);
        assert_eq!(all[1].id, 1);
    }

    #[test]
    fn sync_watermark_roundtrip() {
        let conn = Connection::open_in_memory().expect("open db");
        schema::init(&conn).expect("init schema");

        assert!(last_synced_at(&conn, 1).unwrap().is_none());
        let when = Utc.with_ymd_and_hms(2024, 6, 3, 0, 0, 0).unwrap();
        set_last_synced_at(&conn, 1, when).unwrap();
        let got = last_synced_at(&conn, 1).unwrap().unwrap();
        assert_eq!(got, when);
    }

    #[test]
    fn oauth_roundtrip() {
        let conn = Connection::open_in_memory().expect("open db");
        schema::init(&conn).expect("init schema");

        // Empty cache: get returns None.
        assert!(get_oauth(&conn, 42).unwrap().is_none());

        let expires = Utc.with_ymd_and_hms(2024, 6, 3, 12, 0, 0).unwrap();
        let stored = StoredTokens {
            athlete_id: 42,
            refresh_token: "refresh-abc".to_string(),
            access_token: Some("access-xyz".to_string()),
            access_expires_at: Some(expires),
        };
        upsert_oauth(&conn, &stored).expect("upsert");

        let got = get_oauth(&conn, 42).expect("get").expect("row present");
        assert_eq!(got.athlete_id, 42);
        assert_eq!(got.refresh_token, "refresh-abc");
        assert_eq!(got.access_token.as_deref(), Some("access-xyz"));
        assert_eq!(got.access_expires_at, Some(expires));

        // Null access_token / expires also round-trip.
        let stored2 = StoredTokens {
            athlete_id: 99,
            refresh_token: "refresh-2".to_string(),
            access_token: None,
            access_expires_at: None,
        };
        upsert_oauth(&conn, &stored2).expect("upsert null");
        let got2 = get_oauth(&conn, 99).expect("get").expect("row present");
        assert_eq!(got2.refresh_token, "refresh-2");
        assert!(got2.access_token.is_none());
        assert!(got2.access_expires_at.is_none());
    }

    #[test]
    fn oauth_upsert_overwrites() {
        let conn = Connection::open_in_memory().expect("open db");
        schema::init(&conn).expect("init schema");

        let t0 = Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap();
        upsert_oauth(
            &conn,
            &StoredTokens {
                athlete_id: 7,
                refresh_token: "first".to_string(),
                access_token: Some("aaa".to_string()),
                access_expires_at: Some(t0),
            },
        )
        .expect("first upsert");

        let t1 = Utc.with_ymd_and_hms(2024, 6, 2, 0, 0, 0).unwrap();
        upsert_oauth(
            &conn,
            &StoredTokens {
                athlete_id: 7,
                refresh_token: "second".to_string(),
                access_token: Some("bbb".to_string()),
                access_expires_at: Some(t1),
            },
        )
        .expect("second upsert");

        let got = get_oauth(&conn, 7).expect("get").expect("row present");
        assert_eq!(got.refresh_token, "second");
        assert_eq!(got.access_token.as_deref(), Some("bbb"));
        assert_eq!(got.access_expires_at, Some(t1));

        // Only one row per athlete_id.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM oauth", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}

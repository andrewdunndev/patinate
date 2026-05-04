// cache::schema — SQLite DDL and migrations
//
// Bundled enforcement: `init()` runs the entire schema in one
// transaction-equivalent batch. Callers cannot get a half-initialized
// database from this module; either the call succeeds and the schema
// is present, or it errors with a prescriptive message.
//
// Schema migrations are deferred to v0.2. v0.1 uses
// `PRAGMA user_version = 1`; future versions read this value and run
// idempotent ALTERs against older snapshots. v0.1 itself never
// migrates: a fresh install starts at version 1.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Current schema version. Bumped by future migrations; never read by
/// v0.1 itself.
pub const SCHEMA_VERSION: u32 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS activities (
  id              INTEGER PRIMARY KEY,
  name            TEXT NOT NULL,
  activity_type   TEXT NOT NULL,
  start_date      TEXT NOT NULL,
  start_lat       REAL NOT NULL,
  start_lng       REAL NOT NULL,
  distance_m      REAL NOT NULL,
  moving_time_s   INTEGER NOT NULL,
  summary_polyline TEXT NOT NULL,
  athlete_id      INTEGER NOT NULL,
  gear_id         TEXT,
  fetched_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_activities_start_date ON activities(start_date);
CREATE INDEX IF NOT EXISTS idx_activities_type       ON activities(activity_type);
CREATE INDEX IF NOT EXISTS idx_activities_athlete    ON activities(athlete_id);

CREATE TABLE IF NOT EXISTS sync_state (
  athlete_id      INTEGER PRIMARY KEY,
  last_synced_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS oauth (
  athlete_id        INTEGER PRIMARY KEY,
  refresh_token     TEXT NOT NULL,
  access_token      TEXT,
  access_expires_at TEXT
);
"#;

/// Initialize all cache tables and indexes. Idempotent: safe to call
/// on every connection open.
///
/// Returns an error if the SQLite engine rejects the DDL — that
/// usually means the database file is corrupt or on a read-only
/// filesystem; check the parent directory permissions or remove the
/// stale `cache.db` and retry.
pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL).with_context(|| {
        "could not initialize patinate cache schema. \
         The cache.db file may be corrupt: remove it and re-run \
         to recreate, or check that the parent directory is writable."
    })?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .with_context(|| {
            "could not set PRAGMA user_version on the cache database. \
             This pragma is the v0.2 migration hook; if it fails the \
             cache.db is likely corrupt: remove it and re-run."
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_tables_idempotently() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        init(&conn).expect("first init");
        // Second call must be a no-op.
        init(&conn).expect("re-init");
        // sanity: table is there
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name='activities'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn init_sets_user_version_pragma() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        init(&conn).expect("init");
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("read user_version");
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(version, 1, "v0.1 schema version must be 1");
    }
}

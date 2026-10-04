// module: cache — SQLite-backed activity store
//
// Bundled enforcement: callers cannot obtain a `Connection` from this
// module without the schema also being initialized. `open()` runs the
// directory create + connection open + `schema::init` steps in one go,
// so downstream code never has to remember which order to call them in.

pub mod queries;
pub mod schema;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default cache database path, following XDG-style conventions.
///
/// * Linux:   `~/.local/share/patinate/cache.db`
/// * macOS:   `~/Library/Application Support/dev.dunn.patinate/cache.db`
/// * Windows: `%APPDATA%\dunn\patinate\data\cache.db`
///
/// Returns an error only on the (rare) systems where directories
/// cannot resolve a home directory at all; in that case set
/// `--cache <path>` explicitly.
pub fn default_cache_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "dunn", "patinate").with_context(|| {
        "could not resolve a per-user data directory. \
         Pass `--cache <path>` to choose an explicit location."
    })?;
    Ok(dirs.data_dir().join("cache.db"))
}

/// Open (or create) a cache database at `path`, ensuring the parent
/// directory exists and the schema is initialized. The returned
/// connection is ready for use by `queries::*`.
///
/// The cache holds the Strava tokens and every synced route, home
/// included, so on unix a new directory is created 0700 and the
/// database is created, or tightened, to 0600. SQLite gives its
/// journal the database's mode.
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_private_dir(parent).with_context(|| {
                format!(
                    "could not create cache directory {parent:?}. \
                     Check filesystem permissions or pick a different \
                     `--cache` path."
                )
            })?;
        }
    }
    make_private_file(path)
        .with_context(|| format!("could not restrict the cache database {path:?} to 0600"))?;
    let conn = Connection::open(path).with_context(|| {
        format!(
            "could not open SQLite database at {path:?}. \
             If the file is corrupt, remove it and re-run \
             `patinate sync` to rebuild the cache."
        )
    })?;
    // Guard against `database is locked` when a cron sync overlaps a
    // manual one. Five seconds is generous for the workloads here
    // (single-writer, short transactions); WAL mode is deferred.
    conn.busy_timeout(Duration::from_secs(5)).with_context(|| {
        format!(
            "could not open SQLite database at {path:?}. \
             If the file is corrupt, remove it and re-run \
             `patinate sync` to rebuild the cache."
        )
    })?;
    schema::init(&conn)?;
    Ok(conn)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn make_private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_parent_and_schema() {
        let tmp = std::env::temp_dir().join("patinate-test-cache");
        let _ = std::fs::remove_dir_all(&tmp);
        let db = tmp.join("nested").join("cache.db");
        let conn = open(&db).expect("open");
        // schema present
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='activities'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn open_keeps_the_cache_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        let tmp = std::env::temp_dir().join("patinate-test-cache-private");
        let _ = std::fs::remove_dir_all(&tmp);

        let db = tmp.join("nested").join("cache.db");
        open(&db).expect("open");
        assert_eq!(mode(&db), 0o600, "new database");
        assert_eq!(mode(&tmp.join("nested")), 0o700, "new directory");

        // A database left world-readable by an older release is tightened.
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        open(&db).expect("reopen");
        assert_eq!(mode(&db), 0o600, "existing database");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

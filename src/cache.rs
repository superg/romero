use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};

use crate::error::{Result, RomeroError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CacheRecord {
    pub area: String,
    pub path: String,
    pub size: u64,
    pub modified_ns: i64,
    pub sha1: String,
}

pub(crate) trait CacheStore {
    fn get(&self, area: &str, path: &str) -> Result<Option<CacheRecord>>;
    fn put(&mut self, record: &CacheRecord) -> Result<()>;
    fn remove(&mut self, area: &str, path: &str) -> Result<bool>;
    fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<bool>;
    fn checkpoint(&mut self) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
}

pub(crate) struct SqliteCache {
    connection: Connection,
    transaction_open: bool,
}

impl SqliteCache {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
    }

    fn open_with_flags(path: &Path, flags: OpenFlags) -> Result<Self> {
        let connection = Connection::open_with_flags(path, flags).map_err(|error| {
            RomeroError::Cache(format!("cannot open cache {}: {error}", path.display()))
        })?;
        Self::initialize(
            connection,
            "PRAGMA journal_mode = DELETE;
             PRAGMA synchronous = FULL;",
            "cannot initialize cache",
        )
    }

    fn initialize(connection: Connection, pragmas: &str, error_context: &str) -> Result<Self> {
        connection.busy_timeout(Duration::ZERO).map_err(|error| {
            RomeroError::Cache(format!("cannot configure cache locking: {error}"))
        })?;
        connection
            .execute_batch(pragmas)
            .map_err(|error| RomeroError::Cache(format!("{error_context}: {error}")))?;
        connection
            .execute_batch(
                "BEGIN EXCLUSIVE;
                 CREATE TABLE IF NOT EXISTS files (
                    area TEXT NOT NULL,
                    path TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    modified_ns INTEGER NOT NULL,
                    sha1 TEXT NOT NULL,
                    PRIMARY KEY (area, path)
                 );",
            )
            .map_err(|error| {
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) {
                    RomeroError::Cache("another Romero process is already running".into())
                } else {
                    RomeroError::Cache(format!("{error_context}: {error}"))
                }
            })?;
        Ok(Self {
            connection,
            transaction_open: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()
            .map_err(|error| RomeroError::Cache(format!("cannot open memory cache: {error}")))?;
        Self::initialize(
            connection,
            "PRAGMA journal_mode = MEMORY;
             PRAGMA temp_store = MEMORY;
             PRAGMA synchronous = OFF;",
            "cannot initialize memory cache",
        )
    }
}

impl CacheStore for SqliteCache {
    fn get(&self, area: &str, path: &str) -> Result<Option<CacheRecord>> {
        let mut statement = self
            .connection
            .prepare("SELECT size, modified_ns, sha1 FROM files WHERE area = ?1 AND path = ?2")
            .map_err(cache_error)?;
        let mut rows = statement.query(params![area, path]).map_err(cache_error)?;
        let Some(row) = rows.next().map_err(cache_error)? else {
            return Ok(None);
        };
        let size: i64 = row.get(0).map_err(cache_error)?;
        Ok(Some(CacheRecord {
            area: area.to_owned(),
            path: path.to_owned(),
            size: u64::try_from(size)
                .map_err(|_| RomeroError::Cache("negative cached file size".into()))?,
            modified_ns: row.get(1).map_err(cache_error)?,
            sha1: row.get(2).map_err(cache_error)?,
        }))
    }

    fn put(&mut self, record: &CacheRecord) -> Result<()> {
        let size = i64::try_from(record.size)
            .map_err(|_| RomeroError::Cache("file is too large for the cache".into()))?;
        self.connection
            .execute(
                "INSERT INTO files (area, path, size, modified_ns, sha1)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(area, path) DO UPDATE SET
                    size = excluded.size,
                    modified_ns = excluded.modified_ns,
                    sha1 = excluded.sha1",
                params![
                    record.area,
                    record.path,
                    size,
                    record.modified_ns,
                    record.sha1
                ],
            )
            .map_err(cache_error)?;
        Ok(())
    }

    fn remove(&mut self, area: &str, path: &str) -> Result<bool> {
        let removed = self
            .connection
            .execute(
                "DELETE FROM files WHERE area = ?1 AND path = ?2",
                params![area, path],
            )
            .map_err(cache_error)?;
        Ok(removed != 0)
    }

    fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<bool> {
        let mut rows_by_key = BTreeMap::new();
        {
            let mut statement = self
                .connection
                .prepare("SELECT area, path FROM files")
                .map_err(cache_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(cache_error)?;
            for row in rows {
                let key = row.map_err(cache_error)?;
                rows_by_key.insert(key.clone(), key);
            }
        }
        let mut changed = false;
        for (area, path) in rows_by_key.into_keys() {
            if !seen.contains(&(area.clone(), path.clone())) {
                changed |= self.remove(&area, &path)?;
            }
        }
        Ok(changed)
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.connection
            .execute_batch("COMMIT")
            .map_err(cache_error)?;
        self.transaction_open = false;
        self.connection
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(cache_lock_error)?;
        self.transaction_open = true;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.connection
            .execute_batch("COMMIT")
            .map_err(cache_error)?;
        self.transaction_open = false;
        Ok(())
    }
}

impl Drop for SqliteCache {
    fn drop(&mut self) {
        if self.transaction_open {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
    }
}

fn cache_error(error: rusqlite::Error) -> RomeroError {
    RomeroError::Cache(format!("cache operation failed: {error}"))
}

fn cache_lock_error(error: rusqlite::Error) -> RomeroError {
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        RomeroError::Cache("another Romero process is already running".into())
    } else {
        cache_error(error)
    }
}

pub(crate) fn relative_cache_key(path: &Path) -> String {
    encode_path(path)
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut encoded = String::from("u:");
    for byte in path.as_os_str().as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;

    let mut encoded = String::from("w:");
    for unit in path.as_os_str().encode_wide() {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{unit:04x}");
    }
    encoded
}

#[cfg(not(any(unix, windows)))]
fn encode_path(path: &Path) -> String {
    format!("p:{}", path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_cache_supports_lifecycle_without_filesystem_access() {
        let mut cache = SqliteCache::in_memory().unwrap();
        let record = CacheRecord {
            area: "work".into(),
            path: relative_cache_key(Path::new("disc.bin")),
            size: 42,
            modified_ns: 7,
            sha1: "1".repeat(40),
        };
        cache.put(&record).unwrap();
        assert_eq!(
            cache.get(&record.area, &record.path).unwrap(),
            Some(record.clone())
        );
        cache.remove(&record.area, &record.path).unwrap();
        assert_eq!(cache.get(&record.area, &record.path).unwrap(), None);
        cache.commit().unwrap();
    }

    #[test]
    fn checkpoint_persists_prior_changes_and_starts_a_new_transaction() {
        let mut cache = SqliteCache::in_memory().unwrap();
        let record = CacheRecord {
            area: "work".into(),
            path: relative_cache_key(Path::new("disc.bin")),
            size: 42,
            modified_ns: 7,
            sha1: "1".repeat(40),
        };
        cache.put(&record).unwrap();
        cache.checkpoint().unwrap();
        cache.remove(&record.area, &record.path).unwrap();

        cache.connection.execute_batch("ROLLBACK").unwrap();
        cache.transaction_open = false;

        assert_eq!(cache.get(&record.area, &record.path).unwrap(), Some(record));
    }

    #[test]
    fn cache_keys_do_not_contain_absolute_roots() {
        let key = relative_cache_key(Path::new("Sony - PlayStation/Game.bin"));
        assert!(!key.contains("/home"));
        assert!(!key.contains("C:\\"));
    }

    #[test]
    fn retain_removes_only_unseen_records() {
        let mut cache = SqliteCache::in_memory().unwrap();
        let kept = CacheRecord {
            area: "work".into(),
            path: relative_cache_key(Path::new("kept.bin")),
            size: 1,
            modified_ns: 1,
            sha1: "1".repeat(40),
        };
        let removed = CacheRecord {
            path: relative_cache_key(Path::new("removed.bin")),
            ..kept.clone()
        };
        cache.put(&kept).unwrap();
        cache.put(&removed).unwrap();
        let seen = BTreeSet::from([(kept.area.clone(), kept.path.clone())]);

        assert!(cache.retain(&seen).unwrap());
        assert_eq!(cache.get(&kept.area, &kept.path).unwrap(), Some(kept));
        assert_eq!(cache.get(&removed.area, &removed.path).unwrap(), None);
        assert!(!cache.retain(&seen).unwrap());
    }

    #[test]
    fn rejects_sizes_outside_sqlite_integer_range() {
        let mut cache = SqliteCache::in_memory().unwrap();
        let oversized = CacheRecord {
            area: "work".into(),
            path: relative_cache_key(Path::new("large.bin")),
            size: u64::MAX,
            modified_ns: 1,
            sha1: "1".repeat(40),
        };
        assert!(
            cache
                .put(&oversized)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );

        cache
            .connection
            .execute(
                "INSERT INTO files (area, path, size, modified_ns, sha1)
                 VALUES ('work', 'negative', -1, 1, 'hash')",
                [],
            )
            .unwrap();
        assert!(
            cache
                .get("work", "negative")
                .unwrap_err()
                .to_string()
                .contains("negative cached file size")
        );
    }

    #[test]
    fn in_memory_cache_forces_sqlite_temporary_storage_to_memory() {
        let cache = SqliteCache::in_memory().unwrap();
        let temp_store: i64 = cache
            .connection
            .query_row("PRAGMA temp_store", [], |row| row.get(0))
            .unwrap();
        assert_eq!(temp_store, 2);
    }

    #[test]
    fn busy_and_locked_sqlite_errors_use_the_concurrent_run_message() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
            assert_eq!(
                cache_lock_error(error).to_string(),
                "another Romero process is already running"
            );
        }

        let error = rusqlite::Error::InvalidQuery;
        assert!(
            cache_lock_error(error)
                .to_string()
                .contains("cache operation failed")
        );
    }
}

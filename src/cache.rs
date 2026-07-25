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
    fn remove(&mut self, area: &str, path: &str) -> Result<()>;
    fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<()>;
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
        connection.busy_timeout(Duration::ZERO).map_err(|error| {
            RomeroError::Cache(format!("cannot configure cache locking: {error}"))
        })?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = DELETE;
                 PRAGMA synchronous = FULL;
                 BEGIN EXCLUSIVE;
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
                    RomeroError::Cache(format!("cannot initialize cache: {error}"))
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
        connection
            .execute_batch(
                "BEGIN EXCLUSIVE;
                 CREATE TABLE files (
                    area TEXT NOT NULL,
                    path TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    modified_ns INTEGER NOT NULL,
                    sha1 TEXT NOT NULL,
                    PRIMARY KEY (area, path)
                 );",
            )
            .map_err(|error| {
                RomeroError::Cache(format!("cannot initialize memory cache: {error}"))
            })?;
        Ok(Self {
            connection,
            transaction_open: true,
        })
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

    fn remove(&mut self, area: &str, path: &str) -> Result<()> {
        self.connection
            .execute(
                "DELETE FROM files WHERE area = ?1 AND path = ?2",
                params![area, path],
            )
            .map_err(cache_error)?;
        Ok(())
    }

    fn retain(&mut self, seen: &BTreeSet<(String, String)>) -> Result<()> {
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
        for (area, path) in rows_by_key.into_keys() {
            if !seen.contains(&(area.clone(), path.clone())) {
                self.remove(&area, &path)?;
            }
        }
        Ok(())
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
}

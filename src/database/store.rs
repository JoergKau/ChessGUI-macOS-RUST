//! Opening, creating and describing `.sbd` database files.
//!
//! A `.sbd` is a plain SQLite 3 file. The header check plus the
//! `db_info.schema_version` row is what separates "one of ours" from "some
//! other SQLite file the user happened to pick": the latter is refused with a
//! message instead of being written to.
//!
//! **Threading rule:** every caller opens its own [`Connection`] (see
//! [`connection`]). Connections are cheap, SQLite's WAL mode allows one writer
//! and any number of readers, and this keeps the UI thread free of database
//! work. [`Connection`] is `Send`, so it can be created inside a blocking
//! task and dropped there.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use super::model::DatabaseInfo;

/// Schema version this build writes and understands.
pub const SCHEMA_VERSION: i64 = 1;

/// The schema, embedded so a release binary carries it.
const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Everything that can go wrong in the database layer.
#[derive(Debug)]
pub enum DbError {
    /// SQLite reported a problem.
    Sql(rusqlite::Error),
    /// A file could not be read or written.
    Io {
        /// The file involved.
        path: PathBuf,
        /// The operating system's reason.
        source: std::io::Error,
    },
    /// The file is not a SiliconBoard database (wrong header or no schema).
    NotADatabase {
        /// The file that was refused.
        path: PathBuf,
    },
    /// The file was written by a newer build than this one.
    NewerSchema {
        /// Version found in the file.
        found: i64,
        /// Version this build understands.
        supported: i64,
    },
    /// A database was asked to be created where a file already is.
    AlreadyExists {
        /// The path in the way.
        path: PathBuf,
    },
    /// The Scid decoder refused the file or one of its games.
    Scid(String),
    /// A position filter was used but the database has no position index.
    NoPositionIndex,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sql(source) => write!(f, "database error: {source}"),
            DbError::Io { path, source } => {
                write!(f, "`{}` could not be read or written: {source}", path.display())
            }
            DbError::NotADatabase { path } => write!(
                f,
                "`{}` is not a SiliconBoard database",
                path.display()
            ),
            DbError::NewerSchema { found, supported } => write!(
                f,
                "the database was written by a newer SiliconBoard (schema {found}, this build reads {supported})"
            ),
            DbError::AlreadyExists { path } => write!(
                f,
                "`{}` already exists — choose another name",
                path.display()
            ),
            DbError::Scid(message) => write!(f, "Scid file: {message}"),
            DbError::NoPositionIndex => write!(
                f,
                "This database has no position index, so the Position filter cannot match. \
                 Use \"Rebuild position index\" on the database overview, or clear the Position field."
            ),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DbError::Sql(source) => Some(source),
            DbError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(source: rusqlite::Error) -> Self {
        DbError::Sql(source)
    }
}

/// A fresh connection to `path` with the PRAGMAs every job uses.
///
/// WAL keeps readers running while an import writes; `busy_timeout` turns a
/// momentary write lock into a short wait instead of an error, and the page
/// cache and mmap sizes are set for the multi-million-game case.
pub fn connection(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open(path).map_err(|source| DbError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(source),
    })?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -131072;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 268435456;",
    )?;
    Ok(conn)
}

/// Create a new database at `path` with the given title and description.
///
/// Refuses to overwrite an existing file so a conversion can never destroy a
/// database the user already has. The header of the new file is checked again
/// through [`open`], which also proves the schema ran.
pub fn create(path: &Path, title: &str, description: &str) -> Result<DatabaseInfo, DbError> {
    if path.exists() {
        return Err(DbError::AlreadyExists {
            path: path.to_path_buf(),
        });
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| DbError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let conn = connection(path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    let created_at = unix_now();
    let info = [
        ("schema_version", SCHEMA_VERSION.to_string()),
        ("title", title.to_string()),
        ("description", description.to_string()),
        ("created_at", created_at.to_string()),
        ("game_count", "0".to_string()),
    ];
    for (key, value) in info {
        set_info(&conn, key, &value)?;
    }
    drop(conn);
    open(path)
}

/// Open an existing database and read its description.
pub fn open(path: &Path) -> Result<DatabaseInfo, DbError> {
    if !path.is_file() {
        return Err(DbError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        });
    }
    // Refuse anything that is not SQLite before handing it to the driver.
    let header = read_header(path)?;
    if &header != b"SQLite format 3\0" {
        return Err(DbError::NotADatabase {
            path: path.to_path_buf(),
        });
    }
    let conn = connection(path)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(DbError::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version < SCHEMA_VERSION {
        // No schema of ours, even if the file is valid SQLite.
        return Err(DbError::NotADatabase {
            path: path.to_path_buf(),
        });
    }
    let title = get_info(&conn, "title").unwrap_or_default();
    let description = get_info(&conn, "description").unwrap_or_default();
    let stored_count = get_info(&conn, "game_count")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    let created_at = get_info(&conn, "created_at")
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(0);
    let live_count: i64 = conn.query_row("SELECT COUNT(*) FROM games", [], |row| row.get(0))?;
    Ok(DatabaseInfo {
        path: path.to_path_buf(),
        title,
        description,
        game_count: stored_count.max(live_count as u64),
        created_at,
    })
}

/// Read one `db_info` value.
pub fn get_info(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM db_info WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

/// Write one `db_info` value, replacing it when present.
pub fn set_info(conn: &Connection, key: &str, value: &str) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO db_info(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// The first 16 bytes of a file, for the SQLite magic check.
fn read_header(path: &Path) -> Result<[u8; 16], DbError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|source| DbError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header).map_err(|source| DbError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(header)
}

/// Seconds since the Unix epoch, saturating at 0 before 1970.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sbd");
        (dir, path)
    }

    #[test]
    fn create_then_open_round_trips_the_metadata() {
        let (_dir, path) = temp_db();
        let created = create(&path, "My games", "A test database").unwrap();
        assert_eq!(created.title, "My games");
        assert_eq!(created.description, "A test database");
        assert_eq!(created.game_count, 0);

        let reopened = open(&path).unwrap();
        assert_eq!(reopened.title, "My games");
        assert_eq!(reopened.path, path);
    }

    #[test]
    fn creating_over_an_existing_file_is_refused() {
        let (_dir, path) = temp_db();
        create(&path, "One", "").unwrap();
        match create(&path, "Two", "") {
            Err(DbError::AlreadyExists { .. }) => {}
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_sqlite_file_is_not_our_database() {
        let (_dir, path) = temp_db();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE unrelated (x);").unwrap();
        drop(conn);
        match open(&path) {
            Err(DbError::NotADatabase { .. }) => {}
            other => panic!("expected NotADatabase, got {other:?}"),
        }
    }

    #[test]
    fn a_file_with_another_magic_is_refused() {
        let (_dir, path) = temp_db();
        std::fs::write(&path, b"not a database at all").unwrap();
        match open(&path) {
            Err(DbError::NotADatabase { .. }) => {}
            other => panic!("expected NotADatabase, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_file_reports_io_not_a_crash() {
        let (_dir, path) = temp_db();
        match open(&path) {
            Err(DbError::Io { .. }) => {}
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn a_newer_schema_is_named_in_the_error() {
        let (_dir, path) = temp_db();
        create(&path, "Future", "").unwrap();
        let conn = connection(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        match open(&path) {
            Err(DbError::NewerSchema { found, supported }) => {
                assert_eq!(found, SCHEMA_VERSION + 1);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("expected NewerSchema, got {other:?}"),
        }
    }
}

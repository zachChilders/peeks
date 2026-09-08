//! The capture log: one SQLite row per photo taken, tying the name the photo was saved
//! under to the position it was taken from.
//!
//! # Why the app has to keep this itself
//!
//! iOS never hands an app a file path for a photo it added to the Photos library, so
//! there is no filename to read back — the camera plugin *assigns* one at save time (see
//! `capturePhoto` in `CameraPlugin.swift`) and returns it, and this module is what
//! remembers it. The coordinates come from a position fix taken at the shutter, not from
//! the AR scene's observer: the scene is deliberately allowed to lag the phone by up to
//! `OBSERVER_STALE_M` (see `src/lib/orientation.ts`), which is fine for deciding which
//! peaks are visible and wrong for saying where a photo was taken.
//!
//! # Why the database lives in the app data directory
//!
//! iCloud Backup takes everything in the app's container *except* `Library/Caches` and
//! `tmp`, plus anything explicitly flagged with `isExcludedFromBackup`. Tauri's
//! `app_data_dir()` resolves on iOS to `<container>/Library/Application Support/<bundle
//! id>`, which is inside that backed-up set, and nothing here sets the exclusion flag —
//! so the log rides along in the device backup and survives a restore onto a new phone.
//!
//! Note the deliberate contrast with [`crate::dem`], which puts its DEM tile cache under
//! `app_cache_dir()` (`Library/Caches`). That data is re-downloadable and has no business
//! inflating a backup. This is neither: once the shutter has fired, the position that
//! photo was taken from cannot be recovered from anywhere else.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;

/// Filename under the app data directory. `.sqlite3` rather than `.db` so that what it
/// is is obvious to whoever eventually pulls it out of a backup.
const DB_FILE: &str = "photos.sqlite3";

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not resolve the app data directory: {0}")]
    Tauri(#[from] tauri::Error),
    #[error("could not create the app data directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("photo log database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// One captured photo and where it was taken from.
///
/// Every field except the name and the coordinates is optional, because every one of them
/// can genuinely be missing on a real capture — and a row with a name and a position is
/// already the thing this log exists to record.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PhotoFix {
    /// The name the photo carries in the Photos library, from the camera plugin.
    pub file_name: String,
    /// The Photos library's handle for the asset (`PHObject.localIdentifier`). Not
    /// required for anything here, but it is the only value that can fetch the photo
    /// itself back, which is what makes a row in this table worth more than a timestamp.
    pub local_identifier: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Altitude as GPS reported it, metres. Deliberately *not* the observer altitude the
    /// AR scene uses — that one is a DEM surface sample plus an assumed eye height, which
    /// is the right number for projecting peaks and a fabricated one to record as "where
    /// this photo was taken". `None` when the fix carried no altitude.
    pub altitude_m: Option<f64>,
    /// Horizontal accuracy radius of the fix, metres. Kept because it is the only thing
    /// that distinguishes a row worth trusting from one recorded off a cold, bad fix.
    pub accuracy_m: Option<f64>,
}

/// Tauri-managed state: the open handle to the capture log, resident once opened.
///
/// A plain `std::sync::Mutex`, unlike [`crate::dem::DemCache`]'s and
/// [`crate::peakstore::PeakStore`]'s tokio ones: rusqlite is a blocking API and every
/// operation here is a single small statement, so there is no `.await` for a guard to be
/// held across.
#[derive(Default)]
pub struct PhotoLog(Mutex<Option<Connection>>);

impl PhotoLog {
    /// Record one capture, opening (and if necessary creating) the database on first use.
    pub fn record<R: tauri::Runtime>(&self, app: &tauri::AppHandle<R>, fix: &PhotoFix) -> Result<()> {
        let mut guard = self.0.lock().expect("photo log mutex poisoned");

        if guard.is_none() {
            let dir = app.path().app_data_dir()?;
            // `Library/Application Support` is not created for an app until something
            // asks for it, so this is not the no-op it looks like on the first launch.
            std::fs::create_dir_all(&dir)?;
            *guard = Some(open(&dir.join(DB_FILE))?);
        }

        let conn = guard.as_ref().expect("just opened above");
        insert(conn, fix, now_ms())?;
        Ok(())
    }
}

/// Open `path`, creating the file and the schema if they are not there yet.
///
/// Left in SQLite's default rollback-journal mode on purpose. WAL is the usual choice,
/// but it leaves `-wal`/`-shm` sidecar files beside the database that a backup can catch
/// part-way through a checkpoint, and with one writer and one small insert per photo
/// there is no concurrency here for it to buy. Being restorable is the entire point of
/// where this file lives — see the module doc.
fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS photos (
             id               INTEGER PRIMARY KEY,
             file_name        TEXT    NOT NULL,
             local_identifier TEXT,
             lat              REAL    NOT NULL,
             lon              REAL    NOT NULL,
             altitude_m       REAL,
             accuracy_m       REAL,
             captured_at_ms   INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS photos_captured_at ON photos (captured_at_ms);",
    )?;
    Ok(conn)
}

/// Insert one row, returning its id.
///
/// `captured_at_ms` is a parameter rather than read from the clock in here so the tests
/// can assert on an exact value.
fn insert(conn: &Connection, fix: &PhotoFix, captured_at_ms: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO photos
             (file_name, local_identifier, lat, lon, altitude_m, accuracy_m, captured_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            fix.file_name,
            fix.local_identifier,
            fix.lat,
            fix.lon,
            fix.altitude_m,
            fix.accuracy_m,
            captured_at_ms,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Milliseconds since the Unix epoch. Stored as an integer rather than a formatted
/// timestamp so the column sorts and compares without a parse; `datetime(captured_at_ms /
/// 1000, 'unixepoch')` renders it for whoever is reading the table.
///
/// A clock behind the epoch (which on iOS means a device that has never had its time set)
/// records as 0 rather than failing the capture: the position is the part worth keeping.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Records one capture against the position it was taken from.
///
/// `async` with nothing awaited in it on purpose: Tauri runs async commands off the main
/// thread, and this does blocking filesystem and SQLite work — the insert's commit is an
/// fsync — that has no business on the thread driving the AR overlay.
#[tauri::command]
#[specta::specta]
pub async fn log_photo(
    app: tauri::AppHandle,
    photo_log: tauri::State<'_, PhotoLog>,
    photo: PhotoFix,
) -> std::result::Result<(), String> {
    photo_log.record(&app, &photo).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix() -> PhotoFix {
        PhotoFix {
            file_name: "Peeks-20260908-143012482.jpg".to_string(),
            local_identifier: Some("A1B2C3D4-0000-1111-2222-333344445555/L0/001".to_string()),
            lat: 46.85287,
            lon: -121.76039,
            altitude_m: Some(4392.0),
            accuracy_m: Some(6.5),
        }
    }

    /// Every column round-trips, including the nullable ones — the whole point of the
    /// table is that what went in comes back out.
    #[test]
    fn insert_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join(DB_FILE)).unwrap();

        let id = insert(&conn, &fix(), 1_757_340_000_000).unwrap();

        let row: (String, Option<String>, f64, f64, Option<f64>, Option<f64>, i64) = conn
            .query_row(
                "SELECT file_name, local_identifier, lat, lon, altitude_m, accuracy_m,
                        captured_at_ms
                 FROM photos WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(row.0, fix().file_name);
        assert_eq!(row.1, fix().local_identifier);
        assert_eq!(row.2, fix().lat);
        assert_eq!(row.3, fix().lon);
        assert_eq!(row.4, fix().altitude_m);
        assert_eq!(row.5, fix().accuracy_m);
        assert_eq!(row.6, 1_757_340_000_000);
    }

    /// A fix with nothing but a name and coordinates is still a row. Photos not handing
    /// back an identifier, or a fix with no altitude, must not cost the capture its
    /// position.
    #[test]
    fn optional_columns_may_be_missing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join(DB_FILE)).unwrap();

        let sparse = PhotoFix {
            local_identifier: None,
            altitude_m: None,
            accuracy_m: None,
            ..fix()
        };
        let id = insert(&conn, &sparse, 0).unwrap();

        let nulls: i64 = conn
            .query_row(
                "SELECT (local_identifier IS NULL) + (altitude_m IS NULL)
                        + (accuracy_m IS NULL)
                 FROM photos WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 3);
    }

    /// Reopening the same file keeps what is already in it: the schema statements are
    /// `IF NOT EXISTS`, so the second launch must not start an empty log. This is the
    /// property the whole "put it where iCloud backs it up" argument rests on — a
    /// database that resets itself on open would restore just as empty.
    #[test]
    fn reopening_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);

        {
            let conn = open(&path).unwrap();
            insert(&conn, &fix(), 1).unwrap();
        }
        let conn = open(&path).unwrap();
        insert(&conn, &fix(), 2).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}

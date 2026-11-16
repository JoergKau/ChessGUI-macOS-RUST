//! Scid `.si4`/`.si5` support behind a small boundary.
//!
//! Scid files are an **interchange format**, never the application's live
//! store: the decoding crate loads all three files into memory and offers no
//! search API, so a Gigabase is converted once into a `.sbd` (SQLite) and the
//! original file is only read, never written.
//!
//! Everything the crate can do is reached through the four functions below, so
//! replacing the decoder later is a one-file change. Nothing outside this
//! module names the crate.
//!
//! The `scid` Cargo feature (on by default) can be switched off; then
//! [`detect`] answers `None` and the UI hides Scid support.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::store::DbError;

/// The two Scid generations we can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScidFormat {
    /// Scid vs PC: `.si4` / `.sn4` / `.sg4`.
    Si4,
    /// Scid 5.x: `.si5` / `.sn5` / `.sg5`.
    Si5,
}

impl ScidFormat {
    /// Human-readable name for the import dialog.
    pub fn label(self) -> &'static str {
        match self {
            ScidFormat::Si4 => "SI4 (Scid vs PC)",
            ScidFormat::Si5 => "SI5 (Scid 5)",
        }
    }
}

/// Detect a Scid database by its file extension.
///
/// The extension is the only reliable hint: the three files of one database
/// share a base name, and the index file's own header is versioned. A wrong or
/// corrupt file is reported by the open call with the decoder's reason.
pub fn detect(path: &Path) -> Option<ScidFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "si4" => Some(ScidFormat::Si4),
        "si5" => Some(ScidFormat::Si5),
        _ => None,
    }
}

/// Number of games in a Scid database, including games marked deleted.
#[cfg(feature = "scid")]
pub fn game_count(path: &Path, format: ScidFormat) -> Result<usize, DbError> {
    match format {
        ScidFormat::Si4 => {
            let paths = chess_scid_rw::Si4Paths::from_index_path(path);
            let db = chess_scid_rw::Si4Database::open(&paths)
                .map_err(|err| DbError::Scid(err.to_string()))?;
            Ok(db.game_count())
        }
        ScidFormat::Si5 => {
            let paths = chess_scid_rw::Si5Paths::from_index_path(path);
            let db = chess_scid_rw::Si5Database::open(&paths)
                .map_err(|err| DbError::Scid(err.to_string()))?;
            Ok(db.game_count())
        }
    }
}

/// Visit every game as PGN text.
///
/// `visit` receives the 0-based game index and the PGN; returning an error
/// stops the walk. The database is opened for the duration of the call and
/// dropped afterwards — its RAM footprint is roughly the size of the three
/// files, so the caller must not keep anything alive beyond this function.
///
/// Returns `Ok(true)` when the user cancelled (the flag is checked between
/// games) and `Ok(false)` when the whole database was visited.
#[cfg(feature = "scid")]
pub fn for_each_game(
    path: &Path,
    format: ScidFormat,
    cancel: &AtomicBool,
    mut visit: impl FnMut(usize, &str) -> Result<(), DbError>,
) -> Result<bool, DbError> {
    match format {
        ScidFormat::Si4 => {
            let paths = chess_scid_rw::Si4Paths::from_index_path(path);
            let db = chess_scid_rw::Si4Database::open(&paths)
                .map_err(|err| DbError::Scid(err.to_string()))?;
            walk(
                db.game_count(),
                cancel,
                |index| db.game_pgn(index),
                &mut visit,
            )
        }
        ScidFormat::Si5 => {
            let paths = chess_scid_rw::Si5Paths::from_index_path(path);
            let db = chess_scid_rw::Si5Database::open(&paths)
                .map_err(|err| DbError::Scid(err.to_string()))?;
            walk(
                db.game_count(),
                cancel,
                |index| db.game_pgn(index),
                &mut visit,
            )
        }
    }
}

/// Shared loop for both formats: decode, hand over, check the cancel flag.
#[cfg(feature = "scid")]
fn walk(
    game_count: usize,
    cancel: &AtomicBool,
    mut decode: impl FnMut(usize) -> Result<String, chess_scid_rw::GameDecodeError>,
    visit: &mut impl FnMut(usize, &str) -> Result<(), DbError>,
) -> Result<bool, DbError> {
    for index in 0..game_count {
        if cancel.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let pgn = match decode(index) {
            Ok(pgn) => pgn,
            Err(err) => {
                // One unreadable game must not end the conversion: the count
                // is reported at the end and the rest of the file is usable.
                log::warn!("Scid game {index} could not be decoded: {err}");
                continue;
            }
        };
        visit(index, &pgn)?;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_by_extension_and_case_insensitive() {
        assert_eq!(detect(Path::new("/tmp/base.si5")), Some(ScidFormat::Si5));
        assert_eq!(detect(Path::new("/tmp/base.SI4")), Some(ScidFormat::Si4));
        assert_eq!(detect(Path::new("/tmp/base.pgn")), None);
        assert_eq!(detect(Path::new("/tmp/base")), None);
    }

    #[cfg(feature = "scid")]
    #[test]
    fn a_generated_si5_round_trips_through_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round.si5");
        let paths = chess_scid_rw::Si5Paths::from_index_path(&path);
        let mut db = chess_scid_rw::Si5Database::create(&paths);
        db.add_game("[Event \"t\"]\n[White \"A\"]\n[Black \"B\"]\n[Result \"1-0\"]\n\n1. e4 e5 2. Nf3 1-0\n")
            .unwrap();
        db.save().unwrap();
        drop(db);

        assert_eq!(game_count(&path, ScidFormat::Si5).unwrap(), 1);
        let mut seen = Vec::new();
        let cancelled = for_each_game(&path, ScidFormat::Si5, &AtomicBool::new(false), |i, pgn| {
            seen.push((i, pgn.to_string()));
            Ok(())
        })
        .unwrap();
        assert!(!cancelled);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].1.contains("1. e4 e5"));
    }
}

//! PGN import: source file → `.sbd` rows.
//!
//! Published collections are stored **verbatim**: the importer splits the file
//! into games with [`PgnStream`], reads the headers, counts the mainline moves
//! and writes the original PGN text plus a dedupe fingerprint. Moves are not
//! validated by default — a game collection that is already in circulation has
//! been through a real engine, and re-validating every move is 20–50× slower
//! (the same conclusion the reference implementations reached). A game that is
//! broken is therefore stored too and reported when the user opens it; the
//! *Validate moves* option turns the opposite behaviour on for hand-kept files.
//!
//! Everything here runs inside a blocking task, one connection per job, and
//! posts progress through the closure the caller passes in. Cancelling is
//! cooperative: the open batch is committed and the games up to that point are
//! kept.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rusqlite::Connection;

use crate::chess::pgn::{self, MovetextScan, PgnResult};

use super::dedupe::{fingerprint, FingerprintFields};
use super::model::{ImportOptions, ImportSummary, JobProgress};
use super::pgn_stream::PgnStream;
use super::store::{self, DbError};

/// Games between progress callbacks (and cancel checks).
const PROGRESS_EVERY: u64 = 100;
/// Games per transaction. A crash or a cancel loses at most one batch.
const COMMIT_EVERY: u64 = 1_000;
/// How many skip reasons the summary keeps for the dialog.
const MAX_REASONS: usize = 5;

/// Import a PGN file into an existing database.
///
/// The database must already exist ([`store::create`]); this writes game rows
/// into it, so the same function serves "new database from PGN", "add games"
/// and "re-import from source".
pub fn import_pgn(
    db_path: &Path,
    source: &Path,
    options: &ImportOptions,
    progress: &mut dyn FnMut(JobProgress),
    cancel: &AtomicBool,
) -> Result<ImportSummary, DbError> {
    let total_bytes = std::fs::metadata(source)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let file = File::open(source).map_err(|source_err| DbError::Io {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut stream = PgnStream::new(reader);

    let mut conn = store::connection(db_path)?;
    let mut cache = NameCache::default();
    let started = Instant::now();
    let mut summary = ImportSummary::default();
    let mut batch: u64 = 0;
    let mut tx = conn.transaction()?;

    while let Some(game) = stream.next() {
        let text = game.map_err(|source_err| DbError::Io {
            path: source.to_path_buf(),
            source: source_err,
        })?;
        match insert_game(&tx, &mut cache, &text, options)? {
            Outcome::Imported => summary.imported += 1,
            Outcome::Duplicate => summary.duplicates += 1,
            Outcome::Skipped(reason) => {
                summary.skipped += 1;
                if summary.reasons.len() < MAX_REASONS {
                    summary.reasons.push(reason);
                }
            }
        }
        batch += 1;
        if batch.is_multiple_of(PROGRESS_EVERY) {
            progress(JobProgress {
                games: summary.imported + summary.duplicates + summary.skipped,
                total_games: None,
                bytes: stream.bytes_read(),
                total_bytes,
            });
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
        }
        if batch.is_multiple_of(COMMIT_EVERY) {
            tx.commit()?;
            tx = conn.transaction()?;
        }
    }
    tx.commit()?;

    summary.elapsed = started.elapsed();
    finish(&conn, summary)
}

/// Scid `.si4`/`.si5` import, behind the `scid` Cargo feature.
#[cfg(feature = "scid")]
pub fn import_scid(
    db_path: &Path,
    source: &Path,
    options: &ImportOptions,
    progress: &mut dyn FnMut(JobProgress),
    cancel: &AtomicBool,
) -> Result<ImportSummary, DbError> {
    use super::scid;

    let format = scid::detect(source).ok_or_else(|| DbError::NotADatabase {
        path: source.to_path_buf(),
    })?;
    let total_bytes = std::fs::metadata(source)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let total_games = scid::game_count(source, format)
        .map(|count| count as u64)
        .ok();

    let conn = store::connection(db_path)?;
    let mut cache = NameCache::default();
    let started = Instant::now();
    let mut summary = ImportSummary::default();
    let mut batch: u64 = 0;

    // A manual BEGIN/COMMIT (rather than `Connection::transaction`) lets the
    // per-game callback commit its own batch: the closure is called many times
    // while it runs, so it cannot hold a `Transaction` borrow across calls.
    conn.execute_batch("BEGIN")?;
    let outcome = scid::for_each_game(source, format, cancel, |_index, game| {
        match insert_game(&conn, &mut cache, game, options)? {
            Outcome::Imported => summary.imported += 1,
            Outcome::Duplicate => summary.duplicates += 1,
            Outcome::Skipped(reason) => {
                summary.skipped += 1;
                if summary.reasons.len() < MAX_REASONS {
                    summary.reasons.push(reason);
                }
            }
        }
        batch += 1;
        if batch.is_multiple_of(PROGRESS_EVERY) {
            progress(JobProgress {
                games: summary.imported + summary.duplicates + summary.skipped,
                total_games,
                bytes: 0,
                total_bytes,
            });
        }
        if batch.is_multiple_of(COMMIT_EVERY) {
            conn.execute_batch("COMMIT; BEGIN")?;
        }
        Ok(())
    });
    conn.execute_batch("COMMIT")?;

    match outcome {
        Ok(cancelled) => summary.cancelled = cancelled,
        Err(err) => {
            summary.reasons.push(err.to_string());
        }
    }
    summary.elapsed = started.elapsed();
    finish(&conn, summary)
}

/// Write the stored game count, tidy the file and hand back the summary.
fn finish(conn: &Connection, summary: ImportSummary) -> Result<ImportSummary, DbError> {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM games", [], |row| row.get(0))?;
    store::set_info(conn, "game_count", &total.to_string())?;
    store::set_info(conn, "last_import_at", &store::unix_now().to_string())?;
    conn.execute_batch("PRAGMA optimize;")?;
    log::info!(
        "database import: {} imported, {} duplicates, {} skipped in {:.1}s",
        summary.imported,
        summary.duplicates,
        summary.skipped,
        summary.elapsed.as_secs_f64()
    );
    Ok(summary)
}

/// What one game did.
enum Outcome {
    /// A new row.
    Imported,
    /// The fingerprint was already present.
    Duplicate,
    /// The game could not be stored; the string is the reason.
    Skipped(String),
}

/// Interned names, so a big import does not resolve the same player or event
/// through SQL again and again.
#[derive(Default)]
struct NameCache {
    players: HashMap<String, i64>,
    events: HashMap<String, i64>,
    sites: HashMap<String, i64>,
}

/// Store one game.
fn insert_game(
    conn: &Connection,
    cache: &mut NameCache,
    text: &str,
    options: &ImportOptions,
) -> Result<Outcome, DbError> {
    let (tags, movetext) = pgn::scan_game_text(text);
    let tag = |name: &str| {
        tags.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty() && *value != "?")
    };

    let scan = pgn::scan_movetext(&movetext);
    if scan.plies == 0 {
        // A header-only or empty game is not worth a row.
        return Ok(Outcome::Skipped("no moves".to_string()));
    }
    if options.validate_moves || options.index_positions {
        // Both need a parsed game; a bad one is reported instead of stored
        // when the caller asked for validation, and skipped either way when
        // the position index was requested.
        if let Err(err) = pgn::parse_pgn_with_report(text) {
            if options.validate_moves {
                return Ok(Outcome::Skipped(err.to_string()));
            }
        }
    }

    let white_name = tag("White").unwrap_or("?");
    let black_name = tag("Black").unwrap_or("?");
    let white_norm = normalize_player(white_name);
    let black_norm = normalize_player(black_name);
    let date_key = tag("Date").map_or(0, parse_date_key);
    let result = tag("Result")
        .and_then(PgnResult::parse)
        .or_else(|| movetext.split_whitespace().rev().find_map(PgnResult::parse))
        .map_or(3, result_code);
    let fields = FingerprintFields {
        white_norm: white_norm.clone(),
        black_norm: black_norm.clone(),
        date_key,
        result,
        ply_count: scan.plies as i64,
        first_moves: scan.first_moves.clone(),
        last_moves: scan.last_moves.clone(),
    };
    let fingerprint = fingerprint(&fields);

    // Dedupe before doing the rest of the work: a large re-import is mostly
    // hits, and each hit should cost one indexed lookup.
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM games WHERE dedupe = ?1)",
        [&fingerprint[..]],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(Outcome::Duplicate);
    }

    let white_id = intern(conn, cache, NameKind::Player, white_name, &white_norm)?;
    let black_id = intern(conn, cache, NameKind::Player, black_name, &black_norm)?;
    let event_id = tag("Event")
        .map(|name| intern(conn, cache, NameKind::Event, name, name))
        .transpose()?;
    let site_id = tag("Site")
        .map(|name| intern(conn, cache, NameKind::Site, name, name))
        .transpose()?;

    let flags = flags(&scan, tag("FEN").is_some());
    conn.execute(
        "INSERT INTO games(
            white_id, black_id, event_id, site_id, round, date_key, result,
            white_elo, black_elo, white_title, black_title, eco, ply_count,
            start_fen, termination, time_control, flags, pgn, dedupe, added_at
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                  ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        rusqlite::params![
            white_id,
            black_id,
            event_id,
            site_id,
            tag("Round"),
            date_key,
            result,
            tag("WhiteElo").and_then(parse_elo),
            tag("BlackElo").and_then(parse_elo),
            tag("WhiteTitle"),
            tag("BlackTitle"),
            tag("ECO").filter(|eco| eco.len() == 3),
            scan.plies as i64,
            tag("FEN"),
            tag("Termination"),
            tag("TimeControl"),
            flags,
            text.as_bytes(),
            &fingerprint[..],
            store::unix_now(),
        ],
    )?;
    let id = conn.last_insert_rowid();

    if options.index_positions {
        index_positions(conn, id, text)?;
    }
    Ok(Outcome::Imported)
}

/// Which name table [`intern`] works on.
enum NameKind {
    Player,
    Event,
    Site,
}

/// Resolve a name to its row id, inserting it when new.
fn intern(
    conn: &Connection,
    cache: &mut NameCache,
    kind: NameKind,
    name: &str,
    norm: &str,
) -> Result<i64, DbError> {
    let key = match kind {
        NameKind::Player => norm,
        NameKind::Event | NameKind::Site => name,
    };
    let table = match kind {
        NameKind::Player => &mut cache.players,
        NameKind::Event => &mut cache.events,
        NameKind::Site => &mut cache.sites,
    };
    if let Some(id) = table.get(key) {
        return Ok(*id);
    }
    let id = match kind {
        NameKind::Player => {
            conn.execute(
                "INSERT INTO players(name, norm) VALUES(?1, ?2)
                 ON CONFLICT(norm) DO NOTHING",
                rusqlite::params![name, norm],
            )?;
            conn.query_row("SELECT id FROM players WHERE norm = ?1", [norm], |row| {
                row.get(0)
            })?
        }
        NameKind::Event => {
            conn.execute(
                "INSERT INTO events(name) VALUES(?1) ON CONFLICT(name) DO NOTHING",
                [name],
            )?;
            conn.query_row("SELECT id FROM events WHERE name = ?1", [name], |row| {
                row.get(0)
            })?
        }
        NameKind::Site => {
            conn.execute(
                "INSERT INTO sites(name) VALUES(?1) ON CONFLICT(name) DO NOTHING",
                [name],
            )?;
            conn.query_row("SELECT id FROM sites WHERE name = ?1", [name], |row| {
                row.get(0)
            })?
        }
    };
    table.insert(key.to_string(), id);
    Ok(id)
}

/// Write the Polyglot key of every position of one game.
///
/// Indexing the whole game (not just the opening) is what lets the Position
/// filter find a FEN taken from any point of a game, including the final
/// position. The cost is one row per half-move, written through a cached
/// statement because a long game has ~100 of them.
fn index_positions(conn: &Connection, id: i64, text: &str) -> Result<(), DbError> {
    let mut game = match pgn::parse_pgn_with_report(text) {
        Ok(import) => import.game,
        Err(_) => return Ok(()),
    };
    conn.execute("DELETE FROM positions WHERE game_id = ?1", [id])?;
    let mut insert = conn
        .prepare_cached("INSERT OR IGNORE INTO positions(hash, game_id, ply) VALUES(?1, ?2, ?3)")?;
    let plies = game.total_plies();
    for ply in 0..=plies {
        game.goto_ply(ply);
        let hash = game.zobrist() as i64;
        insert.execute(rusqlite::params![hash, id, ply as i64])?;
    }
    Ok(())
}

/// Rebuild the position index of every game in an existing database.
///
/// Databases imported before the index covered whole games keep the old,
/// opening-only entries; this job replaces them. Returns the number of games
/// indexed. Cancelling keeps whatever was written so far.
pub fn rebuild_position_index(
    db_path: &Path,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<u64, DbError> {
    let conn = store::connection(db_path)?;
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM games", [], |row| row.get(0))?;
    let total = total.max(0) as u64;
    let mut done = 0u64;
    let mut statement = conn.prepare("SELECT id, pgn FROM games ORDER BY id")?;
    let mut rows = statement.query([])?;
    conn.execute_batch("BEGIN")?;
    while let Some(row) = rows.next()? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let id: i64 = row.get(0)?;
        let pgn: Vec<u8> = row.get(1)?;
        if let Ok(text) = String::from_utf8(pgn) {
            index_positions(&conn, id, &text)?;
        }
        done += 1;
        if done.is_multiple_of(PROGRESS_EVERY) {
            progress(done, total);
        }
        if done.is_multiple_of(COMMIT_EVERY) {
            conn.execute_batch("COMMIT; BEGIN")?;
        }
    }
    conn.execute_batch("COMMIT")?;
    conn.execute_batch("PRAGMA optimize;")?;
    progress(done, total);
    Ok(done)
}

/// Bit set stored in `games.flags`.
fn flags(scan: &MovetextScan, from_fen: bool) -> i64 {
    let mut flags = 0;
    if scan.variations > 0 {
        flags |= 2;
    }
    if scan.comments {
        flags |= 4;
    }
    if scan.nags {
        flags |= 8;
    }
    if from_fen {
        flags |= 16;
    }
    flags
}

/// `games.result` code from a PGN result.
fn result_code(result: PgnResult) -> i64 {
    match result {
        PgnResult::WhiteWin => 0,
        PgnResult::BlackWin => 1,
        PgnResult::Draw => 2,
    }
}

/// `YYYY.MM.DD` → `yyyymmdd`, unknown month/day as `01`, unknown year as `0`.
///
/// Filling the unknown parts with `01` keeps a year search inclusive: a game
/// dated `2024.??.??` matches `2024` instead of falling outside the range.
pub fn parse_date_key(text: &str) -> i64 {
    let mut parts = text.trim().split('.');
    let year = parts
        .next()
        .and_then(|part| part.parse::<i64>().ok())
        .filter(|year| (1000..=9999).contains(year));
    let Some(year) = year else {
        return 0;
    };
    let month = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .filter(|month| (1..=12).contains(month))
        .unwrap_or(1);
    let day = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .filter(|day| (1..=31).contains(day))
        .unwrap_or(1);
    year * 10_000 + i64::from(month) * 100 + i64::from(day)
}

/// Parse a PGN rating; `?`, `-` and anything non-numeric is "unrated".
pub fn parse_elo(text: &str) -> Option<i64> {
    let text = text.trim();
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 5 {
        return None;
    }
    digits.parse::<i64>().ok().filter(|elo| *elo > 0)
}

/// Fold a player name into the searchable form stored in `players.norm`.
///
/// Lowercase, common diacritics folded to ASCII, everything that is not a
/// letter or digit turned into a space, and the remaining tokens sorted — so
/// `Carlsen, Magnus`, `Magnus Carlsen` and `CARLSEN M.` all contain
/// `carlsen` and `magnus`.
pub fn normalize_player(name: &str) -> String {
    let mut folded = String::with_capacity(name.len());
    for ch in name.chars() {
        let mapped = match ch.to_ascii_lowercase() {
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'š' => 's',
            'ž' => 'z',
            'ý' => 'y',
            'ł' => 'l',
            'đ' => 'd',
            'ß' => 's',
            other => other,
        };
        if mapped.is_ascii_alphanumeric() {
            folded.push(mapped);
        } else {
            folded.push(' ');
        }
    }
    let mut tokens: Vec<&str> = folded.split_whitespace().collect();
    tokens.sort_unstable();
    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::model::ImportOptions;

    #[test]
    fn dates_fill_unknown_parts_so_a_year_search_matches() {
        assert_eq!(parse_date_key("2024.05.12"), 20_240_512);
        assert_eq!(parse_date_key("2024.??.??"), 20_240_101);
        assert_eq!(parse_date_key("????.??.??"), 0);
        assert_eq!(parse_date_key(""), 0);
    }

    #[test]
    fn ratings_are_optional_and_ignore_junk() {
        assert_eq!(parse_elo("2750"), Some(2750));
        assert_eq!(parse_elo("?"), None);
        assert_eq!(parse_elo(""), None);
        assert_eq!(parse_elo("-"), None);
        assert_eq!(parse_elo("123456"), None);
    }

    #[test]
    fn player_names_fold_and_sort() {
        assert_eq!(normalize_player("Carlsen, Magnus"), "carlsen magnus");
        assert_eq!(normalize_player("Magnus CARLSEN"), "carlsen magnus");
        assert_eq!(normalize_player("Polgár, Judit"), "judit polgar");
        assert_eq!(
            normalize_player("Nepomniachtchi, Ian"),
            "ian nepomniachtchi"
        );
        assert_eq!(normalize_player(""), "");
    }

    #[test]
    fn a_game_is_stored_with_its_metadata_and_a_repeat_is_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.sbd");
        super::super::store::create(&db, "T", "").unwrap();
        let mut conn = super::super::store::connection(&db).unwrap();
        let mut cache = NameCache::default();
        let text = "[Event \"World Championship\"]\n[Site \"Reykjavik\"]\n[Date \"1972.07.11\"]\n[White \"Spassky, Boris V\"]\n[Black \"Fischer, Robert James\"]\n[Result \"0-1\"]\n[WhiteElo \"2660\"]\n[BlackElo \"2785\"]\n[ECO \"D59\"]\n\n1. d4 Nf6 2. c4 e6 0-1\n";
        let tx = conn.transaction().unwrap();
        assert!(matches!(
            insert_game(&tx, &mut cache, text, &ImportOptions::default()).unwrap(),
            Outcome::Imported
        ));
        assert!(matches!(
            insert_game(&tx, &mut cache, text, &ImportOptions::default()).unwrap(),
            Outcome::Duplicate
        ));
        tx.commit().unwrap();

        let (white, black, date, result, eco): (String, String, i64, i64, String) = conn
            .query_row(
                "SELECT w.name, b.name, g.date_key, g.result, g.eco
                 FROM games g JOIN players w ON w.id = g.white_id
                 JOIN players b ON b.id = g.black_id",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(white, "Spassky, Boris V");
        assert_eq!(black, "Fischer, Robert James");
        assert_eq!(date, 19_720_711);
        assert_eq!(result, 1);
        assert_eq!(eco, "D59");
    }

    #[test]
    fn importing_a_two_game_file_counts_both_and_a_second_run_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.sbd");
        let pgn = dir.path().join("games.pgn");
        std::fs::write(
            &pgn,
            "[Event \"A\"]\n[White \"One\"]\n[Black \"Two\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n[Event \"B\"]\n[White \"Three\"]\n[Black \"Four\"]\n[Result \"0-1\"]\n\n1. d4 d5 0-1\n",
        )
        .unwrap();
        super::super::store::create(&db, "T", "").unwrap();
        let cancel = AtomicBool::new(false);
        let first = import_pgn(&db, &pgn, &ImportOptions::default(), &mut |_| {}, &cancel).unwrap();
        assert_eq!(first.imported, 2);
        let second =
            import_pgn(&db, &pgn, &ImportOptions::default(), &mut |_| {}, &cancel).unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.duplicates, 2);
    }
}

//! SQL for the game table: filtered pages, counts, stats and maintenance.
//!
//! Every user value is a bound parameter — the SQL text itself is assembled
//! only from fixed fragments, so no search field can change the statement's
//! shape or inject SQL. The `WHERE` builder produces one predicate per active
//! filter, which keeps the query readable and lets SQLite use the index that
//! matches the most selective column (`games_white`/`games_black` for a player
//! search, `games_date` for a year range and so on).
//!
//! Paging is `LIMIT/OFFSET` because the browser shows numbered pages; a deep
//! page on an 11 M-row database costs one indexed scan, which the performance
//! budget allows. Counting is a separate query so the table can appear while
//! the total is still being computed.

use rusqlite::types::Value;
use rusqlite::Connection;

use super::import::normalize_player;
use super::model::{
    DbStats, GameFilter, GamePage, GameRow, ResultMask, SideFilter, SortDir, SortKey,
};
use super::store::DbError;

/// One page of the table is this many rows unless a caller says otherwise.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// `%text%` with LIKE's wildcards escaped, so a player called `A_B` is found
/// literally.
fn like_contains(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Build the `WHERE …` clause and its bound values for a filter.
///
/// The `g` alias is the games table; sub-selects reach `players` and `events`
/// by name. `1=1` is the always-true seed, so appending conditions never needs
/// a special case.
///
/// The player and Elo fields are coupled:
///
/// * with a player name, the Elo range is that player's rating on whichever
///   side they played (`Carlsen, both sides, 2800–2900` = Carlsen games where
///   *he* was rated that high);
/// * without a player name, `Both sides` (the `Either` variant) requires **both**
///   ratings in the window — a 2058–1600 game is not a match for `max 1800`;
///   `White`/`Black` restrict it to that one side's rating.
fn build_where(filter: &GameFilter) -> (String, Vec<Value>) {
    let mut parts: Vec<String> = vec!["1=1".to_string()];
    let mut values: Vec<Value> = Vec::new();

    if !filter.include_deleted {
        parts.push("(g.flags & 1) = 0".to_string());
    }

    let player = normalize_player(&filter.player);
    let player = (!player.is_empty()).then(|| like_contains(&player));
    let elo = (filter.elo_min.is_some() || filter.elo_max.is_some()).then(|| {
        (
            i64::from(filter.elo_min.unwrap_or(0)),
            i64::from(filter.elo_max.unwrap_or(u16::MAX)),
        )
    });

    let (white, white_values) = side_condition("g.white_id", "g.white_elo", &player, elo);
    let (black, black_values) = side_condition("g.black_id", "g.black_elo", &player, elo);
    match filter.side {
        SideFilter::Either => {
            if player.is_some() {
                // A named player: the range is that player's own rating, on
                // whichever side they had it.
                match (white.is_empty(), black.is_empty()) {
                    (true, true) => {}
                    (false, true) => {
                        parts.push(white);
                        values.extend(white_values);
                    }
                    (true, false) => {
                        parts.push(black);
                        values.extend(black_values);
                    }
                    (false, false) => {
                        parts.push(format!("(({white}) OR ({black}))"));
                        values.extend(white_values);
                        values.extend(black_values);
                    }
                }
            } else if let Some((min, max)) = elo {
                // No player named: "both sides" means both ratings must fall
                // inside the window. A game rated 2058–1600 is *not* a match
                // for "max 1800", even though one of its players is.
                parts.push(
                    "(g.white_elo IS NOT NULL AND g.white_elo BETWEEN ? AND ?
                      AND g.black_elo IS NOT NULL AND g.black_elo BETWEEN ? AND ?)"
                        .to_string(),
                );
                values.extend([
                    Value::Integer(min),
                    Value::Integer(max),
                    Value::Integer(min),
                    Value::Integer(max),
                ]);
            }
        }
        SideFilter::White => {
            if !white.is_empty() {
                parts.push(white);
                values.extend(white_values);
            }
        }
        SideFilter::Black => {
            if !black.is_empty() {
                parts.push(black);
                values.extend(black_values);
            }
        }
    }

    let event = filter.event.trim();
    if !event.is_empty() {
        parts.push(
            "g.event_id IN (SELECT id FROM events WHERE name LIKE ? ESCAPE '\\')".to_string(),
        );
        values.push(Value::Text(like_contains(event)));
    }

    if !filter.results.any() {
        // No result ticked: match nothing, which is what the checkboxes mean.
        parts.push("0=1".to_string());
    } else if filter.results != ResultMask::default() {
        let codes = filter.results.codes();
        let placeholders = vec!["?"; codes.len()].join(", ");
        parts.push(format!("g.result IN ({placeholders})"));
        values.extend(codes.into_iter().map(Value::Integer));
    }

    if let Some(from) = filter.year_from {
        parts.push("g.date_key >= ?".to_string());
        values.push(Value::Integer(i64::from(from) * 10_000 + 101));
    }
    if let Some(to) = filter.year_to {
        parts.push("g.date_key <= ?".to_string());
        values.push(Value::Integer(i64::from(to) * 10_000 + 1231));
    }

    if let Some(position) = filter.position {
        parts.push(
            "EXISTS (SELECT 1 FROM positions p WHERE p.game_id = g.id AND p.hash = ?)".to_string(),
        );
        values.push(Value::Integer(position as i64));
    }

    (format!("WHERE {}", parts.join(" AND ")), values)
}

/// One side's player/Elo condition, appending its bound values in the order
/// the placeholders appear in the returned fragment.
fn side_condition(
    player_column: &str,
    elo_column: &str,
    player: &Option<String>,
    elo: Option<(i64, i64)>,
) -> (String, Vec<Value>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    if let Some(pattern) = player {
        conditions.push(format!(
            "{player_column} IN (SELECT id FROM players WHERE norm LIKE ? ESCAPE '\\')"
        ));
        values.push(Value::Text(pattern.clone()));
    }
    if let Some((min, max)) = elo {
        conditions.push(format!(
            "({elo_column} IS NOT NULL AND {elo_column} BETWEEN ? AND ?)"
        ));
        values.push(Value::Integer(min));
        values.push(Value::Integer(max));
    }
    (conditions.join(" AND "), values)
}

/// The SQL column a [`SortKey`] sorts by.
fn sort_column(key: SortKey) -> &'static str {
    match key {
        SortKey::Id => "g.id",
        SortKey::Date => "g.date_key",
        SortKey::WhiteElo => "g.white_elo",
        SortKey::BlackElo => "g.black_elo",
        SortKey::PlyCount => "g.ply_count",
        SortKey::WhiteName => "w.name",
        SortKey::BlackName => "b.name",
    }
}

/// The `games.result` code as the PGN token.
fn result_token(code: i64) -> String {
    match code {
        0 => "1-0",
        1 => "0-1",
        2 => "1/2-1/2",
        _ => "*",
    }
    .to_string()
}

/// Whether the database has any position rows.
fn has_position_index(conn: &Connection) -> Result<bool, DbError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM positions LIMIT 1)",
        [],
        |row| row.get(0),
    )?;
    Ok(exists)
}

/// Refuse a position filter on a database that was imported without the index,
/// instead of silently returning nothing.
fn check_position_index(conn: &Connection, filter: &GameFilter) -> Result<(), DbError> {
    if filter.position.is_some() && !has_position_index(conn)? {
        return Err(DbError::NoPositionIndex);
    }
    Ok(())
}

/// One filtered page of the game table.
///
/// `page` is zero-based. One extra row is fetched so the caller knows whether
/// another page follows without a second count.
pub fn page(
    conn: &Connection,
    filter: &GameFilter,
    sort: SortKey,
    direction: SortDir,
    page: usize,
    page_size: usize,
) -> Result<GamePage, DbError> {
    check_position_index(conn, filter)?;
    let (where_sql, values) = build_where(filter);
    let sql = format!(
        "SELECT g.id, w.name, b.name, g.date_key, g.result, COALESCE(e.name, ''),
                g.white_elo, g.black_elo, g.ply_count, g.flags, COALESCE(g.eco, '')
         FROM games g
         JOIN players w ON w.id = g.white_id
         JOIN players b ON b.id = g.black_id
         LEFT JOIN events e ON e.id = g.event_id
         {where_sql}
         ORDER BY {column} {dir}, g.id {dir}
         LIMIT ? OFFSET ?",
        column = sort_column(sort),
        dir = direction.sql()
    );
    let mut all = values;
    all.push(Value::Integer(page_size as i64 + 1));
    all.push(Value::Integer((page * page_size) as i64));

    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(all), |row| {
        Ok(GameRow {
            id: row.get(0)?,
            white: row.get(1)?,
            black: row.get(2)?,
            date_key: row.get(3)?,
            result: result_token(row.get::<_, i64>(4)?),
            event: row.get(5)?,
            white_elo: row.get(6)?,
            black_elo: row.get(7)?,
            ply_count: row.get(8)?,
            flags: row.get(9)?,
            eco: row.get(10)?,
        })
    })?;
    let mut collected: Vec<GameRow> = rows.collect::<Result<_, _>>()?;
    let has_more = collected.len() > page_size;
    collected.truncate(page_size);
    Ok(GamePage {
        rows: collected,
        has_more,
    })
}

/// How many games the filter matches. Run as its own job: on a large database
/// this takes seconds and must not hold up the first page.
pub fn count(conn: &Connection, filter: &GameFilter) -> Result<u64, DbError> {
    check_position_index(conn, filter)?;
    let (where_sql, values) = build_where(filter);
    let sql = format!("SELECT COUNT(*) FROM games g {where_sql}");
    let total: i64 = conn.query_row(&sql, rusqlite::params_from_iter(values), |row| row.get(0))?;
    Ok(total.max(0) as u64)
}

/// Stored player names matching every token the user has typed so far.
///
/// The browser's Player field uses this for its autocomplete list. Matching is
/// the same folded form the search uses (`normalize_player`), but one
/// condition per token, so the tokens may be typed in any order and may be
/// fragments: `magnus carl` finds `Carlsen, Magnus` before the word is
/// complete. Names come back alphabetically; `limit` keeps the popup short.
pub fn player_suggestions(
    conn: &Connection,
    fragment: &str,
    limit: usize,
) -> Result<Vec<String>, DbError> {
    let norm = normalize_player(fragment);
    if norm.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let patterns: Vec<String> = norm.split_whitespace().map(like_contains).collect();
    let conditions = vec!["norm LIKE ? ESCAPE '\\'"; patterns.len()].join(" AND ");
    let sql = format!(
        "SELECT name FROM players
         WHERE {conditions}
         ORDER BY name COLLATE NOCASE
         LIMIT ?"
    );
    let mut values: Vec<Value> = patterns.into_iter().map(Value::Text).collect();
    values.push(Value::Integer(limit as i64));
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values), |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The counts the overview screen shows.
pub fn stats(conn: &Connection) -> Result<DbStats, DbError> {
    let games: i64 = conn.query_row(
        "SELECT COUNT(*) FROM games WHERE (flags & 1) = 0",
        [],
        |row| row.get(0),
    )?;
    // Players and events are the name tables: every name ever imported. That
    // is the number the user expects to see and it is a single COUNT.
    let players: i64 = conn.query_row("SELECT COUNT(*) FROM players", [], |row| row.get(0))?;
    let events: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    Ok(DbStats {
        games: games.max(0) as u64,
        players: players.max(0) as u64,
        events: events.max(0) as u64,
    })
}

/// The stored PGN text of one game.
pub fn load_game(conn: &Connection, id: i64) -> Result<Option<Vec<u8>>, DbError> {
    let mut statement = conn.prepare("SELECT pgn FROM games WHERE id = ?1")?;
    let mut rows = statement.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Delete the later copy of every game that differs in formatting only.
///
/// The import dedupe key already stops identical games; this cleanup catches
/// the same pairing, date, result and move count imported from two different
/// sources, keeping the lowest id (the first import).
pub fn remove_duplicates(conn: &Connection) -> Result<u64, DbError> {
    let removed = conn.execute(
        "DELETE FROM games WHERE id NOT IN (
             SELECT MIN(id) FROM games
             GROUP BY white_id, black_id, date_key, result, ply_count
         )",
        [],
    )?;
    Ok(removed as u64)
}

/// Delete games that have no moves at all.
pub fn remove_empty_games(conn: &Connection) -> Result<u64, DbError> {
    let removed = conn.execute("DELETE FROM games WHERE ply_count = 0", [])?;
    Ok(removed as u64)
}

/// Write the games the filter matches as one PGN file.
///
/// Games are read in chunks so a whole-database export never holds more than a
/// few hundred PGN texts at a time. Returns the number written.
pub fn export_pgn(
    conn: &Connection,
    filter: &GameFilter,
    sort: SortKey,
    direction: SortDir,
    target: &std::path::Path,
    progress: &mut dyn FnMut(u64),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<u64, DbError> {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    let (where_sql, values) = build_where(filter);
    let sql = format!(
        "SELECT g.id, g.pgn FROM games g
         JOIN players w ON w.id = g.white_id
         JOIN players b ON b.id = g.black_id
         {where_sql}
         ORDER BY {column} {dir}, g.id {dir}
         LIMIT ? OFFSET ?",
        column = sort_column(sort),
        dir = direction.sql()
    );
    let file = std::fs::File::create(target).map_err(|source| DbError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut written = 0u64;
    let mut offset = 0usize;
    loop {
        let mut chunk = values.clone();
        chunk.push(Value::Integer(500));
        chunk.push(Value::Integer(offset as i64));
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(chunk))?;
        let mut any = false;
        while let Some(row) = rows.next()? {
            any = true;
            let pgn: Vec<u8> = row.get(1)?;
            writer.write_all(&pgn).map_err(|source| DbError::Io {
                path: target.to_path_buf(),
                source,
            })?;
            writer.write_all(b"\n\n").map_err(|source| DbError::Io {
                path: target.to_path_buf(),
                source,
            })?;
            written += 1;
            if written.is_multiple_of(500) {
                progress(written);
            }
            if cancel.load(Ordering::Relaxed) {
                writer.flush().ok();
                return Ok(written);
            }
        }
        if !any {
            break;
        }
        offset += 500;
    }
    writer.flush().map_err(|source| DbError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    progress(written);
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::model::ImportOptions;
    use crate::database::{import, store};
    use std::sync::atomic::AtomicBool;

    /// Build a database with three games and return its path in a temp dir.
    fn sample_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let db = dir.path().join("games.sbd");
        let pgn = dir.path().join("games.pgn");
        std::fs::write(
            &pgn,
            "[Event \"Alpha\"]\n[White \"Carlsen, Magnus\"]\n[Black \"Nepomniachtchi, Ian\"]\n[Date \"2021.12.03\"]\n[Result \"1/2-1/2\"]\n[WhiteElo \"2855\"]\n[BlackElo \"2782\"]\n[ECO \"C88\"]\n\n1. e4 e5 2. Nf3 Nc6 1/2-1/2\n\n\
             [Event \"Beta\"]\n[White \"Ding, Liren\"]\n[Black \"Carlsen, Magnus\"]\n[Date \"2023.04.30\"]\n[Result \"0-1\"]\n[WhiteElo \"2788\"]\n[BlackElo \"2853\"]\n[ECO \"D02\"]\n\n1. d4 d5 2. c4 e6 0-1\n\n\
             [Event \"Gamma\"]\n[White \"Caruana, Fabiano\"]\n[Black \"Ding, Liren\"]\n[Date \"2024.??.??\"]\n[Result \"1-0\"]\n\n1. e4 c5 2. Nf3 d6 1-0\n",
        )
        .unwrap();
        store::create(&db, "Test", "").unwrap();
        import::import_pgn(
            &db,
            &pgn,
            &ImportOptions::default(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        db
    }

    fn load(db: &std::path::Path) -> Connection {
        store::connection(db).unwrap()
    }

    #[test]
    fn the_stats_count_games_players_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        assert_eq!(
            stats(&conn).unwrap(),
            DbStats {
                games: 3,
                players: 4,
                events: 3
            }
        );
    }

    #[test]
    fn a_player_search_matches_either_colour_and_a_name_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        let all = GameFilter::default();
        let mut filter = all.clone();
        filter.player = "carlsen".to_string();
        assert_eq!(count(&conn, &filter).unwrap(), 2);
        filter.side = SideFilter::White;
        assert_eq!(count(&conn, &filter).unwrap(), 1);
        filter.side = SideFilter::Black;
        assert_eq!(count(&conn, &filter).unwrap(), 1);

        // "Magnus Carlsen" works even though the file writes "Carlsen, Magnus".
        let mut filter = all.clone();
        filter.player = "Magnus Carlsen".to_string();
        assert_eq!(count(&conn, &filter).unwrap(), 2);
    }

    #[test]
    fn player_suggestions_match_each_typed_token_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        assert_eq!(
            player_suggestions(&conn, "carl", 10).unwrap(),
            vec!["Carlsen, Magnus".to_string()]
        );
        assert_eq!(
            player_suggestions(&conn, "magnus carl", 10).unwrap(),
            vec!["Carlsen, Magnus".to_string()],
            "the tokens may be typed in either order and as fragments"
        );
        assert_eq!(
            player_suggestions(&conn, "ding", 10).unwrap(),
            vec!["Ding, Liren".to_string()]
        );
        assert!(player_suggestions(&conn, "   ", 10).unwrap().is_empty());
        assert!(player_suggestions(&conn, "carl", 0).unwrap().is_empty());
    }

    #[test]
    fn a_player_filter_couples_the_elo_range_to_that_player() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);

        // Carlsen is 2855 as White and 2853 as Black in the fixture.
        let carlsen = |elo_min, elo_max| GameFilter {
            player: "carlsen".to_string(),
            side: SideFilter::Either,
            elo_min: Some(elo_min),
            elo_max: Some(elo_max),
            ..GameFilter::default()
        };
        assert_eq!(count(&conn, &carlsen(2850, 2900)).unwrap(), 2);
        // His opponents were 2782 and 2788 — not in this window, and the
        // range is about *his* rating, not theirs.
        assert_eq!(count(&conn, &carlsen(2770, 2790)).unwrap(), 0);
        // Without a player name, both ratings must be inside the window; the
        // 2855–2782 and 2788–2853 games do not fit 2770–2790.
        let both = GameFilter {
            elo_min: Some(2770),
            elo_max: Some(2790),
            ..GameFilter::default()
        };
        assert_eq!(count(&conn, &both).unwrap(), 0);
        let both = GameFilter {
            elo_min: Some(2780),
            elo_max: Some(2860),
            ..GameFilter::default()
        };
        assert_eq!(count(&conn, &both).unwrap(), 2);
    }

    #[test]
    fn elo_event_result_and_year_filters_combine() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        let base = GameFilter::default();

        // With no player name, "both sides" means *both* ratings in the
        // window: 2855–2782 and 2788–2853 both fit 2780–2860.
        let mut filter = base.clone();
        filter.elo_min = Some(2780);
        filter.elo_max = Some(2860);
        assert_eq!(count(&conn, &filter).unwrap(), 2, "both ratings in range");

        // One side only: 2855 (White of game 1) and 2853 (Black of game 2).
        let mut filter = base.clone();
        filter.side = SideFilter::White;
        filter.elo_min = Some(2850);
        filter.elo_max = Some(2860);
        assert_eq!(count(&conn, &filter).unwrap(), 1);
        filter.side = SideFilter::Black;
        assert_eq!(count(&conn, &filter).unwrap(), 1);

        let mut filter = base.clone();
        filter.event = "beta".to_string();
        assert_eq!(count(&conn, &filter).unwrap(), 1);

        let mut filter = base.clone();
        filter.results = ResultMask {
            white: true,
            draw: false,
            black: false,
        };
        assert_eq!(count(&conn, &filter).unwrap(), 1);

        let mut filter = base.clone();
        filter.year_from = Some(2024);
        filter.year_to = Some(2024);
        assert_eq!(
            count(&conn, &filter).unwrap(),
            1,
            "the unknown month/day still matches its year"
        );
    }

    #[test]
    fn a_page_marks_that_more_rows_follow() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        let filter = GameFilter::default();
        let first = page(&conn, &filter, SortKey::Date, SortDir::Desc, 0, 2).unwrap();
        assert_eq!(first.rows.len(), 2);
        assert!(first.has_more);
        assert_eq!(first.rows[0].date_key, 20_240_101);
        let second = page(&conn, &filter, SortKey::Date, SortDir::Desc, 1, 2).unwrap();
        assert_eq!(second.rows.len(), 1);
        assert!(!second.has_more);
    }

    #[test]
    fn loading_a_game_returns_the_stored_text() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        let page = page(
            &conn,
            &GameFilter::default(),
            SortKey::Id,
            SortDir::Asc,
            0,
            10,
        )
        .unwrap();
        let text = load_game(&conn, page.rows[0].id).unwrap().unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("[White \"Carlsen, Magnus\"]"));
    }

    #[test]
    fn removing_duplicates_keeps_the_first_copy() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        // Insert a second copy of game one with different move text but the
        // same pairing, date, result and ply count.
        conn.execute(
            "INSERT INTO games(white_id, black_id, event_id, date_key, result,
                ply_count, flags, pgn, dedupe, added_at)
             SELECT white_id, black_id, event_id, date_key, result, ply_count, flags,
                    pgn, randomblob(8), added_at FROM games WHERE id = 1",
            [],
        )
        .unwrap();
        assert_eq!(remove_duplicates(&conn).unwrap(), 1);
        assert_eq!(stats(&conn).unwrap().games, 3);
    }

    #[test]
    fn empty_games_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        conn.execute(
            "INSERT INTO games(white_id, black_id, ply_count, flags, pgn, dedupe, added_at)
             VALUES(1, 1, 0, 0, X'00', randomblob(8), 0)",
            [],
        )
        .unwrap();
        assert_eq!(remove_empty_games(&conn).unwrap(), 1);
    }

    #[test]
    fn the_position_filter_needs_an_index_and_matches_indexed_games() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("positions.sbd");
        let pgn = dir.path().join("games.pgn");
        std::fs::write(
            &pgn,
            "[Event \"A\"]\n[White \"One\"]\n[Black \"Two\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n\
             [Event \"B\"]\n[White \"Three\"]\n[Black \"Four\"]\n[Result \"0-1\"]\n\n1. d4 d5 0-1\n",
        )
        .unwrap();
        store::create(&db, "Positions", "").unwrap();
        let indexed = GameFilter {
            position: Some(0x463b_9618_1691_fc9c),
            ..GameFilter::default()
        };
        let conn = load(&db);
        assert!(
            matches!(
                count(&conn, &indexed),
                Err(crate::database::DbError::NoPositionIndex)
            ),
            "an unindexed database must say so, not return nothing"
        );
        drop(conn);

        import::import_pgn(
            &db,
            &pgn,
            &ImportOptions {
                validate_moves: false,
                index_positions: true,
            },
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let conn = load(&db);
        // The start position is in both games.
        assert_eq!(count(&conn, &indexed).unwrap(), 2);
        // After 1.e4 only the first game has it.
        let after_e4 = GameFilter {
            position: Some(crate::chess::pgn::parse_pgn("1. e4 *").unwrap().zobrist()),
            ..GameFilter::default()
        };
        assert_eq!(count(&conn, &after_e4).unwrap(), 1);
    }

    #[test]
    fn exporting_writes_every_matching_game() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample_db(&dir);
        let conn = load(&db);
        let target = dir.path().join("out.pgn");
        let written = export_pgn(
            &conn,
            &GameFilter::default(),
            SortKey::Id,
            SortDir::Asc,
            &target,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(written, 3);
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text.matches("[Event ").count(), 3);
    }
}

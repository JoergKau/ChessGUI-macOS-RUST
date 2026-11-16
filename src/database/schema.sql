-- SiliconBoard database schema, version 1.
--
-- One `.sbd` file is one SQLite database holding many chess games. The games
-- themselves are stored as the verbatim PGN text they were imported from, so
-- comments, variations, NAGs and unknown tags survive; the columns beside it
-- exist only for filtering and for the game table.
--
-- Column notes:
--   * `date_key` is `yyyymmdd` with unknown month/day filled as 01/01 and
--     `0` when the year itself is unknown, so a year range can be compared as
--     plain integers.
--   * `result` is 0 white wins, 1 black wins, 2 draw, 3 unknown.
--   * `flags` is a bit set: 1 deleted, 2 has variations, 4 has comments,
--     8 has NAGs, 16 starts from a FEN position.
--   * `pgn` is a BLOB so a later schema can compress it (codec column) without
--     rewriting the table.
--   * `dedupe` is an 8-byte fingerprint (see `dedupe.rs`); the unique index is
--     what makes a repeated import a no-op.
--   * `positions` is filled only when the import asked for position indexing;
--     it maps a Polyglot key to the game and ply it occurs at.

CREATE TABLE db_info (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE players (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL,             -- as written: "Carlsen, Magnus"
  norm TEXT NOT NULL UNIQUE       -- search form: "carlsen magnus"
);

CREATE TABLE events (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE sites  (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);

CREATE TABLE games (
  id          INTEGER PRIMARY KEY,             -- insertion order, 1-based
  white_id    INTEGER NOT NULL REFERENCES players(id),
  black_id    INTEGER NOT NULL REFERENCES players(id),
  event_id    INTEGER          REFERENCES events(id),
  site_id     INTEGER          REFERENCES sites(id),
  round       TEXT,
  date_key    INTEGER NOT NULL DEFAULT 0,      -- yyyymmdd, 0 = unknown
  result      INTEGER NOT NULL DEFAULT 3,      -- 0 white, 1 black, 2 draw, 3 unknown
  white_elo   INTEGER,
  black_elo   INTEGER,
  white_title TEXT,
  black_title TEXT,
  eco         TEXT,                            -- "B90"
  ply_count   INTEGER NOT NULL DEFAULT 0,      -- mainline half-moves, not validated
  start_fen   TEXT,                            -- NULL = standard start
  termination TEXT,
  time_control TEXT,
  flags       INTEGER NOT NULL DEFAULT 0,
  pgn         BLOB    NOT NULL,                -- verbatim PGN of this game
  dedupe      BLOB    NOT NULL,                -- 8-byte fingerprint
  added_at    INTEGER NOT NULL                 -- unix seconds
);

CREATE INDEX games_white  ON games(white_id, date_key);
CREATE INDEX games_black  ON games(black_id, date_key);
CREATE INDEX games_date   ON games(date_key);
CREATE INDEX games_result ON games(result, date_key);
CREATE INDEX games_eco    ON games(eco, date_key);
CREATE INDEX games_welo   ON games(white_elo);
CREATE INDEX games_belo   ON games(black_elo);
CREATE UNIQUE INDEX games_dedupe ON games(dedupe);

CREATE TABLE positions (
  hash    INTEGER NOT NULL,                    -- Polyglot key, u64 as i64
  game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
  ply     INTEGER NOT NULL,
  PRIMARY KEY (hash, game_id, ply)
) WITHOUT ROWID;

CREATE INDEX positions_game ON positions(game_id);

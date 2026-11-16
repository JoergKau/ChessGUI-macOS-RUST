//! Data types shared by the database store, its queries and the UI.
//!
//! Nothing in this module touches SQLite; [`store`](super::store) and
//! [`query`](super::query) turn these structures into statements. Keeping the
//! types separate means the browser window can be laid out and tested without
//! a database file.

use std::time::Duration;

/// A database as the UI shows it: identity plus the headline counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseInfo {
    /// The `.sbd` file.
    pub path: std::path::PathBuf,
    /// `db_info.title` — the name the user gave it.
    pub title: String,
    /// `db_info.description`, empty when none was given.
    pub description: String,
    /// Games in the file, including games marked deleted.
    pub game_count: u64,
    /// Unix seconds when the file was created.
    pub created_at: i64,
}

/// Counts shown on the database overview screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DbStats {
    /// Games that are not marked deleted.
    pub games: u64,
    /// Distinct player names used by those games.
    pub players: u64,
    /// Distinct event names used by those games.
    pub events: u64,
}

/// Which side the player and Elo filters apply to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SideFilter {
    /// Both sides: the player name may match either player, and an Elo range
    /// must hold for **both** players of the game.
    #[default]
    Either,
    /// Only the White player.
    White,
    /// Only the Black player.
    Black,
}

impl SideFilter {
    /// Label for the UI selector.
    pub fn label(self) -> &'static str {
        match self {
            SideFilter::Either => "Both sides",
            SideFilter::White => "White",
            SideFilter::Black => "Black",
        }
    }
}

/// The result values a search may return. All three are on by default; a
/// filter with none selected matches nothing, which is what the checkboxes
/// then say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultMask {
    /// `1-0`.
    pub white: bool,
    /// `1/2-1/2`.
    pub draw: bool,
    /// `0-1`.
    pub black: bool,
}

impl Default for ResultMask {
    fn default() -> Self {
        Self {
            white: true,
            draw: true,
            black: true,
        }
    }
}

impl ResultMask {
    /// Whether at least one result is selected.
    pub fn any(self) -> bool {
        self.white || self.draw || self.black
    }

    /// The `games.result` codes this mask allows (0 white, 1 black, 2 draw).
    pub fn codes(self) -> Vec<i64> {
        let mut codes = Vec::new();
        if self.white {
            codes.push(0);
        }
        if self.draw {
            codes.push(2);
        }
        if self.black {
            codes.push(1);
        }
        codes
    }
}

/// One search. An empty field means "no restriction"; the UI keeps the raw
/// text and [`Default`] is the unfiltered search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameFilter {
    /// Substring of the player name, matched against the normalised form
    /// (`carlsen magnus`), so "Carlsen, M." is found by `carlsen m`.
    pub player: String,
    /// Which side [`player`](Self::player) applies to.
    pub side: SideFilter,
    /// Inclusive Elo window; `None` bounds are ignored.
    pub elo_min: Option<u16>,
    /// Upper inclusive Elo bound.
    pub elo_max: Option<u16>,
    /// Substring of the event name.
    pub event: String,
    /// Results to include.
    pub results: ResultMask,
    /// First year (inclusive), when set.
    pub year_from: Option<i32>,
    /// Last year (inclusive), when set.
    pub year_to: Option<i32>,
    /// Polyglot key of a position the game must have contained.
    pub position: Option<u64>,
    /// Include games marked deleted.
    pub include_deleted: bool,
}

/// A column the game table can be sorted by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    /// Insertion order (the default: newest import last).
    #[default]
    Id,
    /// `date_key`.
    Date,
    /// White's rating.
    WhiteElo,
    /// Black's rating.
    BlackElo,
    /// Number of half-moves.
    PlyCount,
    /// White's name.
    WhiteName,
    /// Black's name.
    BlackName,
}

/// Ascending or descending.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortDir {
    /// Smallest first.
    Asc,
    /// Largest first.
    #[default]
    Desc,
}

impl SortDir {
    /// The SQL keyword.
    pub fn sql(self) -> &'static str {
        match self {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        }
    }
}

/// One row of the game table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRow {
    /// `games.id`, used to load the game.
    pub id: i64,
    /// White player as written.
    pub white: String,
    /// Black player as written.
    pub black: String,
    /// White's rating, when the PGN had one.
    pub white_elo: Option<i64>,
    /// Black's rating, when the PGN had one.
    pub black_elo: Option<i64>,
    /// `yyyymmdd`; 0 when the year is unknown.
    pub date_key: i64,
    /// `1-0`, `0-1`, `1/2-1/2` or `*`.
    pub result: String,
    /// Event name, empty when the PGN had none.
    pub event: String,
    /// ECO code, empty when the PGN had none.
    pub eco: String,
    /// Mainline half-moves.
    pub ply_count: i64,
    /// Bit 0 is the deleted flag, bit 1 variations, bit 2 comments, bit 3 NAGs.
    pub flags: i64,
}

impl GameRow {
    /// `2024.05.12`, `2024.??.??` or `????.??.??` as a PGN would show it.
    pub fn date_text(&self) -> String {
        date_key_text(self.date_key)
    }
}

/// Format a `yyyymmdd` key the way PGN does (`2024.05.12`).
pub fn date_key_text(date_key: i64) -> String {
    if date_key <= 0 {
        return "????.??.??".to_string();
    }
    let year = date_key / 10_000;
    let month = (date_key / 100) % 100;
    let day = date_key % 100;
    format!("{year:04}.{month:02}.{day:02}")
}

/// One page of the game table.
#[derive(Clone, Debug, Default)]
pub struct GamePage {
    /// Rows for the requested page, in the requested order.
    pub rows: Vec<GameRow>,
    /// Whether another page follows.
    pub has_more: bool,
}

/// What an import did, for the status line and the dialog.
#[derive(Clone, Debug, Default)]
pub struct ImportSummary {
    /// Games written to the database.
    pub imported: u64,
    /// Games already present (same dedupe fingerprint).
    pub duplicates: u64,
    /// Games that could not be read (only with move validation, or a Scid
    /// decode error).
    pub skipped: u64,
    /// Wall-clock time the import took.
    pub elapsed: Duration,
    /// The user cancelled; the games up to that point are kept.
    pub cancelled: bool,
    /// First few reasons a game was skipped, for the dialog's details line.
    pub reasons: Vec<String>,
}

/// Progress of a running database job (import, export, index build).
#[derive(Clone, Copy, Debug, Default)]
pub struct JobProgress {
    /// Games processed so far.
    pub games: u64,
    /// Total games, when known up front (Scid knows it, PGN does not).
    pub total_games: Option<u64>,
    /// Source bytes consumed.
    pub bytes: u64,
    /// Source size in bytes (0 when unknown).
    pub total_bytes: u64,
}

impl JobProgress {
    /// Fraction done in `0.0..=1.0`, preferring the game count when known.
    pub fn fraction(&self) -> f32 {
        if let Some(total) = self.total_games.filter(|total| *total > 0) {
            return (self.games as f32 / total as f32).clamp(0.0, 1.0);
        }
        if self.total_bytes > 0 {
            return (self.bytes as f32 / self.total_bytes as f32).clamp(0.0, 1.0);
        }
        0.0
    }
}

/// How an import was asked to behave.
#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    /// Parse and validate every move before storing the game. Slow (20–50×);
    /// off for published collections, whose moves are already trustworthy.
    pub validate_moves: bool,
    /// Build the Polyglot position index for every position of each imported
    /// game, so the Position filter can find any FEN the game went through.
    pub index_positions: bool,
}

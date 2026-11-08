//! Engine tournaments: configuration, game scheduling, EPD opening books,
//! adjudication and progress reporting.
//!
//! The pure parts live here so they can be tested without an engine process.
//! The application drives the games on its normal board: one game at a time,
//! with the move list, clocks and engine cards live. The behaviour covers what
//! a two-engine match needs: colours alternate, an
//! EPD file supplies the starting positions, `resign`/`draw` adjudication uses
//! the engines' own scores, and the PGN file receives one game per round.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use shakmaty::uci::UciMove;
use shakmaty::zobrist::Zobrist64;
use shakmaty::{CastlingMode, EnPassantMode, Move, Position, Role};

use crate::chess::clock::{TimeControl, TimeMode};
use crate::chess::opening_book::PolyglotBook;
use crate::chess::pgn::PgnHeaders;
use crate::chess::PieceColor;
use crate::engine::protocol::Score;

/// Default number of games in a new match (the screenshot's value).
pub const DEFAULT_GAMES: u32 = 2;
/// Default UCI `Hash` size in MB.
pub const DEFAULT_HASH_MB: u32 = 64;
/// Default UCI `NalimovCache` size in MB.
pub const DEFAULT_NALIMOV_CACHE_MB: u32 = 4;
/// Default resign adjudication: consecutive moves and centipawn limit.
pub const DEFAULT_RESIGN_MOVECOUNT: u32 = 10;
/// Default resign score, in centipawns below zero.
pub const DEFAULT_RESIGN_SCORE: i32 = 2000;
/// Default draw adjudication: first full move, consecutive moves and score.
pub const DEFAULT_DRAW_MOVENUMBER: u32 = 90;
/// Default draw move count.
pub const DEFAULT_DRAW_MOVECOUNT: u32 = 20;
/// Default draw score, in centipawns from zero.
pub const DEFAULT_DRAW_SCORE: i32 = 5;
/// Default base time: 4 minutes.
pub const DEFAULT_TIME_BASE_MS: u64 = 4 * 60 * 1000;
/// Default increment: 1 second.
pub const DEFAULT_TIME_INCREMENT_MS: u64 = 1_000;

/// One engine in a match: the executable and the name shown before its
/// handshake answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSpec {
    /// Engine executable.
    pub path: PathBuf,
    /// Display name (the engine's `id name` replaces it once it is running).
    pub name: String,
}

impl EngineSpec {
    /// A spec for `path`, using the file name as the display name.
    pub fn from_path(path: PathBuf) -> Self {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self { path, name }
    }

    /// An empty slot, used by [`TournamentConfig::default`].
    pub fn empty(name: &str) -> Self {
        Self {
            path: PathBuf::new(),
            name: name.to_string(),
        }
    }

    /// Whether a real engine file has been chosen.
    pub fn is_set(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }
}

/// How the EPD opening book is walked.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BookOrder {
    /// Shuffle the file and cycle through the shuffled positions.
    Random,
    /// Walk the file from top to bottom and wrap around.
    Sequential,
}

/// An EPD file used to start games from different positions, or a PolyGlot
/// `.bin` book used to play opening moves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpeningBookConfig {
    /// `.epd` or `.bin` file.
    pub path: PathBuf,
    /// Position order, for EPD files.
    pub order: BookOrder,
}

/// What kind of opening book a file is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BookKind {
    /// EPD positions used as starting positions.
    Epd,
    /// PolyGlot book used for opening moves.
    Polyglot,
}

/// A loaded tournament opening book.
#[derive(Debug)]
pub struct TournamentBook {
    /// EPD positions used as starting positions.
    pub start: Option<EpdBook>,
    /// PolyGlot moves used for the opening.
    pub moves: Option<PolyglotBook>,
}

impl TournamentBook {
    /// Number of EPD start positions. A PolyGlot book does not provide start
    /// positions; it provides moves from the normal start.
    pub fn opening_count(&self) -> usize {
        self.start.as_ref().map_or(0, EpdBook::len)
    }

    /// The EPD positions, when an EPD book was selected.
    pub fn epd(&self) -> Option<&EpdBook> {
        self.start.as_ref()
    }

    /// The PolyGlot moves, when a `.bin` book was selected.
    pub fn polyglot(&self) -> Option<&PolyglotBook> {
        self.moves.as_ref()
    }

    /// Whether the book has no content at all.
    pub fn is_empty(&self) -> bool {
        self.start.is_none() && self.moves.is_none()
    }
}

/// Decide whether `path` is an EPD or a PolyGlot book from its extension.
pub fn book_kind(path: &Path) -> Result<BookKind, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "epd" => Ok(BookKind::Epd),
        "bin" => Ok(BookKind::Polyglot),
        _ => Err(format!(
            "The opening book `{}` must be an .epd or .bin file.",
            path.display()
        )),
    }
}

/// Everything a tournament run needs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TournamentConfig {
    /// Engine that plays White in the first game.
    pub white: EngineSpec,
    /// Engine that plays Black in the first game.
    pub black: EngineSpec,
    /// Clock both engines share.
    pub time_control: TimeControl,
    /// Total number of games.
    pub games: u32,
    /// Swap colours after each game.
    pub reverse_colors: bool,
    /// UCI `Hash` size in MB.
    pub hash_mb: u32,
    /// UCI `NalimovCache` size in MB.
    pub nalimov_cache_mb: u32,
    /// PGN file the finished games are written to.
    pub pgn_path: PathBuf,
    /// Optional EPD or PolyGlot opening book.
    pub opening_book: Option<OpeningBookConfig>,
    /// Optional PolyGlot `.bin` move book used in addition to an EPD start
    /// book. `None` means the primary `.bin` book (if any) is the move book.
    #[serde(default)]
    pub move_book: Option<PathBuf>,
    /// Whether resign/draw adjudication is active.
    pub enable_adjudication: bool,
    /// Resign after this many consecutive moves below `resign_score`.
    pub resign_move_count: u32,
    /// Resign score, in centipawns below zero.
    pub resign_score: i32,
    /// Do not adjudicate a draw before this full move.
    pub draw_move_number: u32,
    /// Draw after this many consecutive full moves inside `draw_score`.
    pub draw_move_count: u32,
    /// Draw score, in centipawns from zero.
    pub draw_score: i32,
    /// PGN `Event` tag and the title shown in the UI.
    pub event_name: String,
}

impl Default for TournamentConfig {
    /// The defaults from the feature request: 4+1, two games, reversed
    /// colours, Hash 64, Nalimov 4, random EPD order, resign 10/2000 and
    /// draw 90/20/5.
    fn default() -> Self {
        Self {
            white: EngineSpec::empty("White engine"),
            black: EngineSpec::empty("Black engine"),
            time_control: TimeControl::fischer(DEFAULT_TIME_BASE_MS, DEFAULT_TIME_INCREMENT_MS),
            games: DEFAULT_GAMES,
            reverse_colors: true,
            hash_mb: DEFAULT_HASH_MB,
            nalimov_cache_mb: DEFAULT_NALIMOV_CACHE_MB,
            pgn_path: default_pgn_path(),
            opening_book: None,
            move_book: None,
            enable_adjudication: true,
            resign_move_count: DEFAULT_RESIGN_MOVECOUNT,
            resign_score: DEFAULT_RESIGN_SCORE,
            draw_move_number: DEFAULT_DRAW_MOVENUMBER,
            draw_move_count: DEFAULT_DRAW_MOVECOUNT,
            draw_score: DEFAULT_DRAW_SCORE,
            event_name: "Engine match".to_string(),
        }
    }
}

impl TournamentConfig {
    /// Check the values the runner cannot repair. The dialog shows the same
    /// messages, so a bad configuration never starts a match.
    pub fn validate(&self) -> Result<(), String> {
        if !self.white.is_set() {
            return Err("Choose the White engine.".to_string());
        }
        if !self.black.is_set() {
            return Err("Choose the Black engine.".to_string());
        }
        if self.games == 0 {
            return Err("The match needs at least one game.".to_string());
        }
        if self.pgn_path.as_os_str().is_empty() {
            return Err("Choose a PGN file for the games.".to_string());
        }
        if self.time_control.base_ms == 0 {
            return Err("The time control must be more than zero.".to_string());
        }
        if let Some(book) = &self.opening_book {
            if book.path.as_os_str().is_empty() {
                return Err(
                    "Choose an EPD or PolyGlot opening book or clear the field.".to_string()
                );
            }
            book_kind(&book.path)?;
        }
        if let Some(path) = &self.move_book {
            if path.as_os_str().is_empty() {
                return Err("Choose a PolyGlot move book or clear the field.".to_string());
            }
            if book_kind(path)? != BookKind::Polyglot {
                return Err(format!(
                    "The move book `{}` must be a .bin file.",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// The `setoption` values sent to both engines. The analysis session only
    /// forwards options the engine actually declares, so an engine without a
    /// Nalimov cache simply ignores that entry.
    pub fn engine_options(&self) -> std::collections::BTreeMap<String, String> {
        let mut options = std::collections::BTreeMap::new();
        options.insert("Hash".to_string(), self.hash_mb.to_string());
        options.insert(
            "NalimovCache".to_string(),
            self.nalimov_cache_mb.to_string(),
        );
        // Some engines spell the option with a `Size` suffix; the analysis
        // session only forwards names the engine declared.
        options.insert(
            "NalimovCacheSize".to_string(),
            self.nalimov_cache_mb.to_string(),
        );
        options
    }

    /// The PGN tag value for the clock, in minutes+seconds form.
    pub fn time_control_tag(&self) -> String {
        time_control_tag(&self.time_control)
    }
}

/// The default destination for tournament games: `engine-match.pgn` in a
/// writable folder under the user's home.
///
/// It must not be relative to the process working directory: a macOS app
/// launched from Finder starts with `/` as its working directory, and `/` is
/// read-only, so a relative default fails with “Could not create /tournament:
/// Read-only file system”. `~/Documents/SiliconBoard` always exists or can be
/// created, and the file is easy to find in Finder.
pub fn default_pgn_path() -> PathBuf {
    default_output_dir().join("engine-match.pgn")
}

/// The folder tournament PGNs go to unless the user picks another one:
/// `~/Documents/SiliconBoard`. Relative paths typed in the dialog — or saved
/// by an older version, when the default followed the working directory — are
/// anchored here as well, so they never resolve against `/`.
pub fn default_output_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join("Documents").join("SiliconBoard"),
        None => std::env::temp_dir().join("SiliconBoard"),
    }
}

/// Make `path` absolute: an absolute path is kept, and a relative one is
/// anchored in [`default_output_dir`] rather than the working directory.
pub fn resolve_pgn_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        default_output_dir().join(path)
    }
}

/// A `TimeControl` as the PGN tag and the dialog show it: `4+1` for four
/// minutes and one second, or `90` for sudden death.
pub fn time_control_tag(control: &TimeControl) -> String {
    match control.mode {
        TimeMode::FischerIncrement => {
            let minutes = control.base_ms / 60_000;
            let seconds = (control.base_ms % 60_000) / 1000;
            let increment = control.increment_ms / 1000;
            if seconds == 0 {
                format!("{}+{}", minutes, increment)
            } else {
                format!("{}:{:02}+{}", minutes, seconds, increment)
            }
        }
        TimeMode::SuddenDeath | TimeMode::FixedPerMove => {
            format!("{}", control.base_ms / 1000)
        }
    }
}

/// One game in the schedule. `white_is_first` says whether the configuration's
/// White engine has the white pieces; `opening` indexes the EPD book.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledGame {
    /// 1-based game number, also the PGN `Round` tag.
    pub number: u32,
    /// Whether the first configured engine plays White.
    pub white_is_first: bool,
    /// EPD position index, when a book is in use.
    pub opening: Option<usize>,
}

/// Build the match schedule. Colours alternate when `reverse_colors` is on;
/// EPD positions are shuffled for random order and walked for sequential
/// order, wrapping around when there are more games than positions.
pub fn schedule_games(config: &TournamentConfig, opening_count: usize) -> Vec<ScheduledGame> {
    schedule_games_with_seed(config, opening_count, random_seed())
}

/// [`schedule_games`] with a fixed shuffle seed, for tests.
pub fn schedule_games_with_seed(
    config: &TournamentConfig,
    opening_count: usize,
    seed: u64,
) -> Vec<ScheduledGame> {
    let mut rng = XorShift::with_seed(seed);
    let mut permutation: Vec<usize> = (0..opening_count).collect();
    let mut shuffled = false;
    let mut games = Vec::with_capacity(config.games as usize);
    for index in 0..config.games {
        let white_is_first = !(config.reverse_colors && index % 2 == 1);
        let opening = if opening_count == 0 {
            None
        } else {
            let position = index as usize % opening_count;
            let random = config
                .opening_book
                .as_ref()
                .is_some_and(|book| book.order == BookOrder::Random);
            if random {
                if !shuffled || position == 0 {
                    shuffle(&mut permutation, &mut rng);
                    shuffled = true;
                }
                Some(permutation[position])
            } else {
                Some(position)
            }
        };
        games.push(ScheduledGame {
            number: index + 1,
            white_is_first,
            opening,
        });
    }
    games
}

/// Fisher-Yates shuffle.
fn shuffle(values: &mut [usize], rng: &mut XorShift) {
    for i in (1..values.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        values.swap(i, j);
    }
}

/// Small xorshift generator, matching the opening book picker's approach.
struct XorShift {
    state: u64,
}

impl XorShift {
    fn with_seed(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

/// A seed that differs between runs.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x746f_7572_6e61_6d65);
    hasher.finish()
}

// ─────────────────────────────────────────────────────────────────────────────
// EPD opening book
// ─────────────────────────────────────────────────────────────────────────────

/// An EPD file indexed by byte offset. The UHO suite has millions of lines,
/// so the file is not held in memory: only the offsets are kept and the FEN
/// is read when a game needs it.
#[derive(Debug)]
pub struct EpdBook {
    path: PathBuf,
    offsets: Vec<u64>,
}

impl EpdBook {
    /// Index the non-empty lines of `path`.
    pub fn load(path: &Path) -> Result<Self, EpdError> {
        use std::io::{BufRead, BufReader};

        let file = std::fs::File::open(path).map_err(|source| EpdError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        let mut reader = BufReader::new(file);
        let mut offsets = Vec::new();
        let mut offset = 0u64;
        let mut line = String::new();
        loop {
            line.clear();
            let start = offset;
            let read = reader
                .read_line(&mut line)
                .map_err(|source| EpdError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            offset += read as u64;
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with(';') {
                offsets.push(start);
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            offsets,
        })
    }

    /// Number of positions in the file.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    /// Whether the file has no positions.
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Read the FEN at `index`. `None` when the line has no valid EPD prefix.
    pub fn fen_at(&self, index: usize) -> Result<Option<String>, EpdError> {
        use std::io::{BufRead, BufReader, Seek, SeekFrom};

        let offset = *self.offsets.get(index).ok_or(EpdError::Index {
            index,
            len: self.offsets.len(),
        })?;
        let mut file = std::fs::File::open(&self.path).map_err(|source| EpdError::Open {
            path: self.path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|source| EpdError::Read {
                path: self.path.clone(),
                source,
            })?;
        let mut line = String::new();
        BufReader::new(file)
            .read_line(&mut line)
            .map_err(|source| EpdError::Read {
                path: self.path.clone(),
                source,
            })?;
        Ok(parse_epd_line(&line))
    }
}

/// Turn one EPD line into a six-field FEN. The first four fields are the
/// position; `hmvc` and `fmvn` operations supply the move counters.
pub fn parse_epd_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
        return None;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 4 {
        return None;
    }
    let mut fen = tokens[..4].join(" ");
    let mut halfmove = None;
    let mut fullmove = None;
    // Many EPD suites (including the UHO file) are full six-field FENs.
    if tokens.len() >= 6 && tokens[4].parse::<u32>().is_ok() && tokens[5].parse::<u32>().is_ok() {
        halfmove = Some(clean_operand(tokens[4]));
        fullmove = Some(clean_operand(tokens[5]));
    }
    let mut index = 4;
    while index + 1 < tokens.len() {
        match tokens[index] {
            "hmvc" if halfmove.is_none() => halfmove = Some(clean_operand(tokens[index + 1])),
            "fmvn" if fullmove.is_none() => fullmove = Some(clean_operand(tokens[index + 1])),
            _ => {}
        }
        index += 1;
    }
    fen.push(' ');
    fen.push_str(halfmove.unwrap_or("0"));
    fen.push(' ');
    fen.push_str(fullmove.unwrap_or("1"));
    Some(fen)
}

/// EPD operands end at a semicolon and may be quoted.
fn clean_operand(token: &str) -> &str {
    token.trim_end_matches(';').trim_matches('"').trim()
}

/// Why an EPD file could not be read.
#[derive(Debug)]
pub enum EpdError {
    /// The file could not be opened.
    Open {
        /// The EPD file.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// The file could not be read.
    Read {
        /// The EPD file.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// A position index was outside the file.
    Index {
        /// Requested index.
        index: usize,
        /// Number of positions.
        len: usize,
    },
}

impl std::fmt::Display for EpdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpdError::Open { path, source } => {
                write!(
                    f,
                    "could not open EPD book `{}`: {}",
                    path.display(),
                    source
                )
            }
            EpdError::Read { path, source } => {
                write!(
                    f,
                    "could not read EPD book `{}`: {}",
                    path.display(),
                    source
                )
            }
            EpdError::Index { index, len } => {
                write!(
                    f,
                    "EPD position {} is outside the book ({} positions)",
                    index, len
                )
            }
        }
    }
}

impl std::error::Error for EpdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EpdError::Open { source, .. } | EpdError::Read { source, .. } => Some(source),
            EpdError::Index { .. } => None,
        }
    }
}

/// Find a line of PolyGlot book moves from the standard start to `target`.
///
/// EPD files contain positions, not moves. When an EPD start is combined with
/// a PolyGlot move book, this lets the GUI replay the book line that reaches
/// the EPD position, so the PGN contains the opening moves as well as the
/// start position. Returns `None` when the book has no such line.
pub fn find_book_path(book: &PolyglotBook, target: &shakmaty::Chess) -> Option<Vec<String>> {
    let start = shakmaty::Chess::default();
    if &start == target {
        return Some(Vec::new());
    }
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![(start, Vec::new())];
    let fullmove = target.fullmoves().get() as usize;
    let max_depth = if fullmove > 1 {
        fullmove.saturating_mul(2).saturating_add(4)
    } else {
        40
    };
    while let Some((pos, path)) = stack.pop() {
        if path.len() >= max_depth || visited.len() > 200_000 {
            continue;
        }
        let key = pos.zobrist_hash::<Zobrist64>(EnPassantMode::Legal).0;
        if !visited.insert(key) {
            continue;
        }
        for entry in book.find(key) {
            let Some(m) = entry.to_move(&pos) else {
                continue;
            };
            let mut next = pos.clone();
            next.play_unchecked(m);
            let mut next_path = path.clone();
            next_path.push(UciMove::from_move(m, CastlingMode::Standard).to_string());
            if &next == target {
                return Some(next_path);
            }
            stack.push((next, next_path));
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Adjudication
// ─────────────────────────────────────────────────────────────────────────────
/// The outcome of one score observation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Adjudication {
    /// Nothing to decide yet.
    None,
    /// Adjudicate a draw.
    Draw,
    /// Adjudicate a loss for this side.
    Resign(PieceColor),
}

/// Resign and draw adjudication: score and move thresholds that end a game.
#[derive(Clone, Debug)]
pub struct Adjudicator {
    resign_move_count: u32,
    resign_score: i32,
    draw_move_number: u32,
    draw_move_count: u32,
    draw_score: i32,
    draw_score_count: u32,
    resign_score_count: [u32; 2],
}

impl Adjudicator {
    /// Build an adjudicator from the match configuration.
    pub fn new(config: &TournamentConfig) -> Self {
        Self {
            resign_move_count: config.resign_move_count,
            resign_score: config.resign_score,
            draw_move_number: config.draw_move_number,
            draw_move_count: config.draw_move_count,
            draw_score: config.draw_score,
            draw_score_count: 0,
            resign_score_count: [0, 0],
        }
    }

    /// Observe the score of the side that just moved, at the position after
    /// its move. `last_move` resets the draw counter after captures and pawn
    /// moves.
    pub fn observe(
        &mut self,
        mover: PieceColor,
        score: Option<Score>,
        last_move: Option<&Move>,
        fullmove: u32,
    ) -> Adjudication {
        let index = color_index(mover);
        let Some(score) = score else {
            self.draw_score_count = 0;
            self.resign_score_count[index] = 0;
            return Adjudication::None;
        };
        let score = score_cp(score);

        if last_move.is_some_and(is_irreversible) {
            self.draw_score_count = 0;
        }

        // Draw: both sides' scores stay near zero for enough consecutive plies.
        if self.draw_move_count > 0 {
            if score.abs() <= self.draw_score {
                self.draw_score_count += 1;
            } else {
                self.draw_score_count = 0;
            }
            if fullmove >= self.draw_move_number
                && self.draw_score_count >= self.draw_move_count.saturating_mul(2)
            {
                return Adjudication::Draw;
            }
        }

        // Resign: the side that just moved sees itself far below zero.
        if self.resign_move_count > 0 {
            if score <= -self.resign_score {
                self.resign_score_count[index] += 1;
            } else {
                self.resign_score_count[index] = 0;
            }
            if self.resign_score_count[index] >= self.resign_move_count {
                return Adjudication::Resign(mover);
            }
        }

        Adjudication::None
    }
}

/// A capture or pawn move resets the draw counter.
fn is_irreversible(m: &Move) -> bool {
    m.capture().is_some() || m.role() == Role::Pawn
}

/// Convert a UCI score to centipawns for adjudication. A mate is far outside
/// any resign or draw threshold.
pub fn score_cp(score: Score) -> i32 {
    match score {
        Score::Cp(cp) => cp,
        Score::Mate(plies) if plies > 0 => 30_000,
        Score::Mate(_) => -30_000,
    }
}

/// `0` for White and `1` for Black.
fn color_index(color: PieceColor) -> usize {
    match color {
        PieceColor::White => 0,
        PieceColor::Black => 1,
    }
}

/// Display text for a score that is already from White's point of view.
pub fn score_label(score: Score) -> String {
    match score {
        Score::Cp(cp) => format!("{:+.2}", f64::from(cp) / 100.0),
        Score::Mate(plies) if plies > 0 => format!("#{}", plies),
        Score::Mate(plies) => format!("#-{}", -plies),
    }
}

/// The score of the side that just moved, for the tournament window. `score`
/// is the mover's own score, from the search before its move (the side to move
/// then). The window shows White's point of view, as every GUI evaluation does:
/// a Black engine that is ahead shows as a negative number.
pub fn white_score_label(score: Score, mover: PieceColor) -> String {
    let from_white = match (score, mover) {
        (score, PieceColor::White) => score,
        (Score::Cp(cp), PieceColor::Black) => Score::Cp(-cp),
        (Score::Mate(plies), PieceColor::Black) => Score::Mate(-plies),
    };
    score_label(from_white)
}

// ─────────────────────────────────────────────────────────────────────────────
// PGN headers and dates
// ─────────────────────────────────────────────────────────────────────────────

/// PGN headers for one tournament game.
pub fn game_headers(
    config: &TournamentConfig,
    round: u32,
    white: &str,
    black: &str,
    reason: &str,
    ply_count: usize,
) -> PgnHeaders {
    PgnHeaders {
        event: config.event_name.clone(),
        site: "SiliconBoard".to_string(),
        date: today_utc(),
        round: round.to_string(),
        white: white.to_string(),
        black: black.to_string(),
        extra_tags: vec![
            ("TimeControl".to_string(), config.time_control_tag()),
            ("Termination".to_string(), reason.to_string()),
            ("PlyCount".to_string(), ply_count.to_string()),
        ],
    }
}

/// Today's date as PGN writes it, `YYYY.MM.DD`, in UTC.
pub fn today_utc() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    format_date(seconds)
}

/// Format a Unix timestamp as `YYYY.MM.DD`.
pub fn format_date(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{:04}.{:02}.{:02}", year, month, day)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        year + if month <= 2 { 1 } else { 0 },
        month as u32,
        day as u32,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Progress
// ─────────────────────────────────────────────────────────────────────────────

/// A game that is being played right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentGame {
    /// Game number.
    pub number: u32,
    /// White player.
    pub white: String,
    /// Black player.
    pub black: String,
    /// Plies played so far.
    pub ply: usize,
    /// Full move number of the position.
    pub fullmove: u32,
    /// Last score reported by the engine on the move.
    pub score: Option<String>,
}

/// What the tournament window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TournamentStatus {
    /// Total scheduled games.
    pub total_games: u32,
    /// Games that have finished.
    pub completed_games: u32,
    /// Wins per engine name. Colours alternate, so the match result is kept
    /// per engine rather than per side.
    pub wins: BTreeMap<String, u32>,
    /// The two configured engines, in slot order (White first, Black second).
    pub engines: [String; 2],
    /// Drawn games.
    pub draws: u32,
    /// Games that could not be played.
    pub failed_games: u32,
    /// Whether the runner is still playing.
    pub running: bool,
    /// Whether the user stopped the match.
    pub cancelled: bool,
    /// The PGN destination.
    pub pgn_path: PathBuf,
    /// Games in flight.
    pub current: Vec<CurrentGame>,
    /// A message for the status window, e.g. an error or "loading book".
    pub message: Option<String>,
    /// Whether the run stopped because of an error.
    pub failed: bool,
    /// One line per finished game, e.g. `Game 2: 1-0 (White wins by checkmate)`.
    pub results: Vec<String>,
}

impl Default for TournamentStatus {
    fn default() -> Self {
        Self {
            total_games: 0,
            completed_games: 0,
            wins: BTreeMap::new(),
            engines: [String::new(), String::new()],
            draws: 0,
            failed_games: 0,
            running: false,
            cancelled: false,
            pgn_path: PathBuf::new(),
            current: Vec::new(),
            message: None,
            failed: false,
            results: Vec::new(),
        }
    }
}

impl TournamentStatus {
    /// A one-line summary for the status bar.
    pub fn summary(&self) -> String {
        let scores = self.score_line();
        if self.running {
            if let Some(game) = self.current.first() {
                return format!("Tournament: game {} of {}", game.number, self.total_games);
            }
            if self.completed_games == 0 {
                return format!("Tournament: 0 of {} games, starting…", self.total_games);
            }
            return format!(
                "Tournament: {} of {} games finished ({})",
                self.completed_games, self.total_games, scores
            );
        }
        if self.cancelled {
            return format!(
                "Tournament stopped: {} of {} {} ({})",
                self.completed_games,
                self.total_games,
                games_word(self.total_games),
                scores
            );
        }
        format!(
            "Tournament finished: {} {} ({})",
            self.completed_games,
            games_word(self.completed_games),
            scores
        )
    }

    /// The head-to-head score in standard chess notation, e.g.
    /// `Lizard vs Triumviratus: 2½–3½`. Draws count half a point for each side.
    pub fn match_line(&self) -> String {
        let left = self.engines[0].as_str();
        let right = self.engines[1].as_str();
        if left.is_empty() || right.is_empty() {
            return self.score_line();
        }
        let left_wins = self.wins.get(left).copied().unwrap_or(0);
        let right_wins = self.wins.get(right).copied().unwrap_or(0);
        if left == right {
            return format!(
                "{}: {} wins, {} draws",
                left,
                left_wins.max(right_wins),
                self.draws
            );
        }
        format!(
            "{} vs {}: {}–{}",
            left,
            right,
            score_text(left_wins, self.draws),
            score_text(right_wins, self.draws)
        )
    }

    /// Points per engine plus the draw count, e.g.
    /// `Stockfish 2½, Triumviratus 3½, 1 draw`.
    pub fn score_line(&self) -> String {
        let mut parts: Vec<String> = self
            .wins
            .iter()
            .map(|(name, wins)| format!("{} {}", name, score_text(*wins, self.draws)))
            .collect();
        if self.draws > 0 {
            parts.push(format!(
                "{} draw{}",
                self.draws,
                if self.draws == 1 { "" } else { "s" }
            ));
        }
        if self.failed_games > 0 {
            parts.push(format!("{} failed", self.failed_games));
        }
        if parts.is_empty() {
            "no games finished".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// A match score in standard chess notation: wins plus half a point per draw,
/// e.g. `0` → `0`, `1` half point → `½`, `7` half points → `3½`.
fn score_text(wins: u32, draws: u32) -> String {
    let halves = wins * 2 + draws;
    let whole = halves / 2;
    if halves % 2 == 1 {
        if whole == 0 {
            "½".to_string()
        } else {
            format!("{whole}½")
        }
    } else {
        whole.to_string()
    }
}

/// `game` or `games`, for the progress summary.
fn games_word(count: u32) -> &'static str {
    if count == 1 {
        "game"
    } else {
        "games"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chess::clock::TimeControl;

    fn config() -> TournamentConfig {
        TournamentConfig {
            white: EngineSpec::from_path(PathBuf::from("/opt/stockfish")),
            black: EngineSpec::from_path(PathBuf::from("/opt/triumviratus")),
            ..TournamentConfig::default()
        }
    }

    #[test]
    fn defaults_match_the_requested_values() {
        let config = TournamentConfig::default();
        assert_eq!(config.time_control, TimeControl::fischer(240_000, 1_000));
        assert_eq!(config.games, 2);
        assert!(config.reverse_colors);
        assert_eq!(config.hash_mb, 64);
        assert_eq!(config.nalimov_cache_mb, 4);
        assert_eq!(config.resign_move_count, 10);
        assert_eq!(config.resign_score, 2000);
        assert_eq!(config.draw_move_number, 90);
        assert_eq!(config.draw_move_count, 20);
        assert_eq!(config.draw_score, 5);
        assert_eq!(
            config.opening_book.as_ref().map(|book| book.order),
            None,
            "no book until one is chosen"
        );
    }

    /// A Finder-launched app has `/` as its working directory, and `/` is
    /// read-only: the default PGN destination must never be relative.
    #[test]
    fn the_default_pgn_path_is_absolute_and_in_a_writable_folder() {
        let path = default_pgn_path();
        assert!(
            path.is_absolute(),
            "a relative default resolves against `/` in a Finder launch"
        );
        assert!(
            path.ends_with(
                Path::new("Documents")
                    .join("SiliconBoard")
                    .join("engine-match.pgn")
            ),
            "unexpected default: {}",
            path.display()
        );
        assert_eq!(path.parent(), Some(default_output_dir().as_path()));
    }

    #[test]
    fn relative_pgn_paths_are_anchored_in_the_default_folder() {
        let resolved = resolve_pgn_path(Path::new("tournament/engine-match.pgn"));
        assert!(
            resolved.is_absolute(),
            "a relative path must not stay relative"
        );
        assert_eq!(
            resolved,
            default_output_dir().join("tournament/engine-match.pgn"),
            "old relative settings land under the default folder"
        );

        let absolute = Path::new("/tmp/games.pgn");
        assert_eq!(
            resolve_pgn_path(absolute),
            absolute,
            "absolute paths are kept"
        );
    }

    #[test]
    fn colours_alternate_only_when_reverse_colors_is_on() {
        let mut config = config();
        config.games = 4;
        let games = schedule_games_with_seed(&config, 0, 7);
        assert_eq!(
            games
                .iter()
                .map(|game| game.white_is_first)
                .collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
        config.reverse_colors = false;
        let games = schedule_games_with_seed(&config, 0, 7);
        assert!(games.iter().all(|game| game.white_is_first));
    }

    #[test]
    fn sequential_book_order_walks_the_file_and_wraps() {
        let mut config = config();
        config.games = 5;
        config.opening_book = Some(OpeningBookConfig {
            path: PathBuf::from("openings.epd"),
            order: BookOrder::Sequential,
        });
        let games = schedule_games_with_seed(&config, 3, 99);
        assert_eq!(
            games.iter().map(|game| game.opening).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2), Some(0), Some(1)]
        );
    }

    #[test]
    fn random_book_order_uses_a_shuffled_permutation() {
        let mut config = config();
        config.games = 8;
        config.opening_book = Some(OpeningBookConfig {
            path: PathBuf::from("openings.epd"),
            order: BookOrder::Random,
        });
        let games = schedule_games_with_seed(&config, 4, 12345);
        let openings: Vec<usize> = games.iter().filter_map(|game| game.opening).collect();
        assert_eq!(openings.len(), 8);
        // Every group of four is a permutation of 0..4.
        for group in openings.chunks(4) {
            let mut sorted = group.to_vec();
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3]);
        }
        // The seed makes the order repeatable.
        let again = schedule_games_with_seed(&config, 4, 12345);
        assert_eq!(
            games.iter().map(|game| game.opening).collect::<Vec<_>>(),
            again.iter().map(|game| game.opening).collect::<Vec<_>>()
        );
    }

    #[test]
    fn epd_line_uses_hmvc_and_fmvn_or_the_default_counters() {
        assert_eq!(
            parse_epd_line("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - hmvc 4; fmvn 12;")
                .as_deref(),
            Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 4 12")
        );
        assert_eq!(
            parse_epd_line("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -").as_deref(),
            Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
        );
        // UHO lines are full six-field FENs; keep their move counters.
        assert_eq!(
            parse_epd_line("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 2 4")
                .as_deref(),
            Some("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 2 4")
        );
        assert_eq!(parse_epd_line("   "), None);
        assert_eq!(parse_epd_line("# comment"), None);
    }

    #[test]
    fn an_epd_file_indexes_and_yields_a_fen() {
        // A small EPD suite written for the test: no external book file is
        // needed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suite.epd");
        std::fs::write(
            &path,
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - bm e4; id \"start\";\n\
             8/8/8/4k3/8/8/2Q5/2K5 w - - bm Qe2+;\n\
             r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - bm Bb5;\n",
        )
        .unwrap();
        let book = EpdBook::load(&path).expect("the EPD file must index");
        assert_eq!(book.len(), 3);
        let fen = book
            .fen_at(0)
            .unwrap()
            .expect("the first line is a position");
        assert!(crate::chess::GameState::from_fen(&fen).is_ok());
    }

    #[test]
    fn a_polyglot_book_can_reconstruct_the_line_to_an_epd_position() {
        use crate::chess::GameState;

        let start_key = GameState::new().zobrist();
        let mut after_e4 = GameState::new();
        after_e4.apply_uci("e2e4").unwrap();
        let after_e4_key = after_e4.zobrist();
        let mut bytes = Vec::new();
        for (key, encoded) in [(start_key, 0x031cu16), (after_e4_key, 0x0d24u16)] {
            bytes.extend_from_slice(&key.to_be_bytes());
            bytes.extend_from_slice(&encoded.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
        }
        let book = crate::chess::PolyglotBook::from_bytes(&bytes).unwrap();
        let target =
            GameState::from_fen("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2")
                .unwrap();
        let path = find_book_path(&book, &target.position()).expect("the book has the line");
        assert_eq!(path, vec!["e2e4".to_string(), "e7e5".to_string()]);
    }

    #[test]
    fn resign_triggers_after_the_configured_run_of_bad_scores() {
        let mut config = config();
        config.resign_move_count = 3;
        config.resign_score = 200;
        let mut adjudicator = Adjudicator::new(&config);
        let move_ = Move::Normal {
            role: Role::Knight,
            from: shakmaty::Square::from_ascii(b"g1").unwrap(),
            capture: None,
            to: shakmaty::Square::from_ascii(b"f3").unwrap(),
            promotion: None,
        };
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(-250)), Some(&move_), 1),
            Adjudication::None
        );
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(-250)), Some(&move_), 2),
            Adjudication::None
        );
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(-250)), Some(&move_), 3),
            Adjudication::Resign(PieceColor::White)
        );
    }

    #[test]
    fn a_good_score_resets_the_resign_run() {
        let mut config = config();
        config.resign_move_count = 2;
        config.resign_score = 200;
        let mut adjudicator = Adjudicator::new(&config);
        assert_eq!(
            adjudicator.observe(PieceColor::Black, Some(Score::Cp(-500)), None, 1),
            Adjudication::None
        );
        assert_eq!(
            adjudicator.observe(PieceColor::Black, Some(Score::Cp(10)), None, 2),
            Adjudication::None
        );
        assert_eq!(
            adjudicator.observe(PieceColor::Black, Some(Score::Cp(-500)), None, 3),
            Adjudication::None,
            "the run started again"
        );
    }

    #[test]
    fn draw_needs_the_move_number_and_two_plies_per_full_move() {
        let mut config = config();
        config.draw_move_number = 5;
        config.draw_move_count = 2;
        config.draw_score = 10;
        let mut adjudicator = Adjudicator::new(&config);
        // Three plies before the move-number threshold.
        for fullmove in 1..=3 {
            assert_eq!(
                adjudicator.observe(PieceColor::White, Some(Score::Cp(0)), None, fullmove),
                Adjudication::None
            );
        }
        // Four consecutive near-zero plies, but still before move 5.
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(0)), None, 4),
            Adjudication::None
        );
        // At move 5 the same run is enough.
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(0)), None, 5),
            Adjudication::Draw
        );
    }

    #[test]
    fn a_capture_or_pawn_move_resets_the_draw_counter() {
        let mut config = config();
        config.draw_move_number = 1;
        config.draw_move_count = 2;
        config.draw_score = 10;
        let mut adjudicator = Adjudicator::new(&config);
        let quiet = Move::Normal {
            role: Role::Knight,
            from: shakmaty::Square::from_ascii(b"g1").unwrap(),
            capture: None,
            to: shakmaty::Square::from_ascii(b"f3").unwrap(),
            promotion: None,
        };
        let pawn = Move::Normal {
            role: Role::Pawn,
            from: shakmaty::Square::from_ascii(b"e2").unwrap(),
            capture: None,
            to: shakmaty::Square::from_ascii(b"e4").unwrap(),
            promotion: None,
        };
        for _ in 0..3 {
            assert_eq!(
                adjudicator.observe(PieceColor::White, Some(Score::Cp(0)), Some(&quiet), 2),
                Adjudication::None
            );
        }
        // A pawn move clears the run; one more quiet move starts it again.
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(0)), Some(&pawn), 2),
            Adjudication::None
        );
        assert_eq!(
            adjudicator.observe(PieceColor::White, Some(Score::Cp(0)), Some(&quiet), 2),
            Adjudication::None
        );
    }

    #[test]
    fn time_control_tag_uses_minutes_and_seconds() {
        assert_eq!(
            time_control_tag(&TimeControl::fischer(240_000, 1_000)),
            "4+1"
        );
        assert_eq!(
            time_control_tag(&TimeControl::fischer(90_000, 2_000)),
            "1:30+2"
        );
        assert_eq!(time_control_tag(&TimeControl::sudden_death(60_000)), "60");
    }

    #[test]
    fn dates_are_formatted_as_pgn_dates() {
        assert_eq!(format_date(0), "1970.01.01");
        assert_eq!(format_date(1_700_000_000), "2023.11.14");
    }

    #[test]
    fn match_line_shows_the_head_to_head_score() {
        let mut status = TournamentStatus {
            engines: ["Engine A".to_string(), "Engine B".to_string()],
            ..TournamentStatus::default()
        };
        status.wins.insert("Engine A".to_string(), 2);
        status.wins.insert("Engine B".to_string(), 3);
        status.draws = 1;
        assert_eq!(status.match_line(), "Engine A vs Engine B: 2½–3½");
    }

    #[test]
    fn match_line_uses_standard_chess_notation_for_draws() {
        let mut status = TournamentStatus {
            engines: ["Engine A".to_string(), "Engine B".to_string()],
            ..TournamentStatus::default()
        };
        status.wins.insert("Engine B".to_string(), 3);
        status.draws = 1;
        assert_eq!(
            status.match_line(),
            "Engine A vs Engine B: ½–3½",
            "one draw and three losses is ½–3½, not 0 : 3 (1 draw)"
        );
    }

    #[test]
    fn config_validation_names_missing_parts() {
        let mut draft = config();
        assert!(draft.validate().is_ok());
        draft.white = EngineSpec::empty("White engine");
        assert!(draft.validate().unwrap_err().contains("White engine"));
        draft = config();
        draft.games = 0;
        assert!(draft.validate().unwrap_err().contains("at least one game"));
        draft = config();
        draft.pgn_path = PathBuf::new();
        assert!(draft.validate().unwrap_err().contains("PGN"));
    }

    #[test]
    fn the_tournament_window_shows_the_mover_score_from_whites_side() {
        let ahead = Score::Cp(150);
        assert_eq!(white_score_label(ahead, PieceColor::White), "+1.50");
        assert_eq!(
            white_score_label(ahead, PieceColor::Black),
            "-1.50",
            "Black ahead shows negative"
        );
        assert_eq!(white_score_label(Score::Mate(3), PieceColor::Black), "#-3");
        assert_eq!(white_score_label(Score::Mate(3), PieceColor::White), "#3");
    }
}

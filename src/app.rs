//! Application shell: owns the game, the engines and the opening book, lays
//! out the panels, and performs the actions the widgets report.
//!
//! Threading: the egui thread never waits on anything. Slow work (file
//! dialogs, spawning an engine, reading a book or a PGN, writing a file) runs
//! on the Tokio runtime owned by [`SiliconBoardApp`] and reports back as an
//! [`AppEvent`], which [`SiliconBoardApp::drain_app_events`] picks up each frame.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key};
use tokio::sync::mpsc;

use crate::assets::{AssetError, Assets, BoardTexture, BoardTheme, PieceId, PieceSet};
use std::collections::BTreeMap;

use crate::chess::clock::{Clock, TimeControl};
use crate::chess::game_state::{Ending, MoveLine};
use crate::chess::opening_book::BookPicker;
use crate::chess::openings::{OpeningError, OpeningNames};
use crate::chess::pgn::{self, PgnCollection, PgnError, PgnImport};
use crate::chess::players::{accepts_draw, GameMode, Seat};
use crate::chess::{BookError, GameState, PieceColor, PolyglotBook};
use crate::engine::protocol::GoControl;
use crate::engine::{
    AnalysisStatus, EngineError, EngineHost, EngineProcess, EngineWake, SearchGoal,
};
use crate::gui::board_view::{self, BoardAction, BoardView, SetupBoardAction};
use crate::gui::eval_panel::{self, EvalCard, EvalSummary};
use crate::gui::menu_bar::{self, MenuAction, MenuState};
use crate::gui::options_dialog::{default_value, OptionsAction, OptionsDialog};
use crate::gui::setup_board::{self, SetupAction, SetupBoard};
use crate::gui::tablebases::{self, TablebaseDialog, TablebaseOutcome};
use crate::gui::time_control::{self, ClockFace, DialogOutcome, PlayerBar, TimeControlDialog};
use crate::gui::tournament_dialog::{
    self, ProgressAction, TournamentAction, TournamentDialog, TournamentSide,
};
use crate::gui::{pgn_panel, theme};
use crate::settings::{self, Settings, SettingsError};
use crate::tournament::{
    self, book_kind, find_book_path, game_headers, schedule_games, white_score_label, Adjudication,
    Adjudicator, BookKind, CurrentGame, EngineSpec, EpdBook, ScheduledGame, TournamentBook,
    TournamentConfig, TournamentStatus,
};
use shakmaty::Move;

/// How long the app waits at exit for engines to quit before it leaves. The
/// quit path kills an engine that has not gone after 500 ms, so the wait
/// ends at the latest soon after that.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Time an engine thinks for each move in a game without a clock.
const ENGINE_MOVE_MS: u64 = 1_000;

/// While a clock is running, the window is redrawn this often so the digits
/// keep moving and a flag is seen without input.
const CLOCK_REFRESH: Duration = Duration::from_millis(100);

/// How long the engine looks at a position when it is asked for a draw.
const EVALUATION_MS: u64 = 1_000;

/// Opening-book replies stop after this many plies, so the book covers the
/// opening only.
const BOOK_MAX_PLIES: usize = 40;

/// Restarts of one engine slot are counted over this window. Once an engine has
/// been restarted [`RESTART_LIMIT`] times within it, the next failure leaves it
/// offline, so a broken engine cannot loop.
const RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Restarts allowed within [`RESTART_WINDOW`] before an engine is left offline.
const RESTART_LIMIT: usize = 3;

/// How long a started engine may take to answer `uci` before it is declared
/// not to be a UCI engine.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long after the last panel/window resize the settings are written.
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// While an engine is loaded, the window runs a frame this often so the
/// watchdog and the handshake timeout are checked even when the engine is
/// silent. Engine output itself wakes the window at once (see [`EngineWake`]),
/// so the analysis updates do not wait for this tick.
const ENGINE_TICK: Duration = Duration::from_millis(500);

/// Shortest gap between engine-driven repaints: about 30 frames per second.
/// A search emits `info` lines far faster than that, and drawing each one
/// spends frames for no visible gain.
const ENGINE_WAKE_INTERVAL: Duration = Duration::from_millis(33);

/// Default height of the move list when no size has been saved yet.
const DEFAULT_MOVE_LIST_HEIGHT: f32 = 190.0;

/// Smallest useful move list, so a dragged panel can always be grabbed again.
const MIN_MOVE_LIST_HEIGHT: f32 = 80.0;

/// How serious the message in the status line is. Warnings and errors are
/// drawn in colour so they cannot be missed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StatusLevel {
    /// Something that happened as expected.
    Info,
    /// Something the user should know, such as ignored PGN content.
    Warning,
    /// A failed action: it did not happen.
    Error,
}

/// Cache of one engine's principal variation as SAN.
struct PvCache {
    /// Displayed position the SAN was computed from.
    fen: String,
    /// UCI moves the SAN was computed from.
    uci: Vec<String>,
    /// Converted SAN line.
    san: Arc<Vec<String>>,
}

/// The book moves remembered for one position: `(source, FEN, moves)`.
type BookMemo = ((bool, u64), String, Vec<(Move, u16)>);

/// Cached SAN layouts for the move list, keyed by history revision and figurine setting.
type PgnGalleyCache = (u64, bool, Vec<Option<Arc<egui::Galley>>>);

/// A one-second summary of frame costs, used by the frame-cost measurement.
#[derive(Copy, Clone, Debug, PartialEq)]
struct FrameSummary {
    /// Frames counted in the window.
    frames: u32,
    /// Mean CPU time spent in `logic` + `ui` for one frame.
    average_cost: Duration,
    /// Longest frame cost in the window.
    worst_cost: Duration,
    /// Mean wall-clock gap between frames.
    average_gap: Duration,
    /// Longest wall-clock gap between frames.
    worst_gap: Duration,
}

/// Measures the cost of each frame (the `logic` + `ui` work) while an engine
/// searches, and reports one summary per second. Enabled by `SILICONBOARD_PERF`;
/// off by default, so normal runs do not pay for it.
///
/// The gap between frames is logged too: with event-driven repaints it is the
/// engine's `info` rate, not the cost of drawing.
struct FrameTimer {
    /// End of the previous frame, for the wall-clock gap.
    last: Instant,
    /// Start of the current one-second window.
    window: Instant,
    /// Frames counted in the window.
    frames: u32,
    /// Sum of the frame costs in the window.
    total_cost: Duration,
    /// Longest frame cost in the window.
    worst_cost: Duration,
    /// Sum of the wall-clock gaps in the window.
    total_gap: Duration,
    /// Longest wall-clock gap in the window.
    worst_gap: Duration,
    /// Whether the previous recorded frame was active.
    was_active: bool,
}

impl FrameTimer {
    /// Start measuring from `now`.
    fn new_at(now: Instant) -> Self {
        Self {
            last: now,
            window: now,
            frames: 0,
            total_cost: Duration::ZERO,
            worst_cost: Duration::ZERO,
            total_gap: Duration::ZERO,
            worst_gap: Duration::ZERO,
            was_active: false,
        }
    }

    /// Record one frame that took `cost` at `now`. `active` is false while no
    /// engine searches; the window is then reset so idle time does not skew the
    /// numbers.
    fn record_at(&mut self, now: Instant, cost: Duration, active: bool) -> Option<FrameSummary> {
        if !active {
            self.last = now;
            self.window = now;
            self.frames = 0;
            self.total_cost = Duration::ZERO;
            self.worst_cost = Duration::ZERO;
            self.total_gap = Duration::ZERO;
            self.worst_gap = Duration::ZERO;
            self.was_active = false;
            return None;
        }
        // The first active frame only starts the window; the idle gap before it
        // is not a frame interval.
        if !self.was_active {
            self.was_active = true;
            self.window = now;
            self.last = now;
            return None;
        }
        let gap = now.saturating_duration_since(self.last);
        self.last = now;
        self.frames += 1;
        self.total_cost += cost;
        self.worst_cost = self.worst_cost.max(cost);
        self.total_gap += gap;
        self.worst_gap = self.worst_gap.max(gap);
        if now.saturating_duration_since(self.window) < Duration::from_secs(1) {
            return None;
        }
        let summary = FrameSummary {
            frames: self.frames,
            average_cost: self.total_cost / self.frames.max(1),
            worst_cost: self.worst_cost,
            average_gap: self.total_gap / self.frames.max(1),
            worst_gap: self.worst_gap,
        };
        self.window = now;
        self.frames = 0;
        self.total_cost = Duration::ZERO;
        self.worst_cost = Duration::ZERO;
        self.total_gap = Duration::ZERO;
        self.worst_gap = Duration::ZERO;
        Some(summary)
    }
}

/// Sends results of background work to the UI thread and wakes the window,
/// so a result is handled at once even when nothing else is happening. (The
/// window only runs a frame when asked; without the wake-up a result waits
/// for the next mouse move.)
#[derive(Clone)]
pub struct EventSender {
    /// Channel drained once per frame in [`SiliconBoardApp::drain_app_events`].
    tx: mpsc::UnboundedSender<AppEvent>,
    /// The egui context, used only to request a repaint.
    ctx: egui::Context,
}

impl EventSender {
    /// Queue `event` for the UI thread and ask for a frame to handle it.
    fn send(&self, event: AppEvent) {
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }

    /// Ask the window for a frame. Engine readers call this when output
    /// arrives, so the analysis is shown as soon as it exists instead of on a
    /// polling timer.
    pub fn wake(&self) {
        self.ctx.request_repaint();
    }
}

/// A wake-up callback shared by both engine readers. It caps engine-driven
/// repaints at about 30 per second; important lines (handshake, `bestmove`,
/// exit) wake at once. One shared clock keeps two searching engines from
/// doubling the frame rate.
fn engine_wake_for(events: &EventSender) -> EngineWake {
    let sender = events.clone();
    let last = Arc::new(Mutex::new(Instant::now() - ENGINE_WAKE_INTERVAL));
    Arc::new(move |important: bool| {
        let wake = {
            let mut last = last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if wakes_now(important, last.elapsed()) {
                *last = Instant::now();
                true
            } else {
                false
            }
        };
        if wake {
            sender.wake();
        }
    })
}

/// Book-picker seed for one tournament game. The same match and game number
/// always give the same seed, while different games (and different matches)
/// get unrelated sequences, so repeated openings do not replay one book line.
fn book_seed(tournament_generation: u64, game_number: u32) -> u64 {
    tournament_generation.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(game_number)
}

/// Whether the 500 ms frame tick is needed for this engine state. Only a
/// starting engine has a deadline that runs on the frame clock (the handshake
/// timeout in `report_engine_start_up`); a searching engine is watched by its
/// own monitor task and wakes the window on output or a stall.
fn engine_tick_needed(status: AnalysisStatus) -> bool {
    status == AnalysisStatus::Starting
}

/// Whether an engine line should wake the window now. Handshake answers,
/// `bestmove`, watchdog answers and process exit always do; `info` lines wait
/// for the next [`ENGINE_WAKE_INTERVAL`] slot (at most 30 frames per second).
fn wakes_now(important: bool, since_last: Duration) -> bool {
    important || since_last >= ENGINE_WAKE_INTERVAL
}

/// Results of background work, delivered to the UI thread. `None` means the
/// user cancelled a file dialog, which leaves the state unchanged.
enum AppEvent {
    /// An engine was chosen and spawned (or the spawn failed) for `slot`.
    EngineSpawned {
        slot: usize,
        outcome: Option<Result<EngineProcess, EngineError>>,
    },
    /// An opening book was read (or failed to read). `startup` marks the
    /// restored or `SILICONBOARD_BOOK` start-up book, whose path is not
    /// written back to the settings.
    BookLoaded {
        outcome: Option<(PathBuf, String, Result<PolyglotBook, BookError>)>,
        startup: bool,
    },
    /// The ECO opening-name table was indexed.
    OpeningNamesLoaded {
        outcome: Result<OpeningNames, OpeningError>,
    },
    /// A PGN file was read and parsed. `request` is the sequence number of the
    /// Open request; a result from an older request is stale and is ignored, so
    /// two quick opens cannot show the first file's game after the second one.
    PgnOpened {
        request: u64,
        outcome: Option<Result<(String, PgnCollection), OpenPgnError>>,
    },
    /// File was written.
    FileSaved {
        what: &'static str,
        outcome: Option<std::io::Result<PathBuf>>,
    },
    /// A board theme was decoded off the UI thread and is ready to upload.
    BoardThemeLoaded {
        theme: BoardTheme,
        outcome: Result<egui::ColorImage, AssetError>,
    },
    /// A piece set was rasterised off the UI thread and is ready to upload.
    PieceSetLoaded {
        set: PieceSet,
        outcome: Result<Vec<(PieceId, egui::ColorImage)>, AssetError>,
    },
    /// The settings file was written.
    SettingsSaved { outcome: Result<(), SettingsError> },
    /// The tournament dialog asked for an engine file.
    TournamentEnginePicked {
        side: TournamentSide,
        outcome: Option<EngineSpec>,
    },
    /// The tournament dialog asked for an EPD opening book.
    TournamentBookPicked { outcome: Option<PathBuf> },
    /// The tournament dialog asked for a PolyGlot `.bin` move book.
    TournamentMoveBookPicked { outcome: Option<PathBuf> },
    /// The tournament dialog asked for a PGN destination.
    TournamentPgnPicked { outcome: Option<PathBuf> },
    /// The Load-tablebases dialog's Browse picked tablebase folders (or was
    /// cancelled).
    TablebasesPicked { outcome: Option<Vec<PathBuf>> },
    /// Writing a finished tournament game to its PGN file failed.
    TournamentWriteFailed { message: String },
    /// The tournament's opening book was indexed.
    TournamentBookLoaded {
        generation: u64,
        outcome: Result<TournamentBook, String>,
    },
}

/// Why opening a PGN file failed.
#[derive(Debug)]
pub enum OpenPgnError {
    /// The file could not be read.
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// The file is not a readable PGN game.
    Parse(PgnError),
}

impl std::fmt::Display for OpenPgnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenPgnError::Read { path, source } => {
                write!(f, "could not read `{}`: {}", path.display(), source)
            }
            OpenPgnError::Parse(source) => write!(f, "{}", source),
        }
    }
}

impl std::error::Error for OpenPgnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenPgnError::Read { source, .. } => Some(source),
            OpenPgnError::Parse(source) => Some(source),
        }
    }
}

/// Where a GUI tournament is in its life cycle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TournamentPhase {
    /// Reading and indexing the EPD opening book.
    LoadingBook,
    /// Waiting for the two engines to finish their handshakes.
    LoadingEngines,
    /// The previous game is on screen; the next one starts shortly.
    BetweenGames,
    /// The current game is being played on the board.
    Playing,
    /// Every game is finished, or the user stopped the match.
    Finished,
}

/// A tournament driven by the app's normal game loop: one game at a time on
/// the main board, with the move list, clocks and engine cards live.
struct TournamentSession {
    /// The match configuration.
    config: TournamentConfig,
    /// Where the session is.
    phase: TournamentPhase,
    /// The opening book, once loaded.
    book: Option<TournamentBook>,
    /// One entry per game, built after the book is loaded.
    schedule: Vec<ScheduledGame>,
    /// 1-based number of the game being played (or about to be played).
    game_number: u32,
    /// Slot that plays White in the current game.
    white_slot: usize,
    /// Resign/draw adjudication for the current game.
    adjudicator: Adjudicator,
    /// Destination for finished games. Opened and truncated at start.
    pgn_file: std::fs::File,
    /// Progress and the final tally.
    status: TournamentStatus,
    /// When the next game may start, during [`TournamentPhase::BetweenGames`].
    next_game_due: Option<Instant>,
    /// Identifies this run, so a late book-load result cannot land in a newer
    /// tournament.
    generation: u64,
}

impl TournamentSession {
    /// Whether the tournament still owns the board.
    fn active(&self) -> bool {
        self.phase != TournamentPhase::Finished
    }

    /// Whether a game is on the board right now.
    fn is_playing(&self) -> bool {
        self.phase == TournamentPhase::Playing
    }

    /// The engine slot that plays `color` in the current game.
    fn slot_for_color(&self, color: PieceColor) -> usize {
        match color {
            PieceColor::White => self.white_slot,
            PieceColor::Black => 1 - self.white_slot,
        }
    }
}

/// The application state shared by every panel.
pub struct SiliconBoardApp {
    /// Tokio runtime for engines, file dialogs and background I/O.
    runtime: tokio::runtime::Runtime,
    /// Sender cloned into background tasks.
    events_tx: EventSender,
    /// Wakes the window when engine output arrives; shared by both readers.
    engine_wake: EngineWake,
    /// Receiver drained once per frame in [`SiliconBoardApp::logic`].
    events_rx: mpsc::UnboundedReceiver<AppEvent>,
    /// Textures, loaded on the first frame. `None` until then.
    assets: Option<Result<Assets, AssetError>>,
    /// The game: position, history and cursor.
    game: GameState,
    /// Who plays which side: analysis only, a human against engine A, or engine A against engine B.
    mode: GameMode,
    /// The game clock, when a time control is set. Runs only in a play mode.
    clock: Option<Clock>,
    /// The Game → Time control dialog, while it is open.
    time_dialog: Option<TimeControlDialog>,
    /// The Engine → Load tablebases… dialog, while it is open.
    tablebase_dialog: Option<TablebaseDialog>,
    /// Whether an engine on the move plays an opening-book move when one exists.
    book_in_games: bool,
    /// Chooses among the book moves by weight.
    book_picker: BookPicker,
    /// For each slot being restarted after a stall: whether analysis was on.
    restarting: [Option<bool>; 2],
    /// When each slot was restarted after a stall or an exit, newest last.
    restart_times: [Vec<Instant>; 2],
    /// Whether an unexpected exit in this slot should be restarted. Cleared
    /// when an engine fails its handshake or keeps failing, until a new engine
    /// is loaded.
    engine_auto_restart: [bool; 2],
    /// How the game ended by resignation or agreement. A flag is read from the clock.
    ending: Option<Ending>,
    /// `true` from Start until Abort, a game end, or a new game. A play-mode
    /// game does nothing until then: no engine searches and no clock runs.
    game_started: bool,
    /// UCI option values the user chose, per slot. Sent again when the engine restarts.
    engine_settings: [BTreeMap<String, String>; 2],
    /// The engine options window, while it is open.
    options_dialog: Option<OptionsDialog>,
    /// The slot whose engine is evaluating a pending draw offer.
    draw_offer: Option<usize>,
    /// What is kept between runs. Changed here and written back on each change.
    settings: Settings,
    /// The window size last written to the settings, so it is saved only when it changes.
    saved_window_size: Option<settings::WindowSize>,
    /// Whether the game was live on the last frame, to notice it ending.
    game_was_live: bool,
    /// Where `settings` is kept. `None` when there is no home folder.
    settings_file: Option<PathBuf>,
    /// A settings write is in flight.
    settings_saving: bool,
    /// The settings changed while a write was in flight; save again after it.
    settings_save_pending: bool,
    /// When the debounced window/panel save is due.
    settings_save_due: Option<Instant>,
    /// Board widget state (interaction, orientation).
    view: BoardView,
    /// The Setup Board editor, while it is open. The right dashboard shows it
    /// in place of the engine cards and the board edits this position instead
    /// of playing moves.
    setup: Option<SetupBoard>,
    /// The editor revision last copied into the top FEN field. The field is
    /// left alone while the user types in it, and refreshed only when the
    /// editor's position actually changed.
    setup_fen_revision: u64,
    /// Engine slots: A and B.
    engines: [Option<EngineHost>; 2],
    /// `true` while a file dialog or spawn for a slot is in flight.
    engine_loading: [bool; 2],
    /// Whether analysis is switched on for each slot (Start, Abort or the card's toggle).
    analysis_on: [bool; 2],
    /// The loaded opening book and its file name.
    book: Option<(String, PolyglotBook)>,
    /// Incremented whenever `book` is replaced, so the memo below is not reused.
    book_generation: u64,
    /// The book moves of the last position asked for, with the book they came
    /// from (`(is_tournament, generation)`) and the FEN.
    book_memo: RefCell<Option<BookMemo>>,
    /// ECO opening names, keyed by position.
    opening_names: Option<OpeningNames>,
    /// The most recent ECO name at or before the displayed position. Kept
    /// after the line leaves the named part of the table, so the last opening
    /// stays on screen.
    opening_name: Option<String>,
    /// Book moves for the displayed position, heaviest first.
    book_moves: Vec<(String, u16)>,
    /// FEN the engines and book were last synchronised to.
    synced_fen: String,
    /// Cursor position on the previous frame, to drive move-list auto-scroll.
    last_ply: usize,
    /// Status line shown in the evaluation panel.
    status: String,
    /// Severity of the status line.
    status_level: StatusLevel,
    /// Last engine status seen per slot, to report start-up outcomes once.
    engine_status: [Option<AnalysisStatus>; 2],
    /// When each slot's engine was spawned, for the handshake timeout.
    engine_started: [Option<Instant>; 2],
    /// Why the last spawn of a slot failed, so a tournament can name the
    /// reason instead of only saying that an engine could not be started.
    engine_spawn_error: [Option<String>; 2],
    /// The FEN text in the top bar. Follows the position unless the user edits it.
    fen_input: String,
    /// Board images found in `public/images/board/`, for the View menu.
    board_themes: Vec<BoardTheme>,
    /// Piece sets found in `public/piece/`, for the View menu.
    piece_sets: Vec<PieceSet>,
    /// Per-slot SAN cache for the engine principal variations.
    pv_cache: [Option<PvCache>; 2],
    /// Cached move list plus the history revision it was built from.
    pgn_cache: Option<(u64, Vec<MoveLine>)>,
    /// Cached SAN layouts for the move list, keyed by revision and figurine setting.
    pgn_galleys: Option<PgnGalleyCache>,
    /// Frame-time measurement, when `SILICONBOARD_PERF` is set.
    frame_timer: Option<FrameTimer>,
    /// When this frame's `logic` began, so `ui` can report the frame cost.
    frame_started: Option<Instant>,
    /// The tournament configuration window, while it is open.
    tournament_dialog: Option<TournamentDialog>,
    /// The running (or last) tournament.
    tournament: Option<TournamentSession>,
    /// Counter used to reject late results from an older tournament.
    tournament_generation: u64,
    /// Sequence number of the latest Open PGN request. A result whose number
    /// is older than this belongs to a file the user has already replaced.
    pgn_request: u64,
    /// A multi-game PGN whose games are listed on the right panel.
    pgn_collection: Option<PgnCollection>,
    /// Selected game in `pgn_collection` (zero-based).
    pgn_browser_selected: usize,
    /// `Termination` reason from the loaded PGN, shown next to the result in
    /// the bottom move list.
    pgn_termination: Option<String>,
    /// The `White` and `Black` names of the loaded PGN, shown on the player
    /// bars in analysis. Cleared by anything that starts a game of its own.
    pgn_players: Option<(String, String)>,
    /// Whether the PGN games list is visible in the right panel.
    pgn_browser_list_open: bool,
    /// Whether the tournament window is visible.
    tournament_window_open: bool,
    /// Whether the small result pop-up should be shown.
    tournament_finished_popup: bool,
}

impl SiliconBoardApp {
    /// Create the application with the given Tokio runtime, and start the
    /// background loads: the user's book from the settings (none by
    /// default), the opening names, and the engines saved in the settings.
    pub fn new(
        runtime: tokio::runtime::Runtime,
        ctx: egui::Context,
        settings: Settings,
        settings_file: Option<PathBuf>,
    ) -> Self {
        let mut app = Self::unloaded(runtime, ctx);
        app.settings_file = settings_file;
        // The list is needed before the saved theme is validated.
        app.board_themes = crate::assets::board_themes();
        app.piece_sets = crate::assets::piece_sets();
        // The settings were read before the window existed (for its size).
        app.apply_settings(settings);
        app.load_startup_book();
        app.load_startup_pgn();
        app.load_opening_names();
        app
    }

    /// Take `settings`: the book switch, the time control, and the engines
    /// loaded last time, which are started again but not started searching.
    fn apply_settings(&mut self, settings: Settings) {
        self.book_in_games = settings.book_in_games;
        self.clock = settings
            .time_control
            .map(|control| Clock::new(control, self.game.turn()));
        self.saved_window_size = settings.window_size;
        let engines = settings.engines.clone();
        self.settings = settings;
        // A theme that is no longer on disk (or a malformed value) falls back
        // to the default board.
        if let Some(theme) = &self.settings.board_theme {
            if !self.board_themes.iter().any(|known| &known.file == theme) {
                log::warn!(
                    "saved board theme `{}` is not available; using the default",
                    theme
                );
                self.settings.board_theme = None;
            }
        }
        if let Some(set) = &self.settings.piece_set {
            if !self.piece_sets.iter().any(|known| &known.name == set) {
                log::warn!(
                    "saved piece set `{}` is not available; using the default",
                    set
                );
                self.settings.piece_set = None;
            }
        }
        for (slot, path) in engines.into_iter().enumerate() {
            if let Some(path) = path {
                self.spawn_engine_file(slot, path);
            }
        }
    }

    /// The application with no background loads started.
    fn unloaded(runtime: tokio::runtime::Runtime, ctx: egui::Context) -> Self {
        let (tx, events_rx) = mpsc::unbounded_channel();
        let events_tx = EventSender { tx, ctx };
        let engine_wake = engine_wake_for(&events_tx);
        Self {
            runtime,
            events_tx,
            engine_wake,
            events_rx,
            assets: None,
            game: GameState::new(),
            mode: GameMode::Analysis,
            clock: None,
            time_dialog: None,
            tablebase_dialog: None,
            book_in_games: false,
            book_picker: BookPicker::default(),
            restarting: [None, None],
            restart_times: [Vec::new(), Vec::new()],
            engine_auto_restart: [false, false],
            ending: None,
            game_started: false,
            engine_settings: [BTreeMap::new(), BTreeMap::new()],
            options_dialog: None,
            draw_offer: None,
            settings: Settings::default(),
            saved_window_size: None,
            game_was_live: false,
            settings_file: None,
            settings_saving: false,
            settings_save_pending: false,
            settings_save_due: None,
            view: BoardView::default(),
            setup: None,
            setup_fen_revision: 0,
            engines: [None, None],
            engine_loading: [false, false],
            analysis_on: [false, false],
            book: None,
            book_generation: 0,
            book_memo: RefCell::new(None),
            opening_names: None,
            opening_name: None,
            book_moves: Vec::new(),
            synced_fen: String::new(),
            last_ply: 0,
            status: "Welcome — click or drag a piece to make a move.".to_string(),
            status_level: StatusLevel::Info,
            engine_status: [None, None],
            engine_started: [None, None],
            engine_spawn_error: [None, None],
            fen_input: GameState::new().fen(),
            board_themes: Vec::new(),
            piece_sets: Vec::new(),
            pv_cache: [None, None],
            pgn_cache: None,
            pgn_galleys: None,
            frame_timer: std::env::var_os("SILICONBOARD_PERF")
                .map(|_| FrameTimer::new_at(Instant::now())),
            frame_started: None,
            tournament_dialog: None,
            tournament: None,
            tournament_generation: 0,
            pgn_request: 0,
            pgn_collection: None,
            pgn_browser_selected: 0,
            pgn_termination: None,
            pgn_players: None,
            pgn_browser_list_open: false,
            tournament_window_open: false,
            tournament_finished_popup: false,
        }
    }

    // ── Per-frame work ─────────────────────────────────────────────────────

    /// Non-visual work for the frame: load assets once, apply background
    /// results, pump engine output, keep engines in sync, handle arrow keys.
    fn logic_frame(&mut self, ctx: &egui::Context) {
        self.frame_started = Some(Instant::now());
        if self
            .settings_save_due
            .is_some_and(|due| Instant::now() >= due)
        {
            self.settings_save_due = None;
            self.save_settings();
        }
        self.save_window_size(ctx);
        if self.assets.is_none() {
            let loaded = Assets::load_with(
                ctx,
                self.settings.board_theme.as_deref(),
                self.settings.piece_set.as_deref(),
            );
            if let Err(err) = &loaded {
                log::error!("asset load failed: {}", err);
            }
            self.assets = Some(loaded);
        }
        self.drain_app_events(ctx);
        let mut engines_changed = false;
        for host in self.engines.iter_mut().flatten() {
            engines_changed |= host.pump();
        }
        if engines_changed {
            ctx.request_repaint();
        }
        // Engine output wakes the window from the reader task, so a frame runs
        // as soon as there is new analysis. The watchdog of a running search
        // lives in the engine's own monitor task, which wakes the window when
        // it declares a stall. Only the handshake timeout still needs a timer,
        // so a searching engine no longer costs two extra frames per second
        // (BUG-7-20). An idle loaded engine costs no frames either.
        let engine_tick_needed = self
            .engines
            .iter()
            .flatten()
            .any(|host| engine_tick_needed(host.status()));
        if engine_tick_needed {
            ctx.request_repaint_after(ENGINE_TICK);
        }
        self.report_stalled_engines();
        self.play_engine_moves();
        self.adjudicate_engine_vs_engine();
        self.finish_draw_offer();
        self.play_book_reply(ctx);
        // A game that ends (or is replaced) switches its engines off, so each
        // card's button reads Start again and the engine rests.
        let live = self.live_play();
        if self.game_was_live && !live {
            for &slot in self.mode.engine_slots() {
                self.set_engine_analysis(slot, false);
            }
        }
        self.game_was_live = live;
        self.sync_clock(ctx);
        self.report_engine_start_up();
        self.report_exited_engines();
        self.tournament_tick(ctx);
        self.sync_position(false);
        self.handle_navigation_keys(ctx);
    }

    /// How many engines are searching right now.
    fn searching_engines(&self) -> usize {
        self.engines
            .iter()
            .flatten()
            .filter(|host| host.status() == AnalysisStatus::Thinking)
            .count()
    }

    /// Record this frame's `logic` + `ui` cost and log a one-second summary
    /// while an engine searches. `SILICONBOARD_PERF` turns it on.
    fn record_frame(&mut self) {
        let Some(started) = self.frame_started.take() else {
            return;
        };
        if self.frame_timer.is_none() {
            return;
        }
        let cost = started.elapsed();
        let searching = self.searching_engines();
        let summary = self
            .frame_timer
            .as_mut()
            .and_then(|timer| timer.record_at(Instant::now(), cost, searching > 0));
        let Some(summary) = summary else {
            return;
        };
        let nps: u64 = self
            .engines
            .iter()
            .flatten()
            .filter_map(|host| host.view().stats.nps)
            .sum();
        log::info!(
            "frame: {:.2} ms cost (worst {:.2} ms), {:.0} ms gap (worst {:.0} ms); {} frames, {} searching, {:.2} Mn/s",
            summary.average_cost.as_secs_f64() * 1e3,
            summary.worst_cost.as_secs_f64() * 1e3,
            summary.average_gap.as_secs_f64() * 1e3,
            summary.worst_gap.as_secs_f64() * 1e3,
            summary.frames,
            searching,
            nps as f64 / 1e6,
        );
    }

    /// Play the move of every engine whose move search has finished. The move
    /// was searched for the displayed position, so it is played only if it is
    /// still legal there, and only if the game has not been decided on the clock.
    fn play_engine_moves(&mut self) {
        // The board is being edited: a bestmove still in flight belongs to the
        // old position and must not be played into it.
        if self.setup.is_some() {
            return;
        }
        for slot in 0..2 {
            let Some(uci) = self.engines[slot]
                .as_mut()
                .and_then(EngineHost::take_engine_move)
            else {
                continue;
            };
            if self.clock_flagged().is_some() {
                continue;
            }
            match self.game.apply_uci(&uci) {
                Ok(san) => {
                    self.view.clear_selection();
                    self.tournament_observe_move(slot);
                    let mut text = format!("Engine {} played {}.", slot_letter(slot), san);
                    if let Some(result) = self.result_text() {
                        text.push_str(&format!(" {}.", result));
                    }
                    self.set_status(StatusLevel::Info, text);
                }
                Err(err) => {
                    log::error!("engine {} sent `{}`: {}", slot_letter(slot), uci, err);
                    self.set_status(
                        StatusLevel::Error,
                        format!(
                            "Engine {} sent {}, which is not legal here: {}",
                            slot_letter(slot),
                            uci,
                            err
                        ),
                    );
                }
            }
        }
    }

    /// Follow each engine from spawn to ready. An engine that exits, or stays
    /// silent past `HANDSHAKE_TIMEOUT`, before answering `uci` is not a UCI
    /// engine: it is shut down and the status line says so. Later changes
    /// (Abort, exit) do not touch the status line.
    fn report_engine_start_up(&mut self) {
        for slot in 0..2 {
            let letter = slot_letter(slot);
            let timed_out =
                self.engine_started[slot].is_some_and(|t| t.elapsed() > HANDSHAKE_TIMEOUT);
            let failure = match self.engines[slot].as_mut() {
                Some(host) if host.status() == AnalysisStatus::Starting => {
                    // The failure report names the exit status and the engine's last
                    // message, which is usually the reason (a missing file, a
                    // missing interpreter, a missing data file).
                    let reason = if host.has_exited() {
                        Some(format!(
                            "it exited before answering `uci`; {}",
                            host.failure_detail()
                        ))
                    } else if timed_out {
                        Some(format!(
                            "it did not answer `uci` within {} seconds; {}",
                            HANDSHAKE_TIMEOUT.as_secs(),
                            host.failure_detail()
                        ))
                    } else {
                        None
                    };
                    reason.map(|reason| (host.shutdown(), host.view().binary_name, reason))
                }
                _ => None,
            };
            if let Some((process, binary, reason)) = failure {
                self.quit_processes(vec![process]);
                self.engine_auto_restart[slot] = false;
                self.engine_status[slot] = Some(AnalysisStatus::Offline);
                self.engine_started[slot] = None;
                self.analysis_on[slot] = false;
                log::error!(
                    "engine {} is not a UCI engine: {} ({})",
                    letter,
                    binary,
                    reason
                );
                self.set_status(
                    StatusLevel::Error,
                    format!(
                        "Engine {} is not a UCI engine: {} ({}).",
                        letter, binary, reason
                    ),
                );
                continue;
            }

            let Some(host) = &self.engines[slot] else {
                self.engine_status[slot] = None;
                self.engine_started[slot] = None;
                continue;
            };
            let now = host.view();
            let before = self.engine_status[slot];
            if before == Some(now.status) {
                continue;
            }
            if before == Some(AnalysisStatus::Starting)
                && matches!(now.status, AnalysisStatus::Ready | AnalysisStatus::Thinking)
            {
                log::info!(
                    "engine {} ready: {} ({})",
                    letter,
                    now.name.as_deref().unwrap_or(&now.binary_name),
                    now.binary_name
                );
                self.set_status(
                    StatusLevel::Info,
                    format!(
                        "Engine {} ready: {}",
                        letter,
                        now.name.as_deref().unwrap_or(&now.binary_name)
                    ),
                );
            }
            self.engine_status[slot] = Some(now.status);
        }
    }

    /// Apply every background result that arrived since the last frame.
    fn drain_app_events(&mut self, ctx: &egui::Context) {
        let mut received = false;
        while let Ok(event) = self.events_rx.try_recv() {
            received = true;
            match event {
                AppEvent::EngineSpawned { slot, outcome } => {
                    self.engine_loading[slot] = false;
                    match outcome {
                        None => self.set_status(
                            StatusLevel::Info,
                            "Engine selection cancelled.".to_string(),
                        ),
                        Some(Ok(process)) => {
                            self.engine_spawn_error[slot] = None;
                            let binary = process.binary().display().to_string();
                            let path = process.binary().to_path_buf();
                            if let Some(mut old) = self.engines[slot].take() {
                                let stale = old.shutdown();
                                self.quit_processes(vec![stale]);
                            }
                            let restart = self.restarting[slot].take();
                            if restart.is_none() {
                                // A different engine: its own saved values apply, not the old engine's.
                                self.engine_settings[slot] = self
                                    .settings
                                    .engine_options
                                    .get(&binary)
                                    .cloned()
                                    .unwrap_or_default();
                                self.restart_times[slot].clear();
                                self.close_options_dialog(slot);
                                self.remember_engine(&path);
                                self.settings.engines[slot] = Some(path);
                                self.save_settings();
                            }
                            self.engines[slot] = Some(EngineHost::attach_with_settings(
                                process,
                                engine_startup_settings(
                                    &self.engine_settings[slot],
                                    &self.settings,
                                ),
                            ));
                            self.engine_auto_restart[slot] = true;
                            self.engine_status[slot] = None;
                            self.engine_started[slot] = Some(Instant::now());
                            let playing = self.mode.engine_slots().contains(&slot);
                            let was_analysing = restart == Some(true);
                            self.set_engine_analysis(slot, playing || was_analysing);
                            let text = match restart {
                                Some(_) => {
                                    format!("Engine {} restarted: {}…", slot_letter(slot), binary)
                                }
                                None => {
                                    format!("Loading engine {}: {}…", slot_letter(slot), binary)
                                }
                            };
                            self.set_status(StatusLevel::Info, text);
                        }
                        Some(Err(err)) => {
                            self.restarting[slot] = None;
                            self.engine_spawn_error[slot] = Some(err.to_string());
                            log::error!("engine spawn failed: {}", err);
                            self.set_status(
                                StatusLevel::Error,
                                format!("Could not start engine {}: {}", slot_letter(slot), err),
                            );
                        }
                    }
                }
                AppEvent::BookLoaded { outcome, startup } => match outcome {
                    None => {
                        self.set_status(StatusLevel::Info, "Book selection cancelled.".to_string())
                    }
                    Some((_, name, Ok(book))) if book.is_empty() => {
                        self.set_status(
                            StatusLevel::Warning,
                            format!("Opening book {} has no records.", name),
                        );
                    }
                    Some((path, name, Ok(book))) => {
                        self.set_status(
                            StatusLevel::Info,
                            format!("Opening book loaded: {} ({} records)", name, book.len()),
                        );
                        self.book = Some((name, book));
                        self.book_generation += 1;
                        // The user's choice is kept for the next launch. A
                        // restored or `SILICONBOARD_BOOK` start-up book is
                        // not written back.
                        if !startup && self.settings.book_path.as_ref() != Some(&path) {
                            self.settings.book_path = Some(path);
                            self.save_settings();
                        }
                        self.sync_position(true);
                    }
                    Some((_, name, Err(err))) => {
                        self.set_status(
                            StatusLevel::Error,
                            format!("Could not load book {}: {}", name, err),
                        );
                    }
                },
                AppEvent::TablebasesPicked { outcome } => match outcome {
                    None => {
                        self.set_status(
                            StatusLevel::Info,
                            "Tablebase folder selection cancelled.".to_string(),
                        );
                    }
                    Some(dirs) => {
                        // The folders replace the field text in the open
                        // dialog; the user applies them.
                        if let Some(dialog) = self.tablebase_dialog.as_mut() {
                            dialog.set_draft_dirs(&dirs);
                        }
                    }
                },
                AppEvent::OpeningNamesLoaded { outcome } => match outcome {
                    Ok(names) if names.is_empty() => {
                        log::warn!("opening names table is empty");
                    }
                    Ok(names) => {
                        log::info!("opening names indexed: {} positions", names.len());
                        self.opening_names = Some(names);
                        self.opening_name = self.opening_name_for_display();
                    }
                    Err(err) => log::warn!("opening names not loaded: {}", err),
                },
                AppEvent::PgnOpened { request, outcome } => {
                    if request != self.pgn_request {
                        log::debug!(
                            "ignoring a PGN result from request {request}; the latest is {}",
                            self.pgn_request
                        );
                        continue;
                    }
                    match outcome {
                        None => self.set_status(StatusLevel::Info, "Open cancelled.".to_string()),
                        Some(Ok((name, mut collection))) => {
                            collection.name = name.clone();
                            if collection.is_multi_game() {
                                let total = collection.games.len();
                                self.pgn_collection = Some(collection);
                                self.pgn_browser_selected = 0;
                                self.pgn_browser_list_open = true;
                                self.select_pgn_game(0);
                                self.set_status(
                                StatusLevel::Info,
                                format!(
                                    "Opened {} — {} games. Pick a game in the games list on the right.",
                                    name, total
                                ),
                            );
                            } else {
                                self.pgn_collection = None;
                                self.pgn_browser_selected = 0;
                                self.pgn_browser_list_open = false;
                                match collection.load_game(0) {
                                    Ok(import) => self.load_pgn_game(&name, import, None),
                                    Err(err) => {
                                        self.set_status(StatusLevel::Error, err.to_string())
                                    }
                                }
                            }
                        }
                        Some(Err(err)) => {
                            log::error!("PGN open failed: {}", err);
                            self.set_status(StatusLevel::Error, err.to_string());
                        }
                    }
                }
                AppEvent::BoardThemeLoaded { theme, outcome } => match outcome {
                    Ok(image) => {
                        let handle =
                            ctx.load_texture(&theme.file, image, egui::TextureOptions::LINEAR);
                        if let Some(Ok(assets)) = &mut self.assets {
                            assets.board = BoardTexture::from_handle(handle);
                        }
                        self.settings.board_theme = Some(theme.file.clone());
                        self.save_settings();
                        self.set_status(
                            StatusLevel::Info,
                            format!("Board theme: {}.", theme.label),
                        );
                        ctx.request_repaint();
                    }
                    Err(err) => {
                        log::error!("board theme {} failed: {}", theme.file, err);
                        self.set_status(
                            StatusLevel::Error,
                            format!("Could not load board theme {}: {}", theme.label, err),
                        );
                    }
                },
                AppEvent::PieceSetLoaded { set, outcome } => match outcome {
                    Ok(images) => {
                        if let Some(Ok(assets)) = &mut self.assets {
                            assets.pieces = crate::assets::upload_pieces(ctx, images);
                        }
                        self.settings.piece_set = Some(set.name.clone());
                        self.save_settings();
                        self.set_status(StatusLevel::Info, format!("Piece set: {}.", set.label));
                        ctx.request_repaint();
                    }
                    Err(err) => {
                        log::error!("piece set {} failed: {}", set.name, err);
                        self.set_status(
                            StatusLevel::Error,
                            format!("Could not load piece set {}: {}", set.label, err),
                        );
                    }
                },
                AppEvent::SettingsSaved { outcome } => {
                    self.settings_saving = false;
                    if let Err(err) = outcome {
                        log::error!("{}", err);
                        self.set_status(StatusLevel::Error, format!("Settings not saved: {}", err));
                    }
                    if self.settings_save_pending {
                        self.settings_save_pending = false;
                        self.save_settings();
                    }
                }
                AppEvent::FileSaved { what, outcome } => match outcome {
                    None => self.set_status(StatusLevel::Info, format!("{} save cancelled.", what)),
                    Some(Ok(path)) => self.set_status(
                        StatusLevel::Info,
                        format!("Saved {} to {}", what, path.display()),
                    ),
                    Some(Err(err)) => {
                        log::error!("saving {} failed: {}", what, err);
                        self.set_status(
                            StatusLevel::Error,
                            format!("Could not save {}: {}", what, err),
                        );
                    }
                },
                AppEvent::TournamentEnginePicked { side, outcome } => match outcome {
                    None => self.set_status(
                        StatusLevel::Info,
                        format!("{} engine selection cancelled.", side.label()),
                    ),
                    Some(spec) => {
                        if let Some(dialog) = self.tournament_dialog.as_mut() {
                            dialog.set_engine(side, spec.clone());
                        }
                        self.set_status(
                            StatusLevel::Info,
                            format!("{} engine: {}.", side.label(), spec.name),
                        );
                    }
                },
                AppEvent::TournamentBookPicked { outcome } => match outcome {
                    None => self.set_status(
                        StatusLevel::Info,
                        "Opening-book selection cancelled.".to_string(),
                    ),
                    Some(path) => {
                        if let Some(dialog) = self.tournament_dialog.as_mut() {
                            dialog.set_book_path(path.clone());
                        }
                        self.set_status(
                            StatusLevel::Info,
                            format!("Opening book: {}.", path.display()),
                        );
                    }
                },
                AppEvent::TournamentMoveBookPicked { outcome } => match outcome {
                    None => self.set_status(
                        StatusLevel::Info,
                        "Move-book selection cancelled.".to_string(),
                    ),
                    Some(path) => {
                        if let Some(dialog) = self.tournament_dialog.as_mut() {
                            dialog.set_move_book_path(path.clone());
                        }
                        self.set_status(
                            StatusLevel::Info,
                            format!("Move book: {}.", path.display()),
                        );
                    }
                },
                AppEvent::TournamentPgnPicked { outcome } => match outcome {
                    None => self.set_status(
                        StatusLevel::Info,
                        "PGN destination selection cancelled.".to_string(),
                    ),
                    Some(path) => {
                        if let Some(dialog) = self.tournament_dialog.as_mut() {
                            dialog.set_pgn_path(path.clone());
                        }
                        self.set_status(
                            StatusLevel::Info,
                            format!("Tournament PGN: {}.", path.display()),
                        );
                    }
                },
                AppEvent::TournamentWriteFailed { message } => {
                    log::error!("tournament PGN write failed: {}", message);
                    self.fail_tournament(format!("Could not write the PGN file: {}", message));
                }
                AppEvent::TournamentBookLoaded {
                    generation,
                    outcome,
                } => {
                    self.handle_tournament_book_loaded(generation, outcome);
                }
            }
        }
        if received {
            ctx.request_repaint();
        }
    }

    /// Push the displayed position to the engines and recompute the book
    /// suggestions, but only when the position actually changed (or `force`).
    fn sync_position(&mut self, force: bool) {
        let fen = self.game.fen();
        if !force && fen == self.synced_fen {
            return;
        }
        for slot in 0..2 {
            let goal = self.goal_for(slot);
            if let Some(host) = self.engines[slot].as_mut() {
                host.set_position(&fen, goal);
            }
        }
        self.book_moves = match &self.book {
            Some((_, book)) => self.game.book_suggestions(book),
            None => Vec::new(),
        };
        self.opening_name = self.opening_name_for_display();
        self.fen_input = fen.clone();
        self.synced_fen = fen;
    }

    /// The latest ECO name at or before the displayed position. Scanning back
    /// keeps the last opening name when the line leaves the part of the table
    /// that has names, which is what the book box shows.
    fn opening_name_for_display(&self) -> Option<String> {
        let names = self.opening_names.as_ref()?;
        for ply in self.game.active_history().iter().rev() {
            if let Some(name) = names.name_for(&ply.after) {
                return Some(name.to_string());
            }
        }
        names
            .name_for(&self.game.start_position())
            .map(str::to_string)
    }

    /// ← / → step through the game; Home / End jump to the ends.
    fn handle_navigation_keys(&mut self, ctx: &egui::Context) {
        let (left, right, home, end) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
            )
        });
        if left {
            self.game.undo();
        }
        if right {
            self.game.redo();
        }
        if home {
            self.game.goto_start();
        }
        if end {
            self.game.goto_end();
        }
        if left || right || home || end {
            self.view.clear_selection();
        }
    }

    // ── Actions ────────────────────────────────────────────────────────────

    /// Perform a menu or FEN-bar action.
    fn perform(&mut self, action: MenuAction, ctx: &egui::Context) {
        if self
            .tournament
            .as_ref()
            .is_some_and(TournamentSession::active)
            && action_conflicts_with_tournament(action)
        {
            self.set_status(
                StatusLevel::Warning,
                "Stop the tournament before changing the game or the engines.".to_string(),
            );
            return;
        }
        if self.setup.is_some() && action == MenuAction::LoadFen {
            self.load_setup_fen();
            return;
        }
        if self.setup.is_some() && action_conflicts_with_setup(action) {
            self.set_status(
                StatusLevel::Warning,
                "Finish or cancel Setup Board first.".to_string(),
            );
            return;
        }
        match action {
            MenuAction::NewGame => {
                // A fresh game is not part of the loaded PGN: the games list
                // closes.
                self.clear_pgn_browser();
                self.game = GameState::new();
                self.ending = None;
                self.pgn_termination = None;
                self.pgn_players = None;
                self.game_started = false;
                self.reset_clock();
                self.view.clear_selection();
                for host in self.engines.iter_mut().flatten() {
                    host.new_game();
                }
                self.set_status(StatusLevel::Info, "New game.".to_string());
            }
            MenuAction::OpenSetupBoard => self.open_setup_board(),
            MenuAction::StartFromFen => self.start_from_fen(),
            MenuAction::FlipBoard => self.view.flip(),
            MenuAction::SetMode(mode) => self.set_mode(mode),
            MenuAction::ToggleBookInGames => {
                self.book_in_games = !self.book_in_games;
                self.settings.book_in_games = self.book_in_games;
                self.save_settings();
                let text = if self.book_in_games {
                    "Book moves in games: on.".to_string()
                } else {
                    "Book moves in games: off.".to_string()
                };
                self.set_status(StatusLevel::Info, text);
                self.sync_position(true);
            }
            MenuAction::Resign => self.resign(),
            MenuAction::OpenEngineOptions(slot) => self.open_engine_options(slot),
            MenuAction::OfferDraw => self.offer_draw(),
            MenuAction::ClaimThreefold => self.claim_threefold(),
            MenuAction::OpenTimeControl => {
                self.time_dialog = Some(TimeControlDialog::new(
                    self.clock.as_ref().map(Clock::control),
                ));
            }
            MenuAction::OpenPgn => self.open_pgn(),
            MenuAction::ShowPgnGames => {
                if self.pgn_collection.is_some() {
                    self.pgn_browser_list_open = true;
                } else {
                    self.set_status(
                        StatusLevel::Warning,
                        "Open a PGN with more than one game first.".to_string(),
                    );
                }
            }
            MenuAction::SavePgn => {
                let text = pgn::to_pgn(&self.game, self.current_ending());
                self.save_text("PGN", text, "game.pgn", "PGN", &["pgn"]);
            }
            MenuAction::CopyPgn => {
                ctx.copy_text(pgn::to_pgn(&self.game, self.current_ending()));
                self.set_status(
                    StatusLevel::Info,
                    "PGN copied to the clipboard.".to_string(),
                );
            }
            MenuAction::CopyFen => {
                ctx.copy_text(self.game.fen());
                self.set_status(
                    StatusLevel::Info,
                    "FEN copied to the clipboard.".to_string(),
                );
            }
            MenuAction::SaveFen => {
                let text = format!("{}\n", self.game.fen());
                self.save_text("FEN", text, "position.fen", "FEN", &["fen"]);
            }
            MenuAction::LoadEngine(slot) => self.load_engine(slot),
            MenuAction::ChooseEngine { slot, choice } => self.choose_engine(slot, choice),
            MenuAction::ToggleEngine(slot) => self.toggle_engine(slot),
            MenuAction::StartAnalysis => self.set_analysis(true),
            MenuAction::StopAnalysis => {
                // The red Stop button is the obvious way out of a running
                // match, especially once the user has closed the tournament
                // window. Stopping only the searches would leave the match
                // owning the board and unable to continue, so it stops the
                // whole tournament (completed games stay in the PGN).
                if self
                    .tournament
                    .as_ref()
                    .is_some_and(TournamentSession::active)
                {
                    self.stop_tournament();
                } else {
                    self.set_analysis(false);
                }
            }
            MenuAction::LoadFen => match GameState::from_fen(self.fen_input.trim()) {
                Ok(game) => {
                    self.game = game;
                    self.ending = None;
                    self.pgn_termination = None;
                    self.pgn_players = None;
                    self.game_started = false;
                    self.reset_clock();
                    self.view.clear_selection();
                    self.set_status(StatusLevel::Info, "Position loaded from FEN.".to_string());
                }
                Err(err) => self.set_status(StatusLevel::Error, format!("Invalid FEN: {}", err)),
            },
            MenuAction::LoadBook => self.load_book(),
            MenuAction::OpenTablebases => {
                self.tablebase_dialog = Some(TablebaseDialog::new(
                    self.settings.tablebase_path.as_deref(),
                ));
            }
            MenuAction::ToggleTablebases => self.toggle_tablebases(),
            MenuAction::SetBoardTheme(index) => self.set_board_theme(index),
            MenuAction::SetPieceSet(index) => self.set_piece_set(index),
            MenuAction::ToggleFigurinesMoveList => self.toggle_figurines(false),
            MenuAction::ToggleFigurinesEval => self.toggle_figurines(true),
            MenuAction::ToggleAnalysisArrows => self.toggle_analysis_arrows(),
            MenuAction::OpenTournament => self.open_tournament_dialog(),
            MenuAction::StopTournament => self.stop_tournament(),
            MenuAction::ShowTournament => {
                self.tournament_window_open = true;
                self.tournament_finished_popup = false;
            }
            MenuAction::ToggleSessionLog => self.toggle_session_log(),
        }
    }

    /// Apply the move (or selection change) the board widget requested.
    fn apply_board_action(&mut self, action: BoardAction) {
        match action {
            BoardAction::None => {}
            BoardAction::PieceSelected { from } => {
                self.set_status(StatusLevel::Info, format!("Selected {}.", from));
            }
            BoardAction::RequestMove { m } => match self.game.apply_move(m) {
                Ok(()) => {
                    let played = self
                        .game
                        .active_history()
                        .last()
                        .map(|ply| ply.san.to_string())
                        .unwrap_or_default();
                    let text = match self.result_text() {
                        Some(result) => format!("Played {}. {}.", played, result),
                        None => format!("Played {}.", played),
                    };
                    self.set_status(StatusLevel::Info, text);
                }
                Err(err) => self.set_status(StatusLevel::Error, format!("Move rejected: {}", err)),
            },
            BoardAction::PromotionChoice { .. } => {
                self.set_status(
                    StatusLevel::Info,
                    "Promotion: pick Q / R / B / N on the board (Esc cancels).".to_string(),
                );
            }
        }
    }

    /// Replace an engine that has stopped answering `isready`. Its process is
    /// quit and the same file is started again, and the game continues from
    /// the displayed position. Once the slot has used up its restarts
    /// ([`RESTART_LIMIT`] within [`RESTART_WINDOW`]), the engine stays offline
    /// instead, and a tournament match stops.
    fn report_stalled_engines(&mut self) {
        for slot in 0..2 {
            // A stalled host is handled whether it is still live or already
            // offline: the monitor task kills the process the moment it
            // declares the stall, so the next frame usually sees an offline
            // host — skipping it here would leave the engine offline forever,
            // its card frozen on the last evaluation and its analysis arrow
            // gone.
            let stalled = self.engines[slot]
                .as_ref()
                .is_some_and(EngineHost::is_stalled);
            if !stalled {
                continue;
            }
            let Some(host) = self.engines[slot].as_mut() else {
                continue;
            };
            let binary = host.binary_path();
            let reason = host.stall_reason().map_or_else(
                || "it stopped answering".to_string(),
                |reason| reason.to_string(),
            );
            let was_analysing = self.analysis_on[slot];
            let process = host.shutdown();
            self.close_options_dialog(slot);
            self.quit_processes(vec![process]);
            self.analysis_on[slot] = false;
            self.engine_status[slot] = None;
            self.engine_started[slot] = None;

            if !self.may_restart(slot) {
                self.engine_auto_restart[slot] = false;
                self.restart_times[slot].clear();
                log::error!(
                    "engine {} stalled ({}) after {} restarts in {} minutes; leaving it offline",
                    binary.display(),
                    reason,
                    RESTART_LIMIT,
                    RESTART_WINDOW.as_secs() / 60
                );
                self.set_status(
                    StatusLevel::Error,
                    format!(
                        "Engine {} stalled again ({}). It has been restarted {} times, so it is offline; load it to try once more.",
                        slot_letter(slot),
                        reason,
                        RESTART_LIMIT
                    ),
                );
            } else {
                self.note_restart(slot);
                self.engines[slot] = None;
                log::warn!(
                    "engine {} stalled ({}); restarting it from the same file",
                    binary.display(),
                    reason
                );
                self.restart_engine(slot, binary, was_analysing);
                self.set_status(
                    StatusLevel::Warning,
                    format!(
                        "Engine {} stalled ({}). Restarting it.",
                        slot_letter(slot),
                        reason
                    ),
                );
            }
        }
    }

    /// Replace an engine whose process ended on its own after a successful
    /// handshake. Crashes are treated like stalls: restarted, and left offline
    /// once the slot has used up its restarts ([`RESTART_LIMIT`] within
    /// [`RESTART_WINDOW`]).
    fn report_exited_engines(&mut self) {
        for slot in 0..2 {
            let Some(host) = self.engines[slot].as_mut() else {
                continue;
            };
            if !self.engine_auto_restart[slot]
                || !host.has_exited()
                || !host.has_handshaken()
                || host.is_stalled()
            {
                continue;
            }

            let binary = host.binary_path();
            let was_analysing = self.analysis_on[slot];
            let process = host.shutdown();
            self.close_options_dialog(slot);
            self.quit_processes(vec![process]);
            self.engines[slot] = None;
            self.analysis_on[slot] = false;
            self.engine_status[slot] = None;
            self.engine_started[slot] = None;

            if !self.may_restart(slot) {
                self.engine_auto_restart[slot] = false;
                self.restart_times[slot].clear();
                log::error!(
                    "engine {} exited after {} restarts in {} minutes; leaving it offline",
                    binary.display(),
                    RESTART_LIMIT,
                    RESTART_WINDOW.as_secs() / 60
                );
                self.set_status(
                    StatusLevel::Error,
                    format!(
                        "Engine {} stopped again. It has been restarted {} times, so it is offline; load it to try once more.",
                        slot_letter(slot),
                        RESTART_LIMIT
                    ),
                );
            } else {
                self.note_restart(slot);
                log::warn!(
                    "engine {} exited unexpectedly; restarting it from the same file",
                    binary.display()
                );
                self.restart_engine(slot, binary, was_analysing);
                self.set_status(
                    StatusLevel::Warning,
                    format!(
                        "Engine {} stopped unexpectedly. Restarting it.",
                        slot_letter(slot)
                    ),
                );
            }
        }
    }

    /// Open the options window for the engine in `slot`, with the options it declared.
    fn open_engine_options(&mut self, slot: usize) {
        let Some(host) = self.engines[slot].as_ref() else {
            self.set_status(
                StatusLevel::Warning,
                format!("Engine {} is not loaded.", slot_letter(slot)),
            );
            return;
        };
        let view = host.view();
        let options = host.options().to_vec();
        let engine_name = view.name.unwrap_or_else(|| view.binary_name.clone());
        self.options_dialog = Some(OptionsDialog::new(
            slot,
            slot_letter(slot),
            engine_name,
            view.binary_name,
            options,
            &self.engine_settings[slot],
        ));
    }

    /// Set one option on the engine in `slot`, and keep the value for a restart.
    fn set_engine_option(&mut self, slot: usize, name: &str, value: &str) {
        self.engine_settings[slot].insert(name.to_string(), value.to_string());
        let file = match self.engines[slot].as_mut() {
            Some(host) => {
                host.set_option(name, value);
                Some(host.binary_path().display().to_string())
            }
            None => None,
        };
        if let Some(file) = file {
            self.settings
                .engine_options
                .entry(file)
                .or_default()
                .insert(name.to_string(), value.to_string());
            self.save_settings();
        }
        match self.option_warning(slot, name, value) {
            Some(warning) => self.set_status(StatusLevel::Warning, warning),
            None => self.set_status(
                StatusLevel::Info,
                format!("Engine {}: {} set to {}.", slot_letter(slot), name, value),
            ),
        }
    }

    /// A warning for an option value that wastes the machine: more threads
    /// than cores over both engines, or a `MultiPV` above 1, which
    /// makes the engine search lines the card never shows. The value
    /// is still set; the warning only informs the user.
    fn option_warning(&self, slot: usize, name: &str, value: &str) -> Option<String> {
        match name {
            "MultiPV" => {
                let lines: u32 = value.trim().parse().ok()?;
                (lines > 1).then(|| {
                    format!(
                        "Engine {}: MultiPV {} makes it search {} lines; the card shows only line 1, so the search is slower.",
                        slot_letter(slot),
                        lines,
                        lines
                    )
                })
            }
            "Threads" => {
                let own: usize = value.trim().parse().ok()?;
                let other = 1 - slot;
                let other_threads: usize = self.engine_settings[other]
                    .get("Threads")
                    .and_then(|text| text.trim().parse().ok())
                    .unwrap_or(1);
                let total = own + other_threads;
                let cores = std::thread::available_parallelism().map_or(usize::MAX, usize::from);
                (total > cores).then(|| {
                    format!(
                        "Engine {}: Threads {} plus engine {} at {} is {} threads on {} cores; the engines will compete for cores.",
                        slot_letter(slot),
                        own,
                        slot_letter(other),
                        other_threads,
                        total,
                        cores
                    )
                })
            }
            _ => None,
        }
    }

    /// Press a button option on the engine in `slot`.
    fn press_engine_button(&mut self, slot: usize, name: &str) {
        if let Some(host) = self.engines[slot].as_mut() {
            host.press_button(name);
        }
        self.set_status(
            StatusLevel::Info,
            format!("Engine {}: {} pressed.", slot_letter(slot), name),
        );
    }

    /// Close the options window if it is for `slot`, e.g. when that engine is replaced.
    fn close_options_dialog(&mut self, slot: usize) {
        if self
            .options_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.slot() == slot)
        {
            self.options_dialog = None;
        }
    }

    /// Whether `slot` may be restarted now: fewer than [`RESTART_LIMIT`] restarts
    /// within [`RESTART_WINDOW`]. Older restarts are forgotten.
    fn may_restart(&mut self, slot: usize) -> bool {
        self.restart_times[slot].retain(|at| at.elapsed() < RESTART_WINDOW);
        self.restart_times[slot].len() < RESTART_LIMIT
    }

    /// Count a restart of `slot` for [`Self::may_restart`].
    fn note_restart(&mut self, slot: usize) {
        self.restart_times[slot].push(Instant::now());
    }

    /// Start the engine in `slot` again from `binary` (runs on the Tokio runtime).
    /// `was_analysing` tells the attach step whether analysis was on.
    fn restart_engine(&mut self, slot: usize, binary: PathBuf, was_analysing: bool) {
        self.restarting[slot] = Some(was_analysing);
        self.spawn_engine_file(slot, binary);
    }

    /// Start the engine file `path` in `slot` on the Tokio runtime. The result
    /// arrives as `EngineSpawned`.
    fn spawn_engine_file(&mut self, slot: usize, path: PathBuf) {
        log::info!(
            "starting engine {} from {}",
            slot_letter(slot),
            path.display()
        );
        self.engine_loading[slot] = true;
        let tx = self.events_tx.clone();
        let wake = self.engine_wake.clone();
        self.spawn(async move {
            let outcome = Some(EngineProcess::spawn_waking(&path, wake).await);
            tx.send(AppEvent::EngineSpawned { slot, outcome });
        });
    }

    /// Set an option back to the engine's default: the saved value is forgotten.
    fn reset_engine_option(&mut self, slot: usize, name: &str, default: &str) {
        self.engine_settings[slot].remove(name);
        let file = match self.engines[slot].as_mut() {
            Some(host) => {
                host.set_option(name, default);
                Some(host.binary_path().display().to_string())
            }
            None => None,
        };
        if let Some(file) = file {
            if let Some(saved) = self.settings.engine_options.get_mut(&file) {
                saved.remove(name);
            }
            self.save_settings();
        }
        self.set_status(
            StatusLevel::Info,
            format!(
                "Engine {}: {} back to its default ({}).",
                slot_letter(slot),
                name,
                default
            ),
        );
    }

    /// Set every option on the engine in `slot` back to the engine's default,
    /// and forget the values saved for that engine file.
    fn reset_all_engine_options(&mut self, slot: usize) {
        let Some(host) = self.engines[slot].as_mut() else {
            self.set_status(
                StatusLevel::Warning,
                format!("Engine {} is not loaded.", slot_letter(slot)),
            );
            return;
        };
        let defaults: Vec<(String, String)> = host
            .options()
            .iter()
            .filter(|option| !matches!(option.kind, crate::engine::protocol::UciOptionKind::Button))
            .map(|option| (option.name.clone(), default_value(option)))
            .collect();
        for (name, value) in &defaults {
            host.set_option(name, value);
        }
        let file = host.binary_path().display().to_string();
        self.engine_settings[slot].clear();
        if let Some(saved) = self.settings.engine_options.get_mut(&file) {
            saved.clear();
        }
        self.save_settings();
        self.set_status(
            StatusLevel::Info,
            format!(
                "Engine {}: all options back to their defaults.",
                slot_letter(slot)
            ),
        );
    }

    /// Switch session-file logging on or off and keep the choice.
    fn toggle_session_log(&mut self) {
        self.settings.session_log = !self.settings.session_log;
        crate::logging::set_file_enabled(self.settings.session_log);
        self.save_settings();
        let text = if self.settings.session_log {
            "Session log: on, in ~/Library/Logs/SiliconBoard.".to_string()
        } else {
            "Session log: off.".to_string()
        };
        log::info!("{}", text);
        self.set_status(StatusLevel::Info, text);
    }

    /// Show the file picker for an engine and spawn the chosen binary.
    fn load_engine(&mut self, slot: usize) {
        if self.engine_loading[slot] {
            return;
        }
        self.engine_loading[slot] = true;
        // A manual load replaces whatever restart was in flight for this slot,
        // so the new engine is not taken for the restart.
        self.restarting[slot] = None;
        self.set_status(
            StatusLevel::Info,
            format!("Choose an engine for slot {}…", slot_letter(slot)),
        );
        let tx = self.events_tx.clone();
        let wake = self.engine_wake.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose a UCI engine")
                .pick_file()
                .await;
            let outcome = match picked {
                None => None,
                Some(handle) => {
                    let path = handle.path().to_path_buf();
                    Some(EngineProcess::spawn_waking(&path, wake).await)
                }
            };
            tx.send(AppEvent::EngineSpawned { slot, outcome });
        });
    }

    /// Engine card pulldown: start a known engine file in `slot`, replacing
    /// whatever is loaded there. The file list is the one the tournament
    /// window offers (loaded A/B engines and the files used last time).
    fn choose_engine(&mut self, slot: usize, choice: usize) {
        let Some(spec) = self.engine_choices().get(choice).cloned() else {
            return;
        };
        if self.engine_loading[slot] {
            return;
        }
        if self.engine_live(slot)
            && self.engines[slot]
                .as_ref()
                .is_some_and(|host| host.binary_path() == spec.path)
        {
            return; // already running in this slot
        }
        self.restarting[slot] = None;
        self.set_status(
            StatusLevel::Info,
            format!(
                "Loading engine {}: {}…",
                slot_letter(slot),
                spec.path.display()
            ),
        );
        self.spawn_engine_file(slot, spec.path);
    }

    /// Switch the board image. Decoding runs off the UI thread; the texture is
    /// uploaded when [`AppEvent::BoardThemeLoaded`] arrives.
    fn set_board_theme(&mut self, index: usize) {
        let Some(theme) = self.board_themes.get(index).cloned() else {
            return;
        };
        if self.settings.board_theme.as_deref() == Some(theme.file.as_str()) {
            return;
        }
        self.set_status(
            StatusLevel::Info,
            format!("Loading board theme: {}…", theme.label),
        );
        let tx = self.events_tx.clone();
        let file = theme.file.clone();
        self.spawn(async move {
            let outcome = match crate::assets::board_theme_path(&file) {
                Some(path) => {
                    let error_path = path.clone();
                    let joined =
                        tokio::task::spawn_blocking(move || crate::assets::decode_board(&path))
                            .await;
                    match joined {
                        Ok(result) => result,
                        Err(join) => Err(AssetError::OpenRead {
                            path: error_path,
                            source: std::io::Error::other(join),
                        }),
                    }
                }
                None => Err(AssetError::OpenRead {
                    path: PathBuf::from(&file),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "not a board image",
                    ),
                }),
            };
            tx.send(AppEvent::BoardThemeLoaded { theme, outcome });
        });
    }

    /// Switch the piece set. Rasterising the twelve SVGs runs off the UI
    /// thread; the textures are uploaded when [`AppEvent::PieceSetLoaded`]
    /// arrives.
    fn set_piece_set(&mut self, index: usize) {
        let Some(set) = self.piece_sets.get(index).cloned() else {
            return;
        };
        let current = self
            .settings
            .piece_set
            .as_deref()
            .unwrap_or(crate::assets::DEFAULT_PIECE_SET);
        if current == set.name {
            return;
        }
        self.set_status(
            StatusLevel::Info,
            format!("Loading piece set: {}…", set.label),
        );
        let tx = self.events_tx.clone();
        let name = set.name.clone();
        self.spawn(async move {
            let outcome = match crate::assets::piece_set_path(&name) {
                Some(path) => {
                    let error_path = path.clone();
                    match tokio::task::spawn_blocking(move || {
                        crate::assets::rasterise_piece_set(&path)
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(join) => Err(AssetError::OpenRead {
                            path: error_path,
                            source: std::io::Error::other(join),
                        }),
                    }
                }
                None => Err(AssetError::OpenRead {
                    path: PathBuf::from(&name),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "not a complete piece set",
                    ),
                }),
            };
            tx.send(AppEvent::PieceSetLoaded { set, outcome });
        });
    }

    /// Flip one of the two figurine settings and save it. `eval` selects the
    /// engine PV; otherwise the move list.
    fn toggle_figurines(&mut self, eval: bool) {
        let on = {
            let setting = if eval {
                &mut self.settings.figurines_eval
            } else {
                &mut self.settings.figurines_move_list
            };
            *setting = !*setting;
            *setting
        };
        self.save_settings();
        let place = if eval {
            "engine evaluation"
        } else {
            "move list"
        };
        let state = if on { "on" } else { "off" };
        self.set_status(StatusLevel::Info, format!("Figurines in {place}: {state}."));
    }

    /// Flip View → Analysis arrows and save it.
    fn toggle_analysis_arrows(&mut self) {
        self.settings.analysis_arrows = !self.settings.analysis_arrows;
        self.save_settings();
        let state = if self.settings.analysis_arrows {
            "on"
        } else {
            "off"
        };
        self.set_status(StatusLevel::Info, format!("Analysis arrows: {state}."));
    }

    /// Show the tournament configuration window, starting from the saved
    /// configuration and the engines the app already knows.
    fn open_tournament_dialog(&mut self) {
        if self
            .tournament
            .as_ref()
            .is_some_and(TournamentSession::active)
        {
            self.set_status(
                StatusLevel::Warning,
                "A tournament is already running. Stop it before starting another.".to_string(),
            );
            return;
        }
        let choices = self.engine_choices();
        let mut config = self.settings.tournament.clone().unwrap_or_default();
        if !config.white.is_set() {
            if let Some(first) = choices.first() {
                config.white = first.clone();
            }
        }
        if !config.black.is_set() {
            if let Some(second) = choices.get(1).or_else(|| choices.first()) {
                config.black = second.clone();
            }
        }
        if config.pgn_path.as_os_str().is_empty() {
            config.pgn_path = tournament::default_pgn_path();
        }
        // Relative paths (typed earlier, or saved by an older version whose
        // default followed the working directory) would otherwise resolve
        // against `/` in a Finder launch. Show the absolute path instead.
        config.pgn_path = tournament::resolve_pgn_path(&config.pgn_path);
        self.tournament_dialog = Some(TournamentDialog::new(config, choices));
        self.tournament_finished_popup = false;
    }

    /// Remember an engine file in the pulldown history, newest first, without
    /// duplicates.
    fn remember_engine(&mut self, path: &Path) {
        self.settings.engine_history.retain(|known| known != path);
        self.settings.engine_history.insert(0, path.to_path_buf());
        self.settings
            .engine_history
            .truncate(settings::ENGINE_HISTORY_LIMIT);
    }

    /// Engines offered in the tournament dialog: the loaded A/B slots, then the
    /// files used last time, then the remembered history. Paths are unique.
    fn engine_choices(&self) -> Vec<EngineSpec> {
        let mut choices: Vec<EngineSpec> = Vec::new();
        for slot in 0..2 {
            let spec = match &self.engines[slot] {
                Some(host) => EngineSpec {
                    path: host.binary_path(),
                    name: self.engine_name(slot),
                },
                None => match &self.settings.engines[slot] {
                    Some(path) => EngineSpec::from_path(path.clone()),
                    None => continue,
                },
            };
            if spec.is_set() && !choices.iter().any(|choice| choice.path == spec.path) {
                choices.push(spec);
            }
        }
        // Everything loaded before this session, newest first.
        for path in &self.settings.engine_history {
            let spec = EngineSpec::from_path(path.clone());
            if spec.is_set() && !choices.iter().any(|choice| choice.path == spec.path) {
                choices.push(spec);
            }
        }
        choices
    }

    /// Perform what the configuration window asked for.
    fn handle_tournament_action(&mut self, action: TournamentAction) {
        match action {
            TournamentAction::None => {}
            TournamentAction::Cancel => self.tournament_dialog = None,
            TournamentAction::Start(config) => {
                self.tournament_dialog = None;
                self.start_tournament(*config);
            }
            TournamentAction::BrowseEngine(side) => self.pick_tournament_engine(side),
            TournamentAction::BrowseBook => self.pick_tournament_book(),
            TournamentAction::BrowseMoveBook => self.pick_tournament_move_book(),
            TournamentAction::BrowsePgn => self.pick_tournament_pgn(),
        }
    }

    /// Start the match. The games are played on the main board, one after the
    /// other, using the normal game loop.
    fn start_tournament(&mut self, mut config: TournamentConfig) {
        if let Err(message) = config.validate() {
            self.set_status(StatusLevel::Error, message);
            return;
        }
        self.clear_pgn_browser();
        config.pgn_path = tournament::resolve_pgn_path(&config.pgn_path);
        let pgn_file = match open_tournament_pgn(&config.pgn_path) {
            Ok(file) => file,
            Err(message) => {
                self.set_status(StatusLevel::Error, message);
                return;
            }
        };
        self.settings.tournament = Some(config.clone());
        self.save_settings();

        let phase = if config.opening_book.is_some() || config.move_book.is_some() {
            TournamentPhase::LoadingBook
        } else {
            TournamentPhase::LoadingEngines
        };
        let mut status = TournamentStatus {
            total_games: config.games,
            pgn_path: config.pgn_path.clone(),
            engines: [config.white.name.clone(), config.black.name.clone()],
            running: true,
            ..TournamentStatus::default()
        };
        status.message = Some(if phase == TournamentPhase::LoadingBook {
            "Loading opening book…".to_string()
        } else {
            "Loading engines…".to_string()
        });
        self.tournament_generation = self.tournament_generation.wrapping_add(1);
        let generation = self.tournament_generation;
        self.tournament = Some(TournamentSession {
            adjudicator: Adjudicator::new(&config),
            config: config.clone(),
            phase,
            book: None,
            schedule: Vec::new(),
            game_number: 1,
            white_slot: 0,
            pgn_file,
            status,
            next_game_due: None,
            generation,
        });
        self.tournament_window_open = true;
        self.tournament_finished_popup = false;
        // The board stays quiet while the engines and the book are loaded.
        self.mode = GameMode::Analysis;
        self.game_started = false;
        for slot in 0..2 {
            self.set_engine_analysis(slot, false);
        }
        self.sync_position(true);

        let primary = match &config.opening_book {
            Some(book) => match book_kind(&book.path) {
                Ok(kind) => Some((book.path.clone(), kind)),
                Err(message) => {
                    self.fail_tournament(message);
                    return;
                }
            },
            None => None,
        };
        let move_book = config.move_book.clone();
        if primary.is_some() || move_book.is_some() {
            let tx = self.events_tx.clone();
            self.spawn(async move {
                let outcome = tokio::task::spawn_blocking(move || {
                    let mut start = None;
                    let mut moves = None;
                    if let Some((path, kind)) = primary {
                        match kind {
                            BookKind::Epd => {
                                let book = EpdBook::load(&path).map_err(|err| err.to_string())?;
                                if book.is_empty() {
                                    return Err(format!(
                                        "The EPD book {} has no positions.",
                                        path.display()
                                    ));
                                }
                                start = Some(book);
                            }
                            BookKind::Polyglot => {
                                let book =
                                    PolyglotBook::open(&path).map_err(|err| err.to_string())?;
                                if book.is_empty() {
                                    return Err(format!(
                                        "The PolyGlot book {} has no moves.",
                                        path.display()
                                    ));
                                }
                                moves = Some(book);
                            }
                        }
                    }
                    if let Some(path) = move_book {
                        let book = PolyglotBook::open(&path).map_err(|err| err.to_string())?;
                        if book.is_empty() {
                            return Err(format!(
                                "The PolyGlot move book {} has no moves.",
                                path.display()
                            ));
                        }
                        moves = Some(book);
                    }
                    if start.is_none() && moves.is_none() {
                        return Err("The opening book has no content.".to_string());
                    }
                    Ok(TournamentBook { start, moves })
                })
                .await
                .map_err(|join| format!("opening-book task failed: {}", join))
                .and_then(|result| result);
                tx.send(AppEvent::TournamentBookLoaded {
                    generation,
                    outcome,
                });
            });
        } else {
            if let Some(session) = &mut self.tournament {
                session.schedule = schedule_games(&session.config, 0);
            }
            self.load_tournament_engines();
        }
        self.set_status(
            StatusLevel::Info,
            format!("Tournament started: {} games.", config.games),
        );
    }

    /// Make sure the two configured engines are in slots A and B. A slot that
    /// already holds the right file is kept.
    fn load_tournament_engines(&mut self) {
        let Some(session) = &self.tournament else {
            return;
        };
        let specs = [session.config.white.clone(), session.config.black.clone()];
        for (slot, spec) in specs.iter().enumerate() {
            let already = self.engines[slot].as_ref().is_some_and(|host| {
                host.binary_path() == spec.path && host.status() != AnalysisStatus::Offline
            });
            if !already {
                self.spawn_engine_file(slot, spec.path.clone());
            }
        }
        if let Some(session) = &mut self.tournament {
            session.phase = TournamentPhase::LoadingEngines;
            session.status.message = Some("Loading engines…".to_string());
        }
    }

    /// Why a tournament engine is missing or offline: the slot, the engine and
    /// its own diagnosis (exit status and the message that names the problem),
    /// or the spawn error. The generic sentence alone hid cases such as
    /// Pedantic exiting on a missing locale.
    fn tournament_engine_failure(&self) -> String {
        let mut problems = Vec::new();
        for slot in 0..2 {
            let problem = match self.engines[slot].as_ref() {
                Some(host) if host.status() == AnalysisStatus::Offline => {
                    let view = host.view();
                    let name = view.name.filter(|name| !name.is_empty());
                    Some(format!(
                        "engine {} ({}) did not start: {}",
                        slot_letter(slot),
                        name.as_deref().unwrap_or(&view.binary_name),
                        host.failure_detail()
                    ))
                }
                Some(_) => None,
                None => Some(self.engine_spawn_error[slot].as_ref().map_or_else(
                    || format!("engine {} is not loaded", slot_letter(slot)),
                    |error| {
                        format!(
                            "engine {} could not be started: {}",
                            slot_letter(slot),
                            error
                        )
                    },
                )),
            };
            if let Some(problem) = problem {
                problems.push(problem);
            }
        }
        if problems.is_empty() {
            "A tournament engine could not be started.".to_string()
        } else {
            format!(
                "A tournament engine could not be started: {}.",
                problems.join("; ")
            )
        }
    }

    /// The EPD index finished on a blocking thread.
    fn handle_tournament_book_loaded(
        &mut self,
        generation: u64,
        outcome: Result<TournamentBook, String>,
    ) {
        if self.tournament.as_ref().is_none_or(|session| {
            session.generation != generation || session.phase != TournamentPhase::LoadingBook
        }) {
            return;
        }
        match outcome {
            Ok(book) if book.is_empty() => {
                self.fail_tournament("The opening book has no usable entries.".to_string());
            }
            Ok(book) => {
                let count = book.opening_count();
                if let Some(session) = &mut self.tournament {
                    session.schedule = schedule_games(&session.config, count);
                    session.book = Some(book);
                    session.phase = TournamentPhase::LoadingEngines;
                    session.status.message = Some("Loading engines…".to_string());
                }
                self.load_tournament_engines();
            }
            Err(message) => self.fail_tournament(message),
        }
    }

    /// Set up the next scheduled game on the main board.
    fn start_tournament_game(&mut self) {
        let prepared = {
            let Some(session) = self.tournament.as_ref() else {
                return;
            };
            if session.phase == TournamentPhase::Finished {
                return;
            }
            let index = session.game_number.saturating_sub(1) as usize;
            let Some(scheduled) = session.schedule.get(index).cloned() else {
                return self.fail_tournament("The tournament schedule is empty.".to_string());
            };
            let start_fen = match (
                session.book.as_ref().and_then(TournamentBook::epd),
                scheduled.opening,
            ) {
                (Some(book), Some(opening)) => match book.fen_at(opening) {
                    Ok(Some(fen)) => Some(fen),
                    Ok(None) => {
                        return self
                            .fail_tournament(format!("EPD position {} has no FEN.", opening + 1))
                    }
                    Err(err) => return self.fail_tournament(err.to_string()),
                },
                _ => None,
            };
            let target = match &start_fen {
                Some(fen) => match GameState::from_fen(fen) {
                    Ok(game) => game,
                    Err(err) => {
                        return self.fail_tournament(format!(
                            "The opening position is not a legal FEN: {}",
                            err
                        ))
                    }
                },
                None => GameState::new(),
            };
            // If a PolyGlot move book can reach the EPD position, replay that
            // line so the PGN starts at move 1 and contains the book moves.
            let game = if start_fen.is_some() {
                let move_book = session
                    .book
                    .as_ref()
                    .and_then(TournamentBook::polyglot)
                    .or_else(|| self.book.as_ref().map(|(_, book)| book));
                let path = move_book.and_then(|book| find_book_path(book, &target.position()));
                match path {
                    Some(path) => {
                        let mut game = GameState::new();
                        if path.iter().all(|uci| game.apply_uci(uci).is_ok()) {
                            game
                        } else {
                            target.clone()
                        }
                    }
                    None => target.clone(),
                }
            } else {
                target
            };
            if game.is_game_over() {
                return self
                    .fail_tournament("An opening position is already finished.".to_string());
            }
            let white_slot = if scheduled.white_is_first { 0 } else { 1 };
            let white_name = self.engine_name(white_slot);
            let black_name = self.engine_name(1 - white_slot);
            (
                game,
                white_slot,
                white_name,
                black_name,
                session.game_number,
                session.config.games,
                session.config.time_control,
                session.config.engine_options(),
            )
        };

        let (game, white_slot, white_name, black_name, number, total, control, options) = prepared;
        // A fresh per-game seed: games that start from the same position then
        // get independent book lines instead of replaying one fixed sequence,
        // which is what makes a match statistically meaningful (audit §2.7).
        let generation = self
            .tournament
            .as_ref()
            .map_or(0, |session| session.generation);
        self.book_picker = BookPicker::with_seed(book_seed(generation, number));
        self.game = game;
        self.ending = None;
        self.pgn_termination = None;
        self.pgn_players = None;
        self.game_started = true;
        self.mode = GameMode::EngineVsEngine;
        self.clock = Some(Clock::new(control, self.game.turn()));
        self.view.clear_selection();
        // Apply the tournament's Hash and Nalimov values before the first
        // search. They are kept in the per-slot settings, so a watchdog
        // restart sends them again.
        for slot in 0..2 {
            for (name, value) in &options {
                self.engine_settings[slot].insert(name.clone(), value.clone());
                if let Some(host) = self.engines[slot].as_mut() {
                    host.set_option(name, value);
                }
            }
        }
        let engine_names = [self.engine_name(0), self.engine_name(1)];
        if let Some(session) = &mut self.tournament {
            session.white_slot = white_slot;
            session.phase = TournamentPhase::Playing;
            session.adjudicator = Adjudicator::new(&session.config);
            session.next_game_due = None;
            session.status.message = None;
            session.status.engines = engine_names;
            session.status.current = vec![CurrentGame {
                number,
                white: white_name.clone(),
                black: black_name.clone(),
                ply: 0,
                fullmove: self.game.fullmove_number(),
                score: None,
            }];
        }
        for slot in 0..2 {
            if let Some(host) = self.engines[slot].as_mut() {
                host.new_game();
            }
        }
        for slot in 0..2 {
            self.set_engine_analysis(slot, true);
        }
        self.sync_position(true);
        self.set_status(
            StatusLevel::Info,
            format!(
                "Tournament game {} of {}: {} vs {}.",
                number, total, white_name, black_name
            ),
        );
    }

    /// Advance the tournament once per frame.
    fn tournament_tick(&mut self, ctx: &egui::Context) {
        let Some(phase) = self.tournament.as_ref().map(|session| session.phase) else {
            return;
        };
        match phase {
            TournamentPhase::LoadingBook => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            TournamentPhase::LoadingEngines => {
                // Engine results wake the window, but a tick keeps a slow
                // handshake from stalling the match.
                ctx.request_repaint_after(Duration::from_millis(200));
                let ready = (0..2).all(|slot| {
                    self.engines[slot].as_ref().is_some_and(|host| {
                        matches!(
                            host.status(),
                            AnalysisStatus::Ready | AnalysisStatus::Thinking
                        )
                    })
                });
                let loading = self.engine_loading.iter().any(|loading| *loading);
                let failed = (0..2).any(|slot| {
                    self.engines[slot]
                        .as_ref()
                        .is_none_or(|host| host.status() == AnalysisStatus::Offline)
                });
                if ready {
                    self.start_tournament_game();
                } else if failed && !loading {
                    let message = self.tournament_engine_failure();
                    self.fail_tournament(message);
                }
            }
            TournamentPhase::BetweenGames => {
                let due = self
                    .tournament
                    .as_ref()
                    .and_then(|session| session.next_game_due);
                if let Some(due) = due {
                    let remaining = due.saturating_duration_since(Instant::now());
                    ctx.request_repaint_after(remaining.max(Duration::from_millis(16)));
                    if remaining.is_zero() {
                        self.start_tournament_game();
                    }
                }
            }
            TournamentPhase::Playing => {
                // Browsing an earlier move pauses the tournament; it resumes
                // when the cursor returns to the live position.
                if self.game.cur_ply() != self.game.total_plies() {
                    return;
                }
                if self.game.is_threefold_repetition() && self.ending.is_none() {
                    self.ending = Some(Ending::ThreefoldRepetition);
                }
                if self.current_ending().is_some() || self.game.is_game_over() {
                    self.finish_tournament_game();
                    // The next game starts after a pause; make sure a frame
                    // runs even though both engines are now quiet.
                    ctx.request_repaint_after(Duration::from_millis(16));
                } else if (0..2).any(|slot| {
                    !self.engine_loading[slot]
                        && self.engines[slot]
                            .as_ref()
                            .is_none_or(|host| host.status() == AnalysisStatus::Offline)
                }) {
                    self.fail_tournament(
                        "A tournament engine stopped during the game.".to_string(),
                    );
                }
            }
            TournamentPhase::Finished => {}
        }
    }

    /// A book move has no engine score; it resets the adjudication counters,
    /// as for any forced or book move.
    fn tournament_observe_book_move(&mut self) {
        let Some(session) = self.tournament.as_mut() else {
            return;
        };
        if !session.is_playing() {
            return;
        }
        let mover = self.game.turn().opposite();
        let fullmove = self.game.fullmove_number();
        let last_move = self.game.history().last().map(|ply| &ply.move_);
        let _ = session
            .adjudicator
            .observe(mover, None, last_move, fullmove);
        let ply = self.game.total_plies();
        if let Some(game) = session.status.current.first_mut() {
            game.ply = ply;
            game.fullmove = fullmove;
            game.score = None;
        }
    }

    /// Observe the score of the engine that just moved and adjudicate.
    fn tournament_observe_move(&mut self, slot: usize) {
        let Some(session) = self.tournament.as_ref() else {
            return;
        };
        if !session.is_playing() {
            return;
        }
        let white_slot = session.white_slot;
        let mover = if slot == white_slot {
            PieceColor::White
        } else {
            PieceColor::Black
        };
        let score = self.engines[slot]
            .as_ref()
            .and_then(|host| host.view().stats.score);
        let fullmove = self.game.fullmove_number();
        let last_move = self.game.history().last().map(|ply| &ply.move_);
        let adjudication = self
            .tournament
            .as_mut()
            .expect("the tournament exists")
            .adjudicator
            .observe(mover, score, last_move, fullmove);
        let ply = self.game.total_plies();
        // The window shows White's point of view; the adjudication above used
        // the engine's own score.
        let score_text = score.map(|score| white_score_label(score, mover));
        if let Some(session) = &mut self.tournament {
            if let Some(game) = session.status.current.first_mut() {
                game.ply = ply;
                game.fullmove = fullmove;
                game.score = score_text;
            }
        }
        match adjudication {
            Adjudication::None => {}
            Adjudication::Draw => self.ending = Some(Ending::AdjudicatedDraw),
            Adjudication::Resign(loser) => {
                self.ending = Some(Ending::AdjudicatedWin {
                    winner: loser.opposite(),
                });
            }
        }
    }

    /// Write the finished game, update the tally, and start the next one.
    fn finish_tournament_game(&mut self) {
        let (number, white_slot, config) = {
            let Some(session) = self.tournament.as_ref() else {
                return;
            };
            (
                session.game_number,
                session.white_slot,
                session.config.clone(),
            )
        };
        let ending = if self.game.is_game_over() {
            None
        } else {
            self.current_ending()
        };
        let token = ending.map_or_else(
            || pgn::result_token(&self.game, None).to_string(),
            |ending| ending.result_token().to_string(),
        );
        let reason = ending.map_or_else(|| self.game.outcome(), |ending| ending.description());
        if self.game.is_game_over() {
            // A mate or stalemate wins over any adjudication on the same move.
            self.ending = None;
        }
        let white_name = self.engine_name(white_slot);
        let black_name = self.engine_name(1 - white_slot);
        let headers = game_headers(
            &config,
            number,
            &white_name,
            &black_name,
            &reason,
            self.game.total_plies(),
        );
        let text = pgn::to_pgn_with_headers(&self.game, &token, &headers);
        // The write runs off the UI thread. A clone of the handle shares the
        // file position, so games still follow each other in the file.
        let file = self
            .tournament
            .as_ref()
            .expect("the tournament exists")
            .pgn_file
            .try_clone();
        match file {
            Ok(mut file) => {
                let tx = self.events_tx.clone();
                self.spawn(async move {
                    let written = tokio::task::spawn_blocking(move || {
                        file.write_all(text.as_bytes())
                            .and_then(|()| file.write_all(b"\n"))
                            .and_then(|()| file.flush())
                    })
                    .await;
                    let failure = match written {
                        Ok(Ok(())) => None,
                        Ok(Err(err)) => Some(err.to_string()),
                        Err(err) => Some(err.to_string()),
                    };
                    if let Some(message) = failure {
                        tx.send(AppEvent::TournamentWriteFailed { message });
                    }
                });
            }
            Err(err) => {
                self.fail_tournament(format!("Could not write the PGN file: {}", err));
                return;
            }
        }

        let total = config.games;
        let summary = {
            let session = self.tournament.as_mut().expect("the tournament exists");
            session.status.completed_games += 1;
            match token.as_str() {
                "1-0" => *session.status.wins.entry(white_name.clone()).or_default() += 1,
                "0-1" => *session.status.wins.entry(black_name.clone()).or_default() += 1,
                "1/2-1/2" => session.status.draws += 1,
                _ => session.status.failed_games += 1,
            }
            let result_line = format!("Game {}: {} ({})", number, token, reason);
            session.status.results.push(result_line);
            session.status.current.clear();
            session.game_number += 1;
            if session.game_number > total {
                session.phase = TournamentPhase::Finished;
                session.status.running = false;
                session.status.message = None;
                session.book = None;
            } else {
                session.phase = TournamentPhase::BetweenGames;
                session.status.message = Some("Next game…".to_string());
                session.next_game_due = Some(Instant::now() + Duration::from_secs(1));
            }
            session.status.summary()
        };

        self.game_started = false;
        if let Some(clock) = self.clock.as_mut() {
            clock.pause();
        }
        for slot in 0..2 {
            self.set_engine_analysis(slot, false);
        }
        self.sync_position(true);
        if self
            .tournament
            .as_ref()
            .is_some_and(|session| session.phase == TournamentPhase::Finished)
        {
            // The summary pop-up appears even when the user closed the live
            // tournament window earlier: closing it only hides the progress
            // view, never the result of the match.
            self.tournament_window_open = false;
            self.tournament_finished_popup = true;
            self.set_status(StatusLevel::Info, summary);
        } else {
            self.set_status(
                StatusLevel::Info,
                format!(
                    "Tournament game {} finished: {} ({}). Next game…",
                    number, token, reason
                ),
            );
        }
    }

    /// Stop the match because of an error.
    fn fail_tournament(&mut self, message: String) {
        if let Some(session) = &mut self.tournament {
            session.phase = TournamentPhase::Finished;
            session.status.running = false;
            session.status.failed = true;
            session.status.message = Some(message.clone());
            session.status.current.clear();
            session.book = None;
        }
        self.game_started = false;
        if let Some(clock) = self.clock.as_mut() {
            clock.pause();
        }
        for slot in 0..2 {
            self.set_engine_analysis(slot, false);
        }
        self.sync_position(true);
        self.tournament_window_open = false;
        self.tournament_finished_popup = true;
        self.set_status(
            StatusLevel::Error,
            format!("Tournament failed: {}", message),
        );
    }

    /// Stop the running tournament now. Completed games stay in the PGN.
    fn stop_tournament(&mut self) {
        let Some(session) = &mut self.tournament else {
            self.set_status(
                StatusLevel::Warning,
                "No tournament is running.".to_string(),
            );
            return;
        };
        if session.phase == TournamentPhase::Finished {
            self.set_status(
                StatusLevel::Warning,
                "No tournament is running.".to_string(),
            );
            return;
        }
        session.phase = TournamentPhase::Finished;
        session.status.running = false;
        session.status.cancelled = true;
        session.status.message = None;
        session.status.current.clear();
        session.book = None;
        self.game_started = false;
        if let Some(clock) = self.clock.as_mut() {
            clock.pause();
        }
        for slot in 0..2 {
            self.set_engine_analysis(slot, false);
        }
        self.sync_position(true);
        self.tournament_window_open = false;
        self.tournament_finished_popup = true;
        self.set_status(StatusLevel::Warning, "Tournament stopped.".to_string());
    }

    /// File picker for one tournament engine.
    fn pick_tournament_engine(&mut self, side: TournamentSide) {
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose a UCI engine")
                .pick_file()
                .await;
            let outcome = picked.map(|handle| EngineSpec::from_path(handle.path().to_path_buf()));
            tx.send(AppEvent::TournamentEnginePicked { side, outcome });
        });
    }

    /// File picker for the EPD opening book.
    fn pick_tournament_book(&mut self) {
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose an EPD or PolyGlot opening book")
                .add_filter("Opening book", &["epd", "bin"])
                .pick_file()
                .await;
            let outcome = picked.map(|handle| handle.path().to_path_buf());
            tx.send(AppEvent::TournamentBookPicked { outcome });
        });
    }

    /// File picker for the additional PolyGlot move book.
    fn pick_tournament_move_book(&mut self) {
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose a PolyGlot move book")
                .add_filter("PolyGlot book", &["bin"])
                .pick_file()
                .await;
            let outcome = picked.map(|handle| handle.path().to_path_buf());
            tx.send(AppEvent::TournamentMoveBookPicked { outcome });
        });
    }

    /// Save dialog for the tournament PGN.
    fn pick_tournament_pgn(&mut self) {
        let tx = self.events_tx.clone();
        self.spawn(async move {
            // Start in the default output folder, which is writable even in a
            // Finder launch (`/` is not).
            let directory = tournament::default_output_dir();
            let _ = tokio::fs::create_dir_all(&directory).await;
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose the tournament PGN file")
                .set_directory(&directory)
                .set_file_name("engine-match.pgn")
                .add_filter("PGN", &["pgn"])
                .save_file()
                .await;
            let outcome = picked.map(|handle| handle.path().to_path_buf());
            tx.send(AppEvent::TournamentPgnPicked { outcome });
        });
    }

    /// Show the file picker for a PolyGlot book and read it off the UI thread.
    fn load_book(&mut self) {
        self.set_status(StatusLevel::Info, "Choose a PolyGlot book…".to_string());
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter("PolyGlot book", &["bin"])
                .pick_file()
                .await;
            let outcome = match picked {
                None => None,
                Some(handle) => Some(read_book(handle.path().to_path_buf()).await),
            };
            tx.send(AppEvent::BookLoaded {
                outcome,
                startup: false,
            });
        });
    }

    /// Multi-folder picker for the Load-tablebases dialog (Browse…). The
    /// folders replace the open dialog's field text, joined as one
    /// `SyzygyPath` string; Apply sends it.
    fn pick_tablebase_dirs(&mut self) {
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose tablebase folders")
                .pick_folders()
                .await;
            let outcome = picked.map(|handles| {
                handles
                    .into_iter()
                    .map(|handle| handle.path().to_path_buf())
                    .collect::<Vec<_>>()
            });
            tx.send(AppEvent::TablebasesPicked { outcome });
        });
    }

    /// Engine → Tablebases in engines: use the configured `SyzygyPath` or
    /// leave the tablebases out. Switching off clears the loaded engines' path
    /// at once; switching on sends the saved path again. Engines started
    /// afterwards see the switch through [`engine_startup_settings`].
    fn toggle_tablebases(&mut self) {
        let Some(path) = self.settings.tablebase_path.clone() else {
            self.set_status(
                StatusLevel::Warning,
                "Load tablebases first (Engine menu).".to_string(),
            );
            return;
        };
        let on = !self.settings.tablebases_in_engines;
        self.settings.tablebases_in_engines = on;
        self.save_settings();
        // `""` is not a reliable clear: PawnForge (and engines with the same
        // parser) report "Unknown option: SyzygyPath value" and keep their
        // loaded path. The Stockfish sentinel `<empty>` clears both it and
        // Stockfish.
        let sent = if on {
            path.as_str()
        } else {
            tablebases::SYZYGY_EMPTY
        };
        for host in self.engines.iter_mut().flatten() {
            host.set_option(tablebases::SYZYGY_PATH, sent);
        }
        let text = if on {
            format!("Tablebases in engines: on ({}).", path)
        } else {
            "Tablebases in engines: off; the engines ignore the configured path.".to_string()
        };
        self.set_status(StatusLevel::Info, text);
    }

    /// Apply the `SyzygyPath` string from the Load-tablebases dialog: keep
    /// it in the settings, send it to every loaded engine now, and give it
    /// to every engine started afterwards. An empty string clears the
    /// tablebases again (the engines take an empty path until they are
    /// restarted, after which they get none). Applying a non-empty path
    /// switches the tablebases on, because applying means using it.
    fn set_tablebase_path(&mut self, raw: &str) {
        let value = raw.trim().to_string();
        let cleared = value.is_empty();
        self.settings.tablebase_path = (!cleared).then(|| value.clone());
        if !cleared {
            self.settings.tablebases_in_engines = true;
        }
        // The path is one global setting; a stale `SyzygyPath` saved for an
        // engine file must not override it later.
        for saved in self.settings.engine_options.values_mut() {
            saved.remove(tablebases::SYZYGY_PATH);
        }
        for slot in 0..2 {
            if cleared {
                self.engine_settings[slot].remove(tablebases::SYZYGY_PATH);
            } else {
                self.engine_settings[slot]
                    .insert(tablebases::SYZYGY_PATH.to_string(), value.clone());
            }
        }
        self.save_settings();
        let sent = if cleared || !self.settings.tablebases_in_engines {
            tablebases::SYZYGY_EMPTY
        } else {
            value.as_str()
        };
        for host in self.engines.iter_mut().flatten() {
            host.set_option(tablebases::SYZYGY_PATH, sent);
        }
        let text = if cleared {
            "Tablebases cleared off the loaded engines; new engines get no path.".to_string()
        } else {
            "Tablebase path set for the loaded engines.".to_string()
        };
        self.set_status(StatusLevel::Info, text);
    }

    /// Write the current settings to disk, off the UI thread. One writer at a
    /// time: changes made while a write is in flight are saved again when it
    /// finishes, so an older snapshot cannot overwrite a newer one.
    fn save_settings(&mut self) {
        let Some(path) = self.settings_file.clone() else {
            self.set_status(
                StatusLevel::Warning,
                "Settings are not saved: there is no home folder.".to_string(),
            );
            return;
        };
        if self.settings_saving {
            self.settings_save_pending = true;
            return;
        }
        self.settings_saving = true;
        let snapshot = self.settings.clone();
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let outcome = settings::save_sync(&path, &snapshot);
            tx.send(AppEvent::SettingsSaved { outcome });
        });
    }

    /// Debounce a settings write after a window or panel resize. A drag emits
    /// a size change every frame; only the final size is written.
    fn schedule_settings_save(&mut self) {
        self.settings_save_due = Some(Instant::now() + SETTINGS_SAVE_DEBOUNCE);
    }

    /// Keep the window size in the settings when the user changes it, so the
    /// next launch opens the same size.
    fn save_window_size(&mut self, ctx: &egui::Context) {
        let size = ctx.input(|i| i.viewport().inner_rect.map(|rect| rect.size()));
        let Some(size) = size else {
            return;
        };
        let size = settings::WindowSize {
            width: size.x,
            height: size.y,
        };
        if self.saved_window_size == Some(size) {
            return;
        }
        self.saved_window_size = Some(size);
        self.settings.window_size = Some(size);
        self.schedule_settings_save();
    }

    /// Keep the evaluation and move-list panel sizes in the settings when the
    /// user drags them, so the next launch opens the same layout. The sizes are
    /// rounded to whole points, and clamped to the allowed range, so a frame
    /// during a drag does not write a different value than the stored one.
    fn save_panel_sizes(&mut self, bottom_height: f32, right_width: f32) {
        let sizes = settings::PanelSizes {
            right_width: right_width.round().max(eval_panel::MIN_WIDTH),
            bottom_height: bottom_height.round().max(MIN_MOVE_LIST_HEIGHT),
        };
        if self.settings.panels == Some(sizes) {
            return;
        }
        self.settings.panels = Some(sizes);
        self.schedule_settings_save();
    }

    /// Index the ECO opening-name table at start-up, off the UI thread.
    fn load_opening_names(&mut self) {
        let Some(path) = crate::assets::opening_data_path() else {
            return;
        };
        let error_path = path.clone();
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let outcome = match tokio::task::spawn_blocking(move || OpeningNames::load(&path)).await
            {
                Ok(result) => result,
                Err(join) => Err(OpeningError::Read {
                    path: error_path,
                    source: std::io::Error::other(join),
                }),
            };
            tx.send(AppEvent::OpeningNamesLoaded { outcome });
        });
    }

    /// The start-up book, when one is wanted: `SILICONBOARD_BOOK` for an
    /// explicit start-up book, otherwise the book the user loaded last
    /// (settings `book_path`). There is no default book: without either,
    /// the app starts without one and the user picks a book via
    /// Engine → Load PolyGlot book…. Reading runs off the UI thread.
    fn load_startup_book(&mut self) {
        let Some(path) = startup_book_path(&self.settings) else {
            return;
        };
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let outcome = Some(read_book(path).await);
            tx.send(AppEvent::BookLoaded {
                outcome,
                startup: true,
            });
        });
    }

    /// The start-up PGN: `PGN/WorldChamp1972.pgn` next to the asset tree —
    /// the repository folder for a dev run, `Contents/Resources/PGN/` in the
    /// packaged bundle — is opened automatically and its first game selected.
    /// Reading runs off the UI thread like File → Open PGN…; a
    /// missing file starts the app without it.
    fn load_startup_pgn(&mut self) {
        let Some(path) = crate::assets::project_pgn_path("WorldChamp1972.pgn") else {
            log::info!("no start-up PGN: PGN/WorldChamp1972.pgn is not next to the assets");
            return;
        };
        let request = self.next_pgn_request();
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let outcome = Some(read_pgn(&path).await);
            tx.send(AppEvent::PgnOpened { request, outcome });
        });
    }

    /// Take the next Open PGN sequence number. A result that arrives with an
    /// older number is from a file the user has already replaced; the reader
    /// ignores it (BUG-AUDIT-05).
    fn next_pgn_request(&mut self) -> u64 {
        self.pgn_request = self.pgn_request.wrapping_add(1);
        self.pgn_request
    }

    /// Load one parsed PGN game onto the board. `game_number` is set when it
    /// came from a multi-game file, for the status line.
    fn load_pgn_game(
        &mut self,
        name: &str,
        import: PgnImport,
        game_number: Option<(usize, usize)>,
    ) {
        let result = import.result;
        let termination = import.termination.trim().to_string();
        let (white, black) = import.players.clone();
        self.pgn_players = (!(white.is_empty() && black.is_empty())).then_some((white, black));
        let mut game = import.game;
        let position_over = game.is_game_over();
        game.goto_start();
        self.game = game;
        self.pgn_termination = (!termination.is_empty()).then_some(termination);
        self.ending = if position_over {
            None
        } else {
            result.map(|result| Ending::FromPgn {
                winner: result.winner(),
            })
        };
        self.game_started = false;
        self.reset_clock();
        self.view.clear_selection();
        let base = match game_number {
            Some((index, total)) => {
                format!("Opened {} — game {} of {}", name, index + 1, total)
            }
            None => format!("Opened {}", name),
        };
        let base = match import.ignored.summary() {
            Some(note) => format!("{} ({})", base, note),
            None => base,
        };
        let text = match self.result_text() {
            Some(result) => format!("{} — {}.", base, result),
            None => base,
        };
        self.set_status(StatusLevel::Info, text);
    }

    /// Load the game at `index` from the multi-game PGN browser.
    fn select_pgn_game(&mut self, index: usize) {
        let Some(collection) = &self.pgn_collection else {
            return;
        };
        let Some(entry) = collection.games.get(index) else {
            return;
        };
        let name = collection.name.clone();
        let total = collection.games.len();
        let text = entry.text.clone();
        match pgn::parse_pgn_with_report(&text) {
            Ok(import) => {
                self.pgn_browser_selected = index;
                self.load_pgn_game(&name, import, Some((index, total)));
            }
            Err(err) => self.set_status(
                StatusLevel::Error,
                format!("Game {} could not be loaded: {}", index + 1, err),
            ),
        }
    }

    /// Leave the multi-game PGN browser and show the normal move list again.
    fn clear_pgn_browser(&mut self) {
        self.pgn_collection = None;
        self.pgn_browser_selected = 0;
        self.pgn_browser_list_open = false;
    }

    /// Show the file picker for a PGN file, then read and parse it.
    fn open_pgn(&mut self) {
        self.set_status(StatusLevel::Info, "Choose a PGN file…".to_string());
        let request = self.next_pgn_request();
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter("PGN", &["pgn"])
                .pick_file()
                .await;
            let outcome = match picked {
                None => None,
                Some(handle) => {
                    let path = handle.path().to_path_buf();
                    Some(read_pgn(&path).await)
                }
            };
            tx.send(AppEvent::PgnOpened { request, outcome });
        });
    }

    /// Ask for a destination, then write `contents` there.
    fn save_text(
        &mut self,
        what: &'static str,
        contents: String,
        default_name: &'static str,
        filter: &'static str,
        extensions: &'static [&'static str],
    ) {
        self.set_status(
            StatusLevel::Info,
            format!("Choose where to save the {}…", what),
        );
        let tx = self.events_tx.clone();
        self.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_file_name(default_name)
                .add_filter(filter, extensions)
                .save_file()
                .await;
            let outcome = match picked {
                None => None,
                Some(handle) => {
                    let path = handle.path().to_path_buf();
                    Some(tokio::fs::write(&path, contents).await.map(|()| path))
                }
            };
            tx.send(AppEvent::FileSaved { what, outcome });
        });
    }

    /// Whether the slot holds an engine that has not exited.
    fn engine_live(&self, slot: usize) -> bool {
        self.engines[slot]
            .as_ref()
            .is_some_and(|host| host.status() != AnalysisStatus::Offline)
    }

    /// Switch analysis on or off for one slot and point it at the displayed
    /// position. The engine stays loaded either way.
    fn set_engine_analysis(&mut self, slot: usize, enabled: bool) {
        self.analysis_on[slot] = enabled;
        let fen = self.game.fen();
        let goal = self.goal_for(slot);
        if let Some(host) = self.engines[slot].as_mut() {
            host.set_analysis(enabled, &fen, goal);
        }
    }

    /// The engine slot that is on move, if any. During a tournament this uses
    /// the game's colour mapping instead of the normal mode seats.
    fn engine_slot_to_move(&self) -> Option<usize> {
        if let Some(session) = &self.tournament {
            if session.active() {
                if session.is_playing() {
                    return Some(session.slot_for_color(self.game.turn()));
                }
                return None;
            }
        }
        match self.mode.seat(self.game.turn()) {
            Seat::Engine(slot) => Some(slot),
            Seat::Human => None,
        }
    }

    /// What `slot` should do with the displayed position. An engine is asked
    /// for a move only when it is on the move at the live end of a game that
    /// is still running. Browsing an earlier position never asks for a move.
    fn goal_for(&self, slot: usize) -> SearchGoal {
        let on_move = self.engine_slot_to_move() == Some(slot);
        if self.live_play() && on_move {
            if self.book_choices().is_empty() {
                return SearchGoal::Move(self.engine_go());
            }
            // The book answers this move, so the engine does not search.
            return SearchGoal::Rest;
        }
        // In a play mode an engine thinks only when it is to move: nothing is
        // searched on the other side's time.
        if self.mode == GameMode::Analysis
            && !self
                .tournament
                .as_ref()
                .is_some_and(TournamentSession::active)
        {
            SearchGoal::Analyse
        } else {
            SearchGoal::Rest
        }
    }

    /// Book moves the engine on the move plays instead of searching. Empty
    /// unless book play is on, a play mode is live, an engine is on the move,
    /// and the position is in the book. A tournament may combine its EPD start
    /// positions with the PolyGlot book's opening moves.
    fn book_choices(&self) -> Vec<(Move, u16)> {
        let engine_to_move = self.engine_slot_to_move().is_some();
        if !self.live_play() || !engine_to_move || self.game.total_plies() >= BOOK_MAX_PLIES {
            return Vec::new();
        }
        // A PolyGlot book chosen in the tournament dialog is the tournament's
        // opening book. EPD books only supply start positions.
        if let Some(session) = &self.tournament {
            if session.active() {
                if let Some(book) = session.book.as_ref().and_then(TournamentBook::polyglot) {
                    return self.book_moves_cached(book, (true, self.tournament_generation));
                }
            }
        }
        if !self.book_in_games {
            return Vec::new();
        }
        match &self.book {
            Some((_, book)) => self.book_moves_cached(book, (false, self.book_generation)),
            None => Vec::new(),
        }
    }

    /// The book moves of the displayed position from `book`. An engine on the
    /// move asks every frame, so the answer is kept for the position and the
    /// book it came from (`source`).
    fn book_moves_cached(&self, book: &PolyglotBook, source: (bool, u64)) -> Vec<(Move, u16)> {
        let fen = self.game.fen();
        if let Some((memo_source, memo_fen, moves)) = self.book_memo.borrow().as_ref() {
            if *memo_source == source && *memo_fen == fen {
                return moves.clone();
            }
        }
        let moves = self.game.book_moves(book);
        *self.book_memo.borrow_mut() = Some((source, fen, moves.clone()));
        moves
    }

    /// Play a weighted-random book move for the engine on the move, if the
    /// book has one. One move per frame; the frame asks for another so a run
    /// of book moves keeps going.
    fn play_book_reply(&mut self, ctx: &egui::Context) {
        let choices = self.book_choices();
        let weights: Vec<u16> = choices.iter().map(|(_, weight)| *weight).collect();
        let Some(index) = self.book_picker.pick(&weights) else {
            return;
        };
        let Some(&(m, _)) = choices.get(index) else {
            return;
        };
        let Some(slot) = self.engine_slot_to_move() else {
            return;
        };
        match self.game.apply_move(m) {
            Ok(()) => {
                let san = self
                    .game
                    .active_history()
                    .last()
                    .map(|ply| ply.san.to_string())
                    .unwrap_or_default();
                self.view.clear_selection();
                self.tournament_observe_book_move();
                let mut text =
                    format!("Engine {} played {} from the book.", slot_letter(slot), san);
                if let Some(result) = self.result_text() {
                    text.push_str(&format!(" {}.", result));
                }
                self.set_status(StatusLevel::Info, text);
                ctx.request_repaint();
            }
            Err(err) => {
                log::error!("book move {:?} rejected: {}", m, err);
                self.set_status(
                    StatusLevel::Error,
                    format!("The opening book gave an illegal move: {}", err),
                );
            }
        }
    }

    /// The limits for an engine's next move: its clock when there is one, or
    /// a fixed think time in a game without a clock.
    fn engine_go(&self) -> GoControl {
        self.clock
            .as_ref()
            .map_or(GoControl::MoveTime(ENGINE_MOVE_MS), Clock::go_control)
    }

    /// `true` when the game is in a play mode, at the live end, and not over
    /// (checkmate, draw, resignation, agreed draw, or a flag). Only then do
    /// clocks run and moves get played.
    fn live_play(&self) -> bool {
        self.mode != GameMode::Analysis
            && self.game_started
            && self.game.cur_ply() == self.game.total_plies()
            && !self.game.is_game_over()
            && self.current_ending().is_none()
    }

    /// How the game ended by the game loop, if it has: a resignation, an agreed
    /// draw, or a flag. Checkmate and the other drawn positions are not included.
    fn current_ending(&self) -> Option<Ending> {
        self.ending
            .or_else(|| self.clock_flagged().map(|loser| Ending::OnTime { loser }))
    }

    /// `true` while a game against an engine is running: the human can resign or offer a draw.
    fn vs_engine_live(&self) -> bool {
        matches!(self.mode, GameMode::HumanVsEngine { .. }) && self.live_play()
    }

    /// `true` when the human may claim a threefold-repetition draw now. FIDE
    /// gives that right to the player to move, so the engine's turn does not
    /// enable the menu item.
    fn can_claim_threefold(&self) -> bool {
        self.vs_engine_live()
            && self.human_colour() == Some(self.game.turn())
            && self.game.is_threefold_repetition()
    }

    /// The colour the human plays, in a game against an engine.
    fn human_colour(&self) -> Option<PieceColor> {
        match self.mode {
            GameMode::HumanVsEngine { human, .. } => Some(human),
            _ => None,
        }
    }

    /// The human resigns. The game ends, and the engine wins.
    fn resign(&mut self) {
        if !self.vs_engine_live() {
            self.set_status(
                StatusLevel::Warning,
                "No game against an engine is running.".to_string(),
            );
            return;
        }
        if let Some(loser) = self.human_colour() {
            self.ending = Some(Ending::Resigned { loser });
        }
        let text = format!(
            "You resigned. {}.",
            self.current_ending()
                .map_or_else(String::new, Ending::description)
        );
        self.set_status(StatusLevel::Info, text);
        self.sync_position(true);
    }

    /// End an engine-vs-engine game on a draw that no engine can claim: a
    /// threefold repetition (FIDE 9.2) or the fifty-move rule (9.3). The
    /// tournament loop has its own threefold check; a play-mode engine game
    /// has no claimant and would otherwise run forever. The score-based
    /// tournament adjudicator is deliberately not used here: a cp threshold is
    /// match policy, not a rule.
    fn adjudicate_engine_vs_engine(&mut self) {
        if self.mode != GameMode::EngineVsEngine || !self.live_play() || self.ending.is_some() {
            return;
        }
        if self.game.is_threefold_repetition() {
            self.ending = Some(Ending::ThreefoldRepetition);
            log::info!("engine-vs-engine draw adjudicated: threefold repetition");
            self.set_status(
                StatusLevel::Info,
                "Draw by threefold repetition (adjudicated).".to_string(),
            );
        } else if self.game.is_fifty_move_rule() {
            self.ending = Some(Ending::FiftyMoveRule);
            log::info!("engine-vs-engine draw adjudicated: fifty-move rule");
            self.set_status(
                StatusLevel::Info,
                "Draw by the fifty-move rule (adjudicated).".to_string(),
            );
        }
    }

    /// The human claims a draw because the displayed position occurred three
    /// times (FIDE Article 9.2). Only the player to move may claim.
    fn claim_threefold(&mut self) {
        if !self.can_claim_threefold() {
            self.set_status(
                StatusLevel::Warning,
                "Threefold repetition can only be claimed on your turn in a running game."
                    .to_string(),
            );
            return;
        }
        self.ending = Some(Ending::ThreefoldRepetition);
        log::info!("draw claimed by threefold repetition");
        self.set_status(
            StatusLevel::Info,
            "Draw claimed: the position occurred three times.".to_string(),
        );
        self.sync_position(true);
    }

    /// The human offers a draw. The engine evaluates the position for a moment,
    /// and accepts when the score is close to equal (see `finish_draw_offer`).
    fn offer_draw(&mut self) {
        if !self.vs_engine_live() {
            self.set_status(
                StatusLevel::Warning,
                "No game against an engine is running.".to_string(),
            );
            return;
        }
        let slot = self.mode.engine_slots()[0];
        let letter = slot_letter(slot);
        if !self.engine_live(slot) {
            self.set_status(
                StatusLevel::Error,
                format!("Engine {} is not running, so it cannot answer.", letter),
            );
            return;
        }
        if !self.analysis_on[slot] {
            self.set_status(
                StatusLevel::Warning,
                format!(
                    "Engine {} is switched off, so it cannot judge the position. Switch it on.",
                    letter
                ),
            );
            return;
        }
        let fen = self.game.fen();
        if let Some(host) = self.engines[slot].as_mut() {
            host.set_position(
                &fen,
                SearchGoal::Evaluate(GoControl::MoveTime(EVALUATION_MS)),
            );
        }
        self.draw_offer = Some(slot);
        self.set_status(
            StatusLevel::Info,
            format!("Engine {} is considering the draw…", letter),
        );
    }

    /// Decide a pending draw offer once the engine's evaluation has arrived. The
    /// offer is dropped when the game ends, or when the engine stops evaluating
    /// (Abort, or the engine is switched off).
    fn finish_draw_offer(&mut self) {
        let Some(slot) = self.draw_offer else {
            return;
        };
        if !self.live_play() || !self.analysis_on[slot] {
            self.draw_offer = None;
            return;
        }
        let Some(result) = self.engines[slot]
            .as_mut()
            .and_then(EngineHost::take_evaluation)
        else {
            return;
        };
        self.draw_offer = None;
        let letter = slot_letter(slot);
        let white_to_move = self.game.turn() == PieceColor::White;
        let score = result.map(|score| eval_panel::white_point_of_view(score, white_to_move));
        if accepts_draw(score) {
            self.ending = Some(Ending::DrawAgreed);
            self.set_status(
                StatusLevel::Info,
                format!("Engine {} accepts the draw.", letter),
            );
        } else if score.is_none() {
            self.set_status(
                StatusLevel::Warning,
                format!(
                    "Engine {} has no evaluation of this position, so it declines the draw.",
                    letter
                ),
            );
        } else {
            self.set_status(
                StatusLevel::Warning,
                format!(
                    "Engine {} declines the draw: it does not see the position as equal.",
                    letter
                ),
            );
        }
        // Back to the goals of the position: an engine to move searches again.
        self.sync_position(true);
    }

    /// The side that ran out of time, if any.
    fn clock_flagged(&self) -> Option<PieceColor> {
        self.clock.as_ref().and_then(Clock::flagged)
    }

    /// What ends the game, when it has ended: a flag, or the position itself.
    fn result_text(&self) -> Option<String> {
        if let Some(ending) = self.current_ending() {
            return Some(ending.description());
        }
        self.game.is_game_over().then(|| self.game.outcome())
    }

    /// Keep the clock in step with the game and detect a flag. The clock runs
    /// only while [`Self::live_play`] holds. Each ply played since the last
    /// call is one tap: the mover's increment is added and the turn passes.
    fn sync_clock(&mut self, ctx: &egui::Context) {
        let turn = self.game.turn();
        let playing = self.live_play();
        let Some(clock) = self.clock.as_mut() else {
            return;
        };
        let was_flagged = clock.flagged();
        if playing {
            if clock.side != turn {
                clock.tap();
            }
            clock.start();
            // Commits the running side; a side that has run out is flagged here.
            if clock.remaining_ms(clock.side) == 0 {
                clock.pause();
            }
        } else {
            clock.pause();
        }
        let flagged = clock.flagged();
        if playing && flagged.is_none() {
            ctx.request_repaint_after(CLOCK_REFRESH);
        }
        if let (Some(loser), None) = (flagged, was_flagged) {
            self.set_status(
                StatusLevel::Warning,
                format!(
                    "{} ran out of time. {} wins on time.",
                    colour_name(loser),
                    colour_name(loser.opposite())
                ),
            );
            // A move search still running for the game now gets the Analyse goal,
            // so it stops, and its move is dropped when it arrives.
            self.sync_position(true);
        }
    }

    /// Start the clock again from full time, for the side to move. Called when
    /// the game is replaced or the mode changes, so no time carries over.
    fn reset_clock(&mut self) {
        let turn = self.game.turn();
        if let Some(clock) = self.clock.as_mut() {
            clock.reset(turn);
        }
    }

    /// Set the time control (`None` = no clock). Clocks start from full time.
    fn set_time_control(&mut self, control: Option<TimeControl>) {
        let text = match control {
            Some(control) => format!("Time control: {}.", control.label()),
            None => "No clock.".to_string(),
        };
        self.clock = control.map(|control| Clock::new(control, self.game.turn()));
        self.settings.time_control = control;
        self.save_settings();
        self.set_status(StatusLevel::Info, text);
        self.sync_position(true);
    }

    /// The two player bars in board order: the one at the top first. Each has
    /// the side's name and, in a play mode with a clock, the clock.
    fn player_bars(&self) -> [PlayerBar; 2] {
        let (top, bottom) = if self.view.white_on_bottom {
            (PieceColor::Black, PieceColor::White)
        } else {
            (PieceColor::White, PieceColor::Black)
        };
        [self.player_bar(top), self.player_bar(bottom)]
    }

    /// The name of the player of `color` in the loaded PGN, when it has one.
    fn pgn_player_name(&self, color: PieceColor) -> Option<String> {
        let (white, black) = self.pgn_players.as_ref()?;
        let name = match color {
            PieceColor::White => white,
            PieceColor::Black => black,
        };
        (!name.is_empty()).then(|| name.clone())
    }

    /// One side's bar. The name is the engine's own name (as its handshake
    /// gives it), `You` for the human, or the colour in analysis mode.
    fn player_bar(&self, color: PieceColor) -> PlayerBar {
        let name = match &self.tournament {
            Some(session) if session.active() => self.engine_name(session.slot_for_color(color)),
            _ => match self.mode {
                GameMode::Analysis => self
                    .pgn_player_name(color)
                    .unwrap_or_else(|| colour_name(color).to_string()),
                _ => match self.mode.seat(color) {
                    Seat::Human => "You".to_string(),
                    Seat::Engine(slot) => self.engine_name(slot),
                },
            },
        };
        let clock = match (&self.clock, self.mode) {
            (Some(clock), mode) if mode != GameMode::Analysis => Some(ClockFace {
                remaining_ms: clock.remaining_ms(color),
                base_ms: clock.control().base_ms,
                active: self.live_play() && clock.side == color,
                flagged: clock.flagged() == Some(color),
            }),
            _ => None,
        };
        PlayerBar { name, clock }
    }

    /// The name of the engine in `slot`: its handshake name, or `Engine A` before that.
    fn engine_name(&self, slot: usize) -> String {
        self.engines[slot]
            .as_ref()
            .and_then(|host| host.view().name)
            .unwrap_or_else(|| format!("Engine {}", slot_letter(slot)))
    }

    /// Game → Setup board…: edit the displayed position in the right panel.
    /// A running game is paused first, so an engine cannot move while the
    /// position is being changed; the game itself is replaced only when the
    /// user closes the editor on a valid position.
    fn open_setup_board(&mut self) {
        if self
            .tournament
            .as_ref()
            .is_some_and(TournamentSession::active)
        {
            self.set_status(
                StatusLevel::Warning,
                "Stop the tournament before editing the position.".to_string(),
            );
            return;
        }
        if self.setup.is_some() {
            self.set_status(
                StatusLevel::Info,
                "Setup Board is already open.".to_string(),
            );
            return;
        }
        self.game_started = false;
        if let Some(clock) = self.clock.as_mut() {
            clock.pause();
        }
        for slot in 0..2 {
            if self.engine_live(slot) {
                self.set_engine_analysis(slot, false);
            }
        }
        self.setup = Some(SetupBoard::from_position(&self.game.position()));
        self.sync_setup_fen_field();
        self.view.clear_selection();
        self.set_status(
            StatusLevel::Info,
            "Setup Board: choose a piece, then click a square. Right-click removes a piece."
                .to_string(),
        );
    }

    /// Keep the top FEN field on the edited position, but only after an actual
    /// change in the editor. Copying it every frame would erase what the user
    /// is typing there, because Enter loads that field into the editor.
    fn sync_setup_fen_field(&mut self) {
        let Some(setup) = &self.setup else {
            return;
        };
        if setup.revision() == self.setup_fen_revision {
            return;
        }
        self.fen_input = setup.fen();
        self.setup_fen_revision = setup.revision();
    }

    /// Enter in the top FEN field while Setup Board is open: load the typed
    /// position into the editor instead of replacing the game.
    fn load_setup_fen(&mut self) {
        let Some(setup) = self.setup.as_mut() else {
            return;
        };
        match setup.set_fen(&self.fen_input.clone()) {
            Ok(()) => {
                self.set_status(
                    StatusLevel::Info,
                    "FEN loaded into Setup Board.".to_string(),
                );
                self.sync_setup_fen_field();
            }
            Err(err) => self.set_status(StatusLevel::Error, err),
        }
    }

    /// Apply the Setup Board position and return to the normal board. The
    /// editor refuses to close while errors exist, so this ends in a playable
    /// position; the user then picks a game mode and presses Start.
    fn close_setup_board(&mut self) {
        let Some(setup) = self.setup.as_ref() else {
            return;
        };
        let chess = match setup.to_chess() {
            Ok(chess) => chess,
            Err(errors) => {
                if let Some(setup) = self.setup.as_mut() {
                    setup.refresh_errors();
                }
                let text = errors
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "The position is not valid.".to_string());
                self.set_status(StatusLevel::Error, format!("Setup Board: {}", text));
                return;
            }
        };
        self.game = GameState::from_position(chess);
        self.ending = None;
        self.pgn_termination = None;
        self.pgn_players = None;
        self.game_started = false;
        self.clear_pgn_browser();
        self.reset_clock();
        self.view.clear_selection();
        self.setup = None;
        for host in self.engines.iter_mut().flatten() {
            host.new_game();
        }
        self.sync_position(true);
        self.set_status(
            StatusLevel::Info,
            "Position set. Choose a game mode, then press Start.".to_string(),
        );
    }

    /// Leave Setup Board without changing the game.
    fn cancel_setup_board(&mut self) {
        if self.setup.take().is_none() {
            return;
        }
        self.sync_position(true);
        let text = if self.mode == GameMode::Analysis {
            "Setup cancelled. Press Start to analyse again."
        } else {
            "Setup cancelled. Press Start to continue the game."
        };
        self.set_status(StatusLevel::Info, text.to_string());
    }

    /// Game → Start from here…: two engines play the position in the FEN
    /// field with the current time control. The game's start position is that
    /// FEN, so the PGN carries `SetUp`/`FEN` and the moves from there.
    fn start_from_fen(&mut self) {
        let fen = self.fen_input.trim();
        let game = match GameState::from_fen(fen) {
            Ok(game) => game,
            Err(err) => {
                self.set_status(StatusLevel::Error, format!("Invalid FEN: {}", err));
                return;
            }
        };
        if game.is_game_over() {
            self.set_status(
                StatusLevel::Warning,
                "The FEN position is already over; nothing can be played from it.".to_string(),
            );
            return;
        }
        let mut missing = Vec::new();
        for slot in 0..2 {
            if !self.engine_live(slot) {
                missing.push(slot_letter(slot).to_string());
            }
        }
        if !missing.is_empty() {
            self.set_status(
                StatusLevel::Warning,
                format!("Load engine {} first.", missing.join(" and ")),
            );
            return;
        }

        self.game = game;
        self.ending = None;
        self.pgn_termination = None;
        self.pgn_players = None;
        self.game_started = false;
        self.view.clear_selection();
        // A play mode of its own: the PGN browser closes, the clock starts
        // from full time for the side to move, and both engines are pointed
        // at the position.
        self.set_mode(GameMode::EngineVsEngine);
        self.set_analysis(true);
        if self.live_play() {
            self.set_status(
                StatusLevel::Info,
                "Two engines play from the FEN position.".to_string(),
            );
        }
    }

    /// Switch the session to `mode`. The engines the mode plays with are
    /// switched on, so the game starts without a Start press. A missing engine
    /// is named in the status line. The clocks start from full time.
    fn set_mode(&mut self, mode: GameMode) {
        self.mode = mode;
        // A play mode is a new game of its own: the PGN games list closes.
        // Analysis only keeps it open — that is
        // the mode the user browses different games of a file in.
        if mode != GameMode::Analysis {
            self.clear_pgn_browser();
        }
        if mode != GameMode::Analysis {
            // A game against an engine or between engines has its own names.
            self.pgn_players = None;
        }
        self.game_started = false;
        // A tournament's adjudication result belongs to the finished match.
        // Leaving the match for a play mode should not keep the board blocked.
        if mode != GameMode::Analysis
            && self
                .tournament
                .as_ref()
                .is_some_and(|session| session.phase == TournamentPhase::Finished)
        {
            self.ending = None;
        }
        self.view.clear_selection();
        self.reset_clock();
        let mut missing = Vec::new();
        for &slot in mode.engine_slots() {
            if self.engine_live(slot) {
                self.set_engine_analysis(slot, true);
            } else {
                missing.push(slot_letter(slot).to_string());
            }
        }
        self.sync_position(true);
        if missing.is_empty() {
            let text = if mode == GameMode::Analysis {
                format!("{}.", mode.label())
            } else {
                format!("{}. Press Start to begin.", mode.label())
            };
            self.set_status(StatusLevel::Info, text);
        } else {
            self.set_status(
                StatusLevel::Warning,
                format!(
                    "{}: load engine {} first.",
                    mode.label(),
                    missing.join(" and ")
                ),
            );
        }
    }

    /// A card's play/pause toggle: switch analysis for that engine only. In a
    /// play mode only the engines the mode plays with have a switch, so an idle
    /// loaded engine cannot be started into a game that does not use it.
    fn toggle_engine(&mut self, slot: usize) {
        let letter = slot_letter(slot);
        if !self.engine_live(slot) {
            self.set_status(
                StatusLevel::Warning,
                format!("Engine {} is not loaded.", letter),
            );
            return;
        }
        if self.mode != GameMode::Analysis && !self.mode.engine_slots().contains(&slot) {
            self.set_status(
                StatusLevel::Warning,
                format!("Engine {} is not playing in this game.", letter),
            );
            return;
        }
        let enabled = !self.analysis_on[slot];
        self.set_engine_analysis(slot, enabled);
        let text = if enabled {
            format!("Analysis started for engine {}.", letter)
        } else {
            format!("Analysis stopped for engine {}.", letter)
        };
        self.set_status(StatusLevel::Info, text);
    }

    /// Start (`true`) or Abort (`false`) for every loaded engine. Engines stay
    /// loaded either way; nothing here quits a process.
    fn set_analysis(&mut self, enabled: bool) {
        let playing = self.mode != GameMode::Analysis;
        if playing {
            // Start begins the game; Abort pauses it. The engines are then
            // switched on or off below, with the game state already set.
            self.game_started = enabled;
        }
        let mut live = 0usize;
        for slot in 0..2 {
            if self.engine_live(slot) {
                live += 1;
                // In a play mode Start begins the game for the engines that play
                // it. A merely loaded engine is left switched off.
                let playing_slot = !playing || self.mode.engine_slots().contains(&slot);
                self.set_engine_analysis(slot, enabled && playing_slot);
            } else {
                self.analysis_on[slot] = false;
            }
        }
        if enabled && self.engines.iter().flatten().any(EngineHost::is_stopping) {
            // The engine answers `stop` with `bestmove` first; the new search
            // starts then, or the stall deadline restarts the engine.
            return self.set_status(
                StatusLevel::Warning,
                "An engine is still stopping its last search. The new search starts when it has stopped.".to_string(),
            );
        }
        let text = match (enabled, playing, live) {
            (true, true, _) if !self.live_play() => {
                return self.set_status(
                    StatusLevel::Warning,
                    "The game cannot go on from here: it is over, or the last move is not shown. Use Game → New game for a fresh game.".to_string(),
                );
            }
            (true, true, _) => "Game started.".to_string(),
            (false, true, _) => "Game paused. Start continues it.".to_string(),
            (true, false, 0) => {
                return self.set_status(StatusLevel::Warning, "Load an engine first.".to_string());
            }
            (true, false, _) => "Analysis started.".to_string(),
            (false, false, _) => "Analysis stopped.".to_string(),
        };
        self.set_status(StatusLevel::Info, text);
    }

    /// Ask engines to quit, escalating to a kill, without blocking the UI.
    fn quit_processes(&self, processes: Vec<EngineProcess>) {
        for process in processes {
            self.spawn(async move {
                if let Err(err) = process.terminate().await {
                    log::warn!("engine did not accept quit: {}", err);
                }
            });
        }
    }

    /// Replace the status line.
    fn set_status(&mut self, level: StatusLevel, text: String) {
        self.status = text;
        self.status_level = level;
    }

    /// Run a future on the Tokio runtime, detached.
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }

    /// Quit all engines before the window closes (`stop`, `quit`, then kill).
    fn shutdown_engines(&mut self) {
        let processes: Vec<EngineProcess> = self
            .engines
            .iter_mut()
            .flatten()
            .map(EngineHost::shutdown)
            .collect();
        if processes.is_empty() {
            return;
        }
        self.runtime.block_on(async move {
            for process in &processes {
                if let Err(err) = process.terminate().await {
                    log::warn!("engine did not accept quit at exit: {}", err);
                }
            }
            // Leave as soon as every engine has gone, instead of always waiting.
            let deadline = Instant::now() + EXIT_GRACE;
            while processes.iter().any(|process| !process.has_exited()) && Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }

    /// Analysis-mode evaluation arrows: the first move of each live engine's
    /// principal variation. Engine A draws green, engine B red. A new search
    /// (a new evaluation) moves the arrow. When both engines choose the same
    /// move only one arrow is drawn, deduped by from/to squares; engine A's
    /// green arrow wins.
    ///
    /// An arrow belongs to a search that is **running right now** on the
    /// displayed position: an engine that was switched off (Abort or its card
    /// toggle), and statistics that belong to an earlier position, draw
    /// nothing. That keeps the board from showing a stale "best move".
    fn eval_arrows(&self) -> Vec<board_view::EvalArrow> {
        if self.mode != GameMode::Analysis || !self.settings.analysis_arrows {
            return Vec::new();
        }
        let fen = self.game.fen();
        let mut arrows: Vec<board_view::EvalArrow> = Vec::new();
        for slot in [0usize, 1] {
            if !self.analysis_on[slot] {
                continue;
            }
            let Some(host) = self.engines[slot].as_ref() else {
                continue;
            };
            let view = host.view();
            if view.status != AnalysisStatus::Thinking {
                continue;
            }
            if view.searched_fen.as_deref() != Some(fen.as_str()) {
                continue;
            }
            let Some((from, to)) = view
                .stats
                .pv
                .first()
                .and_then(|uci| board_view::arrow_endpoints(uci))
            else {
                continue;
            };
            if arrows
                .iter()
                .any(|arrow| arrow.from == from && arrow.to == to)
            {
                continue;
            }
            arrows.push(board_view::EvalArrow {
                from,
                to,
                color: if slot == 0 {
                    theme::ARROW_ENGINE_A
                } else {
                    theme::ARROW_ENGINE_B
                },
            });
        }
        arrows
    }

    /// Snapshot of the evaluation panel for this frame.
    fn eval_summary(&mut self) -> EvalSummary {
        let fen = self.game.fen();
        let cards = [0usize, 1].map(|slot| {
            let view = self.engines[slot].as_ref().map(EngineHost::view);
            let pv_san = match view.as_ref() {
                Some(v) if !v.stats.pv.is_empty() => {
                    let fresh = self.pv_cache[slot]
                        .as_ref()
                        .is_some_and(|cache| cache.fen == fen && cache.uci == v.stats.pv);
                    if !fresh {
                        let san = Arc::new(self.game.uci_moves_to_san(&v.stats.pv));
                        self.pv_cache[slot] = Some(PvCache {
                            fen: fen.clone(),
                            uci: v.stats.pv.clone(),
                            san: Arc::clone(&san),
                        });
                    }
                    self.pv_cache[slot]
                        .as_ref()
                        .map(|cache| Arc::clone(&cache.san))
                        .unwrap_or_default()
                }
                _ => Arc::new(Vec::new()),
            };
            // A play-mode engine's button says whether the game is running, but
            // only the engines the mode plays with follow the game. An engine
            // that is merely loaded stays at play.
            let playing_slot =
                self.mode == GameMode::Analysis || self.mode.engine_slots().contains(&slot);
            let running = self.engine_live(slot)
                && playing_slot
                && match self.mode {
                    GameMode::Analysis => self.analysis_on[slot],
                    _ => self.live_play(),
                };
            EvalCard {
                slot,
                view,
                binary_path: self.engines[slot].as_ref().map(EngineHost::binary_path),
                pv_san,
                running,
                toggle_enabled: playing_slot
                    && !self
                        .tournament
                        .as_ref()
                        .is_some_and(TournamentSession::active),
            }
        });
        let tournament_active = self
            .tournament
            .as_ref()
            .is_some_and(TournamentSession::active);
        EvalSummary {
            cards,
            engine_choices: self.engine_choices(),
            outcome: self.result_text().unwrap_or_else(|| self.game.outcome()),
            book_moves: if tournament_active {
                Vec::new()
            } else {
                self.book_moves.clone()
            },
            book_name: self.book.as_ref().map(|(name, _)| name.clone()),
            opening_name: if !tournament_active && self.book_in_games && self.book.is_some() {
                self.opening_name.clone()
            } else {
                None
            },
            book_in_games: self.book_in_games && !tournament_active,
            figurines: self.settings.figurines_eval,
            status: self.status.clone(),
            status_level: self.status_level,
        }
    }
}

impl eframe::App for SiliconBoardApp {
    /// Background frame: loading, event draining, engine pumping.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.logic_frame(ctx);
    }

    /// Visual frame: menu and FEN bar on top, evaluation dashboard on the
    /// right, move list at the bottom, board in the centre.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let menu_state = MenuState {
            mode: self.mode,
            book_in_games: self.book_in_games,
            session_log: self.settings.session_log,
            vs_engine_live: self.vs_engine_live(),
            can_claim_threefold: self.can_claim_threefold(),
            options_ready: [0, 1].map(|slot| {
                self.engine_live(slot)
                    && self.engines[slot]
                        .as_ref()
                        .is_some_and(|host| host.status() != AnalysisStatus::Starting)
            }),
            book_name: self.book.as_ref().map(|(name, _)| name.as_str()),
            tablebase_path: self.settings.tablebase_path.as_deref(),
            tablebases_in_engines: self.settings.tablebases_in_engines,
            figurines_move_list: self.settings.figurines_move_list,
            figurines_eval: self.settings.figurines_eval,
            analysis_arrows: self.settings.analysis_arrows,
            board_themes: &self.board_themes,
            board_theme: Some(
                self.settings
                    .board_theme
                    .as_deref()
                    .unwrap_or(crate::assets::DEFAULT_BOARD_IMAGE),
            ),
            piece_sets: &self.piece_sets,
            piece_set: Some(
                self.settings
                    .piece_set
                    .as_deref()
                    .unwrap_or(crate::assets::DEFAULT_PIECE_SET),
            ),
            ply_count: self.game.total_plies(),
            tournament_running: self
                .tournament
                .as_ref()
                .is_some_and(TournamentSession::active),
            tournament_has_run: self.tournament.is_some(),
            pgn_games_available: self.pgn_collection.is_some(),
        };
        let human_to_move = if self
            .tournament
            .as_ref()
            .is_some_and(TournamentSession::active)
        {
            false
        } else {
            match self.mode {
                GameMode::Analysis => true,
                _ => {
                    self.game_started
                        && self.mode.seat(self.game.turn()) == Seat::Human
                        && self.current_ending().is_none()
                }
            }
        };
        let bars = self.player_bars();
        let mut requested: Option<MenuAction> = None;
        let fen_input = &mut self.fen_input;

        egui::Panel::top("top_bar")
            .frame(
                egui::Frame::new()
                    .fill(theme::TOOLBAR_BG)
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 5,
                        bottom: 5,
                    }),
            )
            .show(ui, |ui| {
                if let Some(action) = menu_bar::show(ui, &menu_state, fen_input) {
                    requested = Some(action);
                }
            });

        let follow = self.game.cur_ply() != self.last_ply;
        self.last_ply = self.game.cur_ply();
        let figurines_move_list = self.settings.figurines_move_list;
        let revision = self.game.revision();
        if self
            .pgn_cache
            .as_ref()
            .is_none_or(|(rev, _)| *rev != revision)
        {
            self.pgn_cache = Some((revision, self.game.pgn_lines()));
        }
        let (cached_revision, lines) = self.pgn_cache.take().unwrap_or((revision, Vec::new()));
        let refresh_layouts = self
            .pgn_galleys
            .as_ref()
            .is_none_or(|(rev, figs, _)| *rev != revision || *figs != figurines_move_list);
        if refresh_layouts {
            let mut galleys = vec![None; self.game.total_plies() + 1];
            for line in &lines {
                for (entry, color) in [
                    (&line.white, theme::WHITE_MOVE),
                    (&line.black, theme::BLACK_MOVE),
                ] {
                    let Some(entry) = entry else { continue };
                    let job =
                        crate::gui::figurines::san_job(ui, &entry.san, color, figurines_move_list);
                    let galley = ui.ctx().fonts_mut(|fonts| fonts.layout_job(job));
                    if let Some(slot) = galleys.get_mut(entry.ply) {
                        *slot = Some(galley);
                    }
                }
            }
            self.pgn_galleys = Some((revision, figurines_move_list, galleys));
        }
        let (galley_revision, galley_figurines, galleys) =
            self.pgn_galleys
                .take()
                .unwrap_or((revision, figurines_move_list, Vec::new()));
        let move_list_height = self
            .settings
            .panels
            .map_or(DEFAULT_MOVE_LIST_HEIGHT, |sizes| sizes.bottom_height);
        let result = self
            .current_ending()
            .map(|ending| ending.result_token().to_string())
            .or_else(|| {
                self.game
                    .is_game_over()
                    .then(|| pgn::result_token(&self.game, None).to_string())
            });
        // The bottom move list shows the result token; when the game came from
        // a PGN with a `Termination` tag, show the reason next to it.
        let result = result_label(result, self.pgn_termination.as_deref());
        let move_list = egui::Panel::bottom("move_list")
            .resizable(true)
            .default_size(move_list_height)
            .min_size(MIN_MOVE_LIST_HEIGHT)
            .show(ui, |ui| {
                pgn_panel::show(
                    ui,
                    &mut self.game,
                    &lines,
                    &galleys,
                    follow,
                    figurines_move_list,
                    result.as_deref(),
                );
            });
        self.pgn_cache = Some((cached_revision, lines));
        self.pgn_galleys = Some((galley_revision, galley_figurines, galleys));

        let summary = self.eval_summary();
        let dashboard_width = self
            .settings
            .panels
            .map_or(eval_panel::MIN_WIDTH, |sizes| sizes.right_width);
        // The multi-game PGN list docks inside the right panel, under
        // the two engine cards. The sub-panel is shown first, so it takes the
        // space below the cards; the cards keep the rest. A row click and the
        // Close button are handled after the panel closes.
        let mut list_action = pgn_panel::GameListAction::None;
        let game_list = if self.pgn_browser_list_open {
            self.pgn_collection.as_ref()
        } else {
            None
        };
        let mut setup_action = SetupAction::None;
        let dashboard = egui::Panel::right("evaluation")
            .resizable(true)
            .default_size(dashboard_width)
            .min_size(eval_panel::MIN_WIDTH)
            .show(ui, |ui| {
                if let Some(setup) = self.setup.as_mut() {
                    match self.assets.as_ref() {
                        Some(Ok(assets)) => setup_action = setup_board::show(ui, setup, assets),
                        Some(Err(err)) => {
                            ui.colored_label(theme::DANGER_RED, "Asset load failed:");
                            ui.label(err.to_string());
                        }
                        None => {
                            ui.label("Loading piece and board assets…");
                        }
                    }
                    return;
                }
                if let Some(collection) = game_list {
                    egui::Panel::bottom("pgn_games_list")
                        .resizable(true)
                        .default_size(360.0)
                        .min_size(160.0)
                        .show(ui, |ui| {
                            list_action = pgn_panel::show_game_list_panel(
                                ui,
                                collection,
                                self.pgn_browser_selected,
                            );
                        });
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(action) = eval_panel::show(ui, &summary) {
                            requested = Some(action);
                        }
                    });
            });
        match setup_action {
            SetupAction::Close => self.close_setup_board(),
            SetupAction::Cancel => self.cancel_setup_board(),
            SetupAction::None => {}
        }
        self.sync_setup_fen_field();
        match list_action {
            pgn_panel::GameListAction::Select(index) => self.select_pgn_game(index),
            pgn_panel::GameListAction::Close => self.pgn_browser_list_open = false,
            pgn_panel::GameListAction::None => {}
        }
        self.save_panel_sizes(
            move_list.response.rect.height(),
            dashboard.response.rect.width(),
        );

        if let Some(dialog) = self.options_dialog.as_mut() {
            let slot = dialog.slot();
            match dialog.show(&ctx) {
                OptionsAction::None => {}
                OptionsAction::Close => self.options_dialog = None,
                OptionsAction::Set { name, value } => self.set_engine_option(slot, &name, &value),
                OptionsAction::Reset { name, value } => {
                    self.reset_engine_option(slot, &name, &value)
                }
                OptionsAction::ResetAll => self.reset_all_engine_options(slot),
                OptionsAction::Press { name } => self.press_engine_button(slot, &name),
            }
        }

        if let Some(dialog) = self.time_dialog.as_mut() {
            match dialog.show(&ctx) {
                DialogOutcome::Open => {}
                DialogOutcome::Cancel => self.time_dialog = None,
                DialogOutcome::Apply(control) => {
                    self.time_dialog = None;
                    self.set_time_control(control);
                }
            }
        }

        if let Some(dialog) = self.tablebase_dialog.as_mut() {
            match dialog.show(&ctx) {
                TablebaseOutcome::Open => {}
                TablebaseOutcome::Cancel => self.tablebase_dialog = None,
                TablebaseOutcome::Browse => {
                    self.pick_tablebase_dirs();
                }
                TablebaseOutcome::Apply(value) => {
                    self.tablebase_dialog = None;
                    self.set_tablebase_path(&value);
                }
            }
        }

        let tournament_action = self
            .tournament_dialog
            .as_mut()
            .map(|dialog| dialog.show(&ctx));
        if let Some(action) = tournament_action {
            self.handle_tournament_action(action);
        }

        if self.tournament_window_open {
            let action = self
                .tournament
                .as_ref()
                .map(|session| tournament_dialog::show_progress(&ctx, &session.status));
            match action {
                Some(ProgressAction::Stop) => self.stop_tournament(),
                Some(ProgressAction::Close) => self.tournament_window_open = false,
                Some(ProgressAction::None) => {}
                None => self.tournament_window_open = false,
            }
        }

        if self.tournament_finished_popup {
            let closed = self
                .tournament
                .as_ref()
                .is_some_and(|session| tournament_dialog::show_finished(&ctx, &session.status));
            if closed || self.tournament.is_none() {
                self.tournament_finished_popup = false;
            }
        }

        let mut board_action = BoardAction::None;
        let mut setup_board_action = SetupBoardAction::None;
        egui::CentralPanel::default().show(ui, |ui| {
            // A player bar above and below the board. The top
            // bar belongs to the side at the top, so flipping the board swaps them.
            let area = ui.available_rect_before_wrap();
            ui.allocate_rect(area, egui::Sense::hover());
            let (top_bar, board_rect, bottom_bar) = time_control::split_board_area(area);
            time_control::paint_bar(ui, top_bar, &bars[0]);
            time_control::paint_bar(ui, bottom_bar, &bars[1]);
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(board_rect),
                |ui| match &self.assets {
                    None => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(24.0);
                            ui.label("Loading piece and board assets…");
                        });
                    }
                    Some(Err(err)) => {
                        ui.colored_label(theme::DANGER_RED, "Asset load failed:");
                        ui.label(err.to_string());
                    }
                    Some(Ok(assets)) => {
                        if let Some(setup) = self.setup.as_ref() {
                            setup_board_action =
                                board_view::show_setup(ui, setup.board(), assets, &mut self.view);
                        } else {
                            let arrows = self.eval_arrows();
                            board_action = board_view::show(
                                ui,
                                &self.game,
                                assets,
                                &mut self.view,
                                human_to_move,
                                &arrows,
                            );
                        }
                    }
                },
            );
        });

        if let Some(setup) = self.setup.as_mut() {
            match setup_board_action {
                SetupBoardAction::Place { square } => setup.place(square),
                SetupBoardAction::Remove { square } => setup.remove(square),
                SetupBoardAction::None => {}
            }
        }
        self.sync_setup_fen_field();
        self.apply_board_action(board_action);
        if let Some(action) = requested {
            self.perform(action, &ctx);
        }
        self.record_frame();
    }

    /// Stop every engine before the process exits, so none is left orphaned.
    fn on_exit(&mut self) {
        self.shutdown_engines();
        // The window is gone now, so the size is written here, synchronously.
        if let Some(path) = &self.settings_file {
            if let Err(err) = settings::save_sync(path, &self.settings) {
                log::error!("{}", err);
            }
        }
    }
}

/// `A` for slot 0, `B` for slot 1.
fn slot_letter(slot: usize) -> char {
    char::from(b'A' + slot as u8)
}

/// Combine a result token with a PGN `Termination` reason for the move list.
fn result_label(result: Option<String>, termination: Option<&str>) -> Option<String> {
    match (result, termination) {
        (Some(token), Some(reason)) if !reason.is_empty() => Some(format!("{token} — {reason}")),
        (Some(token), _) => Some(token),
        (None, Some(reason)) if !reason.is_empty() => Some(reason.to_string()),
        (None, _) => None,
    }
}

/// Menu actions that would change the board or engines while a tournament
/// owns them. Cosmetic actions, file copies and the engine options window are
/// allowed.
fn action_conflicts_with_tournament(action: MenuAction) -> bool {
    matches!(
        action,
        MenuAction::NewGame
            | MenuAction::OpenSetupBoard
            | MenuAction::StartFromFen
            | MenuAction::SetMode(_)
            | MenuAction::OpenTimeControl
            | MenuAction::OpenPgn
            | MenuAction::LoadFen
            | MenuAction::LoadEngine(_)
            | MenuAction::ToggleEngine(_)
            | MenuAction::StartAnalysis
            | MenuAction::ToggleBookInGames
            | MenuAction::Resign
            | MenuAction::OfferDraw
            | MenuAction::ClaimThreefold
    )
}

/// Menu actions that would change the game, start/stop an engine, or open
/// another board view while Setup Board is editing. Cosmetic actions, copying
/// and saving files, the settings dialogs and the book remain available.
fn action_conflicts_with_setup(action: MenuAction) -> bool {
    matches!(
        action,
        MenuAction::NewGame
            | MenuAction::OpenSetupBoard
            | MenuAction::StartFromFen
            | MenuAction::SetMode(_)
            | MenuAction::OpenPgn
            | MenuAction::ShowPgnGames
            | MenuAction::LoadEngine(_)
            | MenuAction::ToggleEngine(_)
            | MenuAction::StartAnalysis
            | MenuAction::StopAnalysis
            | MenuAction::ToggleBookInGames
            | MenuAction::Resign
            | MenuAction::OfferDraw
            | MenuAction::ClaimThreefold
            | MenuAction::OpenTournament
            | MenuAction::StopTournament
            | MenuAction::ShowTournament
    )
}

/// The settings map handed to a freshly attached engine: the slot's own saved
/// option values, with the global `SyzygyPath` overlaid — inserted when
/// tablebases are configured and switched on, and removed when they are
/// cleared or switched off, so a stale value saved for an engine file cannot
/// survive either.
fn engine_startup_settings(
    saved: &BTreeMap<String, String>,
    settings: &Settings,
) -> BTreeMap<String, String> {
    let mut map = saved.clone();
    match (
        settings.tablebases_in_engines,
        settings.tablebase_path.as_ref(),
    ) {
        (true, Some(path)) => {
            map.insert(tablebases::SYZYGY_PATH.to_string(), path.clone());
        }
        _ => {
            map.remove(tablebases::SYZYGY_PATH);
        }
    }
    map
}

/// `White` or `Black`, as shown to the user.
fn colour_name(color: PieceColor) -> &'static str {
    match color {
        PieceColor::White => "White",
        PieceColor::Black => "Black",
    }
}

/// A start-up book is only used when it is asked for explicitly:
/// `SILICONBOARD_BOOK` (an existing file), else the book the user loaded
/// last (settings `book_path`) if its file still exists. Without either,
/// there is no start-up book.
fn startup_book_path(settings: &Settings) -> Option<PathBuf> {
    match std::env::var_os("SILICONBOARD_BOOK") {
        Some(raw) => {
            let path = PathBuf::from(raw);
            path.is_file().then_some(path)
        }
        None => settings.book_path.clone().filter(|path| path.is_file()),
    }
}

/// Read a PolyGlot book on a blocking thread. Returns the file path, its
/// name and the result.
async fn read_book(path: PathBuf) -> (PathBuf, String, Result<PolyglotBook, BookError>) {
    let name = file_name_of(&path);
    let loader_path = path.clone();
    let joined = tokio::task::spawn_blocking(move || PolyglotBook::open(&loader_path)).await;
    let result = match joined {
        Ok(result) => result,
        Err(join_error) => Err(BookError::Open {
            path: path.clone(),
            source: std::io::Error::other(join_error),
        }),
    };
    (path, name, result)
}

/// File name for display, falling back to the full path.
fn file_name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Open the tournament PGN for writing, creating its folder. The file is
/// truncated so a new match starts with a clean game list.
fn open_tournament_pgn(path: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("Could not create {}: {}", parent.display(), err))?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|err| format!("Could not open {}: {}", path.display(), err))
}

/// Read and parse a PGN file (runs on the Tokio runtime).
async fn read_pgn(path: &Path) -> Result<(String, PgnCollection), OpenPgnError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| OpenPgnError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let text = String::from_utf8_lossy(&bytes);
    let mut collection = pgn::parse_pgn_collection(&text).map_err(OpenPgnError::Parse)?;
    let name = file_name_of(path);
    collection.name = name.clone();
    Ok((name, collection))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_wakes_are_limited_to_about_thirty_per_second() {
        assert!(
            !wakes_now(false, Duration::ZERO),
            "a second info line in the same frame waits"
        );
        assert!(!wakes_now(
            false,
            ENGINE_WAKE_INTERVAL - Duration::from_millis(1)
        ));
        assert!(
            wakes_now(false, ENGINE_WAKE_INTERVAL),
            "the next slot after 33 ms is allowed"
        );
        assert!(
            wakes_now(true, Duration::ZERO),
            "bestmove, handshake and exit never wait for the slot"
        );
    }

    #[test]
    fn only_a_starting_engine_needs_the_frame_tick() {
        assert!(engine_tick_needed(AnalysisStatus::Starting));
        assert!(
            !engine_tick_needed(AnalysisStatus::Thinking),
            "the monitor task watches a search and wakes the window itself"
        );
        assert!(!engine_tick_needed(AnalysisStatus::Ready));
        assert!(!engine_tick_needed(AnalysisStatus::Offline));
    }

    #[test]
    fn a_tournament_game_gets_its_own_book_seed() {
        let first = book_seed(7, 1);
        assert_eq!(first, book_seed(7, 1), "one game always gets the same seed");
        assert_ne!(first, book_seed(7, 2), "the next game gets a new sequence");
        assert_ne!(first, book_seed(8, 1), "a new match starts afresh");
    }

    #[test]
    fn frame_timer_reports_one_second_of_frames() {
        let start = Instant::now();
        let cost = Duration::from_millis(2);
        let mut timer = FrameTimer::new_at(start);
        // The first active frame starts the window.
        assert!(timer
            .record_at(start + Duration::from_millis(16), cost, true)
            .is_none());
        let mut summary = None;
        for i in 2..=64u32 {
            summary = timer.record_at(start + Duration::from_millis(16 * u64::from(i)), cost, true);
            if i < 64 {
                assert!(summary.is_none(), "no summary before the window closes");
            }
        }
        let summary = summary.expect("one second of frames closes the window");
        assert_eq!(summary.frames, 63);
        assert_eq!(summary.average_cost, cost);
        assert_eq!(summary.worst_cost, cost);
        assert_eq!(summary.average_gap, Duration::from_millis(16));
        assert_eq!(summary.worst_gap, Duration::from_millis(16));
    }

    #[test]
    fn frame_timer_ignores_idle_time() {
        let start = Instant::now();
        let cost = Duration::from_millis(2);
        let mut timer = FrameTimer::new_at(start);
        // Ten seconds with no search: nothing is measured.
        assert!(timer
            .record_at(start + Duration::from_secs(10), cost, false)
            .is_none());
        // The first active frame after the pause only starts the window.
        assert!(timer
            .record_at(start + Duration::from_secs(11), cost, true)
            .is_none());
        let mut summary = None;
        for i in 1..=63u32 {
            summary = timer.record_at(
                start + Duration::from_secs(11) + Duration::from_millis(16 * u64::from(i)),
                cost,
                true,
            );
        }
        let summary = summary.expect("active frames are measured again after an idle gap");
        assert_eq!(summary.frames, 63);
        assert_eq!(summary.average_gap, Duration::from_millis(16));
    }

    /// Without SILICONBOARD_BOOK and without a saved book no book loads:
    /// no workspace book is picked up automatically.
    #[test]
    fn no_book_loads_without_explicit_choice() {
        if std::env::var_os("SILICONBOARD_BOOK").is_some() {
            return; // the environment asks for an explicit start-up book
        }
        assert!(startup_book_path(&Settings::default()).is_none());
    }

    /// The book the user loaded last is restored at start-up while its
    /// file still exists.
    #[test]
    fn the_saved_book_is_restored_at_start_up() {
        let path =
            std::env::temp_dir().join(format!("siliconboard-test-book-{}.bin", std::process::id()));
        std::fs::write(&path, []).unwrap();
        let settings = Settings {
            book_path: Some(path.clone()),
            ..Settings::default()
        };
        let found = startup_book_path(&settings);
        let _ = std::fs::remove_file(&path);
        assert_eq!(found.as_deref(), Some(path.as_path()));
    }

    /// A saved book whose file disappeared means no start-up book.
    #[test]
    fn a_missing_saved_book_is_not_tried() {
        if std::env::var_os("SILICONBOARD_BOOK").is_some() {
            return;
        }
        let settings = Settings {
            book_path: Some("/no/such/book.bin".into()),
            ..Default::default()
        };
        assert!(startup_book_path(&settings).is_none());
    }
}

#[cfg(test)]
mod game_flow_tests {
    use super::*;
    use crate::chess::clock::TimeControl;
    use crate::engine::{AnalysisStatus, EngineHost, EngineProcess, SearchGoal};
    use crate::tournament::{BookOrder, OpeningBookConfig};

    /// An app with Stockfish in both slots and no book. It never reads or
    /// writes the user's settings file.
    fn app_with_two_engines() -> SiliconBoardApp {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        let binary = crate::engine::test_engine_path();
        for slot in 0..2 {
            let process = app.runtime.block_on(EngineProcess::spawn(&binary)).unwrap();
            app.engines[slot] = Some(EngineHost::attach_with_settings(process, BTreeMap::new()));
        }
        app
    }

    /// Run frames until `done` holds, or give up after 30 s.
    fn run_until(
        app: &mut SiliconBoardApp,
        ctx: &egui::Context,
        done: impl Fn(&SiliconBoardApp) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            app.logic_frame(ctx);
            if done(app) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A tournament session in a chosen phase, with a PGN file in `dir`. Used
    /// by the window and stop tests, which need no engines.
    fn tournament_session_for_test(
        dir: &tempfile::TempDir,
        phase: TournamentPhase,
        config: TournamentConfig,
    ) -> TournamentSession {
        let pgn_file = std::fs::File::create(dir.path().join("match.pgn")).unwrap();
        let total_games = config.games;
        TournamentSession {
            adjudicator: Adjudicator::new(&config),
            config,
            phase,
            book: None,
            schedule: Vec::new(),
            game_number: 1,
            white_slot: 0,
            pgn_file,
            status: TournamentStatus {
                running: phase != TournamentPhase::Finished,
                total_games,
                ..TournamentStatus::default()
            },
            next_game_due: None,
            generation: 0,
        }
    }

    #[test]
    fn engine_game_waits_for_start_then_only_the_side_to_move_thinks() {
        let mut app = app_with_two_engines();
        let ctx = egui::Context::default();
        app.set_time_control(Some(TimeControl::fischer(60_000, 0)));
        app.set_mode(GameMode::EngineVsEngine);
        assert!(
            run_until(&mut app, &ctx, |a| a
                .engines
                .iter()
                .flatten()
                .all(|h| h.status() == AnalysisStatus::Ready)),
            "both engines must finish their handshake"
        );

        // Before Start nothing happens: no move, no clock, no search.
        let until = Instant::now() + Duration::from_millis(1_500);
        while Instant::now() < until {
            app.logic_frame(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.game.total_plies(), 0, "no move before Start");
        assert_eq!(
            app.clock
                .as_ref()
                .map(|c| c.remaining_ms(PieceColor::White)),
            Some(60_000)
        );
        assert!(
            app.engines
                .iter()
                .flatten()
                .all(|h| h.status() == AnalysisStatus::Ready),
            "no engine thinks before Start"
        );

        app.set_analysis(true);
        assert!(
            run_until(&mut app, &ctx, |a| a.game.total_plies() >= 4),
            "the game goes on after Start"
        );
        assert!(
            app.clock
                .as_ref()
                .is_some_and(|c| c.remaining_ms(PieceColor::White) < 60_000),
            "the clock runs once the game has started"
        );
    }

    #[test]
    fn starting_from_a_fen_plays_engine_vs_engine_with_a_pgn_start_position() {
        let mut app = app_with_two_engines();
        let ctx = egui::Context::default();
        let fen = "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2";
        app.fen_input = fen.to_string();

        app.perform(MenuAction::StartFromFen, &ctx);

        assert_eq!(app.mode, GameMode::EngineVsEngine, "two engines play");
        assert!(
            app.game_started,
            "the game starts without a separate Start press"
        );
        assert_eq!(app.game.fen(), fen, "the board keeps the FEN position");
        let text = pgn::to_pgn(&app.game, None);
        assert!(text.contains("[SetUp \"1\"]"), "PGN: {text}");
        assert!(
            text.contains("[FEN \"rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\"]"),
            "PGN: {text}"
        );
    }

    #[test]
    fn starting_from_an_invalid_fen_changes_nothing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        app.fen_input = "not a FEN".to_string();

        app.perform(MenuAction::StartFromFen, &egui::Context::default());

        assert_eq!(app.mode, GameMode::Analysis, "the mode is unchanged");
        assert!(!app.game_started);
        assert!(app.status.contains("Invalid FEN"), "status: {}", app.status);
    }

    #[test]
    fn starting_from_a_fen_needs_both_engines_loaded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        app.fen_input = "8/8/8/4k3/8/8/2Q5/2K5 w - - 0 1".to_string();

        app.perform(MenuAction::StartFromFen, &egui::Context::default());

        assert!(!app.game_started);
        assert_eq!(app.mode, GameMode::Analysis, "nothing started");
        assert!(
            app.status.contains("Load engine A and B first"),
            "status: {}",
            app.status
        );
    }

    #[test]
    fn the_engine_cards_offer_the_known_engines() {
        let mut app = app_with_two_engines();
        app.settings.engine_history = vec![PathBuf::from("/opt/other-engine")];
        let summary = app.eval_summary();
        assert!(
            summary
                .engine_choices
                .iter()
                .any(|choice| choice.path == Path::new("/opt/other-engine")),
            "the card pulldown lists the remembered engines"
        );
        assert!(
            summary.cards.iter().all(|card| card.binary_path.is_some()),
            "each card knows the file it runs"
        );

        // Choosing the engine that is already in the slot does not restart it.
        app.perform(
            MenuAction::ChooseEngine { slot: 0, choice: 0 },
            &egui::Context::default(),
        );
        assert!(!app.engine_loading[0], "the same engine is not reloaded");
    }

    #[test]
    fn the_engine_history_is_newest_first_deduplicated_and_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        app.remember_engine(Path::new("/opt/a"));
        app.remember_engine(Path::new("/opt/b"));
        app.remember_engine(Path::new("/opt/a"));
        assert_eq!(
            app.settings.engine_history,
            vec![PathBuf::from("/opt/a"), PathBuf::from("/opt/b")],
            "newest first, no duplicate"
        );

        for i in 0..settings::ENGINE_HISTORY_LIMIT + 5 {
            app.remember_engine(Path::new(&format!("/opt/engine-{i}")));
        }
        assert_eq!(
            app.settings.engine_history.len(),
            settings::ENGINE_HISTORY_LIMIT,
            "the history stays bounded"
        );
    }

    #[test]
    fn the_engine_rests_while_the_human_is_to_move() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        app.mode = GameMode::HumanVsEngine {
            human: PieceColor::White,
            engine: 0,
        };
        app.game_started = true;
        assert!(
            matches!(app.goal_for(0), SearchGoal::Rest),
            "White is the human: engine A waits"
        );
        app.game.apply_uci("e2e4").unwrap();
        assert!(
            matches!(app.goal_for(0), SearchGoal::Move(_)),
            "Black to move: engine A searches"
        );
        app.game_started = false;
        assert!(
            matches!(app.goal_for(0), SearchGoal::Rest),
            "before Start the engine rests"
        );
    }

    #[test]
    fn book_replies_stop_after_the_opening_depth() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        let book = crate::chess::opening_book::sample_start_book();
        app.book = Some(("sample.bin".to_string(), book));
        app.book_in_games = true; // the user switched book play on
        app.mode = GameMode::EngineVsEngine;
        app.game_started = true;
        assert!(
            !app.book_choices().is_empty(),
            "the book answers the first move"
        );

        // Forty plies of knight moves that return to the start position.
        for _ in 0..10 {
            for uci in ["g1f3", "g8f6", "f3g1", "f6g8"] {
                app.game.apply_uci(uci).unwrap();
            }
        }
        assert_eq!(app.game.total_plies(), BOOK_MAX_PLIES);
        assert!(
            app.book_choices().is_empty(),
            "the book stops at the opening depth"
        );
    }

    #[test]
    fn a_play_mode_switches_only_the_engine_that_plays() {
        let mut app = app_with_two_engines();
        app.mode = GameMode::HumanVsEngine {
            human: PieceColor::White,
            engine: 0,
        };
        app.game_started = true;
        let cards = app.eval_summary().cards;
        assert!(cards[0].running, "engine A plays this game");
        assert!(!cards[1].running, "engine B is only loaded, not playing");
        assert!(!cards[1].toggle_enabled, "engine B has no game switch here");

        // Black against engine B: only B follows the game.
        app.mode = GameMode::HumanVsEngine {
            human: PieceColor::Black,
            engine: 1,
        };
        let cards = app.eval_summary().cards;
        assert!(!cards[0].running);
        assert!(cards[1].running);

        // Both engines play each other.
        app.mode = GameMode::EngineVsEngine;
        let cards = app.eval_summary().cards;
        assert!(cards[0].running && cards[1].running);

        // Nothing is running before Start.
        app.game_started = false;
        let cards = app.eval_summary().cards;
        assert!(!cards[0].running && !cards[1].running);
    }

    #[test]
    fn a_play_engine_switches_off_when_its_game_ends() {
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            egui::Context::default(),
        );
        app.mode = GameMode::HumanVsEngine {
            human: PieceColor::White,
            engine: 0,
        };
        let ctx = egui::Context::default();
        app.game_started = true;
        app.analysis_on[0] = true;
        app.game_was_live = true;

        // The human resigns: the game is over, so the engine is switched off.
        app.ending = Some(Ending::Resigned {
            loser: PieceColor::White,
        });
        app.logic_frame(&ctx);
        assert!(
            !app.analysis_on[0],
            "a finished game leaves the engine switched off"
        );
        assert!(!app.game_was_live);

        // A new game also waits for Start, with the engine switched off.
        app.game_started = true;
        app.ending = None;
        app.game_was_live = true;
        app.analysis_on[0] = true;
        app.game = GameState::new();
        app.game_started = false;
        app.logic_frame(&ctx);
        assert!(!app.analysis_on[0], "a new game waits for Start");
    }

    #[test]
    fn the_last_opening_name_stays_when_the_line_leaves_the_table() {
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            egui::Context::default(),
        );
        app.opening_names = Some(OpeningNames::parse(
            "eco\tname\tpgn\nC20\tOpen Game\t1. e4 e5\n",
        ));

        app.game.apply_uci("e2e4").unwrap();
        app.game.apply_uci("e7e5").unwrap();
        app.sync_position(true);
        assert_eq!(app.opening_name.as_deref(), Some("Open Game"));

        // 2.Nf3 is not a named table row; the last name must stay.
        app.game.apply_uci("g1f3").unwrap();
        app.sync_position(true);
        assert_eq!(
            app.opening_name.as_deref(),
            Some("Open Game"),
            "leaving the named rows must not clear the opening name"
        );

        // A new game starts before any named position.
        app.game = GameState::new();
        app.sync_position(true);
        assert_eq!(app.opening_name, None);
    }

    #[test]
    fn a_human_can_claim_threefold_on_their_turn() {
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            egui::Context::default(),
        );
        app.mode = GameMode::HumanVsEngine {
            human: PieceColor::White,
            engine: 0,
        };
        app.game_started = true;
        for _ in 0..2 {
            for uci in ["g1f3", "g8f6", "f3g1", "f6g8"] {
                app.game.apply_uci(uci).unwrap();
            }
        }

        assert!(app.can_claim_threefold());
        assert_eq!(
            app.game.outcome(),
            "Unfinished",
            "the claim is not automatic"
        );
        app.claim_threefold();
        assert_eq!(app.ending, Some(Ending::ThreefoldRepetition));
        assert_eq!(app.result_text().as_deref(), Some("Draw by repetition"));
    }

    /// An engine-vs-engine game with plenty of material and 100 quiet plies:
    /// no player can claim the fifty-move draw, so the game loop must.
    fn fifty_move_fen() -> &'static str {
        "7k/8/8/8/8/8/R7/R6K w - - 100 80"
    }

    #[test]
    fn an_engine_game_ends_on_threefold_repetition() {
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            egui::Context::default(),
        );
        app.mode = GameMode::EngineVsEngine;
        app.game_started = true;
        for _ in 0..2 {
            for uci in ["g1f3", "g8f6", "f3g1", "f6g8"] {
                app.game.apply_uci(uci).unwrap();
            }
        }
        assert!(
            app.game.is_threefold_repetition(),
            "the knight shuffle repeats the start position"
        );

        app.adjudicate_engine_vs_engine();

        assert_eq!(app.ending, Some(Ending::ThreefoldRepetition));
        assert!(
            app.status.contains("threefold repetition"),
            "status: {}",
            app.status
        );
    }

    #[test]
    fn an_engine_game_ends_on_the_fifty_move_rule() {
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            egui::Context::default(),
        );
        app.mode = GameMode::EngineVsEngine;
        app.game_started = true;
        app.game = GameState::from_fen(fifty_move_fen()).unwrap();
        assert!(app.game.is_fifty_move_rule());

        app.adjudicate_engine_vs_engine();

        assert_eq!(app.ending, Some(Ending::FiftyMoveRule));
        assert!(
            app.status.contains("fifty-move rule"),
            "status: {}",
            app.status
        );
    }

    #[test]
    fn a_human_engine_game_is_not_adjudicated() {
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            egui::Context::default(),
        );
        app.mode = GameMode::HumanVsEngine {
            human: PieceColor::White,
            engine: 0,
        };
        app.game_started = true;
        app.game = GameState::from_fen(fifty_move_fen()).unwrap();

        app.adjudicate_engine_vs_engine();

        assert_eq!(app.ending, None, "the human holds the fifty-move claim");
        assert!(
            !app.status.contains("fifty-move"),
            "nothing is adjudicated for a human game: {}",
            app.status
        );
    }

    #[test]
    fn a_gui_tournament_plays_on_the_board_and_raises_the_finished_popup() {
        let engine = crate::engine::test_engine_path();
        let dir = tempfile::tempdir().unwrap();
        let pgn = dir.path().join("match.pgn");
        // A one-move PolyGlot book for White makes the book reply deterministic.
        let book_path = dir.path().join("book.bin");
        let key = GameState::new().zobrist();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&key.to_be_bytes());
        bytes.extend_from_slice(&0x0210u16.to_be_bytes()); // a2-a3
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        std::fs::write(&book_path, &bytes).unwrap();
        let config = TournamentConfig {
            white: EngineSpec::from_path(engine.clone()),
            black: EngineSpec::from_path(engine),
            time_control: TimeControl::fischer(10_000, 100),
            games: 2,
            reverse_colors: true,
            hash_mb: 16,
            nalimov_cache_mb: 0,
            pgn_path: pgn.clone(),
            opening_book: Some(OpeningBookConfig {
                path: book_path,
                order: BookOrder::Random,
            }),
            move_book: None,
            enable_adjudication: true,
            resign_move_count: 2,
            resign_score: 100,
            draw_move_number: 1,
            draw_move_count: 2,
            draw_score: 100,
            event_name: "App test match".to_string(),
        };
        let ctx = egui::Context::default();
        let mut app = app_with_two_engines();
        app.start_tournament(config);
        assert!(app.tournament.is_some(), "the match starts at once");
        assert!(app.tournament_window_open, "the tournament window opens");

        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !app.tournament_finished_popup {
            app.logic_frame(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            app.tournament_finished_popup,
            "the result pop-up must appear when the match ends"
        );
        assert!(
            app.game.total_plies() > 0,
            "the tournament game must be played on the main board"
        );
        let status = &app.tournament.as_ref().unwrap().status;
        assert_eq!(status.completed_games, 2);
        assert!(!status.running);
        assert!(
            !app.tournament_window_open,
            "the tournament window closes on finish"
        );

        let text = std::fs::read_to_string(&pgn).unwrap();
        assert_eq!(
            text.matches("[Event \"App test match\"]").count(),
            2,
            "both scheduled games must be written: {}",
            text
        );
        assert!(text.contains("[Round \"1\"]"), "PGN: {}", text);
        assert!(text.contains("[Round \"2\"]"), "PGN: {}", text);
        assert!(text.contains("[Result \""), "PGN: {}", text);
        assert!(
            !text.contains("[PlyCount \"0\"]"),
            "the engines must actually play moves: {}",
            text
        );
        assert!(
            text.contains("1. a3"),
            "the PolyGlot book move must be in the tournament PGN: {}",
            text
        );

        // After the tournament the board and engines must be usable again.
        app.set_mode(GameMode::Analysis);
        app.set_analysis(true);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut thinking = false;
        while Instant::now() < deadline && !thinking {
            app.logic_frame(&ctx);
            thinking = app
                .engines
                .iter()
                .flatten()
                .any(|host| host.status() == AnalysisStatus::Thinking);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            thinking,
            "an engine must start analysing after the tournament ends"
        );
    }

    #[test]
    fn an_epd_tournament_records_the_start_position_in_the_pgn() {
        let engine = crate::engine::test_engine_path();
        let dir = tempfile::tempdir().unwrap();
        let pgn = dir.path().join("epd-match.pgn");
        let epd = dir.path().join("openings.epd");
        std::fs::write(
            &epd,
            "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n",
        )
        .unwrap();
        // A one-move PolyGlot book for the EPD position: 2.h3.
        let epd_game =
            GameState::from_fen("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2")
                .unwrap();
        let key = epd_game.zobrist();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&key.to_be_bytes());
        bytes.extend_from_slice(&0x03d7u16.to_be_bytes()); // h2-h3
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let move_book = dir.path().join("moves.bin");
        std::fs::write(&move_book, &bytes).unwrap();
        let config = TournamentConfig {
            white: EngineSpec::from_path(engine.clone()),
            black: EngineSpec::from_path(engine),
            time_control: TimeControl::fischer(10_000, 100),
            games: 1,
            reverse_colors: false,
            hash_mb: 16,
            nalimov_cache_mb: 0,
            pgn_path: pgn.clone(),
            opening_book: Some(OpeningBookConfig {
                path: epd,
                order: BookOrder::Sequential,
            }),
            move_book: Some(move_book),
            enable_adjudication: true,
            resign_move_count: 2,
            resign_score: 100,
            draw_move_number: 1,
            draw_move_count: 2,
            draw_score: 100,
            event_name: "EPD test match".to_string(),
        };
        let ctx = egui::Context::default();
        let mut app = app_with_two_engines();
        app.start_tournament(config);

        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !app.tournament_finished_popup {
            app.logic_frame(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.tournament_finished_popup, "the EPD match must finish");

        let text = std::fs::read_to_string(&pgn).unwrap();
        assert!(text.contains("[SetUp \"1\"]"), "PGN: {}", text);
        assert!(text.contains("[FEN \""), "PGN: {}", text);
        assert!(
            text.contains("4p3/4P3"),
            "the EPD start position must be in the PGN: {}",
            text
        );
        assert!(
            text.contains("2. h3"),
            "the PolyGlot move book must be in the PGN: {}",
            text
        );
        assert!(
            !text.contains("[PlyCount \"0\"]"),
            "the engines must play moves from the EPD position: {}",
            text
        );
    }

    #[test]
    fn an_epd_start_with_a_move_book_replays_the_line_in_the_pgn() {
        let engine = crate::engine::test_engine_path();
        let dir = tempfile::tempdir().unwrap();
        let pgn = dir.path().join("epd-path.pgn");
        let epd = dir.path().join("openings.epd");
        std::fs::write(
            &epd,
            "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n",
        )
        .unwrap();
        // The move book contains the line to the EPD position and one book
        // move from there: 1.e4 e5 2.h3.
        let start_key = GameState::new().zobrist();
        let mut after_e4 = GameState::new();
        after_e4.apply_uci("e2e4").unwrap();
        let after_e4_key = after_e4.zobrist();
        let after_e4_e5 =
            GameState::from_fen("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2")
                .unwrap();
        let after_e4_e5_key = after_e4_e5.zobrist();
        let mut bytes = Vec::new();
        for (key, encoded) in [
            (start_key, 0x031cu16),       // e2-e4
            (after_e4_key, 0x0d24u16),    // e7-e5
            (after_e4_e5_key, 0x03d7u16), // h2-h3
        ] {
            bytes.extend_from_slice(&key.to_be_bytes());
            bytes.extend_from_slice(&encoded.to_be_bytes());
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
        }
        let move_book = dir.path().join("moves.bin");
        std::fs::write(&move_book, &bytes).unwrap();
        let config = TournamentConfig {
            white: EngineSpec::from_path(engine.clone()),
            black: EngineSpec::from_path(engine),
            time_control: TimeControl::fischer(10_000, 100),
            games: 1,
            reverse_colors: false,
            hash_mb: 16,
            nalimov_cache_mb: 0,
            pgn_path: pgn.clone(),
            opening_book: Some(OpeningBookConfig {
                path: epd,
                order: BookOrder::Sequential,
            }),
            move_book: Some(move_book),
            enable_adjudication: true,
            resign_move_count: 2,
            resign_score: 100,
            draw_move_number: 1,
            draw_move_count: 2,
            draw_score: 100,
            event_name: "EPD path test".to_string(),
        };
        let ctx = egui::Context::default();
        let mut app = app_with_two_engines();
        app.start_tournament(config);

        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !app.tournament_finished_popup {
            app.logic_frame(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            app.tournament_finished_popup,
            "the EPD path match must finish"
        );

        let text = std::fs::read_to_string(&pgn).unwrap();
        assert!(
            text.contains("1. e4 e5 2. h3"),
            "the book line to the EPD position must be in the PGN: {}",
            text
        );
        assert!(
            !text.contains("[SetUp \"1\"]"),
            "a reconstructed line starts from the standard position: {}",
            text
        );
    }

    #[test]
    fn a_multi_game_pgn_fills_the_browser_and_selects_games() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let text = "[Event \"One\"]\n[White \"A\"]\n[Black \"B\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n[Event \"Two\"]\n[White \"C\"]\n[Black \"D\"]\n[Result \"0-1\"]\n\n1. d4 d5 0-1\n";
        let collection = pgn::parse_pgn_collection(text).unwrap();
        app.events_tx.send(AppEvent::PgnOpened {
            request: 0,
            outcome: Some(Ok(("multi.pgn".to_string(), collection))),
        });
        app.logic_frame(&ctx);
        assert_eq!(app.pgn_collection.as_ref().map(|c| c.games.len()), Some(2));
        assert_eq!(app.pgn_browser_selected, 0);
        assert!(app.pgn_browser_list_open, "the PGN games list opens");
        assert_eq!(app.game.total_plies(), 2, "the first game is loaded");
        assert_eq!(
            app.game.history()[0].san.to_string(),
            "e4",
            "the first game's move is on the board"
        );

        app.select_pgn_game(1);
        assert_eq!(app.pgn_browser_selected, 1);
        assert_eq!(app.game.total_plies(), 2);
        assert_eq!(app.game.history()[0].san.to_string(), "d4");
        assert_eq!(app.result_text().as_deref(), Some("Black won (from PGN)"));

        // Analysis only is the browsing mode: the PGN browser stays open.
        app.set_mode(GameMode::Analysis);
        assert!(
            app.pgn_collection.is_some(),
            "choosing Analysis only keeps the PGN browser open"
        );
        assert!(app.pgn_browser_list_open);

        // Any play mode is a game of its own and closes the browser.
        app.set_mode(GameMode::EngineVsEngine);
        assert!(
            app.pgn_collection.is_none(),
            "choosing a play mode closes the PGN browser"
        );
        assert!(!app.pgn_browser_list_open);

        // The list can be shown again after a mode change, and a new game
        // closes it as well.
        app.pgn_collection = Some(pgn::parse_pgn_collection(text).unwrap());
        app.pgn_browser_list_open = true;
        app.perform(MenuAction::NewGame, &egui::Context::default());
        assert!(
            app.pgn_collection.is_none(),
            "a new game closes the PGN browser"
        );
        assert!(!app.pgn_browser_list_open);
    }

    #[test]
    fn an_older_pgn_result_does_not_replace_a_newer_one() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        // The user has already opened a newer file; the first file's slow read
        // arrives last and must be ignored.
        app.pgn_request = 2;
        let text = "[Event \"Old\"]\n\n1. e4 e5 1-0\n\n[Event \"Older\"]\n\n1. d4 d5 0-1\n";
        let collection = pgn::parse_pgn_collection(text).unwrap();
        app.events_tx.send(AppEvent::PgnOpened {
            request: 1,
            outcome: Some(Ok(("stale.pgn".to_string(), collection))),
        });
        app.logic_frame(&ctx);
        assert!(
            app.pgn_collection.is_none(),
            "a result from an older Open request is discarded"
        );
    }

    #[test]
    fn opening_a_pgn_with_a_result_shows_the_result() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let collection = pgn::parse_pgn_collection(
            "[Result \"1-0\"]\n[Termination \"White wins on time\"]\n\n1. e4 e5 2. Nf3 Nc6 1-0\n",
        )
        .unwrap();
        app.events_tx.send(AppEvent::PgnOpened {
            request: 0,
            outcome: Some(Ok(("result.pgn".to_string(), collection))),
        });
        app.logic_frame(&ctx);
        assert!(
            app.pgn_collection.is_none(),
            "a single-game PGN keeps the normal move list"
        );
        assert_eq!(
            app.ending,
            Some(Ending::FromPgn {
                winner: Some(PieceColor::White)
            })
        );
        assert_eq!(app.pgn_termination.as_deref(), Some("White wins on time"));
        assert_eq!(app.result_text().as_deref(), Some("White won (from PGN)"));
        assert_eq!(
            result_label(Some("1-0".to_string()), app.pgn_termination.as_deref()).as_deref(),
            Some("1-0 — White wins on time")
        );
        assert!(
            app.status.contains("White won (from PGN)"),
            "the status line should show the result: {}",
            app.status
        );
    }

    #[test]
    fn the_move_list_result_can_carry_a_termination_reason() {
        assert_eq!(
            result_label(Some("0-1".to_string()), Some("Black wins on time")).as_deref(),
            Some("0-1 — Black wins on time")
        );
        assert_eq!(
            result_label(Some("1/2-1/2".to_string()), Some("Draw by repetition")).as_deref(),
            Some("1/2-1/2 — Draw by repetition")
        );
        assert_eq!(
            result_label(Some("1-0".to_string()), None).as_deref(),
            Some("1-0")
        );
        assert_eq!(result_label(None, None), None);
    }

    #[test]
    fn a_between_games_pause_requests_a_repaint() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let dir = tempfile::tempdir().unwrap();
        let pgn_file = std::fs::File::create(dir.path().join("pause.pgn")).unwrap();
        let config = TournamentConfig::default();
        app.tournament = Some(TournamentSession {
            adjudicator: Adjudicator::new(&config),
            config,
            phase: TournamentPhase::BetweenGames,
            book: None,
            schedule: Vec::new(),
            game_number: 2,
            white_slot: 0,
            pgn_file,
            status: TournamentStatus {
                running: true,
                ..TournamentStatus::default()
            },
            next_game_due: Some(Instant::now() + Duration::from_secs(1)),
            generation: 0,
        });
        app.tournament_tick(&ctx);
        assert!(
            ctx.has_requested_repaint(),
            "the pause before the next game must schedule a frame"
        );
    }

    /// Owner report: after the user closes the tournament window, the red
    /// Stop button in the top bar refused with "Stop the tournament before…",
    /// so the match could not be stopped any more. It must stop the whole
    /// tournament instead of being blocked.
    #[test]
    fn the_top_bar_stop_stops_a_running_tournament_after_its_window_was_closed() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let dir = tempfile::tempdir().unwrap();
        app.tournament = Some(tournament_session_for_test(
            &dir,
            TournamentPhase::BetweenGames,
            TournamentConfig::default(),
        ));
        app.tournament_window_open = false; // the user closed the live window

        app.perform(MenuAction::StopAnalysis, &ctx);

        let session = app.tournament.as_ref().unwrap();
        assert_eq!(session.phase, TournamentPhase::Finished);
        assert!(!session.status.running);
        assert!(session.status.cancelled, "the match counts as stopped");
        assert!(
            app.tournament_finished_popup,
            "the summary opens when the user stops the match"
        );
        assert!(
            !app.status.contains("Stop the tournament before"),
            "the red Stop must not be refused: {}",
            app.status
        );
    }

    /// A tournament that cannot start must name the failing slot and the
    /// reason instead of only the generic sentence.
    #[test]
    fn a_tournament_start_failure_names_the_engine_and_reason() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            ctx,
        );
        app.engine_spawn_error[1] = Some("permission denied".to_string());
        let message = app.tournament_engine_failure();
        assert!(
            message.starts_with("A tournament engine could not be started:"),
            "{message}"
        );
        assert!(
            message.contains("engine B could not be started: permission denied"),
            "{message}"
        );
    }

    /// The user may close the progress window while the match runs. The
    /// end-of-match summary must still appear.
    #[test]
    fn a_closed_tournament_window_still_shows_the_summary_at_the_end() {
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let dir = tempfile::tempdir().unwrap();
        let config = TournamentConfig {
            games: 1,
            ..TournamentConfig::default()
        };
        app.tournament = Some(tournament_session_for_test(
            &dir,
            TournamentPhase::Playing,
            config,
        ));
        // The user closed the live window while the match still ran.
        app.tournament_window_open = false;
        // Fool's mate: the game is over, so the last scheduled game completes.
        app.game =
            GameState::from_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3")
                .unwrap();

        app.finish_tournament_game();

        let session = app.tournament.as_ref().unwrap();
        assert_eq!(session.phase, TournamentPhase::Finished);
        assert_eq!(session.status.completed_games, 1);
        assert!(
            app.tournament_finished_popup,
            "the summary must open even though the live window was closed"
        );
        assert!(!app.tournament_window_open);
    }

    #[test]
    fn an_engine_that_exits_after_handshake_is_restarted_once() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("restarted");
        let script = dir.path().join("exits-on-go.sh");
        let body = format!(
            "#!/bin/sh\nmarker=\"{}\"\nif [ ! -f \"$marker\" ]; then\n  : > \"$marker\"\n  flaky=1\nelse\n  flaky=0\nfi\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Flaky'; echo 'option name Hash type spin default 16 min 1 max 1024'; echo uciok;;\n    isready) echo readyok;;\n    go*) if [ \"$flaky\" = 1 ]; then exit 3; fi; echo bestmove e2e4;;\n    quit) exit 0;;\n  esac\ndone\n",
            marker.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let process = app.runtime.block_on(EngineProcess::spawn(&script)).unwrap();
        app.engines[0] = Some(EngineHost::attach_with_settings(process, BTreeMap::new()));
        app.engine_auto_restart[0] = true;
        app.engine_started[0] = Some(Instant::now());

        assert!(
            run_until(&mut app, &ctx, |a| a.engines[0]
                .as_ref()
                .is_some_and(|h| h.status() == AnalysisStatus::Ready)),
            "the flaky engine must finish its handshake"
        );

        // The first process exits as soon as it is asked to search.
        app.engines[0]
            .as_mut()
            .unwrap()
            .set_analysis(true, &app.game.fen(), SearchGoal::Analyse);
        assert!(
            run_until(&mut app, &ctx, |a| !a.restart_times[0].is_empty()),
            "an exited engine must trigger a restart"
        );
        assert!(
            run_until(&mut app, &ctx, |a| a.engines[0]
                .as_ref()
                .is_some_and(|h| h.status() == AnalysisStatus::Ready)),
            "the replacement engine must complete its handshake"
        );
        assert!(app.engine_auto_restart[0], "one restart is allowed");
    }

    #[test]
    fn a_slot_may_restart_three_times_in_ten_minutes_and_no_more() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        for _ in 0..RESTART_LIMIT {
            assert!(app.may_restart(0));
            app.note_restart(0);
        }
        assert!(
            !app.may_restart(0),
            "the fourth restart within the window is refused"
        );
        assert!(app.may_restart(1), "the other slot is not affected");
        app.restart_times[0] =
            vec![Instant::now() - RESTART_WINDOW - Duration::from_secs(1); RESTART_LIMIT];
        assert!(
            app.may_restart(0),
            "restarts older than the window no longer count"
        );
    }

    #[test]
    fn options_that_waste_the_machine_are_warned_about() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        assert!(app.option_warning(0, "MultiPV", "1").is_none());
        assert!(app
            .option_warning(0, "MultiPV", "3")
            .unwrap()
            .contains("MultiPV 3"));
        assert!(app.option_warning(0, "Hash", "4096").is_none());

        let cores = std::thread::available_parallelism().map_or(usize::MAX, usize::from);
        assert!(
            app.option_warning(0, "Threads", "1").is_none(),
            "one thread each fits"
        );
        if cores < usize::MAX {
            let too_many = cores.to_string();
            assert!(
                app.option_warning(0, "Threads", &too_many).is_some(),
                "{cores} + 1 threads exceed {cores} cores"
            );
        }
        app.engine_settings[1].insert("Threads".to_string(), "4".to_string());
        if cores < 5 {
            assert!(
                app.option_warning(0, "Threads", "2").is_some(),
                "the other engine's threads count too"
            );
        }
    }

    #[test]
    fn a_loaded_pgn_shows_its_players_on_the_bars_until_a_new_game() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        let import = pgn::parse_pgn_with_report(
            "[White \"Karpov, Anatoly\"]\n[Black \"Kasparov, Garry\"]\n\n1. e4 e5 *\n",
        )
        .unwrap();
        app.load_pgn_game("game.pgn", import, None);
        let names: Vec<String> = app
            .player_bars()
            .iter()
            .map(|bar| bar.name.clone())
            .collect();
        assert_eq!(
            names,
            vec!["Kasparov, Garry".to_string(), "Karpov, Anatoly".to_string()],
            "top first: Black, then White"
        );
        app.perform(MenuAction::NewGame, &egui::Context::default());
        let names: Vec<String> = app
            .player_bars()
            .iter()
            .map(|bar| bar.name.clone())
            .collect();
        assert_eq!(
            names,
            vec!["Black".to_string(), "White".to_string()],
            "a new game drops the PGN names"
        );
    }

    #[test]
    fn applying_tablebases_keeps_the_path_and_removes_stale_per_engine_values() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        // An engine file with its own (stale) saved SyzygyPath.
        app.settings
            .engine_options
            .entry("/opt/sf/stockfish".into())
            .or_default()
            .insert(tablebases::SYZYGY_PATH.into(), "/stale".into());
        app.engine_settings[0].insert(tablebases::SYZYGY_PATH.into(), "/stale".into());

        app.set_tablebase_path(" /tb/5:/tb/6 ");
        assert_eq!(
            app.settings.tablebase_path.as_deref(),
            Some("/tb/5:/tb/6"),
            "the trimmed string is kept as one global setting"
        );
        assert!(
            app.settings.tablebases_in_engines,
            "applying a path means using it, so the switch turns on"
        );
        assert_eq!(
            app.engine_settings[0]
                .get(tablebases::SYZYGY_PATH)
                .map(String::as_str),
            Some("/tb/5:/tb/6")
        );
        assert!(
            !app.settings.engine_options["/opt/sf/stockfish"].contains_key(tablebases::SYZYGY_PATH),
            "a stale per-engine value must not override the global one"
        );

        app.set_tablebase_path("   ");
        assert_eq!(
            app.settings.tablebase_path, None,
            "an empty string clears the tablebases"
        );
        assert!(
            !app.engine_settings[0].contains_key(tablebases::SYZYGY_PATH),
            "a cold restart of an engine gets no path again"
        );
    }

    #[test]
    fn the_tablebases_switch_keeps_the_path_but_turns_it_on_and_off() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, egui::Context::default());
        app.set_tablebase_path("/tb/5:/tb/6");
        assert!(app.settings.tablebases_in_engines);

        app.toggle_tablebases();
        assert!(
            !app.settings.tablebases_in_engines,
            "the switch turns the tablebases off"
        );
        assert_eq!(
            app.settings.tablebase_path.as_deref(),
            Some("/tb/5:/tb/6"),
            "switching off keeps the configured path"
        );

        app.toggle_tablebases();
        assert!(
            app.settings.tablebases_in_engines,
            "the switch turns them on again"
        );

        // Applying a new path after switching off means using it again.
        app.toggle_tablebases();
        assert!(!app.settings.tablebases_in_engines);
        app.set_tablebase_path("/tb/new");
        assert!(
            app.settings.tablebases_in_engines,
            "applying a path switches the tablebases on"
        );
    }

    #[test]
    fn switching_the_tablebases_off_sends_the_empty_sentinel() {
        use std::os::unix::fs::PermissionsExt;

        // A fake engine that records every command it receives. PawnForge
        // rejects `setoption name SyzygyPath value ` (nothing after `value`)
        // and keeps the loaded path; the app must send `<empty>` instead.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("commands.log");
        let script = dir.path().join("logger.sh");
        let body = format!(
            "#!/bin/sh\nlog='{}'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> \"$log\"\n  case \"$line\" in\n    uci) echo 'id name Logger'; echo uciok;;\n    isready) echo readyok;;\n    go*) echo 'bestmove e2e4';;\n    quit) exit 0;;\n  esac\ndone\n",
            log.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            ctx.clone(),
        );
        let process = app.runtime.block_on(EngineProcess::spawn(&script)).unwrap();
        app.engines[0] = Some(EngineHost::attach_with_settings(process, BTreeMap::new()));
        assert!(run_until(&mut app, &ctx, |a| a.engines[0]
            .as_ref()
            .is_some_and(|host| host.status() == AnalysisStatus::Ready)));

        app.settings.tablebase_path = Some("/tb/5".into());
        app.settings.tablebases_in_engines = true;
        app.toggle_tablebases();
        assert!(!app.settings.tablebases_in_engines);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text
                .lines()
                .any(|line| line == "setoption name SyzygyPath value <empty>")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the engine never received the clear sentinel; commands:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let process = app.engines[0].as_mut().unwrap().shutdown();
        app.runtime.block_on(process.terminate()).unwrap();
    }

    #[test]
    fn the_tablebase_dialog_opens_from_the_menu_and_applies_the_draft() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        app.perform(MenuAction::OpenTablebases, &ctx);
        assert!(
            app.tablebase_dialog.is_some(),
            "the menu item opens the dialog"
        );
        assert_eq!(
            app.tablebase_dialog.as_ref().unwrap().draft(),
            "",
            "no tablebases yet"
        );

        app.tablebase_dialog = Some(TablebaseDialog::new(Some("/tb/5:/tb/6")));
        assert_eq!(
            app.tablebase_dialog.as_ref().unwrap().draft(),
            "/tb/5:/tb/6",
            "the dialog starts from the current setting"
        );
        app.tablebase_dialog
            .as_mut()
            .unwrap()
            .set_draft_dirs(["/tb/5", "/tb/6"]);
        assert_eq!(
            app.tablebase_dialog.as_ref().unwrap().draft(),
            tablebases::join_paths(["/tb/5", "/tb/6"])
        );
    }

    #[test]
    fn the_tablebase_path_overlays_the_engine_settings_at_attach() {
        let mut saved = BTreeMap::new();
        saved.insert("Hash".to_string(), "256".to_string());
        saved.insert(tablebases::SYZYGY_PATH.to_string(), "/stale".to_string());

        let configured = Settings {
            tablebase_path: Some("/tb/5:/tb/6".to_string()),
            ..Settings::default()
        };
        let with = engine_startup_settings(&saved, &configured);
        assert_eq!(
            with.get(tablebases::SYZYGY_PATH).map(String::as_str),
            Some("/tb/5:/tb/6")
        );
        assert_eq!(
            with.get("Hash").map(String::as_str),
            Some("256"),
            "the engine's own values survive"
        );

        let switched_off = Settings {
            tablebases_in_engines: false,
            ..configured.clone()
        };
        let off = engine_startup_settings(&saved, &switched_off);
        assert!(
            !off.contains_key(tablebases::SYZYGY_PATH),
            "a switched-off tablebase path is not handed to a fresh engine"
        );
        assert_eq!(off.get("Hash").map(String::as_str), Some("256"));

        let cleared = Settings {
            tablebase_path: None,
            ..Settings::default()
        };
        let without = engine_startup_settings(&saved, &cleared);
        assert!(
            !without.contains_key(tablebases::SYZYGY_PATH),
            "a cleared tablebase path is not handed to a fresh engine"
        );
        assert_eq!(without.get("Hash").map(String::as_str), Some("256"));
    }

    #[test]
    fn setup_board_opens_on_the_displayed_position_and_close_replaces_the_game() {
        use shakmaty::{Piece, Role, Square};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        app.game.apply_uci("e2e4").unwrap();
        let displayed = app.game.fen();

        app.perform(MenuAction::OpenSetupBoard, &ctx);
        let setup = app.setup.as_ref().expect("the menu item opens Setup Board");
        assert_eq!(
            setup.fen(),
            displayed,
            "editing starts from the displayed position"
        );
        assert!(
            setup.errors().is_empty(),
            "the displayed position is valid: {:?}",
            setup.errors()
        );

        // Clear the board and build K vs K with Black to move.
        let setup = app.setup.as_mut().unwrap();
        setup.clear();
        setup.set_selected(Some(Piece {
            color: shakmaty::Color::White,
            role: Role::King,
        }));
        setup.place(Square::E1);
        setup.set_selected(Some(Piece {
            color: shakmaty::Color::Black,
            role: Role::King,
        }));
        setup.place(Square::E8);
        setup.set_turn(shakmaty::Color::Black);

        app.close_setup_board();
        assert!(app.setup.is_none(), "close leaves the setup view");
        assert_eq!(app.game.fen(), "4k3/8/8/8/8/8/8/4K3 b - - 0 1");
        assert_eq!(
            app.game.total_plies(),
            0,
            "the edited position starts a new history"
        );
        assert!(!app.game_started, "the user picks a mode and presses Start");
    }

    #[test]
    fn the_top_fen_field_loads_into_an_open_setup_board() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        let game_before = app.game.fen();

        app.perform(MenuAction::OpenSetupBoard, &ctx);
        app.fen_input = "4k3/8/8/8/8/8/8/4K3 b - - 0 1".to_string();
        app.perform(MenuAction::LoadFen, &ctx);

        assert!(app.setup.is_some(), "the editor stays open");
        assert_eq!(
            app.setup.as_ref().unwrap().fen(),
            "4k3/8/8/8/8/8/8/4K3 b - - 0 1",
            "the typed FEN replaces the editor's position"
        );
        assert_eq!(
            app.game.fen(),
            game_before,
            "the game itself is untouched until the editor closes"
        );

        app.close_setup_board();
        assert_eq!(app.game.fen(), "4k3/8/8/8/8/8/8/4K3 b - - 0 1");
    }

    #[test]
    fn setup_board_refuses_to_close_on_an_invalid_position() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        let before = app.game.fen();

        app.perform(MenuAction::OpenSetupBoard, &ctx);
        app.setup.as_mut().unwrap().clear();
        app.close_setup_board();

        assert!(
            app.setup.is_some(),
            "the editor stays open while the position is invalid"
        );
        assert_eq!(app.game.fen(), before, "the game is unchanged");
        assert_eq!(app.status_level, StatusLevel::Error);
        assert!(
            app.status.contains("no king"),
            "the status names the reason: {}",
            app.status
        );
    }

    #[test]
    fn setup_board_blocks_a_menu_action_that_would_replace_the_game() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = egui::Context::default();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        app.perform(MenuAction::OpenSetupBoard, &ctx);
        app.perform(MenuAction::NewGame, &ctx);

        assert!(
            app.setup.is_some(),
            "Setup Board keeps the board while it is open"
        );
        assert_eq!(app.status, "Finish or cancel Setup Board first.");
        app.cancel_setup_board();
        assert!(app.setup.is_none(), "cancel leaves the editor");
    }

    /// A scripted UCI engine that keeps searching (a background loop prints
    /// `info` lines while the flag file exists), answers `isready` and `stop`
    /// like a real engine, and reports `pv`.
    fn write_script_engine(dir: &std::path::Path, slot: usize, pv: &str) -> std::path::PathBuf {
        let script = dir.join(format!("engine{slot}.sh"));
        let flag = dir.join(format!("search{slot}.flag"));
        let body = format!(
            "#!/bin/sh\nFLAG='{flag_path}'\n( while :; do\n  if [ -e \"$FLAG\" ]; then\n    echo \"info depth 20 score cp 10 pv {pv}\"\n  fi\n  sleep 0.05\ndone ) &\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Arrow{slot}'; echo uciok;;\n    isready) echo readyok;;\n    position*) ;;\n    go*) touch \"$FLAG\";;\n    stop) rm -f \"$FLAG\"; echo 'bestmove 0000';;\n    quit) exit 0;;\n  esac\ndone\n",
            flag_path = flag.display(),
            slot = slot,
            pv = pv,
        );
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Owner report: with two engines analysing the displayed position and
    /// picking different moves from the same from-square (Kc4 vs Kb4), only
    /// one arrow appeared. Both arrows must be on the board.
    #[test]
    fn two_engines_picking_different_moves_show_two_arrows() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        for (slot, pv) in ["b5c4", "b5b4"].iter().enumerate() {
            let script = write_script_engine(dir.path(), slot, pv);
            let process = app.runtime.block_on(EngineProcess::spawn(&script)).unwrap();
            app.engines[slot] = Some(EngineHost::attach_with_settings(process, BTreeMap::new()));
        }
        app.game = GameState::from_fen("8/1p6/1P1K4/pk6/8/8/5B2/8 b - - 3 56").unwrap();
        app.perform(MenuAction::StartAnalysis, &ctx);
        assert!(
            run_until(&mut app, &ctx, |a| a
                .engines
                .iter()
                .flatten()
                .all(|h| h.status() == AnalysisStatus::Thinking)),
            "both engines analyse the FEN position"
        );
        assert!(
            run_until(&mut app, &ctx, |a| a.eval_arrows().len() == 2),
            "both arrows are on the board: {:?}",
            app.eval_arrows()
        );
        app.perform(MenuAction::StopAnalysis, &ctx);
    }

    /// Same report, but with browsing: the engines start on one position and
    /// the user then moves to the reported FEN while both are searching, so
    /// both run a stop -> bestmove -> restart cycle. After it, both arrows
    /// must be back and stay.
    #[test]
    fn both_arrows_survive_browsing_to_the_reported_position() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        for (slot, pv) in ["a1a2", "a1a3"].iter().enumerate() {
            let script = write_script_engine(dir.path(), slot, pv);
            let process = app.runtime.block_on(EngineProcess::spawn(&script)).unwrap();
            app.engines[slot] = Some(EngineHost::attach_with_settings(process, BTreeMap::new()));
        }
        app.perform(MenuAction::StartAnalysis, &ctx);
        assert!(
            run_until(&mut app, &ctx, |a| a.eval_arrows().len() == 2),
            "both arrows are on the board on the first position"
        );
        // The user browses to the reported position.
        app.game = GameState::from_fen("8/1p6/1P1K4/pk6/8/8/5B2/8 b - - 3 56").unwrap();
        // The restart transient (searched_fen still the old position) may
        // briefly show nothing; once both engines search the new position,
        // both arrows must be back and stay.
        assert!(
            run_until(&mut app, &ctx, |a| a.eval_arrows().len() == 2),
            "both arrows return after browsing: {:?} - statuses {:?}, fens {:?}, stalls {:?}",
            app.eval_arrows(),
            app.engines
                .iter()
                .map(|h| h.as_ref().map(|h| h.status()))
                .collect::<Vec<_>>(),
            app.engines
                .iter()
                .map(|h| h.as_ref().map(|h| h.view().searched_fen))
                .collect::<Vec<_>>(),
            app.engines
                .iter()
                .map(|h| h.as_ref().and_then(|h| h.stall_reason()))
                .collect::<Vec<_>>(),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            app.logic_frame(&ctx);
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(app.eval_arrows().len(), 2, "both arrows stay on the board");
        }
        app.perform(MenuAction::StopAnalysis, &ctx);
    }

    /// An engine that misses a watchdog deadline (here: it never answers
    /// `stop` when the user browses on) is killed by the monitor task and the
    /// host flips to Offline in the same frame. The stall restart must still
    /// happen, so the arrow - like the card - comes back. Before the fix the
    /// slot stayed Offline forever while its card kept showing the last
    /// evaluation, which looked like a missing arrow.
    #[test]
    fn an_engine_killed_by_the_watchdog_returns_and_draws_its_arrow_again() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut app = SiliconBoardApp::unloaded(runtime, ctx.clone());
        // Slot 0 misbehaves once the user browses: it prints `info` lines in a
        // loop and never reads the `stop` that the position change sends.
        for (slot, pv, stuck) in [("0", "b5c4", true), ("1", "b5b4", false)].iter() {
            let body = format!(
                "#!/bin/sh\nn=1\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Arrow{slot}'; echo uciok;;\n    isready) echo readyok;;\n    position*) ;;\n    go*)\n      while :; do\n        echo \"info depth $n score cp 10 pv {pv}\"\n        n=$((n+1))\n        sleep 0.05\n      done;;\n    stop) {stop_reply};;\n    quit) exit 0;;\n  esac\ndone\n",
                slot = slot,
                pv = pv,
                stop_reply = if *stuck {
                    " ;; # stuck: no bestmove"
                } else {
                    "echo 'bestmove 0000'"
                },
            );
            let script = dir.path().join(format!("engine{slot}.sh"));
            std::fs::write(&script, body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let process = app.runtime.block_on(EngineProcess::spawn(&script)).unwrap();
            app.engines[slot.parse::<usize>().unwrap()] =
                Some(EngineHost::attach_with_settings(process, BTreeMap::new()));
        }
        app.perform(MenuAction::StartAnalysis, &ctx);
        assert!(
            run_until(&mut app, &ctx, |a| a.eval_arrows().len() == 2),
            "both arrows are on the board before browsing"
        );
        // The user browses on: engine A misses the stop deadline, the monitor
        // kills it, and the app must restart it and show its arrow again.
        app.game = GameState::from_fen("8/1p6/1P1K4/pk6/8/8/5B2/8 b - - 3 56").unwrap();
        assert!(
            run_until(&mut app, &ctx, |a| a.eval_arrows().len() == 2),
            "both arrows return after the stall restart: {:?} - statuses {:?}",
            app.eval_arrows(),
            app.engines
                .iter()
                .map(|h| h.as_ref().map(|h| h.status()))
                .collect::<Vec<_>>(),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            app.logic_frame(&ctx);
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(app.eval_arrows().len(), 2, "both arrows stay on the board");
        }
        app.perform(MenuAction::StopAnalysis, &ctx);
    }
}

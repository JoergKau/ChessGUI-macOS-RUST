//! The database windows: new database, overview and the game browser.
//!
//! All three are ordinary movable egui windows drawn over the board, in the
//! same style as the tournament windows. They are kept in one state struct so
//! the app only has to hold a [`DatabaseUi`] and act on the [`DbAction`] it
//! returns; every job (import, query, export) runs through the app's event
//! channel, never on the UI thread.
//!
//! Flow:
//!   * **New** — name, description and a PGN (or Scid) file; `Create` starts
//!     the import and the dialog shows its progress.
//!   * **Overview** — the headline counts, `Open database` and the housekeeping
//!     actions from the design: remove duplicates, remove empty games, add
//!     games, export to PGN.
//!   * **Browser** — filters, a paged game table, sorting; double-clicking a
//!     row asks the app to load that game and close the windows.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use eframe::egui::{
    self, Align, Align2, Button, Color32, FontId, RichText, ScrollArea, Sense, Vec2,
};
use shakmaty::{
    fen::Fen, zobrist::Zobrist64, CastlingMode, Chess, EnPassantMode, FromSetup, Position,
};

use crate::database::{
    DatabaseInfo, DbStats, GameFilter, GameRow, JobProgress, ResultMask, SideFilter, SortDir,
    SortKey,
};
use crate::gui::theme;

/// Horizontal padding inside every database window. A frame with this margin
/// on both sides keeps the content away from the window edge and — more
/// importantly — makes the left and right gaps the same size, which they were
/// not before.
const WINDOW_PAD_X: i8 = 20;

/// Vertical padding above the first and below the last row of a window body.
const WINDOW_PAD_Y: i8 = 6;

/// The row height of the game table.
const ROW_HEIGHT: f32 = 26.0;

/// Draw a window body inside an equally padded frame.
fn window_body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(WINDOW_PAD_X, WINDOW_PAD_Y))
        .show(ui, add)
        .inner
}

/// A filter caption: muted normally, yellow while its filter is in use.
fn filter_label(ui: &mut egui::Ui, text: &str, active: bool) {
    ui.label(RichText::new(text).size(13.0).color(if active {
        theme::ALERT_YELLOW
    } else {
        theme::MUTED
    }));
}

/// A text field whose typed value turns yellow once it holds a filter.
fn filter_text_edit<'a>(text: &'a mut String, width: f32, hint: &str) -> egui::TextEdit<'a> {
    let active = !text.trim().is_empty();
    egui::TextEdit::singleline(text)
        .desired_width(width)
        .hint_text(hint)
        .text_color(if active {
            theme::ALERT_YELLOW
        } else {
            theme::TEXT
        })
}

/// A button whose label is left aligned. egui's own [`egui::Button`] centres
/// its text; the wide "Delete database…" housekeeping button reads better with
/// the label starting at the same left edge as the text above it.
fn left_aligned_button(
    ui: &mut egui::Ui,
    label: &str,
    size: Vec2,
    colour: Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter()
        .rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
    ui.painter().rect_stroke(
        rect,
        visuals.corner_radius,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(15.0),
        colour,
    );
    response
}

/// Format a count with the thousands separator of the user's locale:
/// `12.123` in Germany, `12,123` in the English-speaking world. Only the
/// separator and the Indian 2-digit grouping differ; that covers the expected
/// game counts without pulling in a full number-formatting crate.
pub fn grouped_number(value: u64) -> String {
    // The system locale cannot change meaningfully while the app runs, and
    // the windows call this for every count on every frame.
    static LOCALE: OnceLock<String> = OnceLock::new();
    group_with_locale(
        value,
        LOCALE.get_or_init(|| sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string())),
    )
}

/// Testable core of [`grouped_number`]. Locale tags are BCP 47 (`de-DE`) but
/// may arrive with an underscore or a `.UTF-8` suffix, so both are normalised.
fn group_with_locale(value: u64, locale: &str) -> String {
    let tag = locale.to_ascii_lowercase().replace('_', "-");
    let tag = tag.split('.').next().unwrap_or(&tag);
    let mut parts = tag.split('-');
    let language = parts.next().unwrap_or_default();
    let region = parts.next().unwrap_or_default();

    // Swiss German/French/Italian write thousands with an apostrophe.
    if region == "ch" {
        return insert_groups(&value.to_string(), '\u{2019}', 3);
    }
    match language {
        // Indian languages group the last three digits, then pairs.
        "hi" | "bn" | "mr" | "ta" | "te" | "gu" | "kn" | "ml" | "pa" | "ur" => {
            indian_groups(&value.to_string(), ',')
        }
        // Locales whose convention is 12.123.
        "de" | "es" | "it" | "pt" | "nl" | "id" | "tr" | "el" | "ro" | "vi" | "hr" | "sr"
        | "sl" | "bg" | "da" | "is" => insert_groups(&value.to_string(), '.', 3),
        // Locales whose convention is 12 123.
        "fr" | "pl" | "cs" | "sk" | "sv" | "fi" | "no" | "nb" | "nn" | "hu" | "et" | "lv"
        | "lt" | "ru" | "uk" | "be" | "kk" => insert_groups(&value.to_string(), '\u{202f}', 3),
        _ => insert_groups(&value.to_string(), ',', 3),
    }
}

/// Insert `separator` every `group` digits, counting from the right.
fn insert_groups(digits: &str, separator: char, group: usize) -> String {
    let chars: Vec<char> = digits.chars().collect();
    let mut out = String::with_capacity(digits.len() + digits.len() / group);
    for (index, ch) in chars.iter().enumerate() {
        if index > 0 && (chars.len() - index).is_multiple_of(group) {
            out.push(separator);
        }
        out.push(*ch);
    }
    out
}

/// Indian grouping: 1,23,45,678 (three digits, then groups of two).
fn indian_groups(digits: &str, separator: char) -> String {
    if digits.len() <= 3 {
        return digits.to_string();
    }
    let (head, tail) = digits.split_at(digits.len() - 3);
    let chars: Vec<char> = head.chars().collect();
    let mut out = String::with_capacity(digits.len() + chars.len() / 2);
    for (index, ch) in chars.iter().enumerate() {
        if index > 0 && (chars.len() - index).is_multiple_of(2) {
            out.push(separator);
        }
        out.push(*ch);
    }
    out.push(separator);
    out.push_str(tail);
    out
}

/// Which of the three windows is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbScreen {
    /// Name, description, source file.
    New,
    /// Counts and housekeeping.
    Overview,
    /// Filters and the game table.
    Browser,
}

/// A running background job, for the progress strip.
#[derive(Clone)]
pub struct RunningJob {
    /// What the job is doing, e.g. "Importing…".
    pub title: String,
    /// Latest progress numbers.
    pub progress: JobProgress,
    /// Cooperative cancel flag shared with the job.
    pub cancel: Arc<AtomicBool>,
}

/// One instruction from the database windows to the app.
#[derive(Debug)]
pub enum DbAction {
    /// Nothing to do.
    None,
    /// Close the windows, keeping the database loaded.
    Close,
    /// Pick a source file for the new-database form.
    PickSource,
    /// Pick where the new database file is written.
    PickTarget,
    /// Create the database from the form and import the source.
    Create {
        /// `db_info.title`.
        title: String,
        /// `db_info.description`.
        description: String,
        /// PGN or Scid file to import.
        source: PathBuf,
        /// The `.sbd` file the user chose to write.
        target: PathBuf,
    },
    /// Show the browser for the loaded database.
    Explore,
    /// Re-run the query after a filter change.
    ExploreFilter,
    /// Look up player names matching the typed Player text.
    SuggestPlayers(String),
    /// Delete formatting-only duplicates.
    RemoveDuplicates,
    /// Delete games without moves.
    RemoveEmptyGames,
    /// Pick a PGN file and add its games to the open database.
    AddGames,
    /// Pick a PGN destination and export the current selection.
    ExportPgn,
    /// Show the database file in Finder.
    RevealInFinder,
    /// Delete the database file (after the inline confirmation).
    DeleteDatabase,
    /// Rebuild the position index of the open database.
    RebuildPositions,
    /// Append the next page to the table (infinite scroll).
    LoadMore,
    /// Jump to a zero-based page (the ◀ / ▶ buttons).
    Page(usize),
    /// Change the sort column; picking the active column flips the direction.
    Sort(SortKey),
    /// Load this game onto the board.
    LoadGame(i64),
    /// Cancel the running job.
    CancelJob,
}

/// The raw filter fields. They are kept as text so a half-typed number or FEN
/// does not reset the search; [`FilterForm::build`] converts them.
#[derive(Clone, Debug)]
pub struct FilterForm {
    /// Player name fragment.
    pub player: String,
    /// Side the player filter applies to.
    pub side: SideFilter,
    /// Minimum Elo, as typed.
    pub elo_min: String,
    /// Maximum Elo, as typed.
    pub elo_max: String,
    /// Event name fragment.
    pub event: String,
    /// Include `1-0`.
    pub white: bool,
    /// Include draws.
    pub draw: bool,
    /// Include `0-1`.
    pub black: bool,
    /// First year, as typed.
    pub year_from: String,
    /// Last year, as typed.
    pub year_to: String,
    /// FEN of the position the game must contain.
    pub fen: String,
}

impl Default for FilterForm {
    fn default() -> Self {
        Self {
            player: String::new(),
            side: SideFilter::Either,
            elo_min: String::new(),
            elo_max: String::new(),
            event: String::new(),
            white: true,
            draw: true,
            black: true,
            year_from: String::new(),
            year_to: String::new(),
            fen: String::new(),
        }
    }
}

impl FilterForm {
    /// Build the query filter. An unparsable FEN is reported so the caller can
    /// keep the old search instead of silently searching for nothing.
    pub fn build(&self) -> (GameFilter, Option<String>) {
        let elo = |text: &str| -> Option<u16> {
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            digits.parse::<u16>().ok()
        };
        let year = |text: &str| -> Option<i32> {
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            digits
                .parse::<i32>()
                .ok()
                .filter(|year| (1000..=9999).contains(year))
        };
        let mut error = None;
        let position = if self.fen.trim().is_empty() {
            None
        } else {
            match position_hash(&self.fen) {
                Some(hash) => Some(hash),
                None => {
                    error = Some(
                        "That FEN cannot be read — the position filter is unchanged.".to_string(),
                    );
                    None
                }
            }
        };
        let filter = GameFilter {
            player: self.player.clone(),
            side: self.side,
            elo_min: elo(&self.elo_min),
            elo_max: elo(&self.elo_max),
            event: self.event.clone(),
            results: ResultMask {
                white: self.white,
                draw: self.draw,
                black: self.black,
            },
            year_from: year(&self.year_from),
            year_to: year(&self.year_to),
            position,
            include_deleted: false,
        };
        (filter, error)
    }

    /// Reset every field, keeping the current position text out of it.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Turn a database name into a safe file stem (`My games 2026` → `My-games-2026`).
pub fn sanitize_stem(name: &str) -> String {
    let mut stem = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            stem.push(ch);
        } else if ch == ' ' || ch == '.' {
            stem.push('-');
        }
    }
    while stem.ends_with('-') {
        stem.pop();
    }
    stem
}

/// Polyglot key of a FEN, exposed for the database diagnostics test.
#[cfg(test)]
pub fn position_hash_for_tests(fen: &str) -> Option<u64> {
    position_hash(fen)
}

/// Polyglot key of a FEN, or `None` when it does not parse.
fn position_hash(fen: &str) -> Option<u64> {
    let parsed = Fen::from_ascii(fen.trim().as_bytes()).ok()?;
    let chess = Chess::from_setup(parsed.into_setup(), CastlingMode::Standard).ok()?;
    Some(chess.zobrist_hash::<Zobrist64>(EnPassantMode::Legal).0)
}

/// All state of the database windows.
pub struct DatabaseUi {
    /// Whether any of the windows is visible.
    pub visible: bool,
    /// Which one.
    pub screen: DbScreen,
    /// New-database form: name.
    pub new_name: String,
    /// New-database form: description.
    pub new_description: String,
    /// New-database form: source file.
    pub new_source: Option<PathBuf>,
    /// New-database form: the `.sbd` file the user chose to write.
    pub new_target: Option<PathBuf>,
    /// Whether the user chose the file themselves (then the name no longer
    /// rewrites it).
    pub target_edited: bool,
    /// Whether the overview asks "really delete this file?".
    pub confirm_delete: bool,
    /// Index positions while importing (slower, enables the position filter).
    pub index_positions: bool,
    /// Validate every move while importing (slow, for hand-kept files).
    pub validate_moves: bool,
    /// The loaded database, once open.
    pub info: Option<DatabaseInfo>,
    /// Its counts.
    pub stats: Option<DbStats>,
    /// The filter fields.
    pub form: FilterForm,
    /// The filter the current rows were loaded with.
    pub applied: GameFilter,
    /// Rows loaded so far; scrolling appends further pages.
    pub rows: Vec<GameRow>,
    /// Pages loaded so far (the table starts with page 0).
    pub pages_loaded: usize,
    /// Rows per page.
    pub page_size: usize,
    /// Whether another page follows.
    pub has_more: bool,
    /// Whether a page request is in flight, so scrolling does not stack them.
    pub loading: bool,
    /// Zero-based page the loaded rows start at (page navigation).
    pub base_page: usize,
    /// Page number derived from the first visible row (1-based).
    pub visible_page: usize,
    /// Total matching games, once counted.
    pub total: Option<u64>,
    /// Active sort column.
    pub sort: SortKey,
    /// Active sort direction.
    pub sort_dir: SortDir,
    /// Selected game id, for the row highlight.
    pub selected: Option<i64>,
    /// Running import/export job.
    pub job: Option<RunningJob>,
    /// Status text under the header (errors in red).
    pub message: Option<(bool, String)>,
    /// Player names matching the typed Player filter, for the autocomplete
    /// popup.
    pub player_suggestions: Vec<String>,
    /// The Player text the current suggestions belong to. A late answer for an
    /// older prefix is ignored.
    pub suggestions_for: String,
    /// Whether the suggestion popup is open.
    pub suggestions_open: bool,
}

impl Default for DatabaseUi {
    fn default() -> Self {
        Self {
            visible: false,
            screen: DbScreen::Overview,
            new_name: String::new(),
            new_description: String::new(),
            new_source: None,
            new_target: None,
            target_edited: false,
            confirm_delete: false,
            index_positions: true,
            validate_moves: false,
            info: None,
            stats: None,
            form: FilterForm::default(),
            applied: GameFilter::default(),
            rows: Vec::new(),
            pages_loaded: 0,
            page_size: crate::database::query::DEFAULT_PAGE_SIZE,
            has_more: false,
            loading: false,
            base_page: 0,
            visible_page: 1,
            total: None,
            sort: SortKey::Date,
            sort_dir: SortDir::Desc,
            selected: None,
            job: None,
            message: None,
            player_suggestions: Vec::new(),
            suggestions_for: String::new(),
            suggestions_open: false,
        }
    }
}

impl DatabaseUi {
    /// Show the current screen and return what the user asked for.
    ///
    /// `board_fen` is the position on the board; the browser's *Use board*
    /// button copies it into the position field.
    pub fn show(&mut self, ctx: &egui::Context, board_fen: &str) -> DbAction {
        if !self.visible {
            return DbAction::None;
        }
        match self.screen {
            DbScreen::New => self.show_new(ctx),
            DbScreen::Overview => self.show_overview(ctx),
            DbScreen::Browser => self.show_browser(ctx, board_fen),
        }
    }

    /// Prepare the new-database form for a source file the user picked.
    pub fn prefill(&mut self, source: PathBuf) {
        let stem = source
            .file_stem()
            .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
        if self.new_name.trim().is_empty() {
            self.new_name = stem;
        }
        self.new_target = Some(crate::database::default_database_path(&source));
        self.target_edited = false;
        self.new_source = Some(source);
        self.screen = DbScreen::New;
        self.visible = true;
        self.message = None;
        self.player_suggestions.clear();
        self.suggestions_for.clear();
        self.suggestions_open = false;
    }

    /// Show the overview for a freshly opened database.
    pub fn show_loaded(&mut self, info: DatabaseInfo, stats: DbStats) {
        self.info = Some(info);
        self.stats = Some(stats);
        self.screen = DbScreen::Overview;
        self.visible = true;
        self.message = None;
        self.player_suggestions.clear();
        self.suggestions_for.clear();
        self.suggestions_open = false;
    }

    /// One line describing the filter the table is showing, so the effect of
    /// "Either side" versus "White"/"Black" is visible instead of guessed.
    fn filter_summary(&self) -> String {
        let filter = &self.applied;
        let mut parts: Vec<String> = Vec::new();
        if !filter.player.trim().is_empty() {
            parts.push(format!("player “{}”", filter.player.trim()));
        }
        if filter.elo_min.is_some() || filter.elo_max.is_some() {
            let min = filter
                .elo_min
                .map_or_else(|| "any".to_string(), |elo| elo.to_string());
            let max = filter
                .elo_max
                .map_or_else(|| "any".to_string(), |elo| elo.to_string());
            parts.push(format!("Elo {min}–{max}"));
        }
        if !filter.event.trim().is_empty() {
            parts.push(format!("event “{}”", filter.event.trim()));
        }
        if filter.year_from.is_some() || filter.year_to.is_some() {
            parts.push(format!(
                "year {}–{}",
                filter
                    .year_from
                    .map_or_else(|| "any".to_string(), |year| year.to_string()),
                filter
                    .year_to
                    .map_or_else(|| "any".to_string(), |year| year.to_string())
            ));
        }
        if filter.position.is_some() {
            parts.push("position".to_string());
        }
        if parts.is_empty() {
            "No filter — showing the whole database".to_string()
        } else {
            format!("Filter: {} · {}", parts.join(" · "), filter.side.label())
        }
    }

    /// `Name #55.513 games` for the browser window and the overview: the
    /// matching count once the background count has arrived, the stored total
    /// before that. The number uses the locale's thousands separator.
    fn title_with_count(&self) -> String {
        let name = self
            .info
            .as_ref()
            .map_or_else(|| "Database".to_string(), |info| info.title.clone());
        let count = self.total.or_else(|| self.stats.map(|stats| stats.games));
        match count {
            Some(count) => format!("{name} #{} games", grouped_number(count)),
            None => format!("{name} #… games"),
        }
    }

    fn show_new(&mut self, ctx: &egui::Context) -> DbAction {
        let mut action = DbAction::None;
        let mut open = true;
        egui::Window::new("New database")
            // A fixed id: egui identifies a window by its title unless told
            // otherwise, and a title that changes (the browser shows the game
            // count in it) would destroy and re-create the window on every
            // count update — which looks like the window closing and opening.
            .id(egui::Id::unique("siliconboard_database_new"))
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .default_pos(ctx.content_rect().center() - egui::vec2(280.0, 190.0))
            .open(&mut open)
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    ui.label(
                        RichText::new("Create a database from a PGN file!")
                            .size(16.0)
                            .strong()
                            .color(theme::NEON_GREEN),
                    );
                    ui.add_space(8.0);
                    ui.label(RichText::new("Name *").size(14.0).color(theme::MUTED));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_name)
                            .desired_width(f32::INFINITY)
                            .hint_text("My games"),
                    );
                    // The file name follows the database name until the user
                    // picks a location themselves.
                    if !self.target_edited {
                        let stem = sanitize_stem(&self.new_name);
                        self.new_target = (!stem.is_empty()).then(|| {
                            crate::database::default_database_dir().join(format!("{stem}.sbd"))
                        });
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Description").size(14.0).color(theme::MUTED));
                    ui.add(
                        egui::TextEdit::multiline(&mut self.new_description)
                            .desired_width(f32::INFINITY)
                            .desired_rows(2)
                            .hint_text("optional"),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("PGN file *").size(14.0).color(theme::MUTED));
                    let source_text = self.new_source.as_ref().map_or_else(
                        || "Click to select the PGN file".to_string(),
                        |path| path.display().to_string(),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add(Button::new("Browse…").min_size(Vec2::new(90.0, 26.0)))
                            .clicked()
                        {
                            action = DbAction::PickSource;
                        }
                        ui.add(
                            egui::Label::new(RichText::new(source_text).size(13.0).color(
                                if self.new_source.is_some() {
                                    theme::TEXT
                                } else {
                                    theme::MUTED
                                },
                            ))
                            .truncate(),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Database file *")
                            .size(14.0)
                            .color(theme::MUTED),
                    );
                    let target_text = self.new_target.as_ref().map_or_else(
                        || "Choose where to save the database".to_string(),
                        |path| path.display().to_string(),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add(Button::new("Browse…").min_size(Vec2::new(90.0, 26.0)))
                            .on_hover_text("Choose where the .sbd file is saved")
                            .clicked()
                        {
                            action = DbAction::PickTarget;
                        }
                        ui.add(
                            egui::Label::new(RichText::new(target_text).size(13.0).color(
                                if self.new_target.is_some() {
                                    theme::TEXT
                                } else {
                                    theme::MUTED
                                },
                            ))
                            .truncate(),
                        );
                    });
                    ui.add_space(6.0);
                    ui.checkbox(
                        &mut self.index_positions,
                        RichText::new("Index positions (enables the position filter; slower)")
                            .size(13.0),
                    );
                    ui.checkbox(
                        &mut self.validate_moves,
                        RichText::new("Validate every move while importing (slow)").size(13.0),
                    );
                    if let Some((error, text)) = &self.message {
                        ui.add_space(4.0);
                        ui.colored_label(
                            if *error {
                                theme::DANGER_RED
                            } else {
                                theme::TEXT
                            },
                            RichText::new(text).size(13.0),
                        );
                    }
                    self.progress_strip(ui, &mut action);
                    // Breathing room between the last checkbox and the actions:
                    // the two rows read as separate groups instead of one block.
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        let ready = !self.new_name.trim().is_empty()
                            && self.new_source.is_some()
                            && self.new_target.is_some()
                            && self.job.is_none();
                        if ui
                            .add_enabled(
                                ready,
                                Button::new("Create").min_size(Vec2::new(120.0, 32.0)),
                            )
                            .clicked()
                        {
                            if let (Some(source), Some(target)) =
                                (self.new_source.clone(), self.new_target.clone())
                            {
                                action = DbAction::Create {
                                    title: self.new_name.trim().to_string(),
                                    description: self.new_description.trim().to_string(),
                                    source,
                                    target,
                                };
                            }
                        }
                        if ui
                            .add(
                                Button::new(RichText::new("Cancel").color(theme::ALERT_YELLOW))
                                    .min_size(Vec2::new(100.0, 32.0)),
                            )
                            .clicked()
                        {
                            action = DbAction::Close;
                        }
                    });
                });
            });
        if !open {
            return DbAction::Close;
        }
        action
    }

    fn show_overview(&mut self, ctx: &egui::Context) -> DbAction {
        let mut action = DbAction::None;
        let mut open = true;
        egui::Window::new("Database")
            .id(egui::Id::unique("siliconboard_database_overview"))
            .collapsible(false)
            .resizable(false)
            .default_width(600.0)
            .default_pos(ctx.content_rect().center() - egui::vec2(300.0, 260.0))
            .open(&mut open)
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                let Some(info) = self.info.as_ref() else {
                    ui.label(RichText::new("No database is open.").color(theme::MUTED));
                    return;
                };
                let stats = self.stats.unwrap_or_default();
                // Name and game count are the headline: both in the palette's
                // evaluation green, with the count grouped for the locale.
                ui.label(
                    RichText::new(format!(
                        "{} #{} games",
                        info.title,
                        grouped_number(stats.games)
                    ))
                    .size(20.0)
                    .strong()
                    .color(theme::NEON_GREEN),
                );
                if !info.description.is_empty() {
                    ui.label(
                        RichText::new(&info.description)
                            .size(14.0)
                            .color(theme::MUTED),
                    );
                }
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Saved in:").size(12.0).color(theme::MUTED));
                    ui.label(
                        RichText::new(info.path.display().to_string())
                            .size(12.0)
                            .color(theme::TEXT),
                    );
                    if ui
                        .add(Button::new("Reveal in Finder").min_size(Vec2::new(130.0, 22.0)))
                        .clicked()
                    {
                        action = DbAction::RevealInFinder;
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    for (label, value) in [
                        ("GAMES", stats.games),
                        ("PLAYERS", stats.players),
                        ("EVENTS", stats.events),
                    ] {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(label).size(12.0).strong().color(theme::MUTED));
                            ui.label(
                                RichText::new(grouped_number(value))
                                    .size(22.0)
                                    .strong()
                                    .color(theme::NEON_GREEN),
                            );
                        });
                        ui.add_space(40.0);
                    }
                });
                ui.add_space(10.0);
                if ui
                    .add(
                        Button::new(RichText::new("Open database  →").size(16.0))
                            .min_size(Vec2::new(ui.available_width(), 36.0)),
                    )
                    .clicked()
                {
                    action = DbAction::Explore;
                }
                ui.add_space(10.0);
                ui.separator();
                ui.label(
                    RichText::new("Batch delete")
                        .size(14.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new("These actions remove games from the database. Use with care.")
                        .size(12.0)
                        .color(theme::MUTED),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new("Remove Duplicates").min_size(Vec2::new(160.0, 30.0)))
                        .clicked()
                    {
                        action = DbAction::RemoveDuplicates;
                    }
                    if ui
                        .add(Button::new("Remove Empty Games").min_size(Vec2::new(170.0, 30.0)))
                        .clicked()
                    {
                        action = DbAction::RemoveEmptyGames;
                    }
                });
                ui.add_space(10.0);
                ui.separator();
                if ui
                    .add(Button::new("Rebuild position index").min_size(Vec2::new(200.0, 28.0)))
                    .on_hover_text(
                        "Indexes every position of every game, so the Position filter finds any FEN",
                    )
                    .clicked()
                {
                    action = DbAction::RebuildPositions;
                }
                ui.add_space(10.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new("Add Games").min_size(Vec2::new(140.0, 30.0)))
                        .on_hover_text("Import another PGN file into this database")
                        .clicked()
                    {
                        action = DbAction::AddGames;
                    }
                    if ui
                        .add(Button::new("Export to PGN").min_size(Vec2::new(150.0, 30.0)))
                        .clicked()
                    {
                        action = DbAction::ExportPgn;
                    }
                });
                self.progress_strip(ui, &mut action);
                if let Some((error, text)) = &self.message {
                    ui.add_space(4.0);
                    ui.colored_label(
                        if *error {
                            theme::DANGER_RED
                        } else {
                            theme::TEXT
                        },
                        RichText::new(text).size(13.0),
                    );
                }
                ui.add_space(18.0);
                if self.confirm_delete {
                    ui.colored_label(
                        theme::DANGER_RED,
                        RichText::new(
                            "Delete this database file? The games in it are gone for good.",
                        )
                        .size(13.0),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                Button::new(RichText::new("Delete").color(theme::DANGER_RED))
                                    .min_size(Vec2::new(110.0, 30.0)),
                            )
                            .clicked()
                        {
                            action = DbAction::DeleteDatabase;
                        }
                        if ui
                            .add(
                                Button::new(
                                    RichText::new("Cancel").color(theme::ALERT_YELLOW),
                                )
                                .min_size(Vec2::new(110.0, 30.0)),
                            )
                            .clicked()
                        {
                            self.confirm_delete = false;
                        }
                    });
                } else {
                    // Both housekeeping buttons sit on the right, spaced away
                    // from the content above. Their labels are yellow, and the
                    // wide destructive button left-aligns its text.
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if left_aligned_button(
                                ui,
                                "Delete database…",
                                Vec2::new(210.0, 30.0),
                                theme::ALERT_YELLOW,
                            )
                            .clicked()
                            {
                                self.confirm_delete = true;
                            }
                            ui.add_space(12.0);
                            if ui
                                .add(
                                    Button::new(
                                        RichText::new("Close").color(theme::ALERT_YELLOW),
                                    )
                                    .min_size(Vec2::new(100.0, 30.0)),
                                )
                                .clicked()
                            {
                                action = DbAction::Close;
                            }
                        });
                    });
                }
                });
            });
        if !open {
            return DbAction::Close;
        }
        action
    }

    fn show_browser(&mut self, ctx: &egui::Context, board_fen: &str) -> DbAction {
        let mut action = DbAction::None;
        let mut open = true;
        let title = self.title_with_count();
        egui::Window::new(RichText::new(title).color(theme::NEON_GREEN))
            .id(egui::Id::unique("siliconboard_database_browser"))
            .collapsible(false)
            .resizable(true)
            .default_size(Vec2::new(1240.0, 780.0))
            .min_size(Vec2::new(1000.0, 560.0))
            .max_size(Vec2::new(1600.0, 1000.0))
            .default_pos(ctx.content_rect().center() - egui::vec2(620.0, 390.0))
            .open(&mut open)
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    // The window title already names the database and its count,
                    // so this row is the way back only — the old repeated headline
                    // was clutter.
                    ui.horizontal(|ui| {
                        if ui
                            .add(Button::new("←").min_size(Vec2::new(30.0, 24.0)))
                            .on_hover_text("Back to the database overview")
                            .clicked()
                        {
                            self.screen = DbScreen::Overview;
                        }
                        ui.label(RichText::new("go back…").size(14.0).color(theme::MUTED));
                    });
                    ui.add_space(4.0);
                    self.filter_bar(ui, board_fen, &mut action);
                    ui.add_space(6.0);
                    let filtered = self.applied != GameFilter::default();
                    ui.label(
                        RichText::new(self.filter_summary())
                            .size(12.0)
                            .color(if filtered {
                                theme::ALERT_YELLOW
                            } else {
                                theme::MUTED
                            }),
                    );
                    ui.separator();
                    let current = std::mem::replace(&mut action, DbAction::None);
                    action = self.table(ui, current);
                    ui.separator();
                    ui.horizontal(|ui| {
                        let shown = self.rows.len();
                        let total = self.total.or_else(|| self.stats.map(|stats| stats.games));
                        let page_count =
                            total.map_or(1, |total| total.div_ceil(self.page_size as u64).max(1));
                        let total_text = total.map_or_else(|| "…".to_string(), grouped_number);
                        if ui
                            .add_enabled(self.visible_page > 1, Button::new("◀"))
                            .clicked()
                        {
                            action = DbAction::Page(self.visible_page.saturating_sub(2));
                        }
                        ui.label(
                            RichText::new(format!(
                                "Page {} of {}  ·  games 1–{} of {}",
                                self.visible_page,
                                page_count,
                                grouped_number(shown as u64),
                                total_text
                            ))
                            .size(13.0),
                        );
                        if ui
                            .add_enabled((self.visible_page as u64) < page_count, Button::new("▶"))
                            .clicked()
                        {
                            action = DbAction::Page(self.visible_page);
                        }
                        if self.loading {
                            ui.label(RichText::new("loading…").size(12.0).color(theme::MUTED));
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new("Double-click a game to load it on the board")
                                    .size(12.0)
                                    .color(theme::MUTED),
                            );
                        });
                    });
                    if let Some((error, text)) = &self.message {
                        ui.colored_label(
                            if *error {
                                theme::DANGER_RED
                            } else {
                                theme::TEXT
                            },
                            RichText::new(text).size(12.0),
                        );
                    }
                    self.progress_strip(ui, &mut action);
                });
            });
        if !open {
            return DbAction::Close;
        }
        action
    }

    /// The two-row filter strip.
    ///
    /// A caption and the typed value turn yellow as soon as that field really
    /// filters something, so the eye can see the active parts of a search at a
    /// glance. Typing in Player opens the name suggestion popup.
    fn filter_bar(&mut self, ui: &mut egui::Ui, board_fen: &str, action: &mut DbAction) {
        let player_response = ui
            .horizontal(|ui| {
                filter_label(ui, "Player", !self.form.player.trim().is_empty());
                let response = ui.add(filter_text_edit(&mut self.form.player, 180.0, "name"));

                let side_active = self.form.side != SideFilter::Either;
                egui::ComboBox::from_id_salt("db_side")
                    .width(100.0)
                    .selected_text(RichText::new(self.form.side.label()).size(13.0).color(
                        if side_active {
                            theme::ALERT_YELLOW
                        } else {
                            theme::TEXT
                        },
                    ))
                    .show_ui(ui, |ui| {
                        for side in [SideFilter::Either, SideFilter::White, SideFilter::Black] {
                            ui.selectable_value(&mut self.form.side, side, side.label());
                        }
                    });

                let elo_active =
                    !self.form.elo_min.trim().is_empty() || !self.form.elo_max.trim().is_empty();
                filter_label(ui, "Elo", elo_active);
                ui.add(filter_text_edit(&mut self.form.elo_min, 70.0, "min"));
                ui.add(filter_text_edit(&mut self.form.elo_max, 70.0, "max"));
                filter_label(ui, "Event", !self.form.event.trim().is_empty());
                ui.add(filter_text_edit(&mut self.form.event, 170.0, "event"));
                response
            })
            .inner;

        // A changed Player field asks the app for names; the answer arrives as
        // an app event and only fills the popup if it still belongs to the
        // text that is on screen now.
        if player_response.changed() {
            let fragment = self.form.player.trim().to_string();
            self.suggestions_for.clear();
            self.player_suggestions.clear();
            if fragment.is_empty() {
                self.suggestions_open = false;
            } else {
                self.suggestions_open = true;
                *action = DbAction::SuggestPlayers(fragment);
            }
        }
        self.player_suggestion_popup(&player_response);

        ui.horizontal(|ui| {
            let results = ResultMask {
                white: self.form.white,
                draw: self.form.draw,
                black: self.form.black,
            };
            filter_label(ui, "Result", results != ResultMask::default());
            ui.checkbox(&mut self.form.white, RichText::new("1-0").size(13.0));
            ui.checkbox(&mut self.form.draw, RichText::new("½-½").size(13.0));
            ui.checkbox(&mut self.form.black, RichText::new("0-1").size(13.0));
            ui.add_space(8.0);
            let date_active =
                !self.form.year_from.trim().is_empty() || !self.form.year_to.trim().is_empty();
            filter_label(ui, "Date", date_active);
            ui.add(filter_text_edit(&mut self.form.year_from, 60.0, "from"));
            ui.add(filter_text_edit(&mut self.form.year_to, 60.0, "to"));
            ui.add_space(8.0);
            filter_label(ui, "Position", !self.form.fen.trim().is_empty());
            ui.add(filter_text_edit(&mut self.form.fen, 210.0, "FEN"));
            if ui.button("Use board").clicked() {
                self.form.fen = board_fen.to_string();
            }
            if ui
                .add(Button::new(RichText::new("Apply").strong()))
                .clicked()
            {
                let (filter, error) = self.form.build();
                self.applied = filter;
                if let Some(error) = error {
                    self.message = Some((true, error));
                }
                self.suggestions_open = false;
                *action = DbAction::ExploreFilter;
            }
            if ui.button("Clear").clicked() {
                self.form.clear();
                self.applied = GameFilter::default();
                self.player_suggestions.clear();
                self.suggestions_open = false;
                *action = DbAction::ExploreFilter;
            }
        });
    }

    /// The autocomplete list under the Player field. A popup closes on Escape
    /// or a click outside, so it never becomes a permanent overlay.
    fn player_suggestion_popup(&mut self, anchor: &egui::Response) {
        if !self.suggestions_open {
            return;
        }
        let mut chosen = None;
        let suggestions = &self.player_suggestions;
        let open = &mut self.suggestions_open;
        let answered = self.suggestions_for.trim() == self.form.player.trim();
        let width = anchor.rect.width().max(240.0);
        egui::Popup::from_response(anchor)
            .open_bool(open)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(width)
            .gap(4.0)
            .show(|ui| {
                ui.set_max_height(250.0);
                ScrollArea::vertical()
                    .id_salt("db_player_suggestion_list")
                    .max_height(230.0)
                    .show(ui, |ui| {
                        if suggestions.is_empty() {
                            let (text, colour) = if answered {
                                ("no matching name", theme::MUTED)
                            } else {
                                ("searching…", theme::MUTED)
                            };
                            ui.label(RichText::new(text).size(13.0).color(colour));
                        } else {
                            for name in suggestions {
                                if ui
                                    .add(
                                        Button::new(RichText::new(name).size(14.0))
                                            .frame(false)
                                            .min_size(Vec2::new(width - 16.0, 24.0)),
                                    )
                                    .on_hover_text("Use this player name")
                                    .clicked()
                                {
                                    chosen = Some(name.clone());
                                }
                            }
                        }
                    });
            });
        if let Some(name) = chosen {
            self.form.player = name;
            self.suggestions_open = false;
            self.player_suggestions.clear();
        }
    }

    /// The game table. Returns the action the rows asked for.
    fn table(&mut self, ui: &mut egui::Ui, mut action: DbAction) -> DbAction {
        let row_height = ROW_HEIGHT;
        let width = ui.available_width();
        let widths = table_widths(width);
        // Header strip with clickable sort targets.
        let (header_rect, _) = ui.allocate_exact_size(egui::vec2(width, 24.0), Sense::hover());
        ui.painter()
            .rect_filled(header_rect, 0.0, theme::TOOLBAR_BG);
        let mut x = header_rect.left();
        for (label, key, column_width) in [
            ("#", SortKey::Id, widths[0]),
            ("White", SortKey::WhiteName, widths[1]),
            ("Elo", SortKey::WhiteElo, widths[2]),
            ("Black", SortKey::BlackName, widths[3]),
            ("Elo", SortKey::BlackElo, widths[4]),
            ("Date", SortKey::Date, widths[5]),
            ("Result", SortKey::Id, widths[6]),
            ("Event", SortKey::Id, widths[7]),
            ("Plies", SortKey::PlyCount, widths[8]),
        ] {
            let rect = egui::Rect::from_min_size(
                egui::pos2(x, header_rect.top()),
                egui::vec2(column_width, header_rect.height()),
            );
            let response = ui.interact(
                rect,
                ui.scope_id().with(("db_head", label, x as i32)),
                Sense::click(),
            );
            let arrow = if self.sort == key {
                match self.sort_dir {
                    SortDir::Asc => " ↑",
                    SortDir::Desc => " ↓",
                }
            } else {
                ""
            };
            ui.painter().text(
                egui::pos2(x + 6.0, header_rect.center().y),
                egui::Align2::LEFT_CENTER,
                format!("{label}{arrow}"),
                egui::FontId::proportional(13.0),
                theme::TEXT,
            );
            if response.clicked() {
                action = DbAction::Sort(key);
            }
            x += column_width;
        }
        ui.separator();
        if self.rows.is_empty() && self.message.is_none() {
            ui.add_space(8.0);
            ui.label(
                RichText::new("No games match these filters.")
                    .size(13.0)
                    .color(theme::MUTED),
            );
        }
        let viewport = (ui.available_height() - 40.0).max(120.0);
        let out = ScrollArea::vertical()
            .id_salt("db_games")
            .auto_shrink([false, false])
            .max_height(viewport)
            .show_rows(ui, row_height, self.rows.len(), |ui, range| {
                for index in range {
                    let row = &self.rows[index];
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(width, row_height), Sense::click());
                    let fill = if self.selected == Some(row.id) {
                        Some(ui.visuals().selection.bg_fill)
                    } else if response.hovered() {
                        Some(ui.visuals().widgets.hovered.bg_fill)
                    } else if index % 2 == 1 {
                        Some(ui.visuals().faint_bg_color)
                    } else {
                        None
                    };
                    if let Some(fill) = fill {
                        ui.painter().rect_filled(rect, 0.0, fill);
                    }
                    let cells = [
                        row.id.to_string(),
                        row.white.clone(),
                        row.white_elo
                            .map_or_else(String::new, |elo| elo.to_string()),
                        row.black.clone(),
                        row.black_elo
                            .map_or_else(String::new, |elo| elo.to_string()),
                        row.date_text(),
                        row.result.clone(),
                        row.event.clone(),
                        row.ply_count.to_string(),
                    ];
                    paint_row(ui, rect, &widths, &cells);
                    if response.clicked() {
                        self.selected = Some(row.id);
                    }
                    if response.double_clicked() {
                        action = DbAction::LoadGame(row.id);
                    }
                }
            });
        // The visible page follows the scroll position.
        self.visible_page = self.base_page
            + (out.state.clamped_offset().y / row_height).floor() as usize / self.page_size
            + 1;
        let content = self.rows.len() as f32 * row_height;
        let short = content <= viewport + row_height;
        let near_end = out.state.clamped_offset().y + viewport >= content - 4.0 * row_height;
        if self.has_more && !self.loading && (short || near_end) {
            action = DbAction::LoadMore;
        }
        action
    }

    /// Progress bar and cancel button for a running job.
    fn progress_strip(&mut self, ui: &mut egui::Ui, action: &mut DbAction) {
        let Some(job) = self.job.clone() else {
            return;
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&job.title).size(13.0).color(theme::TEXT));
            ui.add(
                egui::ProgressBar::new(job.progress.fraction())
                    .desired_width(260.0)
                    .show_percentage(),
            );
            ui.label(
                RichText::new(format!("{} games", job.progress.games))
                    .size(12.0)
                    .color(theme::MUTED),
            );
            if ui
                .add(Button::new(
                    RichText::new("Cancel").color(theme::ALERT_YELLOW),
                ))
                .clicked()
            {
                *action = DbAction::CancelJob;
            }
        });
    }
}

/// Column widths that add up to the table width; the Event column takes the
/// rest so the row always fills the window.
fn table_widths(total: f32) -> [f32; 9] {
    let number = 56.0;
    let name = (total * 0.16).clamp(120.0, 240.0);
    let elo = 52.0;
    let date = 96.0;
    let result = 68.0;
    let plies = 56.0;
    let event = (total - number - name * 2.0 - elo * 2.0 - date - result - plies).max(140.0);
    [number, name, elo, name, elo, date, result, event, plies]
}

/// Draw one row, clipping each cell to its column.
fn paint_row(ui: &egui::Ui, rect: egui::Rect, widths: &[f32; 9], cells: &[String; 9]) {
    let painter = ui.painter();
    let font = egui::FontId::proportional(14.0);
    let mut x = rect.left();
    for (text, width) in cells.iter().zip(widths.iter()) {
        let cell = egui::Rect::from_min_max(
            egui::pos2(x, rect.top()),
            egui::pos2(x + width, rect.bottom()),
        );
        painter.with_clip_rect(cell.intersect(ui.clip_rect())).text(
            egui::pos2(x + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            font.clone(),
            theme::TEXT,
        );
        x += width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_builds_the_filter_from_the_text_fields() {
        let form = FilterForm {
            player: "Carlsen, Magnus".to_string(),
            elo_min: "2800".to_string(),
            elo_max: "2900".to_string(),
            year_from: "2020".to_string(),
            year_to: "2024".to_string(),
            draw: false,
            black: false,
            ..FilterForm::default()
        };
        let (filter, error) = form.build();
        assert!(error.is_none());
        assert_eq!(filter.player, "Carlsen, Magnus");
        assert_eq!(filter.elo_min, Some(2800));
        assert_eq!(filter.elo_max, Some(2900));
        assert_eq!(filter.year_from, Some(2020));
        assert_eq!(filter.year_to, Some(2024));
        assert!(!filter.results.draw);
    }

    #[test]
    fn an_unreadable_fen_is_reported_and_leaves_the_position_unset() {
        let form = FilterForm {
            fen: "not a fen".to_string(),
            ..FilterForm::default()
        };
        let (filter, error) = form.build();
        assert!(error.is_some());
        assert!(filter.position.is_none());
    }

    #[test]
    fn the_standard_start_position_hashes_to_the_polyglot_key() {
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        assert_eq!(position_hash(fen), Some(0x463b_9618_1691_fc9c));
    }

    #[test]
    fn thousands_separators_follow_the_locale() {
        assert_eq!(group_with_locale(12_123, "de-DE"), "12.123");
        assert_eq!(group_with_locale(12_123, "en-US"), "12,123");
        assert_eq!(group_with_locale(1_234_567, "de_DE.UTF-8"), "1.234.567");
        assert_eq!(group_with_locale(999, "de-DE"), "999");
        assert_eq!(group_with_locale(12_123, "en-IN"), "12,123");
        assert_eq!(group_with_locale(1_234_567, "hi-IN"), "12,34,567");
        assert_eq!(
            group_with_locale(1_234_567, "fr-FR"),
            "1\u{202f}234\u{202f}567"
        );
        assert_eq!(group_with_locale(12_123, "de-CH"), "12\u{2019}123");
    }

    #[test]
    fn the_browser_title_uses_the_hash_and_locale_grouping() {
        let ui = DatabaseUi {
            info: Some(DatabaseInfo {
                path: PathBuf::from("/tmp/example.sbd"),
                title: "jupp".to_string(),
                description: String::new(),
                game_count: 12_123,
                created_at: 0,
            }),
            stats: Some(DbStats {
                games: 12_123,
                players: 2,
                events: 1,
            }),
            ..DatabaseUi::default()
        };
        // Locale-independent shape: the count is grouped, but the separator
        // itself depends on the machine running the test.
        let title = ui.title_with_count();
        assert!(title.starts_with("jupp #"), "{title}");
        assert!(title.ends_with(" games"), "{title}");
        assert!(
            !title.contains("#12123 games"),
            "count must be grouped: {title}"
        );
    }
}

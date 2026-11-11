//! Tournament configuration window and the running/finished dialogs.
//!
//! The layout follows a classic engine-match dialog, drawn with the
//! app's dark egui palette: engines at the top, then the clock, match details,
//! opening book, adjudication and the PGN destination. The dialog edits a
//! draft and reports a [`TournamentAction`]; the app owns the real settings and
//! plays the games on the main board. The running and result windows are
//! ordinary movable egui windows.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Button, ComboBox, DragValue, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2,
};

use crate::chess::clock::{TimeControl, TimeMode};
use crate::gui::options_dialog::section_heading;
use crate::gui::theme;
use crate::tournament::{
    time_control_tag, BookOrder, EngineSpec, OpeningBookConfig, TournamentConfig, TournamentStatus,
};

/// Height of a field or button in the dialog.
const FIELD_HEIGHT: f32 = 30.0;

/// Width of the label column.
const LABEL_WIDTH: f32 = 178.0;

/// The side whose engine is being edited.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TournamentSide {
    /// White in the first game.
    White,
    /// Black in the first game.
    Black,
}

impl TournamentSide {
    /// `White` or `Black`.
    pub fn label(self) -> &'static str {
        match self {
            TournamentSide::White => "White",
            TournamentSide::Black => "Black",
        }
    }

    fn id(self) -> u8 {
        match self {
            TournamentSide::White => 0,
            TournamentSide::Black => 1,
        }
    }
}

/// What the configuration window asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum TournamentAction {
    /// Nothing happened.
    None,
    /// Close without starting.
    Cancel,
    /// Start the match with this configuration.
    Start(Box<TournamentConfig>),
    /// Open a file picker for one engine.
    BrowseEngine(TournamentSide),
    /// Open a file picker for the EPD or PolyGlot opening book.
    BrowseBook,
    /// Open a file picker for the additional PolyGlot `.bin` move book.
    BrowseMoveBook,
    /// Open a save dialog for the PGN file.
    BrowsePgn,
}

/// The clock shape offered by the dialog.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TimeKind {
    Fischer,
    SuddenDeath,
    FixedPerMove,
}

impl TimeKind {
    const ALL: [Self; 3] = [Self::Fischer, Self::SuddenDeath, Self::FixedPerMove];

    fn label(self) -> &'static str {
        match self {
            Self::Fischer => "Time per game with increment",
            Self::SuddenDeath => "Sudden death",
            Self::FixedPerMove => "Fixed time per move",
        }
    }

    fn from_control(control: &TimeControl) -> Self {
        match control.mode {
            TimeMode::FischerIncrement => Self::Fischer,
            TimeMode::SuddenDeath => Self::SuddenDeath,
            TimeMode::FixedPerMove => Self::FixedPerMove,
        }
    }
}

/// Editable draft of a tournament configuration.
pub struct TournamentDialog {
    config: TournamentConfig,
    engine_choices: Vec<EngineSpec>,
    white_choice: usize,
    black_choice: usize,
    time_kind: TimeKind,
    base_min: u32,
    base_sec: u32,
    inc_sec: u32,
    pgn_path: String,
    book_path: String,
    move_book_path: String,
    book_order: BookOrder,
}

impl TournamentDialog {
    /// A dialog for `config`. `choices` are the engines already known to the
    /// app (the loaded A/B slots and the files used last time).
    pub fn new(config: TournamentConfig, mut choices: Vec<EngineSpec>) -> Self {
        for spec in [&config.white, &config.black] {
            if spec.is_set() && !choices.iter().any(|choice| choice.path == spec.path) {
                choices.push(spec.clone());
            }
        }
        let white_choice = choices
            .iter()
            .position(|choice| choice.path == config.white.path)
            .unwrap_or(0);
        let black_choice = choices
            .iter()
            .position(|choice| choice.path == config.black.path)
            .unwrap_or_else(|| choices.len().saturating_sub(1));
        let time_kind = TimeKind::from_control(&config.time_control);
        Self {
            base_min: (config.time_control.base_ms / 60_000) as u32,
            base_sec: ((config.time_control.base_ms % 60_000) / 1000) as u32,
            inc_sec: (config.time_control.increment_ms / 1000) as u32,
            pgn_path: config.pgn_path.display().to_string(),
            book_path: config
                .opening_book
                .as_ref()
                .map_or_else(String::new, |book| book.path.display().to_string()),
            move_book_path: config
                .move_book
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            book_order: config
                .opening_book
                .as_ref()
                .map_or(BookOrder::Random, |book| book.order),
            white_choice,
            black_choice,
            time_kind,
            engine_choices: choices,
            config,
        }
    }

    /// Set one engine, adding it to the combo list when it is new.
    pub fn set_engine(&mut self, side: TournamentSide, spec: EngineSpec) {
        if !self
            .engine_choices
            .iter()
            .any(|choice| choice.path == spec.path)
        {
            self.engine_choices.push(spec.clone());
        }
        let index = self
            .engine_choices
            .iter()
            .position(|choice| choice.path == spec.path)
            .unwrap_or(0);
        match side {
            TournamentSide::White => {
                self.white_choice = index;
                self.config.white = spec;
            }
            TournamentSide::Black => {
                self.black_choice = index;
                self.config.black = spec;
            }
        }
    }

    /// Set the EPD opening book path.
    pub fn set_book_path(&mut self, path: PathBuf) {
        self.book_path = path.display().to_string();
    }

    /// Set the additional PolyGlot `.bin` move book.
    pub fn set_move_book_path(&mut self, path: PathBuf) {
        self.move_book_path = path.display().to_string();
    }

    /// Set the PGN destination.
    pub fn set_pgn_path(&mut self, path: PathBuf) {
        self.pgn_path = path.display().to_string();
    }

    /// Draw the window and report what the user chose.
    pub fn show(&mut self, ctx: &egui::Context) -> TournamentAction {
        let mut action = TournamentAction::None;
        let mut open = true;
        let screen = ctx.content_rect();
        let max_height = (screen.height() * 0.92).max(560.0);
        let body_height = (max_height - 170.0).max(320.0);
        let default_pos = screen.center() - egui::vec2(380.0, 320.0);

        egui::Window::new("Engine match")
            .collapsible(false)
            .resizable(true)
            .default_width(760.0)
            .min_width(640.0)
            .max_height(max_height)
            .default_pos(default_pos)
            .open(&mut open)
            .show(ctx, |ui| {
                ScrollArea::vertical()
                    .id_salt("tournament_body")
                    .auto_shrink([false, true])
                    .max_height(body_height)
                    .show(ui, |ui| {
                        if let Some(chosen) = self.engines_section(ui) {
                            action = chosen;
                        }
                        if let Some(chosen) = self.time_section(ui) {
                            action = chosen;
                        }
                        if let Some(chosen) = self.details_section(ui) {
                            action = chosen;
                        }
                        if let Some(chosen) = self.book_section(ui) {
                            action = chosen;
                        }
                        if let Some(chosen) = self.adjudication_section(ui) {
                            action = chosen;
                        }
                        if let Some(chosen) = self.pgn_section(ui) {
                            action = chosen;
                        }
                    });

                ui.add_space(8.0);
                ui.separator();
                let draft = self.build_config();
                ui.horizontal(|ui| {
                    match &draft {
                        Ok(_) => {
                            ui.label(
                                RichText::new("The games start automatically after Start.")
                                    .size(13.0)
                                    .color(theme::MUTED),
                            );
                        }
                        Err(message) => {
                            ui.colored_label(theme::DANGER_RED, message);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let start = ui.add_enabled(
                            draft.is_ok(),
                            Button::new(RichText::new("Start tournament").color(theme::TEXT))
                                .min_size(Vec2::new(160.0, 32.0)),
                        );
                        if start.clicked() {
                            if let Ok(config) = &draft {
                                action = TournamentAction::Start(Box::new(config.clone()));
                            }
                        }
                        if ui
                            .add(Button::new("Cancel").min_size(Vec2::new(100.0, 32.0)))
                            .clicked()
                        {
                            action = TournamentAction::Cancel;
                        }
                    });
                });
            });

        if !open || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            return TournamentAction::Cancel;
        }
        action
    }

    /// The engines and their UCI memory options.
    fn engines_section(&mut self, ui: &mut Ui) -> Option<TournamentAction> {
        let mut action = None;
        section_heading(ui, "Engines");
        if let Some(chosen) = self.engine_row(ui, TournamentSide::White) {
            action = Some(chosen);
        }
        if let Some(chosen) = self.engine_row(ui, TournamentSide::Black) {
            action = Some(chosen);
        }
        ui.add_space(4.0);
        row(ui, "Hash table size", |ui| {
            ui.add(
                DragValue::new(&mut self.config.hash_mb)
                    .range(1..=65_536)
                    .suffix(" MB"),
            );
        });
        row(ui, "Nalimov cache size", |ui| {
            ui.add(
                DragValue::new(&mut self.config.nalimov_cache_mb)
                    .range(0..=65_536)
                    .suffix(" MB"),
            );
        });
        ui.add_space(8.0);
        action
    }

    fn engine_row(&mut self, ui: &mut Ui, side: TournamentSide) -> Option<TournamentAction> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new(side.label()));
            let current = match side {
                TournamentSide::White => self.config.white.clone(),
                TournamentSide::Black => self.config.black.clone(),
            };
            let choices = self.engine_choices.clone();
            let selected = if current.is_set() {
                current.name.clone()
            } else {
                "— none —".to_string()
            };
            ComboBox::from_id_salt(("tournament_engine", side.id()))
                .width(340.0)
                .selected_text(RichText::new(selected).color(theme::TEXT))
                .show_ui(ui, |ui| {
                    for choice in &choices {
                        if ui
                            .selectable_label(current.path == choice.path, &choice.name)
                            .clicked()
                        {
                            self.set_engine(side, choice.clone());
                        }
                    }
                });
            if ui
                .add(Button::new("Browse…").min_size(Vec2::new(88.0, FIELD_HEIGHT)))
                .clicked()
            {
                action = Some(TournamentAction::BrowseEngine(side));
            }
        });
        if current_is_set(&self.config, side) {
            let path = match side {
                TournamentSide::White => &self.config.white.path,
                TournamentSide::Black => &self.config.black.path,
            };
            ui.horizontal(|ui| {
                ui.add_space(LABEL_WIDTH + 6.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(path.display().to_string())
                            .size(12.0)
                            .color(theme::MUTED),
                    )
                    .truncate(),
                );
            });
        }
        ui.add_space(4.0);
        action
    }

    /// The clock fields.
    fn time_section(&mut self, ui: &mut Ui) -> Option<TournamentAction> {
        section_heading(ui, "Time control");
        ui.horizontal(|ui| {
            ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new("Mode"));
            ComboBox::from_id_salt("tournament_time_kind")
                .width(340.0)
                .selected_text(RichText::new(self.time_kind.label()).color(theme::TEXT))
                .show_ui(ui, |ui| {
                    for kind in TimeKind::ALL {
                        ui.selectable_value(&mut self.time_kind, kind, kind.label());
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new("Base time"));
            ui.add(
                DragValue::new(&mut self.base_min)
                    .range(0..=180)
                    .suffix(" min"),
            );
            ui.add(
                DragValue::new(&mut self.base_sec)
                    .range(0..=59)
                    .suffix(" s"),
            );
        });
        if self.time_kind == TimeKind::Fischer {
            ui.horizontal(|ui| {
                ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new("Increment"));
                ui.add(DragValue::new(&mut self.inc_sec).range(0..=60).suffix(" s"));
            });
        }
        let tag = time_control_tag(&self.draft_time_control());
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + 6.0);
            ui.label(
                RichText::new(format!("Clock: {}", tag))
                    .size(13.0)
                    .color(theme::MUTED),
            );
        });
        ui.add_space(8.0);
        None
    }

    /// Number of games, colours and the match title.
    fn details_section(&mut self, ui: &mut Ui) -> Option<TournamentAction> {
        section_heading(ui, "Match details");
        row(ui, "Number of games", |ui| {
            ui.add(DragValue::new(&mut self.config.games).range(1..=100_000));
        });
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + 6.0);
            ui.checkbox(
                &mut self.config.reverse_colors,
                "Reverse colours after each game",
            );
        });
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + 6.0);
            ui.checkbox(&mut self.config.enable_adjudication, "Enable adjudication");
        });
        row(ui, "Match title", |ui| {
            let id = ui.make_persistent_id("match_title");
            let selection = crate::gui::text_field::selection_before(ui, id);
            let response = ui.add(
                TextEdit::singleline(&mut self.config.event_name)
                    .id(id)
                    .desired_width(360.0)
                    .hint_text("Engine match"),
            );
            crate::gui::text_field::after_edit(
                ui,
                id,
                &mut self.config.event_name,
                selection,
                &response,
            );
        });
        ui.add_space(8.0);
        None
    }

    /// EPD opening book path and order, or a PolyGlot `.bin` book.
    fn book_section(&mut self, ui: &mut Ui) -> Option<TournamentAction> {
        let mut action = None;
        section_heading(ui, "Opening books");
        ui.horizontal(|ui| {
            ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new("Book file"));
            let id = ui.make_persistent_id("tournament_book_path");
            let selection = crate::gui::text_field::selection_before(ui, id);
            let response = ui.add(
                TextEdit::singleline(&mut self.book_path)
                    .id(id)
                    .desired_width(340.0)
                    .hint_text("optional .epd or .bin file"),
            );
            crate::gui::text_field::after_edit(ui, id, &mut self.book_path, selection, &response);
            if ui
                .add(Button::new("Browse…").min_size(Vec2::new(88.0, FIELD_HEIGHT)))
                .clicked()
            {
                action = Some(TournamentAction::BrowseBook);
            }
            if ui
                .add_enabled(!self.book_path.trim().is_empty(), Button::new("Clear"))
                .clicked()
            {
                self.book_path.clear();
            }
        });
        ui.horizontal(|ui| {
            ui.add_sized(
                [LABEL_WIDTH, FIELD_HEIGHT],
                egui::Label::new("Move book (.bin)"),
            );
            let id = ui.make_persistent_id("tournament_move_book_path");
            let selection = crate::gui::text_field::selection_before(ui, id);
            let response = ui.add(
                TextEdit::singleline(&mut self.move_book_path)
                    .id(id)
                    .desired_width(340.0)
                    .hint_text("optional PolyGlot moves"),
            );
            crate::gui::text_field::after_edit(
                ui,
                id,
                &mut self.move_book_path,
                selection,
                &response,
            );
            if ui
                .add(Button::new("Browse…").min_size(Vec2::new(88.0, FIELD_HEIGHT)))
                .clicked()
            {
                action = Some(TournamentAction::BrowseMoveBook);
            }
            if ui
                .add_enabled(!self.move_book_path.trim().is_empty(), Button::new("Clear"))
                .clicked()
            {
                self.move_book_path.clear();
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + 6.0);
            ui.label(
                RichText::new(
                    "An EPD file supplies start positions; a .bin file supplies opening moves.",
                )
                .size(12.0)
                .color(theme::MUTED),
            );
        });
        let polyglot = self.book_path.trim().to_ascii_lowercase().ends_with(".bin");
        ui.horizontal(|ui| {
            ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new("Order"));
            if polyglot {
                ui.label(
                    RichText::new("PolyGlot moves are chosen by weight.")
                        .size(12.0)
                        .color(theme::MUTED),
                );
            } else {
                ui.add_enabled_ui(!self.book_path.trim().is_empty(), |ui| {
                    ComboBox::from_id_salt("tournament_book_order")
                        .width(200.0)
                        .selected_text(match self.book_order {
                            BookOrder::Random => "Random",
                            BookOrder::Sequential => "Sequential",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.book_order, BookOrder::Random, "Random");
                            ui.selectable_value(
                                &mut self.book_order,
                                BookOrder::Sequential,
                                "Sequential",
                            );
                        });
                });
            }
        });
        ui.add_space(8.0);
        action
    }

    /// Resign and draw thresholds.
    fn adjudication_section(&mut self, ui: &mut Ui) -> Option<TournamentAction> {
        section_heading(ui, "Adjudication");
        ui.add_enabled_ui(self.config.enable_adjudication, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [LABEL_WIDTH, FIELD_HEIGHT],
                    egui::Label::new("Resign after"),
                );
                ui.add(
                    DragValue::new(&mut self.config.resign_move_count)
                        .range(0..=1_000)
                        .suffix(" moves"),
                );
                ui.label("below");
                ui.add(
                    DragValue::new(&mut self.config.resign_score)
                        .range(0..=100_000)
                        .suffix(" cp"),
                );
            });
            ui.horizontal(|ui| {
                ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new("Draw after"));
                ui.add(
                    DragValue::new(&mut self.config.draw_move_number)
                        .range(0..=1_000)
                        .suffix(" moves"),
                );
                ui.label("for");
                ui.add(
                    DragValue::new(&mut self.config.draw_move_count)
                        .range(0..=1_000)
                        .suffix(" moves"),
                )
                .on_hover_text(
                    "Half-moves (plies): each side's move counts as one. \
                     20 plies are 10 full moves.",
                );
                ui.label("within");
                ui.add(
                    DragValue::new(&mut self.config.draw_score)
                        .range(0..=100_000)
                        .suffix(" cp"),
                );
            });
        });
        ui.add_space(8.0);
        None
    }

    /// The PGN destination.
    fn pgn_section(&mut self, ui: &mut Ui) -> Option<TournamentAction> {
        let mut action = None;
        section_heading(ui, "PGN output");
        ui.horizontal(|ui| {
            ui.add_sized(
                [LABEL_WIDTH, FIELD_HEIGHT],
                egui::Label::new("Save games to"),
            );
            let id = ui.make_persistent_id("tournament_pgn_path");
            let selection = crate::gui::text_field::selection_before(ui, id);
            let response = ui.add(
                TextEdit::singleline(&mut self.pgn_path)
                    .id(id)
                    .desired_width(340.0)
                    .hint_text("~/Documents/SiliconBoard/engine-match.pgn"),
            );
            crate::gui::text_field::after_edit(ui, id, &mut self.pgn_path, selection, &response);
            if ui
                .add(Button::new("Browse…").min_size(Vec2::new(88.0, FIELD_HEIGHT)))
                .clicked()
            {
                action = Some(TournamentAction::BrowsePgn);
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + 6.0);
            ui.label(
                RichText::new(
                    "The file is replaced when the match starts; games are written in round order.",
                )
                .size(12.0)
                .color(theme::MUTED),
            );
        });
        action
    }

    /// Build the configuration the Start button would launch.
    fn build_config(&self) -> Result<TournamentConfig, String> {
        let mut config = self.config.clone();
        config.time_control = self.draft_time_control();
        config.pgn_path = ensure_pgn_extension(PathBuf::from(self.pgn_path.trim()));
        config.opening_book = if self.book_path.trim().is_empty() {
            None
        } else {
            Some(OpeningBookConfig {
                path: PathBuf::from(self.book_path.trim()),
                order: self.book_order,
            })
        };
        config.move_book = if self.move_book_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.move_book_path.trim()))
        };
        config.validate()?;
        Ok(config)
    }

    /// The clock currently described by the fields. A zero base is left as a
    /// zero clock; validation reports it.
    fn draft_time_control(&self) -> TimeControl {
        let base_ms = (u64::from(self.base_min) * 60 + u64::from(self.base_sec)) * 1000;
        let increment_ms = u64::from(self.inc_sec) * 1000;
        match self.time_kind {
            TimeKind::Fischer => TimeControl::fischer(base_ms, increment_ms),
            TimeKind::SuddenDeath => TimeControl::sudden_death(base_ms),
            TimeKind::FixedPerMove => TimeControl::fixed_per_move(base_ms),
        }
    }
}

fn current_is_set(config: &TournamentConfig, side: TournamentSide) -> bool {
    match side {
        TournamentSide::White => config.white.is_set(),
        TournamentSide::Black => config.black.is_set(),
    }
}

fn ensure_pgn_extension(path: PathBuf) -> PathBuf {
    if path.as_os_str().is_empty() || path.extension().is_some() {
        path
    } else {
        path.with_extension("pgn")
    }
}

/// One labelled row with a fixed label column.
fn row(ui: &mut Ui, label: &str, contents: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized([LABEL_WIDTH, FIELD_HEIGHT], egui::Label::new(label));
        contents(ui);
    });
}

/// What the running/finished tournament window asked for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProgressAction {
    /// Nothing.
    None,
    /// Stop the match now. Completed games stay in the PGN.
    Stop,
    /// Close the window.
    Close,
}

/// The movable tournament window shown while the match runs. It can be
/// dragged anywhere on screen.
pub fn show_progress(ctx: &egui::Context, status: &TournamentStatus) -> ProgressAction {
    let mut action = ProgressAction::None;
    let mut open = true;
    egui::Window::new("Tournament")
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .default_pos(ctx.content_rect().left_top() + egui::vec2(80.0, 80.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(RichText::new(status.summary()).size(16.0).strong());
            if let Some(message) = &status.message {
                ui.label(RichText::new(message).size(13.0).color(theme::MUTED));
            }
            if !status.current.is_empty() {
                ui.add_space(6.0);
                for game in &status.current {
                    let score = game
                        .score
                        .as_ref()
                        .map_or_else(String::new, |score| format!("  {}", score));
                    ui.label(
                        RichText::new(format!(
                            "Game {}: {} vs {} — move {}{}",
                            game.number, game.white, game.black, game.fullmove, score
                        ))
                        .size(13.0)
                        .color(theme::TEXT),
                    );
                }
            }
            ui.label(
                RichText::new(status.match_line())
                    .size(15.0)
                    .strong()
                    .color(theme::TEXT),
            );
            if !status.results.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Results so far")
                        .size(13.0)
                        .strong()
                        .color(theme::MUTED),
                );
                ScrollArea::vertical()
                    .id_salt("tournament_results")
                    .max_height(140.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for result in status.results.iter().rev() {
                            ui.label(RichText::new(result).size(12.0).color(theme::TEXT));
                        }
                    });
            }
            if !status.pgn_path.as_os_str().is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("PGN: {}", status.pgn_path.display()))
                        .size(12.0)
                        .color(theme::MUTED),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if status.running {
                    if ui
                        .add(Button::new("Stop").min_size(Vec2::new(100.0, 30.0)))
                        .clicked()
                    {
                        action = ProgressAction::Stop;
                    }
                } else if ui
                    .add(Button::new("Close").min_size(Vec2::new(100.0, 30.0)))
                    .clicked()
                {
                    action = ProgressAction::Close;
                }
            });
        });
    if !open {
        return ProgressAction::Close;
    }
    action
}

/// The small result window shown automatically when a match ends. Returns
/// `true` when the user closed it.
pub fn show_finished(ctx: &egui::Context, status: &TournamentStatus) -> bool {
    let mut close = false;
    egui::Window::new("Tournament finished")
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .default_pos(ctx.content_rect().center() - egui::vec2(210.0, 140.0))
        .show(ctx, |ui| {
            let title = if status.failed {
                "Tournament failed"
            } else if status.cancelled {
                "Tournament stopped"
            } else {
                "Tournament finished"
            };
            ui.label(RichText::new(title).size(22.0).strong().color(theme::TEXT));
            ui.add_space(6.0);
            ui.label(
                RichText::new(status.match_line())
                    .size(16.0)
                    .color(theme::TEXT),
            );
            ui.label(
                RichText::new(format!(
                    "{} of {} games played",
                    status.completed_games, status.total_games
                ))
                .size(14.0)
                .color(theme::MUTED),
            );
            if !status.results.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Results")
                        .size(13.0)
                        .strong()
                        .color(theme::MUTED),
                );
                ScrollArea::vertical()
                    .id_salt("finished_results")
                    .max_height(140.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for result in status.results.iter().rev() {
                            ui.label(RichText::new(result).size(12.0).color(theme::TEXT));
                        }
                    });
            }
            if status.failed {
                if let Some(message) = &status.message {
                    ui.add_space(4.0);
                    ui.colored_label(theme::DANGER_RED, message);
                }
            }
            if !status.pgn_path.as_os_str().is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("PGN: {}", status.pgn_path.display()))
                        .size(12.0)
                        .color(theme::MUTED),
                );
            }
            ui.add_space(10.0);
            if ui
                .add(Button::new("Close").min_size(Vec2::new(110.0, 32.0)))
                .clicked()
            {
                close = true;
            }
        });
    close
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tournament::EngineSpec;

    fn config() -> TournamentConfig {
        TournamentConfig {
            white: EngineSpec::from_path(PathBuf::from("/opt/stockfish")),
            black: EngineSpec::from_path(PathBuf::from("/opt/triumviratus")),
            ..TournamentConfig::default()
        }
    }

    #[test]
    fn the_dialog_starts_from_the_configuration() {
        let dialog = TournamentDialog::new(config(), Vec::new());
        assert_eq!(dialog.time_kind, TimeKind::Fischer);
        assert_eq!(
            (dialog.base_min, dialog.base_sec, dialog.inc_sec),
            (4, 0, 1)
        );
        assert_eq!(dialog.config.games, 2);
        assert!(dialog.config.reverse_colors);
        assert_eq!(dialog.book_order, BookOrder::Random);
        assert_eq!(dialog.engine_choices.len(), 2);
    }

    #[test]
    fn building_the_draft_applies_the_clock_and_paths() {
        let mut dialog = TournamentDialog::new(config(), Vec::new());
        dialog.base_min = 10;
        dialog.base_sec = 30;
        dialog.inc_sec = 2;
        dialog.pgn_path = "out/games".to_string();
        dialog.book_path = "openings.epd".to_string();
        dialog.book_order = BookOrder::Sequential;
        let built = dialog.build_config().unwrap();
        assert_eq!(built.time_control, TimeControl::fischer(630_000, 2_000));
        assert!(built.pgn_path.ends_with("out/games.pgn"));
        assert_eq!(
            built.opening_book,
            Some(OpeningBookConfig {
                path: PathBuf::from("openings.epd"),
                order: BookOrder::Sequential,
            })
        );
    }

    #[test]
    fn choosing_an_engine_updates_the_side() {
        let mut dialog = TournamentDialog::new(config(), Vec::new());
        let new_engine = EngineSpec::from_path(PathBuf::from("/opt/lc0"));
        dialog.set_engine(TournamentSide::Black, new_engine.clone());
        assert_eq!(dialog.config.black.path, PathBuf::from("/opt/lc0"));
        assert_eq!(dialog.engine_choices.len(), 3);
        assert_eq!(dialog.black_choice, 2);
    }

    #[test]
    fn a_missing_engine_blocks_start() {
        let mut dialog = TournamentDialog::new(TournamentConfig::default(), Vec::new());
        assert!(dialog.build_config().is_err());
        dialog.set_engine(
            TournamentSide::White,
            EngineSpec::from_path(PathBuf::from("/opt/stockfish")),
        );
        dialog.set_engine(
            TournamentSide::Black,
            EngineSpec::from_path(PathBuf::from("/opt/stockfish")),
        );
        assert!(dialog.build_config().is_ok());
    }

    #[test]
    fn the_finished_window_reports_the_result() {
        let mut status = TournamentStatus {
            total_games: 2,
            completed_games: 2,
            draws: 1,
            ..TournamentStatus::default()
        };
        status.wins.insert("Stockfish".to_string(), 1);
        status.cancelled = false;
        assert!(status.summary().contains("Tournament finished"));
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let _ = show_finished(ui.ctx(), &status);
        });
        output.textures_delta.clear();
    }

    #[test]
    fn the_dialog_lays_out_every_section() {
        let mut dialog = TournamentDialog::new(config(), Vec::new());
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let _ = dialog.show(ui.ctx());
        });
        output.textures_delta.clear();
    }

    #[test]
    fn the_running_tournament_window_lays_out() {
        let status = TournamentStatus {
            total_games: 3,
            completed_games: 1,
            draws: 1,
            wins: std::collections::BTreeMap::from([("Stockfish 19".to_string(), 1)]),
            results: vec!["Game 1: 1-0 (White wins by checkmate)".to_string()],
            running: true,
            pgn_path: PathBuf::from("games.pgn"),
            current: vec![crate::tournament::CurrentGame {
                number: 2,
                white: "Stockfish 19".to_string(),
                black: "Triumviratus".to_string(),
                ply: 12,
                fullmove: 7,
                score: Some("+0.20".to_string()),
            }],
            ..TournamentStatus::default()
        };
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let _ = show_progress(ui.ctx(), &status);
        });
        output.textures_delta.clear();
    }
}

//! Top bar, one row: Game / File / Engine menus, Start and Stop buttons, and
//! the FEN field.
//!
//! The bar only reports what the user chose as a [`MenuAction`]; the app
//! performs it. Start and Stop switch analysis on and off for every engine;
//! they never quit an engine. Each engine card has its own toggle. The FEN
//! text is owned by the app, so this module only edits it.

use eframe::egui::{self, Color32, FontId, RichText, Ui};

use crate::chess::players::{GameMode, MODES};
use crate::gui::theme;

/// Something the user asked for in the top bar.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Game → New game.
    NewGame,
    /// Game → Start from here…: two engines play the FEN in the top bar.
    StartFromFen,
    /// An engine card's pulldown: load the `choice`-th known engine into `slot`.
    ChooseEngine { slot: usize, choice: usize },
    /// Game → Flip board.
    FlipBoard,
    /// Game → one of the modes (analysis, play a colour, engine vs engine).
    SetMode(GameMode),
    /// Game → Time control…: open the dialog.
    OpenTimeControl,
    /// Game → Book moves in games: switch the book reply on or off.
    ToggleBookInGames,
    /// Game → Resign: the human side gives up the game.
    Resign,
    /// Game → Offer draw: the engine accepts or declines.
    OfferDraw,
    /// Game → Claim draw (threefold): the player to move claims the draw.
    ClaimThreefold,
    /// Engine → Engine A options… / B options… (`0` = A, `1` = B).
    OpenEngineOptions(usize),
    /// Engine → Session log: switch the session log file on or off.
    ToggleSessionLog,
    /// File → Open PGN…
    OpenPgn,
    /// File → PGN games…: show the multi-game list again.
    ShowPgnGames,
    /// File → Save PGN…
    SavePgn,
    /// File → Copy PGN.
    CopyPgn,
    /// File → Copy FEN.
    CopyFen,
    /// File → Save FEN…
    SaveFen,
    /// Engine → Load engine A… / B… (`0` = A, `1` = B).
    LoadEngine(usize),
    /// Engine → Load tablebases…: open the SyzygyPath dialog.
    OpenTablebases,
    /// Engine → Tablebases in engines: switch the configured path on or off.
    ToggleTablebases,
    /// An engine card's play/pause toggle (`0` = A, `1` = B).
    ToggleEngine(usize),
    /// Engine → Load PolyGlot book…
    LoadBook,
    /// View → Board theme: switch to the image at this index in the theme list.
    SetBoardTheme(usize),
    /// View → Piece set: switch to the set at this index.
    SetPieceSet(usize),
    /// View → Figurines in move list: toggle `♘f3` there.
    ToggleFigurinesMoveList,
    /// View → Figurines in engine evaluation: toggle `♘f3` in the PV.
    ToggleFigurinesEval,
    /// View → Analysis arrows: show or hide the engines' best-move arrows.
    ToggleAnalysisArrows,
    /// Tournament → New tournament…: open the engine-match dialog.
    OpenTournament,
    /// Tournament → Stop tournament: stop the running match.
    StopTournament,
    /// Tournament → Show tournament: reopen the tournament window.
    ShowTournament,
    /// Start: analyse the displayed position with every loaded engine.
    StartAnalysis,
    /// Stop: stop all searches. Engines stay loaded and can be started again.
    StopAnalysis,
    /// Enter in the FEN field: set the board to the FEN text.
    LoadFen,
}

/// Read-only facts the menu needs to draw its labels.
#[derive(Clone, Debug)]
pub struct MenuState<'a> {
    /// The mode the session is in; its menu entry is marked.
    pub mode: GameMode,
    /// Whether engines play opening-book moves in games.
    pub book_in_games: bool,
    /// Whether the session log file is being written.
    pub session_log: bool,
    /// `true` while a game against an engine is running, so the human can
    /// resign or offer a draw.
    pub vs_engine_live: bool,
    /// `true` when the human may claim a draw by threefold repetition now.
    pub can_claim_threefold: bool,
    /// Whether each slot's engine has answered its handshake, so its options are known.
    pub options_ready: [bool; 2],
    /// File name of the loaded opening book, if any.
    pub book_name: Option<&'a str>,
    /// The `SyzygyPath` string currently given to the engines, if any.
    pub tablebase_path: Option<&'a str>,
    /// Whether the configured tablebases are used at all.
    pub tablebases_in_engines: bool,
    /// Board images from `public/images/board/`.
    pub board_themes: &'a [crate::assets::BoardTheme],
    /// File name of the board image in use, if one has been chosen.
    pub board_theme: Option<&'a str>,
    /// Piece sets from `public/piece/`.
    pub piece_sets: &'a [crate::assets::PieceSet],
    /// Folder name of the piece set in use, if one has been chosen.
    pub piece_set: Option<&'a str>,
    /// Whether the move list draws piece letters as figurines.
    pub figurines_move_list: bool,
    /// Whether the engine PV draws piece letters as figurines.
    pub figurines_eval: bool,
    /// Whether the board shows the engines' best-move arrows.
    pub analysis_arrows: bool,
    /// Number of moves played, shown in the Game menu.
    pub ply_count: usize,
    /// Whether a tournament is currently playing games.
    pub tournament_running: bool,
    /// Whether a tournament result is available to show.
    pub tournament_has_run: bool,
    /// Whether a multi-game PGN is loaded, so the games list can be reopened.
    pub pgn_games_available: bool,
}

/// The flat toolbar look: padded outlined buttons, a slightly
/// lighter bar background and a compact 15 pt label. Submenus keep the normal
/// app style.
fn style_menu_bar(style: &mut egui::Style) {
    style.spacing.button_padding = egui::vec2(11.0, 5.0);
    style.spacing.item_spacing = egui::vec2(5.0, 0.0);
    style.spacing.interact_size.y = 28.0;
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(15.0));

    let widgets = &mut style.visuals.widgets;
    widgets.inactive.weak_bg_fill = theme::TOOLBAR_BUTTON_BG;
    widgets.inactive.bg_fill = theme::TOOLBAR_BUTTON_BG;
    widgets.inactive.bg_stroke = egui::Stroke::new(1.0, theme::TOOLBAR_STROKE);
    widgets.inactive.fg_stroke = egui::Stroke::new(1.0, theme::TEXT);
    widgets.inactive.corner_radius = egui::CornerRadius::same(4);

    widgets.hovered.weak_bg_fill = theme::TOOLBAR_HOVER;
    widgets.hovered.bg_fill = theme::TOOLBAR_HOVER;
    widgets.hovered.bg_stroke = egui::Stroke::new(1.0, theme::TOOLBAR_STROKE);
    widgets.hovered.fg_stroke = egui::Stroke::new(1.2, theme::TEXT);
    widgets.hovered.corner_radius = egui::CornerRadius::same(4);

    widgets.active.weak_bg_fill = theme::TOOLBAR_ACTIVE;
    widgets.active.bg_fill = theme::TOOLBAR_ACTIVE;
    widgets.active.bg_stroke = egui::Stroke::new(1.0, theme::BLUE);
    widgets.active.fg_stroke = egui::Stroke::new(1.2, theme::NEON_CYAN);
    widgets.active.corner_radius = egui::CornerRadius::same(4);

    widgets.open.weak_bg_fill = theme::TOOLBAR_ACTIVE;
    widgets.open.bg_fill = theme::TOOLBAR_ACTIVE;
    widgets.open.bg_stroke = egui::Stroke::new(1.0, theme::BLUE);
    widgets.open.fg_stroke = egui::Stroke::new(1.2, theme::TEXT);
    widgets.open.corner_radius = egui::CornerRadius::same(4);
}

/// Draw the top bar. `fen_input` is the FEN text shown in the field; the user
/// may edit it, and Enter loads it into the board. Cut, copy and paste work
/// in the field as in any text box. Returns the action chosen this frame.
pub fn show(ui: &mut Ui, state: &MenuState, fen_input: &mut String) -> Option<MenuAction> {
    let mut action = None;
    egui::MenuBar::new().style(style_menu_bar).ui(ui, |ui| {
        ui.menu_button("Game", |ui| {
            if ui.button("New game").clicked() {
                action = Some(MenuAction::NewGame);
                ui.close();
            }
            let start_from_fen = ui.button("Start from here…").on_hover_text(
                "Two engines play the position in the FEN field, with the current time control",
            );
            if start_from_fen.clicked() {
                action = Some(MenuAction::StartFromFen);
                ui.close();
            }
            if ui.button("Flip board").clicked() {
                action = Some(MenuAction::FlipBoard);
                ui.close();
            }
            ui.separator();
            for mode in MODES {
                if ui.radio(state.mode == mode, mode.label()).clicked() {
                    action = Some(MenuAction::SetMode(mode));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Time control…").clicked() {
                action = Some(MenuAction::OpenTimeControl);
                ui.close();
            }
            ui.separator();
            let vs_engine = state.vs_engine_live;
            if ui
                .add_enabled(vs_engine, egui::Button::new("Resign"))
                .on_disabled_hover_text("Only in a running game against an engine.")
                .clicked()
            {
                action = Some(MenuAction::Resign);
                ui.close();
            }
            if ui
                .add_enabled(vs_engine, egui::Button::new("Offer draw"))
                .on_disabled_hover_text("Only in a running game against an engine.")
                .clicked()
            {
                action = Some(MenuAction::OfferDraw);
                ui.close();
            }
            if ui
                .add_enabled(
                    state.can_claim_threefold,
                    egui::Button::new("Claim draw (threefold)"),
                )
                .on_disabled_hover_text(
                    "Available on your turn when the position has occurred three times.",
                )
                .clicked()
            {
                action = Some(MenuAction::ClaimThreefold);
                ui.close();
            }
            ui.separator();
            let book_available = state.book_name.is_some();
            let book = ui
                .add_enabled(
                    book_available,
                    egui::Button::selectable(state.book_in_games, "Book moves in games"),
                )
                .on_disabled_hover_text("Load an opening book first (Engine menu).");
            if book.clicked() {
                action = Some(MenuAction::ToggleBookInGames);
                ui.close();
            }
            ui.separator();
            ui.add_enabled(
                false,
                egui::Button::new(format!("{} half-moves played", state.ply_count)),
            );
        });
        ui.menu_button("File", |ui| {
            if ui.button("Open PGN…").clicked() {
                action = Some(MenuAction::OpenPgn);
                ui.close();
            }
            let games = ui
                .add_enabled(state.pgn_games_available, egui::Button::new("PGN games…"))
                .on_disabled_hover_text("Open a PGN with more than one game first.");
            if games.clicked() {
                action = Some(MenuAction::ShowPgnGames);
                ui.close();
            }
            if ui.button("Save PGN…").clicked() {
                action = Some(MenuAction::SavePgn);
                ui.close();
            }
            if ui.button("Copy PGN").clicked() {
                action = Some(MenuAction::CopyPgn);
                ui.close();
            }
            ui.separator();
            if ui.button("Copy FEN").clicked() {
                action = Some(MenuAction::CopyFen);
                ui.close();
            }
            if ui.button("Save FEN…").clicked() {
                action = Some(MenuAction::SaveFen);
                ui.close();
            }
        });
        ui.menu_button("Engine", |ui| {
            for slot in 0..2 {
                let letter = char::from(b'A' + slot as u8);
                // The item always reads "Load engine X…"; picking a file while
                // an engine is loaded replaces it in the same slot.
                if ui.button(format!("Load engine {}…", letter)).clicked() {
                    action = Some(MenuAction::LoadEngine(slot));
                    ui.close();
                }
            }
            ui.separator();
            for slot in 0..2 {
                let letter = char::from(b'A' + slot as u8);
                let button = ui
                    .add_enabled(
                        state.options_ready[slot],
                        egui::Button::new(format!("Engine {} options…", letter)),
                    )
                    .on_disabled_hover_text("Available once the engine has started.");
                if button.clicked() {
                    action = Some(MenuAction::OpenEngineOptions(slot));
                    ui.close();
                }
            }
            ui.separator();
            let session_log = ui
                .add(egui::Button::selectable(state.session_log, "Session log"))
                .on_hover_text("Write the session log to ~/Library/Logs/SiliconBoard");
            if session_log.clicked() {
                action = Some(MenuAction::ToggleSessionLog);
                ui.close();
            }
            ui.separator();
            let book_label = match state.book_name {
                Some(name) => format!("Opening book: {} (replace…)", name),
                None => "Load opening book…".to_string(),
            };
            if ui.button(book_label).clicked() {
                action = Some(MenuAction::LoadBook);
                ui.close();
            }
            let tablebases =
                ui.button("Load tablebases…")
                    .on_hover_text(match state.tablebase_path {
                        Some(path) => format!("Change the Syzygy folders (now: {})", path),
                        None => {
                            "Give the engines a Syzygy tablebase folder (SyzygyPath).".to_string()
                        }
                    });
            if tablebases.clicked() {
                action = Some(MenuAction::OpenTablebases);
                ui.close();
            }
            let tablebases_ready = state.tablebase_path.is_some();
            let hover = match state.tablebase_path {
                Some(path) if state.tablebases_in_engines => {
                    format!("The engines use these tablebases: {}", path)
                }
                Some(path) => format!("The engines do not use these tablebases: {}", path),
                None => "Load tablebases first (Engine menu).".to_string(),
            };
            let item = ui.add_enabled(
                tablebases_ready,
                egui::Button::selectable(state.tablebases_in_engines, "Tablebases in engines"),
            );
            let in_engines = if tablebases_ready {
                item.on_hover_text(hover)
            } else {
                item.on_disabled_hover_text("Load tablebases first (Engine menu).")
            };
            if in_engines.clicked() {
                action = Some(MenuAction::ToggleTablebases);
                ui.close();
            }
        });
        ui.menu_button("View", |ui| {
            ui.menu_button("Board theme", |ui| {
                if state.board_themes.is_empty() {
                    ui.label("No board images found");
                }
                for (index, theme) in state.board_themes.iter().enumerate() {
                    if ui
                        .radio(state.board_theme == Some(theme.file.as_str()), &theme.label)
                        .clicked()
                    {
                        action = Some(MenuAction::SetBoardTheme(index));
                        ui.close();
                    }
                }
            });
            ui.separator();
            ui.menu_button("Piece set", |ui| {
                if state.piece_sets.is_empty() {
                    ui.label("No complete piece sets found");
                }
                for (index, set) in state.piece_sets.iter().enumerate() {
                    if ui
                        .radio(state.piece_set == Some(set.name.as_str()), &set.label)
                        .clicked()
                    {
                        action = Some(MenuAction::SetPieceSet(index));
                        ui.close();
                    }
                }
            });
            ui.separator();
            if ui
                .add(egui::Button::selectable(
                    state.analysis_arrows,
                    "Analysis arrows",
                ))
                .on_hover_text("Show each engine's best move as an arrow on the board")
                .clicked()
            {
                action = Some(MenuAction::ToggleAnalysisArrows);
                ui.close();
            }
            if ui
                .add(egui::Button::selectable(
                    state.figurines_move_list,
                    "Figurines in move list",
                ))
                .on_hover_text("Show ♘f3 instead of Nf3 in the move list")
                .clicked()
            {
                action = Some(MenuAction::ToggleFigurinesMoveList);
                ui.close();
            }
            if ui
                .add(egui::Button::selectable(
                    state.figurines_eval,
                    "Figurines in engine evaluation",
                ))
                .on_hover_text("Show ♘f3 instead of Nf3 in the principal variation")
                .clicked()
            {
                action = Some(MenuAction::ToggleFigurinesEval);
                ui.close();
            }
        });
        ui.menu_button("Tournament", |ui| {
            let new = ui
                .add_enabled(
                    !state.tournament_running,
                    egui::Button::new("New tournament…"),
                )
                .on_disabled_hover_text("Stop the running tournament first.");
            if new.clicked() {
                action = Some(MenuAction::OpenTournament);
                ui.close();
            }
            let stop = ui
                .add_enabled(
                    state.tournament_running,
                    egui::Button::new("Stop tournament"),
                )
                .on_disabled_hover_text("No tournament is running.");
            if stop.clicked() {
                action = Some(MenuAction::StopTournament);
                ui.close();
            }
            let show = ui
                .add_enabled(
                    state.tournament_has_run,
                    egui::Button::new("Show tournament"),
                )
                .on_disabled_hover_text("No tournament has been run yet.");
            if show.clicked() {
                action = Some(MenuAction::ShowTournament);
                ui.close();
            }
        });

        ui.separator();
        let start = egui::Button::new(RichText::new("Start").color(Color32::BLACK).strong())
            .fill(theme::NEON_GREEN);
        if ui
            .add(start)
            .on_hover_text("Analyse the position with every loaded engine")
            .clicked()
        {
            action = Some(MenuAction::StartAnalysis);
        }
        let stop_searches = egui::Button::new(RichText::new("Stop").color(Color32::WHITE).strong())
            .fill(theme::DANGER_RED);
        if ui
            .add(stop_searches)
            .on_hover_text("Stop all searches (engines stay loaded)")
            .clicked()
        {
            action = Some(MenuAction::StopAnalysis);
        }

        ui.separator();
        ui.label(RichText::new("FEN").color(theme::MUTED).size(14.0));
        let fen_id = ui.make_persistent_id("fen_field");
        let selection = crate::gui::text_field::selection_before(ui, fen_id);
        let field = ui
            .add(
                egui::TextEdit::singleline(fen_input)
                    .id(fen_id)
                    .font(FontId::monospace(15.0))
                    .desired_width(ui.available_width().max(120.0))
                    .hint_text("paste a FEN and press Enter"),
            )
            .on_hover_text("Enter loads this position");
        crate::gui::text_field::after_edit(ui, fen_id, fen_input, selection, &field);
        // A single-line field gives up focus when Enter is pressed.
        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            action = Some(MenuAction::LoadFen);
        }
    });
    action
}

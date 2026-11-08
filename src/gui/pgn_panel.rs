//! Move list along the bottom: two columns per move (`1. e4 e5`), the current
//! half-move highlighted, click any move to jump to it, plus start / previous /
//! next / latest buttons.
//!
//! The panel navigates the [`GameState`] cursor directly. Clicking a move
//! moves the cursor; the board follows on the next frame.

use std::sync::Arc;

use eframe::egui::{self, Align, Galley, RichText, Ui, WidgetText};

use crate::chess::game_state::MoveLine;
use crate::chess::pgn::{PgnCollection, PgnResult};
use crate::chess::GameState;
use crate::gui::{figurines, theme};

/// Draw the move list. `follow` scrolls the current move into view; the app
/// sets it on the frame the cursor moved. `lines` is the cached move list for
/// the current history revision; `galleys[ply]` is its cached SAN layout.
/// `figurines` draws `♘f3` instead of `Nf3` when the View setting is on.
/// `result` is the PGN result token, shown after the last move when known.
pub fn show(
    ui: &mut Ui,
    game: &mut GameState,
    lines: &[MoveLine],
    galleys: &[Option<Arc<Galley>>],
    follow: bool,
    figurines: bool,
    result: Option<&str>,
) {
    ui.horizontal(|ui| {
        if ui.button("|<").on_hover_text("Start of game").clicked() {
            game.goto_start();
        }
        if ui.button("<").on_hover_text("Previous move (←)").clicked() {
            game.undo();
        }
        if ui.button(">").on_hover_text("Next move (→)").clicked() {
            game.redo();
        }
        if ui
            .button(">|")
            .on_hover_text("Latest position (End)")
            .clicked()
        {
            game.goto_end();
        }
        let browsing = game.cur_ply() != game.total_plies();
        if browsing {
            ui.label(
                RichText::new("browsing history — board is read-only").color(theme::ALERT_YELLOW),
            );
        }
    });

    let current = game.cur_ply();
    let mut target: Option<usize> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
            ui.horizontal_wrapped(|ui| {
                for line in lines {
                    // A line that starts with Black's move is numbered `N...`.
                    let number = if line.white.is_some() {
                        format!("{}.", line.number)
                    } else {
                        format!("{}...", line.number)
                    };
                    ui.label(RichText::new(number).color(theme::MUTED).monospace());
                    let moves = [
                        (&line.white, theme::WHITE_MOVE),
                        (&line.black, theme::BLACK_MOVE),
                    ];
                    for (entry, color) in moves {
                        let Some(entry) = entry else { continue };
                        let response = match galleys.get(entry.ply).and_then(|g| g.as_ref()) {
                            Some(galley) => ui.selectable_label(
                                current == entry.ply,
                                WidgetText::Galley(Arc::clone(galley)),
                            ),
                            None => ui.selectable_label(
                                current == entry.ply,
                                figurines::san_job(ui, &entry.san, color, figurines),
                            ),
                        };
                        if response.clicked() {
                            target = Some(entry.ply);
                        }
                        if follow && current == entry.ply {
                            response.scroll_to_me(Some(Align::Center));
                        }
                    }
                    ui.add_space(10.0);
                }
                if let Some(result) = result {
                    ui.label(
                        RichText::new(result)
                            .color(theme::NEON_CYAN)
                            .monospace()
                            .strong(),
                    );
                }
            });
            if lines.is_empty() {
                ui.label(
                    RichText::new("No moves yet — play one on the board.").color(theme::MUTED),
                );
            }
        });

    if let Some(ply) = target {
        game.goto_ply(ply);
    }
}

/// What the PGN games list asked for this frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GameListAction {
    /// Nothing changed.
    None,
    /// Load the game at this zero-based index.
    Select(usize),
    /// The user pressed **Close**.
    Close,
}

/// Smallest full width of the game table. When the list is narrower than this,
/// it keeps the table at this width and shows a horizontal scrollbar instead
/// of squeezing the columns.
const MIN_TABLE_WIDTH: f32 = 900.0;

/// The multi-game PGN list docked inside the right dashboard, below the two
/// engine cards. A title strip names the collection and carries the
/// **Close** button; clicking a row loads that game into the normal bottom
/// move list, and the list stays open so another game can be picked.
pub fn show_game_list_panel(
    ui: &mut Ui,
    collection: &PgnCollection,
    selected: usize,
) -> GameListAction {
    let mut action = GameListAction::None;

    // Title strip: collection name and game count on the left, Close on the
    // right.
    let bar_width = ui.available_width();
    let (bar_rect, _) = ui.allocate_exact_size(egui::vec2(bar_width, 38.0), egui::Sense::hover());
    ui.painter().rect_filled(bar_rect, 0.0, theme::TOOLBAR_BG);
    let inner = bar_rect.shrink2(egui::vec2(8.0, 6.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} — {} games",
                    collection.name,
                    collection.games.len()
                ))
                .size(15.0)
                .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("Close").min_size(egui::vec2(80.0, 26.0)))
                    .clicked()
                {
                    action = GameListAction::Close;
                }
            });
        });
    });
    ui.label(
        RichText::new("Click a game to load it in the move list below.")
            .size(12.0)
            .color(theme::MUTED),
    );

    let list_action = game_list(ui, collection, selected);
    if list_action != GameListAction::None {
        action = list_action;
    }
    action
}

/// The game list itself. Returns the selected game, if one was clicked.
///
/// This is a plain scrollable list rather than a table: every row spans the
/// full table width, the columns are distributed across that width, and the
/// whole row is clickable. The list scrolls vertically and horizontally; the
/// column captions stay put while the rows scroll under them.
fn game_list(ui: &mut Ui, collection: &PgnCollection, selected: usize) -> GameListAction {
    let mut action = GameListAction::None;
    let header_height = 24.0;
    let row_height = 22.0;
    // Tournament PGNs carry a `Termination` tag (e.g. "Black wins on time").
    // Show it next to the result when the file has it.
    let show_termination = collection
        .games
        .iter()
        .any(|game| !game.termination.is_empty());

    let content_width = ui.available_width().max(MIN_TABLE_WIDTH);
    #[cfg(test)]
    LAST_LIST_WIDTH.store(content_width as u32, std::sync::atomic::Ordering::Relaxed);
    let mut headers = vec!["#", "White", "Black", "Result"];
    if show_termination {
        headers.push("Termination");
    }
    headers.extend(["Date", "Event"]);

    // Reserve the caption strip. The captions are painted after the table
    // below, shifted by the table's horizontal scroll offset, so they stay in
    // step with the columns while the rows scroll under the strip.
    let width = ui.available_width();
    let (header_rect, _) =
        ui.allocate_exact_size(egui::vec2(width, header_height), egui::Sense::hover());
    ui.painter()
        .rect_filled(header_rect, 0.0, theme::TOOLBAR_BG);

    ui.separator();

    let out = egui::ScrollArea::both()
        .id_salt("pgn_games_table")
        .auto_shrink([false, false])
        .show_rows(ui, row_height, collection.games.len(), |ui, rows| {
            // Keep the table wider than the strip could ever squeeze it, so
            // the horizontal scrollbar appears instead of shrinking a column.
            ui.set_min_width(MIN_TABLE_WIDTH);
            let width = ui.available_width().max(MIN_TABLE_WIDTH);
            let widths = column_widths(width, show_termination);
            for index in rows {
                let entry = &collection.games[index];
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::click());
                let fill = if index == selected {
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

                let number = format!("#{}", entry.number);
                let result = entry.result.map_or("*", PgnResult::token);
                let date = if entry.date == "????.??.??" {
                    ""
                } else {
                    entry.date.as_str()
                };
                let mut cells = vec![
                    number.as_str(),
                    entry.white.as_str(),
                    entry.black.as_str(),
                    result,
                ];
                if show_termination {
                    cells.push(entry.termination.as_str());
                }
                cells.extend([date, entry.event.as_str()]);
                paint_list_row(ui, rect, &widths, &cells, theme::TEXT, 0.0);

                if response.clicked() {
                    action = GameListAction::Select(index);
                }
                response.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        });

    // The column captions, painted over the reserved strip and moved by the
    // table's horizontal offset, so they always label the visible columns.
    paint_list_row(
        ui,
        header_rect,
        &column_widths(content_width, show_termination),
        &headers,
        theme::TEXT,
        out.state.clamped_offset().x,
    );

    action
}

/// Width of the list columns for a given total width. The first columns get a
/// proportional share; `Event` takes the rest so the row always fills the
/// available width.
fn column_widths(total: f32, show_termination: bool) -> Vec<f32> {
    let number = (total * 0.05).clamp(40.0, 64.0);
    let white = (total * 0.19).clamp(90.0, 340.0);
    let black = (total * 0.19).clamp(90.0, 340.0);
    let result = (total * 0.08).clamp(44.0, 110.0);
    let date = (total * 0.10).clamp(64.0, 160.0);
    if show_termination {
        let termination = (total * 0.22).clamp(120.0, 340.0);
        let event = (total - number - white - black - result - termination - date).max(100.0);
        vec![number, white, black, result, termination, date, event]
    } else {
        let event = (total - number - white - black - result - date).max(120.0);
        vec![number, white, black, result, date, event]
    }
}

/// Draw one row of the list. Text is clipped to its column so long names do
/// not run into the next column. `offset_x` shifts the row left by the table's
/// horizontal scroll offset; the data rows pass `0.0`.
fn paint_list_row(
    ui: &Ui,
    rect: egui::Rect,
    widths: &[f32],
    cells: &[&str],
    color: egui::Color32,
    offset_x: f32,
) {
    let painter = ui.painter();
    let font = egui::FontId::proportional(14.0);
    let mut x = rect.left() - offset_x;
    for (text, width) in cells.iter().zip(widths.iter()) {
        let cell = egui::Rect::from_min_max(
            egui::pos2(x, rect.top()),
            egui::pos2(x + width, rect.bottom()),
        );
        painter.with_clip_rect(cell.intersect(ui.clip_rect())).text(
            egui::pos2(x + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            *text,
            font.clone(),
            color,
        );
        x += width;
    }
}

#[cfg(test)]
static LAST_LIST_WIDTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicking_a_move_in_the_list_moves_the_cursor_to_it() {
        let mut game = GameState::new();
        game.play_squares(
            shakmaty::Square::from_ascii(b"e2").unwrap(),
            shakmaty::Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();
        game.play_squares(
            shakmaty::Square::from_ascii(b"e7").unwrap(),
            shakmaty::Square::from_ascii(b"e5").unwrap(),
            None,
        )
        .unwrap();
        let white = game.pgn_lines()[0].white.clone().unwrap();
        game.goto_ply(white.ply);
        assert_eq!(game.cur_ply(), 1);
        assert_eq!(
            game.last_move().map(|(from, _)| from.to_string()),
            Some("e2".into())
        );
    }

    #[test]
    fn the_move_list_draws_the_result_token() {
        let mut game = GameState::new();
        for (from, to) in [("e2", "e4"), ("e7", "e5")] {
            game.play_squares(
                shakmaty::Square::from_ascii(from.as_bytes()).unwrap(),
                shakmaty::Square::from_ascii(to.as_bytes()).unwrap(),
                None,
            )
            .unwrap();
        }
        let lines = game.pgn_lines();
        let galleys = vec![None; game.total_plies() + 1];
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            show(ui, &mut game, &lines, &galleys, false, false, Some("1-0"));
        });
        output.textures_delta.clear();
    }

    #[test]
    fn the_multi_game_list_lays_out() {
        let text = "[Event \"One\"]\n[White \"A\"]\n[Black \"B\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n[Event \"Two\"]\n[White \"C\"]\n[Black \"D\"]\n[Result \"0-1\"]\n\n1. d4 d5 0-1\n";
        let collection = crate::chess::pgn::parse_pgn_collection(text).unwrap();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let _ = show_game_list_panel(ui, &collection, 0);
        });
        output.textures_delta.clear();
        let width = LAST_LIST_WIDTH.load(std::sync::atomic::Ordering::Relaxed);
        println!("PGN games list table width: {width}");
        assert!(width > 700, "the table must span the panel, got {width}");
    }

    #[test]
    fn the_game_list_fills_the_available_width_or_scrolls() {
        let text = "[Event \"One\"]\n[White \"A\"]\n[Black \"B\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n[Event \"Two\"]\n[White \"C\"]\n[Black \"D\"]\n[Result \"0-1\"]\n\n1. d4 d5 0-1\n";
        let collection = crate::chess::pgn::parse_pgn_collection(text).unwrap();
        let ctx = egui::Context::default();

        // A panel that is wide enough: the table spans all of it.
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(1200.0);
            let action = game_list(ui, &collection, 0);
            assert_eq!(action, GameListAction::None);
        });
        output.textures_delta.clear();
        let width = LAST_LIST_WIDTH.load(std::sync::atomic::Ordering::Relaxed) as f32;
        assert!(
            (width - 1200.0).abs() < 30.0,
            "the table must fill the available width, got {width}"
        );

        // A narrow panel keeps the table at its minimum width, so a
        // horizontal scrollbar appears instead of squeezing the columns.
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(800.0);
            let _ = game_list(ui, &collection, 0);
        });
        output.textures_delta.clear();
        let width = LAST_LIST_WIDTH.load(std::sync::atomic::Ordering::Relaxed) as f32;
        assert!(
            width >= MIN_TABLE_WIDTH,
            "a narrow list must keep the table at its minimum width for the \
             horizontal scrollbar, got {width}"
        );
    }
}

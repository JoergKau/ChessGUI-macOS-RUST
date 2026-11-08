//! Right-hand dashboard: one card per engine, the opening-book suggestions and
//! the outcome of the game.
//!
//! The panel only draws what it is given ([`EvalSummary`]); the app builds the
//! summary from the engine hosts and the game state. Score, number and move
//! formatting are pure functions with unit tests.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui,
    Vec2,
};

use crate::app::StatusLevel;
use crate::engine::{AnalysisStatus, EngineView, Score};
use crate::gui::menu_bar::MenuAction;
use crate::gui::theme;
use crate::tournament::EngineSpec;

/// Number of lines of principal variation shown. Both engine cards use it, so
/// their PV areas are the same height; the rest is cut off with "…".
const PV_LINES: usize = 2;

/// Space between the two engine cards, so their evaluations read separately.
const CARD_GAP: f32 = 32.0;

/// Narrowest the right panel may be. At this width one header line still fits:
/// toggle, engine name, speed pill, and the EVAL, DEPTH and NODES block.
pub const MIN_WIDTH: f32 = 520.0;

/// Room kept for the engine letter and the speed pill in the header. The name
/// gets what is left, and is cut short with "…" when it does not fit.
const LETTER_SLOT_WIDTH: f32 = 14.0;
const PILL_SLOT_WIDTH: f32 = 80.0;

/// Size of the EVAL / DEPTH / NODES block: each column is `METRIC_WIDTH` wide
/// with `METRIC_GAP` between columns, and the block is `METRIC_HEIGHT` tall.
const METRIC_WIDTH: f32 = 64.0;
const METRIC_GAP: f32 = 8.0;
const METRIC_HEIGHT: f32 = 44.0;

/// One engine card: the slot it fills plus its live view, if an engine is loaded.
#[derive(Clone, Debug)]
pub struct EvalCard {
    /// `0` = Engine A, `1` = Engine B.
    pub slot: usize,
    /// Engine snapshot; `None` when the slot is empty.
    pub view: Option<EngineView>,
    /// File the slot's engine was started from, for the pulldown's tick.
    pub binary_path: Option<PathBuf>,
    /// Principal variation in SAN, relative to the displayed position.
    pub pv_san: Arc<Vec<String>>,
    /// Whether analysis is switched on for this slot (the card's toggle).
    pub running: bool,
    /// Whether the toggle can act on this engine. A loaded engine that does not
    /// play in the current game is shown, but its toggle is disabled.
    pub toggle_enabled: bool,
}

impl EvalCard {
    /// A loaded engine that has not exited.
    fn is_live(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.status != AnalysisStatus::Offline)
    }
}

/// Everything the evaluation panel draws in one frame.
#[derive(Clone, Debug)]
pub struct EvalSummary {
    /// Engine cards, Engine A first.
    pub cards: [EvalCard; 2],
    /// Engines the cards' pulldowns offer: the loaded A/B files and the files
    /// used last time, as in the tournament window.
    pub engine_choices: Vec<EngineSpec>,
    /// Outcome text, e.g. `"Unfinished"` or `"Black wins by checkmate"`.
    pub outcome: String,
    /// Opening-book moves as `(SAN, weight)`, heaviest first.
    pub book_moves: Vec<(String, u16)>,
    /// Name of the loaded opening book, if any.
    pub book_name: Option<String>,
    /// Most recent ECO opening name at or before the displayed position. It
    /// stays after the line leaves the named part of the table, so the book
    /// box keeps showing the last opening.
    pub opening_name: Option<String>,
    /// Whether engines answer with book moves in games (Game → Book moves in games).
    pub book_in_games: bool,
    /// Whether piece letters in the PV are drawn as figurines.
    pub figurines: bool,
    /// Last status message of the application.
    pub status: String,
    /// Severity of the status message.
    pub status_level: StatusLevel,
}

/// Draw the dashboard. Returns the menu action requested by a card's toggle,
/// engine pulldown or Browse item, if one was clicked.
pub fn show(ui: &mut Ui, summary: &EvalSummary) -> Option<MenuAction> {
    let mut requested = None;

    ui.add_space(4.0);
    ui.label(RichText::new(&summary.outcome).color(theme::NEON_CYAN));
    ui.add_space(6.0);

    for card in &summary.cards {
        if let Some(action) = engine_card(ui, card, &summary.engine_choices, summary.figurines) {
            requested = Some(action);
        }
        ui.add_space(CARD_GAP);
    }

    // The book box belongs to book play. With Game → Book moves in games
    // switched off it is not shown at all.
    if summary.book_in_games {
        book_box(ui, summary);
    }

    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        let color = match summary.status_level {
            StatusLevel::Info => theme::MUTED,
            StatusLevel::Warning => theme::ALERT_YELLOW,
            StatusLevel::Error => theme::DANGER_RED,
        };
        let text = RichText::new(&summary.status).small().color(color);
        let text = if summary.status_level == StatusLevel::Info {
            text
        } else {
            text.strong()
        };
        ui.label(text);
    });
    requested
}

/// One engine card: a header row (toggle, engine pulldown, speed, metrics), a
/// blue rule, and the score badge with the principal variation. Returns a menu
/// action when the toggle, the pulldown or Browse is used.
fn engine_card(
    ui: &mut Ui,
    card: &EvalCard,
    choices: &[EngineSpec],
    figurines: bool,
) -> Option<MenuAction> {
    let mut action = None;
    let live = card.is_live();
    let letter = slot_letter(card.slot);
    // Convert with the turn of the position this engine searched, not the
    // game's current turn: a frozen card's score belongs to the position one
    // half-move ago and has the opposite side to move.
    let (search_white_to_move, search_fullmove) = card
        .view
        .as_ref()
        .map(|view| (view.search_white_to_move, view.search_fullmove))
        .unwrap_or((true, 1));
    egui::Frame::new()
        .fill(theme::CARD_BG)
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let toggle_enabled = live && card.toggle_enabled;
                let disabled_hint = if live {
                    "This engine is not playing in this game"
                } else {
                    "Load an engine first"
                };
                if toggle_button(ui, card.running, toggle_enabled, disabled_hint) {
                    action = Some(MenuAction::ToggleEngine(card.slot));
                }
                ui.label(
                    RichText::new(letter.to_string())
                        .small()
                        .color(theme::MUTED),
                );
                let spacing = ui.spacing().item_spacing.x;
                let name_width = if live {
                    (ui.available_width()
                        - LETTER_SLOT_WIDTH
                        - PILL_SLOT_WIDTH
                        - metrics_width()
                        - 4.0 * spacing)
                        .max(120.0)
                } else {
                    (ui.available_width() - LETTER_SLOT_WIDTH - spacing).max(120.0)
                };
                // The engine name is the same pulldown the tournament window
                // uses: known engines plus Browse….
                engine_choice_combo(ui, card, choices, name_width, &mut action);
                if let Some(view) = card.view.as_ref().filter(|_| live) {
                    speed_pill(ui, view);
                    let white_score = view
                        .stats
                        .score
                        .map(|s| white_point_of_view(s, search_white_to_move));
                    let depth = view
                        .stats
                        .depth
                        .map_or_else(|| "--".to_string(), |d| d.to_string());
                    let nodes = format_count(view.stats.nodes.unwrap_or(0));
                    let eval = white_score.map_or_else(|| "--".to_string(), format_eval_one_digit);
                    ui.add_space((ui.available_width() - metrics_width()).max(METRIC_GAP));
                    metrics_block(ui, &eval, &depth, &nodes);
                }
            });

            if !live {
                let text = match &card.view {
                    None => format!("Engine {}: no engine loaded", letter),
                    Some(_) => format!("Engine {}: offline", letter),
                };
                ui.label(RichText::new(text).color(theme::MUTED));
                return;
            }
            let Some(view) = card.view.as_ref() else {
                return;
            };

            ui.add_space(6.0);
            let (rule, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 2.0), Sense::hover());
            ui.painter().rect_filled(rule, 1.0, theme::BLUE);
            ui.add_space(6.0);

            let white_score = view
                .stats
                .score
                .map(|s| white_point_of_view(s, search_white_to_move));
            let badge = white_score.map_or_else(|| "--".to_string(), format_score_white);
            let tokens = number_pv(
                search_fullmove,
                search_white_to_move,
                card.pv_san.as_slice(),
            );
            ui.horizontal_top(|ui| {
                score_badge(ui, &badge);
                ui.add_space(4.0);
                pv_flow(ui, &tokens, figurines);
            });
        });
    action
}

/// The engine's name, or its file name when it did not give one.
/// The engine pulldown: the loaded A/B engines and the files used last time,
/// with a tick on the one the slot runs, plus Browse… for another file. This
/// is the same choice the tournament window offers.
fn engine_choice_combo(
    ui: &mut Ui,
    card: &EvalCard,
    choices: &[EngineSpec],
    width: f32,
    action: &mut Option<MenuAction>,
) {
    let selected = card
        .view
        .as_ref()
        .map(engine_name)
        .unwrap_or_else(|| "— none —".to_string());
    egui::ComboBox::from_id_salt(("engine_slot", card.slot))
        .width(width)
        .selected_text(RichText::new(selected).strong().color(theme::TEXT))
        .show_ui(ui, |ui| {
            for (index, choice) in choices.iter().enumerate() {
                let current = card.binary_path.as_deref() == Some(choice.path.as_path());
                if ui.selectable_label(current, &choice.name).clicked() {
                    *action = Some(MenuAction::ChooseEngine {
                        slot: card.slot,
                        choice: index,
                    });
                }
            }
            if !choices.is_empty() {
                ui.separator();
            }
            if ui.button("Browse…").clicked() {
                *action = Some(MenuAction::LoadEngine(card.slot));
            }
        });
}

fn engine_name(view: &EngineView) -> String {
    view.name
        .clone()
        .unwrap_or_else(|| view.binary_name.clone())
}

/// Second line of the header: the speed as a dark pill while the engine
/// searches, otherwise a word for its state.
fn speed_pill(ui: &mut Ui, view: &EngineView) {
    match view.status {
        AnalysisStatus::Thinking => {
            let nps = format_nps(view.stats.nps.unwrap_or(0));
            egui::Frame::new()
                .fill(theme::BAR_BLACK)
                .corner_radius(4.0)
                .inner_margin(3.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(nps)
                            .monospace()
                            .small()
                            .color(theme::NEON_GREEN),
                    );
                });
        }
        AnalysisStatus::Starting => {
            ui.label(RichText::new("starting…").small().color(theme::MUTED));
        }
        AnalysisStatus::Ready => {
            ui.label(RichText::new("idle").small().color(theme::MUTED));
        }
        AnalysisStatus::Offline => {}
    }
}

/// Round play/pause button drawn with the painter. Pause while the engine is
/// running, play while it is stopped. Returns `true` when clicked; a disabled
/// button never reports a click, and its tooltip gives `disabled_hint`.
fn toggle_button(ui: &mut Ui, running: bool, enabled: bool, disabled_hint: &str) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), sense);
    let (fill, ink) = match (enabled, running) {
        (false, _) => (theme::BAR_BLACK, theme::MUTED),
        (true, true) => (theme::BLUE, Color32::WHITE),
        (true, false) => (theme::BAR_BLACK, theme::NEON_GREEN),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, fill);
    let c = rect.center();
    if running {
        for x in [c.x - 5.0, c.x + 1.0] {
            let bar = Rect::from_min_size(Pos2::new(x, c.y - 7.0), Vec2::new(4.0, 14.0));
            painter.rect_filled(bar, 1.0, ink);
        }
    } else {
        let points = vec![
            Pos2::new(c.x - 4.0, c.y - 7.0),
            Pos2::new(c.x - 4.0, c.y + 7.0),
            Pos2::new(c.x + 7.0, c.y),
        ];
        painter.add(Shape::convex_polygon(points, ink, Stroke::NONE));
    }
    let hover = if !enabled {
        disabled_hint
    } else if running {
        "Stop this engine's analysis"
    } else {
        "Start this engine's analysis"
    };
    response.on_hover_text(hover).clicked()
}

/// Total width of the EVAL / DEPTH / NODES block.
fn metrics_width() -> f32 {
    3.0 * METRIC_WIDTH + 2.0 * METRIC_GAP
}

/// EVAL, DEPTH and NODES side by side, each label centred over its value.
/// The block is painted at fixed positions, so the three columns always share
/// one top line.
fn metrics_block(ui: &mut Ui, eval: &str, depth: &str, nodes: &str) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(metrics_width(), METRIC_HEIGHT), Sense::hover());
    let painter = ui.painter();
    let columns = [
        ("EVAL", eval, theme::NEON_GREEN),
        ("DEPTH", depth, theme::NEON_CYAN),
        ("NODES", nodes, theme::NEON_CYAN),
    ];
    for (i, (label, value, color)) in columns.into_iter().enumerate() {
        let x = rect.left() + i as f32 * (METRIC_WIDTH + METRIC_GAP) + METRIC_WIDTH / 2.0;
        painter.text(
            Pos2::new(x, rect.top()),
            Align2::CENTER_TOP,
            label,
            FontId::proportional(15.0),
            theme::MUTED,
        );
        painter.text(
            Pos2::new(x, rect.top() + 18.0),
            Align2::CENTER_TOP,
            value,
            FontId::proportional(17.0),
            color,
        );
    }
}

/// The evaluation from White's point of view, on a white badge.
fn score_badge(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(Color32::WHITE)
        .corner_radius(4.0)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .monospace()
                    .strong()
                    .color(Color32::BLACK),
            );
        });
}

/// The principal variation as wrapped text: move numbers are muted, moves use
/// the text colour. At most [`PV_LINES`] lines are shown; the rest is replaced
/// by "…", and the full line is available as a tooltip.
fn pv_flow(ui: &mut Ui, tokens: &[String], figurines: bool) {
    // Always reserve both PV rows, so a card does not grow from one line to
    // two (and shove the other card) when a longer PV arrives.
    let size = Vec2::new(ui.available_width(), PV_LINES as f32 * pv_row_height(ui));
    if tokens.is_empty() {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let small = ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Small)
            .cloned()
            .unwrap_or_else(|| FontId::proportional(12.0));
        ui.painter().text(
            rect.left_top(),
            Align2::LEFT_TOP,
            "PV: --",
            small,
            theme::MUTED,
        );
        return;
    }
    let job = pv_job(ui, tokens, figurines);
    // Paint the laid-out galley directly: `Label` would replace the wrap
    // settings (and with them the two-line limit) with its own.
    let galley = ui.ctx().fonts_mut(|fonts| fonts.layout_job(job));
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().galley(rect.min, galley, theme::TEXT);
    // Built only when the card is hovered, not on every frame.
    response.on_hover_ui(|ui| {
        ui.label(tokens.join(" "));
    });
}

/// Height of one PV row, measured from a probe that mixes the two fonts a row
/// may use (body text and the figurine family), so the fixed reservation for
/// [`PV_LINES`] also covers the tallest glyph mix.
fn pv_row_height(ui: &Ui) -> f32 {
    let body = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_default();
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    crate::gui::figurines::append_san(&mut job, "AgN", &body, theme::TEXT, true);
    ui.ctx().fonts_mut(|fonts| fonts.layout_job(job)).size().y
}

/// Lay the PV out as one wrapped text job, truncated after [`PV_LINES`] rows.
/// Piece letters use figurines when the setting is on.
fn pv_job(ui: &Ui, tokens: &[String], figurines: bool) -> egui::text::LayoutJob {
    let body = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_default();

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    job.wrap.max_rows = PV_LINES;
    job.wrap.overflow_character = Some('…');
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            job.append(
                " ",
                0.0,
                egui::TextFormat::simple(body.clone(), theme::MUTED),
            );
        }
        if token.ends_with('.') {
            job.append(
                token,
                0.0,
                egui::TextFormat::simple(body.clone(), theme::MUTED),
            );
            continue;
        }
        crate::gui::figurines::append_san(&mut job, token, &body, theme::TEXT, figurines);
    }
    job
}

/// Opening-book box: the heaviest book moves with their share of the weight.
/// Drawn only while book moves in games are on; otherwise it is not shown.
fn book_box(ui: &mut Ui, summary: &EvalSummary) {
    egui::Frame::new()
        .fill(theme::CARD_BG)
        .corner_radius(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let heading = match &summary.opening_name {
                Some(name) => format!("Opening book · {}", name),
                None => "Opening book".to_string(),
            };
            ui.label(RichText::new(heading).color(theme::NEON_CYAN).small());
            if summary.book_name.is_none() {
                ui.label(RichText::new("none loaded").color(theme::MUTED).small());
            } else if summary.book_moves.is_empty() {
                ui.label(RichText::new("not in book").color(theme::MUTED).small());
            }
            let total: u32 = summary.book_moves.iter().map(|(_, w)| u32::from(*w)).sum();
            for (san, weight) in summary.book_moves.iter().take(6) {
                let share = if total == 0 {
                    0.0
                } else {
                    f64::from(*weight) * 100.0 / f64::from(total)
                };
                ui.label(
                    RichText::new(format!("{:<6} {:>5.1}%", san, share))
                        .monospace()
                        .small(),
                );
            }
        });
}

/// Convert a score from the side-to-move's point of view to White's.
pub fn white_point_of_view(score: Score, white_to_move: bool) -> Score {
    if white_to_move {
        return score;
    }
    match score {
        Score::Cp(cp) => Score::Cp(-cp),
        Score::Mate(n) => Score::Mate(-n),
    }
}

/// Display text for a White-point-of-view score: `+0.45`, `-1.20`, `#3`, `-#2`.
pub fn format_score_white(score: Score) -> String {
    match score {
        Score::Cp(cp) => format!("{:+.2}", f64::from(cp) / 100.0),
        Score::Mate(0) => "#0".to_string(),
        Score::Mate(n) if n > 0 => format!("#{}", n),
        Score::Mate(n) => format!("-#{}", n.unsigned_abs()),
    }
}

/// The header's evaluation, one decimal: `+0.2`, `-1.2`, `#3`.
pub fn format_eval_one_digit(score: Score) -> String {
    match score {
        Score::Cp(cp) => format!("{:+.1}", f64::from(cp) / 100.0),
        Score::Mate(_) => format_score_white(score),
    }
}

/// Search speed: `598.7kn/s` below a million nodes per second, `1.25Mn/s` above.
pub fn format_nps(nps: u64) -> String {
    if nps < 1_000_000 {
        format!("{:.1}kn/s", nps as f64 / 1e3)
    } else {
        format!("{:.2}Mn/s", nps as f64 / 1e6)
    }
}

/// Compact count: `512`, `1.2k`, `980k`, `2.45M`, `1.20G`.
pub fn format_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.2}M", n as f64 / 1e6),
        _ => format!("{:.2}G", n as f64 / 1e9),
    }
}

/// The principal variation with move numbers: `3... Nf6 4. O-O Nxe4`. A
/// number is written before each White move, and before the first move when
/// that move is Black's. `first_number` is the displayed full-move number.
pub fn number_pv(first_number: u32, white_to_move: bool, sans: &[String]) -> Vec<String> {
    let black_first = u32::from(!white_to_move);
    let mut out = Vec::with_capacity(sans.len() * 2);
    for (i, san) in sans.iter().enumerate() {
        let i = i as u32;
        let number = first_number + (i + black_first) / 2;
        let is_white = i.is_multiple_of(2) == white_to_move;
        if is_white {
            out.push(format!("{}.", number));
        } else if i == 0 {
            out.push(format!("{}...", number));
        }
        out.push(san.clone());
    }
    out
}

/// Letter used in card titles.
fn slot_letter(slot: usize) -> char {
    char::from(b'A' + slot as u8)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sans(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn score_is_shown_from_whites_point_of_view() {
        // Black to move and Black is better by 45 cp: White sees -0.45.
        assert_eq!(white_point_of_view(Score::Cp(45), false), Score::Cp(-45));
        assert_eq!(white_point_of_view(Score::Cp(45), true), Score::Cp(45));
        assert_eq!(white_point_of_view(Score::Mate(3), false), Score::Mate(-3));
    }

    #[test]
    fn centipawn_scores_format_with_sign_and_two_decimals() {
        assert_eq!(format_score_white(Score::Cp(45)), "+0.45");
        assert_eq!(format_score_white(Score::Cp(-120)), "-1.20");
        assert_eq!(format_score_white(Score::Cp(0)), "+0.00");
    }

    #[test]
    fn mate_scores_format_as_hash_notation() {
        assert_eq!(format_score_white(Score::Mate(3)), "#3");
        assert_eq!(format_score_white(Score::Mate(-2)), "-#2");
        assert_eq!(format_score_white(Score::Mate(0)), "#0");
    }

    #[test]
    fn card_eval_has_one_decimal() {
        assert_eq!(format_eval_one_digit(Score::Cp(17)), "+0.2");
        assert_eq!(format_eval_one_digit(Score::Cp(14)), "+0.1");
        assert_eq!(format_eval_one_digit(Score::Cp(-120)), "-1.2");
        assert_eq!(format_eval_one_digit(Score::Cp(0)), "+0.0");
        assert_eq!(format_eval_one_digit(Score::Mate(3)), "#3");
        assert_eq!(format_eval_one_digit(Score::Mate(-2)), "-#2");
    }

    #[test]
    fn speed_uses_kilo_or_mega_nodes_per_second() {
        assert_eq!(format_nps(598_700), "598.7kn/s");
        assert_eq!(format_nps(1_250_000), "1.25Mn/s");
        assert_eq!(format_nps(0), "0.0kn/s");
    }

    #[test]
    fn counts_use_compact_suffixes() {
        assert_eq!(format_count(512), "512");
        assert_eq!(format_count(1_200), "1.2k");
        assert_eq!(format_count(980_000), "980.0k");
        assert_eq!(format_count(2_450_000), "2.45M");
        assert_eq!(format_count(1_200_000_000), "1.20G");
    }

    #[test]
    fn pv_numbers_start_with_white_when_white_is_to_move() {
        assert_eq!(
            number_pv(1, true, &sans(&["e4", "e5", "Nf3"])),
            sans(&["1.", "e4", "e5", "2.", "Nf3"])
        );
    }

    #[test]
    fn pv_numbers_a_black_first_line_with_dots() {
        assert_eq!(
            number_pv(3, false, &sans(&["Nf6", "O-O", "Nxe4", "d4"])),
            sans(&["3...", "Nf6", "4.", "O-O", "Nxe4", "5.", "d4"])
        );
    }

    #[test]
    fn a_long_pv_is_elided_after_two_lines() {
        let mut rows = 0;
        let mut elided = false;
        egui::__run_test_ui(|ui| {
            ui.set_width(120.0);
            let words: Vec<String> = (1..=40).map(|i| format!("Nf{}", i % 8)).collect();
            let tokens = number_pv(1, true, &words);
            let galley = ui
                .ctx()
                .fonts_mut(|fonts| fonts.layout_job(pv_job(ui, &tokens, false)));
            rows = galley.rows.len();
            elided = galley.elided;
        });
        assert_eq!(rows, PV_LINES, "the PV stops after two lines");
        assert!(elided, "the rest of the PV is replaced by an ellipsis");
    }

    #[test]
    fn engine_slots_are_lettered_a_and_b() {
        assert_eq!(slot_letter(0), 'A');
        assert_eq!(slot_letter(1), 'B');
    }
}

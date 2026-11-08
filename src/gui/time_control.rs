//! Clock labels above the board and the time-control dialog.
//!
//! The clocks are drawn from [`ClockRow`] values the app builds. The dialog
//! edits a draft and reports Apply or Cancel; the app owns the real setting.

use eframe::egui::{self, Align2, Button, DragValue, FontId, Pos2, Rect, RichText, Ui, Vec2};

use crate::chess::clock::{format_clock, TimeControl, TimeMode};
use crate::gui::theme;

/// The clock of one side, as the bar shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockFace {
    /// Time left, in milliseconds.
    pub remaining_ms: u64,
    /// The side's starting time, for the progress line.
    pub base_ms: u64,
    /// `true` while this side's clock runs (the side to move in a started game).
    pub active: bool,
    /// `true` once this side has run out of time.
    pub flagged: bool,
}

/// One player bar: a name, and the clock when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerBar {
    /// Shown on the left, e.g. `You` or `Stockfish 19`.
    pub name: String,
    /// Shown on the right, when the game has a clock.
    pub clock: Option<ClockFace>,
}

/// Height of a player bar, in points.
pub const BAR_HEIGHT: f32 = 36.0;

/// Width of the clock box on a bar.
const CLOCK_WIDTH: f32 = 104.0;

/// Split the area of the central panel into the top bar, the square board and
/// the bottom bar. The board is as large as fits, and the group is centred.
pub fn split_board_area(area: Rect) -> (Rect, Rect, Rect) {
    let side = (area.width().min(area.height() - 2.0 * BAR_HEIGHT))
        .floor()
        .max(1.0);
    let total = side + 2.0 * BAR_HEIGHT;
    let top = area.center().y - total / 2.0;
    let left = area.center().x - side / 2.0;
    let bar = |y: f32| Rect::from_min_size(Pos2::new(left, y), Vec2::new(side, BAR_HEIGHT));
    let board = Rect::from_min_size(Pos2::new(left, top + BAR_HEIGHT), Vec2::splat(side));
    (bar(top), board, bar(top + BAR_HEIGHT + side))
}

/// Paint one player bar into `rect`: the name on the left, and the clock box
/// on the right with a progress line under it. A clock that is not running is dimmed.
pub fn paint_bar(ui: &Ui, rect: Rect, bar: &PlayerBar) {
    let painter = ui.painter();
    let clock_box = bar.clock.as_ref().map(|clock| {
        let box_rect = Rect::from_min_max(
            Pos2::new(rect.right() - CLOCK_WIDTH, rect.top() + 2.0),
            Pos2::new(rect.right(), rect.bottom() - 2.0),
        );
        (clock, box_rect)
    });
    let name_colour = match &clock_box {
        Some((clock, _)) if !clock.active => theme::MUTED,
        _ => theme::TEXT,
    };
    painter.text(
        Pos2::new(rect.left() + 4.0, rect.center().y),
        Align2::LEFT_CENTER,
        &bar.name,
        FontId::proportional(17.0),
        name_colour,
    );
    let Some((clock, box_rect)) = clock_box else {
        return;
    };
    let text_colour = if clock.flagged {
        theme::DANGER_RED
    } else if clock.active {
        theme::TEXT
    } else {
        theme::MUTED
    };
    painter.rect_filled(box_rect, 4.0, theme::BAR_BLACK);
    painter.text(
        box_rect.center() - Vec2::new(0.0, 3.0),
        Align2::CENTER_CENTER,
        format_clock(clock.remaining_ms),
        FontId::monospace(20.0),
        text_colour,
    );
    // Progress line: the share of the starting time still left, blue while running.
    let fraction = if clock.base_ms == 0 {
        0.0
    } else {
        (clock.remaining_ms as f32 / clock.base_ms as f32).clamp(0.0, 1.0)
    };
    let line = Rect::from_min_max(
        Pos2::new(box_rect.left(), box_rect.bottom() - 3.0),
        Pos2::new(box_rect.right(), box_rect.bottom()),
    );
    painter.rect_filled(line, 0.0, theme::BAR_BLACK);
    let filled = Rect::from_min_max(
        line.min,
        Pos2::new(line.left() + line.width() * fraction, line.bottom()),
    );
    let colour = if clock.active {
        theme::BLUE
    } else {
        theme::MUTED
    };
    painter.rect_filled(filled, 0.0, colour);
}

/// The time-control choices offered by the dialog.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Kind {
    NoClock,
    SuddenDeath,
    Fischer,
    FixedPerMove,
}

/// What the dialog did this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Still open; nothing chosen yet.
    Open,
    /// Apply the chosen time control (`None` = no clock).
    Apply(Option<TimeControl>),
    /// Close without changing anything.
    Cancel,
}

/// Editable copy of a time control, shown in the Game → Time control dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeControlDialog {
    kind: Kind,
    base_min: u32,
    base_sec: u32,
    inc_sec: u32,
}

impl TimeControlDialog {
    /// A dialog that starts from the current setting. With no clock, the
    /// fields start at the default 5+3.
    pub fn new(current: Option<TimeControl>) -> Self {
        let default = TimeControl::default();
        let (kind, control) = match current {
            None => (Kind::NoClock, default),
            Some(control) => {
                let kind = match control.mode {
                    TimeMode::SuddenDeath => Kind::SuddenDeath,
                    TimeMode::FischerIncrement => Kind::Fischer,
                    TimeMode::FixedPerMove => Kind::FixedPerMove,
                };
                (kind, control)
            }
        };
        Self {
            kind,
            base_min: (control.base_ms / 60_000) as u32,
            base_sec: ((control.base_ms % 60_000) / 1000) as u32,
            inc_sec: (control.increment_ms / 1000) as u32,
        }
    }

    /// Draw the dialog as a window over the board and report what happened.
    pub fn show(&mut self, ctx: &egui::Context) -> DialogOutcome {
        let mut outcome = DialogOutcome::Open;
        egui::Window::new("Time control")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.radio_value(&mut self.kind, Kind::NoClock, "No clock");
                ui.radio_value(&mut self.kind, Kind::SuddenDeath, "Sudden death");
                ui.radio_value(&mut self.kind, Kind::Fischer, "Fischer increment");
                ui.radio_value(&mut self.kind, Kind::FixedPerMove, "Fixed time per move");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Base time");
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
                if self.kind == Kind::Fischer {
                    ui.horizontal(|ui| {
                        ui.label("Increment");
                        ui.add(DragValue::new(&mut self.inc_sec).range(0..=60).suffix(" s"));
                    });
                }
                if self.kind == Kind::FixedPerMove {
                    ui.label(
                        RichText::new("The engine uses the whole base time on every move.")
                            .color(theme::MUTED)
                            .small(),
                    );
                }
                ui.separator();

                let draft =
                    control_from_draft(self.kind, self.base_min, self.base_sec, self.inc_sec);
                if let Err(message) = &draft {
                    ui.colored_label(theme::DANGER_RED, *message);
                }
                ui.horizontal(|ui| {
                    let apply = ui.add_enabled(draft.is_ok(), Button::new("Apply"));
                    if apply.clicked() {
                        if let Ok(control) = draft {
                            outcome = DialogOutcome::Apply(control);
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = DialogOutcome::Cancel;
                    }
                });
            });
        outcome
    }
}

/// The time control a draft describes. `Err` names the problem when the
/// draft cannot be a clock (a zero base time).
fn control_from_draft(
    kind: Kind,
    base_min: u32,
    base_sec: u32,
    inc_sec: u32,
) -> Result<Option<TimeControl>, &'static str> {
    if kind == Kind::NoClock {
        return Ok(None);
    }
    let base_ms = (u64::from(base_min) * 60 + u64::from(base_sec)) * 1000;
    if base_ms == 0 {
        return Err("The base time must be more than zero.");
    }
    let inc_ms = u64::from(inc_sec) * 1000;
    Ok(Some(match kind {
        Kind::SuddenDeath => TimeControl::sudden_death(base_ms),
        Kind::Fischer => TimeControl::fischer(base_ms, inc_ms),
        Kind::FixedPerMove => TimeControl::fixed_per_move(base_ms),
        Kind::NoClock => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_describes_the_time_control() {
        assert_eq!(control_from_draft(Kind::NoClock, 0, 0, 0), Ok(None));
        assert_eq!(
            control_from_draft(Kind::Fischer, 5, 0, 3),
            Ok(Some(TimeControl::fischer(300_000, 3_000)))
        );
        assert_eq!(
            control_from_draft(Kind::SuddenDeath, 1, 30, 9),
            Ok(Some(TimeControl::sudden_death(90_000)))
        );
        assert_eq!(
            control_from_draft(Kind::FixedPerMove, 0, 10, 0),
            Ok(Some(TimeControl::fixed_per_move(10_000)))
        );
    }

    #[test]
    fn a_zero_base_time_is_refused() {
        assert!(control_from_draft(Kind::Fischer, 0, 0, 3).is_err());
        assert!(control_from_draft(Kind::FixedPerMove, 0, 0, 0).is_err());
    }

    #[test]
    fn dialog_starts_from_the_current_setting() {
        let dialog = TimeControlDialog::new(Some(TimeControl::fischer(300_000, 3_000)));
        assert_eq!(dialog.kind, Kind::Fischer);
        assert_eq!(
            (dialog.base_min, dialog.base_sec, dialog.inc_sec),
            (5, 0, 3)
        );
        assert_eq!(TimeControlDialog::new(None).kind, Kind::NoClock);
    }
}

//! Time-control arbiter.
//!
//! Tracks the remaining time on both sides and applies the appropriate
//! increment / fixed-time rules on each move. `app.rs` drives it in a play
//! mode: one `tap()` per ply, `start()` and `pause()` with the game's
//! state, and the `go` limits for engines come from [`Clock::go_control`].
//! The time-control dialog and the clock labels live in `gui::time_control`.
//!
//! # Properties
//! * Microsoft-style monophonic design — there is only ever one active
//!   player at a time, tracked by [`Clock::side`].
//! * Time accounting uses `std::time::Instant` for monotonic wall-clock
//!   reading; we never compare against `SystemTime` (so DST or NTP
//!   leaps have no effect).
//! * The clock is **greedy** for the active side — every read of
//!   [`Clock::remaining_ms`] reflects the live wall-clock delta since
//!   the last `tap()`. The Phase-5 game loop is expected to call
//!   `tap()` immediately after each engine / user move.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::chess::PieceColor;
use crate::engine::protocol::GoControl;

// ─────────────────────────────────────────────────────────────────────────────
// Time-control configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Time-control mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeMode {
    /// Single countdown per side. Reaching 0 forfeits the game.
    SuddenDeath,
    /// Fischer style: `base_ms + increment_ms` per move.
    FischerIncrement,
    /// Each player gets exactly `base_ms` for the whole game, with no
    /// per-move increment. Useful for blitz.
    FixedPerMove,
}

/// Configuration parameters describing a time control. Reads as a recipe;
/// turned into a live [`Clock`] with [`Clock::new`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeControl {
    pub mode: TimeMode,
    pub base_ms: u64,
    pub increment_ms: u64,
}

impl Default for TimeControl {
    /// 5-minute Fischer with a 3-second increment.
    fn default() -> Self {
        Self {
            mode: TimeMode::FischerIncrement,
            base_ms: 5 * 60 * 1000,
            increment_ms: 3 * 1000,
        }
    }
}

impl TimeControl {
    /// Construct a new time control with the given parameters.
    pub const fn new(mode: TimeMode, base_ms: u64, increment_ms: u64) -> Self {
        Self {
            mode,
            base_ms,
            increment_ms,
        }
    }

    /// Sudden death (e.g. 60 minutes, no increment).
    pub const fn sudden_death(base_ms: u64) -> Self {
        Self::new(TimeMode::SuddenDeath, base_ms, 0)
    }

    /// Fischer increment (e.g. 5+3).
    pub const fn fischer(base_ms: u64, increment_ms: u64) -> Self {
        Self::new(TimeMode::FischerIncrement, base_ms, increment_ms)
    }

    /// Fixed time per move (e.g. 10 s per move): every turn starts with the
    /// full `base_ms` allowance and unused time is not carried over.
    pub const fn fixed_per_move(base_ms: u64) -> Self {
        Self::new(TimeMode::FixedPerMove, base_ms, 0)
    }

    /// Human-readable label for the time control — used in menus and in
    /// the Phase-5 eval panel's "current game" widget.
    pub fn label(&self) -> String {
        let base_min = self.base_ms / 60_000;
        let base_sec = (self.base_ms % 60_000) / 1000;
        match self.mode {
            TimeMode::SuddenDeath => {
                if base_min >= 1 {
                    format!("Sudden Death {}m", base_min)
                } else {
                    format!("Sudden Death {}s", base_sec)
                }
            }
            TimeMode::FischerIncrement => {
                let inc_sec = self.increment_ms / 1000;
                if base_min >= 1 {
                    format!("Fischer {}+{}", base_min, inc_sec)
                } else {
                    format!("Fischer {}s +{}s", base_sec, inc_sec)
                }
            }
            TimeMode::FixedPerMove => format!("Fixed {}s per move", self.base_ms / 1000),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Live clock
// ─────────────────────────────────────────────────────────────────────────────

/// Live game clock. Constructed with [`Clock::new`].
///
/// Cloning is intentionally fine — once a clock exists, multiple widgets
/// (board label, eval dashboard, system tray) can hold owned copies; all
/// edits go through `&mut self`.
#[derive(Clone, Debug)]
pub struct Clock {
    control: TimeControl,
    /// Remaining time per side, in milliseconds.
    remaining: [u64; 2],
    /// Whose turn it currently is.
    pub side: PieceColor,
    /// Wall-clock instant the current side's turn began. None iff the
    /// clock is paused (game not yet started / explicitly paused).
    turn_started: Option<Instant>,
    /// `true` iff a side has run out of time. Frozen — further taps are
    /// no-ops until [`Clock::reset`] is called.
    expired_for: Option<PieceColor>,
}

impl Clock {
    /// Build a fresh clock from a [`TimeControl`], starting with the given
    /// side to move. By default `turn_started = None`; call
    /// [`Clock::start`] to begin countdown.
    pub fn new(control: TimeControl, starting_side: PieceColor) -> Self {
        let base_ms = control.base_ms;
        Self {
            control,
            remaining: [base_ms, base_ms],
            side: starting_side,
            turn_started: None,
            expired_for: None,
        }
    }

    /// Begin counting down. Idempotent.
    pub fn start(&mut self) {
        if self.turn_started.is_none() && self.expired_for.is_none() {
            self.turn_started = Some(Instant::now());
        }
    }

    /// Pause without switching sides; resume later with another `start`.
    pub fn pause(&mut self) {
        self.commit_active();
        self.turn_started = None;
    }

    /// Reset to the initial state of the configured time control. Used
    /// when starting a new game.
    pub fn reset(&mut self, starting_side: PieceColor) {
        let base = self.control.base_ms;
        self.remaining = [base, base];
        self.side = starting_side;
        self.expired_for = None;
        self.turn_started = None;
    }

    /// Switch the active side — apply the configured time-control rules
    /// to the side that just finished a move and start the new side's
    /// timer.
    ///
    /// # Returns
    /// `Some(side)` of the player who has run out of time, otherwise
    /// `None`.
    pub fn tap(&mut self) -> Option<PieceColor> {
        if let Some(flagged) = self.expired_for {
            return Some(flagged);
        }
        self.commit_active();

        // The side that just moved ran out of time while thinking: it loses
        // immediately and gets no increment.
        if let Some(flagged) = self.expired_for {
            self.turn_started = None;
            return Some(flagged);
        }

        // Apply increment (only Fischer-style gets it).
        if matches!(self.control.mode, TimeMode::FischerIncrement) {
            let just_played_idx = self.side.index();
            self.remaining[just_played_idx] =
                self.remaining[just_played_idx].saturating_add(self.control.increment_ms);
        }

        // Switch sides.
        self.side = self.side.opposite();
        self.turn_started = Some(Instant::now());

        // Fixed-per-move: every turn gets the full allowance, nothing carries over.
        if matches!(self.control.mode, TimeMode::FixedPerMove) {
            self.remaining[self.side.index()] = self.control.base_ms;
        }

        // Did we just expire the new active side?
        if self.remaining[self.side.index()] == 0 {
            self.expired_for = Some(self.side);
            self.turn_started = None;
            return Some(self.side);
        }
        None
    }

    /// Live remaining time for a given side, *including* the live
    /// countdown of the active side.
    pub fn remaining_ms(&self, color: PieceColor) -> u64 {
        let idx = color.index();
        let stored = self.remaining[idx];
        if color == self.side {
            if let Some(started) = self.turn_started {
                let elapsed = started.elapsed().as_millis() as u64;
                stored.saturating_sub(elapsed)
            } else {
                stored
            }
        } else {
            stored
        }
    }

    /// `Some(side)` of the player whose clock has expired, `None` if the
    /// game is still live. Stays set until [`Clock::reset`].
    pub fn flagged(&self) -> Option<PieceColor> {
        self.expired_for
    }

    /// The configured time control.
    pub fn control(&self) -> TimeControl {
        self.control
    }

    /// The `go` limits that tell an engine how to budget its next move from
    /// this clock. Fixed-per-move sends its allowance as a move time; the
    /// other modes send both remaining times and the increment.
    pub fn go_control(&self) -> GoControl {
        match self.control.mode {
            TimeMode::FixedPerMove => GoControl::MoveTime(self.control.base_ms),
            TimeMode::SuddenDeath | TimeMode::FischerIncrement => {
                let increment = match self.control.mode {
                    TimeMode::FischerIncrement => self.control.increment_ms,
                    _ => 0,
                };
                GoControl::Clock {
                    wtime: self.remaining_ms(PieceColor::White).max(1),
                    btime: self.remaining_ms(PieceColor::Black).max(1),
                    winc: increment,
                    binc: increment,
                    movetime: None,
                }
            }
        }
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /// Snapshot the live countdown back into `remaining[active]`.
    fn commit_active(&mut self) {
        if let Some(started) = self.turn_started.take() {
            let elapsed = started.elapsed().as_millis() as u64;
            let idx = self.side.index();
            self.remaining[idx] = self.remaining[idx].saturating_sub(elapsed);
            // Detect "just expired" so we can flag without further taps.
            if self.remaining[idx] == 0 {
                self.expired_for = Some(self.side);
            }
        }
    }
}

/// The clock face for `ms` milliseconds left: `05:00` from a minute up,
/// `1:02:03` from an hour up, and `00:09.5` under a
/// minute. Tenths are truncated, as on a real clock.
pub fn format_clock(ms: u64) -> String {
    if ms < 60_000 {
        return format!("00:{:02}.{}", ms / 1000, (ms % 1000) / 100);
    }
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

// Tiny extension trait on `Color` so the clock can `index()` into a `[u64; 2]`.
trait ColorIndexExt {
    fn index(self) -> usize;
}

impl ColorIndexExt for PieceColor {
    #[inline]
    fn index(self) -> usize {
        match self {
            PieceColor::White => 0,
            PieceColor::Black => 1,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn fischer_increment_adds_after_each_move() {
        let mut clock = Clock::new(TimeControl::fischer(60_000, 5_000), PieceColor::White);
        clock.start();
        // Simulate white playing for 2 seconds.
        std::thread::sleep(Duration::from_millis(20));
        // Tap while sleep is tiny so timing won't flake — we just want to
        // know that *some* time was subtracted before increment.
        clock.tap();
        // After white's first move, white hasn't run out, and increment
        // has been applied (≥ 5_000ms added). We can't reliably test
        // millisecond-precise subtraction in a test, but we can assert
        // the lower bound.
        assert!(clock.remaining_ms(PieceColor::White) >= 60_000);

        // Simulate black playing and swapping back.
        std::thread::sleep(Duration::from_millis(10));
        clock.tap();
        assert!(clock.remaining_ms(PieceColor::Black) >= 60_000);
    }

    #[test]
    fn reset_restores_initial_time() {
        let mut clock = Clock::new(TimeControl::sudden_death(120_000), PieceColor::White);
        clock.start();
        clock.remaining[0] = 5_000;
        clock.reset(PieceColor::White);
        assert_eq!(clock.remaining_ms(PieceColor::White), 120_000);
        assert_eq!(clock.flagged(), None);
    }

    #[test]
    fn label_formatting_examples() {
        let one_min_sudden = TimeControl::sudden_death(60_000);
        assert_eq!(one_min_sudden.label(), "Sudden Death 1m");
        let five_three = TimeControl::fischer(300_000, 3_000);
        assert_eq!(five_three.label(), "Fischer 5+3");
        let ten = TimeControl::sudden_death(10_000);
        assert_eq!(ten.label(), "Sudden Death 10s");
    }

    #[test]
    fn tap_reports_side_that_flagged() {
        // 10 ms of base time, 5 s increment: White overruns on the first tap.
        let mut clock = Clock::new(TimeControl::fischer(10, 5_000), PieceColor::White);
        clock.start();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(clock.tap(), Some(PieceColor::White));
        assert_eq!(clock.flagged(), Some(PieceColor::White));
        assert!(clock.flagged().is_some());
        // The increment must not revive a side that already flagged.
        assert_eq!(clock.remaining_ms(PieceColor::White), 0);
    }

    #[test]
    fn fixed_per_move_resets_clock_each_turn() {
        let mut clock = Clock::new(TimeControl::fixed_per_move(1_000), PieceColor::White);
        clock.start();
        std::thread::sleep(Duration::from_millis(30));
        clock.tap();
        // Black's clock starts a fresh 1 s turn.
        assert!(clock.remaining_ms(PieceColor::Black) >= 990);
        std::thread::sleep(Duration::from_millis(30));
        clock.tap();
        // White's clock is reset to the full per-move allowance again.
        assert!(clock.remaining_ms(PieceColor::White) >= 990);
    }

    #[test]
    fn pause_then_start_does_not_double_count() {
        let mut clock = Clock::new(TimeControl::fischer(60_000, 0), PieceColor::White);
        clock.start();
        std::thread::sleep(Duration::from_millis(20));
        clock.pause();
        let t_paused = clock.remaining_ms(PieceColor::White);

        // Sleep through a chunk that should NOT be deducted.
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(clock.remaining_ms(PieceColor::White), t_paused);

        clock.start();
        std::thread::sleep(Duration::from_millis(20));
        let t_after_resume = clock.remaining_ms(PieceColor::White);
        assert!(t_after_resume < t_paused, "expected timer to resume");
    }

    #[test]
    fn clock_face_examples() {
        assert_eq!(format_clock(300_000), "05:00");
        assert_eq!(format_clock(59_900), "00:59.9");
        assert_eq!(format_clock(3_600_000), "1:00:00");
        assert_eq!(format_clock(10_000), "00:10.0");
        assert_eq!(format_clock(9_500), "00:09.5");
        assert_eq!(format_clock(0), "00:00.0");
    }

    #[test]
    fn go_control_sends_both_clocks_and_the_increment() {
        // Not started, so the remaining times are exact.
        let clock = Clock::new(TimeControl::fischer(300_000, 3_000), PieceColor::White);
        assert_eq!(
            clock.go_control(),
            GoControl::Clock {
                wtime: 300_000,
                btime: 300_000,
                winc: 3_000,
                binc: 3_000,
                movetime: None,
            }
        );
        let sudden = Clock::new(TimeControl::sudden_death(60_000), PieceColor::White);
        assert_eq!(
            sudden.go_control(),
            GoControl::Clock {
                wtime: 60_000,
                btime: 60_000,
                winc: 0,
                binc: 0,
                movetime: None,
            }
        );
    }

    #[test]
    fn fixed_per_move_clock_sends_its_allowance_as_a_move_time() {
        let clock = Clock::new(TimeControl::fixed_per_move(10_000), PieceColor::White);
        assert_eq!(clock.go_control(), GoControl::MoveTime(10_000));
    }

    #[test]
    fn pausing_after_the_time_ran_out_flags_that_side() {
        let mut clock = Clock::new(TimeControl::fischer(10, 0), PieceColor::White);
        clock.start();
        std::thread::sleep(Duration::from_millis(30));
        clock.pause();
        assert_eq!(clock.flagged(), Some(PieceColor::White));
    }
}

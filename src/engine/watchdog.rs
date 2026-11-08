//! Engine watchdog: deadlines that catch an engine which has stopped answering
//! or stopped making progress.
//!
//! The host runs three kinds of check while an engine is *Thinking*:
//!
//! 1. **Search deadlines** (no ping needed):
//!    - after `stop`, a `bestmove` must arrive within [`Timing::stop_within`];
//!    - a move search (`go movetime`, or a clock search) must end within its own
//!      time plus [`Timing::move_grace`];
//!    - an analysis (`go infinite`) must show progress (a new depth or node count)
//!      within [`Timing::progress_within`].
//! 2. **Pings**: `isready` every [`Timing::ping_every`]. An unanswered ping is
//!    overdue after [`Timing::response_within`].
//! 3. **Hard limit**: a ping unanswered for [`Timing::hard_limit`] is a stall even
//!    if the engine keeps printing.
//!
//! Only real signs of life count: `readyok`, `uciok`, `bestmove`, and `info`
//! lines that carry a depth or a node count. Noise lines do not, so an engine
//! that prints garbage while it ignores its input is still reported.
//!
//! [`Watchdog`] is pure timing: the host asks it what to do with the current
//! instant and reports every event it sees.

use std::fmt;
use std::time::{Duration, Instant};

/// How often a ping is sent while the engine is searching.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long a ping may wait for its `readyok` before the engine is stalled.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `bestmove` may take after `stop`. Stockfish 19, Triumviratus and
/// Lizard answered within about a millisecond on the audit machine.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Time beyond a move's own limit that its `bestmove` may still take.
pub const MOVE_GRACE: Duration = Duration::from_secs(3);

/// How long an analysis may go without a new depth or node count.
pub const PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);

/// A ping unanswered this long is a stall, even while output keeps arriving.
pub const HARD_LIMIT: Duration = Duration::from_secs(30);

/// The deadlines the watchdog enforces. Tests shorten them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Time between pings.
    pub ping_every: Duration,
    /// Time a ping may wait for its answer before it is overdue.
    pub response_within: Duration,
    /// Time `bestmove` may take after `stop`.
    pub stop_within: Duration,
    /// Time beyond a move's own limit.
    pub move_grace: Duration,
    /// Time an analysis may go without progress.
    pub progress_within: Duration,
    /// Time after which an unanswered ping is a stall regardless of output.
    pub hard_limit: Duration,
}

impl Default for Timing {
    /// The production deadlines.
    fn default() -> Self {
        Self {
            ping_every: PING_INTERVAL,
            response_within: RESPONSE_TIMEOUT,
            stop_within: STOP_TIMEOUT,
            move_grace: MOVE_GRACE,
            progress_within: PROGRESS_TIMEOUT,
            hard_limit: HARD_LIMIT,
        }
    }
}

/// Why an engine was declared stalled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StallReason {
    /// A ping went unanswered (or was overdue past the hard limit).
    NoReadyOk,
    /// `stop` was sent and no `bestmove` followed in time.
    StopNotAnswered,
    /// A move search did not end within its time plus the grace.
    MoveNotAnswered,
    /// An analysis showed no new depth or nodes for too long.
    NoProgress,
}

impl fmt::Display for StallReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StallReason::NoReadyOk => write!(f, "it stopped answering isready"),
            StallReason::StopNotAnswered => write!(f, "it sent no bestmove after stop"),
            StallReason::MoveNotAnswered => write!(f, "it sent no move in time"),
            StallReason::NoProgress => write!(f, "it showed no search progress for a long time"),
        }
    }
}

/// What the host should do after a check.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Nothing to do.
    Nothing,
    /// Send `isready` now.
    Ping,
    /// The engine is stalled. Reported on every check after the first; the state stays stalled.
    Stalled(StallReason),
}

/// The search the host has started and is waiting on.
#[derive(Copy, Clone, Debug)]
struct SearchWatch {
    /// When `go` was sent.
    started: Instant,
    /// The move's own time limit, or `None` for an analysis.
    limit: Option<Duration>,
    /// The last time the search showed a new depth or node count.
    last_progress: Instant,
}

/// Timing state for one engine.
#[derive(Clone, Debug)]
pub struct Watchdog {
    /// The deadlines in force.
    timing: Timing,
    /// When the last ping was sent, if any.
    last_ping: Option<Instant>,
    /// When the ping still waiting for `readyok` was last sent, if any.
    awaiting: Option<Instant>,
    /// When the oldest unanswered ping was first sent, if any.
    unanswered_since: Option<Instant>,
    /// When the engine last showed a sign of life, if ever.
    last_output: Option<Instant>,
    /// The running search, while one is in progress.
    search: Option<SearchWatch>,
    /// When `stop` was sent for the running search, if it was.
    stop_sent: Option<Instant>,
    /// Set once the engine is declared stalled. Sticky.
    stalled: Option<StallReason>,
    /// `false` while the engine is not searching: nothing is checked then.
    active: bool,
}

impl Default for Watchdog {
    /// The production deadlines.
    fn default() -> Self {
        Self::with_timing(Timing::default())
    }
}

impl Watchdog {
    /// A watchdog with the given ping interval and response timeout, and the
    /// production values for the other deadlines. Tests use it to shorten the
    /// ping timing; the app uses [`Self::default`].
    #[cfg(test)]
    pub fn new(ping_every: Duration, response_within: Duration) -> Self {
        Self::with_timing(Timing {
            ping_every,
            response_within,
            ..Timing::default()
        })
    }

    /// A watchdog with every deadline given.
    pub fn with_timing(timing: Timing) -> Self {
        Self {
            timing,
            last_ping: None,
            awaiting: None,
            unanswered_since: None,
            last_output: None,
            search: None,
            stop_sent: None,
            stalled: None,
            active: true,
        }
    }

    /// Replace the deadlines. The running search keeps its start time.
    pub fn set_timing(&mut self, timing: Timing) {
        self.timing = timing;
    }

    /// Switch the checks on (the engine is searching) or off (it is not). Off
    /// forgets the running search, so the next one starts a fresh window.
    pub fn set_active(&mut self, active: bool) {
        if !active {
            self.idle();
        }
        self.active = active;
    }

    /// Decide what to do at `now`. Sends nothing by itself: a
    /// [`WatchdogAction::Ping`] means the caller must send `isready`.
    pub fn check(&mut self, now: Instant) -> WatchdogAction {
        if let Some(reason) = self.stalled {
            return WatchdogAction::Stalled(reason);
        }
        if !self.active {
            return WatchdogAction::Nothing;
        }
        if let Some(reason) = self.overdue_search(now) {
            return self.stall(reason);
        }
        if let Some(since) = self.unanswered_since {
            if now.duration_since(since) > self.timing.hard_limit {
                return self.stall(StallReason::NoReadyOk);
            }
        }
        if let Some(sent) = self.awaiting {
            if now.duration_since(sent) > self.timing.response_within {
                // Recent output means the engine is alive but has not reached
                // its input loop yet. Ping again; the hard limit still applies.
                let output_is_recent = self
                    .last_output
                    .is_some_and(|at| now.duration_since(at) <= self.timing.response_within);
                if output_is_recent {
                    self.awaiting = Some(now);
                    self.last_ping = Some(now);
                    return WatchdogAction::Ping;
                }
                return self.stall(StallReason::NoReadyOk);
            }
            return WatchdogAction::Nothing;
        }
        let due = match self.last_ping {
            None => true,
            Some(last) => now.duration_since(last) >= self.timing.ping_every,
        };
        if due {
            self.awaiting = Some(now);
            self.unanswered_since = Some(now);
            self.last_ping = Some(now);
            WatchdogAction::Ping
        } else {
            WatchdogAction::Nothing
        }
    }

    /// The search deadline that has passed at `now`, if any.
    fn overdue_search(&self, now: Instant) -> Option<StallReason> {
        if let Some(stop) = self.stop_sent {
            if now.duration_since(stop) > self.timing.stop_within {
                return Some(StallReason::StopNotAnswered);
            }
        }
        let search = self.search?;
        match search.limit {
            Some(limit) if now.duration_since(search.started) > limit + self.timing.move_grace => {
                Some(StallReason::MoveNotAnswered)
            }
            Some(_) => None,
            None if now.duration_since(search.last_progress) > self.timing.progress_within => {
                Some(StallReason::NoProgress)
            }
            None => None,
        }
    }

    /// Mark the engine stalled for `reason` and return that action.
    fn stall(&mut self, reason: StallReason) -> WatchdogAction {
        self.stalled = Some(reason);
        WatchdogAction::Stalled(reason)
    }

    /// Record a `readyok`. It answers the ping that is waiting, if any.
    pub fn answered(&mut self) {
        self.awaiting = None;
        self.unanswered_since = None;
    }

    /// Record a sign of life: any `readyok`, `uciok`, `bestmove` or progress line.
    pub fn note_output(&mut self, now: Instant) {
        self.last_output = Some(now);
    }

    /// Record `info` with a new depth or node count: progress, and a sign of life.
    pub fn note_progress(&mut self, now: Instant) {
        self.last_output = Some(now);
        if let Some(search) = self.search.as_mut() {
            search.last_progress = now;
        }
    }

    /// A search has been sent. `limit` is the move's own time, or `None` for an analysis.
    pub fn begin_search(&mut self, now: Instant, limit: Option<Duration>) {
        self.search = Some(SearchWatch {
            started: now,
            limit,
            last_progress: now,
        });
        self.stop_sent = None;
        self.active = true;
    }

    /// `stop` has been sent for the running search. The first one starts the clock.
    pub fn note_stop(&mut self, now: Instant) {
        if self.stop_sent.is_none() {
            self.stop_sent = Some(now);
        }
    }

    /// The running search ended with `bestmove`.
    pub fn search_finished(&mut self) {
        self.search = None;
        self.stop_sent = None;
        self.active = false;
    }

    /// Time since the running search was sent, if one is running.
    pub fn search_age(&self, now: Instant) -> Option<Duration> {
        self.search.map(|search| now.duration_since(search.started))
    }

    /// Time since `stop` was sent for the running search, if it was.
    pub fn stop_age(&self, now: Instant) -> Option<Duration> {
        self.stop_sent.map(|stop| now.duration_since(stop))
    }

    /// `true` while a `stop` is waiting for its `bestmove`.
    pub fn stop_pending(&self) -> bool {
        self.stop_sent.is_some()
    }

    /// Forget the timing of the previous search, so the next one starts a
    /// fresh window. Called while the engine is idle.
    pub fn idle(&mut self) {
        if self.stalled.is_none() {
            self.last_ping = None;
            self.awaiting = None;
            self.unanswered_since = None;
            self.last_output = None;
            self.search = None;
            self.stop_sent = None;
        }
    }

    /// `true` once the engine has been declared stalled.
    pub fn is_stalled(&self) -> bool {
        self.stalled.is_some()
    }

    /// Why the engine was declared stalled, if it was.
    pub fn stall_reason(&self) -> Option<StallReason> {
        self.stalled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn fast_timing() -> Timing {
        Timing {
            ping_every: Duration::from_millis(100),
            response_within: Duration::from_millis(300),
            stop_within: Duration::from_millis(300),
            move_grace: Duration::from_millis(300),
            progress_within: Duration::from_millis(500),
            hard_limit: Duration::from_millis(1_000),
        }
    }

    #[test]
    fn first_check_pings_then_waits_for_the_interval() {
        let base = Instant::now();
        let mut w = Watchdog::new(Duration::from_millis(100), Duration::from_millis(50));
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        w.answered();
        assert_eq!(w.check(at(base, 50)), WatchdogAction::Nothing);
        assert_eq!(w.check(at(base, 100)), WatchdogAction::Ping);
    }

    #[test]
    fn no_second_ping_while_one_is_outstanding() {
        let base = Instant::now();
        let mut w = Watchdog::new(Duration::from_millis(100), Duration::from_millis(500));
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        assert_eq!(w.check(at(base, 200)), WatchdogAction::Nothing);
        assert!(!w.is_stalled(), "still inside the response timeout");
    }

    #[test]
    fn unanswered_ping_past_the_timeout_stalls_the_engine() {
        let base = Instant::now();
        let mut w = Watchdog::new(Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        assert_eq!(
            w.check(at(base, 300)),
            WatchdogAction::Nothing,
            "exactly at the timeout is not late"
        );
        assert_eq!(
            w.check(at(base, 301)),
            WatchdogAction::Stalled(StallReason::NoReadyOk)
        );
        assert!(w.is_stalled());
        assert_eq!(
            w.check(at(base, 5_000)),
            WatchdogAction::Stalled(StallReason::NoReadyOk),
            "stalled stays stalled"
        );
    }

    #[test]
    fn recent_output_keeps_a_slow_engine_alive() {
        let base = Instant::now();
        let mut w = Watchdog::new(Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        // A real sign of life while the engine has not reached `isready` yet.
        w.note_output(at(base, 250));
        assert_eq!(
            w.check(at(base, 350)),
            WatchdogAction::Ping,
            "recent output must extend the deadline, not restart the search"
        );
        assert!(!w.is_stalled());
    }

    #[test]
    fn silence_after_the_last_output_still_stalls() {
        let base = Instant::now();
        let mut w = Watchdog::new(Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        w.note_output(at(base, 250));
        assert_eq!(w.check(at(base, 350)), WatchdogAction::Ping);
        assert_eq!(
            w.check(at(base, 700)),
            WatchdogAction::Stalled(StallReason::NoReadyOk)
        );
        assert!(w.is_stalled());
    }

    #[test]
    fn an_answer_in_time_keeps_the_engine_healthy() {
        let base = Instant::now();
        let mut w = Watchdog::new(Duration::from_millis(100), Duration::from_millis(300));
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        w.answered();
        assert_eq!(w.check(at(base, 50)), WatchdogAction::Nothing);
        assert_eq!(w.check(at(base, 100)), WatchdogAction::Ping);
        w.answered();
        assert_eq!(w.check(at(base, 150)), WatchdogAction::Nothing);
        assert!(!w.is_stalled());
    }

    #[test]
    fn a_stop_without_bestmove_stalls_even_when_isready_is_answered() {
        let base = Instant::now();
        let mut w = Watchdog::with_timing(fast_timing());
        w.begin_search(at(base, 0), None);
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        w.answered();
        w.note_stop(at(base, 50));
        assert!(w.stop_pending());
        // `readyok` keeps coming, so the pings never fail; the stop still does.
        for ms in (100..=350).step_by(50) {
            let action = w.check(at(base, ms));
            assert!(
                !matches!(action, WatchdogAction::Stalled(_)),
                "at {ms} ms: {action:?}"
            );
            w.answered();
        }
        assert_eq!(
            w.check(at(base, 351)),
            WatchdogAction::Stalled(StallReason::StopNotAnswered)
        );
    }

    #[test]
    fn a_bestmove_after_stop_clears_the_stop_deadline() {
        let base = Instant::now();
        let mut w = Watchdog::with_timing(fast_timing());
        w.begin_search(at(base, 0), None);
        w.note_stop(at(base, 10));
        w.search_finished();
        assert!(!w.stop_pending());
        assert_eq!(
            w.check(at(base, 5_000)),
            WatchdogAction::Nothing,
            "no stop deadline, and an idle engine is not pinged"
        );
        assert!(!w.is_stalled());
    }

    #[test]
    fn a_move_search_must_end_within_its_time_plus_the_grace() {
        let base = Instant::now();
        let mut w = Watchdog::with_timing(fast_timing());
        w.begin_search(at(base, 0), Some(Duration::from_millis(200)));
        w.answered();
        // Limit 200 ms + grace 300 ms = 500 ms.
        assert_ne!(
            w.check(at(base, 400)),
            WatchdogAction::Stalled(StallReason::MoveNotAnswered)
        );
        w.answered();
        assert_eq!(
            w.check(at(base, 501)),
            WatchdogAction::Stalled(StallReason::MoveNotAnswered)
        );
    }

    #[test]
    fn an_analysis_without_progress_stalls_after_the_progress_timeout() {
        let base = Instant::now();
        let mut w = Watchdog::with_timing(fast_timing());
        w.begin_search(at(base, 0), None);
        w.answered();
        w.note_progress(at(base, 200));
        w.answered();
        assert_eq!(
            w.check(at(base, 600)),
            WatchdogAction::Ping,
            "progress at 200 ms, limit 500 ms"
        );
        w.answered();
        assert_eq!(
            w.check(at(base, 701)),
            WatchdogAction::Stalled(StallReason::NoProgress)
        );
    }

    #[test]
    fn noise_cannot_keep_an_engine_alive_past_the_hard_limit() {
        let base = Instant::now();
        let mut w = Watchdog::with_timing(fast_timing());
        assert_eq!(w.check(at(base, 0)), WatchdogAction::Ping);
        // The engine prints every 100 ms but never answers `isready`.
        let mut ms = 0;
        let mut stalled_at = None;
        while ms < 2_000 {
            ms += 100;
            w.note_output(at(base, ms));
            if let WatchdogAction::Stalled(reason) = w.check(at(base, ms)) {
                assert_eq!(reason, StallReason::NoReadyOk);
                stalled_at = Some(ms);
                break;
            }
        }
        let stalled_at =
            stalled_at.expect("the hard limit must stop noise from keeping the engine alive");
        assert!(
            stalled_at > 1_000,
            "the hard limit is 1 s, got {stalled_at} ms"
        );
    }

    #[test]
    fn idle_clears_the_search_but_not_a_stall() {
        let base = Instant::now();
        let mut w = Watchdog::with_timing(fast_timing());
        w.begin_search(at(base, 0), Some(Duration::from_millis(1)));
        w.note_stop(at(base, 0));
        w.idle();
        assert!(!w.stop_pending());
        assert!(w.search_age(at(base, 10_000)).is_none());
        assert_eq!(w.check(at(base, 10_000)), WatchdogAction::Ping);
    }
}

//! Analysis state machine: keeps one engine searching the displayed position.
//!
//! [`AnalysisSession`] is pure: it takes engine events and returns the UCI
//! commands to send, with no I/O. [`crate::engine::EngineHost`] feeds it the
//! real event stream and forwards its commands to the process.
//!
//! Retargeting rule: when the position changes while the engine is searching,
//! send `stop` and wait for its `bestmove` before issuing `position` and
//! `go infinite`. Otherwise `info` lines from the old search would be shown
//! against the new position.

use std::collections::BTreeMap;

use crate::engine::protocol::{
    EngineCommand, EngineEvent, EngineStats, GoControl, Score, UciOption,
};

/// What the engine is asked to do with the target position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchGoal {
    /// Search until stopped, for the evaluation card.
    Analyse,
    /// Search with these `go` limits and play the result as the engine's move.
    Move(GoControl),
    /// Search nothing. A running search is stopped, and its result is dropped.
    Rest,
    /// Search with these limits, and keep the score for the caller. No move is played.
    Evaluate(GoControl),
}

/// What the evaluation panel shows for an engine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnalysisStatus {
    /// Process started, UCI handshake not finished yet.
    Starting,
    /// Handshake done; analysis disabled or idle.
    Ready,
    /// Searching (or stopping a search before restarting).
    Thinking,
    /// Process exited or its output stream closed.
    Offline,
}

/// Internal search phase.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// No search running.
    Idle,
    /// `go infinite` has been sent and its `info` lines are accepted.
    Searching,
    /// `stop` has been sent; waiting for `bestmove` before restarting.
    AwaitingStop,
}

/// Analysis state for one engine. See the module docs for the protocol rules.
#[derive(Debug)]
pub struct AnalysisSession {
    /// Engine name from `id name …`, set when the handshake completes.
    name: Option<String>,
    /// `true` once the engine has answered `readyok`.
    ready: bool,
    /// `true` once the engine's stream ended.
    offline: bool,
    /// Whether analysis is switched on by the user.
    enabled: bool,
    /// FEN of the position the user is looking at.
    target: Option<String>,
    /// What to do with `target`: analyse it, or search it for a move.
    goal: SearchGoal,
    /// Current search phase.
    phase: Phase,
    /// `true` while the running search was started for a move.
    search_is_move: bool,
    /// Move found by the last move search that ran to its `bestmove`.
    pending_move: Option<String>,
    /// Score of the last evaluation that ran to its `bestmove`. The outer
    /// `Option` says whether one finished; the inner one whether it had a score.
    pending_evaluation: Option<Option<Score>>,
    /// `true` while the running search was started for an evaluation.
    search_is_evaluation: bool,
    /// Statistics of the current search.
    stats: EngineStats,
    /// Commands (`ucinewgame`, `setoption`) that must wait for a running search
    /// to stop. They are sent in order after its `bestmove`.
    pending: Vec<EngineCommand>,
    /// Options the engine declared in its `uciok`.
    options: Vec<UciOption>,
    /// Option values the user chose, by name. Sent again after the engine starts.
    settings: BTreeMap<String, String>,
    /// `true` once the engine has answered `uci`.
    uciok_seen: bool,
}

impl Default for AnalysisSession {
    fn default() -> Self {
        Self::new()
    }
}

impl AnalysisSession {
    /// A session that starts with these option values (name → value).
    pub fn with_settings(settings: BTreeMap<String, String>) -> Self {
        Self {
            settings,
            ..Self::new()
        }
    }

    /// A session with no engine handshake yet and analysis switched off.
    pub fn new() -> Self {
        Self {
            name: None,
            ready: false,
            offline: false,
            enabled: false,
            target: None,
            goal: SearchGoal::Analyse,
            phase: Phase::Idle,
            search_is_move: false,
            pending_move: None,
            pending_evaluation: None,
            search_is_evaluation: false,
            stats: EngineStats::default(),
            pending: Vec::new(),
            options: Vec::new(),
            settings: BTreeMap::new(),
            uciok_seen: false,
        }
    }

    /// Commands that start the UCI handshake. Sent once when the process attaches.
    pub fn handshake(&mut self) -> Vec<EngineCommand> {
        vec![EngineCommand::Initialize]
    }

    /// A new game starts (`ucinewgame`). While a search runs, it is stopped
    /// first and `ucinewgame` goes out after its `bestmove`, because the UCI
    /// protocol does not allow `ucinewgame` during a search.
    pub fn new_game(&mut self) -> Vec<EngineCommand> {
        if self.offline {
            return Vec::new();
        }
        self.when_idle(EngineCommand::NewGame)
    }

    /// Set a UCI option. The value is kept, so it is sent again when the engine
    /// is started again. It goes out now if the engine has declared its options.
    pub fn set_option(&mut self, name: &str, value: &str) -> Vec<EngineCommand> {
        self.settings.insert(name.to_string(), value.to_string());
        if !self.uciok_seen || self.offline {
            return Vec::new();
        }
        self.when_idle(EngineCommand::SetOption {
            name: name.to_string(),
            value: Some(value.to_string()),
        })
    }

    /// Press a button option. Buttons have no value, so nothing is kept.
    pub fn press_button(&mut self, name: &str) -> Vec<EngineCommand> {
        if !self.uciok_seen || self.offline {
            return Vec::new();
        }
        self.when_idle(EngineCommand::SetOption {
            name: name.to_string(),
            value: None,
        })
    }

    /// Options the engine declared, in the order it gave them. Empty until the
    /// handshake is done.
    pub fn options(&self) -> &[UciOption] {
        &self.options
    }

    /// Send `command` now when no search is running. Otherwise queue it and
    /// stop the running search; the queue goes out after its `bestmove`.
    fn when_idle(&mut self, command: EngineCommand) -> Vec<EngineCommand> {
        if self.phase == Phase::Idle {
            return vec![command];
        }
        self.pending.push(command);
        if self.phase == Phase::Searching {
            self.phase = Phase::AwaitingStop;
            return vec![EngineCommand::Stop];
        }
        Vec::new()
    }

    /// Switch analysis on or off. Turning it off stops a running search.
    pub fn set_enabled(&mut self, enabled: bool) -> Vec<EngineCommand> {
        if self.enabled == enabled {
            return Vec::new();
        }
        self.enabled = enabled;
        if enabled {
            return self.restart_if_possible();
        }
        match self.phase {
            Phase::Searching => {
                self.phase = Phase::AwaitingStop;
                vec![EngineCommand::Stop]
            }
            Phase::Idle | Phase::AwaitingStop => Vec::new(),
        }
    }

    /// Point the engine at a position (FEN) with a goal. Repeating both is a no-op.
    /// Changing either stops a running search; its result is then discarded.
    pub fn set_target(&mut self, fen: &str, goal: SearchGoal) -> Vec<EngineCommand> {
        if self.target.as_deref() == Some(fen) && self.goal == goal {
            return Vec::new();
        }
        if self.target.as_deref() != Some(fen) {
            self.pending_move = None;
        }
        self.target = Some(fen.to_string());
        self.goal = goal;
        self.restart_if_possible()
    }

    /// The move the engine chose for the requested position, once its search
    /// has run to `bestmove`. Taking it clears it; `None` until then, and
    /// after a search that was stopped.
    pub fn take_engine_move(&mut self) -> Option<String> {
        self.pending_move.take()
    }

    /// The score of the evaluation that finished, once: `Some(None)` when it
    /// finished without a score, `None` while it runs or when none was asked for.
    pub fn take_evaluation(&mut self) -> Option<Option<Score>> {
        self.pending_evaluation.take()
    }

    /// Feed one engine event; returns the commands it triggers.
    pub fn on_event(&mut self, event: &EngineEvent) -> Vec<EngineCommand> {
        match event {
            EngineEvent::UciOk { name, options, .. } => {
                if !name.is_empty() {
                    self.name = Some(name.clone());
                }
                self.options = options.clone();
                self.uciok_seen = true;
                // Saved values go out before `isready`, for options the engine declares.
                let mut commands: Vec<EngineCommand> = self
                    .settings
                    .iter()
                    .filter(|(name, _)| self.options.iter().any(|option| &option.name == *name))
                    .map(|(name, value)| EngineCommand::SetOption {
                        name: name.clone(),
                        value: Some(value.clone()),
                    })
                    .collect();
                commands.push(EngineCommand::IsReady);
                commands
            }
            EngineEvent::ReadyOk => {
                if self.ready {
                    return Vec::new();
                }
                self.ready = true;
                self.restart_if_possible()
            }
            EngineEvent::Info(update) => {
                // Only the principal line (MultiPV 1) is shown.
                if self.phase == Phase::Searching && update.multipv.unwrap_or(1) == 1 {
                    self.stats.merge(update);
                }
                Vec::new()
            }
            EngineEvent::BestMove { best_move, .. } => match self.phase {
                // A stopped search: its move no longer fits the target, so it is dropped.
                Phase::AwaitingStop => {
                    self.phase = Phase::Idle;
                    let mut commands = self.take_pending();
                    commands.extend(self.restart_if_possible());
                    commands
                }
                Phase::Searching => {
                    self.phase = Phase::Idle;
                    if self.search_is_move {
                        self.pending_move = best_move.clone();
                    }
                    if self.search_is_evaluation {
                        self.pending_evaluation = Some(self.stats.score);
                    }
                    self.take_pending()
                }
                Phase::Idle => self.take_pending(),
            },
            EngineEvent::Terminated => {
                self.mark_offline();
                Vec::new()
            }
        }
    }

    /// Record that the engine can no longer be reached.
    pub fn mark_offline(&mut self) {
        self.offline = true;
        self.phase = Phase::Idle;
        // Nothing queued for a search or a move can be delivered any more.
        self.pending.clear();
        self.pending_move = None;
        self.pending_evaluation = None;
    }

    /// Current status for the UI badge.
    pub fn status(&self) -> AnalysisStatus {
        if self.offline {
            AnalysisStatus::Offline
        } else if !self.ready {
            AnalysisStatus::Starting
        } else if self.phase != Phase::Idle {
            AnalysisStatus::Thinking
        } else {
            AnalysisStatus::Ready
        }
    }

    /// Statistics of the current search (empty until the first `info` line).
    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// Engine name from the handshake, when known.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// `true` once the engine has answered its initial `uci` with `uciok`.
    /// An engine that exits before this is not restartable: it never proved
    /// that it speaks UCI.
    pub fn has_handshaken(&self) -> bool {
        self.uciok_seen
    }

    /// The commands queued while a search was running, in order.
    fn take_pending(&mut self) -> Vec<EngineCommand> {
        std::mem::take(&mut self.pending)
    }

    /// Start a search when everything needed is present, or ask the running
    /// search to stop so the next one can start.
    fn restart_if_possible(&mut self) -> Vec<EngineCommand> {
        if self.goal == SearchGoal::Rest {
            return match self.phase {
                Phase::Searching => {
                    self.phase = Phase::AwaitingStop;
                    vec![EngineCommand::Stop]
                }
                Phase::Idle | Phase::AwaitingStop => Vec::new(),
            };
        }
        if !self.enabled || !self.ready || self.offline || self.target.is_none() {
            return Vec::new();
        }
        match self.phase {
            Phase::Idle => self.start_search(),
            Phase::Searching => {
                self.phase = Phase::AwaitingStop;
                vec![EngineCommand::Stop]
            }
            // The pending `bestmove` will restart with the latest target.
            Phase::AwaitingStop => Vec::new(),
        }
    }

    /// Begin a search on the current target for the current goal, with cleared statistics.
    fn start_search(&mut self) -> Vec<EngineCommand> {
        let Some(fen) = self.target.clone() else {
            return Vec::new();
        };
        let go = match &self.goal {
            SearchGoal::Analyse => GoControl::Infinite,
            SearchGoal::Move(limits) => limits.clone(),
            SearchGoal::Evaluate(limits) => limits.clone(),
            SearchGoal::Rest => return Vec::new(),
        };
        self.stats = EngineStats::default();
        self.phase = Phase::Searching;
        self.search_is_move = matches!(self.goal, SearchGoal::Move(_));
        self.search_is_evaluation = matches!(self.goal, SearchGoal::Evaluate(_));
        vec![
            EngineCommand::SetPosition {
                fen: Some(fen),
                moves: Vec::new(),
            },
            EngineCommand::Go(go),
        ]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    const AFTER_E4: &str = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";

    /// A session that has completed the handshake.
    fn ready_session() -> AnalysisSession {
        let mut s = AnalysisSession::new();
        s.handshake();
        s.on_event(&EngineEvent::UciOk {
            name: "Fake 1".into(),
            author: String::new(),
            options: Vec::new(),
        });
        s.on_event(&EngineEvent::ReadyOk);
        s
    }

    fn depth_info(depth: u32, cp: i32) -> EngineEvent {
        EngineEvent::Info(EngineStats {
            depth: Some(depth),
            score: Some(Score::Cp(cp)),
            pv: vec!["e2e4".into()],
            ..EngineStats::default()
        })
    }

    #[test]
    fn handshake_requests_init_then_isready_on_uciok() {
        let mut s = AnalysisSession::new();
        assert_eq!(s.handshake(), vec![EngineCommand::Initialize]);
        assert_eq!(s.status(), AnalysisStatus::Starting);
        let cmds = s.on_event(&EngineEvent::UciOk {
            name: "Fake 1".into(),
            author: String::new(),
            options: Vec::new(),
        });
        assert_eq!(cmds, vec![EngineCommand::IsReady]);
        assert_eq!(s.name(), Some("Fake 1"));
    }

    #[test]
    fn enabling_analysis_after_readyok_starts_an_infinite_search() {
        let mut s = ready_session();
        assert_eq!(s.status(), AnalysisStatus::Ready);
        assert!(
            s.set_target(START, SearchGoal::Analyse).is_empty(),
            "analysis is still off"
        );
        let cmds = s.set_enabled(true);
        assert_eq!(
            cmds,
            vec![
                EngineCommand::SetPosition {
                    fen: Some(START.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::Infinite),
            ]
        );
        assert_eq!(s.status(), AnalysisStatus::Thinking);
    }

    #[test]
    fn target_set_before_readyok_starts_once_the_engine_is_ready() {
        let mut s = AnalysisSession::new();
        s.handshake();
        s.set_enabled(true);
        assert!(s.set_target(START, SearchGoal::Analyse).is_empty());
        let cmds = s.on_event(&EngineEvent::ReadyOk);
        assert_eq!(cmds.len(), 2);
        assert_eq!(s.status(), AnalysisStatus::Thinking);
    }

    #[test]
    fn retargeting_while_searching_stops_first_and_restarts_on_bestmove() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        s.on_event(&depth_info(5, 20));

        // The position changes: only `stop` goes out for now.
        assert_eq!(
            s.set_target(AFTER_E4, SearchGoal::Analyse),
            vec![EngineCommand::Stop]
        );
        assert_eq!(s.status(), AnalysisStatus::Thinking);

        // Late info from the old search is ignored.
        assert!(s.on_event(&depth_info(9, -999)).is_empty());
        assert_eq!(s.stats().depth, Some(5));

        // bestmove arrives: the new position is searched.
        let restart = s.on_event(&EngineEvent::BestMove {
            best_move: Some("e7e5".into()),
            ponder: None,
        });
        assert_eq!(
            restart,
            vec![
                EngineCommand::SetPosition {
                    fen: Some(AFTER_E4.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::Infinite),
            ]
        );
        assert_eq!(
            s.stats(),
            &EngineStats::default(),
            "stats reset for the new position"
        );
    }

    #[test]
    fn repeated_target_sends_nothing() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        assert!(s.set_target(START, SearchGoal::Analyse).is_empty());
    }

    #[test]
    fn info_lines_merge_into_the_running_statistics() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        s.on_event(&depth_info(4, 30));
        s.on_event(&EngineEvent::Info(EngineStats {
            depth: Some(6),
            nps: Some(1_200_000),
            ..EngineStats::default()
        }));
        assert_eq!(s.stats().depth, Some(6));
        assert_eq!(s.stats().nps, Some(1_200_000));
        assert_eq!(
            s.stats().score,
            Some(Score::Cp(30)),
            "score kept from earlier line"
        );
    }

    #[test]
    fn secondary_multipv_lines_are_ignored() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        s.on_event(&EngineEvent::Info(EngineStats {
            depth: Some(8),
            multipv: Some(2),
            score: Some(Score::Cp(-50)),
            ..EngineStats::default()
        }));
        assert_eq!(s.stats().depth, None);
    }

    #[test]
    fn disabling_analysis_stops_and_does_not_restart() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        assert_eq!(s.set_enabled(false), vec![EngineCommand::Stop]);
        let after = s.on_event(&EngineEvent::BestMove {
            best_move: Some("e2e4".into()),
            ponder: None,
        });
        assert!(after.is_empty());
        assert_eq!(s.status(), AnalysisStatus::Ready);
    }

    #[test]
    fn terminated_engine_reports_offline() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        s.on_event(&EngineEvent::Terminated);
        assert_eq!(s.status(), AnalysisStatus::Offline);
        assert!(
            s.set_target(AFTER_E4, SearchGoal::Analyse).is_empty(),
            "offline engines get no commands"
        );
    }

    #[test]
    fn new_game_during_a_search_waits_for_bestmove() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        assert_eq!(s.new_game(), vec![EngineCommand::Stop]);
        // The new position arrives while the stop is pending: nothing yet.
        assert!(s.set_target(AFTER_E4, SearchGoal::Analyse).is_empty());
        let after_stop = s.on_event(&EngineEvent::BestMove {
            best_move: Some("e2e4".into()),
            ponder: None,
        });
        assert_eq!(
            after_stop,
            vec![
                EngineCommand::NewGame,
                EngineCommand::SetPosition {
                    fen: Some(AFTER_E4.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::Infinite),
            ]
        );
    }

    #[test]
    fn new_game_is_not_sent_to_an_offline_engine() {
        let mut s = ready_session();
        s.mark_offline();
        assert!(s.new_game().is_empty());
    }

    #[test]
    fn new_game_while_idle_is_sent_immediately() {
        let mut s = ready_session();
        assert_eq!(s.new_game(), vec![EngineCommand::NewGame]);
    }

    fn spin(name: &str) -> UciOption {
        UciOption {
            name: name.into(),
            kind: crate::engine::protocol::UciOptionKind::Spin {
                default: 16,
                min: 1,
                max: 1024,
            },
        }
    }

    fn uci_ok(options: Vec<UciOption>) -> EngineEvent {
        EngineEvent::UciOk {
            name: "Fake 1".into(),
            author: String::new(),
            options,
        }
    }

    fn set(name: &str, value: &str) -> EngineCommand {
        EngineCommand::SetOption {
            name: name.into(),
            value: Some(value.into()),
        }
    }

    #[test]
    fn saved_values_are_sent_once_the_engine_declares_its_options() {
        let settings = [
            ("Hash".to_string(), "64".to_string()),
            ("Gone".to_string(), "1".to_string()),
        ];
        let mut s = AnalysisSession::with_settings(settings.into_iter().collect());
        s.handshake();
        let cmds = s.on_event(&uci_ok(vec![spin("Hash")]));
        assert_eq!(
            cmds,
            vec![set("Hash", "64"), EngineCommand::IsReady],
            "options before isready, unknown ones skipped"
        );
        assert_eq!(s.options().len(), 1);
    }

    #[test]
    fn option_set_before_the_handshake_is_only_kept() {
        let mut s = AnalysisSession::new();
        s.handshake();
        assert!(s.set_option("Hash", "128").is_empty());
        let cmds = s.on_event(&uci_ok(vec![spin("Hash")]));
        assert_eq!(cmds, vec![set("Hash", "128"), EngineCommand::IsReady]);
    }

    #[test]
    fn option_set_while_idle_is_sent_at_once() {
        let mut s = ready_session();
        assert_eq!(s.set_option("Hash", "256"), vec![set("Hash", "256")]);
    }

    #[test]
    fn option_set_during_a_search_waits_for_bestmove_then_restarts() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        assert_eq!(s.set_option("Hash", "512"), vec![EngineCommand::Stop]);
        let after_stop = s.on_event(&EngineEvent::BestMove {
            best_move: Some("e2e4".into()),
            ponder: None,
        });
        assert_eq!(
            after_stop,
            vec![
                set("Hash", "512"),
                EngineCommand::SetPosition {
                    fen: Some(START.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::Infinite),
            ]
        );
    }

    #[test]
    fn button_press_is_sent_without_a_value_and_not_kept() {
        let mut s = ready_session();
        assert_eq!(
            s.press_button("Clear Hash"),
            vec![EngineCommand::SetOption {
                name: "Clear Hash".into(),
                value: None,
            }]
        );
        // A restart sends only the saved values, never the button.
        let mut restarted = AnalysisSession::with_settings(s.settings.clone());
        restarted.handshake();
        let cmds = restarted.on_event(&uci_ok(vec![spin("Hash")]));
        assert_eq!(cmds, vec![EngineCommand::IsReady]);
    }

    #[test]
    fn options_are_not_sent_to_an_offline_engine() {
        let mut s = ready_session();
        s.mark_offline();
        assert!(s.set_option("Hash", "64").is_empty());
        assert!(s.press_button("Clear Hash").is_empty());
    }

    #[test]
    fn bestmove_none_does_not_break_the_cycle() {
        // Mate/stalemate positions answer `bestmove (none)`.
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Analyse);
        let cmds = s.on_event(&EngineEvent::BestMove {
            best_move: None,
            ponder: None,
        });
        assert!(cmds.is_empty());
        assert_eq!(s.status(), AnalysisStatus::Ready);
    }

    fn move_goal() -> SearchGoal {
        SearchGoal::Move(GoControl::MoveTime(1_000))
    }

    fn bestmove(mv: &str) -> EngineEvent {
        EngineEvent::BestMove {
            best_move: Some(mv.into()),
            ponder: None,
        }
    }

    #[test]
    fn evaluate_goal_keeps_the_score_and_plays_no_move() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Evaluate(GoControl::MoveTime(500)));
        s.on_event(&depth_info(6, 20));
        s.on_event(&bestmove("e2e4"));
        assert_eq!(s.take_evaluation(), Some(Some(Score::Cp(20))));
        assert_eq!(s.take_engine_move(), None, "an evaluation plays no move");
        assert_eq!(s.take_evaluation(), None, "an evaluation is taken once");
    }

    #[test]
    fn rest_goal_starts_nothing_and_stops_a_running_search() {
        let mut s = ready_session();
        s.set_enabled(true);
        assert!(
            s.set_target(START, SearchGoal::Rest).is_empty(),
            "resting engines do not search"
        );
        assert_eq!(s.status(), AnalysisStatus::Ready);

        s.set_target(START, SearchGoal::Analyse);
        assert_eq!(
            s.set_target(START, SearchGoal::Rest),
            vec![EngineCommand::Stop]
        );
        assert!(
            s.on_event(&bestmove("e2e4")).is_empty(),
            "a rest does not restart by itself"
        );
        assert_eq!(s.status(), AnalysisStatus::Ready);
    }

    #[test]
    fn leaving_rest_starts_the_search_for_the_new_goal() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, SearchGoal::Rest);
        assert_eq!(
            s.set_target(START, move_goal()),
            vec![
                EngineCommand::SetPosition {
                    fen: Some(START.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::MoveTime(1_000)),
            ]
        );
    }

    #[test]
    fn move_goal_searches_with_its_limit_and_yields_the_move_on_bestmove() {
        let mut s = ready_session();
        s.set_enabled(true);
        assert_eq!(
            s.set_target(START, move_goal()),
            vec![
                EngineCommand::SetPosition {
                    fen: Some(START.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::MoveTime(1_000)),
            ]
        );
        assert_eq!(s.status(), AnalysisStatus::Thinking);
        assert_eq!(s.take_engine_move(), None, "no move before bestmove");

        assert!(
            s.on_event(&bestmove("e2e4")).is_empty(),
            "a finished move search does not restart"
        );
        assert_eq!(s.status(), AnalysisStatus::Ready);
        assert_eq!(s.take_engine_move(), Some("e2e4".to_string()));
        assert_eq!(s.take_engine_move(), None, "a move is taken once");
    }

    #[test]
    fn move_goal_waits_for_the_analysis_switch_like_analysis_does() {
        let mut s = ready_session();
        assert!(
            s.set_target(START, move_goal()).is_empty(),
            "analysis is still off"
        );
        let cmds = s.set_enabled(true);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[1], EngineCommand::Go(GoControl::MoveTime(1_000)));
    }

    #[test]
    fn changing_the_position_during_a_move_search_discards_its_move() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, move_goal());
        assert_eq!(
            s.set_target(AFTER_E4, SearchGoal::Analyse),
            vec![EngineCommand::Stop]
        );

        let restart = s.on_event(&bestmove("e7e5"));
        assert_eq!(
            restart,
            vec![
                EngineCommand::SetPosition {
                    fen: Some(AFTER_E4.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::Infinite),
            ]
        );
        assert_eq!(
            s.take_engine_move(),
            None,
            "the move was found for the old position"
        );
    }

    #[test]
    fn a_move_found_before_the_position_changes_is_dropped_with_it() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, move_goal());
        s.on_event(&bestmove("e2e4"));
        s.set_target(AFTER_E4, SearchGoal::Analyse);
        assert_eq!(s.take_engine_move(), None);
    }

    #[test]
    fn abort_during_a_move_search_discards_the_move_and_start_resumes_it() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, move_goal());
        assert_eq!(s.set_enabled(false), vec![EngineCommand::Stop]);
        assert!(s.on_event(&bestmove("e2e4")).is_empty());
        assert_eq!(
            s.take_engine_move(),
            None,
            "an aborted search gives no move"
        );
        assert_eq!(s.status(), AnalysisStatus::Ready);

        let resumed = s.set_enabled(true);
        assert_eq!(
            resumed,
            vec![
                EngineCommand::SetPosition {
                    fen: Some(START.into()),
                    moves: Vec::new(),
                },
                EngineCommand::Go(GoControl::MoveTime(1_000)),
            ]
        );
    }

    #[test]
    fn start_pressed_before_the_aborted_bestmove_restarts_once_it_arrives() {
        let mut s = ready_session();
        s.set_enabled(true);
        s.set_target(START, move_goal());
        s.set_enabled(false);
        assert!(
            s.set_enabled(true).is_empty(),
            "the stopped search is still running"
        );
        let restart = s.on_event(&bestmove("e2e4"));
        assert_eq!(restart.len(), 2);
        assert_eq!(
            s.take_engine_move(),
            None,
            "the stopped search gives no move"
        );
    }
}

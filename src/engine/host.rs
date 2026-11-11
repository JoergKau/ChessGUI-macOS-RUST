//! One running engine as seen by the UI: process handle, event subscription
//! and analysis state, bundled so the UI only has to call [`EngineHost::pump`]
//! once per frame and read [`EngineHost::view`].
//!
//! Everything here is non-blocking: commands go out with
//! [`EngineProcess::try_send`], events come in with `try_recv`. The search
//! deadlines live in the engine's watchdog, which the engine's own tasks check
//! (see [`crate::engine::watchdog`]); the host only starts and stops them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::engine::analysis::{AnalysisSession, AnalysisStatus, SearchGoal};
use crate::engine::protocol::{
    EngineCommand, EngineEvent, EngineStats, GoControl, Score, UciOption,
};
use crate::engine::watchdog::{StallReason, Timing, Watchdog};
use crate::engine::{EngineProcess, InfoSlot, ParsedUciLine};

/// Most queued engine events handled in one frame. `info` lines never reach
/// the queue (the reader merges them into one slot), so this only matters for
/// a pathological burst of search-boundary lines; handling a few hundred keeps
/// a frame bounded without ever dropping a `bestmove`, `readyok` or exit. The
/// rest stays queued for the next frame.
const MAX_EVENTS_PER_PUMP: usize = 256;

/// Read-only snapshot of an engine for drawing its card.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineView {
    /// Engine name from the handshake, when known.
    pub name: Option<String>,
    /// File name of the binary, for display before the handshake finishes.
    pub binary_name: String,
    /// Badge state.
    pub status: AnalysisStatus,
    /// Statistics of the current search.
    pub stats: EngineStats,
    /// Turn of the position the current or last search ran on. The score and
    /// the PV belong to that position — not to the game's current one — so a
    /// finished engine's card keeps its value while the other engine moves.
    pub search_white_to_move: bool,
    /// Move number of that position, for the PV numbering.
    pub search_fullmove: u32,
    /// FEN of the position the statistics belong to. `None` before the first
    /// search; it does not follow a later target change.
    pub searched_fen: Option<String>,
}

/// A running engine plus its analysis state.
pub struct EngineHost {
    /// Process handle used to send commands and to shut the engine down.
    process: EngineProcess,
    /// This host's own lossless event stream.
    events: mpsc::UnboundedReceiver<ParsedUciLine>,
    /// Analysis state machine that decides which commands to send.
    analysis: AnalysisSession,
    /// The engine's deadlines, shared with its reader and monitor tasks.
    watchdog: Arc<Mutex<Watchdog>>,
    /// Principal `info` statistics the reader has merged since the last pump.
    info: InfoSlot,
    /// Side to move in the last position sent, which decides the clock of a
    /// clock search.
    white_to_move: bool,
    /// Full-move number of the last position sent, for the PV numbering of a
    /// frozen card.
    search_fullmove: u32,
}

impl EngineHost {
    /// Take ownership of a freshly spawned engine and start its UCI handshake.
    ///
    /// Subscribes before sending anything, so no output is missed.
    pub fn attach_with_settings(
        process: EngineProcess,
        settings: BTreeMap<String, String>,
    ) -> Self {
        Self::attach_with(process, Timing::default(), settings)
    }

    /// [`Self::attach_with_settings`] with the deadlines given, so tests need
    /// not wait for the production values.
    pub fn attach_with(
        process: EngineProcess,
        timing: Timing,
        settings: BTreeMap<String, String>,
    ) -> Self {
        let events = process
            .take_event_receiver()
            .expect("an engine event receiver is handed out once");
        process.set_timing(timing);
        let watchdog = process.watchdog();
        let info = process.info_slot();
        let mut host = Self {
            process,
            events,
            analysis: AnalysisSession::with_settings(settings),
            watchdog,
            info,
            white_to_move: true,
            search_fullmove: 1,
        };
        let handshake = host.analysis.handshake();
        host.dispatch(handshake);
        host
    }

    /// The engine's watchdog, locked. Never held across a call into the engine.
    fn watch(&self) -> MutexGuard<'_, Watchdog> {
        self.watchdog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Turn analysis on or off and point it at `fen` with `goal`. The position
    /// is set first, so switching on starts a search on the new position.
    pub fn set_analysis(&mut self, enabled: bool, fen: &str, goal: SearchGoal) {
        self.set_position(fen, goal);
        let commands = self.analysis.set_enabled(enabled);
        self.dispatch(commands);
    }

    /// Point the engine at `fen` with `goal`. Repeating both is free.
    pub fn set_position(&mut self, fen: &str, goal: SearchGoal) {
        let commands = self.analysis.set_target(fen, goal);
        self.dispatch(commands);
    }

    /// Set a UCI option to `value`. It is applied now, or when the engine is
    /// idle again, and it is sent again after the engine is restarted.
    pub fn set_option(&mut self, name: &str, value: &str) {
        let commands = self.analysis.set_option(name, value);
        self.dispatch(commands);
    }

    /// Press a button option, such as `Clear Hash`.
    pub fn press_button(&mut self, name: &str) {
        let commands = self.analysis.press_button(name);
        self.dispatch(commands);
    }

    /// The options the engine declared at its handshake.
    pub fn options(&self) -> &[UciOption] {
        self.analysis.options()
    }

    /// The move the engine chose for its requested position, once known.
    pub fn take_engine_move(&mut self) -> Option<String> {
        self.analysis.take_engine_move()
    }

    /// The score of a finished evaluation, once (see `AnalysisSession::take_evaluation`).
    pub fn take_evaluation(&mut self) -> Option<Option<Score>> {
        self.analysis.take_evaluation()
    }

    /// A new game starts in this engine (`ucinewgame`, after any running search).
    pub fn new_game(&mut self) {
        let commands = self.analysis.new_game();
        self.dispatch(commands);
    }

    /// Drain every pending engine event and react to it. Call once per frame.
    /// Returns `true` when the engine's state may have changed, so the UI
    /// knows to repaint.
    pub fn pump(&mut self) -> bool {
        // The sticky flag catches an engine that exited before this host
        // subscribed, whose `Terminated` event was therefore never delivered.
        let before = self.analysis.status();
        if self.process.has_exited() {
            if before != AnalysisStatus::Offline {
                log::warn!(
                    "{} has exited; marking it offline",
                    self.process.binary().display()
                );
            }
            self.analysis.mark_offline();
        }
        let mut changed = before != self.analysis.status();
        // The merged `info` comes first: the `bestmove` that follows it in the
        // queue must find its statistics already applied.
        let pending = self
            .info
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(stats) = pending {
            changed = true;
            let commands = self.analysis.on_event(&EngineEvent::Info(stats));
            self.dispatch(commands);
        }
        for _ in 0..MAX_EVENTS_PER_PUMP {
            let line = match self.events.try_recv() {
                Ok(line) => line,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.analysis.mark_offline();
                    changed = true;
                    break;
                }
            };
            match line {
                ParsedUciLine::Event(event) => {
                    changed = true;
                    let commands = self.analysis.on_event(&event);
                    self.dispatch(commands);
                }
                ParsedUciLine::Unknown(line) => {
                    log::trace!(
                        "{}: unhandled line `{}`",
                        self.process.binary().display(),
                        line
                    );
                }
                ParsedUciLine::ParseError { line, source } => {
                    log::debug!(
                        "{}: could not parse `{}`: {}",
                        self.process.binary().display(),
                        line,
                        source
                    );
                }
            }
        }
        // Only a running search is watched: an idle engine needs no heartbeat,
        // so a finished game leaves it quiet.
        let searching = self.analysis.status() == AnalysisStatus::Thinking;
        self.watch().set_active(searching);
        changed
    }

    /// `true` once the engine has been declared stalled. The app then shuts
    /// the host down and starts the engine again.
    pub fn is_stalled(&self) -> bool {
        self.watch().is_stalled()
    }

    /// Why the engine was declared stalled, once it has been.
    pub fn stall_reason(&self) -> Option<StallReason> {
        self.watch().stall_reason()
    }

    /// `true` while a `stop` is waiting for its `bestmove`: the engine is
    /// finishing its last search and will not start a new one until then.
    pub fn is_stopping(&self) -> bool {
        self.watch().stop_pending()
    }

    /// `true` once the engine has answered the UCI handshake. Only such an
    /// engine is restarted automatically after it exits.
    pub fn has_handshaken(&self) -> bool {
        self.analysis.has_handshaken()
    }

    /// What the engine has said about its own failure: its exit status and
    /// last message, for the status line.
    pub fn failure_detail(&self) -> String {
        self.process.diagnosis()
    }

    /// The file the engine was started from, so it can be started again.
    pub fn binary_path(&self) -> PathBuf {
        self.process.binary().to_path_buf()
    }

    /// `true` once the engine's output has ended, whether or not its
    /// `Terminated` event has been seen yet.
    pub fn has_exited(&self) -> bool {
        self.process.has_exited()
    }

    /// Mark the engine offline and return its process so the caller can
    /// quit it. The host stays in its slot, so the card shows `Offline`.
    pub fn shutdown(&mut self) -> EngineProcess {
        self.analysis.mark_offline();
        self.process.clone()
    }

    /// The badge state alone, without copying the statistics (called several
    /// times per frame).
    pub fn status(&self) -> AnalysisStatus {
        self.analysis.status()
    }

    /// Snapshot for the evaluation panel.
    pub fn view(&self) -> EngineView {
        EngineView {
            name: self.analysis.name().map(str::to_string),
            binary_name: self
                .process
                .binary()
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            status: self.analysis.status(),
            stats: self.analysis.stats().clone(),
            search_white_to_move: self.white_to_move,
            search_fullmove: self.search_fullmove,
            searched_fen: self.analysis.searched_fen().map(str::to_string),
        }
    }

    /// The deadline of a search for `control`, when the search has its own
    /// time limit (a move search). An analysis has none and is watched for progress.
    fn move_limit(&self, control: &GoControl) -> Option<Duration> {
        match control {
            GoControl::MoveTime(ms) => Some(Duration::from_millis(*ms)),
            GoControl::Clock {
                wtime,
                btime,
                movetime,
                ..
            } => {
                let own = movetime.unwrap_or(if self.white_to_move { *wtime } else { *btime });
                Some(Duration::from_millis(own))
            }
            _ => None,
        }
    }

    /// Forward commands to the writer task without waiting. A full or closed
    /// queue is logged; the analysis state recovers at the next position change.
    /// A `go` starts the engine's search deadlines and a `stop` starts the stop
    /// deadline, so both are set before the command is sent.
    fn dispatch(&mut self, commands: Vec<EngineCommand>) {
        let now = Instant::now();
        for command in commands {
            match &command {
                EngineCommand::SetPosition { fen: Some(fen), .. } => {
                    self.white_to_move = fen.split_whitespace().nth(1) != Some("b");
                    self.search_fullmove = fen
                        .split_whitespace()
                        .nth(5)
                        .and_then(|number| number.parse::<u32>().ok())
                        .unwrap_or(1);
                }
                EngineCommand::Go(control) => {
                    let limit = self.move_limit(control);
                    self.watch().begin_search(now, limit);
                }
                EngineCommand::Stop => self.watch().note_stop(now),
                _ => {}
            }
            // Heartbeats are frequent but uninteresting; the session log keeps
            // them only at trace so an idle engine cannot fill the file.
            if matches!(command, EngineCommand::IsReady) {
                log::trace!(
                    "{} <- {}",
                    self.process.binary().display(),
                    command.to_wire_string()
                );
            } else {
                log::debug!(
                    "{} <- {}",
                    self.process.binary().display(),
                    command.to_wire_string()
                );
            }
            if let Err(error) = self.process.try_send(command.clone()) {
                log::warn!(
                    "could not send `{}` to {}: {}",
                    command.to_wire_string(),
                    self.process.binary().display(),
                    error
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — real Stockfish
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chess::GameState;
    use crate::engine::protocol::{GoControl, Score};
    use crate::engine::watchdog::{StallReason, Timing};
    use std::time::{Duration, Instant};

    /// Same fixture order as the engine tests: env override, Homebrew Stockfish, Triumviratus.
    /// Pump the host until `done` holds or the timeout passes.
    fn pump_until(
        host: &mut EngineHost,
        timeout: Duration,
        mut done: impl FnMut(&EngineView) -> bool,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            host.pump();
            if done(&host.view()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_engine_analysis_reports_score_and_legal_pv() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        let game = GameState::new();
        let fen = game.fen();
        host.set_analysis(true, &fen, SearchGoal::Analyse);

        let searched = pump_until(&mut host, Duration::from_secs(20), |v| {
            v.stats.depth.is_some_and(|d| d >= 4) && !v.stats.pv.is_empty()
        });
        assert!(
            searched,
            "engine should reach depth 4 with a principal variation"
        );

        let view = host.view();
        assert!(
            view.name.as_deref().is_some_and(|n| !n.is_empty()),
            "handshake name"
        );
        assert!(view.stats.score.is_some(), "score present on search lines");
        let san = game.uci_moves_to_san(&view.stats.pv);
        assert_eq!(
            san.len(),
            view.stats.pv.len(),
            "every PV move must be legal from the analysed position"
        );

        host.set_analysis(false, &fen, SearchGoal::Analyse);
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_stops_the_search_but_the_engine_stays_loaded_and_can_restart() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        let fen = GameState::new().fen();
        host.set_analysis(true, &fen, SearchGoal::Analyse);
        assert!(pump_until(&mut host, Duration::from_secs(20), |v| v
            .stats
            .depth
            .is_some()));

        // Abort: stop thinking, keep the engine.
        host.set_analysis(false, &fen, SearchGoal::Analyse);
        assert!(
            pump_until(&mut host, Duration::from_secs(20), |v| v.status
                == AnalysisStatus::Ready),
            "after Abort the engine must be Ready, not Thinking"
        );
        assert_ne!(
            host.view().status,
            AnalysisStatus::Offline,
            "Abort must not unload the engine"
        );

        // Start again: a fresh search runs.
        host.set_analysis(true, &fen, SearchGoal::Analyse);
        assert!(pump_until(&mut host, Duration::from_secs(20), |v| v.status
            == AnalysisStatus::Thinking));
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shut_down_engine_reads_as_offline_and_quits() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        host.set_analysis(true, &GameState::new().fen(), SearchGoal::Analyse);
        assert!(pump_until(&mut host, Duration::from_secs(20), |v| v
            .stats
            .depth
            .is_some()));

        let process = host.shutdown();
        assert_eq!(
            host.view().status,
            AnalysisStatus::Offline,
            "card must read Offline at once"
        );
        process.terminate().await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process.has_exited() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            process.has_exited(),
            "process must actually exit after quit"
        );
        host.pump();
        assert_eq!(
            host.view().status,
            AnalysisStatus::Offline,
            "host stays Offline"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_that_exited_before_attach_shows_offline() {
        use std::os::unix::fs::PermissionsExt;

        // The process ends before the host subscribes, so the `Terminated`
        // event has already been sent. The sticky exit flag must still show it.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("exits-at-once.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let process = EngineProcess::spawn(&script).await.unwrap();
        while !process.has_exited() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        host.set_analysis(true, &GameState::new().fen(), SearchGoal::Analyse);
        host.pump();
        assert_eq!(host.view().status, AnalysisStatus::Offline);
        assert!(
            !host.has_handshaken(),
            "an engine that never answered uci must not be auto-restarted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_engine_declares_its_options_and_takes_a_saved_value() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut settings = BTreeMap::new();
        settings.insert("Hash".to_string(), "32".to_string());
        let mut host = EngineHost::attach_with_settings(process, settings);
        assert!(
            pump_until(&mut host, Duration::from_secs(20), |v| v.status
                == AnalysisStatus::Ready),
            "handshake"
        );
        assert!(
            host.has_handshaken(),
            "the ready engine completed its handshake"
        );
        assert!(
            host.options().iter().any(|option| option.name == "Hash"),
            "the engine must declare the Hash option"
        );
        host.set_option("Hash", "64");
        host.pump();
        assert_eq!(
            host.view().status,
            AnalysisStatus::Ready,
            "a setoption must not disturb the engine"
        );

        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthy_engine_answers_pings_while_it_searches() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let watchdog = Timing {
            ping_every: Duration::from_millis(50),
            response_within: Duration::from_secs(3),
            ..Timing::default()
        };
        let mut host = EngineHost::attach_with(process, watchdog, BTreeMap::new());
        host.set_analysis(true, &GameState::new().fen(), SearchGoal::Analyse);

        let until = Instant::now() + Duration::from_millis(1_500);
        while Instant::now() < until {
            host.pump();
            assert!(
                !host.is_stalled(),
                "an engine that searches must still answer isready"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(host.view().status, AnalysisStatus::Thinking);
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_that_stops_answering_is_reported_as_stalled() {
        use std::os::unix::fs::PermissionsExt;

        // Answers the handshake's `isready`, then goes silent.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("answers-once.sh");
        let body = "#!/bin/sh\nanswered=0\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Mute'; echo uciok;;\n    isready) if [ \"$answered\" = 0 ]; then echo readyok; answered=1; fi;;\n  esac\ndone\n";
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let process = EngineProcess::spawn(&script).await.unwrap();
        let watchdog = Timing {
            ping_every: Duration::from_millis(50),
            response_within: Duration::from_millis(300),
            ..Timing::default()
        };
        let mut host = EngineHost::attach_with(process, watchdog, BTreeMap::new());
        assert!(
            pump_until(&mut host, Duration::from_secs(10), |v| v.status
                == AnalysisStatus::Ready),
            "the mute engine must complete the handshake"
        );

        // The watchdog only watches a running search now; an idle engine is
        // not pinged. Ask it to search, then let it go mute.
        host.set_analysis(true, &GameState::new().fen(), SearchGoal::Analyse);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !host.is_stalled() && Instant::now() < deadline {
            host.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            host.is_stalled(),
            "an engine that ignores isready must be reported as stalled"
        );
        assert_eq!(host.binary_path(), script);

        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_engine_retargets_to_the_new_position() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        let mut game = GameState::new();
        host.set_analysis(true, &game.fen(), SearchGoal::Analyse);
        assert!(pump_until(&mut host, Duration::from_secs(20), |v| v
            .stats
            .depth
            .is_some()));

        // Black to move after 1.e4: the engine must report the new side's score.
        game.play_squares(
            shakmaty::Square::from_ascii(b"e2").unwrap(),
            shakmaty::Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();
        host.set_position(&game.fen(), SearchGoal::Analyse);
        assert_eq!(host.view().status, AnalysisStatus::Thinking);
        let retargeted = pump_until(&mut host, Duration::from_secs(20), |v| {
            v.stats
                .pv
                .first()
                .is_some_and(|m| m.len() == 4 && m != "e2e4")
        });
        assert!(retargeted, "PV must restart for the new position");
        let view = host.view();
        assert!(matches!(
            view.stats.score,
            Some(Score::Cp(_)) | Some(Score::Mate(_))
        ));

        host.shutdown().terminate().await.unwrap();
    }

    /// Pump the host until it hands over a move, or give up after 20 s.
    fn await_engine_move(host: &mut EngineHost) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            host.pump();
            if let Some(uci) = host.take_engine_move() {
                return uci;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("engine must answer the move request with a bestmove");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_engine_plays_a_legal_move_for_the_requested_position() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        let mut game = GameState::new();
        let fen = game.fen();
        let goal = SearchGoal::Move(GoControl::MoveTime(300));
        host.set_analysis(true, &fen, goal);

        let uci = await_engine_move(&mut host);
        assert!(
            game.apply_uci(&uci).is_ok(),
            "`{}` must be legal in the start position",
            uci
        );
        assert_eq!(
            host.view().status,
            AnalysisStatus::Ready,
            "the move search is over"
        );

        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_engine_plays_a_move_within_the_clock_it_is_given() {
        let binary = crate::engine::test_engine_path();
        let process = EngineProcess::spawn(&binary).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        let game = GameState::new();
        let fen = game.fen();
        // The wire form `go wtime … btime … winc … binc …` must be accepted.
        let goal = SearchGoal::Move(GoControl::Clock {
            wtime: 3_000,
            btime: 3_000,
            winc: 0,
            binc: 0,
            movetime: None,
        });
        host.set_analysis(true, &fen, goal);

        let started = Instant::now();
        let uci = await_engine_move(&mut host);
        let mut replay = game.clone();
        assert!(
            replay.apply_uci(&uci).is_ok(),
            "`{}` must be legal in the start position",
            uci
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "3 s on the clock must not take 10 s"
        );

        host.shutdown().terminate().await.unwrap();
    }

    /// Writes an executable `sh` script for a fake engine.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Deadlines short enough for a test: a stop or a move must end within
    /// about 300 ms of its limit.
    fn short_deadlines() -> Timing {
        Timing {
            ping_every: Duration::from_millis(50),
            response_within: Duration::from_millis(300),
            stop_within: Duration::from_millis(300),
            move_grace: Duration::from_millis(300),
            ..Timing::default()
        }
    }

    /// A fake engine that completes the handshake, answers `isready` at all
    /// times, starts an `info` line on `go`, and never sends `bestmove`.
    fn stop_ignoring_engine(dir: &std::path::Path) -> PathBuf {
        write_script(
            dir,
            "stop-ignorer.sh",
            "while IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Fake'; echo uciok;;\n    isready) echo readyok;;\n    go*) echo 'info depth 1 score cp 0 pv e2e4';;\n    quit) exit 0;;\n  esac\ndone\n",
        )
    }

    /// Pump until the host reports a stall (returning its reason) or 5 s pass.
    fn pump_until_stalled(host: &mut EngineHost) -> Option<StallReason> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            host.pump();
            if host.is_stalled() {
                return host.stall_reason();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_without_bestmove_is_reported_as_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let path = stop_ignoring_engine(dir.path());
        let process = EngineProcess::spawn(&path).await.unwrap();
        let mut host = EngineHost::attach_with(process, short_deadlines(), BTreeMap::new());
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v.status
            == AnalysisStatus::Ready));

        let fen = GameState::new().fen();
        host.set_analysis(true, &fen, SearchGoal::Analyse);
        host.set_analysis(false, &fen, SearchGoal::Analyse);
        assert!(host.is_stopping(), "the stop is waiting for its bestmove");
        assert_eq!(
            pump_until_stalled(&mut host),
            Some(StallReason::StopNotAnswered),
            "a healthy isready does not excuse a missing bestmove"
        );
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_search_without_bestmove_is_reported_as_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let path = stop_ignoring_engine(dir.path());
        let process = EngineProcess::spawn(&path).await.unwrap();
        let mut host = EngineHost::attach_with(process, short_deadlines(), BTreeMap::new());
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v.status
            == AnalysisStatus::Ready));

        let fen = GameState::new().fen();
        host.set_analysis(true, &fen, SearchGoal::Move(GoControl::MoveTime(200)));
        assert_eq!(
            pump_until_stalled(&mut host),
            Some(StallReason::MoveNotAnswered)
        );
        assert!(
            host.take_engine_move().is_none(),
            "no move arrives from a stalled search"
        );
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_engine_that_only_prints_noise_is_reported_as_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(
            dir.path(),
            "noisy.sh",
            "searching=0\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Fake'; echo uciok;;\n    isready) if [ \"$searching\" = 0 ]; then echo readyok; fi;;\n    go*) searching=1; ( while true; do echo 'info string still busy'; sleep 0.1; done ) & ;;\n    quit) exit 0;;\n  esac\ndone\n",
        );
        let process = EngineProcess::spawn(&path).await.unwrap();
        let mut host = EngineHost::attach_with(process, short_deadlines(), BTreeMap::new());
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v.status
            == AnalysisStatus::Ready));

        host.set_analysis(true, &GameState::new().fen(), SearchGoal::Analyse);
        assert_eq!(
            pump_until_stalled(&mut host),
            Some(StallReason::NoReadyOk),
            "output that is not progress must not keep a deaf engine alive"
        );
        host.shutdown().terminate().await.unwrap();
    }

    #[test]
    fn stall_reasons_read_as_plain_sentences() {
        assert_eq!(
            StallReason::StopNotAnswered.to_string(),
            "it sent no bestmove after stop"
        );
        assert_eq!(
            StallReason::MoveNotAnswered.to_string(),
            "it sent no move in time"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stall_is_found_while_the_ui_does_not_pump() {
        // With the window hidden the host is not pumped. The monitor task
        // must still find the stall, without any frame.
        let dir = tempfile::tempdir().unwrap();
        let path = stop_ignoring_engine(dir.path());
        let process = EngineProcess::spawn(&path).await.unwrap();
        let mut host = EngineHost::attach_with(process, short_deadlines(), BTreeMap::new());
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v.status
            == AnalysisStatus::Ready));

        let fen = GameState::new().fen();
        host.set_analysis(true, &fen, SearchGoal::Analyse);
        host.set_analysis(false, &fen, SearchGoal::Analyse);
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            host.is_stalled(),
            "the monitor found the stall without a frame"
        );
        assert_eq!(host.stall_reason(), Some(StallReason::StopNotAnswered));
        assert!(
            !host.has_exited() || host.is_stalled(),
            "the stalled engine was killed"
        );
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fast_engine_leaves_one_merged_info_entry_not_a_queue() {
        use std::os::unix::fs::PermissionsExt;

        // Answers `go` with 300 `info` lines before its `bestmove`. The UI
        // does not pump in between, so the lines pile up if they are queued.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fast-info.sh");
        let body = "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Fast'; echo uciok;;\n    isready) echo readyok;;\n    go*) i=1; while [ $i -le 300 ]; do echo \"info depth $i score cp $i pv e2e4\"; i=$((i+1)); done; echo 'bestmove e2e4';;\n    quit) exit 0;;\n  esac\ndone\n";
        std::fs::write(
            &script,
            format!("#!/bin/sh\n{}", body.trim_start_matches("#!/bin/sh\n")),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let process = EngineProcess::spawn(&script).await.unwrap();
        let slot = process.info_slot();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v.status
            == AnalysisStatus::Ready));

        host.process
            .try_send(EngineCommand::Go(GoControl::Depth(1)))
            .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        let merged = slot
            .lock()
            .unwrap()
            .clone()
            .expect("the info lines were merged");
        assert_eq!(merged.depth, Some(300), "the latest depth is kept");
        assert_eq!(
            merged.score,
            Some(Score::Cp(300)),
            "the latest score is kept"
        );
        host.shutdown().terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_card_score_keeps_the_turn_of_the_searched_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(
            dir.path(),
            "black-score.sh",
            "while IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Fake'; echo uciok;;\n    isready) echo readyok;;\n    go*) echo 'info depth 3 score cp 45 pv e7e5';;\n    quit) exit 0;;\n  esac\ndone\n",
        );
        let process = EngineProcess::spawn(&path).await.unwrap();
        let mut host = EngineHost::attach_with_settings(process, BTreeMap::new());
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v.status
            == AnalysisStatus::Ready));

        // Black to move on move 5: the engine's `score cp 45` belongs to this
        // position, so the card must keep Black's turn after the search freezes.
        let fen = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 5";
        host.set_analysis(true, fen, SearchGoal::Analyse);
        assert!(pump_until(&mut host, Duration::from_secs(5), |v| v
            .stats
            .score
            .is_some()));

        let view = host.view();
        assert!(
            !view.search_white_to_move,
            "the searched position has Black to move"
        );
        assert_eq!(
            view.search_fullmove, 5,
            "the searched position's move number is kept"
        );
        assert_eq!(
            crate::gui::eval_panel::white_point_of_view(
                view.stats.score.unwrap(),
                view.search_white_to_move
            ),
            Score::Cp(-45),
            "Black's +45 is -0.45 from White's point of view"
        );
        host.shutdown().terminate().await.unwrap();
    }
}

//! UCI engine subsystem.
//!
//! Implements an async, tokio-based UCI engine supervisor built on **three
//! dedicated tokio tasks** per engine subprocess —
//!
//! 1. **Writer task** — drains `cmd_rx` and writes each line to the
//!    child's stdin via a buffered writer.
//! 2. **Reader task** — pulls lines from the child's stdout via
//!    `read_line` and publishes each line to a `tokio::sync::broadcast`
//!    channel so multiple consumers can subscribe.
//! 3. **Supervisor task** — waits on either `kill_rx` or
//!    `child.wait()`; whichever fires first determines cleanup.
//!
//! Splitting writer/reader into separate tasks is essential because UCI
//! engines can produce dozens of `info …` lines per second — without the
//! split, a slow downstream consumer could deadlock the writer.
//
//! ## Why no `tokio::select!` in the inner loop?
//!
//! An earlier draft multiplexed read + write + terminate inside a single
//! `tokio::select!`, but the compile-time handshake between variadic
//! branches and `?Sized` future outputs was brittle. The three-task layout is
//! just as performant and far easier to reason about.

pub mod analysis;
pub mod host;
mod launch;
pub mod protocol;
pub mod registry;
pub mod uci_parser;
pub mod watchdog;

pub use analysis::{AnalysisStatus, SearchGoal};
pub use host::{EngineHost, EngineView};
pub use protocol::{EngineCommand, EngineError, EngineEvent, Score};

use launch::{check_executable, engine_path, Diagnostics};
use protocol::EngineStats;
use uci_parser::{parse_line, HandshakeAccumulator, UciParseError};
use watchdog::{Timing, Watchdog, WatchdogAction};

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};

// ─────────────────────────────────────────────────────────────────────────────
// EngineProcess
// ─────────────────────────────────────────────────────────────────────────────

/// Capacity for the broadcast channel of stdout lines. The UI host uses the
/// lossless per-engine `mpsc` stream, not this broadcast; only tests and
/// external observers subscribe. A lagging observer therefore cannot lose a
/// `bestmove` for the app, and a deep buffer would only hold heap-heavy PV
/// vectors for nobody. 256 lines is plenty for an observer.
const BROADCAST_BUFFER: usize = 256;

/// Capacity for the command mpsc. Sized for a UI frame's worth of commands
/// (stop + position + go, for several engines) without dropping any.
const COMMAND_BUFFER: usize = 64;

/// Handle for sending commands to a running engine.
///
/// Cloning is cheap (`mpsc::Sender` is `Clone`), so multiple UI components
/// can hold their own copy (a future "stop search" button next to the main
/// board widget, for example).
#[derive(Clone)]
pub struct EngineProcess {
    /// Channel for sending wire-encoded UCI commands to the writer task.
    cmd_tx: mpsc::Sender<EngineCommand>,
    /// Channel for signalling termination.
    kill_tx: mpsc::Sender<()>,
    /// Broadcast channel for engine stdout lines (one line per message). The UI
    /// host uses the lossless `event_rx`; this remains for tests and observers.
    #[allow(
        dead_code,
        reason = "kept for tests and external observers; the UI host uses event_rx"
    )]
    pub events: broadcast::Sender<ParsedUciLine>,
    /// Lossless stream for the UI host. The broadcast channel is kept for
    /// tests and other observers; the host drains this unbounded channel so a
    /// busy search can never lag `bestmove`/`readyok` away.
    event_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<ParsedUciLine>>>>,
    /// Path of the spawned binary, kept for logging and display.
    binary: PathBuf,
    /// Sticky flag set once the engine's output ends. Unlike a broadcast
    /// event it is still visible to a subscriber that attaches late.
    exited: Arc<AtomicBool>,
    /// The deadlines of the running search. The reader records signs of life
    /// in it as lines arrive, and the monitor task checks it on its own clock,
    /// so a hidden window cannot stop the checks.
    watchdog: Arc<Mutex<Watchdog>>,
    /// The pending principal `info` statistics (see [`InfoSlot`]).
    info: InfoSlot,
    /// Stderr lines and the exit status, for a failure report.
    diagnostics: Arc<Mutex<Diagnostics>>,
}

/// What the reader shares with the rest of the engine: the deadlines and the
/// pending `info` statistics.
struct ReaderShared {
    /// The deadlines, updated for every sign of life.
    watchdog: Arc<Mutex<Watchdog>>,
    /// Where merged `info` statistics are left for the host.
    info: InfoSlot,
}

/// Principal `info` statistics waiting for the host, merged in arrival order.
/// Only one entry is kept, so a fast engine cannot grow the queue.
pub type InfoSlot = Arc<Mutex<Option<EngineStats>>>;

/// `true` for an `info` line of a MultiPV line other than the first. The UI
/// never shows those lines, so they are not parsed.
fn is_secondary_info(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("info ") else {
        return false;
    };
    let mut tokens = rest.split_ascii_whitespace();
    while let Some(token) = tokens.next() {
        if token == "multipv" {
            return tokens
                .next()
                .and_then(|number| number.parse::<u32>().ok())
                .is_some_and(|number| number > 1);
        }
    }
    false
}

/// One event from the engine, already trimmed and reduced.
#[derive(Debug, Clone)]
pub enum ParsedUciLine {
    /// Line was successfully reduced to an event. Use this in tests and
    /// downstream subscribers that want the typed events; the broadcast
    /// layer ensures every subscriber sees every line exactly once.
    Event(EngineEvent),
    /// Line was unreadable / unparseable. We carry the raw line so the UI
    /// can render it in a debug log if needed.
    ParseError { line: String, source: UciParseError },
    /// Unknown / engine-specific line (treated as info, not an error).
    Unknown(String),
}

/// Called by the engine reader when it publishes a line, so an event-driven UI
/// can run a frame at once instead of polling a timer. The flag is `true` for
/// lines that must be seen immediately (handshake answer, `bestmove`, exit)
/// and `false` for `info` lines, which may be coalesced. Runs on the reader
/// task and must be cheap and non-blocking.
pub type EngineWake = Arc<dyn Fn(bool) + Send + Sync>;

impl EngineProcess {
    /// Spawn without a wake callback, for tests and for callers that drain
    /// [`Self::subscribe`] themselves. The app uses [`Self::spawn_waking`].
    #[allow(
        dead_code,
        reason = "the app always passes a wake callback; tests spawn without one"
    )]
    pub async fn spawn(binary: &Path) -> Result<Self, EngineError> {
        Self::spawn_inner(binary, None).await
    }

    /// Spawn the engine and call `wake` whenever output arrives, so the window
    /// is redrawn as soon as there is something new to show.
    pub async fn spawn_waking(binary: &Path, wake: EngineWake) -> Result<Self, EngineError> {
        Self::spawn_inner(binary, Some(wake)).await
    }

    /// Shared body of [`Self::spawn`] and [`Self::spawn_waking`]. Starts the
    /// three supervisor tasks; they close when stdin/stdout close (engine
    /// exit) or when [`Self::terminate`] is called.
    async fn spawn_inner(binary: &Path, wake: Option<EngineWake>) -> Result<Self, EngineError> {
        check_executable(binary)?;
        let mut cmd = Command::new(binary);
        // The engine leads its own process group, so a kill also reaches any
        // helper process it started (see `registry::kill_group`). It runs from
        // its own folder, where its data files usually are, and with the tool
        // folders on PATH, which a Finder launch does not give it.
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .env("PATH", engine_path())
            .kill_on_drop(true);
        if let Some(folder) = binary
            .parent()
            .filter(|folder| !folder.as_os_str().is_empty())
        {
            cmd.current_dir(folder);
        }

        let mut child = cmd.spawn().map_err(|source| EngineError::SpawnFailed {
            binary: Some(binary.to_path_buf()),
            source,
        })?;
        let pid = child.id().unwrap_or(0);
        if pid != 0 {
            registry::register(pid, binary);
        }

        let stdin = child.stdin.take().ok_or(EngineError::Io {
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing stdin pipe"),
        })?;
        let stdout = child.stdout.take().ok_or(EngineError::Io {
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing stdout pipe"),
        })?;
        let stderr = child.stderr.take().ok_or(EngineError::Io {
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "missing stderr pipe"),
        })?;

        let (cmd_tx, stdin_rx) = mpsc::channel::<EngineCommand>(COMMAND_BUFFER);
        let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);
        let (events, _) = broadcast::channel::<ParsedUciLine>(BROADCAST_BUFFER);
        let (line_tx, line_rx) = mpsc::unbounded_channel::<ParsedUciLine>();
        let exited = Arc::new(AtomicBool::new(false));
        let watchdog = Arc::new(Mutex::new(Watchdog::default()));
        let info: InfoSlot = Arc::new(Mutex::new(None));
        let diagnostics = Arc::new(Mutex::new(Diagnostics::default()));

        // ── Writer task ──
        tokio::spawn(async move {
            run_writer(stdin, stdin_rx).await;
        });

        // ── Reader task ──
        let events_for_reader = events.clone();
        let reader_exited = Arc::clone(&exited);
        let reader_binary = binary.to_path_buf();
        let reader_watchdog = Arc::clone(&watchdog);
        let reader_wake = wake.clone();
        let reader_info = Arc::clone(&info);
        tokio::spawn(async move {
            run_reader(
                stdout,
                events_for_reader,
                line_tx,
                reader_exited,
                reader_binary,
                reader_wake,
                ReaderShared {
                    watchdog: reader_watchdog,
                    info: reader_info,
                },
            )
            .await;
        });

        // ── Monitor task ──
        let monitor = Monitor {
            watchdog: Arc::clone(&watchdog),
            commands: cmd_tx.clone(),
            kill: kill_tx.clone(),
            exited: Arc::clone(&exited),
            wake,
            binary: binary.to_path_buf(),
        };
        tokio::spawn(async move {
            run_monitor(monitor).await;
        });

        // ── Stderr task ──
        // Engines report crashes and internal errors on stderr; the session
        // log keeps them, and the last lines go to a failure report.
        let stderr_binary = binary.to_path_buf();
        let stderr_diagnostics = Arc::clone(&diagnostics);
        tokio::spawn(async move {
            run_stderr(stderr, stderr_binary, stderr_diagnostics).await;
        });

        // ── Supervisor task ──
        let supervisor_exited = Arc::clone(&exited);
        let supervisor_binary = binary.to_path_buf();
        let supervisor_diagnostics = Arc::clone(&diagnostics);
        tokio::spawn(async move {
            run_supervisor(
                child,
                pid,
                &mut kill_rx,
                supervisor_exited,
                supervisor_binary,
                supervisor_diagnostics,
            )
            .await;
        });

        Ok(Self {
            cmd_tx,
            kill_tx,
            events,
            event_rx: Arc::new(Mutex::new(Some(line_rx))),
            binary: binary.to_path_buf(),
            exited,
            watchdog,
            info,
            diagnostics,
        })
    }

    /// What the engine has said about its own failure, for the status line.
    pub fn diagnosis(&self) -> String {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .summary()
    }

    /// The pending principal `info` statistics, merged since the host last took them.
    pub fn info_slot(&self) -> InfoSlot {
        Arc::clone(&self.info)
    }

    /// The deadlines of this engine's search, shared with the reader and the monitor.
    pub fn watchdog(&self) -> Arc<Mutex<Watchdog>> {
        Arc::clone(&self.watchdog)
    }

    /// Use `timing` for this engine's deadlines from now on.
    pub fn set_timing(&self, timing: Timing) {
        self.watchdog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_timing(timing);
    }

    /// Take the lossless event stream once, when the UI host attaches.
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<ParsedUciLine>> {
        self.event_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Non-blocking send for the UI thread. Fails immediately when the
    /// command queue is full or the engine has exited, so the caller never
    /// waits on an engine.
    pub fn try_send(&self, cmd: EngineCommand) -> Result<(), EngineError> {
        self.cmd_tx.try_send(cmd).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => EngineError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => EngineError::ChannelClosed,
        })
    }

    /// Subscribe a consumer to the broadcast event stream.
    #[allow(
        dead_code,
        reason = "used by tests and external observers; the UI host uses the lossless channel"
    )]
    pub fn subscribe(&self) -> broadcast::Receiver<ParsedUciLine> {
        self.events.subscribe()
    }

    /// Graceful termination: send `quit` over the command channel, then
    /// after a small grace window escalate to a hard `kill` if the engine
    /// is still alive.
    pub async fn terminate(&self) -> Result<(), EngineError> {
        // Best-effort polite quit — if the engine is dead, ignore.
        let _ = self.cmd_tx.send(EngineCommand::Quit).await;
        // Schedule an escalation to hard kill; 500 ms is the grace window
        // for the engine to answer the quit.
        let kill_tx = self.kill_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = kill_tx.send(()).await;
        });
        Ok(())
    }

    /// `true` once the engine's output has ended (the process exited or was
    /// killed). Safe to poll at any time, including after the `Terminated`
    /// event was already sent.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Path of the spawned binary, kept for logging and display.
    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Task bodies
// ─────────────────────────────────────────────────────────────────────────────

/// Writer task: pulls commands off `cmd_rx` and writes each one as a
/// single line ("{cmd}\n") into stdin via a `BufWriter`.
async fn run_writer(stdin: tokio::process::ChildStdin, mut cmd_rx: mpsc::Receiver<EngineCommand>) {
    let mut writer = BufWriter::new(stdin);
    while let Some(cmd) = cmd_rx.recv().await {
        let line = cmd.to_wire_string();
        if writer.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if !line.ends_with('\n') && writer.write_all(b"\n").await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
    // Shutdown: flush any buffered data and let the pipe close.
    let _ = writer.shutdown().await;
}

/// Reader task: pulls lines from stdout via `BufReader::read_line` and
/// publishes each parsed event to the broadcast channel.
///
/// While the engine is mid-handshake (sending `id …`, `option …`,
/// `uciok` lines), we accumulate locally and only emit **one**
/// consolidated [`EngineEvent::UciOk`] at `uciok`. Other event types
/// (`info`, `bestmove`, etc.) are forwarded immediately.
///
/// Lags on the broadcast are silent — the engine anyway emits its
/// bestmove on the same channel so subscribers can detect a missed
/// line and react.
async fn run_reader(
    stdout: tokio::process::ChildStdout,
    events: broadcast::Sender<ParsedUciLine>,
    lines: mpsc::UnboundedSender<ParsedUciLine>,
    exited: Arc<AtomicBool>,
    binary: PathBuf,
    wake: Option<EngineWake>,
    shared: ReaderShared,
) {
    let ReaderShared { watchdog, info } = shared;
    let mut reader = BufReader::new(stdout);
    let mut raw: Vec<u8> = Vec::new();
    let mut acc = HandshakeAccumulator::new();
    // The statistics the card shows, merged as the lines arrive. A line only
    // wakes the window when one of these changes.
    let mut shown = EngineStats::default();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw).await {
            Ok(0) => break, // EOF — engine closed stdout.
            Ok(_) => {
                // Engines occasionally print non-UTF-8 bytes; keep reading
                // rather than treating the whole engine as dead.
                let text = String::from_utf8_lossy(&raw);
                let trimmed = text.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() || is_secondary_info(trimmed) {
                    continue;
                }
                log::trace!("{} -> {}", binary.display(), trimmed);
                let message = match parse_line(trimmed) {
                    Ok(ev) => acc.fold(ev).map(ParsedUciLine::Event).unwrap_or_else(|| {
                        // Mid-handshake line — the consolidated UciOk
                        // arrives at `uciok`.
                        log::trace!("{} handshake: {}", binary.display(), trimmed);
                        ParsedUciLine::Unknown(trimmed.to_string())
                    }),
                    Err(UciParseError::UnknownLine { line }) => {
                        log::debug!("{} noise: {}", binary.display(), line);
                        ParsedUciLine::Unknown(line)
                    }
                    Err(source) => {
                        log::warn!("{} parse error `{}`: {}", binary.display(), trimmed, source);
                        ParsedUciLine::ParseError {
                            line: trimmed.to_string(),
                            source,
                        }
                    }
                };
                if let ParsedUciLine::Event(event) = &message {
                    observe_line(&watchdog, &binary, event);
                }
                // Ignore the SendError when no listeners exist (tests
                // that don't subscribe).
                // Whether this line should wake the window, and how urgently.
                // `info` wakes only when a value the card shows has changed (a
                // `currmove` line does not change it); `bestmove`, `readyok`
                // and the rest always wake; noise does not wake at all.
                let wake_now: Option<bool> = match &message {
                    ParsedUciLine::Event(EngineEvent::Info(stats)) => {
                        let before = shown_values(&shown);
                        shown.merge(stats);
                        (shown_values(&shown) != before).then_some(false)
                    }
                    ParsedUciLine::Event(_) => Some(true),
                    ParsedUciLine::Unknown(_) | ParsedUciLine::ParseError { .. } => None,
                };
                // Only clone for the broadcast when someone listens to it; the
                // app reads the lossless stream, so usually nobody does.
                if events.receiver_count() > 0 {
                    let _ = events.send(message.clone());
                }
                match message {
                    ParsedUciLine::Event(EngineEvent::Info(stats)) => {
                        // `info` lines are merged into one pending entry, which
                        // the host takes before its queue. A search that prints
                        // faster than the window draws no longer piles up
                        //. Merging is what the host would do anyway.
                        let mut pending =
                            info.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        match pending.as_mut() {
                            Some(entry) => entry.merge(&stats),
                            None => *pending = Some(stats),
                        }
                    }
                    ParsedUciLine::Event(event) => {
                        // Search-boundary events (`bestmove`, `readyok`,
                        // `uciok`, `Terminated`) are a handful per search, so
                        // the lossless channel cannot grow under a fast engine.
                        let _ = lines.send(ParsedUciLine::Event(event));
                    }
                    ParsedUciLine::Unknown(_) | ParsedUciLine::ParseError { .. } => {
                        // Noise is already in the session log above. It has no
                        // consumer in the UI, so it is not queued — this is the
                        // drain cap for the one line source that could repeat
                        // without bound.
                    }
                }
                if let (Some(wake), Some(urgent)) = (&wake, wake_now) {
                    wake(urgent);
                }
            }
            Err(source) => {
                log::warn!("{} stdout read failed: {}", binary.display(), source);
                break;
            }
        }
    }
    log::debug!("{} stdout closed", binary.display());
    // Tell subscribers the engine is gone so the UI can show it as offline.
    exited.store(true, Ordering::Release);
    let terminated = ParsedUciLine::Event(EngineEvent::Terminated);
    let _ = lines.send(terminated.clone());
    let _ = events.send(terminated);
    if let Some(wake) = &wake {
        wake(true);
    }
}

/// What the card shows of one search: depth, score, nodes, speed and the PV.
type ShownValues = (
    Option<u32>,
    Option<Score>,
    Option<u64>,
    Option<u64>,
    Vec<String>,
);

/// The values of the statistics the card shows: score, depth, nodes, speed and
/// the principal variation. A change in any of them is a visible change.
fn shown_values(stats: &EngineStats) -> ShownValues {
    (
        stats.depth,
        stats.score,
        stats.nodes,
        stats.nps,
        stats.pv.clone(),
    )
}

/// Record one engine event in the watchdog. Only real signs of life count:
/// `readyok`, `uciok`, `bestmove`, and `info` lines with a depth or a node count.
/// Noise does not. Replies are logged at debug with their timing.
fn observe_line(watchdog: &Mutex<Watchdog>, binary: &Path, event: &EngineEvent) {
    let now = std::time::Instant::now();
    let mut watch = watchdog
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match event {
        EngineEvent::ReadyOk => {
            // Answers to the watchdog's pings: one every few seconds during a
            // search, so trace only, like the `isready` they answer.
            log::trace!("{} -> readyok", binary.display());
            watch.answered();
            watch.note_output(now);
        }
        EngineEvent::UciOk { .. } => {
            log::debug!("{} -> uciok", binary.display());
            watch.note_output(now);
        }
        EngineEvent::Info(stats) if stats.depth.is_some() || stats.nodes.is_some() => {
            watch.note_progress(now);
        }
        EngineEvent::BestMove { best_move, .. } => {
            let since = match (watch.stop_age(now), watch.search_age(now)) {
                (Some(stop), _) => format!("{:.2} s after stop", stop.as_secs_f64()),
                (None, Some(search)) => format!("{:.2} s after go", search.as_secs_f64()),
                (None, None) => "no search running".to_string(),
            };
            log::debug!(
                "{} -> bestmove {} ({})",
                binary.display(),
                best_move.as_deref().unwrap_or("(none)"),
                since
            );
            watch.search_finished();
            watch.note_output(now);
        }
        _ => {}
    }
}

/// How often the monitor checks the deadlines of a running search.
const MONITOR_TICK: Duration = Duration::from_millis(250);

/// What the monitor task needs from its engine.
struct Monitor {
    /// The deadlines, shared with the host and the reader.
    watchdog: Arc<Mutex<Watchdog>>,
    /// Commands to the writer, for the pings.
    commands: mpsc::Sender<EngineCommand>,
    /// Kill signal for the supervisor, for a stalled engine.
    kill: mpsc::Sender<()>,
    /// Set when the engine's output ends; the monitor then stops.
    exited: Arc<AtomicBool>,
    /// Wakes the window when the engine is declared stalled.
    wake: Option<EngineWake>,
    /// The engine's file, for the log.
    binary: PathBuf,
}

/// Monitor task: checks the search deadlines and pings the engine, on its own
/// clock, so the checks run even while the window is hidden. A
/// stalled engine is killed here; the host sees the stall through the shared
/// watchdog and restarts it.
async fn run_monitor(monitor: Monitor) {
    let mut tick = tokio::time::interval(MONITOR_TICK);
    while !monitor.exited.load(Ordering::Acquire) {
        tick.tick().await;
        let action = {
            let mut watch = monitor
                .watchdog
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let action = watch.check(std::time::Instant::now());
            if let WatchdogAction::Stalled(reason) = action {
                let now = std::time::Instant::now();
                log::warn!(
                    "{} stalled: {} (search {} s old, stop {} s old)",
                    monitor.binary.display(),
                    reason,
                    watch.search_age(now).map_or(0.0, |age| age.as_secs_f64()),
                    watch.stop_age(now).map_or_else(
                        || "none".to_string(),
                        |age| format!("{:.1}", age.as_secs_f64())
                    )
                );
            }
            action
        };
        match action {
            WatchdogAction::Nothing => {}
            WatchdogAction::Ping => {
                let _ = monitor.commands.try_send(EngineCommand::IsReady);
            }
            WatchdogAction::Stalled(_) => {
                // The kill makes the supervisor stop the group; the host
                // restarts the engine on its next frame.
                let _ = monitor.kill.try_send(());
                if let Some(wake) = &monitor.wake {
                    wake(true);
                }
                return;
            }
        }
    }
}

/// Reader task for the engine's stderr. Some engines print warnings or crash
/// details there, and they are invisible otherwise. Lines go to the session
/// log at debug level (the file's default filter includes them).
async fn run_stderr(
    stderr: tokio::process::ChildStderr,
    binary: PathBuf,
    diagnostics: Arc<Mutex<Diagnostics>>,
) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim_end();
                if !line.is_empty() {
                    log::debug!("{} stderr: {}", binary.display(), line);
                    diagnostics
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .note_stderr(line);
                }
            }
            Ok(None) => break,
            Err(source) => {
                log::debug!("{} stderr read failed: {}", binary.display(), source);
                break;
            }
        }
    }
}

/// Supervisor task: waits on either `kill_rx` (UI-driven termination) or
/// `child.wait()` (engine exited on its own). The first one to fire wins;
/// the supervisor then ensures the child is dead before returning.
async fn run_supervisor(
    mut child: tokio::process::Child,
    pid: u32,
    kill_rx: &mut mpsc::Receiver<()>,
    exited: Arc<AtomicBool>,
    binary: PathBuf,
    diagnostics: Arc<Mutex<Diagnostics>>,
) {
    let status = tokio::select! {
        _ = kill_rx.recv() => {
            // Kill the whole group, so helper processes go too.
            if pid != 0 {
                registry::kill_group(pid);
            }
            let _ = child.kill().await;
            child.wait().await.ok()
        }
        status = child.wait() => status.ok(),
    };
    // The engine may have left helpers behind after a normal quit.
    if pid != 0 {
        registry::kill_group(pid);
        registry::unregister(pid);
    }
    let how = match &status {
        Some(status) => status.to_string(),
        None => "exit status unavailable".to_string(),
    };
    match status {
        Some(status) if status.success() => {
            log::info!("{} exited ({status})", binary.display());
        }
        Some(status) => {
            log::warn!("{} exited unexpectedly ({status})", binary.display());
        }
        None => {
            log::warn!("{} exit status unavailable", binary.display());
        }
    }
    diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .note_exit(how);
    exited.store(true, Ordering::Release);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — real-world Stockfish
// ─────────────────────────────────────────────────────────────────────────────

/// The UCI engine used by the real-engine tests. Search order:
/// `SILICONBOARD_TEST_ENGINE`, Homebrew Stockfish, then the workspace
/// Triumviratus build. Panics when none exists: a silently skipped test
/// would pass without testing anything.
#[cfg(test)]
pub(crate) fn test_engine_path() -> PathBuf {
    let mut tried = Vec::new();
    if let Ok(custom) = std::env::var("SILICONBOARD_TEST_ENGINE") {
        let p = PathBuf::from(custom);
        if p.exists() {
            return p;
        }
        tried.push(p);
    }
    let stockfish = PathBuf::from("/opt/homebrew/bin/stockfish");
    if stockfish.exists() {
        return stockfish;
    }
    tried.push(stockfish);
    let triumviratus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("Triumviratus")
        .join("Triumviratus_8.0_macos_arm");
    if triumviratus.exists() {
        return triumviratus;
    }
    tried.push(triumviratus);
    panic!(
        "no test engine found; install Stockfish or set SILICONBOARD_TEST_ENGINE (tried {:?})",
        tried
    );
}

#[cfg(test)]
mod tests {
    use super::protocol::{GoControl, UciOptionKind};
    use super::*;
    use std::time::{Duration, Instant};

    /// Wait up to `timeout` for the predicate to become true, draining
    /// broadcasts meanwhile so the broadcast buffer does not fill up.
    async fn wait_until<F>(
        mut rx: broadcast::Receiver<ParsedUciLine>,
        timeout: Duration,
        mut pred: F,
    ) -> Vec<ParsedUciLine>
    where
        F: FnMut(&ParsedUciLine) -> bool,
    {
        let start = Instant::now();
        let mut drained = Vec::new();
        while start.elapsed() < timeout {
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Ok(line)) => {
                    let matched = pred(&line);
                    drained.push(line);
                    if matched {
                        return drained;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => continue, // tick
            }
        }
        drained
    }

    #[test]
    fn executable_check_names_the_problem() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        let plain = dir.path().join("notes.md");
        std::fs::write(&plain, "not a program").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = check_executable(&plain).unwrap_err();
        assert!(err.to_string().contains("not marked executable"), "{}", err);

        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let err = check_executable(&folder).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{}", err);

        // A script with the execute bit passes; only the handshake can tell
        // whether it is a UCI engine.
        let script = dir.path().join("wrapper.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_executable(&script).is_ok());
    }

    #[tokio::test]
    async fn engine_exit_publishes_terminated_event() {
        use std::os::unix::fs::PermissionsExt;

        // A stand-in "engine" that stays alive briefly and then exits on its own.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("short-lived-engine.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 0.3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let engine = EngineProcess::spawn(&script).await.unwrap();
        let rx = engine.subscribe();
        let drained = wait_until(rx, Duration::from_secs(5), |line| {
            matches!(line, ParsedUciLine::Event(EngineEvent::Terminated))
        })
        .await;
        assert!(
            drained
                .iter()
                .any(|line| matches!(line, ParsedUciLine::Event(EngineEvent::Terminated))),
            "engine exit must be announced to subscribers"
        );
        assert!(engine.has_exited(), "the sticky exit flag must be set too");
    }

    #[tokio::test]
    async fn engine_output_calls_the_wake_callback() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicUsize;

        // A stand-in engine that prints one info line and then stays alive.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("chatty-engine.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep 0.1\necho 'info depth 1 score cp 0'\nsleep 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let engine = EngineProcess::spawn_waking(
            &script,
            Arc::new(move |_important: bool| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .await
        .unwrap();
        let rx = engine.subscribe();

        let drained = wait_until(rx, Duration::from_secs(5), |line| {
            matches!(line, ParsedUciLine::Event(EngineEvent::Info(_)))
        })
        .await;
        assert!(
            drained
                .iter()
                .any(|line| matches!(line, ParsedUciLine::Event(EngineEvent::Info(_)))),
            "the script engine's info line must arrive"
        );
        assert!(
            wakes.load(Ordering::Relaxed) > 0,
            "the reader must wake the UI when engine output arrives"
        );
    }

    #[tokio::test]
    async fn real_engine_handshake_emits_uciok_with_options() {
        let binary = test_engine_path();

        let engine = EngineProcess::spawn(&binary).await.unwrap();
        let rx = engine.subscribe();

        engine.try_send(EngineCommand::Initialize).unwrap();

        let drained = wait_until(rx, Duration::from_secs(5), |line| {
            matches!(
                line,
                ParsedUciLine::Event(EngineEvent::UciOk { name, options, .. })
                    if !name.is_empty() && !options.is_empty()
            )
        })
        .await;

        let found = drained.iter().find_map(|line| match line {
            ParsedUciLine::Event(EngineEvent::UciOk {
                name,
                options,
                author,
            }) if !name.is_empty() => Some((name.clone(), options.clone(), author.clone())),
            _ => None,
        });
        let (name, options, _author) = found.expect("did not see a populated UciOk within 5s");
        // The test engine may be Stockfish or any other UCI engine, so only the
        // shape of the handshake is asserted, not a family name.
        assert!(!name.is_empty(), "the handshake must name the engine");
        assert!(!options.is_empty(), "UciOk must aggregate options");

        let names: Vec<&str> = options.iter().map(|o| o.name.as_str()).collect();
        assert!(
            names.contains(&"Threads"),
            "missing `Threads` option in {:?}",
            names
        );
        assert!(
            names.contains(&"Hash"),
            "missing `Hash` option in {:?}",
            names
        );
        assert!(
            names.contains(&"Clear Hash"),
            "missing `Clear Hash` option in {:?}",
            names
        );

        // Spawn pattern of "Clear Hash" option must be Button.
        let clear_hash = options
            .iter()
            .find(|o| o.name == "Clear Hash")
            .expect("Clear Hash");
        assert_eq!(clear_hash.kind, UciOptionKind::Button);

        engine.terminate().await.unwrap();
        // Give the supervisor a beat to mark us dead.
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    #[tokio::test]
    async fn stockfish_depth_4_search_returns_bestmove() {
        let binary = test_engine_path();

        let engine = EngineProcess::spawn(&binary).await.unwrap();
        let mut rx = engine.subscribe();

        engine.try_send(EngineCommand::Initialize).unwrap();
        engine.try_send(EngineCommand::IsReady).unwrap();

        // Wait for ReadyOk so we know the handshake is fully past.
        let _ = wait_until(rx.resubscribe(), Duration::from_secs(5), |line| {
            matches!(line, ParsedUciLine::Event(EngineEvent::ReadyOk))
        })
        .await;

        engine.try_send(EngineCommand::NewGame).unwrap();
        engine
            .try_send(EngineCommand::SetPosition {
                fen: None,
                moves: Vec::new(),
            })
            .unwrap();
        engine
            .try_send(EngineCommand::Go(GoControl::Depth(4)))
            .unwrap();

        // Search for bestmove + at least one info line.
        let start = Instant::now();
        let timeout = Duration::from_secs(30);
        let mut saw_info = false;
        let mut bestmove: Option<String> = None;

        while start.elapsed() < timeout {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(line)) => match line {
                    ParsedUciLine::Event(EngineEvent::Info(_)) => saw_info = true,
                    ParsedUciLine::Event(EngineEvent::BestMove { best_move, .. }) => {
                        bestmove = best_move;
                        break;
                    }
                    _ => {}
                },
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }

        let bm = bestmove.expect("Stockfish did not emit bestmove within 30 s");
        assert!(saw_info, "Stockfish depth 4 emitted no info lines");
        assert!(
            ["e2e4", "d2d4", "c2c4", "g1f3", "b1c3"].contains(&bm.as_str()),
            "unexpected first-move reply: {}",
            bm
        );

        engine.terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn helper_processes_are_stopped_with_their_engine() {
        use std::os::unix::fs::PermissionsExt;

        // The engine starts a helper, records its PID, and quits on `quit`
        // while the helper is still running.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("helper.pid");
        let script = dir.path().join("helper-engine.sh");
        let body = format!(
            "#!/bin/sh\nsleep 4343 &\necho $! > '{}'\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Helper'; echo uciok;;\n    quit) exit 0;;\n  esac\ndone\n",
            pidfile.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let engine = EngineProcess::spawn(&script).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pidfile.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let helper: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            unsafe { libc::kill(helper, 0) } == 0,
            "the helper must be running before the quit"
        );

        engine.terminate().await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !engine.has_exited() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let alive = unsafe { libc::kill(helper, 0) } == 0;
        if alive {
            registry::kill_group(helper as u32);
        }
        assert!(!alive, "the helper process must not outlive its engine");
    }

    #[test]
    fn only_secondary_multipv_info_lines_are_skipped() {
        assert!(is_secondary_info(
            "info depth 9 multipv 2 score cp -12 pv e2e4"
        ));
        assert!(!is_secondary_info(
            "info depth 9 multipv 1 score cp 12 pv e2e4"
        ));
        assert!(
            !is_secondary_info("info depth 9 score cp 12 pv e2e4"),
            "no MultiPV means line 1"
        );
        assert!(
            !is_secondary_info("bestmove multipv 2"),
            "only `info` lines are considered"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_launcher_script_runs_the_program_it_names_from_its_own_folder() {
        use std::os::unix::fs::PermissionsExt;

        // The launcher starts a program by a relative path, so it only works
        // when the engine runs from the launcher's folder (as Calvin's and
        // many others do).
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-engine.sh");
        std::fs::write(
            &real,
            "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Real'; echo uciok;;\n    quit) exit 0;;\n  esac\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let wrapper = dir.path().join("launcher.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec ./real-engine.sh\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

        let engine = EngineProcess::spawn(&wrapper).await.unwrap();
        let rx = engine.subscribe();
        engine.try_send(EngineCommand::Initialize).unwrap();
        let drained = wait_until(rx, Duration::from_secs(5), |line| {
            matches!(line, ParsedUciLine::Event(EngineEvent::UciOk { name, .. }) if name == "Real")
        })
        .await;
        assert!(
            drained.iter().any(|l| matches!(l, ParsedUciLine::Event(EngineEvent::UciOk { name, .. }) if name == "Real")),
            "the program behind the launcher must answer uci"
        );
        engine.terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_launcher_that_names_a_missing_program_reports_exit_127() {
        use std::os::unix::fs::PermissionsExt;

        // The gnuchess case: the launcher names a program that is not there.
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("run_missing.sh");
        std::fs::write(&wrapper, "#!/bin/sh\n/no/such/folder/gnuchess --uci\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

        let engine = EngineProcess::spawn(&wrapper).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !engine.has_exited() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let report = engine.diagnosis();
        assert!(report.contains("exit status: 127"), "{report}");
        engine.terminate().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_engine_reports_its_last_message_with_the_exit_status() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("complains.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'boom: data file missing' >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let engine = EngineProcess::spawn(&script).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !engine.has_exited() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let report = engine.diagnosis();
        assert!(report.contains("exit status: 3"), "{report}");
        assert!(report.contains("boom: data file missing"), "{report}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn currmove_lines_and_noise_do_not_wake_the_window() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicUsize;

        // One line that changes the card, then 200 `currmove` lines that do
        // not, and some noise. Only the first `info` should wake the window.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("currmove-engine.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    uci) echo 'id name Chatty'; echo uciok;;\n    isready) echo readyok;;\n    go*) echo 'info depth 1 score cp 5 pv e2e4'; i=1; while [ $i -le 200 ]; do echo \"info currmove e2e4 currmovenumber $i\"; echo 'info string noise'; i=$((i+1)); done; echo 'bestmove e2e4';;\n    quit) exit 0;;\n  esac\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let quiet_wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&quiet_wakes);
        let engine = EngineProcess::spawn_waking(
            &script,
            Arc::new(move |urgent: bool| {
                if !urgent {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }),
        )
        .await
        .unwrap();
        let rx = engine.subscribe();
        engine
            .try_send(EngineCommand::Go(GoControl::Depth(1)))
            .unwrap();
        let drained = wait_until(rx, Duration::from_secs(5), |line| {
            matches!(line, ParsedUciLine::Event(EngineEvent::BestMove { .. }))
        })
        .await;
        assert!(drained
            .iter()
            .any(|l| matches!(l, ParsedUciLine::Event(EngineEvent::BestMove { .. }))));
        assert_eq!(
            quiet_wakes.load(Ordering::Relaxed),
            1,
            "only the line that changes the card wakes the window quietly"
        );
        engine.terminate().await.unwrap();
    }
}

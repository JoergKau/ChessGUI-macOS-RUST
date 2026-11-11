# SiliconBoard — Release Notes

## v1.2.0

**Final-audit hardening:** the 2026-11-11 audit findings are applied. A draw
offer can no longer act on an evaluation of a position the game has left, and
an engine card greys its score out while the search has moved to a deeper
level than the score came from (hover the score badge for the reason).

**Fairer tournaments:** each game reseeds the opening-book picker, so games from
the same EPD position get independent book lines instead of replaying one
sequence.

**Smoother PGN use:** a slow PGN read can no longer replace a file opened after
it, and a Black-first game from a FEN round-trips through export and import.

**Two-engine performance:** the Tokio runtime now uses four workers, the
engine event queue carries only search-boundary lines with a 256-event frame
cap, the 500 ms frame tick runs only while a handshake is pending, and analysis
arrows no longer allocate on every frame. The broadcast buffer is smaller.

**Diagnostics:** development and internal release builds keep their symbol
table for useful crash reports; only the packaged `.app` is stripped.

## v1.1.0

**Setup Board:** _Game → Setup board…_ opens a position editor in the right
dashboard, in place of the engine cards. Pick a piece from the palette and
left-click a square to place it; right-click removes a piece. Clear board,
the side-to-move combo and the four castling-right checkboxes cover the rest.
The FEN field follows every edit and also accepts a pasted position, and the
editor lists every reason a position cannot be played. Close applies a valid
position as a fresh game; Cancel leaves the current game unchanged.

**Engine recovery:** an engine killed by the watchdog is now restarted even
after its slot has turned offline, so its evaluation and analysis arrow come
back instead of staying frozen.

**Palette contrast:** the Setup Board piece palette uses a light-grey backing,
so black pieces stay visible against the dark panel.

**Documentation:** the README now has a short session-log section, covering the
log location, rotation, the **Engine → Session log** switch and the
`SILICONBOARD_LOG` / `SILICONBOARD_LOG_FILE` overrides.

## v1.0.1

**New in analysis mode:** each engine's current best move is drawn as an arrow
on the board, from the centre of the from-square to the centre of the
destination square. Engine A is green and engine B red, and the arrow follows
the search as a new best move is found. When both engines choose the same move
only one arrow is drawn. View → Analysis arrows switches the arrows on or off
(on by default).

## v1.0

A native macOS (Apple Silicon) UCI chess GUI in Rust: board on the left, live
engine analysis on the right, move list along the bottom, and a flat toolbar
across the top.

### What works

- **Analysis:** two engine slots (A and B), Start / Abort, per-card play/pause,
  and each card's engine name is a pulldown of the loaded A/B engines and the
  engines used before (remembered across launches) — the same list as the
  tournament window — with Browse… for another file.
  live depth, nodes, speed, evaluation and a two-line principal variation.
  Figurine piece letters in the PV are optional (off by default).
- **Play:** human vs engine (White or Black, against A or B) and engine A vs
  engine B. Human moves by click-click or drag-and-drop, with promotion and
  castling.
- **Play from a FEN:** paste a position into the top-bar FEN field and choose
  _Game → Start from here…_: both engines play on with the current time
  control, and the PGN starts from that position (`SetUp`/`FEN` tags).
- **Clocks:** no clock, sudden death, Fischer increment and fixed time per move;
  player bars above and below the board; flags end the game.
- **Opening book:** PolyGlot (`.bin`) books are loaded on demand —
  Engine → Load PolyGlot book…, and the choice is remembered for the next
  start-up; no book is loaded by default. Weighted-random book replies in
  games are switchable — the switch starts off until a book is loaded.
  The book box labels the current opening by name
  (ECO table) and keeps the last known name after the line leaves the book.
- **Syzygy tablebases:** _Engine → Load tablebases…_ takes the user's
  tablebase folder(s) — several folders are joined into one string
  (`:` on macOS/Linux, `;` on Windows) — and sends it as the `SyzygyPath`
  option to the loaded engines and to every engine started afterwards.
- **Games:** resign, draw by agreement, and a FIDE threefold-repetition claim
  (Game → Claim draw (threefold)), with the result written to the PGN.
- **Tournaments:** Tournament → New tournament… opens a two-engine match
  window (engines, Hash/Nalimov cache, 4+1 clock, games, reverse colours, an
  EPD start book and/or PolyGlot `.bin` move book, resign/draw adjudication and
  a PGN path). Start plays the match on the main board, one game after another, with
  the live move list, clocks and engine cards. EPD books record their start
  position as `SetUp`/`FEN`; a `.bin` move book is replayed from move 1 when it
  contains the line to that position, so the opening moves are in the PGN too. The movable Tournament window shows the head-to-head score in standard chess
  notation (`A vs B: 2½–3½`, draws count half a point) and the results of
  finished games; the end-of-match result pop-up lists them too.
- **Files:** open and save PGN (mainline), copy PGN, copy/save FEN.
- **Multi-game PGN:** opening a file with more than one game opens a movable
  **PGN games** window with a scrollable list (#, White, Black, Result, Date,
  Event, plus the PGN `Termination` reason next to the result when the file
  provides it). Every row spans the full window width and the columns are
  distributed across it. Click anywhere in a row to load that game into the
  normal bottom move list; the window
  stays open so another can be picked, even after choosing a game mode. The
  bottom move list shows the result and the PGN `Termination` reason next to
  it. Close it
  with the top Close button or the title-bar X; File → PGN games… reopens it.
- **Board themes:** View → Board theme picks any PNG/JPEG board from
  `public/images/board/`; the choice is remembered between launches.
- **Window layout:** the main window has a minimum width derived from the saved
  right-panel width plus a 320-point board, so the board can no longer be
  squeezed out of view. The saved layout (right panel 917, window 1246) is the
  minimum for that configuration.
- **Piece sets:** View → Piece set picks any complete cburnett-family set from
  `public/piece/` (41 here); the choice is remembered between launches.
- **Figurine notation:** View → Figurines in move list (on by default) and
  Figurines in engine evaluation (off by default). Saved and copied PGN files
  always stay standard SAN.
- **Engines:** UCI handshake and options, a watchdog that restarts a stalled
  or exited engine, loaded engines remembered between launches. While a search
  runs, the watchdog enforces deadlines: `isready` answered within 10 s; a
  `bestmove` within 3 s of `stop`; a move ends within its time plus 3 s; an
  analysis shows progress (a new depth or node count) within 60 s. Only real
  replies and progress count as signs of life, so noise does not keep a stuck
  engine alive. The status line gives the reason for a stall. The deadlines are
  checked by the engine's own monitor task, so they run while the window is
  hidden. A slot may restart three times in ten minutes; after that the engine
  stays offline, and in a match that stops the match. The
  Engine A/B options window is a grouped settings page: engine header, dashed
  sections, labeled fields in two columns, and a Default button that resets
  every option.
- **Diagnostics:** one session log per run in `~/Library/Logs/SiliconBoard/`;
  the newest five are kept. A log continues in a numbered file at 5 MB, so the
  folder stays below about 25 MB.
  It records lifecycle messages, UCI commands, engine replies (`readyok`,
  `bestmove` with its timing), engine stderr, process exit status and the reason
  for any stall, which is what is needed to diagnose an engine that goes offline.
  Engine → Session log switches the file on or off (on by default, remembered
  between launches). Idle engines are no longer pinged, so a finished match
  leaves the log and the CPU alone.
- **Settings:** time control, book play and the last loaded book, engine
  option values, window size and
  panel sizes survive a restart. The settings file is replaced atomically, so a
  crash during a save keeps the previous settings.
- **Process cleanup:** every engine runs in its own process group, and a stop,
  stall or exit ends the whole group, helpers included. Engines left running by
  a crash or a forced quit are stopped at the next start.
- **Engine files:** launcher scripts (shell, Java and Python wrappers) can be
  loaded. A file that cannot run on this Mac is refused with the reason before
  it starts. An engine runs from its own folder, and a failed start shows the
  exit status and the engine's last message.
- **Players on the board:** a loaded PGN shows its White and Black names on the
  player bars. A new game, a FEN, a play mode or a tournament replaces them.
- **About:** the About item of the app menu shows the standard macOS panel:
  "SiliconBoard", "Version 1.0.1", the project page as a clickable link
  (https://github.com/JoergKau) and the licence, with the application icon.
- **Icon:** the application icon is shown in the window and is included as
  `SiliconBoard.icns` for the app bundle.
- **Warnings:** setting Threads whose total over both engines exceeds the core
  count, or MultiPV above 1, shows a warning; the value is still applied.
- **Performance:** engine output wakes the window directly, coalesced to about
  30 fps; an idle loaded engine causes no redraws. The board paint measures
  8–11 µs, and a live run with two Stockfish 19 instances (2.6–2.9 M NPS
  combined) measured a 0.27 ms average CPU cost per frame, 10.15 ms worst.

### Requirements

- macOS on Apple Silicon.
- A UCI engine binary (Stockfish 19 is used in the tests).
- The workspace `public/` asset tree, or `SILICONBOARD_PUBLIC_ROOT` pointing at
  one.

### Known limitations

Tracked in `features-list.md`:

- Linear history; browsing is read-only (variation trees are not built).
- PGN import keeps the mainline only (no comments or variations on import).
- One principal variation per engine (no multi-PV display).
- SVG board themes in `public/images/board/svg/` are not offered.
- The design's card extras (expand PV, copy PV, pin, settings) are not built.
- Tablebases help endgame play only through the engines' own probing
  (Syzygy via `SyzygyPath`); the GUI itself does not read tablebase files.

### Verification

```bash
cd siliconboard
cargo test --bin siliconboard
cargo clippy --all-targets
cargo build --release
cargo test --release --bin siliconboard board_paint -- --ignored --nocapture
```

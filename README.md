# SiliconBoard

A native **macOS (Apple Silicon) UCI chess GUI** written in Rust. Board on the
left, live engine analysis on the right, the move list along the bottom, and a
flat toolbar across the top.

- **Analysis:** two engine slots with live depth, nodes, speed, evaluation
  and principal variation; a best-move arrow on the board for each engine
  (green for A, red for B); Start / Abort; per-card play/pause. Each card's
  engine name is a pulldown of the known engines (loaded A/B and the engines
  used before, remembered across launches, the same list as the tournament
  window) with Browse….
- **Play:** human vs engine (either colour), engine vs engine, clocks with
  flag handling, resign, draw offers, threefold-repetition claims.
- **Play from a position:** paste a FEN into the top bar and choose
  _Game → Start from here…_ — two engines play on from that position with the
  current time control, and the saved PGN starts there (`SetUp`/`FEN`).
- **Set up a position:** _Game → Setup board…_ opens an editor with a piece
  palette, side-to-move and castling controls, a FEN field and a validity
  check. Close applies the position; Cancel leaves the game unchanged.
- **Engine care:** a watchdog restarts or stops an unresponsive engine,
  launcher scripts and broken files are checked before start, failed starts
  report a reason.
- **Tournaments:** a two-engine match dialog plays a configurable engine
  match on the main board (time control, books, adjudication, PGN output).
- **Everything else:** PolyGlot opening books, PGN open/save including a
  multi-game browser, FEN import/export, board themes and piece sets,
  figurine piece letters, persistent settings, a session log.

A short summary of what SiliconBoard does lives in
[features-list.md](features-list.md); what ships in v1.2.0 is in
[RELEASE_NOTES.md](RELEASE_NOTES.md).

## The ready-to-run app

The repository contains the built, signed app:

```
SiliconBoard-1.2.0-macos-arm64.zip
```

It is the same bundle the packaging script produces — one folder
(`SiliconBoard-1.2.0/`) holding `SiliconBoard.app`, `LICENSE` and this
README — and it is **committed to the repository**, so no build is needed to
run SiliconBoard. It is for macOS on Apple Silicon.

To use it:

1. Download the zip (or clone the repository) and double-click the zip.
2. Drag `SiliconBoard.app` from the unpacked folder into `/Applications`
   (or run it from the folder).
3. The first launch may be blocked by macOS because the download is
   quarantined: **right-click the app → Open**, then confirm **Open**.
   SiliconBoard is ad-hoc signed but not notarized, so this one-time step is
   expected.
4. The app ships no engine. Load yours via _Engine → Load engine A…_ (and B
   if you want two engines); see the next section.

The SHA-256 of the zip is printed by `./package-macos.sh` when it rebuilds
the app and is listed with each release; check a download with:

```shell
shasum -a 256 SiliconBoard-1.2.0-macos-arm64.zip
```

## Requirements

- macOS on Apple Silicon (other platforms are out of scope for now)
- Rust 2021 edition (`cargo`)
- At least one **UCI engine** of your choice — SiliconBoard ships none; see
  the next section to install one

## Loading a UCI engine (example: Stockfish)

No engine is distributed with SiliconBoard: _Engine → Load engine A…_ and
_Load engine B…_ always open the native file picker, and whichever engine
the user points them at fills the slot (loading into an occupied slot
replaces it). The chosen file is checked before it starts — a launcher
script must name an existing interpreter, and a non-program or a non-UCI
binary is refused with the reason. Each engine keeps its own option values,
which are sent again after a restart and restored at the next launch.

Installing the free Stockfish:

1. Download from <https://stockfishchess.org/download/> (the macOS build).
2. Unpack the file; the UCI binary inside is
   `stockfish-macos-universal`.
3. In the GUI, _Engine → Load engine A…_ and point it at that binary.

The process is the same for any other UCI engine: unpack it and point the
menu at its executable.

## Building and running

```shell
cargo run --release
```

The **asset tree** is bundled in this repository under `public/`: board
themes under `public/images/board/`, piece sets (twelve SVGs per folder)
under `public/piece/`, the figurine font at `public/font/lichess-chess.ttf`
and the CC0 ECO openings table at `public/data/openings/openings.tsv` — a
fresh clone runs fully dressed. The per-asset licences (lichess and its
piece authors, typically CC-BY-SA) are documented in
[`public/LICENSING.md`](public/LICENSING.md). SiliconBoard finds the tree
via `$SILICONBOARD_PUBLIC_ROOT`, by walking up from the executable, or as
`./public` (the bundled copy is normally found by the walk-up);
`$SILICONBOARD_PUBLIC_ROOT` points a development setup at a different tree.
The workspace copy is refreshed from the canonical shared tree with
`./sync-public.sh`. The window icon is compiled
in from `assets/icon/` (`siliconboard-512.png`).

SiliconBoard does **not** ship an opening book and loads none by default:
the app starts bookless. Load one via _Engine → Load PolyGlot book…_ —
the choice is kept in the settings and restored at the next start-up.
`$SILICONBOARD_BOOK` names an explicit start-up book instead. For a book
to download, we recommend the
[PolyGlot book collection](https://github.com/ChrisWhittington/polyglot-books).

## Syzygy tablebases

Engines can use Syzygy tablebase files (`*.rtbw` / `*.rtbz`) for endgame
knowledge — perfect play once few pieces are on the board. Point the GUI at
your installation once via _Engine → Load tablebases…_: enter the tablebase
folder(s) in the dialog (or pick them with **Browse…**, which accepts
several folders) and press Apply. The path is kept in the settings and sent
to every loaded engine — and to every engine loaded later — as the
`SyzygyPath` option.

- Several folders are joined into **one string**, separated by `:` on macOS
  and Linux (`;` on Windows). This matters when the 5-men and 6-men files
  live in different folders, as in
  `/Users/you/EGTB/Syzygy-3-4-5:/Users/you/EGTB/Syzygy-6`.
- Applying an empty line clears the tablebases again: the loaded engines get
  an empty path, and engines started afterwards get none.
- The GUI forwards the string untouched — which folder holds which files,
  and whether an engine supports tablebases at all, is the engine's
  business; a user choosing paths knows what they are doing. Look in the
  session log for the engine's own report, e.g.
  `Found 510 WDL and 510 DTZ tablebase files (up to 6-man)`. The files and
  what to download are described at
  [syzygy-tables.info](https://syzygy-tables.info/).

## Session log (troubleshooting)

SiliconBoard writes one log file per run to
`~/Library/Logs/SiliconBoard/`, named
`siliconboard-<UTC time>.log`. It records start-up and settings messages,
engine life-cycle events, the UCI commands and replies exchanged with each
engine, engine stderr, and errors. _Engine → Session log_ switches file
logging at runtime; it is on by default and the choice is kept in the
settings.

The newest five logs are kept. A log that reaches 5 MB is continued in
`-2.log`, `-3.log`, …, so the folder stays at about 25 MB.

To look at the logs:

```shell
open ~/Library/Logs/SiliconBoard
```

The newest file is the run you were in. Search for `WARN` and `ERROR` first,
then for the engine's file name. For a bug report, attach the newest log and
describe what you did.

Two environment variables change logging for one run:

- `SILICONBOARD_LOG=trace` also records every engine line and the
  `isready` / `readyok` heartbeat. The default file filter is
  `info,siliconboard::engine=debug,siliconboard::app=debug`.
- `SILICONBOARD_LOG_FILE=/path/to/file.log` writes one named file instead of
  the per-run log. That file is truncated at start-up and is not rotated or
  pruned.

## Release as a macOS `.app` bundle

```shell
./package-macos.sh
```

The script runs `cargo build --release`, then assembles
`SiliconBoard.app` in the repository root: the binary as
`Contents/MacOS/SiliconBoard`, the icon and the asset tree as
`Contents/Resources/public` — the layout the asset resolver checks first in
a bundle (unit-tested in `src/assets.rs`) — and writes
`Contents/Info.plist` with the version taken from `Cargo.toml`, the bundle
identifier `org.chess-gui.SiliconBoard` and the About-panel copyright
(`macos_about.rs` reads both from the bundle). `plutil -lint` guards the
plist, and the bundle is signed sign ad-hoc:

```shell
codesign --force --deep --sign - SiliconBoard.app   # the script does this
```

The `.app` bundle is a build artefact and stays out of git. The distributable
zip is committed to the repository at the repo root, so the download sits next
to the source.

**Distribution:** the script also packs the signed app into
`SiliconBoard-<version>-macos-arm64.zip` — one folder holding the app plus
`LICENSE` and `README.md` — and prints the zip's SHA-256 for the release
listing. The app is signed _before_ it is zipped (modifying a signed app
breaks the seal), and the zip is verified by unzipping it and re-checking
the signature.

**Install locally:** `./package-macos.sh` installs the freshly signed bundle
into `/Applications` by default, replacing an existing copy there, and then
**removes the repo copy and unregisters it with Launch Services** — so
exactly one app entry ever exists, and it always lives in `/Applications`.
(Installing is the default: a bundle left in the dev folder
gets registered by Launch Services as well and Launchpad shows two icons.)
The old `--install`/`-i` flags still work and mean the same; `--no-install`
(or `-n`) skips the install and leaves the bundle in the repo folder. An
unknown argument is rejected with the usage line. If `/Applications`
cannot be written, the script asks once for administrator rights; cancelling
keeps the bundle in the repo folder, where it can be dragged by hand.

**What to do with it:** double-click to launch, or drag it to
`/Applications`. A copy that came over the internet is quarantined by
macOS: right-click → **Open** the first time.

Besides the assets, the bundle carries the start-up PGN
(`Contents/Resources/PGN/WorldChamp1972.pgn`), so launch opens
Spassky–Fischer, game 1, and the multi-game list is ready. On first
launch you still load the rest:

2. Load an engine via _Engine → Load engine A…_ (and B as needed) — no UCI
   engine is distributed with SiliconBoard; the engines keep running from
   wherever their binaries live (e.g. `~/engines/`).
3. For opening-book play in games, load your book once via _Engine → Load
   PolyGlot book…_ — the choice is remembered across starts.

## Development

```shell
cargo test --bin siliconboard     # unit + real-engine integration tests
cargo clippy --all-targets        # zero warnings
```

The real-engine tests need an available UCI engine fixture; it fails
(rather than skips) when it is missing. The opening-book tests use small
built-in books, so no external `.bin` file is needed.

## AI Assisted by DeepSeek V.4.1 Flash

## Licence

MIT — see [LICENSE](LICENSE).

**Credits:** the bundled board themes, piece sets and the figurine font are
**lichess artwork** (`lichess.org`, asset repository
[`lichess-org/lifat`](https://github.com/lichess-org/lifat)), by the
respective CC-BY-SA-type upstream authors (e.g. the default `cburnett`
pieces by Colin M. L. Burnett). The ECO openings table is CC0. The summary
with sources and terms per asset family is in
[`public/LICENSING.md`](public/LICENSING.md).

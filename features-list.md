# SiliconBoard — Features

SiliconBoard is a native macOS UCI chess GUI for Apple Silicon. The board
sits on the left, the engine analysis on the right, and the move list along
the bottom — with a clean dark theme.

## Play and analyse

- Live analysis with **two UCI engines side by side** — evaluation, depth,
  nodes, speed and the principal variation in standard notation.
- **Play** against an engine as White or Black, or watch **engine vs
  engine**, with configurable time controls, increments and live clocks.
- **Play from a position**: paste a FEN and choose _Game → Start from
  here…_ — two engines play on with the current time control, and the PGN
  starts from that position.
- **Set up a position**: _Game → Setup board…_ opens a piece editor with a
  palette, side-to-move and castling controls, a FEN field and a live validity
  check. Apply the position on Close; Cancel leaves the game unchanged.
- Opening-book moves in games with any PolyGlot `.bin` book you load, plus a
  suggestions box named after the ECO opening.
- Resign, draw offers (accepted by the engine within a score margin),
  threefold-repetition claims, engine-vs-engine games that end themselves on
  threefold repetition or the fifty-move rule, and results written to the PGN.
- Robust engine handling: a file that is not a working UCI engine is
  explained, and an unresponsive engine is restarted automatically.

## PGN and FEN

- Open, save, and copy PGN and FEN.
- **Multi-game PGN browser**: a scrollable games list (White, Black, result,
  event, date) docked next to the analysis; click a game to load it.
- Opens with a classic World Championship game selected.
- Player names from a loaded PGN are shown on the board's player bars.

## Engines and tournaments

- Load engine files directly (including launcher scripts); an options
  dialog for every UCI option an engine declares. Each engine card's name is
  a pulldown of the known engines (loaded A/B and the engines used before,
  remembered across launches, the same list as the tournament window) with
  Browse… for another file.
- **Syzygy tablebases** via _Engine → Load tablebases…_: pick your
  tablebase folder(s) — 5-men and 6-men sets in separate folders are
  supported — and the path is given to every engine as its `SyzygyPath`
  option. _Engine → Tablebases in engines_ switches the configured path on
  or off without forgetting it.
- **Two-engine tournaments** on the main board: time control, alternating
  colours, resign and draw adjudication, optional EPD/PolyGlot opening
  books, and one PGN with every game.

## Look and feel

- Board themes and piece sets; figurine piece symbols (`♘f6`) in the move
  list and the evaluation.
- Everything — engines, engine options, time control, themes, window layout
  — is restored on the next launch.

## Not (yet) included

- Variation trees; PGN comments and variations; multi-PV engine lines;
  in-GUI tablebase probing (the engines do the probing); other platforms.

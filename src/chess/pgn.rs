//! PGN reading and writing for single games (mainline only).
//!
//! * [`to_pgn`] renders a [`GameState`] as a PGN game: tag pairs, a `FEN`
//!   header when the game does not start from the standard position, and
//!   wrapped movetext.
//! * [`parse_pgn`] reads the first game of a PGN text. Comments, NAGs,
//!   variations and move numbers are skipped; each SAN move is validated
//!   against the position it is played in.
//!
//! Variations are dropped on purpose: the game model is linear (see the
//! project plan, §13), so a PGN that is only a line of play round-trips
//! exactly.

use std::fmt::Write;

use shakmaty::{fen::Fen, san::SanPlus, Color, EnPassantMode, Position};

use crate::chess::game_state::{Ending, FenError, GameState};
use crate::chess::PieceColor;

/// Movetext is wrapped before this many columns.
const LINE_WIDTH: usize = 80;

/// FEN of the standard starting position.
const STANDARD_START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

/// The seven mandatory PGN tag pairs, plus any extra tags a caller wants
/// (for example the tournament's `TimeControl` and `Termination` tags).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgnHeaders {
    /// `Event` tag.
    pub event: String,
    /// `Site` tag.
    pub site: String,
    /// `Date` tag.
    pub date: String,
    /// `Round` tag.
    pub round: String,
    /// `White` tag.
    pub white: String,
    /// `Black` tag.
    pub black: String,
    /// Additional tags written after the mandatory seven.
    pub extra_tags: Vec<(String, String)>,
}

impl Default for PgnHeaders {
    /// The headers of a game saved from the board.
    fn default() -> Self {
        Self {
            event: "SiliconBoard game".to_string(),
            site: "?".to_string(),
            date: "????.??.??".to_string(),
            round: "?".to_string(),
            white: "?".to_string(),
            black: "?".to_string(),
            extra_tags: Vec::new(),
        }
    }
}

/// Result token for the game: the ending when the game loop decided it
/// (resignation, agreed draw, flag), otherwise the final position's result.
/// `*` while the game is still running.
pub fn result_token(game: &GameState, ending: Option<Ending>) -> &'static str {
    if let Some(ending) = ending {
        return ending.result_token();
    }
    let pos = game.final_position();
    if pos.is_checkmate() {
        // The side to move is mated.
        match pos.turn() {
            Color::White => "0-1",
            Color::Black => "1-0",
        }
    } else if pos.is_game_over() {
        "1/2-1/2"
    } else {
        "*"
    }
}

/// Render the whole recorded game as PGN, including moves after the cursor.
/// The result tag comes from `ending`, or from the final position when the
/// game loop did not end the game.
pub fn to_pgn(game: &GameState, ending: Option<Ending>) -> String {
    to_pgn_with_headers(game, result_token(game, ending), &PgnHeaders::default())
}

/// Render a game with explicit headers and a result token. Tournament games
/// use this so each game carries its event, round, player names, time control
/// and termination reason.
pub fn to_pgn_with_headers(game: &GameState, result: &str, headers: &PgnHeaders) -> String {
    let start = game.start_position();
    let mut out = String::new();
    push_tag(&mut out, "Event", &headers.event);
    push_tag(&mut out, "Site", &headers.site);
    push_tag(&mut out, "Date", &headers.date);
    push_tag(&mut out, "Round", &headers.round);
    push_tag(&mut out, "White", &headers.white);
    push_tag(&mut out, "Black", &headers.black);
    push_tag(&mut out, "Result", result);
    for (name, value) in &headers.extra_tags {
        push_tag(&mut out, name, value);
    }
    let start_fen = Fen::from_position(&start, EnPassantMode::Legal).to_string();
    if start_fen != STANDARD_START_FEN {
        push_tag(&mut out, "SetUp", "1");
        push_tag(&mut out, "FEN", &start_fen);
    }
    out.push('\n');

    let mut tokens: Vec<String> = Vec::new();
    let first_turn = start.turn();
    let first_number = start.fullmoves().get();
    for (i, ply) in game.history().iter().enumerate() {
        let side = if i % 2 == 0 {
            first_turn
        } else {
            first_turn.other()
        };
        let number = first_number + ((i as u32) + u32::from(first_turn == Color::Black)) / 2;
        match side {
            Color::White => tokens.push(format!("{}.", number)),
            Color::Black if i == 0 => tokens.push(format!("{}...", number)),
            Color::Black => {}
        }
        tokens.push(ply.san.to_string());
    }
    tokens.push(result.to_string());

    let mut line = String::new();
    for token in tokens {
        if !line.is_empty() && line.len() + 1 + token.len() > LINE_WIDTH {
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&token);
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// Append one `[Name "value"]` tag pair. Quotes and backslashes in the value
/// are escaped as the PGN standard requires.
fn push_tag(out: &mut String, name: &str, value: &str) {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    let _ = writeln!(out, "[{} \"{}\"]", name, escaped);
}

/// Parse the first game of a PGN text into a [`GameState`] at its final
/// position. Test convenience; the app uses [`parse_pgn_with_report`].
///
/// # Errors
/// [`PgnError::Empty`] if the text holds no movetext or tags,
/// [`PgnError::Fen`] for an unusable `FEN` tag,
/// [`PgnError::IllegalMove`] when a move does not fit the position it is in.
#[cfg(test)]
pub fn parse_pgn(text: &str) -> Result<GameState, PgnError> {
    parse_pgn_with_report(text).map(|import| import.game)
}

/// Like [`parse_pgn`], but also reports what was left out: variations,
/// comments and further games in the file. The app shows this so nothing is
/// dropped silently.
pub fn parse_pgn_with_report(text: &str) -> Result<PgnImport, PgnError> {
    let (tags, movetext, extra_games) = split_tags_and_movetext(text);
    if tags.is_empty() && movetext.trim().is_empty() {
        return Err(PgnError::Empty);
    }
    let result = tags
        .iter()
        .find(|(name, _)| name == "Result")
        .and_then(|(_, value)| PgnResult::parse(value))
        .or_else(|| result_from_movetext(&movetext));
    let termination = tags
        .iter()
        .find(|(name, _)| name == "Termination")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let tag_value = |wanted: &str| {
        tags.iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let players = (tag_value("White"), tag_value("Black"));
    let mut game = match tags.iter().find(|(name, _)| name == "FEN") {
        Some((_, fen)) => GameState::from_fen(fen).map_err(PgnError::Fen)?,
        None => GameState::new(),
    };
    let (tokens, variations, comments) = movetext_tokens(&movetext);
    for (index, token) in tokens.into_iter().enumerate() {
        let position = game.position();
        // `SanPlus` accepts the check and mate suffixes (`Qb6+`, `Qh4#`).
        let parsed = SanPlus::from_ascii(normalise_castling(&token).as_bytes()).map_err(|_| {
            PgnError::IllegalMove {
                ply: index + 1,
                san: token.clone(),
            }
        })?;
        let m = parsed
            .san
            .to_move(&position)
            .map_err(|_| PgnError::IllegalMove {
                ply: index + 1,
                san: token.clone(),
            })?;
        game.apply_move(m).map_err(|_| PgnError::IllegalMove {
            ply: index + 1,
            san: token.clone(),
        })?;
    }
    Ok(PgnImport {
        game,
        ignored: IgnoredContent {
            variations,
            comments,
            games: extra_games,
        },
        result,
        termination,
        players,
    })
}

/// The result token at the end of the movetext, when the `Result` tag is
/// missing or `*`.
fn result_from_movetext(movetext: &str) -> Option<PgnResult> {
    movetext.split_whitespace().rev().find_map(PgnResult::parse)
}

/// The result token of a PGN game, when the file states one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PgnResult {
    /// `1-0`.
    WhiteWin,
    /// `0-1`.
    BlackWin,
    /// `1/2-1/2`.
    Draw,
}

impl PgnResult {
    /// Parse a PGN result token. `*` and anything else give `None`.
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim() {
            "1-0" => Some(Self::WhiteWin),
            "0-1" => Some(Self::BlackWin),
            "1/2-1/2" => Some(Self::Draw),
            _ => None,
        }
    }

    /// The winning side, or `None` for a draw.
    pub fn winner(self) -> Option<PieceColor> {
        match self {
            Self::WhiteWin => Some(PieceColor::White),
            Self::BlackWin => Some(PieceColor::Black),
            Self::Draw => None,
        }
    }

    /// The PGN result token.
    pub fn token(self) -> &'static str {
        match self {
            Self::WhiteWin => "1-0",
            Self::BlackWin => "0-1",
            Self::Draw => "1/2-1/2",
        }
    }
}

/// A parsed PGN game together with the content that was skipped.
#[derive(Clone, Debug)]
pub struct PgnImport {
    /// The mainline game, at its final position.
    pub game: GameState,
    /// What was skipped while reading it.
    pub ignored: IgnoredContent,
    /// The result the file states, if any.
    pub result: Option<PgnResult>,
    /// `Termination` tag, when the file gives a reason such as
    /// `Black wins on time` or `Draw by repetition`.
    pub termination: String,
    /// The `White` and `Black` tags, empty when the file has none.
    pub players: (String, String),
}

/// One game in a multi-game PGN file. The moves are kept as text and parsed
/// only when the user selects the game.
#[derive(Clone, Debug)]
pub struct PgnGameEntry {
    /// 1-based game number.
    pub number: usize,
    /// `White` tag.
    pub white: String,
    /// `Black` tag.
    pub black: String,
    /// `Event` tag.
    pub event: String,
    /// `Date` tag.
    pub date: String,
    /// `Termination` tag, when the game states why it ended, e.g.
    /// `Black wins on time` or `Draw by repetition`.
    pub termination: String,
    /// Result token, when the game states one.
    pub result: Option<PgnResult>,
    /// The full PGN text of this game, parsed on selection.
    pub text: String,
}

/// All games found in a PGN file.
#[derive(Clone, Debug)]
pub struct PgnCollection {
    /// File name, for the browser heading.
    pub name: String,
    /// Games in file order.
    pub games: Vec<PgnGameEntry>,
}

impl PgnCollection {
    /// Parse one selected game fully.
    pub fn load_game(&self, index: usize) -> Result<PgnImport, PgnError> {
        let entry = self.games.get(index).ok_or(PgnError::Empty)?;
        parse_pgn_with_report(&entry.text)
    }

    /// Whether the file holds more than one game.
    pub fn is_multi_game(&self) -> bool {
        self.games.len() > 1
    }
}

/// Split a PGN text into games and read only their headers. Move text is kept
/// for [`PgnCollection::load_game`], so opening a file with thousands of games
/// stays fast.
pub fn parse_pgn_collection(text: &str) -> Result<PgnCollection, PgnError> {
    let slices = split_games(text);
    if slices.is_empty() {
        return Err(PgnError::Empty);
    }
    let mut games = Vec::with_capacity(slices.len());
    for slice in slices {
        let (tags, movetext, _) = split_tags_and_movetext(slice);
        if tags.is_empty() && movetext.trim().is_empty() {
            continue;
        }
        let tag = |name: &str| {
            tags.iter()
                .find(|(tag_name, _)| tag_name == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let result = tags
            .iter()
            .find(|(name, _)| name == "Result")
            .and_then(|(_, value)| PgnResult::parse(value))
            .or_else(|| result_from_movetext(&movetext));
        games.push(PgnGameEntry {
            number: games.len() + 1,
            white: tag("White"),
            black: tag("Black"),
            event: tag("Event"),
            date: tag("Date"),
            termination: tag("Termination"),
            result,
            text: slice.to_string(),
        });
    }
    if games.is_empty() {
        return Err(PgnError::Empty);
    }
    Ok(PgnCollection {
        name: String::new(),
        games,
    })
}

/// Split a PGN text at the tag section of each following game.
fn split_games(text: &str) -> Vec<&str> {
    let mut games = Vec::new();
    let mut start = 0usize;
    let mut offset = 0usize;
    let mut in_movetext = false;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim();
        let is_tag = trimmed.starts_with('[');
        if is_tag && in_movetext {
            if line_start > start {
                games.push(&text[start..line_start]);
            }
            start = line_start;
            in_movetext = false;
        } else if !is_tag && !trimmed.is_empty() {
            in_movetext = true;
        }
    }
    if start < text.len() {
        games.push(&text[start..]);
    }
    games
}

/// Counts of PGN content the importer does not keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IgnoredContent {
    /// Variations (`( … )`) at any depth.
    pub variations: usize,
    /// Comments (`{ … }` and `; …`).
    pub comments: usize,
    /// Further games after the first one in the file.
    pub games: usize,
}

impl IgnoredContent {
    /// Human-readable list such as `"1 variation, 2 comments"`, or `None`
    /// when nothing was skipped.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.variations > 0 {
            parts.push(format!("{} variation(s)", self.variations));
        }
        if self.comments > 0 {
            parts.push(format!("{} comment(s)", self.comments));
        }
        if self.games > 0 {
            parts.push(format!("{} further game(s)", self.games));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("ignored {}", parts.join(", ")))
        }
    }
}

/// Split the first game's tag pairs from its movetext. Also returns how many
/// further games follow it in the text; their content is not collected.
fn split_tags_and_movetext(text: &str) -> (Vec<(String, String)>, String, usize) {
    let mut tags = Vec::new();
    let mut movetext = String::new();
    let mut in_movetext = false;
    let mut previous_was_tag = false;
    let mut collecting = true;
    let mut extra_games = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        let is_tag = trimmed.starts_with('[');
        if is_tag && !in_movetext {
            if let Some(tag) = parse_tag(trimmed) {
                tags.push(tag);
            }
        } else if is_tag {
            // A tag line after movetext starts another game.
            if !previous_was_tag {
                extra_games += 1;
            }
            collecting = false;
        } else if collecting && (!trimmed.is_empty() || in_movetext) {
            in_movetext = true;
            movetext.push_str(line);
            movetext.push('\n');
        }
        previous_was_tag = is_tag;
    }
    (tags, movetext, extra_games)
}

/// Parse a `[Name "value"]` line. Returns `None` for malformed tags, which
/// are ignored rather than rejected.
fn parse_tag(line: &str) -> Option<(String, String)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (name, rest) = inner.split_once(' ')?;
    let rest = rest.trim();
    // Quoted values unescape `\"` and `\\`. Unquoted values are taken as written:
    // some exporters leave the quotes off, e.g. `[FEN rnbq… w KQkq - 0 1]`.
    let value = match rest.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(quoted) => quoted.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => rest.to_string(),
    };
    Some((name.to_string(), value))
}

/// Movetext → SAN move tokens, plus the number of variations and comments
/// that were skipped. Skips comments (`{…}` and `;…`), variations (`(…)`,
/// nested), NAGs (`$n`), move numbers (`12.` / `12...`), annotation glyphs
/// (`!?`) and the game result.
fn movetext_tokens(movetext: &str) -> (Vec<String>, usize, usize) {
    let mut tokens = Vec::new();
    let mut chars = movetext.chars().peekable();
    let mut variation_depth = 0usize;
    let mut variations = 0usize;
    let mut comments = 0usize;
    let mut current = String::new();

    let flush = |current: &mut String, tokens: &mut Vec<String>, depth: usize| {
        if current.is_empty() {
            return;
        }
        if depth == 0
            && !is_move_number(current)
            && !is_result(current)
            && !current.starts_with('$')
        {
            let cleaned = strip_move_number(current)
                .trim_end_matches(['!', '?'])
                .to_string();
            if !cleaned.is_empty() {
                tokens.push(cleaned);
            }
        }
        current.clear();
    };

    while let Some(c) = chars.next() {
        match c {
            '{' => {
                flush(&mut current, &mut tokens, variation_depth);
                comments += 1;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                }
            }
            ';' => {
                flush(&mut current, &mut tokens, variation_depth);
                comments += 1;
                for inner in chars.by_ref() {
                    if inner == '\n' {
                        break;
                    }
                }
            }
            '(' => {
                flush(&mut current, &mut tokens, variation_depth);
                if variation_depth == 0 {
                    variations += 1;
                }
                variation_depth += 1;
            }
            ')' => {
                flush(&mut current, &mut tokens, variation_depth);
                variation_depth = variation_depth.saturating_sub(1);
            }
            c if c.is_whitespace() => flush(&mut current, &mut tokens, variation_depth),
            c => current.push(c),
        }
    }
    flush(&mut current, &mut tokens, variation_depth);
    (tokens, variations, comments)
}

/// Remove a move number glued to the move: `1.e4` → `e4`, `12...Nc6` → `Nc6`.
fn strip_move_number(token: &str) -> &str {
    let digits = token.trim_start_matches(|c: char| c.is_ascii_digit());
    if digits.len() < token.len() && digits.starts_with('.') {
        digits.trim_start_matches('.')
    } else {
        token
    }
}

/// `12.`, `12...` and similar move-number tokens.
fn is_move_number(token: &str) -> bool {
    let digits = token.trim_end_matches('.');
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && token.len() - digits.len() <= 3
}

/// The four PGN result tokens.
fn is_result(token: &str) -> bool {
    matches!(token, "1-0" | "0-1" | "1/2-1/2" | "*")
}

/// Accept `0-0` / `0-0-0` (zero castling, common in exported games) as
/// `O-O` / `O-O-O`.
fn normalise_castling(token: &str) -> String {
    if token.starts_with("0-0") {
        token.replace('0', "O")
    } else {
        token.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// Errors raised while reading a PGN game.
#[derive(Debug)]
pub enum PgnError {
    /// The text holds neither tags nor movetext.
    Empty,
    /// The `FEN` tag does not describe a legal position.
    Fen(FenError),
    /// A move token does not match a legal move in its position.
    IllegalMove {
        /// 1-based half-move number within the game.
        ply: usize,
        /// The SAN token as written in the file.
        san: String,
    },
}

impl std::fmt::Display for PgnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PgnError::Empty => f.write_str("the PGN file contains no game"),
            PgnError::Fen(source) => write!(f, "invalid FEN tag: {}", source),
            PgnError::IllegalMove { ply, san } => {
                write!(f, "illegal move `{}` at half-move {}", san, ply)
            }
        }
    }
}

impl std::error::Error for PgnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PgnError::Fen(source) => Some(source),
            PgnError::Empty | PgnError::IllegalMove { .. } => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use shakmaty::Square;

    fn sq(name: &str) -> Square {
        Square::from_ascii(name.as_bytes()).unwrap()
    }

    fn play(game: &mut GameState, from: &str, to: &str) {
        game.play_squares(sq(from), sq(to), None).unwrap();
    }

    fn sans(game: &GameState) -> Vec<String> {
        game.active_history()
            .iter()
            .map(|p| p.san.to_string())
            .collect()
    }

    #[test]
    fn written_game_parses_back_to_the_same_moves() {
        let mut game = GameState::new();
        play(&mut game, "e2", "e4");
        play(&mut game, "e7", "e5");
        play(&mut game, "g1", "f3");
        play(&mut game, "b8", "c6");
        let text = to_pgn(&game, None);
        assert!(text.contains("1. e4 e5 2. Nf3 Nc6 *"), "movetext: {}", text);
        let parsed = parse_pgn(&text).unwrap();
        assert_eq!(sans(&parsed), vec!["e4", "e5", "Nf3", "Nc6"]);
        assert_eq!(parsed.fen(), game.fen());
    }

    #[test]
    fn result_tag_matches_checkmate() {
        // Fool's mate: Black wins.
        let mut game = GameState::new();
        play(&mut game, "f2", "f3");
        play(&mut game, "e7", "e5");
        play(&mut game, "g2", "g4");
        play(&mut game, "d8", "h4");
        assert_eq!(result_token(&game, None), "0-1");
        assert!(to_pgn(&game, None).contains("[Result \"0-1\"]"));
        assert!(to_pgn(&game, None).trim_end().ends_with("0-1"));
    }

    #[test]
    fn game_from_fen_writes_fen_tags_and_move_numbers_with_dots() {
        // Black to move at move 12: moves are written as `12...`.
        let fen = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 12";
        let mut game = GameState::from_fen(fen).unwrap();
        play(&mut game, "c7", "c5");
        let text = to_pgn(&game, None);
        assert!(text.contains("[SetUp \"1\"]"));
        assert!(text.contains("[FEN \""));
        assert!(text.contains("12... c5"), "movetext: {}", text);
        let parsed = parse_pgn(&text).unwrap();
        assert_eq!(parsed.fen(), game.fen());
    }

    #[test]
    fn comments_nags_variations_and_numbers_are_skipped() {
        let text = r#"
[Event "Test"]
[Result "1-0"]

1. e4 {the king's pawn} e5 $1 2. Nf3 (2. Bc4 Bc5 3. d3) 2... Nc6 ; line ends
3. Bb5 a6 1-0
"#;
        let game = parse_pgn(text).unwrap();
        assert_eq!(sans(&game), vec!["e4", "e5", "Nf3", "Nc6", "Bb5", "a6"]);
    }

    #[test]
    fn zero_castling_is_accepted() {
        let text = "[Event \"Castle\"]\n\n1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. 0-0 *\n";
        let game = parse_pgn(text).unwrap();
        assert_eq!(sans(&game).last().map(String::as_str), Some("O-O"));
    }

    #[test]
    fn illegal_move_reports_its_ply() {
        let text = "[Event \"Bad\"]\n\n1. e4 e6 2. e5 e5 *\n";
        match parse_pgn(text) {
            Err(PgnError::IllegalMove { ply, san }) => {
                assert_eq!(ply, 4);
                assert_eq!(san, "e5");
            }
            other => panic!("expected IllegalMove, got {:?}", other),
        }
    }

    #[test]
    fn empty_text_is_an_error_and_movetext_only_is_a_game() {
        assert!(matches!(parse_pgn("   \n"), Err(PgnError::Empty)));
        let game = parse_pgn("1. d4 d5 *").unwrap();
        assert_eq!(sans(&game), vec!["d4", "d5"]);
    }

    #[test]
    fn report_counts_variations_comments_and_further_games() {
        let text = "[Event \"One\"]\n\n1. e4 {a} e5 (1... c5) 2. Nf3 ; note\n*\n\n[Event \"Two\"]\n\n1. d4 *\n";
        let import = parse_pgn_with_report(text).unwrap();
        assert_eq!(sans(&import.game), vec!["e4", "e5", "Nf3"]);
        assert_eq!(
            import.ignored,
            IgnoredContent {
                variations: 1,
                comments: 2,
                games: 1
            }
        );
        assert_eq!(
            import.ignored.summary().as_deref(),
            Some("ignored 1 variation(s), 2 comment(s), 1 further game(s)")
        );
    }

    #[test]
    fn a_multi_game_pgn_is_split_into_headers_and_loads_on_demand() {
        let text = "[Event \"One\"]\n[White \"A\"]\n[Black \"B\"]\n[Result \"1-0\"]\n[Termination \"White wins on time\"]\n\n1. e4 e5 1-0\n\n[Event \"Two\"]\n[White \"C\"]\n[Black \"D\"]\n[Result \"0-1\"]\n\n1. d4 d5 0-1\n";
        let collection = parse_pgn_collection(text).unwrap();
        assert_eq!(collection.games.len(), 2);
        assert_eq!(collection.games[0].number, 1);
        assert_eq!(collection.games[0].white, "A");
        assert_eq!(collection.games[0].result, Some(PgnResult::WhiteWin));
        assert_eq!(collection.games[0].termination, "White wins on time");
        assert_eq!(collection.games[1].white, "C");
        assert_eq!(collection.games[1].result, Some(PgnResult::BlackWin));
        assert_eq!(collection.games[1].termination, "");
        let first = collection.load_game(0).unwrap();
        assert_eq!(first.game.total_plies(), 2);
        assert_eq!(first.result, Some(PgnResult::WhiteWin));
    }

    #[test]
    fn the_bundled_pgn_lists_all_games() {
        let text = bundled_pgn("WorldChamp1972.pgn");
        let collection = parse_pgn_collection(&text).unwrap();
        assert_eq!(collection.games.len(), 21);
        assert_eq!(collection.games[0].white, "Spassky, Boris V");
        assert_eq!(collection.games[0].black, "Fischer, Robert James");
        assert_eq!(collection.games[0].result, Some(PgnResult::WhiteWin));
        let import = collection.load_game(0).unwrap();
        assert!(import.game.total_plies() > 0);
    }

    #[test]
    fn clean_game_reports_nothing_ignored() {
        let import = parse_pgn_with_report("1. e4 e5 *").unwrap();
        assert_eq!(import.ignored.summary(), None);
    }

    #[test]
    fn the_pgn_result_is_read_from_the_tag_or_the_final_token() {
        let import = parse_pgn_with_report("[Result \"1-0\"]\n\n1. e4 e5 1-0\n").unwrap();
        assert_eq!(import.result, Some(PgnResult::WhiteWin));
        let import = parse_pgn_with_report("1. e4 e5 0-1\n").unwrap();
        assert_eq!(import.result, Some(PgnResult::BlackWin));
        let import = parse_pgn_with_report("1. e4 e5 1/2-1/2\n").unwrap();
        assert_eq!(import.result, Some(PgnResult::Draw));
        let import = parse_pgn_with_report("[Result \"*\"]\n\n1. e4 *\n").unwrap();
        assert_eq!(import.result, None);
        let import = parse_pgn_with_report("[Result \"0-1\"]\n\n1. e4\n").unwrap();
        assert_eq!(import.result, Some(PgnResult::BlackWin));
    }

    #[test]
    fn move_numbers_glued_to_moves_and_check_suffixes_are_read() {
        let game = parse_pgn("1.e4 e5 2.Nf3 Nc6 3.Bb5+ a6 4.Ba4 Nf6 5.O-O\n").unwrap();
        assert_eq!(
            sans(&game),
            vec!["e4", "e5", "Nf3", "Nc6", "Bb5", "a6", "Ba4", "Nf6", "O-O"]
        );
    }

    #[test]
    fn result_tag_follows_the_ending_the_game_loop_gives() {
        let mut game = GameState::new();
        game.play_squares(
            Square::from_ascii(b"e2").unwrap(),
            Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();
        assert!(to_pgn(&game, None).contains("[Result \"*\"]"));
        let resigned = Some(Ending::Resigned {
            loser: crate::chess::PieceColor::Black,
        });
        assert_eq!(result_token(&game, resigned), "1-0");
        let text = to_pgn(&game, Some(Ending::DrawAgreed));
        assert!(text.contains("[Result \"1/2-1/2\"]"));
        assert!(text.trim_end().ends_with("1/2-1/2"));
    }

    #[test]
    fn mate_suffix_is_read_and_the_result_is_derived_from_the_position() {
        // Scholar's mate: 4.Qxf7# ends the game with White winning.
        let game = parse_pgn("1.e4 e5 2.Bc4 Nc6 3.Qh5 Nf6 4.Qxf7#\n").unwrap();
        assert_eq!(sans(&game).last().map(String::as_str), Some("Qxf7#"));
        assert!(game.is_checkmate());
        assert_eq!(result_token(&game, None), "1-0");
    }

    /// Text of a PGN file that ships with the repository (`PGN/`).
    fn bundled_pgn(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("PGN")
            .join(name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|err| panic!("fixture {} must be readable: {}", path.display(), err));
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn real_world_game_with_crlf_glued_numbers_and_suffixes_loads_fully() {
        // The bundled PGN: CRLF endings, `1.d4` style numbers, `O-O`,
        // `Rxd1+` and a result token directly after the last move.
        let import = parse_pgn_with_report(&bundled_pgn("WorldChamp1972.pgn")).unwrap();
        assert_eq!(
            import.game.total_plies(),
            111,
            "55 full moves and White's 56th"
        );
        assert_eq!(
            import
                .game
                .active_history()
                .last()
                .map(|p| p.san.to_string())
                .as_deref(),
            Some("Kd6")
        );
        assert_eq!(
            import.ignored.variations, 0,
            "no variations in the first game"
        );
        assert_eq!(import.ignored.comments, 0, "no comments in the first game");
        assert_eq!(
            import.ignored.games, 20,
            "the other games of the collection are skipped"
        );
    }

    #[test]
    fn only_the_first_game_of_a_file_is_read() {
        let text = "[Event \"One\"]\n\n1. e4 *\n\n[Event \"Two\"]\n\n1. d4 *\n";
        let game = parse_pgn(text).unwrap();
        assert_eq!(sans(&game), vec!["e4"]);
    }

    #[test]
    fn unquoted_tag_values_are_accepted() {
        // Missing quotes around the FEN must not drop the starting position.
        let text = "[FEN rnbqkb1r/ppp1pppp/5n2/3p4/3P1B2/5N2/PPP1PPPP/RN1QKB1R b KQkq - 3 3]\n\n3... c5 *\n";
        let game = parse_pgn(text).unwrap();
        assert_eq!(sans(&game), vec!["c5"]);
    }

    #[test]
    fn quoted_tag_values_survive_escaping() {
        let (tags, _, _) = split_tags_and_movetext("[Event \"A \\\"quoted\\\" name\"]\n\n*\n");
        assert_eq!(
            tags,
            vec![("Event".to_string(), "A \"quoted\" name".to_string())]
        );
    }

    #[test]
    fn saving_keeps_every_move_even_when_browsing_back() {
        let mut game = GameState::new();
        for (from, to) in [("e2", "e4"), ("e7", "e5"), ("g1", "f3"), ("b8", "c6")] {
            game.play_squares(sq(from), sq(to), None).unwrap();
        }
        game.goto_start();
        let text = to_pgn(&game, None);
        assert!(
            text.contains("1. e4 e5 2. Nf3 Nc6 *"),
            "saved text: {}",
            text
        );
        assert_eq!(game.cur_ply(), 0, "saving must not move the cursor");
    }

    #[test]
    fn the_player_tags_are_read_and_may_be_missing() {
        let with = parse_pgn_with_report(
            "[White \"Karpov, Anatoly\"]\n[Black \"Kasparov\"]\n\n1. e4 e5 *\n",
        )
        .unwrap();
        assert_eq!(
            with.players,
            ("Karpov, Anatoly".to_string(), "Kasparov".to_string())
        );
        let without = parse_pgn_with_report("1. e4 e5 *\n").unwrap();
        assert_eq!(without.players, (String::new(), String::new()));
    }
}

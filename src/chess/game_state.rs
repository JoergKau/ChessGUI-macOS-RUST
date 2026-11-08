//! Chess position state, move history and SAN formatting.
//!
//! This is the **single source of truth** for the board on the UI side:
//! `gui::board_view` (Phase 4) and the PGN panel (Phase 5) both read from
//! here. The engine side (Phase 3) uses [`GameState::position`] to feed
//! `position fen … moves …` to UCI engines and to compute the PolyGlot
//! Zobrist hash for opening-book lookups.
//!
//! # Architecture
//!
//! ```text
//! assets::PieceTextures ──looks up──▶  PieceColor + PieceKind (this module)
//!                                          │
//!                                          ▼
//! GameState ──wraps──▶ shakmaty::Chess ──storing──▶ rich game history
//!                                          │
//!                                          ▼
//!                       SAN strings, FEN, Zobrist hash, legal-move filtering
//! ```
//!
//! # History model
//!
//! [`GameState`] keeps an immutable starting `Chess` and a `Vec<Ply>` of
//! recorded moves plus the position *after* each move, so:
//! * `position()` is **O(1)** (just an Rc clone of the right entry);
//! * navigating back/forward through the move list is just an integer
//!   decrement/increment on `cur_ply`, with stale tail-plies truncated
//!   when the user deviates from a previously-browsed line.
//!
//! This shape was chosen over a "single live `Chess` + replay" model so the
//! PGN panel can paint faster than `O(ply)` per click.

use std::cell::RefCell;

use shakmaty::{
    san::SanPlus, zobrist::Zobrist64, Board, CastlingMode, Chess, Color, EnPassantMode, File,
    FromSetup, Move, Position, Role, Square,
};

use crate::chess::opening_book::PolyglotBook;

// ─────────────────────────────────────────────────────────────────────────────
// Local colour / kind enums
// ─────────────────────────────────────────────────────────────────────────────
//
// We use our own enums (canonical for the asset pipeline and the rest of
// the crate) and provide `From`/`Into` shims for shakmaty's [`Color`]/[`Role`]
// wherever we need to cross the API boundary.

/// Side a piece (and the position's side-to-move) belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PieceColor {
    White,
    Black,
}

impl PieceColor {
    /// Letter used to prefix piece asset filenames (`w`, `b`).
    pub const fn letter(self) -> char {
        match self {
            PieceColor::White => 'w',
            PieceColor::Black => 'b',
        }
    }

    /// The other side.
    pub const fn opposite(self) -> Self {
        match self {
            PieceColor::White => PieceColor::Black,
            PieceColor::Black => PieceColor::White,
        }
    }
}

impl From<Color> for PieceColor {
    fn from(c: Color) -> Self {
        match c {
            Color::White => PieceColor::White,
            Color::Black => PieceColor::Black,
        }
    }
}

impl From<PieceColor> for Color {
    fn from(c: PieceColor) -> Self {
        match c {
            PieceColor::White => Color::White,
            PieceColor::Black => Color::Black,
        }
    }
}

/// Mapping between SiliconBoard's [`PieceKind`] (King/Queen/Rook/Bishop/
/// Knight/Pawn) and shakmaty's [`Role`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PieceKind {
    King,
    Queen,
    Rook,
    Bishop,
    Knight,
    Pawn,
}

impl PieceKind {
    /// shakmaty [`Role`] for this kind.
    pub const fn role(self) -> Role {
        match self {
            PieceKind::Pawn => Role::Pawn,
            PieceKind::Knight => Role::Knight,
            PieceKind::Bishop => Role::Bishop,
            PieceKind::Rook => Role::Rook,
            PieceKind::Queen => Role::Queen,
            PieceKind::King => Role::King,
        }
    }

    /// Inverse of [`Self::role`].
    pub const fn from_role(role: Role) -> Self {
        match role {
            Role::Pawn => PieceKind::Pawn,
            Role::Knight => PieceKind::Knight,
            Role::Bishop => PieceKind::Bishop,
            Role::Rook => PieceKind::Rook,
            Role::Queen => PieceKind::Queen,
            Role::King => PieceKind::King,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Ply = a single entry in the move history
// ─────────────────────────────────────────────────────────────────────────────

/// One recorded half-move (also called a *ply*) in the game's history.
///
/// `san` and `after` are both materialised at apply time so the UI can pull
/// them in O(1) without re-scanning earlier plies.
#[derive(Clone, Debug)]
pub struct Ply {
    /// The actual shakmaty move that was played. Pawn-promotion roles,
    /// castling info etc. live here.
    pub move_: Move,
    /// SAN string for this move (e.g. `"Nf3"` or `"O-O"` or `"exd6"`).
    pub san: SanPlus,
    /// The position *after* the move has been played.
    pub after: Chess,
}

// ─────────────────────────────────────────────────────────────────────────────
// GameState
// ─────────────────────────────────────────────────────────────────────────────

/// Full game state: starting position + move history + navigation cursor.
///
/// Cloning is cheap (`Chess` is internally Rc-shared) — we keep the struct
/// public so the eval panel can take a snapshot for analysis.
#[derive(Clone, Debug)]
pub struct GameState {
    /// Starting position (immutable, retained so we can navigate back to
    /// exactly the same position even after multiple games).
    start: Chess,
    /// Move history. Only the prefix `[..=cur_ply]` is currently active;
    /// the tail is reserved for any "redo" we have room to perform.
    history: Vec<Ply>,
    /// Cursor into `history`. 0 = no moves played yet; equal to
    /// `history.len()` = at the latest position.
    cur_ply: usize,
    /// Bumped whenever the recorded history changes, so derived views (the PGN
    /// move list) can cache their result.
    revision: u64,
    /// The FEN of the displayed position, with the revision and cursor it was
    /// made for. Formatting it clones the position, and the UI asks several
    /// times per frame.
    fen_memo: RefCell<Option<(u64, usize, String)>>,
    /// Whether the displayed position is a threefold repetition, with the
    /// revision and cursor it was counted for.
    repetition_memo: RefCell<Option<(u64, usize, bool)>>,
}

impl Default for GameState {
    fn default() -> Self {
        Self::new()
    }
}

impl GameState {
    /// Standard chess starting position. Equivalent to
    /// `GameState::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")`.
    pub fn new() -> Self {
        Self::from_position(Chess::default())
    }

    /// Wrap an existing shakmaty [`Chess`] as a fresh `GameState` (no
    /// history). The position may already be finished (checkmate, stalemate)
    /// so that finished games can be inspected.
    pub fn from_position(start: Chess) -> Self {
        Self {
            start,
            history: Vec::new(),
            cur_ply: 0,
            revision: 0,
            fen_memo: RefCell::new(None),
            repetition_memo: RefCell::new(None),
        }
    }

    /// Parse a FEN string and use it as the starting position. Accepts
    /// both X-FEN and Shredder/Fairy-Stockfish-style FEN — the underlying
    /// shakmaty parser is forgiving but returns an error on outright
    /// garbage.
    pub fn from_fen(fen: &str) -> Result<Self, FenError> {
        let parsed_fen = shakmaty::fen::Fen::from_ascii(fen.as_bytes())
            .map_err(|source| FenError::Parse { source })?;
        let setup = parsed_fen.into_setup();
        let chess =
            Chess::from_setup(setup, CastlingMode::Standard).map_err(|source| FenError::Setup {
                source: Box::new(source),
            })?;
        Ok(Self::from_position(chess))
    }

    // ── Read-only accessors ─────────────────────────────────────────────

    /// The position currently visible to the user. O(1) Rc clone.
    pub fn position(&self) -> Chess {
        self.position_at(self.cur_ply)
    }

    /// The position after `ply` half-moves (`0` = the starting position).
    fn position_at(&self, ply: usize) -> Chess {
        if ply == 0 {
            self.start.clone()
        } else {
            self.history[ply - 1].after.clone()
        }
    }

    /// The board of the displayed position, borrowed rather than cloned. The
    /// board widget reads the board every frame, so this avoids copying the
    /// whole position just to paint.
    pub fn board(&self) -> &Board {
        if self.cur_ply == 0 {
            self.start.board()
        } else {
            self.history[self.cur_ply - 1].after.board()
        }
    }

    /// Index of the current half-move (0-based). 0 = at the start.
    pub fn cur_ply(&self) -> usize {
        self.cur_ply
    }

    /// The starting position of the game (the position before the first ply).
    pub fn start_position(&self) -> Chess {
        self.start.clone()
    }

    /// Total number of plies currently cached in history (those ≤ `cur_ply`
    /// plus any "redo tail").
    pub fn total_plies(&self) -> usize {
        self.history.len()
    }

    /// A counter that changes whenever the recorded history changes. Derived
    /// views (the PGN move list) use it to cache their result.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Slice of recorded plies that are currently considered played,
    /// i.e. `[0..cur_ply]`. Used for the position shown on the board.
    pub fn active_history(&self) -> &[Ply] {
        &self.history[..self.cur_ply]
    }

    /// The whole recorded game, including plies after the cursor. Used for the
    /// move list and for saving, so browsing never shortens the game.
    pub fn history(&self) -> &[Ply] {
        &self.history
    }

    /// The position at the end of the recorded game (not the cursor).
    pub fn final_position(&self) -> Chess {
        self.history
            .last()
            .map_or_else(|| self.start.clone(), |ply| ply.after.clone())
    }

    /// Whose turn it is.
    pub fn turn(&self) -> PieceColor {
        self.position().turn().into()
    }

    /// The full-move number of the displayed position (from the FEN, starting at 1).
    pub fn fullmove_number(&self) -> u32 {
        self.position().fullmoves().get()
    }

    /// The squares of the move that produced the displayed position, as
    /// `(from, to)`. For castling the `to` square is the king's destination.
    /// `None` at the start of the game.
    pub fn last_move(&self) -> Option<(Square, Square)> {
        let ply = self.cur_ply.checked_sub(1).map(|i| &self.history[i])?;
        ply.move_
            .from()
            .map(|from| (from, target_square(ply.move_)))
    }

    /// Current FEN. Kept until the history or the cursor changes.
    pub fn fen(&self) -> String {
        let key = (self.revision, self.cur_ply);
        if let Some((revision, ply, fen)) = self.fen_memo.borrow().as_ref() {
            if (*revision, *ply) == key {
                return fen.clone();
            }
        }
        let fen =
            shakmaty::fen::Fen::from_position(&self.position(), EnPassantMode::Legal).to_string();
        *self.fen_memo.borrow_mut() = Some((key.0, key.1, fen.clone()));
        fen
    }

    // ── Game-state queries ──────────────────────────────────────────────

    /// `true` iff the side to move is in check.
    pub fn is_check(&self) -> bool {
        self.position().is_check()
    }

    /// `true` iff the side to move has no legal reply and is in check.
    pub fn is_checkmate(&self) -> bool {
        self.position().is_checkmate()
    }

    /// `true` iff the side to move has no legal reply and is *not* in check.
    pub fn is_stalemate(&self) -> bool {
        self.position().is_stalemate()
    }

    /// How many times the position at `ply` has occurred up to that ply,
    /// including the occurrence itself.
    ///
    /// Two positions are the same for repetition purposes when the pieces,
    /// side to move, castling rights and legal en-passant square match.
    /// shakmaty's `Chess` equality implements exactly that rule.
    fn repetition_count_at(&self, ply: usize) -> usize {
        let current = self.position_at(ply);
        let mut count = usize::from(self.start == current);
        for record in &self.history[..ply] {
            if record.after == current {
                count += 1;
            }
        }
        count
    }

    /// `true` when the displayed position has occurred three times. The
    /// position counts once at the start of a game (or at the FEN it began
    /// from), so two returns are enough to make the third occurrence.
    pub fn is_threefold_repetition(&self) -> bool {
        let key = (self.revision, self.cur_ply);
        if let Some((revision, ply, repeated)) = *self.repetition_memo.borrow() {
            if (revision, ply) == key {
                return repeated;
            }
        }
        let repeated = self.repetition_count_at(self.cur_ply) >= 3;
        *self.repetition_memo.borrow_mut() = Some((key.0, key.1, repeated));
        repeated
    }

    /// `true` when the displayed position has reached the fifty-move rule:
    /// 100 half-moves without a pawn move or a capture (FIDE Article 9.3).
    /// Like threefold repetition it is a claim, so the game loop adjudicates
    /// it for an engine-vs-engine game, where no player can claim.
    pub fn is_fifty_move_rule(&self) -> bool {
        self.position().halfmoves() >= 100
    }

    /// `true` iff checkmate, stalemate, or any other terminal condition.
    ///
    /// Threefold repetition is deliberately **not** included: under FIDE
    /// Article 9.2 it must be claimed by the player to move. Fivefold
    /// repetition and the 75-move rule are automatic and come from shakmaty.
    pub fn is_game_over(&self) -> bool {
        self.position().is_game_over()
    }

    /// Outcome of the game, in human-readable form. `"Unfinished"` if the
    /// game is still in progress.
    pub fn outcome(&self) -> String {
        let pos = self.position();
        if self.is_checkmate() {
            // The *side to move* lost; the other side won.
            let winner: PieceColor = pos.turn().other().into();
            format!("{} wins by checkmate", piece_color_label_local(winner))
        } else if self.is_stalemate() {
            "Draw by stalemate".to_string()
        } else if pos.is_insufficient_material() {
            "Draw by insufficient material".to_string()
        } else if pos.is_game_over() {
            // Another terminal position condition shakmaty models.
            "Draw".to_string()
        } else {
            "Unfinished".to_string()
        }
    }

    // ── Move queries ─────────────────────────────────────────────────────

    /// Legal moves from `from` that the user can play by clicking or
    /// dragging. Used by the board widget to highlight candidate
    /// destinations after a user click on a piece.
    pub fn legal_moves_from(&self, from: Square) -> Vec<Move> {
        // Cheap O(legal) filter — shakmaty's filter-by-piece is the fastest
        // way to do this without touching `pos.collect()`.
        self.position()
            .legal_moves()
            .into_iter()
            .filter(|m| m.from() == Some(from))
            .collect()
    }

    /// Apply a fully-formed [`Move`].
    ///
    /// Returns `Ok(())` and advances `cur_ply` on success, `Err(MoveError)`
    /// if the move is not legal in the displayed position.
    pub fn apply_move(&mut self, m: Move) -> Result<(), MoveError> {
        let pos = self.position();
        if !self.is_legal(m) {
            return Err(MoveError::IllegalMove);
        }
        // `from_move_and_play_unchecked` would mutate `pos`; keep the
        // computed SAN around even though we have to compute it via clones.
        let san = SanPlus::from_move(pos.clone(), m);
        let mut next = pos;
        next.play_unchecked(m);
        self.append_ply(Ply {
            move_: m,
            san,
            after: next,
        });
        Ok(())
    }

    /// Play a move given in UCI notation (`e2e4`, `e7e8q`, or castling as the
    /// king's move, `e1g1`). Returns the SAN of the move played. The move must
    /// be legal in the displayed position; the state is unchanged on error.
    pub fn apply_uci(&mut self, uci: &str) -> Result<String, MoveError> {
        let parsed = uci
            .parse::<shakmaty::uci::UciMove>()
            .map_err(|_| MoveError::NotUci)?;
        let m = parsed
            .to_move(&self.position())
            .map_err(|_| MoveError::IllegalMove)?;
        self.apply_move(m)?;
        Ok(self
            .active_history()
            .last()
            .map(|ply| ply.san.to_string())
            .unwrap_or_default())
    }

    /// `true` iff `m` is legal in the displayed position.
    fn is_legal(&self, m: Move) -> bool {
        self.position().is_legal(m)
    }

    /// Test helper: play the legal move that goes from `from` to `to`
    /// (castling by the king's target square). For a promotion, `promotion`
    /// selects the piece; without it a promotion is illegal here.
    #[cfg(test)]
    pub fn play_squares(
        &mut self,
        from: Square,
        to: Square,
        promotion: Option<PieceKind>,
    ) -> Result<(), MoveError> {
        let chosen = self
            .legal_moves_from(from)
            .into_iter()
            .find(|m| target_square(*m) == to && m.promotion() == promotion.map(PieceKind::role))
            .ok_or(MoveError::IllegalMove)?;
        self.apply_move(chosen)
    }

    /// The promotion pieces that are legal for a pawn move `from` → `to`, in
    /// Q/R/B/N order. Empty if the move is not a promotion. The board's
    /// promotion modal renders exactly these choices.
    #[allow(
        dead_code,
        reason = "convenience wrapper for callers without a cached legal-move list"
    )]
    pub fn promotion_choices(&self, from: Square, to: Square) -> Vec<PieceKind> {
        Self::promotion_choices_from(&self.legal_moves_from(from), to)
    }

    /// [`Self::promotion_choices`] using an already-computed legal-move list.
    pub fn promotion_choices_from(legal: &[Move], to: Square) -> Vec<PieceKind> {
        [
            PieceKind::Queen,
            PieceKind::Rook,
            PieceKind::Bishop,
            PieceKind::Knight,
        ]
        .into_iter()
        .filter(|kind| {
            legal
                .iter()
                .any(|m| target_square(*m) == to && m.promotion() == Some(kind.role()))
        })
        .collect()
    }

    // ── Navigation ───────────────────────────────────────────────────────

    /// Undo the most recent played move. Returns `true` if a ply was
    /// removed, `false` if we were already at the start of the game.
    pub fn undo(&mut self) -> bool {
        if self.cur_ply == 0 {
            return false;
        }
        self.cur_ply -= 1;
        true
    }

    /// Redo the previously-undone move. Returns `true` if the cursor
    /// advanced, `false` if we were already at the latest ply.
    pub fn redo(&mut self) -> bool {
        if self.cur_ply == self.history.len() {
            return false;
        }
        self.cur_ply += 1;
        true
    }

    /// Jump to an absolute ply index. Out-of-range values are clamped.
    pub fn goto_ply(&mut self, ply: usize) {
        self.cur_ply = ply.min(self.history.len());
    }

    /// Jump to the start of the game (cur_ply = 0).
    pub fn goto_start(&mut self) {
        self.cur_ply = 0;
    }

    /// Jump to the latest known ply.
    pub fn goto_end(&mut self) {
        self.cur_ply = self.history.len();
    }

    // ── PGN serialisation ────────────────────────────────────────────────

    /// Render the full played portion of the game as an iterable stream of
    /// `(move_number, white_san, black_san)` triples. White moves are
    /// always `Some`; black moves are `Some` only when the game has
    /// progressed past that half-move.
    pub fn pgn_lines(&self) -> Vec<MoveLine> {
        let start = self.start_position();
        let first_is_black = start.turn() == Color::Black;
        let first_number = start.fullmoves().get();
        let mut lines: Vec<MoveLine> = Vec::new();
        for (i, ply) in self.history().iter().enumerate() {
            let entry = MoveEntry {
                ply: i + 1,
                san: ply.san.to_string(),
            };
            let number = first_number + ((i + usize::from(first_is_black)) / 2) as u32;
            let is_white = (i % 2 == 0) != first_is_black;
            if is_white {
                lines.push(MoveLine {
                    number,
                    white: Some(entry),
                    black: None,
                });
            } else {
                match lines.last_mut() {
                    Some(line) if line.number == number && line.black.is_none() => {
                        line.black = Some(entry);
                    }
                    _ => lines.push(MoveLine {
                        number,
                        white: None,
                        black: Some(entry),
                    }),
                }
            }
        }
        lines
    }

    /// Convert a line of UCI moves (e.g. an engine principal variation) into
    /// SAN, starting from the displayed position. Conversion stops at the
    /// first move that is not legal, so a malformed line yields a shorter
    /// result rather than an error.
    pub fn uci_moves_to_san(&self, uci_moves: &[String]) -> Vec<String> {
        let mut pos = self.position();
        let mut san_line = Vec::with_capacity(uci_moves.len());
        for text in uci_moves {
            let Ok(uci) = text.parse::<shakmaty::uci::UciMove>() else {
                break;
            };
            let Ok(m) = uci.to_move(&pos) else {
                break;
            };
            san_line.push(SanPlus::from_move_and_play_unchecked(&mut pos, m).to_string());
        }
        san_line
    }

    /// Opening-book moves for the displayed position as `(SAN, weight)`,
    /// heaviest first. Empty when the position is not in the book.
    pub fn book_suggestions(&self, book: &PolyglotBook) -> Vec<(String, u16)> {
        let pos = self.position();
        let mut suggestions: Vec<(String, u16)> = self
            .book_moves(book)
            .into_iter()
            .map(|(m, weight)| (SanPlus::from_move(pos.clone(), m).to_string(), weight))
            .collect();
        suggestions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        suggestions
    }

    /// Opening-book moves for the displayed position as legal moves with their
    /// weights, in book order. Empty when the position is not in the book.
    pub fn book_moves(&self, book: &PolyglotBook) -> Vec<(Move, u16)> {
        let pos = self.position();
        book.find(self.zobrist())
            .iter()
            .filter_map(|entry| entry.to_move(&pos).map(|m| (m, entry.weight)))
            .collect()
    }

    // ── Engine-binding helpers ───────────────────────────────────────────

    /// PolyGlot Zobrist hash of the current position. Engine subprocesses
    /// expect the FEN, while the Phase-2 PolyGlot reader expects this
    /// hash for O(log N) book lookup.
    pub fn zobrist(&self) -> u64 {
        self.position()
            .zobrist_hash::<Zobrist64>(EnPassantMode::Legal)
            .0
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /// Append a new ply at `cur_ply`, discarding any redo tail.
    fn append_ply(&mut self, ply: Ply) {
        self.history.truncate(self.cur_ply);
        self.history.push(ply);
        self.cur_ply = self.history.len();
        self.revision = self.revision.wrapping_add(1);
    }
}

/// One played move in the move list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveEntry {
    /// Cursor value that shows this move as played (1-based half-move count).
    pub ply: usize,
    /// The move in SAN, with its check or mate suffix.
    pub san: String,
}

/// One line of the move list: a move number with its White and Black moves.
/// A game that starts with Black to move begins with a line that has no White move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveLine {
    /// Full-move number as printed in the game (starts from the FEN's number).
    pub number: u32,
    /// White's move, if this line has one.
    pub white: Option<MoveEntry>,
    /// Black's move, if this line has one.
    pub black: Option<MoveEntry>,
}

/// The square a player clicks to play `m`. Equal to [`Move::to`] except for
/// castling, where shakmaty reports the rook square and the king's
/// destination (g1/c1 and so on) is returned instead.
pub fn target_square(m: Move) -> Square {
    if let Move::Castle { king, rook } = m {
        let file = if rook.file().to_u32() > king.file().to_u32() {
            File::G
        } else {
            File::C
        };
        Square::from_coords(file, king.rank())
    } else {
        m.to()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Human-readable color name (using the local PieceColor).
fn piece_color_label_local(c: PieceColor) -> &'static str {
    match c {
        PieceColor::White => "White",
        PieceColor::Black => "Black",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Endings
// ─────────────────────────────────────────────────────────────────────────────

/// How a game was decided when the position does not say so. Mate, stalemate
/// and the other drawn positions come from the position itself; these three
/// come from the game loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// `loser` resigned.
    Resigned {
        /// The side that resigned.
        loser: PieceColor,
    },
    /// Both sides agreed to a draw.
    DrawAgreed,
    /// The player to move claimed a draw because the position occurred three
    /// times (FIDE Article 9.2).
    ThreefoldRepetition,
    /// The game loop ended the game because 100 half-moves passed without a
    /// pawn move or a capture (FIDE Article 9.3).
    FiftyMoveRule,
    /// A tournament adjudicator awarded the game to `winner`.
    AdjudicatedWin {
        /// The side the adjudicator declared the winner.
        winner: PieceColor,
    },
    /// A tournament adjudicator declared the game drawn.
    AdjudicatedDraw,
    /// The result came from a PGN file and has no reason attached.
    FromPgn {
        /// The winning side, or `None` for a draw.
        winner: Option<PieceColor>,
    },
    /// `loser` ran out of time.
    OnTime {
        /// The side whose clock ran out.
        loser: PieceColor,
    },
}

impl Ending {
    /// PGN result token: `1-0`, `0-1` or `1/2-1/2`.
    pub fn result_token(self) -> &'static str {
        match self {
            Ending::Resigned { loser } | Ending::OnTime { loser } => match loser {
                PieceColor::White => "0-1",
                PieceColor::Black => "1-0",
            },
            Ending::AdjudicatedWin { winner } => match winner {
                PieceColor::White => "1-0",
                PieceColor::Black => "0-1",
            },
            Ending::FromPgn { winner } => match winner {
                Some(PieceColor::White) => "1-0",
                Some(PieceColor::Black) => "0-1",
                None => "1/2-1/2",
            },
            Ending::DrawAgreed
            | Ending::ThreefoldRepetition
            | Ending::FiftyMoveRule
            | Ending::AdjudicatedDraw => "1/2-1/2",
        }
    }

    /// Text for the status line and the eval panel, e.g. `Black wins by resignation`.
    pub fn description(self) -> String {
        match self {
            Ending::Resigned { loser } => {
                format!(
                    "{} wins by resignation",
                    piece_color_label_local(loser.opposite())
                )
            }
            Ending::OnTime { loser } => {
                format!("{} wins on time", piece_color_label_local(loser.opposite()))
            }
            Ending::AdjudicatedWin { winner } => {
                format!("{} wins by adjudication", piece_color_label_local(winner))
            }
            Ending::FromPgn { winner } => match winner {
                Some(winner) => format!("{} won (from PGN)", piece_color_label_local(winner)),
                None => "Draw (from PGN)".to_string(),
            },
            Ending::DrawAgreed => "Draw agreed".to_string(),
            Ending::AdjudicatedDraw => "Draw by adjudication".to_string(),
            Ending::ThreefoldRepetition => "Draw by repetition".to_string(),
            Ending::FiftyMoveRule => "Draw by the fifty-move rule".to_string(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error types
// ─────────────────────────────────────────────────────────────────────────────

/// Errors raised when parsing a FEN string into a starting position.
#[derive(Debug)]
pub enum FenError {
    /// Lexical FEN parse error.
    Parse {
        source: shakmaty::fen::ParseFenError,
    },
    /// `Chess::from_setup` rejected the FEN-derived setup (illegal position).
    Setup {
        source: Box<shakmaty::PositionError<Chess>>,
    },
}

impl std::fmt::Display for FenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FenError::Parse { source } => write!(f, "could not parse FEN: {}", source),
            FenError::Setup { source } => write!(f, "invalid FEN setup: {}", source),
        }
    }
}

impl std::error::Error for FenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FenError::Parse { source } => Some(source),
            FenError::Setup { source } => Some(source),
        }
    }
}

/// Errors raised when applying a move to the game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveError {
    /// The move is not legal in the displayed position.
    IllegalMove,
    /// The text is not a UCI move at all.
    NotUci,
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveError::IllegalMove => f.write_str("illegal move"),
            MoveError::NotUci => f.write_str("not a UCI move"),
        }
    }
}

impl std::error::Error for MoveError {}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(name: &str) -> Square {
        Square::from_ascii(name.as_bytes()).unwrap()
    }

    #[test]
    fn starting_position_has_20_legal_moves() {
        let g = GameState::new();
        assert_eq!(g.position().legal_moves().len(), 20);
        assert!(!g.is_check());
        assert!(!g.is_checkmate());
        assert!(!g.is_stalemate());
        assert_eq!(g.turn(), PieceColor::White);
        assert_eq!(g.cur_ply(), 0);
        assert_eq!(g.total_plies(), 0);
    }

    #[test]
    fn board_follows_the_history_cursor() {
        let mut g = GameState::new();
        assert!(g.board().piece_at(sq("e2")).is_some());
        g.apply_uci("e2e4").unwrap();
        assert!(g.board().piece_at(sq("e2")).is_none(), "the pawn left e2");
        assert!(g.board().piece_at(sq("e4")).is_some(), "and stands on e4");
        g.goto_ply(0);
        assert!(
            g.board().piece_at(sq("e2")).is_some(),
            "browsing shows the earlier board"
        );
    }

    #[test]
    fn playing_e4_then_e5_advances_history() {
        let mut g = GameState::new();

        // White plays e4 (a pawn move, UCI: e2e4).
        let from = Square::from_ascii(b"e2").unwrap();
        let to = Square::from_ascii(b"e4").unwrap();
        g.play_squares(from, to, None).unwrap();

        assert_eq!(g.cur_ply(), 1);
        assert_eq!(g.turn(), PieceColor::Black);
        // Squares e2 and e4 reflect the move.
        assert_eq!(g.position().board().piece_at(from), None);
        assert!(g.position().board().piece_at(to).is_some());

        // Black plays e5.
        let from = Square::from_ascii(b"e7").unwrap();
        let to = Square::from_ascii(b"e5").unwrap();
        g.play_squares(from, to, None).unwrap();
        assert_eq!(g.cur_ply(), 2);
        assert_eq!(g.turn(), PieceColor::White);
    }

    #[test]
    fn illegal_moves_are_rejected() {
        let mut g = GameState::new();
        // e2 → e5 is illegal for white on move 0.
        let r = g.play_squares(
            Square::from_ascii(b"e2").unwrap(),
            Square::from_ascii(b"e5").unwrap(),
            None,
        );
        assert!(matches!(r, Err(MoveError::IllegalMove)));
        // Even after an error the state is unchanged.
        assert_eq!(g.cur_ply(), 0);
    }

    #[test]
    fn undo_and_redo_round_trip() {
        let mut g = GameState::new();
        g.play_squares(
            Square::from_ascii(b"e2").unwrap(),
            Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();
        g.play_squares(
            Square::from_ascii(b"e7").unwrap(),
            Square::from_ascii(b"e5").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(g.cur_ply(), 2);

        assert!(g.undo());
        assert_eq!(g.cur_ply(), 1);
        assert_eq!(g.turn(), PieceColor::Black);

        assert!(g.undo());
        assert_eq!(g.cur_ply(), 0);
        assert_eq!(g.turn(), PieceColor::White);

        assert!(g.redo());
        assert!(g.redo());
        assert_eq!(g.cur_ply(), 2);
    }

    #[test]
    fn threefold_repetition_is_detected_but_not_automatic() {
        let mut g = GameState::new();
        let cycle = ["g1f3", "g8f6", "f3g1", "f6g8"];

        for uci in cycle {
            g.apply_uci(uci).unwrap();
        }
        assert!(
            !g.is_threefold_repetition(),
            "the starting position has occurred twice, not three times"
        );

        for uci in cycle {
            g.apply_uci(uci).unwrap();
        }
        assert!(
            g.is_threefold_repetition(),
            "the starting position has now occurred three times"
        );
        assert!(
            !g.is_game_over(),
            "threefold repetition must be claimed; it is not automatic"
        );
        assert_eq!(g.outcome(), "Unfinished");
    }

    #[test]
    fn fen_roundtrips_after_one_move() {
        let mut g = GameState::new();
        g.play_squares(
            Square::from_ascii(b"e2").unwrap(),
            Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();

        let fen = g.fen();
        let mut parsed = GameState::from_fen(&fen).unwrap();
        // After round-trip, the pawn is on e4 and e2 is empty — so e2->e4
        // is no longer legal, but the same *continuation* (a further legal
        // move) is still parsed correctly.
        assert!(!parsed.is_legal(Move::Normal {
            role: Role::Pawn,
            from: Square::from_ascii(b"e2").unwrap(),
            capture: None,
            to: Square::from_ascii(b"e4").unwrap(),
            promotion: None,
        }));
        // The mirrored black move should be legal after round trip.
        assert!(parsed.is_legal(Move::Normal {
            role: Role::Pawn,
            from: Square::from_ascii(b"e7").unwrap(),
            capture: None,
            to: Square::from_ascii(b"e5").unwrap(),
            promotion: None,
        }));
        // And after we apply e7→e5 on the parsed state, the FEN string should
        // match the one we get by applying both moves on g from the start
        // (the round-trip is loss-less).
        parsed
            .play_squares(
                Square::from_ascii(b"e7").unwrap(),
                Square::from_ascii(b"e5").unwrap(),
                None,
            )
            .unwrap();
        g.play_squares(
            Square::from_ascii(b"e7").unwrap(),
            Square::from_ascii(b"e5").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(parsed.fen(), g.fen());
    }

    #[test]
    fn fools_mate_terminates() {
        // Fool's mate: 1.f3 e5 2.g4 Qh4# — four plies, then checkmate.
        let mut g = GameState::new();
        g.play_squares(
            Square::from_ascii(b"f2").unwrap(),
            Square::from_ascii(b"f3").unwrap(),
            None,
        )
        .unwrap();
        g.play_squares(
            Square::from_ascii(b"e7").unwrap(),
            Square::from_ascii(b"e5").unwrap(),
            None,
        )
        .unwrap();
        g.play_squares(
            Square::from_ascii(b"g2").unwrap(),
            Square::from_ascii(b"g4").unwrap(),
            None,
        )
        .unwrap();
        g.play_squares(
            Square::from_ascii(b"d8").unwrap(),
            Square::from_ascii(b"h4").unwrap(),
            None,
        )
        .unwrap();
        assert!(g.is_checkmate(), "expected checkmate, got FEN {}", g.fen());
        assert_eq!(g.outcome(), "Black wins by checkmate");
    }

    #[test]
    fn promotion_choices_listed() {
        // White pawn on a7, black king on h8, white king on h1 — a7→a8 must
        // promote and all four choices must be legal.
        // FEN rank 7 must total 8 squares, so we use `P7` (P + 7 empty).
        let fen = "7k/P7/8/8/8/8/8/7K w - - 0 1";
        let mut g = GameState::from_fen(fen).unwrap();
        let choices = g.promotion_choices(
            Square::from_ascii(b"a7").unwrap(),
            Square::from_ascii(b"a8").unwrap(),
        );
        assert_eq!(choices.len(), 4);
        // Without a promotion piece the promotion move is not playable.
        let r = g.play_squares(
            Square::from_ascii(b"a7").unwrap(),
            Square::from_ascii(b"a8").unwrap(),
            None,
        );
        assert!(matches!(r, Err(MoveError::IllegalMove)));
        // With Queen explicit, it succeeds.
        g.play_squares(
            Square::from_ascii(b"a7").unwrap(),
            Square::from_ascii(b"a8").unwrap(),
            Some(PieceKind::Queen),
        )
        .unwrap();
    }

    #[test]
    fn fen_parse_rejects_garbage() {
        // Standard starting position — legal.
        assert!(
            GameState::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1").is_ok()
        );
        // Empty board is *illegal* (no kings).
        assert!(GameState::from_fen("8/8/8/8/8/8/8/8 w - - 0 1").is_err());
        // Bogus inputs:
        assert!(GameState::from_fen("xxxxxxx").is_err());
        assert!(GameState::from_fen("").is_err());
        assert!(GameState::from_fen("not a fen").is_err());
    }

    #[test]
    fn zobrist_hash_matches_canonical_polyglot_value() {
        // The PolyGlot "standard" chess starting position has
        // Zobrist key 0x463b96181691fc9c. shakmaty's `zobrist_hash`
        // uses the same compatible random table, so this is a strong
        // proof that our `GameState::zobrist()` agrees with every
        // existing polyglot opening book on disk.
        let g = GameState::new();
        let key = g.zobrist();
        assert_eq!(
            format!("{:016x}", key),
            "463b96181691fc9c",
            "GameState::zobrist() should match PolyGlot canonical value"
        );
    }

    #[test]
    fn last_move_follows_the_history_cursor() {
        let mut g = GameState::new();
        assert_eq!(g.last_move(), None);
        g.play_squares(
            Square::from_ascii(b"e2").unwrap(),
            Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();
        g.play_squares(
            Square::from_ascii(b"e7").unwrap(),
            Square::from_ascii(b"e5").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(g.last_move(), Some((sq("e7"), sq("e5"))));
        g.undo();
        assert_eq!(g.last_move(), Some((sq("e2"), sq("e4"))));
        g.goto_start();
        assert_eq!(g.last_move(), None);
    }

    #[test]
    fn castling_last_move_reports_king_destination() {
        let mut g =
            GameState::from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1").unwrap();
        g.play_squares(sq("e1"), sq("g1"), None).unwrap();
        assert_eq!(g.last_move(), Some((sq("e1"), sq("g1"))));
    }

    #[test]
    fn uci_line_converts_to_san_from_the_current_position() {
        let g = GameState::new();
        let pv: Vec<String> = ["e2e4", "e7e5", "g1f3", "b8c6"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(g.uci_moves_to_san(&pv), vec!["e4", "e5", "Nf3", "Nc6"]);
    }

    #[test]
    fn uci_line_stops_at_the_first_illegal_move() {
        let g = GameState::new();
        let pv: Vec<String> = ["e2e4", "e2e5", "g1f3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(g.uci_moves_to_san(&pv), vec!["e4"]);
    }

    #[test]
    fn apply_uci_plays_the_move_and_reports_its_san() {
        let mut g = GameState::new();
        assert_eq!(g.apply_uci("e2e4").as_deref(), Ok("e4"));
        assert_eq!(g.cur_ply(), 1);
        assert_eq!(g.apply_uci("g8f6").as_deref(), Ok("Nf6"));
        assert_eq!(g.cur_ply(), 2);
    }

    #[test]
    fn apply_uci_rejects_illegal_and_malformed_moves_without_changing_the_game() {
        let mut g = GameState::new();
        assert_eq!(g.apply_uci("e2e5"), Err(MoveError::IllegalMove));
        assert_eq!(g.apply_uci("hello"), Err(MoveError::NotUci));
        assert_eq!(g.apply_uci(""), Err(MoveError::NotUci));
        assert_eq!(g.cur_ply(), 0);
    }

    #[test]
    fn apply_uci_accepts_castling_and_promotion() {
        let mut g =
            GameState::from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1").unwrap();
        assert_eq!(g.apply_uci("e1g1").as_deref(), Ok("O-O"));

        let mut g = GameState::from_fen("7k/P7/8/8/8/8/8/7K w - - 0 1").unwrap();
        assert_eq!(
            g.apply_uci("a7a8"),
            Err(MoveError::IllegalMove),
            "a promotion needs a piece"
        );
        assert_eq!(g.apply_uci("a7a8q").as_deref(), Ok("a8=Q+"));
    }

    #[test]
    fn target_square_maps_castling_onto_the_king_destination() {
        let g = GameState::from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1").unwrap();
        let castles: Vec<Move> = g
            .legal_moves_from(sq("e1"))
            .into_iter()
            .filter(|m| m.is_castle())
            .collect();
        let mut targets: Vec<Square> = castles.iter().map(|m| target_square(*m)).collect();
        targets.sort();
        assert_eq!(targets, vec![sq("c1"), sq("g1")]);
    }

    #[test]
    fn finished_positions_can_be_loaded_for_inspection() {
        // Fool's mate final position: loadable, and reported as checkmate.
        let g =
            GameState::from_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3")
                .unwrap();
        assert!(g.is_checkmate());
        assert_eq!(g.outcome(), "Black wins by checkmate");
    }

    #[test]
    fn book_suggestions_list_the_books_start_moves() {
        let book = crate::chess::opening_book::sample_start_book();
        let suggestions = GameState::new().book_suggestions(&book);
        assert!(
            !suggestions.is_empty(),
            "start position must have book moves"
        );
        // Every suggestion must be a legal SAN move of the start position.
        for (san, _) in &suggestions {
            assert!(
                shakmaty::san::San::from_ascii(san.as_bytes()).is_ok(),
                "book SAN `{}` must parse",
                san
            );
        }
        let weights: Vec<u16> = suggestions.iter().map(|(_, w)| *w).collect();
        assert!(
            weights.windows(2).all(|pair| pair[0] >= pair[1]),
            "heaviest first"
        );
    }

    #[test]
    fn book_moves_are_legal_and_match_the_suggestions() {
        let book = crate::chess::opening_book::sample_start_book();
        let game = GameState::new();
        let moves = game.book_moves(&book);
        assert!(!moves.is_empty(), "start position must have book moves");
        assert_eq!(moves.len(), game.book_suggestions(&book).len());
        for (m, _) in &moves {
            assert!(game.position().is_legal(*m), "book move must be legal");
        }
    }

    #[test]
    fn endings_give_their_pgn_result_and_text() {
        assert_eq!(
            Ending::Resigned {
                loser: PieceColor::Black
            }
            .result_token(),
            "1-0"
        );
        assert_eq!(
            Ending::OnTime {
                loser: PieceColor::White
            }
            .result_token(),
            "0-1"
        );
        assert_eq!(Ending::DrawAgreed.result_token(), "1/2-1/2");
        assert_eq!(Ending::ThreefoldRepetition.result_token(), "1/2-1/2");
        assert_eq!(Ending::FiftyMoveRule.result_token(), "1/2-1/2");
        assert_eq!(
            Ending::AdjudicatedWin {
                winner: PieceColor::White
            }
            .result_token(),
            "1-0"
        );
        assert_eq!(Ending::AdjudicatedDraw.result_token(), "1/2-1/2");
        assert_eq!(
            Ending::FromPgn {
                winner: Some(PieceColor::Black)
            }
            .result_token(),
            "0-1"
        );
        assert_eq!(Ending::FromPgn { winner: None }.result_token(), "1/2-1/2");
        assert_eq!(
            Ending::Resigned {
                loser: PieceColor::White
            }
            .description(),
            "Black wins by resignation"
        );
        assert_eq!(
            Ending::OnTime {
                loser: PieceColor::Black
            }
            .description(),
            "White wins on time"
        );
        assert_eq!(
            Ending::AdjudicatedWin {
                winner: PieceColor::Black
            }
            .description(),
            "Black wins by adjudication"
        );
        assert_eq!(
            Ending::AdjudicatedDraw.description(),
            "Draw by adjudication"
        );
        assert_eq!(
            Ending::FromPgn {
                winner: Some(PieceColor::White)
            }
            .description(),
            "White won (from PGN)"
        );
        assert_eq!(
            Ending::FromPgn { winner: None }.description(),
            "Draw (from PGN)"
        );
        assert_eq!(Ending::DrawAgreed.description(), "Draw agreed");
        assert_eq!(
            Ending::ThreefoldRepetition.description(),
            "Draw by repetition"
        );
        assert_eq!(
            Ending::FiftyMoveRule.description(),
            "Draw by the fifty-move rule"
        );
    }

    #[test]
    fn move_list_lines_follow_the_start_position() {
        let mut g = GameState::new();
        g.play_squares(sq("e2"), sq("e4"), None).unwrap();
        g.play_squares(sq("e7"), sq("e5"), None).unwrap();
        let lines = g.pgn_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].number, 1);
        assert_eq!(
            lines[0].white.as_ref().map(|m| (m.ply, m.san.as_str())),
            Some((1, "e4"))
        );
        assert_eq!(
            lines[0].black.as_ref().map(|m| (m.ply, m.san.as_str())),
            Some((2, "e5"))
        );
    }

    #[test]
    fn move_list_starting_with_black_to_move_numbers_from_the_fen() {
        // Black to move at move 12: the first line has no White move, and
        // the numbering continues from the FEN.
        let mut g =
            GameState::from_fen("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 12")
                .unwrap();
        g.play_squares(sq("c7"), sq("c5"), None).unwrap();
        g.play_squares(sq("g1"), sq("f3"), None).unwrap();
        let lines = g.pgn_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].number, 12);
        assert!(lines[0].white.is_none());
        assert_eq!(
            lines[0].black.as_ref().map(|m| (m.ply, m.san.as_str())),
            Some((1, "c5"))
        );
        assert_eq!(lines[1].number, 13);
        assert_eq!(
            lines[1].white.as_ref().map(|m| (m.ply, m.san.as_str())),
            Some((2, "Nf3"))
        );
        assert!(lines[1].black.is_none());
    }

    #[test]
    fn move_list_keeps_moves_after_the_cursor() {
        let mut g = GameState::new();
        for (from, to) in [
            ("e2", "e4"),
            ("e7", "e5"),
            ("g1", "f3"),
            ("b8", "c6"),
            ("f1", "b5"),
        ] {
            g.play_squares(sq(from), sq(to), None).unwrap();
        }
        g.goto_ply(2);
        let lines = g.pgn_lines();
        assert_eq!(lines.len(), 3, "every move stays listed while browsing");
        assert_eq!(lines[2].white.as_ref().map(|m| m.ply), Some(5));
        assert_eq!(g.cur_ply(), 2, "listing must not move the cursor");
    }
}

#[cfg(test)]
mod memo_tests {
    use super::*;

    #[test]
    fn the_remembered_fen_follows_the_cursor_and_new_moves() {
        let mut game = GameState::new();
        let start = game.fen();
        game.play_squares(
            shakmaty::Square::from_ascii(b"e2").unwrap(),
            shakmaty::Square::from_ascii(b"e4").unwrap(),
            None,
        )
        .unwrap();
        let after = game.fen();
        assert_ne!(start, after);
        assert_eq!(game.fen(), after, "a repeated call gives the same text");
        game.goto_start();
        assert_eq!(game.fen(), start, "moving the cursor changes the FEN");
        game.goto_end();
        assert_eq!(game.fen(), after);
        game.undo();
        game.play_squares(
            shakmaty::Square::from_ascii(b"d2").unwrap(),
            shakmaty::Square::from_ascii(b"d4").unwrap(),
            None,
        )
        .unwrap();
        assert_ne!(game.fen(), after, "a new move replaces the remembered text");
        assert!(!game.is_threefold_repetition());
    }
}

//! Setup Board: edit the position on the board, then continue from it.
//!
//! The editor works on a [`Setup`], which may be an illegal position while
//! the user is arranging the pieces. [`SetupBoard::validate`] returns every
//! reason the position cannot be played, so the **Close** button can stay
//! disabled until the position is sound. The board widget itself only reports
//! square clicks ([`crate::gui::board_view::SetupBoardAction`]); this module
//! owns the palette, the side to move and the castling rights.
//!
//! The checks cover the rules a legality checker has to enforce for a position
//! that did not arise from a game: one king per side, no pawns on the
//! backranks, no side not to move in check, no checkmate or stalemate, valid
//! castling rights and no more material than can be reached in a normal game.

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use shakmaty::{
    Bitboard, Board, CastlingMode, CastlingSide, Chess, Color, EnPassantMode, File, FromSetup,
    Piece, Position, PositionErrorKinds, Rank, Role, Setup, Square,
};

use crate::assets::{Assets, PieceId};
use crate::chess::PieceKind;
use crate::gui::board_view::draw_piece;
use crate::gui::theme;

/// The order the palette shows the pieces in, as in a chess diagram.
const PALETTE: [Role; 6] = [
    Role::King,
    Role::Queen,
    Role::Rook,
    Role::Bishop,
    Role::Knight,
    Role::Pawn,
];

/// Backing colour of a palette button. A light grey keeps the black pieces
/// visible against the dark panel, just as the board's light squares do.
const PALETTE_BG: Color32 = Color32::from_rgb(0xc3, 0xc8, 0xd0);

/// Backing colour under the pointer.
const PALETTE_HOVER_BG: Color32 = Color32::from_rgb(0xd9, 0xdd, 0xe3);

/// Backing colour of the selected piece.
const PALETTE_SELECTED_BG: Color32 = Color32::from_rgb(0x9f, 0xcd, 0xd8);

/// What the setup panel wants the application to do next.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SetupAction {
    /// Nothing happened this frame.
    None,
    /// The position is valid: apply it and leave the setup view.
    Close,
    /// Leave the setup view without changing the game.
    Cancel,
}

/// The position being edited, plus the piece selected in the palette.
#[derive(Clone, Debug)]
pub struct SetupBoard {
    /// Pieces, side to move, castling rights and move counters.
    setup: Setup,
    /// Piece that the next left click on the board places.
    selected: Option<Piece>,
    /// Why the current position cannot be played. Empty means valid.
    errors: Vec<String>,
    /// The FEN field's text. It follows the board until the user types in it,
    /// and is parsed by [`Self::apply_fen`] or Enter.
    fen_draft: String,
    /// Why the last FEN typed into the field could not be parsed.
    fen_error: Option<String>,
    /// Bumped whenever the position changes, so the app can refresh the top
    /// FEN field without overwriting what the user is typing there.
    revision: u64,
}

impl SetupBoard {
    /// Start editing `position`, keeping its pieces, side to move, castling
    /// rights and en-passant square. The en-passant square is dropped by the
    /// first edit that changes the position, because the editor has no control
    /// for it and it cannot be kept meaningful while pieces move.
    pub fn from_position(position: &Chess) -> Self {
        let mut setup = position.to_setup(EnPassantMode::Legal);
        keep_standard_castling_rights(&mut setup);
        let mut board = Self {
            setup,
            selected: None,
            errors: Vec::new(),
            fen_draft: String::new(),
            fen_error: None,
            revision: 0,
        };
        board.changed();
        board
    }

    /// The pieces as the board widget paints them.
    pub fn board(&self) -> &Board {
        &self.setup.board
    }

    /// The side to move, which is also the side the user may give check to.
    pub fn turn(&self) -> Color {
        self.setup.turn
    }

    /// Change the side to move. White is the default for a cleared board.
    pub fn set_turn(&mut self, color: Color) {
        self.setup.turn = color;
        // An en-passant target belongs to the side that just moved; it cannot
        // survive a change of the side to move.
        self.setup.ep_square = None;
        self.changed();
    }

    /// The piece selected in the palette, if any.
    pub fn selected(&self) -> Option<Piece> {
        self.selected
    }

    /// Select a piece for the next placement. Selecting the selected piece
    /// again clears the selection.
    pub fn set_selected(&mut self, piece: Option<Piece>) {
        self.selected = piece;
    }

    /// Whether `color` may castle on `side`.
    pub fn castling_right(&self, color: Color, side: CastlingSide) -> bool {
        self.setup
            .castling_rights
            .contains(corner_square(color, side))
    }

    /// Switch one castling right on or off.
    pub fn set_castling_right(&mut self, color: Color, side: CastlingSide, value: bool) {
        self.setup
            .castling_rights
            .set(corner_square(color, side), value);
        self.changed();
    }

    /// Remove every piece. The palette selection stays, so the user can start
    /// placing pieces at once.
    pub fn clear(&mut self) {
        self.setup = Setup::empty();
        self.changed();
    }

    /// Place the selected palette piece on `square`, replacing what was there.
    /// Does nothing while no piece is selected.
    pub fn place(&mut self, square: Square) {
        let Some(piece) = self.selected else {
            return;
        };
        self.setup.ep_square = None;
        self.setup.board.set_piece_at(square, piece);
        self.changed();
    }

    /// Remove the piece on `square`, if any.
    pub fn remove(&mut self, square: Square) {
        self.setup.ep_square = None;
        self.setup.board.discard_piece_at(square);
        self.changed();
    }

    /// Why the position cannot be played. Empty means it can.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Why the text in the FEN field could not be parsed.
    pub fn fen_error(&self) -> Option<&str> {
        self.fen_error.as_deref()
    }

    /// Counts position changes. The app uses it to refresh the top FEN field
    /// only when the editor changed the position, not while the user types.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Recompute [`Self::errors`].
    pub fn refresh_errors(&mut self) {
        self.errors = self.validate();
    }

    /// Replace the position with a FEN typed into either FEN field. The FEN
    /// may describe an illegal position: it is accepted when it parses, so the
    /// user can fix it piece by piece, and [`Self::errors`] then explains what
    /// is wrong.
    pub fn set_fen(&mut self, fen: &str) -> Result<(), String> {
        let parsed = match shakmaty::fen::Fen::from_ascii(fen.trim().as_bytes()) {
            Ok(parsed) => parsed,
            Err(err) => {
                let message = format!("Not a valid FEN: {}.", err);
                self.fen_error = Some(message.clone());
                return Err(message);
            }
        };
        let mut setup = parsed.into_setup();
        keep_standard_castling_rights(&mut setup);
        self.setup = setup;
        self.changed();
        Ok(())
    }

    /// Parse the text currently in the panel's FEN field.
    pub fn apply_fen(&mut self) -> Result<(), String> {
        let draft = self.fen_draft.clone();
        self.set_fen(&draft)
    }

    /// Apply a position change and keep the FEN field in step.
    fn changed(&mut self) {
        self.errors = self.validate();
        self.fen_draft = self.fen();
        self.fen_error = None;
        self.revision = self.revision.wrapping_add(1);
    }

    /// The FEN of the edited position, including an invalid board. The
    /// en-passant square is shown until the first edit drops it.
    pub fn fen(&self) -> String {
        let mut fen = String::new();
        for rank in (0..8u32).rev() {
            let mut empty = 0u8;
            for file in 0..8u32 {
                let square = Square::from_coords(File::new(file), Rank::new(rank));
                match self.setup.board.piece_at(square) {
                    Some(piece) => {
                        if empty > 0 {
                            fen.push_str(&empty.to_string());
                            empty = 0;
                        }
                        fen.push(piece.char());
                    }
                    None => empty += 1,
                }
            }
            if empty > 0 {
                fen.push_str(&empty.to_string());
            }
            if rank > 0 {
                fen.push('/');
            }
        }
        fen.push(' ');
        fen.push(self.setup.turn.char());
        fen.push(' ');
        let mut castling = String::new();
        for (color, side, letter) in [
            (Color::White, CastlingSide::KingSide, 'K'),
            (Color::White, CastlingSide::QueenSide, 'Q'),
            (Color::Black, CastlingSide::KingSide, 'k'),
            (Color::Black, CastlingSide::QueenSide, 'q'),
        ] {
            if self.castling_right(color, side) {
                castling.push(letter);
            }
        }
        if castling.is_empty() {
            fen.push('-');
        } else {
            fen.push_str(&castling);
        }
        fen.push(' ');
        match self.setup.ep_square {
            Some(square) => fen.push_str(&square.to_string()),
            None => fen.push('-'),
        }
        fen.push_str(&format!(
            " {} {}",
            self.setup.halfmoves,
            self.setup.fullmoves.get()
        ));
        fen
    }

    /// Every reason the position cannot be played. The checks follow the
    /// validity requirements of a standard chess position: one king per side,
    /// no pawns on the backranks, the side not to move is not in check, the
    /// position is not already over, castling rights match the home squares,
    /// and the side has no more material than a legal game can provide.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let board = &self.setup.board;

        for color in Color::ALL {
            let kings = (board.kings() & board.by_color(color)).count();
            match kings {
                0 => errors.push(format!("{} has no king on the board.", color_name(color))),
                1 => {}
                n => errors.push(format!(
                    "{} has {} kings on the board; exactly one is allowed.",
                    color_name(color),
                    n
                )),
            }
        }

        let backrank_pawns = board.pawns() & Bitboard::BACKRANKS;
        if backrank_pawns.any() {
            let squares: Vec<String> = backrank_pawns
                .into_iter()
                .map(|square| square.to_string())
                .collect();
            errors.push(format!(
                "Pawns cannot stand on the first or eighth rank ({}).",
                squares.join(", ")
            ));
        }

        let mut too_much_material = false;
        for color in Color::ALL {
            if !has_standard_material(board, color) {
                too_much_material = true;
                errors.push(format!(
                    "{} has too much material for a legal game (more than eight pawns and promotions).",
                    color_name(color)
                ));
            }
        }

        // Kings next to each other attack each other. This is always an
        // opposite check too, but the explicit message tells the user what to
        // move.
        let kings_adjacent = match (board.king_of(Color::White), board.king_of(Color::Black)) {
            (Some(white), Some(black)) if board.attacks_from(white).contains(black) => {
                errors.push(
                    "The kings stand on adjacent squares; they cannot be next to each other."
                        .to_string(),
                );
                true
            }
            _ => false,
        };

        // A side can only be in check when it just moved. If the side that is
        // *not* to move is attacked, the position could not arise from a legal
        // move and no engine can play on from it.
        let not_to_move = self.setup.turn.other();
        if !kings_adjacent {
            if let Some(king) = board.king_of(not_to_move) {
                if board
                    .attacks_to(king, self.setup.turn, board.occupied())
                    .any()
                {
                    errors.push(format!(
                        "{} is in check, but {} is to move. The side not to move cannot be in check.",
                        color_name(not_to_move),
                        color_name(self.setup.turn)
                    ));
                }
            }
        }

        let mut castling_error = false;
        for color in Color::ALL {
            for side in [CastlingSide::KingSide, CastlingSide::QueenSide] {
                let corner = corner_square(color, side);
                if !self.setup.castling_rights.contains(corner) {
                    continue;
                }
                let king_home = home_king_square(color);
                let king_ok = board.piece_at(king_home)
                    == Some(Piece {
                        color,
                        role: Role::King,
                    });
                let rook_ok = board.piece_at(corner)
                    == Some(Piece {
                        color,
                        role: Role::Rook,
                    });
                if !king_ok || !rook_ok {
                    castling_error = true;
                    errors.push(format!(
                        "{} cannot castle {}: the king must stand on {} and a rook on {}.",
                        color_name(color),
                        side_name(side),
                        king_home,
                        corner
                    ));
                }
            }
        }

        match Chess::from_setup(self.setup.clone(), CastlingMode::Standard) {
            Ok(position) => {
                if position.is_checkmate() {
                    errors.push(format!(
                        "{} is checkmated. No game can be played from this position.",
                        color_name(position.turn())
                    ));
                } else if position.is_stalemate() {
                    errors.push(format!(
                        "{} has no legal move and is not in check (stalemate). No game can be played from this position.",
                        color_name(position.turn())
                    ));
                }
            }
            Err(err) => {
                let kinds = err.kinds();
                if kinds.contains(PositionErrorKinds::IMPOSSIBLE_CHECK) {
                    errors.push(
                        "The position contains an impossible check: the checking pieces could not have reached the king in one legal move."
                            .to_string(),
                    );
                }
                if kinds.contains(PositionErrorKinds::TOO_MUCH_MATERIAL) && !too_much_material {
                    errors.push(
                        "One side has more material than a legal game can provide.".to_string(),
                    );
                }
                if kinds.contains(PositionErrorKinds::INVALID_EP_SQUARE) {
                    errors
                        .push("The en-passant square is not valid for this position.".to_string());
                }
                if kinds.contains(PositionErrorKinds::INVALID_CASTLING_RIGHTS) && !castling_error {
                    errors.push(
                        "A castling right does not match the king and rook positions.".to_string(),
                    );
                }
            }
        }

        errors
    }

    /// Turn the edited position into a playable [`Chess`], or return the
    /// reasons it is not playable.
    pub fn to_chess(&self) -> Result<Chess, Vec<String>> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(errors);
        }
        Chess::from_setup(self.setup.clone(), CastlingMode::Standard)
            .map_err(|err| vec![format!("The position is not valid: {}.", err)])
    }
}

/// Draw the Setup Board panel in the right dashboard. Returns the requested
/// close/cancel action.
pub fn show(ui: &mut Ui, state: &mut SetupBoard, assets: &Assets) -> SetupAction {
    state.refresh_errors();
    let mut action = SetupAction::None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Setup Board")
                        .size(18.0)
                        .strong()
                        .color(theme::NEON_CYAN),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let close = ui.add_enabled(
                        state.errors().is_empty(),
                        egui::Button::new("Close"),
                    );
                    let close = if state.errors().is_empty() {
                        close.on_hover_text("Use this position and return to the board")
                    } else {
                        close.on_disabled_hover_text("Fix the errors below before closing")
                    };
                    if close.clicked() {
                        action = SetupAction::Close;
                    }
                    if ui
                        .button("Cancel")
                        .on_hover_text("Leave setup without changing the position")
                        .clicked()
                    {
                        action = SetupAction::Cancel;
                    }
                });
            });
            ui.add_space(6.0);

            ui.label(RichText::new("FEN").color(theme::MUTED).size(14.0));
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 78.0).max(120.0);
                let id = ui.make_persistent_id("setup_board_fen");
                let selection = crate::gui::text_field::selection_before(ui, id);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut state.fen_draft)
                        .id(id)
                        .font(egui::FontId::monospace(15.0))
                        .desired_width(width)
                        .hint_text("paste a FEN and press Enter"),
                );
                crate::gui::text_field::after_edit(
                    ui,
                    id,
                    &mut state.fen_draft,
                    selection,
                    &field,
                );
                let enter = field.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if ui.button("Apply").clicked() || enter {
                    let _ = state.apply_fen();
                }
            });
            if let Some(error) = state.fen_error() {
                ui.label(
                    RichText::new(error)
                        .small()
                        .color(theme::DANGER_RED),
                );
            }
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui
                    .button("Clear board")
                    .on_hover_text("Remove every piece from the board")
                    .clicked()
                {
                    state.clear();
                }
                ui.add_space(8.0);
                ui.label("Side to move");
                egui::ComboBox::from_id_salt("setup_board_turn")
                    .selected_text(color_name(state.turn()))
                    .show_ui(ui, |ui| {
                        for color in Color::ALL {
                            if ui
                                .selectable_label(state.turn() == color, color_name(color))
                                .clicked()
                            {
                                state.set_turn(color);
                            }
                        }
                    });
            });
            ui.add_space(6.0);

            egui::Grid::new("setup_board_castling")
                .num_columns(3)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    ui.label("");
                    ui.label(RichText::new("White").strong());
                    ui.label(RichText::new("Black").strong());
                    ui.end_row();

                    for (label, side) in [
                        ("O-O", CastlingSide::KingSide),
                        ("O-O-O", CastlingSide::QueenSide),
                    ] {
                        ui.label(label);
                        for color in Color::ALL {
                            let mut value = state.castling_right(color, side);
                            if ui
                                .checkbox(&mut value, "")
                                .on_hover_text(format!(
                                    "{} may castle {}",
                                    color_name(color),
                                    side_name(side)
                                ))
                                .changed()
                            {
                                state.set_castling_right(color, side, value);
                            }
                        }
                        ui.end_row();
                    }
                });
            ui.separator();

            ui.label(RichText::new("Pieces").color(theme::MUTED).size(14.0));
            palette(ui, state, assets, Color::White);
            palette(ui, state, assets, Color::Black);
            ui.add_space(4.0);
            let hint = match state.selected() {
                Some(piece) => format!(
                    "{} {} selected — left-click a square to place it; right-click removes a piece.",
                    color_name(piece.color),
                    role_name(piece.role)
                ),
                None => "Choose a piece, then left-click a square; right-click removes a piece."
                    .to_string(),
            };
            ui.label(RichText::new(hint).small().color(theme::MUTED));

            ui.add_space(8.0);
            if state.errors().is_empty() {
                ui.label(
                    RichText::new("The position is valid.")
                        .small()
                        .color(theme::NEON_GREEN),
                );
            } else {
                ui.label(
                    RichText::new("The position cannot be played yet:")
                        .strong()
                        .color(theme::DANGER_RED),
                );
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for error in state.errors() {
                            ui.label(
                                RichText::new(format!("• {}", error))
                                    .small()
                                    .color(theme::DANGER_RED),
                            );
                        }
                    });
            }
        });

    action
}

/// One row of piece buttons for `color`.
fn palette(ui: &mut Ui, state: &mut SetupBoard, assets: &Assets, color: Color) {
    ui.horizontal(|ui| {
        for role in PALETTE {
            let piece = Piece { color, role };
            let selected = state.selected() == Some(piece);
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::click());
            let painter = ui.painter();
            let fill = if selected {
                PALETTE_SELECTED_BG
            } else if response.hovered() {
                PALETTE_HOVER_BG
            } else {
                PALETTE_BG
            };
            painter.rect_filled(rect, 4.0, fill);
            if selected {
                painter.rect_stroke(
                    rect,
                    4.0,
                    Stroke::new(1.5, theme::NEON_CYAN),
                    StrokeKind::Inside,
                );
            }
            let id = PieceId {
                color: color.into(),
                kind: PieceKind::from_role(role),
            };
            if let Some(texture) = assets.pieces.get(id) {
                draw_piece(painter, rect.center(), texture.id(), 40.0);
            }
            let response =
                response.on_hover_text(format!("{} {}", color_name(color), role_name(role)));
            if response.clicked() {
                state.set_selected(if selected { None } else { Some(piece) });
            }
        }
    });
}

/// Keep standard castling rights only, so a position loaded from a Chess960
/// FEN cannot put the checkboxes in an unreachable state. The editor only
/// offers the four corner rights.
fn keep_standard_castling_rights(setup: &mut Setup) {
    let rights = setup.castling_rights;
    setup.castling_rights = Bitboard::EMPTY;
    for color in Color::ALL {
        for side in [CastlingSide::KingSide, CastlingSide::QueenSide] {
            let corner = corner_square(color, side);
            if rights.contains(corner) {
                setup.castling_rights.set(corner, true);
            }
        }
    }
}

/// The home square of `color`'s king.
fn home_king_square(color: Color) -> Square {
    match color {
        Color::White => Square::E1,
        Color::Black => Square::E8,
    }
}

/// The corner rook square for one castling right.
fn corner_square(color: Color, side: CastlingSide) -> Square {
    match (color, side) {
        (Color::White, CastlingSide::KingSide) => Square::H1,
        (Color::White, CastlingSide::QueenSide) => Square::A1,
        (Color::Black, CastlingSide::KingSide) => Square::H8,
        (Color::Black, CastlingSide::QueenSide) => Square::A8,
    }
}

/// `White` or `Black`, as the panel spells it.
fn color_name(color: Color) -> &'static str {
    match color {
        Color::White => "White",
        Color::Black => "Black",
    }
}

/// `king-side` or `queen-side`.
fn side_name(side: CastlingSide) -> &'static str {
    match side {
        CastlingSide::KingSide => "king-side",
        CastlingSide::QueenSide => "queen-side",
    }
}

/// A palette tooltip name.
fn role_name(role: Role) -> &'static str {
    match role {
        Role::King => "king",
        Role::Queen => "queen",
        Role::Rook => "rook",
        Role::Bishop => "bishop",
        Role::Knight => "knight",
        Role::Pawn => "pawn",
    }
}

/// The material rule of a standard game: the pawns plus the pieces beyond the
/// starting army must not exceed eight. Bishops are counted by square colour,
/// because promotion cannot add a bishop on the colour the side already has
/// two of without using a promotion.
fn has_standard_material(board: &Board, color: Color) -> bool {
    let our = board.by_color(color);
    let promoted = board.queens().intersect(our).count().saturating_sub(1)
        + board.rooks().intersect(our).count().saturating_sub(2)
        + board.knights().intersect(our).count().saturating_sub(2)
        + board
            .bishops()
            .intersect(our)
            .intersect(Bitboard::LIGHT_SQUARES)
            .count()
            .saturating_sub(1)
        + board
            .bishops()
            .intersect(our)
            .intersect(Bitboard::DARK_SQUARES)
            .count()
            .saturating_sub(1);
    board.pawns().intersect(our).count() + promoted <= 8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the editor straight from a FEN, without the legality check that
    /// `Chess::from_setup` would apply first. The tests need to inspect the
    /// editor's own reasons for refusing a position.
    fn board_from(fen: &str) -> SetupBoard {
        let setup = shakmaty::fen::Fen::from_ascii(fen.as_bytes())
            .unwrap()
            .into_setup();
        let mut board = SetupBoard {
            setup,
            selected: None,
            errors: Vec::new(),
            fen_draft: String::new(),
            fen_error: None,
            revision: 0,
        };
        board.changed();
        board
    }

    fn errors_for(fen: &str) -> Vec<String> {
        board_from(fen).validate()
    }

    #[test]
    fn the_starting_position_is_valid_and_has_all_castling_rights() {
        let board = SetupBoard::from_position(&Chess::default());
        assert!(board.errors().is_empty(), "{:?}", board.errors());
        for color in Color::ALL {
            for side in [CastlingSide::KingSide, CastlingSide::QueenSide] {
                assert!(board.castling_right(color, side));
            }
        }
        assert_eq!(
            SetupBoard::from_position(&Chess::default())
                .to_chess()
                .unwrap(),
            Chess::default()
        );
    }

    #[test]
    fn more_than_one_king_is_reported() {
        let errors = errors_for("4k3/8/8/8/8/8/8/3KK3 w - - 0 1");
        assert!(
            errors.iter().any(|e| e.contains("White has 2 kings")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_missing_king_is_reported() {
        let errors = errors_for("8/8/8/8/8/8/8/3K4 w - - 0 1");
        assert!(
            errors.iter().any(|e| e.contains("Black has no king")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_side_in_check_that_is_not_to_move_is_reported() {
        // Black king on e8 is attacked by the white rook on e1, but White is
        // to move: the position could not have arisen from a legal move.
        let errors = errors_for("4k3/8/8/8/8/8/8/4RK2 w - - 0 1");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Black is in check, but White is to move")),
            "{errors:?}"
        );
    }

    #[test]
    fn checkmate_and_stalemate_are_reported_as_terminal() {
        let mate = errors_for("7k/6Q1/6K1/8/8/8/8/8 b - - 0 1");
        assert!(
            mate.iter().any(|e| e.contains("Black is checkmated")),
            "{mate:?}"
        );
        let stalemate = errors_for("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1");
        assert!(
            stalemate.iter().any(|e| e.contains("stalemate")),
            "{stalemate:?}"
        );
    }

    #[test]
    fn adjacent_kings_are_reported_once_with_their_own_reason() {
        let errors = errors_for("8/8/8/8/8/8/8/4Kk2 w - - 0 1");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("kings stand on adjacent squares")),
            "{errors:?}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.contains("in check, but") || e.contains("adjacent squares"))
                .count(),
            1,
            "the adjacency reason replaces the generic opposite-check reason: {errors:?}"
        );
    }

    #[test]
    fn pawns_on_a_backrank_are_reported() {
        let errors = errors_for("4k3/8/8/8/8/8/8/P3K3 w - - 0 1");
        assert!(
            errors.iter().any(|e| e.contains("Pawns cannot stand")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_castling_right_without_its_rook_is_reported() {
        let mut board = SetupBoard::from_position(&Chess::default());
        board.remove(Square::H1);
        assert!(
            board
                .errors()
                .iter()
                .any(|e| e.contains("White cannot castle king-side")),
            "{:?}",
            board.errors()
        );
    }

    #[test]
    fn too_much_material_is_reported_for_the_side_that_has_it() {
        // Nine white pawns.
        let errors = errors_for("4k3/8/8/8/8/P7/PPPPPPPP/4K3 w - - 0 1");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("White has too much material")),
            "{errors:?}"
        );
    }

    #[test]
    fn clearing_the_board_leaves_both_kings_missing_and_no_castling_rights() {
        let mut board = SetupBoard::from_position(&Chess::default());
        board.clear();
        assert_eq!(
            board.fen(),
            "8/8/8/8/8/8/8/8 w - - 0 1",
            "a cleared board must have no pieces and no castling rights"
        );
        assert!(board
            .errors()
            .iter()
            .any(|e| e.contains("White has no king")));
        assert!(board
            .errors()
            .iter()
            .any(|e| e.contains("Black has no king")));
    }

    #[test]
    fn placing_and_removing_pieces_edits_the_board() {
        let mut board = SetupBoard::from_position(&Chess::default());
        board.clear();
        board.set_turn(Color::Black);
        board.set_selected(Some(Piece {
            color: Color::White,
            role: Role::King,
        }));
        board.place(Square::E1);
        board.set_selected(Some(Piece {
            color: Color::Black,
            role: Role::King,
        }));
        board.place(Square::E8);
        board.set_selected(Some(Piece {
            color: Color::White,
            role: Role::Rook,
        }));
        board.place(Square::A1);
        board.set_castling_right(Color::White, CastlingSide::QueenSide, true);
        assert!(board.errors().is_empty(), "{:?}", board.errors());
        assert_eq!(
            board.to_chess().unwrap(),
            Chess::from_setup(
                shakmaty::fen::Fen::from_ascii(b"4k3/8/8/8/8/8/8/R3K3 b Q - 0 1")
                    .unwrap()
                    .into_setup(),
                CastlingMode::Standard
            )
            .unwrap()
        );

        board.remove(Square::A1);
        assert!(board.board().piece_at(Square::A1).is_none());
        assert!(
            board
                .errors()
                .iter()
                .any(|e| e.contains("cannot castle queen-side")),
            "{:?}",
            board.errors()
        );
    }

    #[test]
    fn a_typed_fen_replaces_the_board_and_a_bad_one_is_reported() {
        let mut board = SetupBoard::from_position(&Chess::default());
        board
            .set_fen("4k3/8/8/8/8/8/8/4K3 b - - 0 1")
            .expect("a valid FEN is accepted");
        assert_eq!(board.fen(), "4k3/8/8/8/8/8/8/4K3 b - - 0 1");
        assert!(board.errors().is_empty(), "{:?}", board.errors());
        assert_eq!(
            board.fen_draft,
            board.fen(),
            "the field shows the loaded position"
        );

        let before = board.fen();
        let error = board
            .set_fen("this is not a fen")
            .expect_err("garbage is refused");
        assert!(error.contains("Not a valid FEN"), "{error}");
        assert_eq!(board.fen(), before, "a bad FEN leaves the position alone");
        assert!(board.fen_error().is_some());

        board.fen_draft = "4k3/8/8/8/8/8/8/4K3 w - - 0 1".to_string();
        board.apply_fen().expect("the panel's field applies too");
        assert_eq!(board.fen(), "4k3/8/8/8/8/8/8/4K3 w - - 0 1");
        assert!(board.fen_error().is_none());
    }

    #[test]
    fn the_first_edit_drops_a_preserved_en_passant_square() {
        let fen = "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2";
        let mut board = board_from(fen);
        assert_eq!(board.fen(), fen, "opening the editor preserves the FEN");
        board.set_selected(Some(Piece {
            color: Color::White,
            role: Role::Knight,
        }));
        board.place(Square::F3);
        assert!(
            !board.fen().contains(" d6 "),
            "a piece move drops the en-passant target: {}",
            board.fen()
        );
    }

    #[test]
    fn fen_round_trips_through_the_editor() {
        let mut board = SetupBoard::from_position(&Chess::default());
        board.set_turn(Color::Black);
        board.set_castling_right(Color::White, CastlingSide::QueenSide, false);
        let fen = board.fen();
        assert_eq!(
            fen,
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR b Kkq - 0 1"
        );
        assert!(board.errors().is_empty(), "{:?}", board.errors());
    }

    #[test]
    fn to_chess_refuses_an_invalid_position_with_its_reasons() {
        let mut board = SetupBoard::from_position(&Chess::default());
        board.clear();
        let errors = board.to_chess().unwrap_err();
        assert_eq!(errors, board.errors());
        assert!(!errors.is_empty());
    }
}

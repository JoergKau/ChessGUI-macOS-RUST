//! Game modes and seats: which side a human plays and which side an engine
//! plays.
//!
//! Pure rules, with no UI and no engine I/O. The app asks [`GameMode::seat`]
//! who moves for the side to move; a human seat takes its move on the board,
//! an engine seat is asked for one.

use crate::chess::PieceColor;
use crate::engine::protocol::Score;

/// An engine accepts a draw offer when its evaluation is within this many
/// centipawns of equal (half a pawn).
pub const DRAW_ACCEPT_CP: i32 = 50;

/// Whether an engine accepts a draw offer, given its evaluation from White's
/// point of view. A mate score, or no evaluation yet, is declined.
pub fn accepts_draw(white_pov: Option<Score>) -> bool {
    matches!(white_pov, Some(Score::Cp(cp)) if cp.abs() <= DRAW_ACCEPT_CP)
}

/// What the session is for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GameMode {
    /// Engines analyse; both sides are moved by hand.
    Analysis,
    /// The human plays `human`; the engine in `engine` plays the other colour.
    HumanVsEngine {
        /// The colour the human plays.
        human: PieceColor,
        /// The slot of the engine (`0` = A, `1` = B).
        engine: usize,
    },
    /// Engine A plays White and engine B plays Black.
    EngineVsEngine,
}

/// Modes offered in the Game menu, in menu order.
pub const MODES: [GameMode; 6] = [
    GameMode::Analysis,
    GameMode::HumanVsEngine {
        human: PieceColor::White,
        engine: 0,
    },
    GameMode::HumanVsEngine {
        human: PieceColor::Black,
        engine: 0,
    },
    GameMode::HumanVsEngine {
        human: PieceColor::White,
        engine: 1,
    },
    GameMode::HumanVsEngine {
        human: PieceColor::Black,
        engine: 1,
    },
    GameMode::EngineVsEngine,
];

/// Who moves for one colour.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Seat {
    /// A person moves on the board.
    Human,
    /// The engine in this slot (`0` = A, `1` = B) moves.
    Engine(usize),
}

impl GameMode {
    /// Who moves for `color` in this mode. In analysis mode nobody is an
    /// engine, so both colours are human seats.
    pub fn seat(self, color: PieceColor) -> Seat {
        match self {
            GameMode::Analysis => Seat::Human,
            GameMode::HumanVsEngine { human, engine } => {
                if color == human {
                    Seat::Human
                } else {
                    Seat::Engine(engine)
                }
            }
            GameMode::EngineVsEngine => match color {
                PieceColor::White => Seat::Engine(0),
                PieceColor::Black => Seat::Engine(1),
            },
        }
    }

    /// The engine slots this mode plays with. They are switched on when the
    /// mode starts, so a game starts without an extra Start press.
    pub fn engine_slots(self) -> &'static [usize] {
        match self {
            GameMode::Analysis => &[],
            GameMode::HumanVsEngine { engine, .. } => match engine {
                0 => &[0],
                _ => &[1],
            },
            GameMode::EngineVsEngine => &[0, 1],
        }
    }

    /// Menu label for the mode.
    pub fn label(self) -> String {
        match self {
            GameMode::Analysis => "Analysis only".to_string(),
            GameMode::HumanVsEngine { human, engine } => {
                let side = match human {
                    PieceColor::White => "White",
                    PieceColor::Black => "Black",
                };
                format!(
                    "Play {} vs engine {}",
                    side,
                    char::from(b'A' + engine as u8)
                )
            }
            GameMode::EngineVsEngine => "Engine A vs engine B".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_mode_has_no_engine_seats() {
        assert_eq!(GameMode::Analysis.seat(PieceColor::White), Seat::Human);
        assert_eq!(GameMode::Analysis.seat(PieceColor::Black), Seat::Human);
        assert!(GameMode::Analysis.engine_slots().is_empty());
    }

    #[test]
    fn human_vs_engine_gives_the_other_colour_to_engine_a() {
        let as_white = GameMode::HumanVsEngine {
            human: PieceColor::White,
            engine: 0,
        };
        assert_eq!(as_white.seat(PieceColor::White), Seat::Human);
        assert_eq!(as_white.seat(PieceColor::Black), Seat::Engine(0));
        assert_eq!(as_white.engine_slots(), &[0]);

        let as_black = GameMode::HumanVsEngine {
            human: PieceColor::Black,
            engine: 0,
        };
        assert_eq!(as_black.seat(PieceColor::Black), Seat::Human);
        assert_eq!(as_black.seat(PieceColor::White), Seat::Engine(0));
    }

    #[test]
    fn the_human_can_play_either_colour_against_either_engine() {
        for engine in 0..2 {
            for human in [PieceColor::White, PieceColor::Black] {
                let mode = GameMode::HumanVsEngine { human, engine };
                assert_eq!(mode.seat(human), Seat::Human);
                assert_eq!(mode.seat(human.opposite()), Seat::Engine(engine));
                assert_eq!(mode.engine_slots(), &[engine]);
            }
        }
    }

    #[test]
    fn engine_vs_engine_binds_a_to_white_and_b_to_black() {
        assert_eq!(
            GameMode::EngineVsEngine.seat(PieceColor::White),
            Seat::Engine(0)
        );
        assert_eq!(
            GameMode::EngineVsEngine.seat(PieceColor::Black),
            Seat::Engine(1)
        );
        assert_eq!(GameMode::EngineVsEngine.engine_slots(), &[0, 1]);
    }

    #[test]
    fn engine_accepts_a_draw_only_near_equal() {
        assert!(accepts_draw(Some(Score::Cp(0))));
        assert!(accepts_draw(Some(Score::Cp(DRAW_ACCEPT_CP))));
        assert!(accepts_draw(Some(Score::Cp(-DRAW_ACCEPT_CP))));
        assert!(!accepts_draw(Some(Score::Cp(DRAW_ACCEPT_CP + 1))));
        assert!(!accepts_draw(Some(Score::Mate(3))));
        assert!(!accepts_draw(Some(Score::Mate(-3))));
        assert!(!accepts_draw(None), "no evaluation yet means no draw");
    }

    #[test]
    fn every_menu_mode_has_a_label() {
        let labels: Vec<String> = MODES.iter().map(|m| m.label()).collect();
        assert_eq!(labels.len(), 6);
        assert!(labels.iter().all(|l| !l.is_empty()));
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "menu labels must be distinct");
    }
}

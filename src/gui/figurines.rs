//! Figurine piece letters for SAN text.
//!
//! The PV and the move list can show `♘f6` instead of `Nf6`. The bundled
//! `public/font/lichess-chess.ttf` maps the ASCII piece letters
//! (`K Q R B N`) to figurines, so a move is split into piece-letter runs
//! (drawn with that font) and plain runs (body font). Pawn moves and castling
//! stay plain; a promotion piece after `=` is a figurine too.

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Color32, FontId, TextFormat, Ui};

use crate::gui::theme;

/// Split one SAN token into runs `(text, is_piece_letter)`.
pub fn san_parts(san: &str) -> Vec<(&str, bool)> {
    let castling = san.starts_with('O');
    let mut parts = Vec::new();
    let mut run_start = 0usize;
    let mut run_is_piece: Option<bool> = None;
    let mut previous = None;
    for (index, ch) in san.char_indices() {
        let is_piece = !castling
            && matches!(ch, 'K' | 'Q' | 'R' | 'B' | 'N')
            && (index == 0 || previous == Some('='));
        if run_is_piece != Some(is_piece) {
            if let Some(kind) = run_is_piece {
                parts.push((&san[run_start..index], kind));
            }
            run_start = index;
            run_is_piece = Some(is_piece);
        }
        previous = Some(ch);
    }
    if let Some(kind) = run_is_piece {
        parts.push((&san[run_start..], kind));
    }
    parts
}

/// Append one SAN token to `job`. Piece letters use the figurine font when
/// `use_figurines` is on and the font was installed; everything else uses
/// `body`.
pub fn append_san(
    job: &mut LayoutJob,
    san: &str,
    body: &FontId,
    color: Color32,
    use_figurines: bool,
) {
    let figurine = FontId::new(
        body.size,
        egui::FontFamily::Name(theme::FIGURINE_FAMILY.into()),
    );
    let use_figurines = use_figurines && theme::figurines_available();
    for (text, is_piece) in san_parts(san) {
        let font = if is_piece && use_figurines {
            figurine.clone()
        } else {
            body.clone()
        };
        job.append(text, 0.0, TextFormat::simple(font, color));
    }
}

/// A one-line layout job for a SAN move in the move list.
pub fn san_job(ui: &Ui, san: &str, color: Color32, use_figurines: bool) -> LayoutJob {
    let body = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_default();
    let mut job = LayoutJob::default();
    append_san(&mut job, san, &body, color, use_figurines);
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn san_is_split_into_figurine_piece_letters_and_the_rest() {
        assert_eq!(san_parts("Nf6"), vec![("N", true), ("f6", false)]);
        assert_eq!(san_parts("Qxe7+"), vec![("Q", true), ("xe7+", false)]);
        assert_eq!(san_parts("e8=Q"), vec![("e8=", false), ("Q", true)]);
        assert_eq!(
            san_parts("bxa8=Q+"),
            vec![("bxa8=", false), ("Q", true), ("+", false)]
        );
        assert_eq!(
            san_parts("O-O"),
            vec![("O-O", false)],
            "castling stays text"
        );
        assert_eq!(san_parts("O-O-O"), vec![("O-O-O", false)]);
        assert_eq!(
            san_parts("exd5"),
            vec![("exd5", false)],
            "a pawn move has no piece letter"
        );
        assert_eq!(san_parts("R1e2"), vec![("R", true), ("1e2", false)]);
        assert_eq!(san_parts("N"), vec![("N", true)]);
        assert!(san_parts("").is_empty());
    }
}

//! Dark, high-contrast analysis palette: near-black panels, neon green for
//! evaluations, neon cyan for search metrics.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Color32, FontId, TextStyle};

/// Family name of the chess figurine font used in the PV (`♘f6`).
pub const FIGURINE_FAMILY: &str = "siliconboard-figurines";

/// Whether [`install_chess_font`] succeeded, so the PV can use figurines.
static FIGURINES_READY: AtomicBool = AtomicBool::new(false);

/// Panel and window background (`#16161a`).
pub const PANEL_BG: Color32 = Color32::from_rgb(0x16, 0x16, 0x1a);

/// Background of an engine card or the book box.
pub const CARD_BG: Color32 = Color32::from_rgb(0x20, 0x20, 0x28);

/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xd8, 0xde, 0xe9);

/// Secondary text and captions.
pub const MUTED: Color32 = Color32::from_rgb(0x7a, 0x7f, 0x8c);

/// Evaluation scores (`#00ff88`).
pub const NEON_GREEN: Color32 = Color32::from_rgb(0x00, 0xff, 0x88);

/// Search metrics: depth, NPS, nodes (`#8be9fd`).
pub const NEON_CYAN: Color32 = Color32::from_rgb(0x8b, 0xe9, 0xfd);

/// Accent blue: the running-engine toggle and the rule under each card header.
pub const BLUE: Color32 = Color32::from_rgb(0x3a, 0x74, 0xd6);

/// White's moves in the move list.
pub const WHITE_MOVE: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);

/// Black's moves in the move list (light red), so each side reads at a glance.
pub const BLACK_MOVE: Color32 = Color32::from_rgb(0xff, 0x9e, 0x9e);

/// Engine is searching.
pub const ALERT_YELLOW: Color32 = Color32::from_rgb(0xf1, 0xfa, 0x8c);

/// Analysis arrow of engine A (green), blended at 60 % opacity over the board.
pub const ARROW_ENGINE_A: Color32 = Color32::from_rgba_unmultiplied_const(0x15, 0x78, 0x1b, 0x99);

/// Analysis arrow of engine B (red), blended at 60 % opacity over the board.
pub const ARROW_ENGINE_B: Color32 = Color32::from_rgba_unmultiplied_const(0x88, 0x20, 0x20, 0x99);

/// Engine offline or an error.
pub const DANGER_RED: Color32 = Color32::from_rgb(0xff, 0x55, 0x55);

/// Dark fill for pills and buttons on cards.
pub const BAR_BLACK: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x33);

/// Top toolbar background, slightly lighter than the panels.
pub const TOOLBAR_BG: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);

/// Toolbar buttons: fill, border, hover and open/active fills. These mirror
/// the flat toolbar palette.
pub const TOOLBAR_BUTTON_BG: Color32 = Color32::from_rgb(0x24, 0x26, 0x2b);
pub const TOOLBAR_STROKE: Color32 = Color32::from_rgb(0x34, 0x37, 0x3f);
pub const TOOLBAR_HOVER: Color32 = Color32::from_rgb(0x30, 0x33, 0x3a);
pub const TOOLBAR_ACTIVE: Color32 = Color32::from_rgb(0x3c, 0x40, 0x4a);

/// Text sizes. egui's defaults (Small = 9 pt) are too small for a desktop
/// analysis window, so every style is set explicitly.
fn text_styles() -> [(TextStyle, FontId); 5] {
    [
        (TextStyle::Heading, FontId::proportional(24.0)),
        (TextStyle::Body, FontId::proportional(17.0)),
        (TextStyle::Button, FontId::proportional(17.0)),
        (TextStyle::Small, FontId::proportional(15.0)),
        (TextStyle::Monospace, FontId::monospace(16.0)),
    ]
}

/// Apply the SiliconBoard dark theme and text sizes to the whole egui context.
pub fn apply(ctx: &egui::Context) {
    // egui's default preference follows the system appearance, and
    // `set_visuals` only edits the style of the *current* theme. On a Mac in
    // Light mode that made `Visuals::dark()` land in the light style, so the
    // app painted the default egui light theme while widgets with explicit
    // colours (the engine cards) stayed dark. Pin the preference first, and
    // set both styles so a later theme change cannot bring the light palette
    // back.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = text_styles().into_iter().collect();
    });
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL_BG;
    visuals.window_fill = CARD_BG;
    visuals.extreme_bg_color = PANEL_BG;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = NEON_GREEN.gamma_multiply(0.35);
    visuals.hyperlink_color = NEON_CYAN;
    ctx.set_visuals_of(egui::Theme::Dark, visuals.clone());
    ctx.set_visuals_of(egui::Theme::Light, visuals);
}

/// Add the chess figurine font as a named family, used for the piece letters in
/// the PV. The font is `public/font/lichess-chess.ttf` (a Noto Sans subset).
pub fn install_chess_font(ctx: &egui::Context, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    ctx.add_font(egui::FontInsert {
        name: "siliconboard-chess-figurines".to_owned(),
        data: egui::FontData::from_owned(bytes),
        families: vec![egui::InsertFontFamily {
            family: egui::FontFamily::Name(FIGURINE_FAMILY.into()),
            priority: egui::FontPriority::Highest,
        }],
    });
    FIGURINES_READY.store(true, Ordering::Release);
    Ok(())
}

/// Whether the figurine font is available. When it is not, the PV falls back
/// to plain SAN (`Nf6`).
pub fn figurines_available() -> bool {
    FIGURINES_READY.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: on a Mac set to Light, the app used the default egui light
    /// style because the theme preference was left on `System`. The palette
    /// must stay dark no matter what the system reports.
    #[test]
    fn the_system_light_theme_cannot_override_the_dark_palette() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            system_theme: Some(egui::Theme::Light),
            ..Default::default()
        };
        // The test has no renderer, so consume the font-atlas delta instead of
        // letting it trip the "unapplied textures" assertion on drop.
        let mut output = ctx.run_ui(input, |_ui| {});
        output.textures_delta.clear();

        apply(&ctx);

        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(ctx.global_style().visuals.panel_fill, PANEL_BG);
    }
}

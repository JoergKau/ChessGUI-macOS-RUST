//! The Engine → Load tablebases… dialog: the `SyzygyPath` UCI option for the
//! loaded engines.
//!
//! Syzygy tablebases live in ordinary folders of `.rtbw` / `.rtbz` files; 5-men
//! and 6-men files often sit in different folders. Engines take one path
//! string, with the folders joined by the platform separator (`:` on macOS and
//! Linux, `;` on Windows — the same rule en-croissant uses). The dialog edits
//! that one string; the app sends it to every loaded engine and gives it to
//! each engine started afterwards. The app applies the string as it is: what
//! counts as a usable tablebase installation is the engine's business, not the
//! GUI's.

use eframe::egui::{self, Align2, FontId, RichText, Ui, Vec2};

use std::path::Path;

use crate::gui::theme;

/// The UCI option this dialog edits, sent to the engines as
/// `setoption name SyzygyPath value …`.
pub const SYZYGY_PATH: &str = "SyzygyPath";

/// Value that clears `SyzygyPath`. Stockfish-derived engines (Stockfish,
/// PawnForge, …) accept the literal `<empty>`, while several of them do not
/// parse a missing value and keep their old path when sent an empty string.
pub const SYZYGY_EMPTY: &str = "<empty>";

/// Fixed width of the path field. A width taken from `ui.available_width()`
/// feeds an auto-size loop: the field widens the window, which widens the
/// field, until the dialog covers the screen.
const PATH_FIELD_WIDTH: f32 = 420.0;

/// Minimum width of the dialog, wide enough for the fixed field and buttons.
const DIALOG_MIN_WIDTH: f32 = 520.0;

/// Separator between tablebase folders in `SyzygyPath`: `;` on Windows,
/// `:` everywhere else ([UCI spec](https://www.shredderchess.com/chess-features/
/// chess-software-features/uci-protocol.html), en-croissant `EnginesPage.tsx`).
pub fn path_separator() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

/// Join tablebase folders into one `SyzygyPath` value.
pub fn join_paths<I, P>(dirs: I) -> String
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    dirs.into_iter()
        .map(|dir| dir.as_ref().display().to_string())
        .collect::<Vec<_>>()
        .join(&path_separator().to_string())
}

/// What the dialog did this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TablebaseOutcome {
    /// Still open; nothing chosen yet.
    Open,
    /// Browse…: the app opens its multi-folder picker.
    Browse,
    /// Apply the shown text as the `SyzygyPath` of every engine. An empty
    /// (or all-whitespace) string clears the tablebases.
    Apply(String),
    /// Close without changing anything.
    Cancel,
}

/// The Engine → Load tablebases… window: one editable path string plus a
/// multi-folder picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TablebaseDialog {
    draft: String,
}

impl TablebaseDialog {
    /// A dialog that starts from the current `SyzygyPath`, if one is set.
    pub fn new(current: Option<&str>) -> Self {
        Self {
            draft: current.unwrap_or("").to_string(),
        }
    }

    /// Replace the draft with `dirs` joined as one `SyzygyPath` string. Used
    /// when the folder picker reports its selection; it replaces, not
    /// extends, the text (as en-croissant's picker does).
    pub fn set_draft_dirs<I, P>(&mut self, dirs: I)
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.draft = join_paths(dirs);
    }

    /// The path string currently in the field.
    #[cfg(test)] // the Apply outcome already carries the string for the app
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Draw the dialog as a window over the board and report what happened.
    pub fn show(&mut self, ctx: &egui::Context) -> TablebaseOutcome {
        let mut outcome = TablebaseOutcome::Open;
        let separator = path_separator();
        egui::Window::new("Load tablebases")
            .collapsible(false)
            .resizable(false)
            .min_width(DIALOG_MIN_WIDTH)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Syzygy tablebases give engines endgame knowledge. Enter the \
                         folder(s) holding your .rtbw / .rtbz files — several folders \
                         are joined in one line.",
                    )
                    .color(theme::MUTED),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let field = path_field(ui, &mut self.draft);
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        outcome = TablebaseOutcome::Apply(self.draft.clone());
                    }
                    if ui.button("Browse…").clicked() {
                        outcome = TablebaseOutcome::Browse;
                    }
                });
                ui.label(
                    RichText::new(format!(
                        "Separate folders with `{}` ({})",
                        separator,
                        if cfg!(windows) {
                            "this machine is Windows"
                        } else {
                            "on macOS and Linux; use `;` on Windows"
                        }
                    ))
                    .color(theme::MUTED)
                    .small(),
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        outcome = TablebaseOutcome::Apply(self.draft.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = TablebaseOutcome::Cancel;
                    }
                });
            });
        outcome
    }
}

/// The single-line, monospaced path field, the width the dialog can spare.
fn path_field(ui: &mut Ui, draft: &mut String) -> egui::Response {
    let id = ui.make_persistent_id("tablebase_path");
    let selection = crate::gui::text_field::selection_before(ui, id);
    let response = ui.add(
        egui::TextEdit::singleline(draft)
            .id(id)
            .font(FontId::monospace(13.0))
            .desired_width(PATH_FIELD_WIDTH)
            .hint_text("/path/to/egtb"),
    );
    crate::gui::text_field::after_edit(ui, id, draft, selection, &response);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_separator_follows_the_platform() {
        let expected = if cfg!(windows) { ';' } else { ':' };
        assert_eq!(path_separator(), expected);
        assert_eq!(join_paths(["/a/egtb/5", "/b/egtb/6"]), format!("/a/egtb/5{expected}/b/egtb/6"));
    }

    #[test]
    fn the_clear_value_is_the_stockfish_empty_sentinel() {
        // PawnForge refuses an empty value ("Unknown option: SyzygyPath
        // value") and keeps the loaded path; Stockfish and PawnForge both
        // clear on `<empty>`.
        assert_eq!(SYZYGY_EMPTY, "<empty>");
    }

    #[test]
    fn the_dialog_starts_from_the_current_path() {
        let dialog = TablebaseDialog::new(Some("/tb/5:/tb/6"));
        assert_eq!(dialog.draft(), "/tb/5:/tb/6");
        assert_eq!(TablebaseDialog::new(None).draft(), "");
    }

    #[test]
    fn picked_folders_replace_the_draft_as_one_joined_line() {
        let mut dialog = TablebaseDialog::new(Some("/old:/tb"));
        dialog.set_draft_dirs(["/tb/5", "/tb/6"]);
        assert_eq!(dialog.draft(), join_paths(["/tb/5", "/tb/6"]));
    }
}

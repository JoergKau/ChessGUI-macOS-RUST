//! Right-click Cut / Copy / Paste for text fields.
//!
//! egui 0.37's `TextEdit` ships no context menu of its own, so a mouse user
//! could not cut or paste: only the keyboard shortcuts worked. This module
//! adds the menu and keeps the selection intact when the right-click lands
//! inside it — egui otherwise collapses the selection to the click point.
//!
//! Copy and Cut act on the field's text directly. Paste goes through the same
//! clipboard request the keyboard shortcut uses (the native integration reads
//! the clipboard and delivers the text next frame); the field is given focus
//! back first, because clicking a menu item would otherwise take it away.

use eframe::egui::{self, Id, Response, Ui, ViewportCommand};
use egui::text::{CCursor, CCursorRange};
use egui::widgets::text_edit::TextEditState;

/// The field's selection as of the previous frame, taken *before* the field is
/// drawn. Pass it to [`after_edit`] with the field's response.
pub fn selection_before(ui: &Ui, id: Id) -> Option<CCursorRange> {
    TextEditState::load(ui.ctx(), id).and_then(|state| state.cursor.char_range())
}

/// Restore the selection after a right-click and attach the Cut / Copy /
/// Paste / Select all menu to the field.
pub fn after_edit(
    ui: &mut Ui,
    id: Id,
    text: &mut String,
    selection_before: Option<CCursorRange>,
    response: &Response,
) {
    // egui collapses the selection on the right-button *press*, while the
    // context menu opens on the *release*. Put the selection back on the
    // press frame, so the menu (and a later Paste) still sees it.
    let right_pressed_here =
        ui.input(|i| i.pointer.secondary_pressed()) && response.contains_pointer();
    if right_pressed_here {
        restore_selection(ui, id, selection_before);
    }
    // The menu opens on the release. Moving focus to the menu makes egui
    // collapse the field's selection before the menu content is built, so
    // remember the selection as it was just before that frame.
    if response.secondary_clicked() {
        ui.ctx()
            .data_mut(|data| data.insert_temp(context_selection_key(id), selection_before));
    }
    menu(id, text, response);
}

/// Where the selection at the moment the menu opened is kept.
fn context_selection_key(id: Id) -> Id {
    id.with("context_selection")
}

/// Put the previous selection back when the right-click landed inside it.
/// A click elsewhere places the caret there, and the menu acts there instead.
fn restore_selection(ui: &Ui, id: Id, before: Option<CCursorRange>) {
    let Some(before) = before.filter(|range| !range.is_empty()) else {
        return;
    };
    let Some(mut state) = TextEditState::load(ui.ctx(), id) else {
        return;
    };
    let Some(after) = state.cursor.char_range() else {
        return;
    };
    if before.contains(after) {
        state.cursor.set_char_range(Some(before));
        state.store(ui.ctx(), id);
    }
}

fn menu(id: Id, text: &mut String, response: &Response) {
    response.context_menu(|ui| {
        // Use the selection remembered when the menu opened: the field has
        // lost focus by now and egui may already have collapsed it.
        let remembered: Option<CCursorRange> = ui
            .ctx()
            .data(|data| data.get_temp(context_selection_key(id)))
            .flatten();
        let range = remembered
            .or_else(|| {
                TextEditState::load(ui.ctx(), id).and_then(|state| state.cursor.char_range())
            })
            .filter(|range| !range.is_empty());

        let cut_response = ui.add_enabled(range.is_some(), egui::Button::new("Cut"));
        if cut_response.clicked() {
            if let Some(range) = range {
                cut(ui, id, text, range);
            }
            ui.close();
        }
        if ui
            .add_enabled(range.is_some(), egui::Button::new("Copy"))
            .clicked()
        {
            if let Some(range) = range {
                ui.ctx().copy_text(range.slice_str(text).to_owned());
            }
            ui.close();
        }
        if ui.button("Paste").clicked() {
            // Restore the remembered selection so the paste replaces it, then
            // give the field focus back for the clipboard event next frame.
            if let Some(range) = remembered {
                let mut state = TextEditState::load(ui.ctx(), id).unwrap_or_default();
                state.cursor.set_char_range(Some(range));
                state.store(ui.ctx(), id);
            }
            ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
            ui.ctx().memory_mut(|memory| memory.request_focus(id));
            ui.ctx().request_repaint();
            ui.close();
        }
        ui.separator();
        if ui
            .add_enabled(!text.is_empty(), egui::Button::new("Select all"))
            .clicked()
        {
            select_all(ui, id, text);
            ui.close();
        }
    });
}

fn cut(ui: &Ui, id: Id, text: &mut String, range: CCursorRange) {
    ui.ctx().copy_text(range.slice_str(text).to_owned());
    let [min, max] = range.sorted_cursors();
    let start = byte_index(text, min.index.into());
    let end = byte_index(text, max.index.into());
    text.replace_range(start..end, "");
    let mut state = TextEditState::load(ui.ctx(), id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(min.index))));
    state.store(ui.ctx(), id);
    ui.ctx().request_repaint();
}

fn select_all(ui: &Ui, id: Id, text: &str) {
    let mut state = TextEditState::load(ui.ctx(), id).unwrap_or_default();
    let end = CCursor::new(text.chars().count());
    state
        .cursor
        .set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
    state.store(ui.ctx(), id);
    ui.ctx().memory_mut(|memory| memory.request_focus(id));
    ui.ctx().request_repaint();
}

/// Byte offset of `char_index` in `text` (which counts characters, not bytes).
fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_index_counts_characters_not_bytes() {
        assert_eq!(byte_index("abc", 0), 0);
        assert_eq!(byte_index("abc", 2), 2);
        assert_eq!(byte_index("abc", 3), 3, "the end is the text length");
        assert_eq!(byte_index("abc", 9), 3, "past the end clamps to the length");
        assert_eq!(byte_index("aßc", 1), 1);
        assert_eq!(byte_index("aßc", 2), 3, "ß is two bytes");
    }

    #[test]
    fn cutting_removes_the_selection_and_places_the_caret() {
        egui::__run_test_ui(|ui| {
            let id = Id::unique("cut_test");
            let mut text = "abcdef".to_string();
            cut(
                ui,
                id,
                &mut text,
                CCursorRange::two(CCursor::new(1), CCursor::new(4)),
            );
            assert_eq!(text, "aef", "the selected characters are removed");
            let state = TextEditState::load(ui.ctx(), id).expect("the cursor state is stored");
            let range = state.cursor.char_range().expect("the caret is set");
            assert!(range.is_empty());
            assert_eq!(
                usize::from(range.primary.index),
                1,
                "the caret sits where the cut began"
            );
        });
    }

    #[test]
    fn select_all_covers_every_character() {
        egui::__run_test_ui(|ui| {
            let id = Id::unique("select_all_test");
            let text = "8/8/8/8/8/8/8/8 w - - 0 1";
            select_all(ui, id, text);
            let state = TextEditState::load(ui.ctx(), id).expect("the cursor state is stored");
            let range = state.cursor.char_range().expect("a selection is set");
            assert_eq!(usize::from(range.sorted_cursors()[0].index), 0);
            assert_eq!(
                usize::from(range.sorted_cursors()[1].index),
                text.chars().count(),
                "the whole text is selected"
            );
        });
    }

    /// egui collapses the selection on the right-button press; the menu opens
    /// on the release. The press frame must put the selection back.
    #[test]
    fn a_right_press_inside_the_selection_keeps_it() {
        use egui::{vec2, Event, Modifiers, PointerButton, Pos2, Rect};

        let ctx = egui::Context::default();
        let id = Id::unique("right_press_test");
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let field_rect = Rect::from_min_size(Pos2::new(20.0, 20.0), vec2(300.0, 28.0));
        let mut text = "8/8/8/8/8/8/8/8 w - - 0 1".to_string();

        // Frame 1: draw the field and select characters 2..5.
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.put(field_rect, egui::TextEdit::singleline(&mut text).id(id));
                });
            },
        )
        .drop_without_applying_deltas();
        let mut state = TextEditState::load(&ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(2), CCursor::new(5))));
        state.store(&ctx, id);

        // Frame 2: a secondary press inside the selection (characters 2..5
        // start a few pixels into the monospaced field).
        let pos = Pos2::new(field_rect.left() + 30.0, field_rect.center().y);
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button: PointerButton::Secondary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let before = selection_before(ui, id);
                    let response = ui.put(field_rect, egui::TextEdit::singleline(&mut text).id(id));
                    after_edit(ui, id, &mut text, before, &response);
                });
            },
        )
        .drop_without_applying_deltas();

        let state = TextEditState::load(&ctx, id).expect("the cursor state is stored");
        let range = state.cursor.char_range().expect("a selection is set");
        assert!(
            !range.is_empty(),
            "the right-click must not collapse the selection"
        );
        assert_eq!(usize::from(range.sorted_cursors()[0].index), 2);
        assert_eq!(usize::from(range.sorted_cursors()[1].index), 5);
    }

    /// Full mouse path: select text, right-click it, click Cut in the menu,
    /// and see the text change. This is the interaction the owner reported.
    #[test]
    fn right_click_then_cut_in_the_menu_removes_the_selection() {
        use egui::{vec2, Event, Modifiers, PointerButton, Pos2, Rect};

        let ctx = egui::Context::default();
        let id = Id::unique("menu_cut_test");
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let field_rect = Rect::from_min_size(Pos2::new(20.0, 20.0), vec2(300.0, 28.0));
        let mut text = "abcdef".to_string();

        let draw = |ui: &mut egui::Ui, text: &mut String| {
            egui::CentralPanel::default().show(ui, |ui| {
                let before = selection_before(ui, id);
                let response = ui.put(field_rect, egui::TextEdit::singleline(text).id(id));
                after_edit(ui, id, text, before, &response);
            });
        };
        let frame = |events: Vec<Event>, text: &mut String| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ui| draw(ui, text),
            )
            .drop_without_applying_deltas();
        };

        // Frame 1: draw the field, then select characters 1..4.
        frame(Vec::new(), &mut text);
        let mut state = TextEditState::load(&ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(1), CCursor::new(4))));
        state.store(&ctx, id);

        // Right-click inside the selection: press, then release.
        let pos = Pos2::new(field_rect.left() + 20.0, field_rect.center().y);
        let press = |pressed: bool| {
            vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button: PointerButton::Secondary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        frame(press(true), &mut text);
        frame(press(false), &mut text);

        // The popup is non-interactable on the frame it opens (egui's sizing
        // pass); move the pointer over the item so the next frame hit-tests it.
        let item = Pos2::new(pos.x + 20.0, pos.y + 12.0);
        frame(vec![Event::PointerMoved(item)], &mut text);
        frame(vec![Event::PointerMoved(item)], &mut text);

        // Debug: where is the popup layer?
        let layers = ctx.memory(|m| m.areas().visible_layer_ids());
        eprintln!("layers: {:?}", layers);
        let popup_id = Id::unique("menu_cut_test").with("popup");
        eprintln!(
            "popup response rect: {:?}",
            ctx.read_response(popup_id).map(|r| r.rect)
        );

        // The menu opens at the pointer; its first item is Cut.
        let click = |pressed: bool| {
            vec![
                Event::PointerMoved(item),
                Event::PointerButton {
                    pos: item,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        frame(click(true), &mut text);
        frame(click(false), &mut text);

        assert_eq!(text, "aef", "Cut must remove the selected characters");
    }

    /// The Paste item must ask the integration for the clipboard and give the
    /// field focus back, with the selection restored so the paste replaces it.
    #[test]
    fn right_click_then_paste_in_the_menu_requests_the_clipboard() {
        use egui::{vec2, Event, Modifiers, PointerButton, Pos2, Rect};

        let ctx = egui::Context::default();
        let id = Id::unique("menu_paste_test");
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let field_rect = Rect::from_min_size(Pos2::new(20.0, 20.0), vec2(300.0, 28.0));
        let mut text = "abcdef".to_string();

        let draw = |ui: &mut egui::Ui, text: &mut String| {
            egui::CentralPanel::default().show(ui, |ui| {
                let before = selection_before(ui, id);
                let response = ui.put(field_rect, egui::TextEdit::singleline(text).id(id));
                after_edit(ui, id, text, before, &response);
            });
        };
        let frame = |events: Vec<Event>, text: &mut String| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ui| draw(ui, text),
            )
            .drop_without_applying_deltas();
        };

        frame(Vec::new(), &mut text);
        let mut state = TextEditState::load(&ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(1), CCursor::new(4))));
        state.store(&ctx, id);

        let pos = Pos2::new(field_rect.left() + 20.0, field_rect.center().y);
        let secondary = |pressed: bool| {
            vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button: PointerButton::Secondary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        frame(secondary(true), &mut text);
        frame(secondary(false), &mut text);

        // Paste is the third item; the menu opens at the pointer.
        let item = Pos2::new(pos.x + 20.0, pos.y + 57.0);
        frame(vec![Event::PointerMoved(item)], &mut text);
        frame(vec![Event::PointerMoved(item)], &mut text);
        let click = |pressed: bool| {
            vec![
                Event::PointerMoved(item),
                Event::PointerButton {
                    pos: item,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        frame(click(true), &mut text);
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events: click(false),
                ..Default::default()
            },
            |ui| draw(ui, &mut text),
        );

        let requested = output
            .viewport_output
            .values()
            .flat_map(|viewport| viewport.commands.iter())
            .any(|command| matches!(command, ViewportCommand::RequestPaste));
        let state = TextEditState::load(&ctx, id).expect("the cursor state is stored");
        let range = state
            .cursor
            .char_range()
            .expect("the selection is restored");
        output.drop_without_applying_deltas();

        assert!(
            requested,
            "Paste must request the clipboard from the integration"
        );
        assert_eq!(
            (
                usize::from(range.sorted_cursors()[0].index),
                usize::from(range.sorted_cursors()[1].index)
            ),
            (1, 4),
            "the paste replaces the selection that was there when the menu opened"
        );
        assert!(
            ctx.memory(|memory| memory.has_focus(id)),
            "the field takes focus for the paste"
        );
    }
}

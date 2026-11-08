//! Engine options dialog: the UCI options an engine declared, each with a
//! control of the right kind. A change is reported at once as an
//! [`OptionsAction`], and the app sends it to the engine. The dialog holds
//! only the values it shows.
//!
//! The layout uses dashed section headings, labels above their fields, and a
//! two-column grid so an engine with a dozen options reads as a page instead
//! of a long list.

use std::collections::BTreeMap;

use eframe::egui::{
    self, Align, Align2, Button, ComboBox, DragValue, FontId, Layout, RichText, ScrollArea, Sense,
    Shape, Stroke, TextEdit, Ui, Vec2,
};

use crate::engine::protocol::{UciOption, UciOptionKind};
use crate::gui::theme;

/// Height of a text field, number field or button.
const FIELD_HEIGHT: f32 = 32.0;

/// Gap between the two columns of the grid.
const COLUMN_GAP: f32 = 24.0;

/// Gap below a grid row.
const ROW_GAP: f32 = 16.0;

/// What the dialog asked for this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionsAction {
    /// Nothing changed.
    None,
    /// Set an option to a new value (`check` as `true`/`false`).
    Set {
        /// Option name, as the engine declared it.
        name: String,
        /// The new value in UCI text form.
        value: String,
    },
    /// Press a button option.
    Press {
        /// Option name, as the engine declared it.
        name: String,
    },
    /// Set an option back to the engine's default, and forget the saved value.
    Reset {
        /// Option name, as the engine declared it.
        name: String,
        /// The engine's default, in UCI text form.
        value: String,
    },
    /// Set every option back to the engine's defaults, and forget the saved
    /// values.
    ResetAll,
    /// Close the dialog.
    Close,
}

/// The engine's default for an option, in UCI text form. A button has none.
pub fn default_value(option: &UciOption) -> String {
    match &option.kind {
        UciOptionKind::Check { default } => default.to_string(),
        UciOptionKind::Spin { default, .. } => default.to_string(),
        UciOptionKind::Combo { default, .. } => default.clone(),
        UciOptionKind::String { default } => default.clone(),
        UciOptionKind::Button => String::new(),
    }
}

/// The value an option shows: the saved value when there is one, else the
/// engine's default. A button has no value.
pub fn value_for(option: &UciOption, settings: &BTreeMap<String, String>) -> String {
    match settings.get(&option.name) {
        Some(saved) => saved.clone(),
        None => default_value(option),
    }
}

/// The group an option is drawn under. UCI has no categories, so the control
/// kind decides: the fields an engine owner changes most are splits into
/// search settings and feature switches, long strings get their own full-width
/// rows, and button options are actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Search,
    Features,
    Files,
    Actions,
}

impl Section {
    fn of(option: &UciOption) -> Self {
        match &option.kind {
            UciOptionKind::Spin { .. } | UciOptionKind::Combo { .. } => Self::Search,
            UciOptionKind::Check { .. } => Self::Features,
            UciOptionKind::String { .. } => Self::Files,
            UciOptionKind::Button => Self::Actions,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Search => "Search settings",
            Self::Features => "Features",
            Self::Files => "Files and paths",
            Self::Actions => "Actions",
        }
    }

    const ALL: [Self; 4] = [Self::Search, Self::Features, Self::Files, Self::Actions];
}

/// The options window for one engine slot.
pub struct OptionsDialog {
    /// Slot the engine is in (`0` = A, `1` = B).
    slot: usize,
    /// Window title, e.g. `Engine A options`.
    title: String,
    /// Engine name from the handshake, or the file name before it answers.
    engine_name: String,
    /// File name of the engine binary, shown under the engine name.
    binary_name: String,
    /// The options the engine declared.
    options: Vec<UciOption>,
    /// The values shown, by option name. Buttons have no entry.
    values: BTreeMap<String, String>,
}

impl OptionsDialog {
    /// A dialog for the options of the engine in `slot`, showing `settings`
    /// where the user has set a value.
    pub fn new(
        slot: usize,
        letter: char,
        engine_name: String,
        binary_name: String,
        options: Vec<UciOption>,
        settings: &BTreeMap<String, String>,
    ) -> Self {
        let values = options
            .iter()
            .filter(|option| !matches!(option.kind, UciOptionKind::Button))
            .map(|option| (option.name.clone(), value_for(option, settings)))
            .collect();
        Self {
            slot,
            title: format!("Engine {} options", letter),
            engine_name,
            binary_name,
            options,
            values,
        }
    }

    /// The slot this dialog is for.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Draw the window and report what the user did.
    pub fn show(&mut self, ctx: &egui::Context) -> OptionsAction {
        let mut action = OptionsAction::None;
        let mut open = true;
        let mut requested_close = false;

        let screen = ctx.content_rect();
        let max_height = (screen.height() * 0.9).max(420.0);
        let body_max = (max_height - 210.0).clamp(220.0, 620.0);

        egui::Window::new(self.title.as_str())
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .min_width(560.0)
            .max_width(920.0)
            .max_height(max_height)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                self.header(ui);
                ui.add_space(10.0);

                ScrollArea::vertical()
                    .id_salt(("engine_options_body", self.slot))
                    .auto_shrink([false, true])
                    .max_height(body_max)
                    .show(ui, |ui| {
                        if self.options.is_empty() {
                            ui.label(
                                RichText::new("This engine declares no options.")
                                    .color(theme::MUTED),
                            );
                            return;
                        }
                        action = self.sections(ui);
                    });

                ui.add_space(8.0);
                ui.separator();
                if self.footer(ui) {
                    requested_close = true;
                }
            });

        if !open || requested_close || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            return OptionsAction::Close;
        }
        action
    }

    /// The engine name and the slot it belongs to.
    fn header(&self, ui: &mut Ui) {
        ui.label(
            RichText::new(&self.engine_name)
                .size(22.0)
                .strong()
                .color(theme::TEXT),
        );
        ui.label(
            RichText::new(format!(
                "Engine {} · {}",
                char::from(b'A' + self.slot as u8),
                self.binary_name
            ))
            .size(14.0)
            .color(theme::MUTED),
        );
    }

    /// Every non-empty section, with its options laid out in the grid.
    fn sections(&mut self, ui: &mut Ui) -> OptionsAction {
        let mut action = OptionsAction::None;
        let field_width = ((ui.available_width() - COLUMN_GAP) / 2.0).max(160.0);

        // `Default` is useful even when an engine declares only settable
        // options, so the Actions section is not skipped just because it has
        // no button options.
        let has_settable = self
            .options
            .iter()
            .any(|option| !matches!(option.kind, UciOptionKind::Button));

        for section in Section::ALL {
            // Cloned so the options can be read while `show_field` mutates
            // the value map.
            let options: Vec<UciOption> = self
                .options
                .iter()
                .filter(|option| Section::of(option) == section)
                .cloned()
                .collect();
            let keep_empty_actions = section == Section::Actions && has_settable;
            if options.is_empty() && !keep_empty_actions {
                continue;
            }

            section_heading(ui, section.title());
            match section {
                Section::Files => {
                    for option in &options {
                        let change = self.show_field(ui, option, field_width * 2.0 + COLUMN_GAP);
                        if change != OptionsAction::None {
                            action = change;
                        }
                        ui.add_space(ROW_GAP);
                    }
                }
                Section::Actions => {
                    ui.horizontal_wrapped(|ui| {
                        for option in &options {
                            let change = self.show_field(ui, option, 140.0);
                            if change != OptionsAction::None {
                                action = change;
                            }
                            ui.add_space(8.0);
                        }
                        if has_settable {
                            let default = ui
                                .add(
                                    Button::new("Default").min_size(Vec2::new(140.0, FIELD_HEIGHT)),
                                )
                                .on_hover_text("Reset every option to the engine's defaults");
                            if default.clicked() {
                                self.reset_all_values();
                                action = OptionsAction::ResetAll;
                            }
                        }
                    });
                    ui.add_space(ROW_GAP);
                }
                Section::Search | Section::Features => {
                    for pair in options.chunks(2) {
                        ui.horizontal_top(|ui| {
                            for (index, option) in pair.iter().enumerate() {
                                if index > 0 {
                                    ui.add_space(COLUMN_GAP);
                                }
                                let change = ui
                                    .vertical(|ui| {
                                        ui.set_width(field_width);
                                        self.show_field(ui, option, field_width)
                                    })
                                    .inner;
                                if change != OptionsAction::None {
                                    action = change;
                                }
                            }
                        });
                        ui.add_space(ROW_GAP);
                    }
                }
            }
        }
        action
    }

    /// One labeled field. The reset link is shown only when the value differs
    /// from the engine default, so the page stays quiet.
    fn show_field(&mut self, ui: &mut Ui, option: &UciOption, width: f32) -> OptionsAction {
        let name = option.name.clone();
        if let UciOptionKind::Check { .. } = &option.kind {
            return self.show_check(ui, option, width);
        }

        // A button is its own label; every other kind gets a label above it.
        if !matches!(option.kind, UciOptionKind::Button) && self.label_row(ui, option, width) {
            let value = default_value(option);
            self.values.insert(name.clone(), value.clone());
            return OptionsAction::Reset { name, value };
        }

        match &option.kind {
            UciOptionKind::Check { .. } => unreachable!("checks are drawn as a checkbox row"),
            UciOptionKind::Spin { min, max, default } => {
                let mut number = self
                    .values
                    .get(&name)
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(*default);
                let response = ui.add_sized(
                    [width, FIELD_HEIGHT],
                    DragValue::new(&mut number)
                        .range(*min..=*max)
                        .speed(spin_speed(*min, *max)),
                );
                if response.changed() {
                    let value = number.to_string();
                    self.values.insert(name.clone(), value.clone());
                    return OptionsAction::Set { name, value };
                }
                OptionsAction::None
            }
            UciOptionKind::Combo {
                values: choices, ..
            } => {
                let current = self.values.get(&name).cloned().unwrap_or_default();
                let mut chosen: Option<String> = None;
                ComboBox::from_id_salt(("engine_option_combo", name.as_str()))
                    .width(width)
                    .selected_text(RichText::new(current.clone()).color(theme::TEXT))
                    .show_ui(ui, |ui| {
                        for choice in choices {
                            if ui.selectable_label(*choice == current, choice).clicked() {
                                chosen = Some(choice.clone());
                            }
                        }
                    });
                match chosen {
                    Some(value) => {
                        self.values.insert(name.clone(), value.clone());
                        OptionsAction::Set { name, value }
                    }
                    None => OptionsAction::None,
                }
            }
            UciOptionKind::Button => {
                if ui
                    .add(
                        Button::new(name.clone())
                            .min_size(Vec2::new(width.max(140.0), FIELD_HEIGHT)),
                    )
                    .clicked()
                {
                    OptionsAction::Press { name }
                } else {
                    OptionsAction::None
                }
            }
            UciOptionKind::String { .. } => {
                let text = self.values.entry(name.clone()).or_default();
                let id = ui.make_persistent_id(("engine_option", &name));
                let selection = crate::gui::text_field::selection_before(ui, id);
                let response =
                    ui.add_sized([width, FIELD_HEIGHT], TextEdit::singleline(text).id(id));
                crate::gui::text_field::after_edit(ui, id, text, selection, &response);
                // A text value is sent when the field is left (or Enter is pressed), not on every key.
                if response.lost_focus() {
                    let value = self.values.get(&name).cloned().unwrap_or_default();
                    return OptionsAction::Set { name, value };
                }
                OptionsAction::None
            }
        }
    }

    /// A boolean option as a checkbox row, with its reset link on the right.
    fn show_check(&mut self, ui: &mut Ui, option: &UciOption, width: f32) -> OptionsAction {
        let name = option.name.clone();
        let mut on = self.values.get(&name).is_some_and(|value| value == "true");
        let mut reset = false;
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.set_width(width);
            changed = ui
                .checkbox(&mut on, RichText::new(name.clone()).color(theme::TEXT))
                .changed();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                reset = self.reset_link(ui, option);
            });
        });
        if reset {
            let value = default_value(option);
            self.values.insert(name.clone(), value.clone());
            return OptionsAction::Reset { name, value };
        }
        if changed {
            let value = on.to_string();
            self.values.insert(name.clone(), value.clone());
            return OptionsAction::Set { name, value };
        }
        OptionsAction::None
    }

    /// The option's name above its control, with a reset link on the right
    /// when the current value is not the engine default.
    fn label_row(&self, ui: &mut Ui, option: &UciOption, width: f32) -> bool {
        let mut reset = false;
        ui.horizontal(|ui| {
            ui.set_width(width);
            ui.label(RichText::new(option.name.clone()).color(theme::TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                reset = self.reset_link(ui, option);
            });
        });
        reset
    }

    /// A quiet text button that is only there when the value is not the
    /// engine default.
    fn reset_link(&self, ui: &mut Ui, option: &UciOption) -> bool {
        if !self.is_changed(option) {
            return false;
        }
        ui.add(Button::new(RichText::new("Reset").size(13.0).color(theme::MUTED)).frame(false))
            .on_hover_text(format!(
                "Reset to the engine default: {}",
                default_value(option)
            ))
            .clicked()
    }

    /// Whether the shown value differs from the engine default.
    fn is_changed(&self, option: &UciOption) -> bool {
        self.values
            .get(&option.name)
            .is_some_and(|value| *value != default_value(option))
    }

    /// Put every settable field back to the engine default. The app sends the
    /// same values to the engine when it handles [`OptionsAction::ResetAll`].
    fn reset_all_values(&mut self) {
        for option in &self.options {
            if matches!(option.kind, UciOptionKind::Button) {
                continue;
            }
            self.values
                .insert(option.name.clone(), default_value(option));
        }
    }

    /// The bottom bar: a hint on the left, the Close button on the right.
    fn footer(&self, ui: &mut Ui) -> bool {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Changes apply to the engine immediately.")
                    .size(13.0)
                    .color(theme::MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close = ui
                    .add(
                        Button::new(RichText::new("Close").color(theme::TEXT))
                            .min_size(Vec2::new(104.0, 32.0)),
                    )
                    .clicked();
            });
        });
        close
    }
}

/// A drag speed that feels steady over the whole range instead of crawling
/// through a 1..1_000_000 spin box.
fn spin_speed(min: i64, max: i64) -> f64 {
    let span = (max - min).unsigned_abs();
    if span > 100_000 {
        1_000.0
    } else if span > 10_000 {
        100.0
    } else if span > 1_000 {
        10.0
    } else {
        1.0
    }
}

/// A dashed rule with its label in the middle. Shared with the tournament
/// dialog so both pages read alike.
pub(crate) fn section_heading(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::hover());
    let stroke = Stroke::new(1.0, theme::MUTED.gamma_multiply(0.55));
    let galley =
        ui.painter()
            .layout_no_wrap(title.to_owned(), FontId::proportional(15.0), theme::MUTED);
    let text_width = galley.size().x + 24.0;
    let center = rect.center();
    let y = center.y;
    ui.painter().extend(Shape::dashed_line(
        &[
            egui::pos2(rect.left(), y),
            egui::pos2(center.x - text_width / 2.0, y),
        ],
        stroke,
        5.0,
        4.0,
    ));
    ui.painter().extend(Shape::dashed_line(
        &[
            egui::pos2(center.x + text_width / 2.0, y),
            egui::pos2(rect.right(), y),
        ],
        stroke,
        5.0,
        4.0,
    ));
    ui.painter().galley(
        egui::pos2(
            center.x - galley.size().x / 2.0,
            center.y - galley.size().y / 2.0,
        ),
        galley,
        theme::MUTED,
    );
    ui.add_space(12.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(name: &str, kind: UciOptionKind) -> UciOption {
        UciOption {
            name: name.to_string(),
            kind,
        }
    }

    #[test]
    fn a_saved_value_overrides_the_default() {
        let hash = option(
            "Hash",
            UciOptionKind::Spin {
                default: 16,
                min: 1,
                max: 1024,
            },
        );
        let mut settings = BTreeMap::new();
        assert_eq!(value_for(&hash, &settings), "16");
        settings.insert("Hash".to_string(), "256".to_string());
        assert_eq!(value_for(&hash, &settings), "256");
    }

    #[test]
    fn default_value_ignores_the_saved_one() {
        let hash = option(
            "Hash",
            UciOptionKind::Spin {
                default: 16,
                min: 1,
                max: 1024,
            },
        );
        assert_eq!(default_value(&hash), "16");
    }

    #[test]
    fn each_kind_shows_its_default() {
        let none = BTreeMap::new();
        let check = option("Ponder", UciOptionKind::Check { default: true });
        assert_eq!(value_for(&check, &none), "true");
        let combo = option(
            "Style",
            UciOptionKind::Combo {
                default: "Normal".into(),
                values: vec!["Solid".into(), "Normal".into()],
            },
        );
        assert_eq!(value_for(&combo, &none), "Normal");
        let text = option(
            "SyzygyPath",
            UciOptionKind::String {
                default: "<empty>".into(),
            },
        );
        assert_eq!(value_for(&text, &none), "<empty>");
        let button = option("Clear Hash", UciOptionKind::Button);
        assert_eq!(value_for(&button, &none), "");
    }

    #[test]
    fn options_are_grouped_by_control_kind() {
        assert_eq!(
            Section::of(&option(
                "Hash",
                UciOptionKind::Spin {
                    default: 16,
                    min: 1,
                    max: 1024
                }
            )),
            Section::Search
        );
        assert_eq!(
            Section::of(&option("Ponder", UciOptionKind::Check { default: false })),
            Section::Features
        );
        assert_eq!(
            Section::of(&option(
                "SyzygyPath",
                UciOptionKind::String {
                    default: String::new()
                }
            )),
            Section::Files
        );
        assert_eq!(
            Section::of(&option("Clear Hash", UciOptionKind::Button)),
            Section::Actions
        );
    }

    #[test]
    fn default_resets_every_settable_field_to_the_engine_default() {
        let options = vec![
            option(
                "Hash",
                UciOptionKind::Spin {
                    default: 16,
                    min: 1,
                    max: 1024,
                },
            ),
            option("Ponder", UciOptionKind::Check { default: false }),
            option("Clear Hash", UciOptionKind::Button),
        ];
        let settings = BTreeMap::from([
            ("Hash".to_string(), "256".to_string()),
            ("Ponder".to_string(), "true".to_string()),
        ]);
        let mut dialog = OptionsDialog::new(
            0,
            'A',
            "Stockfish 19".to_string(),
            "stockfish".to_string(),
            options,
            &settings,
        );
        assert_eq!(dialog.values["Hash"], "256");
        assert_eq!(dialog.values["Ponder"], "true");

        dialog.reset_all_values();

        assert_eq!(dialog.values["Hash"], "16");
        assert_eq!(dialog.values["Ponder"], "false");
        assert!(!dialog.values.contains_key("Clear Hash"));
    }

    #[test]
    fn the_dialog_lays_out_every_control_kind() {
        let options = vec![
            option(
                "Hash",
                UciOptionKind::Spin {
                    default: 16,
                    min: 1,
                    max: 1024,
                },
            ),
            option(
                "Threads",
                UciOptionKind::Spin {
                    default: 8,
                    min: 1,
                    max: 1024,
                },
            ),
            option(
                "Style",
                UciOptionKind::Combo {
                    default: "Normal".into(),
                    values: vec!["Solid".into(), "Normal".into()],
                },
            ),
            option("Ponder", UciOptionKind::Check { default: false }),
            option("UCI_Chess960", UciOptionKind::Check { default: false }),
            option(
                "SyzygyPath",
                UciOptionKind::String {
                    default: "<empty>".into(),
                },
            ),
            option("Clear Hash", UciOptionKind::Button),
        ];
        let mut dialog = OptionsDialog::new(
            0,
            'A',
            "Stockfish 19".to_string(),
            "stockfish".to_string(),
            options,
            &BTreeMap::new(),
        );
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let _ = dialog.show(ui.ctx());
        });
        output.textures_delta.clear();
    }

    #[test]
    fn dialog_keeps_values_only_for_settable_options() {
        let options = vec![
            option("Ponder", UciOptionKind::Check { default: false }),
            option("Clear Hash", UciOptionKind::Button),
        ];
        let dialog = OptionsDialog::new(
            0,
            'A',
            "Stockfish 19".to_string(),
            "stockfish".to_string(),
            options,
            &BTreeMap::new(),
        );
        assert_eq!(dialog.values.len(), 1);
        assert!(dialog.values.contains_key("Ponder"));
        assert_eq!(dialog.title, "Engine A options");
        assert_eq!(dialog.engine_name, "Stockfish 19");
        assert_eq!(dialog.slot(), 0);
    }
}

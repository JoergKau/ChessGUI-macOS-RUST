//! Settings that survive a restart: the time control, whether engines play
//! book moves, the session-log switch, the UCI option values chosen for each
//! engine file, the tablebase path, the board theme, the piece set, the two
//! figurine switches, the window and panel sizes, and the last tournament
//! configuration.
//!
//! The file is JSON, kept under `~/Library/Application Support/SiliconBoard/`.
//! `SILICONBOARD_SETTINGS` overrides the path. A missing file means the
//! defaults. A file that cannot be read or parsed is reported, and the
//! defaults are used until the user changes something.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chess::clock::TimeControl;
use crate::tournament::TournamentConfig;

/// How many previously loaded engine files the pulldowns remember.
pub const ENGINE_HISTORY_LIMIT: usize = 12;

/// The tablebases switch is on unless a settings file says otherwise.
fn default_tablebases_in_engines() -> bool {
    true
}

/// Everything kept between runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The time control for play modes, or `None` for no clock.
    pub time_control: Option<TimeControl>,
    /// Whether engines in a play mode answer with opening-book moves.
    /// Off by default; it becomes relevant when a book is loaded.
    pub book_in_games: bool,
    /// The PolyGlot book the user loaded last (Engine → Load PolyGlot
    /// book…), restored at start-up. No book is loaded without it: there
    /// is no default book.
    pub book_path: Option<PathBuf>,
    /// The `SyzygyPath` value chosen in Engine → Load tablebases…: the
    /// tablebase folders, joined by the platform separator. Sent to every
    /// engine at start-up and while it is loaded. `None` when no tablebases
    /// are configured.
    pub tablebase_path: Option<String>,
    /// Whether the configured tablebases are used at all (Engine →
    /// Tablebases in engines). On by default, so a settings file from before
    /// the field keeps using a configured path.
    #[serde(default = "default_tablebases_in_engines")]
    pub tablebases_in_engines: bool,
    /// Whether the app writes the session log (`~/Library/Logs/SiliconBoard`). On by
    /// default; Engine → Session log switches it.
    pub session_log: bool,
    /// Option values per engine file (the path as text), by option name.
    pub engine_options: BTreeMap<String, BTreeMap<String, String>>,
    /// The engine file last loaded into each slot (A, B). Loaded again at start-up.
    pub engines: [Option<PathBuf>; 2],
    /// Engine files loaded before, newest first, for the engine pulldowns.
    /// Bounded by [`ENGINE_HISTORY_LIMIT`]; duplicates are removed.
    pub engine_history: Vec<PathBuf>,
    /// The window's inner size in points, restored at the next launch.
    /// A fresh install opens at the reference layout (1316 × 844).
    pub window_size: Option<WindowSize>,
    /// The evaluation and move-list panel sizes, restored at the next launch.
    /// A fresh install opens with the reference layout (right 637, bottom 190).
    pub panels: Option<PanelSizes>,
    /// Board image file name from `public/images/board/`, e.g. `blue.png`.
    pub board_theme: Option<String>,
    /// Piece set folder from `public/piece/`, e.g. `cburnett`.
    pub piece_set: Option<String>,
    /// Draw move-list piece letters as figurines (`♘f3`). On by default.
    pub figurines_move_list: bool,
    /// Draw engine PV piece letters as figurines. Off by default.
    pub figurines_eval: bool,
    /// Show the analysis best-move arrows on the board (View → Analysis
    /// arrows). On by default.
    pub analysis_arrows: bool,
    /// The last tournament configuration, so the dialog opens where the user
    /// left it.
    pub tournament: Option<TournamentConfig>,
}

/// The window's inner size in points.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSize {
    /// Width in points.
    pub width: f32,
    /// Height in points.
    pub height: f32,
}

/// The resizable panels around the board, in points. The board takes what is
/// left of the window.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelSizes {
    /// Width of the evaluation dashboard on the right.
    pub right_width: f32,
    /// Height of the move list at the bottom.
    pub bottom_height: f32,
}

impl Default for Settings {
    /// No clock, book play off (the menu item waits for a loaded book, which
    /// only the user loads — there is no default book), no saved engine options.
    /// A fresh install opens at the reference layout: a 1316 × 844 window with
    /// a 637-point right panel and a 190-point move list.
    fn default() -> Self {
        Self {
            time_control: None,
            book_in_games: false,
            book_path: None,
            tablebase_path: None,
            tablebases_in_engines: true,
            session_log: true,
            engine_options: BTreeMap::new(),
            engines: [None, None],
            engine_history: Vec::new(),
            window_size: Some(WindowSize {
                width: 1316.0,
                height: 844.0,
            }),
            panels: Some(PanelSizes {
                right_width: 637.0,
                bottom_height: 190.0,
            }),
            board_theme: None,
            piece_set: None,
            figurines_move_list: true,
            figurines_eval: false,
            analysis_arrows: true,
            tournament: None,
        }
    }
}

/// Why the settings could not be read or written.
#[derive(Debug)]
pub enum SettingsError {
    /// No home folder to put the settings in, and no override given.
    NoLocation,
    /// The file exists but could not be read.
    Read {
        /// The settings file.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// The file is not valid settings JSON.
    Parse {
        /// The settings file.
        path: PathBuf,
        /// Why it did not parse.
        source: serde_json::Error,
    },
    /// The file could not be written.
    Write {
        /// The settings file.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::NoLocation => f.write_str("no home folder for the settings file"),
            SettingsError::Read { path, source } => {
                write!(f, "could not read `{}`: {}", path.display(), source)
            }
            SettingsError::Parse { path, source } => {
                write!(f, "`{}` is not valid settings: {}", path.display(), source)
            }
            SettingsError::Write { path, source } => {
                write!(f, "could not write `{}`: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SettingsError::NoLocation => None,
            SettingsError::Read { source, .. } | SettingsError::Write { source, .. } => {
                Some(source)
            }
            SettingsError::Parse { source, .. } => Some(source),
        }
    }
}

/// Where the settings file lives: `SILICONBOARD_SETTINGS` if set, else
/// `~/Library/Application Support/SiliconBoard/settings.json`.
pub fn settings_path() -> Result<PathBuf, SettingsError> {
    if let Some(path) = std::env::var_os("SILICONBOARD_SETTINGS") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or(SettingsError::NoLocation)?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/SiliconBoard")
        .join("settings.json"))
}

/// Read the settings at `path`. A missing file gives the defaults.
///
/// This runs at start-up, before the window exists, because the window size is
/// one of the settings. The file is small, so one synchronous read is fine.
pub fn load_sync(path: &Path) -> Result<Settings, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|source| SettingsError::Parse {
            path: path.to_path_buf(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(source) => Err(SettingsError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write `settings` to `path`, creating the folder if needed. The app runs this
/// on the Tokio runtime while it is open, and synchronously at exit.
pub fn save_sync(path: &Path, settings: &Settings) -> Result<(), SettingsError> {
    let write_error = |source| SettingsError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(write_error)?;
    }
    // Write beside the file, sync, then rename over it. A crash during the
    // write leaves the previous settings in place instead of a truncated file.
    let tmp = path.with_extension("json.tmp");
    let text = to_json(settings);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()
    });
    written.map_err(write_error)?;
    std::fs::rename(&tmp, path).map_err(write_error)
}

/// Parse settings JSON. Fields that are missing take their defaults.
pub fn parse(text: &str) -> Result<Settings, serde_json::Error> {
    serde_json::from_str(text)
}

/// Settings as pretty-printed JSON.
pub fn to_json(settings: &Settings) -> String {
    // Serializing plain data cannot fail; the fallback keeps this total.
    serde_json::to_string_pretty(settings).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chess::clock::TimeControl;

    #[test]
    fn a_save_replaces_the_file_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let first = Settings {
            book_in_games: true,
            ..Settings::default()
        };
        save_sync(&path, &first).unwrap();
        save_sync(&path, &Settings::default()).unwrap();
        assert!(
            !load_sync(&path).unwrap().book_in_games,
            "the second save wins"
        );
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temporary file is left"
        );
    }

    #[test]
    fn settings_round_trip_through_json() {
        let mut settings = Settings {
            time_control: Some(TimeControl::fischer(300_000, 3_000)),
            book_in_games: false,
            ..Settings::default()
        };
        settings
            .engine_options
            .entry("/opt/sf/stockfish".into())
            .or_default()
            .insert("Hash".into(), "256".into());
        let text = to_json(&settings);
        assert_eq!(parse(&text).unwrap(), settings);
    }

    #[test]
    fn session_logging_is_on_by_default_and_round_trips() {
        let settings = Settings::default();
        assert!(
            settings.session_log,
            "session logging is on unless turned off"
        );
        assert!(parse(&to_json(&settings)).unwrap().session_log);
        assert!(!parse(r#"{"session_log":false}"#).unwrap().session_log);
    }

    #[test]
    fn analysis_arrows_are_on_by_default_and_round_trips() {
        assert!(
            Settings::default().analysis_arrows,
            "the best-move arrows are on unless turned off"
        );
        assert!(
            parse(&to_json(&Settings::default()))
                .unwrap()
                .analysis_arrows
        );
        assert!(
            !parse(r#"{"analysis_arrows":false}"#)
                .unwrap()
                .analysis_arrows
        );
        // A settings file from before the field keeps the default (on).
        assert!(parse("{}").unwrap().analysis_arrows);
    }

    #[test]
    fn the_window_size_round_trips() {
        let settings = Settings {
            window_size: Some(WindowSize {
                width: 1440.0,
                height: 900.0,
            }),
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.window_size, settings.window_size);
        assert_eq!(
            Settings::default().window_size,
            Some(WindowSize {
                width: 1316.0,
                height: 844.0
            }),
            "a fresh install opens at the reference window size"
        );
    }

    #[test]
    fn the_panel_sizes_round_trip() {
        let settings = Settings {
            panels: Some(PanelSizes {
                right_width: 540.0,
                bottom_height: 220.0,
            }),
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.panels, settings.panels);
        assert_eq!(
            Settings::default().panels,
            Some(PanelSizes {
                right_width: 637.0,
                bottom_height: 190.0
            }),
            "a fresh install opens at the reference panel sizes"
        );
    }

    #[test]
    fn the_piece_set_round_trips() {
        let settings = Settings {
            piece_set: Some("staunty".into()),
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.piece_set.as_deref(), Some("staunty"));
        assert_eq!(
            Settings::default().piece_set,
            None,
            "the default is cburnett"
        );
    }

    #[test]
    fn figurine_defaults_follow_the_move_list_and_not_the_evaluation() {
        let settings = Settings::default();
        assert!(
            settings.figurines_move_list,
            "the move list uses figurines by default"
        );
        assert!(
            !settings.figurines_eval,
            "the engine PV is plain SAN by default"
        );
        let restored = parse(&to_json(&settings)).unwrap();
        assert!(restored.figurines_move_list);
        assert!(!restored.figurines_eval);
    }

    #[test]
    fn the_board_theme_round_trips() {
        let settings = Settings {
            board_theme: Some("wood2.jpg".into()),
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.board_theme.as_deref(), Some("wood2.jpg"));
        assert_eq!(Settings::default().board_theme, None, "no theme chosen yet");
    }

    #[test]
    fn the_tournament_configuration_round_trips() {
        let config = crate::tournament::TournamentConfig {
            white: crate::tournament::EngineSpec::from_path(PathBuf::from("/opt/stockfish")),
            black: crate::tournament::EngineSpec::from_path(PathBuf::from("/opt/triumviratus")),
            games: 20,
            ..crate::tournament::TournamentConfig::default()
        };
        let settings = Settings {
            tournament: Some(config),
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.tournament, settings.tournament);
        assert_eq!(
            Settings::default().tournament,
            None,
            "no tournament saved yet"
        );
    }

    #[test]
    fn engine_files_are_remembered_per_slot() {
        let settings = Settings {
            engines: [Some(PathBuf::from("/opt/sf/stockfish")), None],
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(
            restored.engines[0],
            Some(PathBuf::from("/opt/sf/stockfish"))
        );
        assert_eq!(restored.engines[1], None);
    }

    #[test]
    fn the_tablebase_path_round_trips() {
        let settings = Settings {
            tablebase_path: Some("/tb/5:/tb/6".into()),
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.tablebase_path.as_deref(), Some("/tb/5:/tb/6"));
        assert_eq!(
            Settings::default().tablebase_path,
            None,
            "no tablebases by default"
        );
        assert!(
            restored.tablebases_in_engines,
            "a configured path is used by default"
        );

        let switched_off = Settings {
            tablebases_in_engines: false,
            ..settings
        };
        let restored = parse(&to_json(&switched_off)).unwrap();
        assert!(!restored.tablebases_in_engines, "the switch round-trips");
    }

    #[test]
    fn the_engine_history_round_trips() {
        let settings = Settings {
            engine_history: vec![
                PathBuf::from("/opt/pawnforge"),
                PathBuf::from("/opt/stockfish"),
            ],
            ..Settings::default()
        };
        let restored = parse(&to_json(&settings)).unwrap();
        assert_eq!(restored.engine_history, settings.engine_history);
        assert!(
            Settings::default().engine_history.is_empty(),
            "no history yet"
        );
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let settings = parse("{}").unwrap();
        assert_eq!(settings, Settings::default());
        assert!(
            !settings.book_in_games,
            "book play is off until a book is loaded"
        );
        assert!(
            settings.tablebases_in_engines,
            "a settings file from before the switch keeps using a configured path"
        );
    }

    #[test]
    fn garbage_is_a_parse_error_not_defaults() {
        assert!(parse("not json at all").is_err());
    }

    #[test]
    fn a_missing_file_loads_as_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_sync(&dir.path().join("absent.json")).unwrap();
        assert_eq!(loaded, Settings::default());
    }

    /// A brand-new user gets the reference layout the owner uses: a
    /// 1316 × 844 window with a 637-point right panel and a 190-point move
    /// list. Everything else is the documented default (no clock, no book,
    /// A/B empty, figurines in the move list only).
    #[test]
    fn a_fresh_install_opens_at_the_reference_layout() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = load_sync(&dir.path().join("absent.json")).unwrap();
        assert_eq!(
            fresh.window_size,
            Some(WindowSize {
                width: 1316.0,
                height: 844.0
            })
        );
        assert_eq!(
            fresh.panels,
            Some(PanelSizes {
                right_width: 637.0,
                bottom_height: 190.0
            })
        );
        assert_eq!(fresh.engines, [None, None], "no engine is preloaded");
        assert!(fresh.engine_history.is_empty());
        assert!(fresh.figurines_move_list);
        assert!(!fresh.figurines_eval);
    }

    #[test]
    fn save_then_load_restores_the_settings_and_creates_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");
        let settings = Settings {
            time_control: Some(TimeControl::sudden_death(60_000)),
            ..Settings::default()
        };
        save_sync(&path, &settings).unwrap();
        assert_eq!(load_sync(&path).unwrap(), settings);
    }

    #[test]
    fn an_unreadable_file_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ broken").unwrap();
        match load_sync(&path) {
            Err(SettingsError::Parse { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("expected a parse error, got {:?}", other),
        }
    }
}

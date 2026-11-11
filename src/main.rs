//! SiliconBoard — entry point.
//!
//! Boots eframe on wgpu/Metal, creates the Tokio runtime that serves engines
//! and background I/O, and hands both to [`app::SiliconBoardApp`]. All the
//! behaviour lives in the modules below.

use eframe::egui;

mod app;
mod assets;
mod chess;
mod engine;
mod gui;
mod logging;
#[cfg(target_os = "macos")]
mod macos_about;
mod settings;
mod tournament;

use app::SiliconBoardApp;

/// Application identifier for the eframe window.
const APP_NAME: &str = "SiliconBoard";

/// Initial window size (board, evaluation dashboard and move list).
const INITIAL_WINDOW_SIZE: [f32; 2] = [1280.0, 800.0];

/// Smallest board edge (in points) that the window may leave visible. This is
/// the size the board has in the user's saved layout, so shrinking the window
/// below it would make the board visibly smaller than the configured minimum.
const MIN_BOARD_SIDE: f32 = 320.0;

/// Space used by the separator between the board area and the right panel.
const PANEL_GAP: f32 = 8.0;

/// Width of the right panel before a panel layout has been saved.
const DEFAULT_RIGHT_WIDTH: f32 = 520.0;

/// Smallest window height, kept for the top bar, board and both engine cards.
const MIN_WINDOW_HEIGHT: f32 = 640.0;

/// Never allow the window to become narrower than this, even with a small
/// right panel.
const ABSOLUTE_MIN_WINDOW_WIDTH: f32 = 960.0;

/// The smallest inner window size that still leaves a usable board next to the
/// saved right panel. The board takes whatever the right panel leaves, so the
/// minimum width is the saved panel width plus the smallest board plus the
/// panel separator.
fn min_inner_size(settings: &settings::Settings) -> [f32; 2] {
    let right_width = settings
        .panels
        .map_or(DEFAULT_RIGHT_WIDTH, |panels| panels.right_width);
    let width = (right_width + MIN_BOARD_SIDE + PANEL_GAP).max(ABSOLUTE_MIN_WINDOW_WIDTH);
    [width, MIN_WINDOW_HEIGHT]
}

/// The application icon for the window. It is compiled in and decoded once.
fn app_icon() -> Option<egui::IconData> {
    let image = image::load_from_memory(include_bytes!("../assets/icon/siliconboard-512.png"))
        .ok()?
        .to_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The settings are read before the logger and the window, because both the
    // session-log switch and the window size are settings. The file is small,
    // so this one read is done here.
    let settings_file = settings::settings_path().ok();
    let (settings, settings_error) = match &settings_file {
        Some(path) => match settings::load_sync(path) {
            Ok(settings) => (settings, None),
            Err(err) => (settings::Settings::default(), Some(err)),
        },
        None => (settings::Settings::default(), None),
    };

    let log_file = logging::init(settings.session_log);

    // The About panel shows the project credits and link (macOS only).
    #[cfg(target_os = "macos")]
    macos_about::install();

    log::info!(
        "SiliconBoard {} starting on {} {} — booting eframe on wgpu/Metal",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Some(err) = settings_error {
        log::warn!("{err}");
    }
    if let Some(folder) = settings_file.as_deref().and_then(|path| path.parent()) {
        engine::registry::configure(folder.join("engines.txt"));
        for message in engine::registry::sweep_configured() {
            log::warn!("{message}");
        }
    }
    if let Some(path) = &log_file {
        if logging::file_enabled() {
            log::info!("session log: {}", path.display());
        }
    } else {
        log::warn!("session log file unavailable; logging to the console only");
    }

    // The runtime only does pipe I/O for the engines and short background
    // jobs, never the search itself (the engines are OS processes with their
    // own cores). Each engine spawns a reader, writer, monitor, stderr and
    // supervisor task; with two fast engines a single busy reader could
    // otherwise occupy one of two workers and delay the other engine's
    // `stop`/`position`/`go`. Four workers leave headroom for that without
    // taking meaningful CPU from the searches.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("siliconboard-io")
        .build()?;

    let min_size = min_inner_size(&settings);
    let window_size = settings
        .window_size
        .map(|size| [size.width.max(min_size[0]), size.height.max(min_size[1])])
        .unwrap_or(INITIAL_WINDOW_SIZE);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(window_size)
        .with_min_inner_size(min_size);
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            gui::theme::apply(&cc.egui_ctx);
            if let Some(path) = assets::chess_font_path() {
                if let Err(err) = gui::theme::install_chess_font(&cc.egui_ctx, &path) {
                    log::warn!("chess figurine font not loaded: {}", err);
                }
            }
            Ok(Box::new(SiliconBoardApp::new(
                runtime,
                cc.egui_ctx.clone(),
                settings,
                settings_file,
            )))
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::PanelSizes;

    #[test]
    fn the_minimum_window_keeps_the_saved_board_size() {
        let settings = settings::Settings {
            panels: Some(PanelSizes {
                right_width: 917.0,
                bottom_height: 465.0,
            }),
            ..Default::default()
        };
        let [width, height] = min_inner_size(&settings);
        assert_eq!(height, MIN_WINDOW_HEIGHT);
        // 917 (right panel) + 320 (board) + 8 (separator) = 1245.
        assert!(
            (width - 1245.0).abs() < 0.01,
            "the saved layout must allow a 320-point board, got {width}"
        );
        assert!(width - settings.panels.unwrap().right_width - PANEL_GAP >= MIN_BOARD_SIDE);
    }

    #[test]
    fn the_application_icon_is_a_square_image() {
        let icon = app_icon().expect("the bundled icon must decode");
        assert_eq!(icon.width, icon.height);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }

    #[test]
    fn the_default_minimum_never_goes_below_the_old_floor() {
        let [width, _] = min_inner_size(&settings::Settings::default());
        assert!(
            width >= ABSOLUTE_MIN_WINDOW_WIDTH,
            "a small right panel must not make the window narrower than the old floor"
        );
    }
}

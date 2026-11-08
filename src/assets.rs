//! SiliconBoard asset pipeline — Phase 1 BIS edition.
//!
//! This module owns every GPU texture the renderer ever touches. As of the
//! Phase 1 BIS refactor it is responsible for:
//!
//! 1. The board image (`public/images/board/<theme>` — the selected theme,
//!    decoded once at load time; the board picker comes later).
//! 2. The twelve cburnett chess-piece sprites (`public/piece/cburnett/<w|b><K|Q|R|B|N|P>.svg`)
//!    which we **rasterise at load time** through the `usvg`/`resvg`/`tiny-skia`
//!    stack. That gives us vector-sharpness at any board scale on Apple
//!    Retina displays without paying a per-frame SVG-decoding cost.
//!
//! [`Assets`] ties the two together; [`BoardTexture`] and [`PieceTextures`]
//! stay independently cloneable so the widgets can hold their own borrow.

use crate::chess::{PieceColor, PieceKind};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

/// Width × height (in pixels) of every rasterised piece sprite.
///
/// 512 is generously above what any sane chess-board square will ever
/// reach on screen (a 4K Apple display squeezed into an 8×8 grid is
/// already in the 300-px range). Bigger rasterisations cost memory; smaller
/// risk looking soft.
pub const PIECE_RASTER_SIZE: u32 = 512;

/// Path of the board image used when no theme has been chosen yet.
pub const DEFAULT_BOARD_IMAGE: &str = "blue.png";

/// Piece set used when none has been chosen yet.
pub const DEFAULT_PIECE_SET: &str = "cburnett";

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// Compound identifier for a chess-piece sprite. Pairs a [`PieceColor`]
/// (white/black) with a [`PieceKind`] (K/Q/R/B/N/P) so we can map cleanly
/// to filenames like `wK.svg` / `bP.svg`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PieceId {
    pub color: PieceColor,
    pub kind: PieceKind,
}

impl PieceId {
    /// Returns the canonical on-disk filename for the piece, e.g. `wK.svg`
    /// (white king) or `bP.svg` (black pawn).
    pub const fn filename(&self) -> &'static str {
        match (self.color, self.kind) {
            (PieceColor::White, PieceKind::King) => "wK.svg",
            (PieceColor::White, PieceKind::Queen) => "wQ.svg",
            (PieceColor::White, PieceKind::Rook) => "wR.svg",
            (PieceColor::White, PieceKind::Bishop) => "wB.svg",
            (PieceColor::White, PieceKind::Knight) => "wN.svg",
            (PieceColor::White, PieceKind::Pawn) => "wP.svg",
            (PieceColor::Black, PieceKind::King) => "bK.svg",
            (PieceColor::Black, PieceKind::Queen) => "bQ.svg",
            (PieceColor::Black, PieceKind::Rook) => "bR.svg",
            (PieceColor::Black, PieceKind::Bishop) => "bB.svg",
            (PieceColor::Black, PieceKind::Knight) => "bN.svg",
            (PieceColor::Black, PieceKind::Pawn) => "bP.svg",
        }
    }
}

/// GPU texture for the chess board image. Wrapped around a single
/// [`TextureHandle`] owned by egui/Metal.
#[derive(Clone)]
pub struct BoardTexture {
    handle: TextureHandle,
}

impl BoardTexture {
    /// Reference to the underlying egui texture handle.
    pub fn handle(&self) -> &TextureHandle {
        &self.handle
    }

    /// Wrap an already-uploaded texture, used when the user picks a theme.
    pub fn from_handle(handle: TextureHandle) -> Self {
        Self { handle }
    }
}

/// One selectable board image from `public/images/board/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardTheme {
    /// File name, e.g. `blue.png`. This is what the settings file stores.
    pub file: String,
    /// Human label, e.g. `Blue marble`.
    pub label: String,
}

/// One selectable piece set from `public/piece/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceSet {
    /// Folder name, e.g. `cburnett`. This is what the settings file stores.
    pub name: String,
    /// Human label, e.g. `Kiwen suwi`.
    pub label: String,
}

/// GPU textures for all twelve pieces. Cheap to clone because the inner
/// [`TextureHandle`]'s storage on the GPU is held via an `Arc`.
#[derive(Clone)]
pub struct PieceTextures {
    /// Inner cache: `PieceId → egui::TextureHandle`.
    textures: HashMap<PieceId, TextureHandle>,
}

impl PieceTextures {
    /// Look up a piece texture by [`PieceId`].
    pub fn get(&self, id: PieceId) -> Option<&TextureHandle> {
        self.textures.get(&id)
    }
}

/// Bundle of every asset SiliconBoard needs at startup. Returned by
/// [`Assets::load`] and stored on the application root. Cloning is cheap
/// (just bumps a few reference counts on egui's GPU side).
#[derive(Clone)]
pub struct Assets {
    /// The single board image used by the current session.
    pub board: BoardTexture,
    /// All twelve piece sprites, rasterised from SVG.
    pub pieces: PieceTextures,
}

impl Assets {
    /// Discover the workspace `public/` root and load every asset with the
    /// default board theme and piece set. Used by the tests and the timing
    /// harness; the app uses [`Self::load_with`].
    ///
    /// # Errors
    /// Returns an [`AssetError`] if the asset directory cannot be located,
    /// if the board image is missing or unreadable, or if any of the twelve
    /// cburnett SVGs fails to parse or rasterise.
    #[cfg(test)]
    pub fn load(ctx: &Context) -> Result<Self, AssetError> {
        Self::load_with(ctx, None, None)
    }

    /// Like [`Self::load`], with the board theme and piece set chosen in the
    /// settings. A missing theme or piece set falls back to the defaults.
    pub fn load_with(
        ctx: &Context,
        board: Option<&str>,
        pieces: Option<&str>,
    ) -> Result<Self, AssetError> {
        let asset_root = resolve_asset_root();
        Self::load_from_with(ctx, &asset_root, board, pieces)
    }

    /// Load assets from an explicit directory with the given board and pieces.
    pub fn load_from_with(
        ctx: &Context,
        asset_root: &Path,
        board: Option<&str>,
        pieces: Option<&str>,
    ) -> Result<Self, AssetError> {
        let board_dir = asset_root.join("images").join("board");
        let requested = board
            .filter(|file| is_board_image(file))
            .unwrap_or(DEFAULT_BOARD_IMAGE);

        // (1) Board texture, falling back to the default if a saved theme
        // has gone missing or is not decodable.
        let board = match load_board(ctx, &board_dir.join(requested)) {
            Ok(board) => board,
            Err(err) if requested != DEFAULT_BOARD_IMAGE => {
                log::warn!(
                    "board theme `{}` could not be loaded ({}); using {}",
                    requested,
                    err,
                    DEFAULT_BOARD_IMAGE
                );
                load_board(ctx, &board_dir.join(DEFAULT_BOARD_IMAGE))?
            }
            Err(err) => return Err(err),
        };

        // (2) Piece textures, with the same fallback.
        let piece_root = asset_root.join("piece");
        let requested_set = pieces.filter(|name| piece_set_exists(&piece_root.join(name)));
        let requested_set = requested_set.unwrap_or(DEFAULT_PIECE_SET);
        let pieces = match load_pieces(ctx, &piece_root.join(requested_set), PIECE_RASTER_SIZE) {
            Ok(pieces) => pieces,
            Err(err) if requested_set != DEFAULT_PIECE_SET => {
                log::warn!(
                    "piece set `{}` could not be loaded ({}); using {}",
                    requested_set,
                    err,
                    DEFAULT_PIECE_SET
                );
                load_pieces(ctx, &piece_root.join(DEFAULT_PIECE_SET), PIECE_RASTER_SIZE)?
            }
            Err(err) => return Err(err),
        };

        Ok(Self { board, pieces })
    }
}

/// List the board images in the workspace `public/images/board/` folder.
pub fn board_themes() -> Vec<BoardTheme> {
    board_themes_from(&resolve_asset_root())
}

/// List the board images in `asset_root/images/board/`. Thumbnails and the
/// horsey overlay variants are excluded; the list is sorted by label.
pub fn board_themes_from(asset_root: &Path) -> Vec<BoardTheme> {
    let dir = asset_root.join("images").join("board");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut themes: Vec<BoardTheme> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let file = path.file_name()?.to_str()?.to_string();
            if !is_board_image(&file) {
                return None;
            }
            let label = board_label(&file);
            Some(BoardTheme { file, label })
        })
        .collect();
    themes.sort_by_key(|theme| theme.label.to_lowercase());
    themes
}

/// Whether `file` is a plain board image we can show: PNG or JPEG, and not one
/// of the thumbnails or overlay variants that live in the same folder.
fn is_board_image(file: &str) -> bool {
    let lower = file.to_lowercase();
    let supported = lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg");
    let overlay = [
        ".thumbnail.",
        ".orig.",
        ".last-move.",
        ".move-dest.",
        ".selected.",
        ".current-premove.",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    supported && !overlay
}

/// Display label for a board file: `blue-marble.jpg` becomes `Blue marble`.
fn board_label(file: &str) -> String {
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let spaced = stem.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// Path of a board theme by file name. `None` for a name with a path
/// component or an unsupported extension, so a settings value cannot escape
/// the board folder.
pub fn board_theme_path(file: &str) -> Option<PathBuf> {
    let name = Path::new(file);
    if name.components().count() != 1 || !is_board_image(file) {
        return None;
    }
    Some(resolve_asset_root().join("images").join("board").join(name))
}

/// Path of the bundled chess figurine font, if it is present.
pub fn chess_font_path() -> Option<PathBuf> {
    let path = resolve_asset_root().join("font").join("lichess-chess.ttf");
    path.is_file().then_some(path)
}

/// Path of the ECO opening-name table, if it is present.
pub fn opening_data_path() -> Option<PathBuf> {
    let path = resolve_asset_root()
        .join("data")
        .join("openings")
        .join("openings.tsv");
    path.is_file().then_some(path)
}

/// Path of a PGN file in the project's `PGN/` folder, next to the resolved
/// `public/` tree (e.g. the start-up fixture `WorldChamp1972.pgn`): from the
/// repository it is `<repo>/siliconboard/PGN/…`, from a bundle it is
/// `Contents/Resources/PGN/…` (the packager bundles the fixture there). Both
/// are one folder above the resolved asset root. `None` when the file is not
/// there, so the app simply starts without the start-up game.
pub fn project_pgn_path(file: &str) -> Option<PathBuf> {
    let name = Path::new(file);
    if name.components().count() != 1
        || name
            .extension()
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("pgn"))
    {
        return None;
    }
    project_pgn_path_from(&resolve_asset_root(), file)
}

/// Pure version of [`project_pgn_path`]: `file` is looked up in the `PGN/`
/// folder next to the given asset root.
fn project_pgn_path_from(asset_root: &Path, file: &str) -> Option<PathBuf> {
    let path = asset_root.parent()?.join("PGN").join(file);
    path.is_file().then_some(path)
}

/// List the piece sets in the workspace `public/piece/` folder.
pub fn piece_sets() -> Vec<PieceSet> {
    piece_sets_from(&resolve_asset_root())
}

/// List the complete piece sets in `asset_root/piece/`. A set is offered only
/// when all twelve expected SVGs are present; the list is sorted by label.
pub fn piece_sets_from(asset_root: &Path) -> Vec<PieceSet> {
    let dir = asset_root.join("piece");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sets: Vec<PieceSet> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() || !piece_set_exists(&path) {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            let label = set_label(&name);
            Some(PieceSet { name, label })
        })
        .collect();
    sets.sort_by_key(|set| set.label.to_lowercase());
    sets
}

/// Path of a piece set by folder name. `None` for a name with a path
/// component or a folder that does not hold all twelve pieces, so a settings
/// value cannot escape `public/piece`.
pub fn piece_set_path(set: &str) -> Option<PathBuf> {
    let name = Path::new(set);
    if name.components().count() != 1 {
        return None;
    }
    let path = resolve_asset_root().join("piece").join(name);
    piece_set_exists(&path).then_some(path)
}

/// Whether `dir` holds all twelve `{w,b}{K,Q,R,B,N,P}.svg` files.
fn piece_set_exists(dir: &Path) -> bool {
    dir.is_dir()
        && [PieceColor::White, PieceColor::Black]
            .into_iter()
            .all(|color| {
                [
                    PieceKind::King,
                    PieceKind::Queen,
                    PieceKind::Rook,
                    PieceKind::Bishop,
                    PieceKind::Knight,
                    PieceKind::Pawn,
                ]
                .into_iter()
                .all(|kind| dir.join(PieceId { color, kind }.filename()).is_file())
            })
}

/// Display label for a piece set folder: `kiwen-suwi` becomes `Kiwen suwi`.
fn set_label(name: &str) -> String {
    let spaced = name.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// Rasterise all twelve SVGs of a piece set. Runs on a blocking thread when
/// the user switches sets; the caller uploads the images afterwards.
pub fn rasterise_piece_set(dir: &Path) -> Result<Vec<(PieceId, ColorImage)>, AssetError> {
    let mut images = Vec::with_capacity(12);
    for &color in &[PieceColor::White, PieceColor::Black] {
        for &kind in &[
            PieceKind::King,
            PieceKind::Queen,
            PieceKind::Rook,
            PieceKind::Bishop,
            PieceKind::Knight,
            PieceKind::Pawn,
        ] {
            let id = PieceId { color, kind };
            let image = rasterise_piece_svg(&dir.join(id.filename()), PIECE_RASTER_SIZE)?;
            images.push((id, image));
        }
    }
    Ok(images)
}

/// Upload twelve rasterised piece images to the GPU.
pub fn upload_pieces(ctx: &Context, images: Vec<(PieceId, ColorImage)>) -> PieceTextures {
    let mut textures = HashMap::with_capacity(images.len());
    for (id, image) in images {
        let handle = ctx.load_texture(id.filename(), image, TextureOptions::LINEAR);
        textures.insert(id, handle);
    }
    PieceTextures { textures }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal loaders
// ─────────────────────────────────────────────────────────────────────────────

/// Read and decode a board image to RGBA8 without uploading it. Runs on a
/// blocking thread when the user switches themes.
pub fn decode_board(path: &Path) -> Result<ColorImage, AssetError> {
    let bytes = std::fs::read(path).map_err(|source| AssetError::OpenRead {
        path: path.to_path_buf(),
        source,
    })?;
    let decoded = image::load_from_memory(&bytes)
        .map_err(|source| AssetError::Decode {
            path: path.to_path_buf(),
            source,
        })?
        .to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err(AssetError::Empty {
            path: path.to_path_buf(),
        });
    }
    Ok(ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        decoded.as_raw(),
    ))
}

/// Load one board image and upload it to the GPU.
fn load_board(ctx: &Context, path: &Path) -> Result<BoardTexture, AssetError> {
    let image = decode_board(path)?;
    let [width, height] = image.size;
    let handle = ctx.load_texture(
        path.file_name().and_then(|s| s.to_str()).unwrap_or("board"),
        image,
        TextureOptions::LINEAR,
    );

    log::debug!(
        "Loaded board texture {:?} ({}x{} px) into egui atlas id = {:?}",
        path.file_name(),
        width,
        height,
        handle.id(),
    );

    Ok(BoardTexture { handle })
}

/// Load all twelve pieces from `piece_root` (a directory containing
/// `wK.svg`, `wQ.svg`, …, `bP.svg`). Each SVG is parsed with `usvg`,
/// rasterised to a fixed `target_px` × `target_px` RGBA8 buffer on a
/// `tiny_skia::Pixmap`, and uploaded to the GPU with `TextureOptions::LINEAR`.
fn load_pieces(
    ctx: &Context,
    piece_root: &Path,
    target_px: u32,
) -> Result<PieceTextures, AssetError> {
    let mut textures = HashMap::with_capacity(12);

    for &color in &[PieceColor::White, PieceColor::Black] {
        for &kind in &[
            PieceKind::King,
            PieceKind::Queen,
            PieceKind::Rook,
            PieceKind::Bishop,
            PieceKind::Knight,
            PieceKind::Pawn,
        ] {
            let id = PieceId { color, kind };
            let path = piece_root.join(id.filename());
            let handle = load_piece_svg(ctx, &path, target_px)?;
            textures.insert(id, handle);
        }
    }

    Ok(PieceTextures { textures })
}

/// Load a single piece SVG, rasterise it, and upload it to the GPU.
fn load_piece_svg(ctx: &Context, path: &Path, target_px: u32) -> Result<TextureHandle, AssetError> {
    let color_image = rasterise_piece_svg(path, target_px)?;
    let handle = ctx.load_texture(
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("piece"),
        color_image,
        TextureOptions::LINEAR,
    );
    Ok(handle)
}

/// Rasterise one piece SVG to a `target_px × target_px` RGBA8 image.
///
/// Steps:
/// 1. Read the SVG file.
/// 2. Parse with `usvg::Tree::from_data(...)`.
/// 3. Compute the scale factor needed to map the source viewBox onto a
///    `target_px × target_px` pixmap.
/// 4. Allocate a `tiny_skia::Pixmap` and ask `resvg` to render into it.
/// 5. Pull the RGBA8 buffer back via `take_demultiplied()` (egui wants
///    *un*premultiplied alpha) and wrap it in a `ColorImage`.
fn rasterise_piece_svg(path: &Path, target_px: u32) -> Result<ColorImage, AssetError> {
    let bytes = std::fs::read(path).map_err(|source| AssetError::OpenRead {
        path: path.to_path_buf(),
        source,
    })?;

    let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).map_err(|source| {
        AssetError::SvgParse {
            path: path.to_path_buf(),
            source,
        }
    })?;

    let src_size = tree.size();
    let src_w = src_size.width().max(1.0);
    let src_h = src_size.height().max(1.0);

    // Pick a uniform scale so the SVG fits exactly into the target square.
    let scale = (target_px as f32 / src_w).min(target_px as f32 / src_h);

    let mut pixmap = tiny_skia::Pixmap::new(target_px, target_px).ok_or(AssetError::Alloc {
        path: path.to_path_buf(),
        width: target_px,
        height: target_px,
    })?;

    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // egui/ColorImage wants *non*-premultiplied RGBA8.
    let data = pixmap.take_demultiplied();
    if data.len() != (target_px as usize) * (target_px as usize) * 4 {
        return Err(AssetError::ChannelMismatch {
            path: path.to_path_buf(),
            expected_bytes: (target_px as usize) * (target_px as usize) * 4,
            actual_bytes: data.len(),
        });
    }

    log::debug!(
        "Rasterised piece {:?} (src {}x{} → {}x{} px, {} bytes)",
        path.file_name(),
        src_w,
        src_h,
        target_px,
        target_px,
        data.len(),
    );

    Ok(ColorImage::from_rgba_unmultiplied(
        [target_px as usize, target_px as usize],
        &data,
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Asset root resolution — Phase 1 BIS flavour
//
// We locate the workspace-level `public/` directory by walking up from the
// executable's path. `cargo run` lands at:
//
//   <repo>/siliconboard/target/{release,debug}/siliconboard
//   parent → target/{release,debug} → target → siliconboard → Chess-GUI/
//
// Bundling a `.app` later will need a Resources fallback; for v0.1 the
// walker is sufficient.
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum number of parent directories to walk up while hunting for
/// `public/`. Six is generous — the exe lives four levels below the repo
/// root in `target/{release,debug}/siliconboard`.
const MAX_PARENT_WALK: usize = 8;

fn resolve_asset_root() -> PathBuf {
    let env_override = std::env::var("SILICONBOARD_PUBLIC_ROOT").ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_asset_root_from(env_override.as_deref(), exe_dir.as_deref())
}

/// Pure version of [`resolve_asset_root`]: the env override is tried first,
/// then the walk up from `exe_dir`, then `./public`.
fn resolve_asset_root_from(env_override: Option<&str>, exe_dir: Option<&Path>) -> PathBuf {
    // (1) Hard override via environment variable.
    if let Some(custom) = env_override {
        let p = PathBuf::from(custom);
        if looks_like_asset_root(&p) {
            return p;
        }
        log::warn!(
            "SILICONBOARD_PUBLIC_ROOT={:?} does not look like a SiliconBoard public/ tree, ignoring",
            p
        );
    }

    // (2) A macOS .app bundle: exe is Contents/MacOS/<binary>, assets live in
    // Contents/Resources (either as Resources/public or directly in Resources).
    if let Some(exe_dir) = exe_dir {
        if let Some(contents) = exe_dir.parent() {
            let resources = contents.join("Resources");
            for candidate in [resources.join("public"), resources] {
                if looks_like_asset_root(&candidate) {
                    return candidate;
                }
            }
        }
    }

    // (3) Walk up from the executable's folder looking for `public/`.
    if let Some(start) = exe_dir {
        let mut cursor: Option<&Path> = Some(start);
        let mut steps = 0usize;
        while let Some(candidate) = cursor {
            if steps > MAX_PARENT_WALK {
                break;
            }
            let proposed = candidate.join("public");
            if looks_like_asset_root(&proposed) {
                return proposed;
            }
            cursor = candidate.parent();
            steps += 1;
        }
    }

    // (4) Fallback — current working directory.
    PathBuf::from("public")
}

/// Returns `true` if `dir` looks like the canonical `public/` asset tree
/// (i.e. it contains both `images/board/` and `piece/cburnett/` with at
/// least one file each).
fn looks_like_asset_root(dir: &Path) -> bool {
    dir.join("images").join("board").is_dir()
        && dir.join("piece").join("cburnett").is_dir()
        && dir
            .join("images")
            .join("board")
            .read_dir()
            .map(|mut rd| rd.next().is_some())
            .unwrap_or(false)
        && dir
            .join("piece")
            .join("cburnett")
            .read_dir()
            .map(|mut rd| rd.next().is_some())
            .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// Errors raised by the asset pipeline.
#[derive(Debug)]
pub enum AssetError {
    /// Could not open (or read) the asset file.
    OpenRead {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The PNG decoder rejected the bytes.
    Decode {
        path: PathBuf,
        source: image::ImageError,
    },
    /// `usvg::Tree::from_data` rejected the SVG.
    SvgParse { path: PathBuf, source: usvg::Error },
    /// A zero-sized image — almost certainly corruption.
    Empty { path: PathBuf },
    /// `tiny_skia::Pixmap::new` returned `None` (oversize dimension).
    Alloc {
        path: PathBuf,
        width: u32,
        height: u32,
    },
    /// The piece pixmap data did not match the configured dimensions.
    ChannelMismatch {
        path: PathBuf,
        expected_bytes: usize,
        actual_bytes: usize,
    },
}

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssetError::OpenRead { path, source } => {
                write!(f, "failed to read asset `{}`: {}", path.display(), source)
            }
            AssetError::Decode { path, source } => write!(
                f,
                "failed to decode PNG at `{}`: {}",
                path.display(),
                source
            ),
            AssetError::SvgParse { path, source } => {
                write!(f, "failed to parse SVG at `{}`: {}", path.display(), source)
            }
            AssetError::Empty { path } => write!(
                f,
                "asset `{}` decoded to a zero-sized image",
                path.display()
            ),
            AssetError::Alloc {
                path,
                width,
                height,
            } => write!(
                f,
                "could not allocate a {}x{} pixmap for `{}`",
                width,
                height,
                path.display()
            ),
            AssetError::ChannelMismatch {
                path,
                expected_bytes,
                actual_bytes,
            } => write!(
                f,
                "pixmap byte count mismatch for `{}` (expected {}, got {})",
                path.display(),
                expected_bytes,
                actual_bytes
            ),
        }
    }
}

impl std::error::Error for AssetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AssetError::OpenRead { source, .. } => Some(source),
            AssetError::Decode { source, .. } => Some(source),
            AssetError::SvgParse { source, .. } => Some(source),
            AssetError::Empty { .. }
            | AssetError::Alloc { .. }
            | AssetError::ChannelMismatch { .. } => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piece_filename_is_svg() {
        assert_eq!(
            PieceId {
                color: PieceColor::White,
                kind: PieceKind::King
            }
            .filename(),
            "wK.svg"
        );
        assert_eq!(
            PieceId {
                color: PieceColor::Black,
                kind: PieceKind::Pawn
            }
            .filename(),
            "bP.svg"
        );
        // No leftover PNG filenames should be reachable through the public API.
        for &color in &[PieceColor::White, PieceColor::Black] {
            for &kind in &[
                PieceKind::King,
                PieceKind::Queen,
                PieceKind::Rook,
                PieceKind::Bishop,
                PieceKind::Knight,
                PieceKind::Pawn,
            ] {
                let f = PieceId { color, kind }.filename();
                assert!(f.ends_with(".svg"), "{:?} expected `.svg` got {}", kind, f);
            }
        }
    }

    #[test]
    fn every_piece_id_has_a_unique_filename() {
        let mut seen = std::collections::HashSet::new();
        for &color in &[PieceColor::White, PieceColor::Black] {
            for &kind in &[
                PieceKind::King,
                PieceKind::Queen,
                PieceKind::Rook,
                PieceKind::Bishop,
                PieceKind::Knight,
                PieceKind::Pawn,
            ] {
                let id = PieceId { color, kind };
                assert!(seen.insert(id), "{:?} has a duplicate filename", id);
            }
        }
    }

    /// Build a minimal fake `public/` tree (one file per required folder).
    fn fake_public_tree(root: &Path) -> PathBuf {
        let public = root.join("public");
        std::fs::create_dir_all(public.join("images").join("board")).unwrap();
        std::fs::create_dir_all(public.join("piece").join("cburnett")).unwrap();
        std::fs::write(public.join("images").join("board").join("blue.png"), b"x").unwrap();
        std::fs::write(public.join("piece").join("cburnett").join("wK.svg"), b"x").unwrap();
        public
    }

    #[test]
    fn board_themes_skip_thumbnails_overlays_and_unsupported_files() {
        let dir = tempfile::tempdir().unwrap();
        let public = fake_public_tree(dir.path());
        let board = public.join("images").join("board");
        for name in [
            "wood2.jpg",
            "green.png",
            "blue.thumbnail.png",
            "wood.orig.jpg",
            "horsey.last-move.png",
            "maple.svg",
            "notes.txt",
        ] {
            std::fs::write(board.join(name), b"x").unwrap();
        }

        let themes = board_themes_from(&public);
        let files: Vec<&str> = themes.iter().map(|theme| theme.file.as_str()).collect();
        assert!(files.contains(&"blue.png"));
        assert!(files.contains(&"green.png"));
        assert!(files.contains(&"wood2.jpg"));
        for bad in [
            "blue.thumbnail.png",
            "wood.orig.jpg",
            "horsey.last-move.png",
            "maple.svg",
            "notes.txt",
        ] {
            assert!(
                !files.contains(&bad),
                "{bad} must not be offered as a theme"
            );
        }
        let labels: Vec<&str> = themes.iter().map(|theme| theme.label.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_by_key(|label| label.to_lowercase());
        assert_eq!(labels, sorted, "themes are sorted by label");
    }

    #[test]
    fn board_labels_are_readable() {
        assert_eq!(board_label("blue.png"), "Blue");
        assert_eq!(board_label("blue-marble.jpg"), "Blue marble");
        assert_eq!(board_label("wood2.jpg"), "Wood2");
    }

    #[test]
    fn a_board_theme_name_cannot_escape_the_board_folder() {
        assert!(board_theme_path("../blue.png").is_none());
        assert!(board_theme_path("svg/blue4.svg").is_none());
        assert!(board_theme_path("blue.thumbnail.png").is_none());
        assert!(board_theme_path("notes.txt").is_none());
    }

    #[test]
    fn piece_sets_require_all_twelve_files() {
        let dir = tempfile::tempdir().unwrap();
        let public = fake_public_tree(dir.path());
        let piece_root = public.join("piece");
        let complete = piece_root.join("staunty");
        std::fs::create_dir_all(&complete).unwrap();
        for color in ["w", "b"] {
            for kind in ["K", "Q", "R", "B", "N", "P"] {
                let file = format!("{color}{kind}.svg");
                std::fs::write(complete.join(&file), b"x").unwrap();
                // The fake cburnett tree only holds wK; complete it too.
                std::fs::write(piece_root.join("cburnett").join(&file), b"x").unwrap();
            }
        }
        let partial = piece_root.join("partial");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("wK.svg"), b"x").unwrap();

        let sets = piece_sets_from(&public);
        let names: Vec<&str> = sets.iter().map(|set| set.name.as_str()).collect();
        assert!(names.contains(&"cburnett"));
        assert!(names.contains(&"staunty"));
        assert!(
            !names.contains(&"partial"),
            "a set with missing pieces is skipped"
        );
    }

    #[test]
    fn piece_set_labels_are_readable() {
        assert_eq!(set_label("cburnett"), "Cburnett");
        assert_eq!(set_label("kiwen-suwi"), "Kiwen suwi");
        assert_eq!(set_label("chess7"), "Chess7");
    }

    #[test]
    fn a_piece_set_name_cannot_escape_the_piece_folder() {
        assert!(piece_set_path("../cburnett").is_none());
        assert!(piece_set_path("nested/cburnett").is_none());
        assert!(piece_set_path("not-a-set").is_none());
    }

    #[test]
    fn looks_like_asset_root_rejects_garbage_dirs() {
        assert!(!looks_like_asset_root(Path::new("definitely/not/here")));
        assert!(!looks_like_asset_root(Path::new(".")));
    }

    #[test]
    fn env_override_wins_over_the_executable_walk() {
        let dir = tempfile::tempdir().unwrap();
        let override_root = fake_public_tree(&dir.path().join("override"));
        let exe_root = fake_public_tree(&dir.path().join("exe-side"));
        let exe_dir = dir.path().join("exe-side").join("target").join("debug");
        std::fs::create_dir_all(&exe_dir).unwrap();

        let chosen = resolve_asset_root_from(Some(override_root.to_str().unwrap()), Some(&exe_dir));
        assert_eq!(chosen, override_root);
        assert_ne!(chosen, exe_root);
    }

    #[test]
    fn invalid_env_override_falls_back_to_executable_walk() {
        let dir = tempfile::tempdir().unwrap();
        let exe_root = fake_public_tree(dir.path());
        let exe_dir = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&exe_dir).unwrap();

        let chosen = resolve_asset_root_from(Some("/definitely/not/a/public/tree"), Some(&exe_dir));
        assert_eq!(chosen, exe_root);
    }

    #[test]
    fn a_macos_bundle_finds_resources_public() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("SiliconBoard.app").join("Contents");
        let public = fake_public_tree(&contents.join("Resources"));
        let macos = contents.join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();

        let chosen = resolve_asset_root_from(None, Some(&macos));
        assert_eq!(chosen, public);
    }

    #[test]
    fn a_macos_bundle_finds_assets_directly_in_resources() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("SiliconBoard.app").join("Contents");
        let resources = contents.join("Resources");
        std::fs::create_dir_all(resources.join("images").join("board")).unwrap();
        std::fs::create_dir_all(resources.join("piece").join("cburnett")).unwrap();
        std::fs::write(
            resources.join("images").join("board").join("blue.png"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            resources.join("piece").join("cburnett").join("wK.svg"),
            b"x",
        )
        .unwrap();
        let macos = contents.join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();

        let chosen = resolve_asset_root_from(None, Some(&macos));
        assert_eq!(chosen, resources);
    }

    #[test]
    fn resolution_falls_back_to_cwd_public_when_nothing_matches() {
        let chosen = resolve_asset_root_from(None, None);
        assert_eq!(chosen, PathBuf::from("public"));
    }

    /// The start-up PGN fixture beside the real asset tree. Like the PolyGlot
    /// book and engine fixtures, a missing file fails the test.
    #[test]
    fn the_startup_pgn_fixture_is_found() {
        let path = project_pgn_path("WorldChamp1972.pgn")
            .expect("the PGN/WorldChamp1972.pgn fixture must sit beside the asset tree");
        assert!(
            path.ends_with(Path::new("PGN").join("WorldChamp1972.pgn")),
            "the resolved path names the PGN folder: {path:?}"
        );
        assert!(path.is_file(), "{path:?} must exist");
    }

    #[test]
    fn a_pgn_path_only_accepts_a_bare_pgn_name() {
        assert!(project_pgn_path("sub/WorldChamp1972.pgn").is_none());
        assert!(project_pgn_path("..").is_none());
        assert!(project_pgn_path("notes.txt").is_none());
        assert!(project_pgn_path("missing.pgn").is_none());
    }

    /// The packaged bundle: the packager puts the fixture into
    /// `Contents/Resources/PGN/`, one folder above the `Contents/Resources/
    /// public` asset root.
    #[test]
    fn a_bundle_finds_the_startup_pgn_in_its_resources() {
        let dir = tempfile::tempdir().unwrap();
        let contents = dir.path().join("SiliconBoard.app").join("Contents");
        let public = fake_public_tree(&contents.join("Resources"));
        let pgn = contents.join("Resources").join("PGN");
        std::fs::create_dir_all(&pgn).unwrap();
        std::fs::write(pgn.join("WorldChamp1972.pgn"), b"[Event \"x\"]\n").unwrap();

        let found = project_pgn_path_from(&public, "WorldChamp1972.pgn").expect("bundled PGN");
        assert_eq!(found, pgn.join("WorldChamp1972.pgn"));
        assert!(project_pgn_path_from(&public, "Karpov.pgn").is_none());
    }
}

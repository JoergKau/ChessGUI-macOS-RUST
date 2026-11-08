//! ECO opening names.
//!
//! The table is the CC0 `lichess-org/chess-openings` data shipped as
//! `public/data/openings/openings.tsv`. Each line is replayed once and the
//! final position is mapped to its name. Matching ignores the half-move and
//! full-move counters, so the name follows the board position.
//!
//! This is a name lookup, not a book: it is only used to label the book box.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shakmaty::fen::Epd;
use shakmaty::san::SanPlus;
use shakmaty::{Chess, EnPassantMode, Position};

/// A read failure for the opening table.
#[derive(Debug)]
pub enum OpeningError {
    /// The table file could not be read.
    Read {
        /// File that failed.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

impl std::fmt::Display for OpeningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpeningError::Read { path, source } => {
                write!(f, "could not read `{}`: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for OpeningError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpeningError::Read { source, .. } => Some(source),
        }
    }
}

/// Opening names keyed by the position after their PGN line.
#[derive(Debug, Default)]
pub struct OpeningNames {
    by_position: HashMap<String, String>,
}

impl OpeningNames {
    /// Read and index the table at `path`.
    pub fn load(path: &Path) -> Result<Self, OpeningError> {
        let text = std::fs::read_to_string(path).map_err(|source| OpeningError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::parse(&text))
    }

    /// Index tab-separated `eco`, `name`, `pgn` rows. A malformed row is
    /// skipped; the table is data, not user input.
    pub fn parse(text: &str) -> Self {
        let mut by_position = HashMap::new();
        for line in text.lines().skip(1) {
            let mut fields = line.splitn(3, '\t');
            let (Some(_eco), Some(name), Some(pgn)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(key) = key_after_pgn(pgn) else {
                continue;
            };
            by_position.entry(key).or_insert_with(|| name.to_string());
        }
        Self { by_position }
    }

    /// The name of the opening that ends at `position`, if the table has it.
    pub fn name_for(&self, position: &Chess) -> Option<&str> {
        let key = Epd::from_position(position, EnPassantMode::Legal).to_string();
        self.by_position.get(&key).map(String::as_str)
    }

    /// Number of indexed positions, for logs and tests.
    pub fn len(&self) -> usize {
        self.by_position.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.by_position.is_empty()
    }
}

/// Play a PGN line and return its final position key, or `None` if a token is
/// not legal. Move numbers (`1.`, `1...`) are skipped.
fn key_after_pgn(pgn: &str) -> Option<String> {
    let mut position = Chess::default();
    for token in pgn.split_whitespace() {
        if token.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let san = SanPlus::from_ascii(token.as_bytes()).ok()?;
        let m = san.san.to_move(&position).ok()?;
        position.play_unchecked(m);
    }
    Some(Epd::from_position(&position, EnPassantMode::Legal).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after_line(line: &str) -> Chess {
        let mut position = Chess::default();
        for token in line.split_whitespace() {
            if token.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            let san = SanPlus::from_ascii(token.as_bytes()).unwrap();
            let m = san.san.to_move(&position).unwrap();
            position.play_unchecked(m);
        }
        position
    }

    #[test]
    fn opening_names_follow_the_position() {
        let names = OpeningNames::parse(
            "eco\tname\tpgn\n\
             C20\tOpen Game\t1. e4 e5\n\
             B20\tSicilian Defense\t1. e4 c5\n\
             C50\tItalian Game\t1. e4 e5 2. Nf3 Nc6 3. Bc4\n",
        );
        assert_eq!(names.name_for(&after_line("1. e4 e5")), Some("Open Game"));
        assert_eq!(
            names.name_for(&after_line("1. e4 c5")),
            Some("Sicilian Defense")
        );
        assert_eq!(
            names.name_for(&after_line("1. e4 e5 2. Nf3 Nc6 3. Bc4")),
            Some("Italian Game")
        );
        assert_eq!(
            names.name_for(&Chess::default()),
            None,
            "the start position is not in the table"
        );
    }

    #[test]
    fn malformed_rows_are_skipped() {
        let names = OpeningNames::parse(
            "eco\tname\tpgn\n\
             not a row\n\
             A00\tAmar Opening\t1. Nh3\n",
        );
        assert_eq!(names.len(), 1);
    }
}

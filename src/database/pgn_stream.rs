//! Streaming PGN reader: one game per iteration, without loading the file.
//!
//! A downloaded collection is easily 500 MB–2.5 GB, so it must never be read
//! into one `String`. [`PgnStream`] reads line by line and hands the caller one
//! complete game — tag pairs plus movetext — at a time. The rule for "this line
//! starts a new game" matches `chess::pgn::split_games`: a tag line that
//! appears after movetext begins the next game.
//!
//! The reader also counts the bytes it has consumed, which is what the import
//! dialog's progress bar follows: the total game count of a PGN is unknown
//! until the file has been read, but its size is known up front.

use std::io::{BufRead, Result};

/// Yields one PGN game per iteration.
pub struct PgnStream<R: BufRead> {
    reader: R,
    line: String,
    bytes: u64,
    /// The first line of the next game, read while closing the current one.
    pending: Option<String>,
    finished: bool,
    /// Whether the game being collected has movetext (a line that is neither a
    /// tag nor blank). A tag line after movetext starts a new game.
    saw_movetext: bool,
}

impl<R: BufRead> PgnStream<R> {
    /// Wrap a buffered reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: String::new(),
            bytes: 0,
            pending: None,
            finished: false,
            saw_movetext: false,
        }
    }

    /// Bytes consumed from the source so far, including the game being read.
    pub fn bytes_read(&self) -> u64 {
        self.bytes
    }

    /// Read one line into `self.line`; `false` at end of file.
    fn next_line(&mut self) -> Result<bool> {
        self.line.clear();
        let read = self.reader.read_line(&mut self.line)?;
        if read == 0 {
            return Ok(false);
        }
        self.bytes += read as u64;
        Ok(true)
    }
}

impl<R: BufRead> Iterator for PgnStream<R> {
    type Item = Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut current = String::new();
        // A line read while closing the previous game already belongs to this
        // one.
        if let Some(line) = self.pending.take() {
            current.push_str(&line);
            self.saw_movetext = !line.trim_start().starts_with('[') && !line.trim().is_empty();
        }
        loop {
            match self.next_line() {
                Ok(true) => {}
                Ok(false) => {
                    self.finished = true;
                    break;
                }
                Err(err) => {
                    self.finished = true;
                    return Some(Err(err));
                }
            }
            let line = &self.line;
            let trimmed = line.trim();
            let is_tag = trimmed.starts_with('[');
            // A tag line after movetext opens the next game.
            if is_tag && self.saw_movetext {
                self.pending = Some(line.clone());
                break;
            }
            if !is_tag && !trimmed.is_empty() {
                self.saw_movetext = true;
            }
            current.push_str(line);
        }
        if current.trim().is_empty() {
            // Trailing blank lines at the end of the file.
            if self.finished {
                return None;
            }
            return self.next();
        }
        Some(Ok(current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn games(text: &str) -> Vec<String> {
        PgnStream::new(Cursor::new(text.as_bytes()))
            .map(|game| game.unwrap())
            .collect()
    }

    #[test]
    fn two_games_are_split_at_the_second_event_tag() {
        let text = "[Event \"One\"]\n\n1. e4 e5 1-0\n\n[Event \"Two\"]\n\n1. d4 d5 0-1\n";
        let games = games(text);
        assert_eq!(games.len(), 2);
        assert!(games[0].contains("1. e4 e5"));
        assert!(games[1].contains("1. d4 d5"));
        assert!(!games[0].contains("Game Two"));
    }

    #[test]
    fn crlf_line_endings_are_fine() {
        let text =
            "[Event \"One\"]\r\n\r\n1. e4 e5 1-0\r\n\r\n[Event \"Two\"]\r\n\r\n1. d4 0-1\r\n";
        assert_eq!(games(text).len(), 2);
    }

    #[test]
    fn a_tag_less_game_is_still_one_game() {
        let games = games("1. e4 e5 2. Nf3 *\n");
        assert_eq!(games.len(), 1);
        assert!(games[0].contains("Nf3"));
    }

    #[test]
    fn glued_move_numbers_are_kept_verbatim() {
        let games = games("[Event \"x\"]\n\n1.e4 e5 2.Nf3 Nc6 *\n");
        assert!(games[0].contains("1.e4 e5 2.Nf3"));
    }

    #[test]
    fn the_byte_counter_reaches_the_file_size() {
        let text = "[Event \"One\"]\n\n1. e4 1-0\n";
        let mut stream = PgnStream::new(Cursor::new(text.as_bytes()));
        while stream.next().is_some() {}
        assert_eq!(stream.bytes_read(), text.len() as u64);
    }

    #[test]
    fn blank_lines_at_the_end_do_not_make_an_empty_game() {
        let games = games("[Event \"One\"]\n\n1. e4 1-0\n\n\n\n");
        assert_eq!(games.len(), 1);
    }
}

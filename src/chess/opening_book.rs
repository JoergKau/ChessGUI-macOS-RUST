//! PolyGlot `.bin` opening-book reader.
//!
//! ## Format
//!
//! A `.bin` book is a flat array of 16-byte records laid out *in ascending
//! Zobrist-key order* so that book probes can be answered with a single
//! `O(log N)` binary search:
//!
//! ```text
//! offset  size  field
//! ──────  ────  ───────────────────────────────────────────────
//! 0       8     Zobrist hash (big-endian u64)
//! 8       2     Encoded move (big-endian u16): from | (to << 6) | (promo << 12)
//! 10      2     Weight (big-endian u16)
//! 12      4     Learn counter (big-endian u32, opaque)
//! ```
//!
//! Square numbering follows the PolyGlot convention rather than shakmaty's,
//! so we translate when converting a hit back to a [`shakmaty::Move`].
//!
//! Promotion codes:
//! * `0` → no promotion (or non-promotion move);
//! * `1` → knight;
//! * `2` → bishop;
//! * `3` → rook;
//! * `4` → queen.
//!
//! ## Why we don't recompute Zobrist
//!
//! `shakmaty::Position::zobrist_hash(EnPassantMode::Legal)` already returns
//! a key compatible with PolyGlot books — that crate ships with all 781
//! PolyGlot random constants inline. We just call into it; no separate
//! Zobrist table required.

use byteorder::{BigEndian, ReadBytesExt};
use shakmaty::{Move, Position, Role, Square};

use std::io::Cursor;
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// On-disk record shape
// ─────────────────────────────────────────────────────────────────────────────

/// Raw, in-memory representation of one PolyGlot record.
///
/// This is the layout after big-endian decoding but *before* we map the
/// encoded fields onto shakmaty types.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BookEntry {
    /// Zobrist hash of the position this move was recorded for.
    pub key: u64,
    /// PolyGlot-encoded move bits. Use
    /// [`BookEntry::to_move`] to translate into a [`shakmaty::Move`].
    pub encoded_move: u16,
    /// Lookup weight — higher means the move is statistically more popular
    /// and should be picked more often. Used by the future "best-weight"
    /// book-selection policy.
    pub weight: u16,
    /// "Learn" field — opaque integer used by PolyGlot-aware engines.
    pub learn: u32,
}

impl BookEntry {
    /// The fully-qualified legal [`Move`] this record encodes in `pos`, or
    /// `None` when no legal move matches (wrong position for this key).
    ///
    /// Every legal move is encoded the PolyGlot way and compared with the
    /// record, which covers castling ("king takes own rook", e.g. e1h1),
    /// en passant and promotion uniformly.
    pub fn to_move<P: Position>(self, pos: &P) -> Option<Move> {
        pos.legal_moves()
            .into_iter()
            .find(|m| polyglot_encoding(m) == Some(self.encoded_move))
    }
}

/// PolyGlot 16-bit encoding of a move (the inverse of
/// [`BookEntry::to_move`]). Castling is encoded as the king moving onto
/// its own rook; `None` for moves without an origin square (crazyhouse drops).
fn polyglot_encoding(m: &Move) -> Option<u16> {
    let from = m.from()?;
    let to = match m {
        Move::Castle { rook, .. } => *rook,
        _ => m.to(),
    };
    let promo: u16 = match m.promotion() {
        None => 0,
        Some(Role::Knight) => 1,
        Some(Role::Bishop) => 2,
        Some(Role::Rook) => 3,
        Some(Role::Queen) => 4,
        Some(_) => return None,
    };
    Some(polyglot_index(to) | (polyglot_index(from) << 6) | (promo << 12))
}

/// PolyGlot square index: a1 = 0, b1 = 1, …, h8 = 63.
fn polyglot_index(sq: Square) -> u16 {
    (sq.rank().to_u32() * 8 + sq.file().to_u32()) as u16
}

// ─────────────────────────────────────────────────────────────────────────────
// Book container
// ─────────────────────────────────────────────────────────────────────────────

/// In-memory PolyGlot opening book.
#[derive(Clone, Debug)]
pub struct PolyglotBook {
    /// Decoded records; always sorted by `key` ascending.
    entries: Vec<BookEntry>,
}

impl PolyglotBook {
    /// Parse a PolyGlot `.bin` file from disk.
    pub fn open(path: &Path) -> Result<Self, BookError> {
        let bytes = std::fs::read(path).map_err(|source| BookError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_bytes(&bytes)
    }

    /// Decode a `.bin` Polglot stream from raw bytes. Useful for tests and
    /// for embedded books (e.g. ones shipped via `include_bytes!`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BookError> {
        if !bytes.len().is_multiple_of(16) {
            return Err(BookError::InvalidSize {
                byte_len: bytes.len(),
            });
        }
        let count = bytes.len() / 16;
        let mut entries: Vec<BookEntry> = Vec::with_capacity(count);
        let mut cursor = Cursor::new(bytes);

        for _ in 0..count {
            let key = cursor.read_u64::<BigEndian>().map_err(BookError::Read)?;
            let encoded_move = cursor.read_u16::<BigEndian>().map_err(BookError::Read)?;
            let weight = cursor.read_u16::<BigEndian>().map_err(BookError::Read)?;
            let learn = cursor.read_u32::<BigEndian>().map_err(BookError::Read)?;
            entries.push(BookEntry {
                key,
                encoded_move,
                weight,
                learn,
            });
        }

        // Sort by key ascending so `find()` and `find_moves()` can rely on
        // monotonic order during binary search. Some on-disk books are
        // already sorted; sorting again here is a no-op in that case.
        entries.sort_by_key(|e| e.key);

        Ok(Self { entries })
    }

    /// Total number of records.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the book holds no records.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All entries recorded for `key`, in book order. Empty when the book
    /// has no move for this position. Uses a lower-bound search and then
    /// walks the run of equal keys, so collisions are never missed.
    pub fn find(&self, key: u64) -> Vec<BookEntry> {
        let start = self.entries.partition_point(|e| e.key < key);
        self.entries[start..]
            .iter()
            .take_while(|e| e.key == key)
            .copied()
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum BookError {
    /// Failed to read the file from disk.
    Open {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// `byteorder` blew up while parsing big-endian integers.
    Read(std::io::Error),
    /// Length is not a multiple of 16 bytes.
    InvalidSize { byte_len: usize },
}

impl std::fmt::Display for BookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BookError::Open { path, source } => {
                write!(f, "could not open book `{}`: {}", path.display(), source)
            }
            BookError::Read(source) => write!(f, "could not parse book record: {}", source),
            BookError::InvalidSize { byte_len } => {
                write!(f, "book is not a multiple of 16 bytes (got {})", byte_len)
            }
        }
    }
}

impl std::error::Error for BookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BookError::Open { source, .. } | BookError::Read(source) => Some(source),
            BookError::InvalidSize { .. } => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Chooses one of several book moves at random, in proportion to their
/// PolyGlot weights. Uses a small xorshift generator; the seed is random per
/// process, and tests fix it.
#[derive(Clone, Debug)]
pub struct BookPicker {
    /// Generator state. Never zero, which xorshift cannot leave.
    state: u64,
}

impl Default for BookPicker {
    /// A picker with a random seed.
    fn default() -> Self {
        Self::with_seed(random_seed())
    }
}

impl BookPicker {
    /// A picker that makes the same choices for the same seed.
    pub fn with_seed(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    /// Index of the chosen move, or `None` when no move has a positive weight.
    /// A move with weight zero is never chosen.
    pub fn pick(&mut self, weights: &[u16]) -> Option<usize> {
        let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        let mut roll = self.next_u64() % total;
        for (index, weight) in weights.iter().enumerate() {
            let weight = u64::from(*weight);
            if roll < weight {
                return Some(index);
            }
            roll -= weight;
        }
        None
    }

    /// One step of xorshift64.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

/// A seed that differs between runs, taken from the standard library's
/// randomly keyed hasher.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x5151_5151);
    hasher.finish()
}

/// A tiny, self-contained book for tests: the four main first moves for the
/// start position, with descending weights. It keeps the test suite free of
/// external `.bin` fixtures (no opening book is part of the repository or
/// the app).
#[cfg(test)]
pub(crate) fn sample_start_book() -> PolyglotBook {
    let key = crate::chess::GameState::new().zobrist();
    let records = [
        (key, 0x031cu16, 60u16, 0u32), // e2e4
        (key, 0x02dbu16, 30u16, 0u32), // d2d4
        (key, 0x0195u16, 20u16, 0u32), // g1f3
        (key, 0x0052u16, 10u16, 0u32), // b1c3
    ];
    let bytes: Vec<u8> = records
        .iter()
        .flat_map(|(key, encoded, weight, learn)| {
            let mut record = Vec::with_capacity(16);
            record.extend_from_slice(&key.to_be_bytes());
            record.extend_from_slice(&encoded.to_be_bytes());
            record.extend_from_slice(&weight.to_be_bytes());
            record.extend_from_slice(&learn.to_be_bytes());
            record
        })
        .collect();
    PolyglotBook::from_bytes(&bytes).expect("the built-in sample book is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chess::GameState;

    /// Build an in-memory mini-book from key/move/weight/learn records —
    /// useful for asserting the binary-search behaviour without a real
    /// .bin file on disk.
    fn make_book(records: &[(u64, u16, u16, u32)]) -> PolyglotBook {
        let bytes: Vec<u8> = records
            .iter()
            .flat_map(|(k, m, w, l)| {
                let mut v = Vec::with_capacity(16);
                v.extend_from_slice(&k.to_be_bytes());
                v.extend_from_slice(&m.to_be_bytes());
                v.extend_from_slice(&w.to_be_bytes());
                v.extend_from_slice(&l.to_be_bytes());
                v
            })
            .collect();
        PolyglotBook::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn rejects_size_mismatch() {
        let err = PolyglotBook::from_bytes(&[0u8; 7]);
        assert!(matches!(err, Err(BookError::InvalidSize { byte_len: 7 })));
    }

    #[test]
    fn accepts_whole_records_and_rejects_partial_ones() {
        assert!(PolyglotBook::from_bytes(&[0u8; 16]).is_ok());
        assert!(PolyglotBook::from_bytes(&[0u8; 15]).is_err());
    }

    #[test]
    fn binary_search_returns_matching_entries() {
        // Synthesize a tiny book with three distinct keys, multiple records
        // for one of them.
        let book = make_book(&[
            (10, 0x0000, 1, 0),
            (10, 0x0001, 2, 0),
            (20, 0x0002, 3, 0),
            (20, 0x0003, 4, 0),
            (20, 0x0004, 5, 0),
            (99, 0x0005, 6, 0),
        ]);

        assert_eq!(book.len(), 6);
        assert_eq!(book.find(10).len(), 2);
        assert_eq!(book.find(20).len(), 3);
        assert_eq!(book.find(99).len(), 1);
        assert_eq!(book.find(7).len(), 0);
        assert_eq!(book.find(30).len(), 0);
        assert_eq!(book.find(91).len(), 0);

        // Weights line up with insertion order.
        assert_eq!(weight_of(&book, 10), 3);
        assert_eq!(weight_of(&book, 20), 12);
        assert_eq!(weight_of(&book, 99), 6);
        assert_eq!(weight_of(&book, 7), 0);
    }

    /// Sum of the weights recorded for `key`.
    fn weight_of(book: &PolyglotBook, key: u64) -> u32 {
        book.find(key).iter().map(|e| u32::from(e.weight)).sum()
    }

    #[test]
    fn encoding_puts_to_square_low_and_promotion_high() {
        // c7-c8=Q: to = c8 (58) in bits 0–5, from = c7 (50) in bits 6–11,
        // promotion = queen (4) in bits 12–14.
        let promo = Move::Normal {
            role: Role::Pawn,
            from: Square::from_ascii(b"c7").unwrap(),
            capture: None,
            to: Square::from_ascii(b"c8").unwrap(),
            promotion: Some(Role::Queen),
        };
        assert_eq!(polyglot_encoding(&promo), Some(58 | (50 << 6) | (4 << 12)));
    }

    #[test]
    fn empty_book_returns_nothing() {
        let book = make_book(&[]);
        assert_eq!(book.find(0).len(), 0);
        assert_eq!(book.len(), 0);
    }

    #[test]
    fn a_book_keyed_on_the_start_position_answers_that_key() {
        // The sample book is built in memory (no external .bin fixture): the
        // same records a real opening book has for the start position.
        let book = sample_start_book();
        assert_eq!(book.len(), 4);

        // The PolyGlot canonical starting-position Zobrist key. If
        // `GameState::zobrist` ever stops matching it, books stop answering.
        let canonical_key: u64 = 0x463b96181691fc9c;
        assert_eq!(
            crate::chess::GameState::new().zobrist(),
            canonical_key,
            "GameState::zobrist() must be the PolyGlot start key"
        );
        let start_moves = book.find(canonical_key);
        assert_eq!(start_moves.len(), 4, "all four first moves are in the book");

        // Every record must describe a real, legal start-position move.
        let start_pos = shakmaty::Chess::default();
        let mut total_weight: u64 = 0;
        for entry in &start_moves {
            let from = (entry.encoded_move >> 6) & 0x3F;
            let to = entry.encoded_move & 0x3F;
            assert_ne!(
                from, to,
                "record {:#06x} is a null move",
                entry.encoded_move
            );
            assert!(
                entry.to_move(&start_pos).is_some(),
                "book move {:#06x} does not map onto a legal start-position move",
                entry.encoded_move
            );
            total_weight = total_weight.saturating_add(u64::from(entry.weight));
        }
        assert!(total_weight > 0, "book's total weight should be non-zero");

        // The key the app really probes with (`GameState::zobrist`) finds the
        // records.
        assert_eq!(
            book.find(crate::chess::GameState::new().zobrist()).len(),
            4,
            "GameState::zobrist() of the start must find the book's entries"
        );
    }

    /// PolyGlot square index (a1 = 0 … h8 = 63) for a square name.
    fn pg(name: &str) -> u16 {
        let sq = Square::from_ascii(name.as_bytes()).unwrap();
        (sq.rank().to_u32() * 8 + sq.file().to_u32()) as u16
    }

    /// Encode a move the PolyGlot way: to-square in bits 0–5, from-square in
    /// bits 6–11, promotion code in bits 12–14.
    fn encode(from: &str, to: &str, promo: u16) -> u16 {
        pg(to) | (pg(from) << 6) | (promo << 12)
    }

    #[test]
    fn real_start_position_record_is_e2e4() {
        // 0x031c is the first-move record for e2e4 in the sample book above
        // (and in well-known PolyGlot books).
        let entry = BookEntry {
            key: 0,
            encoded_move: 0x031c,
            weight: 1,
            learn: 0,
        };
        assert_eq!(
            entry.to_move(&shakmaty::Chess::default()),
            Some(Move::Normal {
                role: Role::Pawn,
                from: Square::from_ascii(b"e2").unwrap(),
                capture: None,
                to: Square::from_ascii(b"e4").unwrap(),
                promotion: None,
            })
        );
    }

    #[test]
    fn book_castling_move_decodes_to_castle() {
        // PolyGlot encodes castling as "king takes own rook" (e1h1 for O-O).
        let game =
            GameState::from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1").unwrap();
        let encoded = encode("e1", "h1", 0);
        let book = make_book(&[(0, encoded, 1, 0)]);
        let entry = book.entries[0];
        let m = entry.to_move(&game.position());
        assert_eq!(
            m,
            Some(Move::Castle {
                king: Square::from_ascii(b"e1").unwrap(),
                rook: Square::from_ascii(b"h1").unwrap(),
            })
        );
    }

    #[test]
    fn book_en_passant_move_decodes_to_en_passant() {
        // White pawn on e5, black just played d7-d5: e5xd6 e.p.
        let game =
            GameState::from_fen("rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 3")
                .unwrap();
        let encoded = encode("e5", "d6", 0);
        let book = make_book(&[(0, encoded, 1, 0)]);
        let entry = book.entries[0];
        assert_eq!(
            entry.to_move(&game.position()),
            Some(Move::EnPassant {
                from: Square::from_ascii(b"e5").unwrap(),
                to: Square::from_ascii(b"d6").unwrap(),
            })
        );
    }

    #[test]
    fn book_promotion_move_decodes_with_role() {
        // a7-a8=N : the PolyGlot promotion code for knight is 1.
        let game = GameState::from_fen("7k/P7/8/8/8/8/8/7K w - - 0 1").unwrap();
        let encoded = encode("a7", "a8", 1);
        let book = make_book(&[(0, encoded, 1, 0)]);
        let entry = book.entries[0];
        assert_eq!(
            entry.to_move(&game.position()),
            Some(Move::Normal {
                role: Role::Pawn,
                from: Square::from_ascii(b"a7").unwrap(),
                capture: None,
                to: Square::from_ascii(b"a8").unwrap(),
                promotion: Some(Role::Knight),
            })
        );
    }

    #[test]
    fn unsorted_input_is_resorted() {
        // Construct a book whose records are *not* sorted by key (we
        // synthesise via `make_book` which IS sorted, so we post-process).
        let mut book = make_book(&[(1, 0, 0, 0), (3, 0, 0, 0), (2, 0, 0, 0)]);
        // Force unsorted by swapping two entries.
        book.entries.swap(1, 2);
        // After a fresh open, the loader sort is called — emulate by re-opening.
        let bytes: Vec<u8> = {
            let mut out = Vec::new();
            for e in &book.entries {
                out.extend_from_slice(&e.key.to_be_bytes());
                out.extend_from_slice(&e.encoded_move.to_be_bytes());
                out.extend_from_slice(&e.weight.to_be_bytes());
                out.extend_from_slice(&e.learn.to_be_bytes());
            }
            out
        };
        let sorted = PolyglotBook::from_bytes(&bytes).unwrap();
        assert_eq!(sorted.find(1).len(), 1);
        assert_eq!(sorted.find(2).len(), 1);
        assert_eq!(sorted.find(3).len(), 1);
    }

    #[test]
    fn picker_never_chooses_a_zero_weight_move() {
        let mut picker = BookPicker::with_seed(7);
        for _ in 0..500 {
            assert_eq!(picker.pick(&[0, 5, 0]), Some(1));
        }
    }

    #[test]
    fn picker_follows_the_weights() {
        let mut picker = BookPicker::with_seed(42);
        let heavy = (0..4000)
            .filter(|_| picker.pick(&[1, 3]) == Some(1))
            .count();
        // Expected 3000 with a standard deviation of about 27.
        assert!(
            (2800..=3200).contains(&heavy),
            "heavy move chosen {} times",
            heavy
        );
    }

    #[test]
    fn picker_gives_nothing_when_no_move_has_weight() {
        let mut picker = BookPicker::with_seed(1);
        assert_eq!(picker.pick(&[]), None);
        assert_eq!(picker.pick(&[0, 0]), None);
    }

    #[test]
    fn picker_with_the_same_seed_makes_the_same_choices() {
        let mut a = BookPicker::with_seed(99);
        let mut b = BookPicker::with_seed(99);
        let first: Vec<_> = (0..20).map(|_| a.pick(&[2, 2, 2])).collect();
        let second: Vec<_> = (0..20).map(|_| b.pick(&[2, 2, 2])).collect();
        assert_eq!(first, second);
    }
}

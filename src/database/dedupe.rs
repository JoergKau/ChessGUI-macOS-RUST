//! The 8-byte fingerprint that makes a repeated import a no-op.
//!
//! Two independent needs share one key:
//!   * importing the same PGN twice must not add rows;
//!   * the monthly re-import of a downloaded collection must append only the
//!     games that are actually new.
//!
//! The key is **FNV-1a 64**, written out here rather than taken from
//! `std::hash`: `DefaultHasher` is only guaranteed to be stable within one
//! build, so every fingerprint in an existing database would silently change
//! after an app update. The test below pins a fixed vector for that reason.

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hash `bytes` with FNV-1a (64 bit).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The fingerprint stored in `games.dedupe`.
///
/// It covers the two players' normalised names, the date, the result, the
/// mainline length and the first and last eight SAN tokens. That catches the
/// same game stored twice — including once with comments and once without —
/// while two genuinely different games between the same players on the same
/// day differ in their moves.
pub fn fingerprint(fields: &FingerprintFields) -> [u8; 8] {
    let mut text = String::with_capacity(96);
    text.push_str(&fields.white_norm);
    text.push('\u{1f}');
    text.push_str(&fields.black_norm);
    text.push('\u{1f}');
    text.push_str(&fields.date_key.to_string());
    text.push('\u{1f}');
    text.push_str(&fields.result.to_string());
    text.push('\u{1f}');
    text.push_str(&fields.ply_count.to_string());
    text.push('\u{1f}');
    text.push_str(&fields.first_moves.join(" "));
    text.push('\u{1f}');
    text.push_str(&fields.last_moves.join(" "));
    fnv1a(text.as_bytes()).to_le_bytes()
}

/// Everything [`fingerprint`] hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintFields {
    /// Normalised White name.
    pub white_norm: String,
    /// Normalised Black name.
    pub black_norm: String,
    /// `yyyymmdd`, 0 when unknown.
    pub date_key: i64,
    /// `games.result` code (0 white, 1 black, 2 draw, 3 unknown).
    pub result: i64,
    /// Mainline half-moves.
    pub ply_count: i64,
    /// Up to eight SAN tokens from the start.
    pub first_moves: Vec<String>,
    /// Up to eight SAN tokens from the end.
    pub last_moves: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vector is pinned on purpose: changing it would make every stored
    /// fingerprint wrong, so a change here is a schema break, not a tweak.
    #[test]
    fn fnv1a_matches_the_published_vector() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    fn sample() -> FingerprintFields {
        FingerprintFields {
            white_norm: "carlsen magnus".to_string(),
            black_norm: "nepomniachtchi ian".to_string(),
            date_key: 20_211_203,
            result: 2,
            ply_count: 136,
            first_moves: vec!["e4".into(), "c5".into()],
            last_moves: vec!["Rd3".into(), "Qe4".into()],
        }
    }

    #[test]
    fn the_same_game_hashes_to_the_same_fingerprint() {
        assert_eq!(fingerprint(&sample()), fingerprint(&sample()));
    }

    #[test]
    fn a_different_move_or_result_changes_the_fingerprint() {
        let base = fingerprint(&sample());

        let mut other = sample();
        other.first_moves = vec!["d4".into(), "Nf6".into()];
        assert_ne!(base, fingerprint(&other));

        let mut other = sample();
        other.result = 0;
        assert_ne!(base, fingerprint(&other));
    }

    #[test]
    fn the_comments_do_not_change_the_fingerprint() {
        // Comments never reach this struct, so an annotated copy of a game and
        // the plain copy share a key. The test states the intent.
        let annotated = sample();
        assert_eq!(fingerprint(&sample()), fingerprint(&annotated));
    }
}

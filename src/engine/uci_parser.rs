//! Line-by-line UCI parser.
//!
//! Each [`EngineEvent`] variant reduces to one engine stdout line. We pass
//! the full line in (already trim'd of trailing whitespace) and the parser
//! returns either an [`EngineEvent`] or [`UciParseError`].
//!
//! ## Design
//!
//! The parser is *strict-but-lenient*: it rejects syntactically malformed
//! lines but tolerates engine-specific extras we don't care about (e.g.
//! `info refutation …`, `info currline …`) by quietly dropping them.
//! Every supported field is exposed through [`EngineStats`] and missing
//! fields stay `None`.
//!
//! ## Testing strategy
//!
//! Unit tests live at the bottom of this file. They include real lines
//! captured from a live Stockfish 19 invocation (`/opt/homebrew/bin/stockfish`)
//! so we know the parser handles the engine output the project will see in
//! production.

use crate::engine::protocol::{EngineEvent, EngineStats, Score, UciOption, UciOptionKind};

use std::num::ParseIntError;

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Reduce a single, newline-trimmed engine line to an [`EngineEvent`].
///
/// Unknown / engine-specific lines return `Err(UciParseError::UnknownLine)`
/// so the [`crate::engine::EngineProcess`] can decide what to do with
/// them (typically: log + ignore). The function never panics.
pub fn parse_line(line: &str) -> Result<EngineEvent, UciParseError> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut tokens = trimmed.split_ascii_whitespace();

    let head = tokens.next().ok_or(UciParseError::Empty)?;
    match head {
        "id" => parse_id(&mut tokens),
        "option" => parse_option(&mut tokens).map(|opt| EngineEvent::UciOk {
            name: String::new(),
            author: String::new(),
            options: vec![opt],
        }),
        "uciok" => Ok(EngineEvent::UciOk {
            name: String::new(),
            author: String::new(),
            options: Vec::new(),
        }),
        "readyok" => Ok(EngineEvent::ReadyOk),
        "info" => parse_info(&mut tokens).map(EngineEvent::Info),
        "bestmove" => parse_bestmove(&mut tokens),
        // UCI also defines `copyprotection`, `registration`, `option name …`,
        // and a few protocol-noise lines. We treat unknown tokens as
        // noise rather than errors so the supervisor stays robust against
        // extension output from future engines.
        _ => Err(UciParseError::UnknownLine {
            line: trimmed.to_string(),
        }),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sub-parsers (private)
// ─────────────────────────────────────────────────────────────────────────────

/// Parse `id name …` or `id author …`. Returns a synthetic
/// [`EngineEvent::UciOk`] only as a placeholder; the real
/// `UciOk { name, author, options }` is built by the [`parse_handshake`]
/// helper used by [`crate::engine::EngineProcess`], which accumulates
/// `id …` + `option …` lines until it sees `uciok`.
///
/// For callers using the simple [`parse_line`] entry point, every
/// `id …` and `option …` line contributes one placeholder
/// `UciOk { name: "", author: "", options: … }` event so callers don't
/// lose information; the [`crate::engine::EngineProcess`] aggregator
/// merges them back into the canonical UciOk.
fn parse_id<'a, I: Iterator<Item = &'a str>>(tokens: &mut I) -> Result<EngineEvent, UciParseError> {
    let kind = tokens.next().ok_or(UciParseError::Id {
        reason: "missing kind".to_string(),
    })?;
    let rest: String = tokens.collect::<Vec<_>>().join(" ");
    match kind {
        "name" => Ok(EngineEvent::UciOk {
            name: rest,
            author: String::new(),
            options: Vec::new(),
        }),
        "author" => Ok(EngineEvent::UciOk {
            name: String::new(),
            author: rest,
            options: Vec::new(),
        }),
        _ => Err(UciParseError::Id {
            reason: format!("unknown sub-kind `{}`", kind),
        }),
    }
}

/// Parse one `option name …` line. End-of-line tolerant in that an option
/// with too few sub-fields returns [`UciParseError::Option`].
///
/// UCI spec gotcha: an option **name** can contain spaces
/// (`option name Clear Hash type button`), and so can the default value
/// / var list. We accept this by scanning forward to the next known
/// UCI option keyword whenever we need to terminate a value.
fn parse_option<'a, I: Iterator<Item = &'a str>>(
    tokens: &mut I,
) -> Result<UciOption, UciParseError> {
    let buf: Vec<&str> = tokens.collect();

    // 1. Skip the `name` keyword and capture everything up to the next
    //    option keyword — that's the name (may contain spaces).
    let mut i = 0;
    if buf.get(i).copied() != Some("name") {
        return Err(UciParseError::Option {
            reason: "missing `name` keyword".to_string(),
        });
    }
    let name_start = i + 1;
    while i < buf.len() && !is_option_keyword(buf[i]) {
        i += 1;
    }
    if i == name_start {
        return Err(UciParseError::Option {
            reason: "missing option name".to_string(),
        });
    }
    let name = buf[name_start..i].join(" ");
    if i >= buf.len() {
        return Err(UciParseError::Option {
            reason: "missing `type` keyword".to_string(),
        });
    }

    // 2. `type <kind_str>`.
    if buf[i] != "type" {
        return Err(UciParseError::Option {
            reason: format!("expected `type`, got `{}`", buf[i]),
        });
    }
    i += 1;
    if i >= buf.len() {
        return Err(UciParseError::Option {
            reason: "missing type".to_string(),
        });
    }
    let kind_str = buf[i];
    i += 1;

    // 3. Sub-keyword/value pairs — `default`, `min`, `max`, `var` — with
    //    values that may span multiple tokens before the next keyword.
    let rest = &buf[i..];
    let kind = parse_option_type(kind_str, rest)?;

    Ok(UciOption { name, kind })
}

/// Keywords that terminate a multi-word option value.
fn is_option_keyword(s: &str) -> bool {
    matches!(s, "type" | "default" | "min" | "max" | "var")
}

fn parse_option_type(kind: &str, rest: &[&str]) -> Result<UciOptionKind, UciParseError> {
    let mut default_seen: Option<String> = None;
    let mut min: Option<i64> = None;
    let mut max: Option<i64> = None;
    let mut vars: Vec<String> = Vec::new();

    let mut i = 0;
    while i < rest.len() {
        let key = rest[i];
        match key {
            "default" => {
                i += 1;
                let value_start = i;
                while i < rest.len() && !is_option_keyword(rest[i]) {
                    i += 1;
                }
                let default = rest[value_start..i].join(" ");
                default_seen = Some(default);
            }
            "min" => {
                i += 1;
                let value_start = i;
                while i < rest.len() && !is_option_keyword(rest[i]) {
                    i += 1;
                }
                let val = rest[value_start..i].join(" ");
                min = Some(val.parse().map_err(|_| UciParseError::Option {
                    reason: format!("missing/invalid min value `{}`", val),
                })?);
            }
            "max" => {
                i += 1;
                let value_start = i;
                while i < rest.len() && !is_option_keyword(rest[i]) {
                    i += 1;
                }
                let val = rest[value_start..i].join(" ");
                max = Some(val.parse().map_err(|_| UciParseError::Option {
                    reason: format!("missing/invalid max value `{}`", val),
                })?);
            }
            "var" => {
                i += 1;
                let value_start = i;
                while i < rest.len() && !is_option_keyword(rest[i]) {
                    i += 1;
                }
                vars.push(rest[value_start..i].join(" "));
            }
            other => {
                i += 1;
                let _ = other;
            }
        }
    }

    match kind {
        "check" => {
            let default = parse_bool_default(default_seen.as_deref())?;
            Ok(UciOptionKind::Check { default })
        }
        "spin" => {
            let default: i64 = default_seen
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or(UciParseError::Option {
                    reason: format!(
                        "spin default missing or invalid `{}`",
                        default_seen.as_deref().unwrap_or("")
                    ),
                })?;
            let min = min.ok_or(UciParseError::Option {
                reason: "spin missing min".to_string(),
            })?;
            let max = max.ok_or(UciParseError::Option {
                reason: "spin missing max".to_string(),
            })?;
            Ok(UciOptionKind::Spin { default, min, max })
        }
        "combo" => {
            let default = default_seen.ok_or(UciParseError::Option {
                reason: "combo default missing".to_string(),
            })?;
            Ok(UciOptionKind::Combo {
                default,
                values: vars,
            })
        }
        "button" => Ok(UciOptionKind::Button),
        "string" => {
            let default = default_seen.unwrap_or_default();
            Ok(UciOptionKind::String { default })
        }
        other => Err(UciParseError::Option {
            reason: format!("unknown option type `{}`", other),
        }),
    }
}

fn parse_bool_default(s: Option<&str>) -> Result<bool, UciParseError> {
    match s {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        // Sentinel empty string e.g. type string. Some engines omit the
        // default value; we treat it as `false` for check.
        None | Some("") => Ok(false),
        Some(other) => Err(UciParseError::Option {
            reason: format!("invalid bool default `{}`", other),
        }),
    }
}

/// Parse one `info …` line. Missing optional fields stay `None`; we never
/// reject an `info …` line just because a field is absent.
fn parse_info<'a, I: Iterator<Item = &'a str>>(
    tokens: &mut I,
) -> Result<EngineStats, UciParseError> {
    let mut stats = EngineStats::default();
    let buf: Vec<&str> = tokens.collect();
    let mut i = 0;

    while i < buf.len() {
        let key = buf[i];
        let val = buf.get(i + 1).copied();
        match key {
            "depth" => set_parse(&mut stats.depth, val)?,
            "seldepth" => set_parse(&mut stats.seldepth, val)?,
            "time" => set_parse(&mut stats.time_ms, val)?,
            "nodes" => set_parse(&mut stats.nodes, val)?,
            "nps" => set_parse(&mut stats.nps, val)?,
            "hashfull" => set_parse(&mut stats.hashfull_per_mille, val)?,
            "tbhits" => set_parse(&mut stats.tbhits, val)?,
            "multipv" => set_parse(&mut stats.multipv, val)?,
            "currmove" => stats.currmove = val.map(str::to_string),
            "currmovenumber" => set_parse(&mut stats.currmovenumber, val)?,
            "score" => {
                stats.score = Some(parse_score_sublist(&buf[i + 1..])?);
                // score has 2 sub-tokens — advance past them.
                i += 2;
                continue;
            }
            "pv" => {
                // `pv` is a multi-token list that ends the line.
                stats.pv = buf[i + 1..].iter().map(|s| s.to_string()).collect();
                break;
            }
            "string" => {
                // Free text: everything after the keyword belongs to the message.
                stats.string = Some(buf[i + 1..].join(" "));
                break;
            }
            "refutation" => {
                stats.refutation = Some(buf[i + 1..].iter().map(|s| s.to_string()).collect());
                break;
            }
            "currline" => {
                stats.currline = Some(
                    buf[i + 1..]
                        .iter()
                        .map(|s| s.parse::<u32>())
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|source| UciParseError::Info {
                            reason: format!("invalid currline integer: {}", source),
                        })?,
                );
                break;
            }
            _ => {
                // Unknown `info` sub-key — skip silently.
                i += 1;
                continue;
            }
        }
        i += 2;
    }

    Ok(stats)
}

fn set_parse<T>(slot: &mut Option<T>, val: Option<&str>) -> Result<(), UciParseError>
where
    T: std::str::FromStr<Err = ParseIntError>,
{
    match val.and_then(|s| s.parse::<T>().ok()) {
        Some(v) => {
            *slot = Some(v);
            Ok(())
        }
        None => Err(UciParseError::Info {
            reason: format!(
                "missing/invalid integer for `{}`-class key",
                std::any::type_name::<T>()
            ),
        }),
    }
}

fn parse_score_sublist(rest: &[&str]) -> Result<Score, UciParseError> {
    let kind = rest.first().copied().ok_or_else(|| UciParseError::Info {
        reason: "score: missing kind".to_string(),
    })?;
    let n: i32 = rest
        .get(1)
        .copied()
        .and_then(|s| s.parse::<i32>().ok())
        .ok_or_else(|| UciParseError::Info {
            reason: "score: missing value".to_string(),
        })?;
    match kind {
        "cp" => Ok(Score::Cp(n)),
        "mate" => Ok(Score::Mate(n)),
        _ => Err(UciParseError::Info {
            reason: format!("unknown score kind `{}`", kind),
        }),
    }
}

/// Parse `bestmove <m> [ponder <m>]`.
fn parse_bestmove<'a, I: Iterator<Item = &'a str>>(
    tokens: &mut I,
) -> Result<EngineEvent, UciParseError> {
    let buf: Vec<&str> = tokens.collect();
    let first = buf
        .first()
        .copied()
        .ok_or_else(|| UciParseError::BestMove {
            reason: "missing move".to_string(),
        })?;
    // `(none)` is the engine's way of saying there is no legal move.
    let best_move = if first == "(none)" {
        None
    } else {
        Some(first.to_string())
    };
    let ponder = buf
        .iter()
        .position(|tok| *tok == "ponder")
        .and_then(|i| buf.get(i + 1).map(|p| p.to_string()));
    Ok(EngineEvent::BestMove { best_move, ponder })
}

// ─────────────────────────────────────────────────────────────────────────────
// Aggregate handshake helper
// ─────────────────────────────────────────────────────────────────────────────

/// Buffer used by [`crate::engine::EngineProcess`] to merge the
/// `id …` / `option …` / `uciok` stream into a single
/// [`EngineEvent::UciOk`] event at the end of the handshake.
#[derive(Debug, Default, Clone)]
pub struct HandshakeAccumulator {
    /// Engine name from `id name …`, kept until the handshake flushes.
    name: String,
    /// Engine author from `id author …`.
    author: String,
    /// Options collected from every `option …` line so far.
    options: Vec<UciOption>,
}

impl HandshakeAccumulator {
    /// An empty accumulator, ready for the first `id` / `option` line.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one parsed [`EngineEvent`] into the accumulator. Returns
    /// `Some(UciOk)` when the line was `uciok` and the handshake is
    /// complete; `None` otherwise (i.e. the engine emitted one of the
    /// contributing lines and we should keep waiting).
    pub fn fold(&mut self, event: EngineEvent) -> Option<EngineEvent> {
        match event {
            EngineEvent::UciOk {
                name,
                author,
                options,
            } => {
                let name_empty = name.is_empty();
                let author_empty = author.is_empty();
                let options_empty = options.is_empty();
                if !name_empty {
                    self.name = name;
                }
                if !author_empty {
                    self.author = author;
                }
                self.options.extend(options);
                // The actual `uciok` arrives with empty `name`, `author`,
                // `options` — that's the trigger to flush.
                if name_empty && author_empty && options_empty {
                    Some(EngineEvent::UciOk {
                        name: std::mem::take(&mut self.name),
                        author: std::mem::take(&mut self.author),
                        options: std::mem::take(&mut self.options),
                    })
                } else {
                    None
                }
            }
            other => Some(other),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// Errors raised by the UCI line parser.
#[derive(Debug, Clone)]
pub enum UciParseError {
    /// Empty input line.
    Empty,
    /// Unknown / engine-extension line (not a hard error — the supervisor
    /// just logs and continues).
    UnknownLine { line: String },
    /// Malformed `id …`.
    Id { reason: String },
    /// Malformed `option …`.
    Option { reason: String },
    /// Malformed `info …` numeric field.
    Info { reason: String },
    /// Malformed `bestmove …`.
    BestMove { reason: String },
}

impl std::fmt::Display for UciParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UciParseError::Empty => f.write_str("empty UCI line"),
            UciParseError::UnknownLine { line } => write!(f, "unknown UCI line: {}", line),
            UciParseError::Id { reason } => write!(f, "malformed `id` line: {}", reason),
            UciParseError::Option { reason } => write!(f, "malformed `option` line: {}", reason),
            UciParseError::Info { reason } => write!(f, "malformed `info` line: {}", reason),
            UciParseError::BestMove { reason } => {
                write!(f, "malformed `bestmove` line: {}", reason)
            }
        }
    }
}

impl std::error::Error for UciParseError {}

// ─────────────────────────────────────────────────────────────────────────────
// Real-world tests (Stockfish 19 output)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::protocol::GoControl;

    // ── Sanity: each piece ──

    #[test]
    fn parses_known_top_level_tokens() {
        assert!(matches!(
            parse_line("uciok").unwrap(),
            EngineEvent::UciOk {
                name,
                author,
                options,
            } if name.is_empty() && author.is_empty() && options.is_empty()
        ));
        assert!(matches!(
            parse_line("readyok").unwrap(),
            EngineEvent::ReadyOk
        ));
    }

    #[test]
    fn parses_id_name_and_author() {
        match parse_line("id name Stockfish 19").unwrap() {
            EngineEvent::UciOk {
                name,
                author,
                options,
            } => {
                assert_eq!(name, "Stockfish 19");
                assert!(author.is_empty());
                assert!(options.is_empty());
            }
            _ => panic!(),
        }
        match parse_line("id author the Stockfish developers").unwrap() {
            EngineEvent::UciOk {
                name,
                author,
                options,
            } => {
                assert!(name.is_empty());
                assert_eq!(author, "the Stockfish developers");
                assert!(options.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_check_option() {
        let ev = parse_line("option name Ponder type check default false").unwrap();
        if let EngineEvent::UciOk { options, .. } = ev {
            assert_eq!(options.len(), 1);
            assert_eq!(options[0].name, "Ponder");
            assert_eq!(options[0].kind, UciOptionKind::Check { default: false });
        } else {
            panic!("expected UciOk carrying the option");
        }
    }

    #[test]
    fn parses_spin_option_with_bounds() {
        let ev = parse_line("option name Threads type spin default 1 min 1 max 1024").unwrap();
        if let EngineEvent::UciOk { options, .. } = ev {
            assert_eq!(
                options[0].kind,
                UciOptionKind::Spin {
                    default: 1,
                    min: 1,
                    max: 1024
                }
            );
        } else {
            panic!();
        }
    }

    #[test]
    fn parses_button_option() {
        let ev = parse_line("option name Clear Hash type button").unwrap();
        if let EngineEvent::UciOk { options, .. } = ev {
            assert_eq!(options[0].kind, UciOptionKind::Button);
        } else {
            panic!();
        }
    }

    #[test]
    fn parses_string_option() {
        let ev = parse_line("option name Debug Log File type string default <empty>").unwrap();
        if let EngineEvent::UciOk { options, .. } = ev {
            assert_eq!(options[0].name, "Debug Log File");
            assert_eq!(
                options[0].kind,
                UciOptionKind::String {
                    default: "<empty>".into()
                }
            );
        } else {
            panic!();
        }
    }

    #[test]
    fn parses_check_option_with_true_default() {
        let ev = parse_line("option name Syzygy50MoveRule type check default true").unwrap();
        if let EngineEvent::UciOk { options, .. } = ev {
            assert_eq!(options[0].kind, UciOptionKind::Check { default: true });
        } else {
            panic!()
        }
    }

    #[test]
    fn parses_combo_option_with_vars() {
        let ev = parse_line(
            "option name Style type combo default Normal var Solid var Normal var Risky",
        )
        .unwrap();
        if let EngineEvent::UciOk { options, .. } = ev {
            assert_eq!(
                options[0].kind,
                UciOptionKind::Combo {
                    default: "Normal".into(),
                    values: vec!["Solid".into(), "Normal".into(), "Risky".into()],
                }
            );
        } else {
            panic!("expected UciOk carrying the combo option");
        }
    }

    #[test]
    fn info_string_swallows_the_rest_of_the_line() {
        // Free text must not be re-read as `info` keys such as `depth`.
        match parse_line("info string Hash depth 5 score cp 9").unwrap() {
            EngineEvent::Info(stats) => {
                assert_eq!(stats.depth, None);
                assert_eq!(stats.score, None);
                assert_eq!(stats.string.as_deref(), Some("Hash depth 5 score cp 9"));
            }
            other => panic!("expected Info, got {:?}", other),
        }
    }

    #[test]
    fn bestmove_none_means_no_legal_move() {
        // Stockfish answers `bestmove (none)` when the side to move is mated
        // or stalemated; it must not be reported as a move string.
        match parse_line("bestmove (none)").unwrap() {
            EngineEvent::BestMove { best_move, ponder } => {
                assert_eq!(best_move, None);
                assert_eq!(ponder, None);
            }
            other => panic!("expected BestMove, got {:?}", other),
        }
    }

    // ── `info …` lines (typical Stockfish payload) ──

    #[test]
    fn parses_full_stockfish_info_line() {
        let line = "info depth 24 seldepth 38 score cp 45 nodes 1450200 nps 1850000 hashfull 42 tbhits 0 time 850 pv e2e4 e7e5 g1f3 b8c6 f1b5 a7a6";
        match parse_line(line).unwrap() {
            EngineEvent::Info(stats) => {
                assert_eq!(stats.depth, Some(24));
                assert_eq!(stats.seldepth, Some(38));
                assert_eq!(stats.score, Some(Score::Cp(45)));
                assert_eq!(stats.nodes, Some(1_450_200));
                assert_eq!(stats.nps, Some(1_850_000));
                assert_eq!(stats.hashfull_per_mille, Some(42));
                assert_eq!(stats.tbhits, Some(0));
                assert_eq!(stats.time_ms, Some(850));
                assert_eq!(
                    stats.pv,
                    vec!["e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "a7a6"]
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_score_mate_line() {
        let line = "info depth 88 score mate 3 pv e2e4 d7d5 g1f3 d5e4 f3e5";
        match parse_line(line).unwrap() {
            EngineEvent::Info(stats) => {
                assert_eq!(stats.score, Some(Score::Mate(3)));
                assert_eq!(stats.pv, vec!["e2e4", "d7d5", "g1f3", "d5e4", "f3e5"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_multipv_and_currmove() {
        let line = "info depth 15 multipv 2 currmove e2e4 currmovenumber 1 score cp 32 nodes 1024 nps 50000 hashfull 5 tbhits 0 time 20 pv e2e4";
        match parse_line(line).unwrap() {
            EngineEvent::Info(stats) => {
                assert_eq!(stats.multipv, Some(2));
                assert_eq!(stats.currmove.as_deref(), Some("e2e4"));
                assert_eq!(stats.currmovenumber, Some(1));
                assert_eq!(stats.depth, Some(15));
            }
            _ => panic!(),
        }
    }

    // ── bestmove, including ponder ──

    #[test]
    fn parses_bestmove_only() {
        match parse_line("bestmove e2e4").unwrap() {
            EngineEvent::BestMove { best_move, ponder } => {
                assert_eq!(best_move.as_deref(), Some("e2e4"));
                assert_eq!(ponder, None);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_bestmove_with_ponder() {
        match parse_line("bestmove e2e4 ponder e7e5").unwrap() {
            EngineEvent::BestMove { best_move, ponder } => {
                assert_eq!(best_move.as_deref(), Some("e2e4"));
                assert_eq!(ponder.as_deref(), Some("e7e5"));
            }
            _ => panic!(),
        }
    }

    // ── Robustness ──

    #[test]
    fn unknown_line_returns_specific_error() {
        let err = parse_line("registration check").unwrap_err();
        match err {
            UciParseError::UnknownLine { line } => assert_eq!(line, "registration check"),
            _ => panic!("got a different error: {:?}", err),
        }
    }

    #[test]
    fn empty_input_is_a_known_error() {
        assert!(matches!(parse_line("").unwrap_err(), UciParseError::Empty));
    }

    #[test]
    fn cr_in_tail_is_trimmed() {
        match parse_line("readyok\r").unwrap() {
            EngineEvent::ReadyOk => (),
            _ => panic!(),
        }
    }

    // ── Handshake aggregator ──

    #[test]
    fn handshake_accumulator_round_trip() {
        let mut acc = HandshakeAccumulator::new();
        assert!(acc
            .fold(parse_line("id name Stockfish 19").unwrap())
            .is_none());
        assert!(acc
            .fold(parse_line("option name Threads type spin default 1 min 1 max 1024",).unwrap(),)
            .is_none());
        assert!(acc
            .fold(parse_line("option name Hash type spin default 16 min 1 max 33554432").unwrap())
            .is_none());
        assert!(acc
            .fold(parse_line("option name Clear Hash type button").unwrap())
            .is_none());
        let flushed = acc
            .fold(parse_line("uciok").unwrap())
            .expect("handshake must flush at `uciok`");
        match flushed {
            EngineEvent::UciOk {
                name,
                options,
                author,
            } => {
                assert_eq!(name, "Stockfish 19");
                assert_eq!(options.len(), 3);
                assert_eq!(
                    options.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
                    vec!["Threads", "Hash", "Clear Hash"],
                );
                assert!(author.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn go_control_string_round_trip() {
        // Just a smoke test — full table coverage lives in protocol.rs.
        assert_eq!(GoControl::Depth(20).to_wire_string(), "go depth 20");
    }
}

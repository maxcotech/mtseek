//! Turning a pattern into a trigram query (Russ Cox, *Regular Expression Matching with a
//! Trigram Index*), as ADR 0010 describes.
//!
//! Every node of the pattern's syntax tree gets an [`Info`]: what its matches are known to
//! look like. Each rule only ever forgets things, never invents them, so the final query is
//! satisfied by every file that can match. The matcher makes the real decision.

use std::collections::BTreeSet;

use mtseek_core::{CaseMode, PatternKind, Query};
use mtseek_index::trigram;
use regex_syntax::ParserBuilder;
use regex_syntax::hir::{Class, Hir, HirKind};

/// A boolean query over trigrams. Every file that can match the pattern satisfies it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Plan {
    /// Every file is a candidate.
    All,
    /// No file can match.
    Nothing,
    /// Files containing this trigram (see [`trigram::key`]).
    Trigram(u32),
    /// Files satisfying all of these.
    And(Vec<Plan>),
    /// Files satisfying any of these.
    Or(Vec<Plan>),
}

impl Plan {
    fn and(items: impl IntoIterator<Item = Plan>) -> Plan {
        let mut out = Vec::new();
        for item in items {
            match item {
                Plan::All => {}
                Plan::Nothing => return Plan::Nothing,
                Plan::And(inner) => out.extend(inner),
                other => out.push(other),
            }
        }
        out.sort();
        out.dedup();
        match out.len() {
            0 => Plan::All,
            1 => out.pop().unwrap_or(Plan::All),
            _ => Plan::And(out),
        }
    }

    fn or(items: impl IntoIterator<Item = Plan>) -> Plan {
        let mut out = Vec::new();
        for item in items {
            match item {
                Plan::All => return Plan::All,
                Plan::Nothing => {}
                Plan::Or(inner) => out.extend(inner),
                other => out.push(other),
            }
        }
        out.sort();
        out.dedup();
        match out.len() {
            0 => Plan::Nothing,
            1 => out.pop().unwrap_or(Plan::Nothing),
            _ => Plan::Or(out),
        }
    }
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Plan::All => f.write_str("ALL"),
            Plan::Nothing => f.write_str("NONE"),
            Plan::Trigram(k) => {
                let bytes = k.to_be_bytes();
                write!(f, "{:?}", String::from_utf8_lossy(&bytes[1..]))
            }
            Plan::And(items) | Plan::Or(items) => {
                let sep = if matches!(self, Plan::And(_)) {
                    " & "
                } else {
                    " | "
                };
                f.write_str("(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(sep)?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str(")")
            }
        }
    }
}

/// The trigram query for `query`.
///
/// Case-sensitive patterns are parsed as they are. Smart case is parsed case-insensitively,
/// a language that contains whatever smart case decides, so the planner needn't repeat its
/// rule. The index folds ASCII case either way. If the pattern can't be analysed, the plan
/// is [`Plan::All`]; the matcher still decides.
pub fn plan(query: &Query) -> Plan {
    let pattern = match query.kind {
        PatternKind::Regex => query.pattern.clone(),
        PatternKind::Literal => regex_syntax::escape(&query.pattern),
    };
    let parsed = ParserBuilder::new()
        .case_insensitive(query.case != CaseMode::Sensitive)
        .utf8(false)
        .build()
        .parse(&pattern);
    match parsed {
        Ok(hir) => info(&hir).into_inexact().query,
        Err(_) => Plan::All,
    }
}

/// Exact sets bigger than this become prefixes and suffixes.
const MAX_EXACT: usize = 64;
/// Prefix and suffix sets bigger than this become "unknown".
const MAX_SET: usize = 64;
/// Sets bigger than this are not turned into trigram queries (the query would be huge).
const MAX_QUERY_SET: usize = 256;
/// Classes with more characters than this are treated as "any character".
const MAX_CLASS: u32 = 16;

/// Byte strings, ASCII-folded like the index.
type Set = BTreeSet<Vec<u8>>;

/// What is known about the strings a pattern node matches (folded).
#[derive(Debug, Clone)]
struct Info {
    /// Exactly the strings it can match, if known and small.
    exact: Option<Set>,
    /// When `exact` is `None`: every match starts with one of these.
    prefix: Set,
    /// When `exact` is `None`: every match ends with one of these.
    suffix: Set,
    /// Every match satisfies this.
    query: Plan,
}

impl Info {
    fn exact(set: Set) -> Self {
        Self {
            exact: Some(set),
            prefix: Set::new(),
            suffix: Set::new(),
            query: Plan::All,
        }
    }

    fn empty_string() -> Self {
        Self::exact(Set::from([Vec::new()]))
    }

    fn anything() -> Self {
        Self {
            exact: None,
            prefix: Set::from([Vec::new()]),
            suffix: Set::from([Vec::new()]),
            query: Plan::All,
        }
    }

    /// The same knowledge without an exact set: prefixes and suffixes are cut to the two
    /// bytes that can form a trigram across a boundary, after the trigrams of the full
    /// strings are added to the query.
    fn into_inexact(self) -> Self {
        let Self {
            exact,
            mut prefix,
            mut suffix,
            mut query,
        } = self;
        if let Some(set) = exact {
            query = Plan::and([query, trigrams_of(&set)]);
            prefix.clone_from(&set);
            suffix = set;
        }
        let query = Plan::and([query, trigrams_of(&prefix), trigrams_of(&suffix)]);
        Self {
            exact: None,
            prefix: shorten(prefix, true),
            suffix: shorten(suffix, false),
            query,
        }
    }
}

fn info(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::empty_string(),
        HirKind::Literal(literal) => Info::exact(Set::from([folded(&literal.0)])),
        HirKind::Class(class) => class_info(class),
        HirKind::Capture(capture) => info(&capture.sub),
        HirKind::Repetition(rep) => match (rep.min, rep.max) {
            (0, Some(1)) => match info(&rep.sub).exact {
                Some(mut set) if set.len() < MAX_EXACT => {
                    set.insert(Vec::new());
                    Info::exact(set)
                }
                _ => Info::anything(),
            },
            (0, _) => Info::anything(),
            (1, Some(1)) => info(&rep.sub),
            // One or more: every match starts, ends with and contains a match of `sub`.
            _ => info(&rep.sub).into_inexact(),
        },
        HirKind::Concat(subs) => subs
            .iter()
            .fold(Info::empty_string(), |acc, sub| concat(acc, info(sub))),
        HirKind::Alternation(subs) => alternation(subs.iter().map(info).collect()),
    }
}

fn class_info(class: &Class) -> Info {
    let mut set = Set::new();
    match class {
        Class::Unicode(c) => {
            let size: u32 = c
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1)
                .sum();
            if size > MAX_CLASS {
                return Info::anything();
            }
            for r in c.ranges() {
                for ch in r.start()..=r.end() {
                    let mut buf = [0u8; 4];
                    set.insert(folded(ch.encode_utf8(&mut buf).as_bytes()));
                }
            }
        }
        Class::Bytes(c) => {
            let size: u32 = c
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1)
                .sum();
            if size > MAX_CLASS {
                return Info::anything();
            }
            for r in c.ranges() {
                for b in r.start()..=r.end() {
                    set.insert(vec![trigram::fold(b)]);
                }
            }
        }
    }
    Info::exact(set)
}

fn concat(a: Info, b: Info) -> Info {
    if let (Some(x), Some(y)) = (&a.exact, &b.exact)
        && x.len() * y.len() <= MAX_EXACT
    {
        return Info::exact(cross(x, y));
    }
    let a_exact = a.exact.clone();
    let b_exact = b.exact.clone();
    let a = a.into_inexact();
    let b = b.into_inexact();
    let prefix = match &a_exact {
        Some(x) => cross(x, &b.prefix),
        None => a.prefix.clone(),
    };
    let suffix = match &b_exact {
        Some(y) => cross(&a.suffix, y),
        None => b.suffix.clone(),
    };
    // A match of `ab` contains some suffix of `a` followed by some prefix of `b`.
    let boundary = trigrams_of(&cross(&a.suffix, &b.prefix));
    Info {
        exact: None,
        prefix,
        suffix,
        query: Plan::and([a.query, b.query, boundary]),
    }
    .into_inexact()
}

fn alternation(infos: Vec<Info>) -> Info {
    if infos.iter().all(|i| i.exact.is_some()) {
        let union: Set = infos
            .iter()
            .flat_map(|i| i.exact.iter().flatten().cloned())
            .collect();
        if union.len() <= MAX_EXACT {
            return Info::exact(union);
        }
    }
    let infos: Vec<Info> = infos.into_iter().map(Info::into_inexact).collect();
    let prefix = infos
        .iter()
        .flat_map(|i| i.prefix.iter().cloned())
        .collect();
    let suffix = infos
        .iter()
        .flat_map(|i| i.suffix.iter().cloned())
        .collect();
    Info {
        exact: None,
        prefix,
        suffix,
        query: Plan::or(infos.into_iter().map(|i| i.query)),
    }
    .into_inexact()
}

/// Every `x` followed by every `y`.
fn cross(x: &Set, y: &Set) -> Set {
    x.iter()
        .flat_map(|a| {
            y.iter().map(move |b| {
                let mut s = a.clone();
                s.extend_from_slice(b);
                s
            })
        })
        .collect()
}

/// A query every string containing one of `set` satisfies.
fn trigrams_of(set: &Set) -> Plan {
    if set.len() > MAX_QUERY_SET {
        return Plan::All;
    }
    Plan::or(set.iter().map(|s| {
        Plan::and(
            s.windows(3)
                .map(|w| Plan::Trigram(trigram::key(w[0], w[1], w[2]))),
        )
    }))
}

/// Keeps the first (or last) two bytes of each string: all a neighbour needs to form a
/// trigram across the boundary. Too many strings means nothing is known.
fn shorten(set: Set, keep_front: bool) -> Set {
    let short: Set = set
        .into_iter()
        .map(|s| {
            if s.len() <= 2 {
                s
            } else if keep_front {
                s[..2].to_vec()
            } else {
                s[s.len() - 2..].to_vec()
            }
        })
        .collect();
    if short.len() > MAX_SET {
        Set::from([Vec::new()])
    } else {
        short
    }
}

fn folded(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|&b| trigram::fold(b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex(p: &str) -> String {
        plan(&Query::regex(p)).to_string()
    }

    #[test]
    fn literals_need_every_trigram() {
        assert_eq!(
            plan(&Query::literal("Paystack")).to_string(),
            r#"("ack" & "ays" & "pay" & "sta" & "tac" & "yst")"#
        );
        assert_eq!(regex("ab"), "ALL");
    }

    #[test]
    fn case_variants_outside_ascii_are_alternatives() {
        // (?i)k also matches U+212A KELVIN SIGN, whose UTF-8 is E2 84 AA.
        for case in [CaseMode::Insensitive, CaseMode::Smart] {
            let p = plan(&Query::regex("kab").with_case(case)).to_string();
            assert!(p.contains("\"kab\"") && p.contains(" | "), "{p}");
        }
        // Case-sensitive: one spelling, folded like the index.
        assert_eq!(regex("KAB"), r#""kab""#);
    }

    #[test]
    fn alternations_and_classes() {
        assert_eq!(
            regex("hello|world"),
            r#"(("ell" & "hel" & "llo") | ("orl" & "rld" & "wor"))"#
        );
        assert_eq!(regex("ab[cd]ef"), {
            // abcef | abdef, folded to exact strings.
            let a = r#"("abc" & "bce" & "cef")"#;
            let b = r#"("abd" & "bde" & "def")"#;
            format!("({a} | {b})")
        });
    }

    #[test]
    fn regexes_keep_their_literal_anchors() {
        let p = regex(r"Connection\w*String");
        for t in ["con", "onn", "ion", "str", "ing"] {
            assert!(p.contains(&format!("\"{t}\"")), "{t} missing from {p}");
        }
        assert_eq!(regex(r"\b\d{3}-\d{3}-\d{4}\b"), "ALL");
        assert_eq!(regex(".*"), "ALL");
        assert_eq!(regex(""), "ALL");
    }

    #[test]
    fn optional_parts_and_repetitions() {
        // colou?r: color | colour.
        let p = regex("colou?r");
        assert!(
            p.contains("\"col\"") && p.contains("\"lor\"") && p.contains("\"our\""),
            "{p}"
        );
        // (abc)+ contains abc.
        assert_eq!(regex("(abc)+"), r#""abc""#);
        // x* says nothing.
        assert_eq!(regex("(abc)*"), "ALL");
    }

    #[test]
    fn impossible_patterns_need_nothing() {
        assert_eq!(regex(r"abc[^\x00-\x{10FFFF}]"), "NONE");
    }

    #[test]
    fn unparseable_patterns_fall_back_to_all() {
        assert_eq!(regex("(unclosed"), "ALL");
    }
}

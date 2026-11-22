//! `--json`: ripgrep's JSON Lines output, message for message (ADR 0015 §3).
//!
//! Per matching file: `begin`, then a `match` or `context` message per line, then `end`.
//! After the search, one `summary`. Fields and their order are ripgrep's, so are the keys
//! of `summary`, which ripgrep writes sorted. Text that isn't UTF-8 is `{"bytes": base64}`.
//! Paths are always `{"text": ...}`: on Windows ripgrep converts them lossily.
//!
//! In `stats`, `matched_lines`, `matches` and `searches_with_match` are ripgrep's numbers.
//! `elapsed`, `bytes_searched`, `bytes_printed` and `searches` describe what mtseek did: a
//! search answered from the index reads only its candidates.

use std::borrow::Cow;
use std::io::{self, Write};
use std::time::Duration;

use mtseek_walk::{FileHit, Summary};
use serde::Serialize;

/// What the JSON messages printed so far add up to, for `summary`.
#[derive(Debug, Default)]
pub(crate) struct Totals {
    bytes_printed: u64,
    matched_lines: u64,
    matches: u64,
}

/// Text as ripgrep's JSON carries it.
#[derive(Serialize)]
#[serde(untagged)]
enum Data<'a> {
    Text { text: Cow<'a, str> },
    Bytes { bytes: String },
}

impl<'a> Data<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Data::Text {
                text: Cow::Borrowed(text),
            },
            Err(_) => Data::Bytes {
                bytes: mtseek_ipc::base64::encode(bytes),
            },
        }
    }

    fn path(path: &'a std::path::Path) -> Self {
        Data::Text {
            text: path.to_string_lossy(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
enum Message<'a> {
    Begin {
        path: Data<'a>,
    },
    Match(LineData<'a>),
    Context(LineData<'a>),
    End {
        path: Data<'a>,
        binary_offset: Option<u64>,
        stats: Stats,
    },
}

#[derive(Serialize)]
struct LineData<'a> {
    path: Data<'a>,
    lines: Data<'a>,
    line_number: u64,
    absolute_offset: u64,
    submatches: Vec<Submatch<'a>>,
}

#[derive(Serialize)]
struct Submatch<'a> {
    #[serde(rename = "match")]
    matched: Data<'a>,
    start: usize,
    end: usize,
}

#[derive(Serialize)]
struct Elapsed {
    secs: u64,
    nanos: u32,
    human: String,
}

impl Elapsed {
    fn new(d: Duration) -> Self {
        Self {
            secs: d.as_secs(),
            nanos: d.subsec_nanos(),
            human: human(d),
        }
    }
}

#[derive(Serialize)]
struct Stats {
    elapsed: Elapsed,
    searches: u64,
    searches_with_match: u64,
    bytes_searched: u64,
    bytes_printed: u64,
    matched_lines: u64,
    matches: u64,
}

/// `summary`, with its keys in the sorted order ripgrep writes them in.
#[derive(Serialize)]
struct SummaryMessage {
    data: SummaryData,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct SummaryData {
    elapsed_total: SortedElapsed,
    stats: SortedStats,
}

#[derive(Serialize)]
struct SortedElapsed {
    human: String,
    nanos: u32,
    secs: u64,
}

impl SortedElapsed {
    fn new(d: Duration) -> Self {
        Self {
            human: human(d),
            nanos: d.subsec_nanos(),
            secs: d.as_secs(),
        }
    }
}

#[derive(Serialize)]
struct SortedStats {
    bytes_printed: u64,
    bytes_searched: u64,
    elapsed: SortedElapsed,
    matched_lines: u64,
    matches: u64,
    searches: u64,
    searches_with_match: u64,
}

/// A duration as ripgrep shows it to people: seconds with six decimals.
fn human(d: Duration) -> String {
    format!("{:0.6}s", d.as_secs_f64())
}

/// Writes one message and its newline; returns the bytes written.
fn write_message(out: &mut impl Write, message: &impl Serialize) -> io::Result<u64> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    out.write_all(&line)?;
    Ok(line.len() as u64)
}

/// Writes one matching file's messages.
pub(crate) fn write_file(
    out: &mut impl Write,
    hit: &FileHit<'_>,
    totals: &mut Totals,
) -> io::Result<()> {
    let path = || Data::path(hit.path);
    let mut printed = write_message(out, &Message::Begin { path: path() })?;
    for line in hit.lines {
        let data = LineData {
            path: path(),
            lines: Data::new(&line.bytes),
            line_number: line.line_number,
            absolute_offset: line.offset,
            submatches: line
                .matches
                .iter()
                .map(|&(start, end)| Submatch {
                    matched: Data::new(line.bytes.get(start..end).unwrap_or_default()),
                    start,
                    end,
                })
                .collect(),
        };
        let message = if line.context {
            Message::Context(data)
        } else {
            Message::Match(data)
        };
        printed += write_message(out, &message)?;
    }
    let stats = Stats {
        elapsed: Elapsed::new(hit.stats.elapsed),
        searches: 1,
        searches_with_match: 1,
        bytes_searched: hit.stats.bytes_searched,
        bytes_printed: printed,
        matched_lines: hit.stats.matched_lines,
        matches: hit.stats.matches,
    };
    write_message(
        out,
        &Message::End {
            path: path(),
            binary_offset: hit.binary_offset,
            stats,
        },
    )?;
    totals.bytes_printed += printed;
    totals.matched_lines += hit.stats.matched_lines;
    totals.matches += hit.stats.matches;
    Ok(())
}

/// Writes `summary` for a search that took `elapsed` in all.
pub(crate) fn write_summary(
    out: &mut impl Write,
    totals: &Totals,
    summary: &Summary,
    elapsed: Duration,
) -> io::Result<()> {
    let message = SummaryMessage {
        data: SummaryData {
            elapsed_total: SortedElapsed::new(elapsed),
            stats: SortedStats {
                bytes_printed: totals.bytes_printed,
                bytes_searched: summary.bytes_searched,
                elapsed: SortedElapsed::new(summary.search_time),
                matched_lines: totals.matched_lines,
                matches: totals.matches,
                searches: summary.files_searched,
                searches_with_match: summary.files_matched,
            },
        },
        kind: "summary",
    };
    write_message(out, &message).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtseek_walk::{FileStats, LineHit};
    use std::path::Path;

    /// The bytes ripgrep 15 prints for `rg --json -A1 needle` on a file `m.txt` holding
    /// "one needle\ntwo\n", with the timing fields set to zero.
    #[test]
    fn messages_are_byte_for_byte_ripgreps() {
        let lines = [
            LineHit {
                matches: vec![(4, 10)],
                ..LineHit::matched(1, 0, b"one needle\n".to_vec())
            },
            LineHit {
                context: true,
                ..LineHit::matched(2, 11, b"two\n".to_vec())
            },
        ];
        let hit = FileHit {
            path: Path::new("m.txt"),
            lines: &lines,
            binary_offset: None,
            stats: FileStats {
                matched_lines: 1,
                matches: 1,
                bytes_searched: 15,
                elapsed: Duration::ZERO,
            },
        };
        let mut out = Vec::new();
        let mut totals = Totals::default();
        write_file(&mut out, &hit, &mut totals).unwrap();
        let summary = Summary {
            files_searched: 1,
            files_matched: 1,
            bytes_searched: 15,
            ..Summary::default()
        };
        write_summary(&mut out, &totals, &summary, Duration::ZERO).unwrap();
        let expected = [
            r#"{"type":"begin","data":{"path":{"text":"m.txt"}}}"#,
            r#"{"type":"match","data":{"path":{"text":"m.txt"},"lines":{"text":"one needle\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"needle"},"start":4,"end":10}]}}"#,
            r#"{"type":"context","data":{"path":{"text":"m.txt"},"lines":{"text":"two\n"},"line_number":2,"absolute_offset":11,"submatches":[]}}"#,
            r#"{"type":"end","data":{"path":{"text":"m.txt"},"binary_offset":null,"stats":{"elapsed":{"secs":0,"nanos":0,"human":"0.000000s"},"searches":1,"searches_with_match":1,"bytes_searched":15,"bytes_printed":360,"matched_lines":1,"matches":1}}}"#,
            r#"{"data":{"elapsed_total":{"human":"0.000000s","nanos":0,"secs":0},"stats":{"bytes_printed":360,"bytes_searched":15,"elapsed":{"human":"0.000000s","nanos":0,"secs":0},"matched_lines":1,"matches":1,"searches":1,"searches_with_match":1}},"type":"summary"}"#,
        ];
        assert_eq!(String::from_utf8(out).unwrap(), expected.join("\n") + "\n");
    }

    #[test]
    fn text_that_is_not_utf8_is_base64() {
        let lines = [LineHit {
            matches: vec![(5, 11)],
            ..LineHit::matched(1, 0, b"caf\xe9 needle\n".to_vec())
        }];
        let hit = FileHit {
            path: Path::new("lat.txt"),
            lines: &lines,
            binary_offset: Some(7),
            stats: FileStats::default(),
        };
        let mut out = Vec::new();
        write_file(&mut out, &hit, &mut Totals::default()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(r#""lines":{"bytes":"Y2Fm6SBuZWVkbGUK"}"#),
            "{text}"
        );
        assert!(text.contains(r#"{"match":{"text":"needle"},"start":5,"end":11}"#));
        assert!(text.contains(r#""binary_offset":7"#));
    }
}

//! Naming series: patterns such as `INV-.YYYY.-.#####` that produce names like
//! `INV-2026-00007`.
//!
//! This module is pure (no I/O): it parses and validates a pattern and renders names. The counter
//! that supplies the number lives in the database (`forgebase-db`, table `fb_series`).
//!
//! # Pattern syntax
//!
//! A pattern is a list of **parts separated by `.`**. A part is one of:
//!
//! | Part | Rendered as |
//! |---|---|
//! | `YYYY` | the 4-digit year |
//! | `YY` | the last two digits of the year |
//! | `MM` | the 2-digit month |
//! | `DD` | the 2-digit day of the month |
//! | a run of `#` (for example `#####`) | the number, zero-padded to the number of `#` |
//! | anything else | itself (literal text) |
//!
//! There must be **exactly one** number part. Parts may not be empty (`..`, a leading or a
//! trailing `.` is an error), a literal may not contain `#` (a part with a `#` must be all `#`)
//! or NUL, and the number part is at most [`MAX_NUMBER_WIDTH`] wide. A number wider than the
//! `#` run is printed in full, never truncated: `INV-.##` renders 123 as `INV-123`. A literal
//! `.` cannot be written (it is the separator).
//!
//! # The counter key
//!
//! Numbers are counted per **prefix**: the rendered text *before* the number part
//! ([`SeriesPattern::prefix`]), for example `INV-2026-`. Date parts after the number part are
//! rendered but do not start a new count. Two patterns that render the same prefix share one
//! counter, whatever their width or suffix, so the width of `#` can be changed later without
//! numbers being reused; the cost is that two DocTypes with the same prefix share a counter (each
//! then sees gaps), so give each DocType its own literal. Because a counter only grows, names
//! of one DocType do not repeat. The exception is a pattern changed to a different prefix: prefix
//! `A-` with number 11 and prefix `A-1` with number 1 both render `A-11`, and the unique `name`
//! constraint then refuses the insert (a last resort, not a check).
//!
//! ```
//! use forgebase_core::series::SeriesPattern;
//! use time::{Date, Month};
//!
//! let pattern = SeriesPattern::parse("INV-.YYYY.-.#####")?;
//! let date = Date::from_calendar_date(2026, Month::October, 29)?;
//! assert_eq!(pattern.prefix(date), "INV-2026-");
//! assert_eq!(pattern.render(date, 7), "INV-2026-00007");
//! assert_eq!(pattern.render(date, 1_234_567), "INV-2026-1234567"); // wider than `#####`
//! assert!(SeriesPattern::parse("INV-.####.-.####").is_err()); // two number parts
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;

use time::Date;

/// The widest number part (`#` run) allowed: a `u64` has at most 20 digits.
pub const MAX_NUMBER_WIDTH: usize = 20;

/// Why a series pattern was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SeriesError {
    /// The pattern is the empty string.
    #[error("must not be empty")]
    Empty,
    /// A part is empty (two dots in a row, or a leading or trailing dot).
    #[error("has an empty part (a leading, trailing or doubled `.`)")]
    EmptyPart,
    /// A literal part mixes `#` with other characters.
    #[error("part `{part}` mixes `#` with other characters (the number part must be only `#`)")]
    MixedNumber {
        /// The rejected part.
        part: String,
    },
    /// A part contains a NUL character, which PostgreSQL text cannot store.
    #[error("must not contain a NUL character")]
    Nul,
    /// There is no number part.
    #[error("needs exactly one number part such as `#####`, found none")]
    NoNumber,
    /// There is more than one number part.
    #[error("needs exactly one number part such as `#####`, found {count}")]
    MultipleNumbers {
        /// How many number parts there are.
        count: usize,
    },
    /// The number part is wider than [`MAX_NUMBER_WIDTH`].
    #[error("number part is {width} wide, the maximum is {MAX_NUMBER_WIDTH}")]
    NumberTooWide {
        /// The number of `#`.
        width: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Year4,
    Year2,
    Month,
    Day,
    /// The number, zero-padded to this width.
    Number(usize),
}

/// A validated naming series pattern, see the [module documentation](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesPattern {
    source: String,
    parts: Vec<Part>,
    /// Index of the number part in `parts`.
    number: usize,
}

impl SeriesPattern {
    /// Parses and validates a pattern.
    pub fn parse(pattern: &str) -> Result<Self, SeriesError> {
        if pattern.is_empty() {
            return Err(SeriesError::Empty);
        }
        if pattern.contains('\0') {
            return Err(SeriesError::Nul);
        }
        let mut parts = Vec::new();
        let mut numbers = Vec::new();
        for (i, text) in pattern.split('.').enumerate() {
            let part = match text {
                "" => return Err(SeriesError::EmptyPart),
                "YYYY" => Part::Year4,
                "YY" => Part::Year2,
                "MM" => Part::Month,
                "DD" => Part::Day,
                _ if text.chars().all(|c| c == '#') => {
                    // Counted in characters; `#` is one byte, so the lengths agree.
                    numbers.push(i);
                    Part::Number(text.len())
                }
                _ if text.contains('#') => {
                    return Err(SeriesError::MixedNumber {
                        part: text.to_owned(),
                    });
                }
                _ => Part::Literal(text.to_owned()),
            };
            parts.push(part);
        }
        let number = match numbers.as_slice() {
            [] => return Err(SeriesError::NoNumber),
            [one] => *one,
            many => return Err(SeriesError::MultipleNumbers { count: many.len() }),
        };
        if let Part::Number(width) = parts[number]
            && width > MAX_NUMBER_WIDTH
        {
            return Err(SeriesError::NumberTooWide { width });
        }
        Ok(Self {
            source: pattern.to_owned(),
            parts,
            number,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The rendered text before the number part, with the date parts replaced by `date`. This
    /// is the key of the counter (see the [module documentation](self)).
    pub fn prefix(&self, date: Date) -> String {
        let mut out = String::new();
        for part in &self.parts[..self.number] {
            push_part(&mut out, part, date, 0);
        }
        out
    }

    /// The name for `number`: the prefix, the number zero-padded to the width of the number
    /// part (a wider number is printed in full) and the rendered parts after it.
    pub fn render(&self, date: Date, number: u64) -> String {
        let mut out = String::new();
        for part in &self.parts {
            push_part(&mut out, part, date, number);
        }
        out
    }
}

fn push_part(out: &mut String, part: &Part, date: Date, number: u64) {
    use fmt::Write as _;
    // Writing to a `String` cannot fail.
    let _ = match part {
        Part::Literal(text) => out.write_str(text),
        Part::Year4 => write!(out, "{:04}", date.year()),
        Part::Year2 => write!(out, "{:02}", date.year().rem_euclid(100)),
        Part::Month => write!(out, "{:02}", u8::from(date.month())),
        Part::Day => write!(out, "{:02}", date.day()),
        Part::Number(width) => write!(out, "{number:0width$}"),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn date(y: i32, m: Month, d: u8) -> Date {
        Date::from_calendar_date(y, m, d).unwrap()
    }

    #[test]
    fn renders_the_documented_example() {
        let p = SeriesPattern::parse("INV-.YYYY.-.#####").unwrap();
        let d = date(2026, Month::October, 29);
        assert_eq!(p.render(d, 7), "INV-2026-00007");
        assert_eq!(p.prefix(d), "INV-2026-");
        assert_eq!(p.as_str(), "INV-.YYYY.-.#####");
    }

    #[test]
    fn renders_every_date_part_zero_padded() {
        let p = SeriesPattern::parse("X.YY.MM.DD.#.-.YYYY").unwrap();
        let d = date(2005, Month::March, 4);
        assert_eq!(p.prefix(d), "X050304");
        assert_eq!(p.render(d, 9), "X0503049-2005");
        let d = date(2026, Month::December, 31);
        assert_eq!(p.render(d, 10), "X26123110-2026");
    }

    #[test]
    fn a_number_wider_than_the_hashes_is_printed_in_full() {
        let p = SeriesPattern::parse("N-.##").unwrap();
        let d = date(2026, Month::January, 1);
        assert_eq!(p.render(d, 5), "N-05");
        assert_eq!(p.render(d, 99), "N-99");
        assert_eq!(p.render(d, 100), "N-100");
        assert_eq!(p.render(d, u64::MAX), "N-18446744073709551615");
    }

    #[test]
    fn the_widest_number_part_is_accepted_and_padded() {
        let p = SeriesPattern::parse(&"#".repeat(MAX_NUMBER_WIDTH)).unwrap();
        let d = date(2026, Month::January, 1);
        assert_eq!(
            p.render(d, 1),
            format!("{}1", "0".repeat(MAX_NUMBER_WIDTH - 1))
        );
        assert_eq!(p.prefix(d), "");
    }

    #[test]
    fn the_prefix_is_the_text_before_the_number_only() {
        let p = SeriesPattern::parse("A.#.-.YYYY").unwrap();
        let d = date(2026, Month::June, 1);
        assert_eq!(p.prefix(d), "A");
        assert_eq!(p.render(d, 3), "A3-2026");
        // The same prefix with another width or suffix shares the key.
        let q = SeriesPattern::parse("A.#####").unwrap();
        assert_eq!(q.prefix(d), p.prefix(d));
    }

    #[test]
    fn literals_that_look_like_tokens_are_literals_unless_exact() {
        let p = SeriesPattern::parse("yyyy.YYY.Mm.#").unwrap();
        assert_eq!(p.render(date(2026, Month::June, 1), 1), "yyyyYYYMm1");
    }

    #[test]
    fn bad_patterns_are_rejected() {
        let cases: Vec<(&str, SeriesError)> = vec![
            ("", SeriesError::Empty),
            ("INV-", SeriesError::NoNumber),
            ("YYYY", SeriesError::NoNumber),
            ("A-.####.-.##", SeriesError::MultipleNumbers { count: 2 }),
            (".#", SeriesError::EmptyPart),
            ("#.", SeriesError::EmptyPart),
            ("A..#", SeriesError::EmptyPart),
            (".", SeriesError::EmptyPart),
            (
                "A#.##",
                SeriesError::MixedNumber {
                    part: "A#".to_owned(),
                },
            ),
            ("A\0.##", SeriesError::Nul),
            (
                "#####################",
                SeriesError::NumberTooWide { width: 21 },
            ),
        ];
        for (pattern, expected) in cases {
            assert_eq!(
                SeriesPattern::parse(pattern),
                Err(expected),
                "pattern {pattern:?}"
            );
        }
    }
}

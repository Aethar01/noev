//! Printing diagnostics.
//!
//! Two formats: `human`, with source excerpts and colour when the terminal supports it, and
//! `short`, one line per diagnostic, which the UI test suite reads.

use std::io::Write;

use anstyle::{AnsiColor, Effects, Style};
use noev_syntax::{Diagnostic, Severity, Span};

use crate::MessageFormat;

const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
const WARNING: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
const NOTE: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
const BOLD: Style = Style::new().effects(Effects::BOLD);

/// Line starts of a source file, for turning byte offsets into line and column numbers.
pub struct Lines<'a> {
    source: &'a str,
    starts: Vec<u32>,
}

impl<'a> Lines<'a> {
    pub fn new(source: &'a str) -> Self {
        let starts = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i as u32 + 1))
            .collect();
        Lines { source, starts }
    }

    /// The zero-based line containing `offset`.
    fn line_of(&self, offset: u32) -> usize {
        self.starts.partition_point(|&start| start <= offset) - 1
    }

    /// One-based line and column (counted in characters) of `offset`.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = self.line_of(offset);
        let start = self.starts[line] as usize;
        let column = self.source[start..offset as usize].chars().count() + 1;
        (line + 1, column)
    }

    /// The text of the zero-based line `line`, without its line break.
    fn text(&self, line: usize) -> &'a str {
        let start = self.starts[line] as usize;
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.source.len(), |&next| next as usize - 1);
        self.source[start..end].trim_end_matches('\r')
    }
}

/// Prints `diagnostics` for the file at `path` to stderr.
pub fn print(path: &str, source: &str, diagnostics: &[Diagnostic], format: MessageFormat) {
    let lines = Lines::new(source);
    let mut out = anstream::stderr().lock();
    for diagnostic in diagnostics {
        let result = match format {
            MessageFormat::Short => short(&mut out, path, &lines, diagnostic),
            MessageFormat::Human => human(&mut out, path, &lines, diagnostic),
        };
        // Nothing useful can be done if stderr is closed.
        let _ = result;
    }
}

fn severity(diagnostic: &Diagnostic) -> (&'static str, Style) {
    match diagnostic.severity {
        Severity::Error => ("error", ERROR),
        Severity::Warning => ("warning", WARNING),
    }
}

fn heading(diagnostic: &Diagnostic) -> String {
    let (name, _) = severity(diagnostic);
    match diagnostic.code {
        Some(code) => format!("{name}[{code}]"),
        None => name.to_string(),
    }
}

/// `FILE:LINE:COLUMN: error[CODE]: message`
fn short(out: &mut impl Write, path: &str, lines: &Lines, d: &Diagnostic) -> std::io::Result<()> {
    let (line, column) = lines.line_col(d.span.start);
    writeln!(out, "{path}:{line}:{column}: {}: {}", heading(d), d.message)
}

fn human(out: &mut impl Write, path: &str, lines: &Lines, d: &Diagnostic) -> std::io::Result<()> {
    let (_, style) = severity(d);
    writeln!(
        out,
        "{style}{}{style:#}{BOLD}: {}{BOLD:#}",
        heading(d),
        d.message
    )?;

    let (line, column) = lines.line_col(d.span.start);
    let last_line = std::iter::once(d.span)
        .chain(d.labels.iter().map(|(span, _)| *span))
        .map(|span| lines.line_of(span.start) + 1)
        .max()
        .unwrap_or(line);
    let gutter = last_line.to_string().len();
    let pad = " ".repeat(gutter);

    writeln!(out, "{pad}{NOTE}-->{NOTE:#} {path}:{line}:{column}")?;
    writeln!(out, "{pad} {NOTE}|{NOTE:#}")?;

    // Every underline, grouped by line, so marks on the same line share one excerpt.
    let mut marks = vec![Mark {
        span: d.span,
        symbol: '^',
        style,
        message: "",
    }];
    marks.extend(d.labels.iter().map(|(span, message)| Mark {
        span: *span,
        symbol: '-',
        style: NOTE,
        message,
    }));
    marks.sort_by_key(|m| (lines.line_of(m.span.start), m.span.start));
    for group in marks.chunk_by(|a, b| lines.line_of(a.span.start) == lines.line_of(b.span.start)) {
        excerpt(out, lines, gutter, group)?;
    }
    if let Some(help) = &d.help {
        writeln!(out, "{pad} {NOTE}= help:{NOTE:#} {help}")?;
    }
    writeln!(out)
}

struct Mark<'a> {
    span: Span,
    symbol: char,
    style: Style,
    message: &'a str,
}

/// One source line with its marks underlined below it, labels at the end of the row.
fn excerpt(
    out: &mut impl Write,
    lines: &Lines,
    gutter: usize,
    marks: &[Mark],
) -> std::io::Result<()> {
    let line = lines.line_of(marks[0].span.start);
    let text = lines.text(line);
    let line_start = lines.starts[line];
    let line_end = line_start + text.len() as u32;
    writeln!(out, "{NOTE}{:>gutter$} |{NOTE:#} {text}", line + 1)?;

    let mut row = String::new();
    let mut column = 0;
    let mut messages = Vec::new();
    for mark in marks {
        let start = mark.span.start.min(line_end);
        // Underline to the end of the span or the line, whichever comes first, at least one wide.
        let end = mark.span.end.min(line_end).max(start);
        let before = &lines.source[line_start as usize..start as usize];
        let target = before.chars().count();
        if target < column {
            continue; // Overlaps an earlier mark; its label still prints below.
        }
        // Keep tabs so the underline lines up with tab-indented code.
        let indent: String = before
            .chars()
            .map(|c| if c == '\t' { '\t' } else { ' ' })
            .collect();
        row.extend(indent.chars().skip(column));
        let width = lines.source[start as usize..end as usize]
            .chars()
            .count()
            .max(1);
        let style = mark.style;
        row.push_str(&format!(
            "{style}{}{style:#}",
            mark.symbol.to_string().repeat(width)
        ));
        column = target + width;
        if !mark.message.is_empty() {
            messages.push((indent, format!("{style}{}{style:#}", mark.message)));
        }
    }
    let pad = " ".repeat(gutter);
    // A lone label sits after its mark; with several marks, each label goes under its own.
    match messages.as_slice() {
        [(_, message)] if marks.len() == 1 => {
            writeln!(out, "{pad} {NOTE}|{NOTE:#} {row} {message}")
        }
        _ => {
            writeln!(out, "{pad} {NOTE}|{NOTE:#} {row}")?;
            for (indent, message) in &messages {
                writeln!(out, "{pad} {NOTE}|{NOTE:#} {indent}{message}")?;
            }
            Ok(())
        }
    }
}

/// Prints how often each result occurred, most frequent first, with a bar for each. Shows the
/// `limit` most frequent (all when `limit` is 0) and summarises the rest.
pub fn histogram(
    counts: &noev_runtime::Counts,
    limit: usize,
    elapsed: std::time::Duration,
    seed: u64,
) {
    const BAR: usize = 40;
    let mut out = anstream::stdout().lock();
    let width = counts
        .results
        .iter()
        .map(|(r, _)| r.chars().count())
        .max()
        .unwrap_or(0);
    let digits = counts.shots.to_string().len();
    let most = counts.results.first().map_or(1, |(_, n)| *n).max(1);
    let shown = if limit == 0 {
        counts.results.len()
    } else {
        limit.min(counts.results.len())
    };
    for (result, count) in &counts.results[..shown] {
        let share = *count as f64 / counts.shots as f64;
        let bar = "█".repeat((count * BAR).div_ceil(most));
        let _ = writeln!(
            out,
            "{BOLD}{result:<width$}{BOLD:#}  {count:>digits$}  {:>5.1}%  {NOTE}{bar}{NOTE:#}",
            share * 100.0
        );
    }
    let hidden = &counts.results[shown..];
    if !hidden.is_empty() {
        let hidden_shots: usize = hidden.iter().map(|(_, n)| n).sum();
        let _ = writeln!(
            out,
            "{NOTE}… and {} more result{} ({hidden_shots} shots); use --limit 0 to show all{NOTE:#}",
            hidden.len(),
            if hidden.len() == 1 { "" } else { "s" }
        );
    }
    let _ = writeln!(
        out,
        "\n{} shots, {} distinct result{}, in {:.1?} (seed {seed})",
        counts.shots,
        counts.results.len(),
        if counts.results.len() == 1 { "" } else { "s" },
        elapsed
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column() {
        let lines = Lines::new("ab\ncdé\nf");
        assert_eq!(lines.line_col(0), (1, 1));
        assert_eq!(lines.line_col(4), (2, 2));
        assert_eq!(lines.line_col(8), (3, 1));
        assert_eq!(lines.text(1), "cdé");
    }
}

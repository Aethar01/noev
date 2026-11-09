//! Printing diagnostics.
//!
//! Two formats: `human`, with source excerpts and colour when the terminal supports it, and
//! `short`, one line per diagnostic, which the UI test suite reads.

use std::io::Write;

use anstyle::{AnsiColor, Effects, Style};
use noev_syntax::{Diagnostic, Severity, SourceFile, SourceMap, Span};

use crate::MessageFormat;

const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
const WARNING: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
const NOTE: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
const BOLD: Style = Style::new().effects(Effects::BOLD);

/// Line starts of a source file, for turning offsets into line and column numbers.
pub struct Lines<'a> {
    file: &'a SourceFile,
    /// Where each line starts, as positions in the file's text.
    starts: Vec<u32>,
}

impl<'a> Lines<'a> {
    pub fn new(file: &'a SourceFile) -> Self {
        let starts = std::iter::once(0)
            .chain(file.text.match_indices('\n').map(|(i, _)| i as u32 + 1))
            .collect();
        Lines { file, starts }
    }

    /// The position in the file's text of `offset`, which is in this file.
    fn local(&self, offset: u32) -> u32 {
        offset - self.file.start
    }

    /// The zero-based line containing `offset`.
    fn line_of(&self, offset: u32) -> usize {
        let local = self.local(offset);
        self.starts.partition_point(|&start| start <= local) - 1
    }

    /// One-based line and column (counted in characters) of `offset`.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = self.line_of(offset);
        let start = self.starts[line] as usize;
        let column = self.file.text[start..self.local(offset) as usize]
            .chars()
            .count()
            + 1;
        (line + 1, column)
    }

    /// The text of the zero-based line `line`, without its line break.
    fn text(&self, line: usize) -> &'a str {
        let start = self.starts[line] as usize;
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.file.text.len(), |&next| next as usize - 1);
        self.file.text[start..end].trim_end_matches('\r')
    }
}

/// The line tables of a source map's files, each made the first time it is needed.
struct Files<'a> {
    map: &'a SourceMap,
    lines: Vec<Option<Lines<'a>>>,
}

impl<'a> Files<'a> {
    fn new(map: &'a SourceMap) -> Self {
        Files {
            map,
            lines: map.files().iter().map(|_| None).collect(),
        }
    }

    /// The index and lines of the file containing `offset`.
    fn of(&mut self, offset: u32) -> usize {
        let index = self.map.find(offset).index();
        let map = self.map;
        self.lines[index].get_or_insert_with(|| Lines::new(&map.files()[index]));
        index
    }

    fn get(&self, index: usize) -> &Lines<'a> {
        self.lines[index].as_ref().expect("lines are made by `of`")
    }
}

/// Prints `diagnostics`, whose spans are in `sources`, to stderr.
pub fn print(sources: &SourceMap, diagnostics: &[Diagnostic], format: MessageFormat) {
    let mut files = Files::new(sources);
    let mut out = anstream::stderr().lock();
    for diagnostic in diagnostics {
        let result = match format {
            MessageFormat::Short => short(&mut out, &mut files, diagnostic),
            MessageFormat::Human => human(&mut out, &mut files, diagnostic),
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

/// `path:line:column` of `offset`.
fn location(files: &mut Files, offset: u32) -> String {
    let file = files.of(offset);
    let lines = files.get(file);
    let (line, column) = lines.line_col(offset);
    format!("{}:{line}:{column}", lines.file.name)
}

/// `FILE:LINE:COLUMN: error[CODE]: message`
fn short(out: &mut impl Write, files: &mut Files, d: &Diagnostic) -> std::io::Result<()> {
    let at = location(files, d.span.start);
    writeln!(out, "{at}: {}: {}", heading(d), d.message)
}

fn human(out: &mut impl Write, files: &mut Files, d: &Diagnostic) -> std::io::Result<()> {
    let (_, style) = severity(d);
    writeln!(
        out,
        "{style}{}{style:#}{BOLD}: {}{BOLD:#}",
        heading(d),
        d.message
    )?;

    // Every underline, with its file, grouped by file and then by line, so marks on the same
    // line share one excerpt. The diagnostic's own file comes first.
    let primary = files.of(d.span.start);
    let mut marks = vec![(
        primary,
        Mark {
            span: d.span,
            symbol: '^',
            style,
            message: "",
        },
    )];
    for (span, message) in &d.labels {
        let mark = Mark {
            span: *span,
            symbol: '-',
            style: NOTE,
            message,
        };
        marks.push((files.of(span.start), mark));
    }
    let gutter = marks
        .iter()
        .map(|(file, m)| files.get(*file).line_of(m.span.start) + 1)
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let pad = " ".repeat(gutter);
    marks.sort_by_key(|(file, m)| {
        (
            *file != primary,
            *file,
            files.get(*file).line_of(m.span.start),
            m.span.start,
        )
    });

    writeln!(
        out,
        "{pad}{NOTE}-->{NOTE:#} {}",
        location(files, d.span.start)
    )?;
    writeln!(out, "{pad} {NOTE}|{NOTE:#}")?;
    for in_file in marks.chunk_by(|a, b| a.0 == b.0) {
        let (file, first) = &in_file[0];
        let lines = files.get(*file);
        if *file != primary {
            let (line, column) = lines.line_col(first.span.start);
            let name = &lines.file.name;
            writeln!(out, "{pad}{NOTE}:::{NOTE:#} {name}:{line}:{column}")?;
            writeln!(out, "{pad} {NOTE}|{NOTE:#}")?;
        }
        let in_file: Vec<&Mark> = in_file.iter().map(|(_, mark)| mark).collect();
        let same_line =
            |a: &&Mark, b: &&Mark| lines.line_of(a.span.start) == lines.line_of(b.span.start);
        for group in in_file.chunk_by(same_line) {
            excerpt(out, lines, gutter, group)?;
        }
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
    marks: &[&Mark],
) -> std::io::Result<()> {
    let line = lines.line_of(marks[0].span.start);
    let text = lines.text(line);
    let source = lines.file.text.as_str();
    let line_start = lines.starts[line];
    let line_end = line_start + text.len() as u32;
    writeln!(out, "{NOTE}{:>gutter$} |{NOTE:#} {text}", line + 1)?;

    let mut row = String::new();
    let mut column = 0;
    let mut messages = Vec::new();
    for mark in marks {
        let start = lines.local(mark.span.start).min(line_end);
        // Underline to the end of the span or the line, whichever comes first, at least one wide.
        let end = lines.local(mark.span.end).min(line_end).max(start);
        let before = &source[line_start as usize..start as usize];
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
        let width = source[start as usize..end as usize].chars().count().max(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column() {
        let map = SourceMap::single("a.nv", "ab\ncdé\nf");
        let lines = Lines::new(&map.files()[0]);
        assert_eq!(lines.line_col(0), (1, 1));
        assert_eq!(lines.line_col(4), (2, 2));
        assert_eq!(lines.line_col(8), (3, 1));
        assert_eq!(lines.text(1), "cdé");
    }

    #[test]
    fn spans_in_a_later_file() {
        let mut map = SourceMap::default();
        map.add("a.nv", "fn a() {}\n").unwrap();
        let b = map.add("b.nv", "x\ny z\n").unwrap();
        let start = map.file(b).start;
        let d = Diagnostic::error(Span::new(start + 4, start + 5), "bad z")
            .with_label(Span::new(1, 2), "defined here");
        let mut files = Files::new(&map);
        let mut short_out = Vec::new();
        short(&mut short_out, &mut files, &d).unwrap();
        assert_eq!(
            String::from_utf8(short_out).unwrap(),
            "b.nv:2:3: error: bad z\n"
        );
        let mut human_out = Vec::new();
        human(&mut human_out, &mut files, &d).unwrap();
        let text = anstream::adapter::strip_str(&String::from_utf8(human_out).unwrap()).to_string();
        assert_eq!(
            text,
            "error: bad z\n \
             --> b.nv:2:3\n  \
             |\n\
             2 | y z\n  \
             |   ^\n \
             ::: a.nv:1:2\n  \
             |\n\
             1 | fn a() {}\n  \
             |  - defined here\n\n"
        );
    }
}

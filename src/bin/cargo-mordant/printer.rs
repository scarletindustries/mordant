//! How `cargo mordant` prints its own messages, in the form
//! `--message-format` asked for: rendered on stderr for `human` and `short`,
//! and as a `compiler-message` on stdout for `json`, so that tools reading
//! cargo's JSON get them with everything else.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};
use serde_json::{Value as Json, json};

use crate::Output;
use crate::records::Note;
use crate::unused_pub::RunUnit;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Severity {
    Warning,
    Error,
}

impl Severity {
    /// The severity of a lint level as rustc spells it.
    pub(crate) fn of(level: &str) -> Severity {
        match level {
            "deny" | "forbid" => Severity::Error,
            _ => Severity::Warning,
        }
    }

    fn level(self) -> Level<'static> {
        match self {
            Severity::Warning => Level::WARNING,
            Severity::Error => Level::ERROR,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// One finding: the message, where it is, and what goes under it.
pub(crate) struct Located<'a> {
    pub(crate) message: String,
    /// Relative to the workspace root, or absolute.
    pub(crate) file: &'a str,
    /// The finding's place in `file`, as byte offsets.
    pub(crate) lo: u32,
    pub(crate) hi: u32,
    /// The lint's name, shown as the code in JSON. None for a warning that
    /// is not the lint's own.
    pub(crate) code: Option<&'a str>,
    pub(crate) help: &'a str,
    pub(crate) notes: &'a [Note],
    /// The unit a JSON message is attributed to.
    pub(crate) unit: &'a RunUnit,
}

/// `a`, `a and b`, `a, b and c`.
pub(crate) fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Prints one error that is about no one item: on stderr in human and short
/// output, and as a `compiler-message` on stdout, attributed to `unit`, in
/// JSON output.
pub(crate) fn print_error(
    root: &Path,
    output: &Output,
    styled: bool,
    unit: Option<&RunUnit>,
    message: String,
) {
    Printer::new(root, output, styled, unit).plain(Severity::Error, message);
}

/// Prints findings in the form `--message-format` asked for.
pub(crate) struct Printer<'a> {
    root: &'a Path,
    output: &'a Output,
    renderer: Renderer,
    /// For `rendered` in JSON: plain unless the format asked for colour.
    json_renderer: Renderer,
    /// What a message not about any one item is attributed to in JSON.
    fallback: Option<&'a RunUnit>,
    sources: HashMap<PathBuf, Option<String>>,
    warnings: usize,
    errors: usize,
}

impl<'a> Printer<'a> {
    pub(crate) fn new(
        root: &'a Path,
        output: &'a Output,
        styled: bool,
        fallback: Option<&'a RunUnit>,
    ) -> Printer<'a> {
        let short = matches!(output, Output::Short);
        let renderer = if styled {
            Renderer::styled()
        } else {
            Renderer::plain()
        }
        .short_message(short);
        let (ansi, short_json) = match output {
            Output::Json(value) => (
                value.contains("rendered-ansi"),
                value.contains("diagnostic-short"),
            ),
            Output::Human | Output::Short => (false, false),
        };
        let json_renderer = if ansi {
            Renderer::styled()
        } else {
            Renderer::plain()
        }
        .short_message(short_json);
        Printer {
            root,
            output,
            renderer,
            json_renderer,
            fallback,
            sources: HashMap::new(),
            warnings: 0,
            errors: 0,
        }
    }

    fn source(&mut self, file: &str) -> Option<String> {
        let path = self.root.join(file);
        self.sources
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(&path).ok())
            .clone()
    }

    pub(crate) fn finding(&mut self, finding: Located<'_>, severity: Severity) {
        let Located {
            message,
            file,
            lo,
            hi,
            code,
            help,
            notes,
            unit,
        } = finding;
        let Some(source) = self.source(file) else {
            self.plain(
                Severity::Warning,
                format!("mordant: {message} ({file} could not be read)"),
            );
            return;
        };
        let sources: Vec<Option<String>> = notes
            .iter()
            .map(|n| n.at.as_ref().and_then(|(file, ..)| self.source(file)))
            .collect();
        let report = |renderer: &Renderer| {
            let span = lo as usize..hi as usize;
            let mut main = severity.level().primary_title(message.as_str()).element(
                Snippet::source(source.as_str())
                    .path(file)
                    .annotation(AnnotationKind::Primary.span(span)),
            );
            main = main.element(Level::HELP.message(help));
            let mut groups = Vec::new();
            for (note, text) in notes.iter().zip(&sources) {
                match (&note.at, text) {
                    (Some((file, lo, hi)), Some(text)) => groups.push(
                        Group::with_title(Level::NOTE.secondary_title(note.message.as_str()))
                            .element(
                                Snippet::source(text.as_str())
                                    .path(file.as_str())
                                    .annotation(
                                        AnnotationKind::Primary.span(*lo as usize..*hi as usize),
                                    ),
                            ),
                    ),
                    _ => {
                        let level = if note.help { Level::HELP } else { Level::NOTE };
                        main = main.element(level.message(note.message.as_str()));
                    }
                }
            }
            let mut report = vec![main];
            report.extend(groups);
            renderer.render(&report)
        };
        match severity {
            Severity::Warning => self.warnings += 1,
            Severity::Error => self.errors += 1,
        }
        if !matches!(self.output, Output::Json(_)) {
            eprintln!("{}\n", report(&self.renderer));
            return;
        }
        let rendered = report(&self.json_renderer);
        let mut children = vec![child("help", help, Vec::new())];
        for (note, text) in notes.iter().zip(&sources) {
            let spans = match (&note.at, text) {
                (Some((file, lo, hi)), Some(text)) => vec![span(file, text, *lo, *hi)],
                _ => Vec::new(),
            };
            let level = if note.help { "help" } else { "note" };
            children.push(child(level, &note.message, spans));
        }
        let diagnostic = json!({
            "$message_type": "diagnostic",
            "message": message,
            "code": code.map(|code| json!({ "code": code, "explanation": null })),
            "level": severity.word(),
            "spans": [span(file, &source, lo, hi)],
            "children": children,
            "rendered": rendered,
        });
        emit(unit, diagnostic);
    }

    /// A message about no one item: `mordant: ...`, with no code and no span.
    pub(crate) fn plain(&mut self, severity: Severity, message: String) {
        let report = |renderer: &Renderer| {
            renderer.render(&[Group::with_title(
                severity.level().primary_title(message.as_str()),
            )])
        };
        match (self.output, self.fallback) {
            (Output::Json(_), Some(unit)) => {
                let rendered = report(&self.json_renderer);
                let diagnostic = json!({
                    "$message_type": "diagnostic",
                    "message": message,
                    "code": null,
                    "level": severity.word(),
                    "spans": [],
                    "children": [],
                    "rendered": rendered,
                });
                emit(unit, diagnostic);
            }
            _ => eprintln!("{}\n", report(&self.renderer)),
        }
    }

    /// The closing count for `lint`, as rustc prints its own; whether any
    /// finding was an error.
    pub(crate) fn tally(&self, lint: &str) -> bool {
        if matches!(self.output, Output::Json(_)) || self.warnings + self.errors == 0 {
            return self.errors > 0;
        }
        let plural = |n: usize, what: &str| {
            if n == 1 {
                format!("1 {what}")
            } else {
                format!("{n} {what}s")
            }
        };
        let summary = match (self.errors, self.warnings) {
            (0, w) => {
                Level::WARNING.primary_title(format!("`{lint}` generated {}", plural(w, "warning")))
            }
            (e, 0) => {
                Level::ERROR.primary_title(format!("`{lint}` generated {}", plural(e, "error")))
            }
            (e, w) => Level::ERROR.primary_title(format!(
                "`{lint}` generated {} and {}",
                plural(e, "error"),
                plural(w, "warning")
            )),
        };
        eprintln!("{}", self.renderer.render(&[Group::with_title(summary)]));
        self.errors > 0
    }
}

/// A `compiler-message` on stdout, for the package whose unit it is about.
fn emit(unit: &RunUnit, diagnostic: Json) {
    let message = json!({
        "reason": "compiler-message",
        "package_id": unit.package_id,
        "manifest_path": unit.manifest_path,
        "target": unit.target,
        "message": diagnostic,
    });
    let _ = writeln!(std::io::stdout().lock(), "{message}");
}

fn child(level: &str, message: &str, spans: Vec<Json>) -> Json {
    json!({
        "message": message,
        "code": null,
        "level": level,
        "spans": spans,
        "children": [],
        "rendered": null,
    })
}

/// A span in rustc's JSON shape: 1-based lines and character columns, and
/// the lines it covers.
fn span(file: &str, source: &str, lo: u32, hi: u32) -> Json {
    let (lo, hi) = (lo as usize, hi as usize);
    let line_start = |at: usize| source[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_of = |at: usize| source[..at].matches('\n').count() + 1;
    let column = |at: usize| source[line_start(at)..at].chars().count() + 1;
    let first = line_start(lo);
    let last = source[hi..].find('\n').map_or(source.len(), |i| hi + i);
    let lines: Vec<&str> = source[first..last].split('\n').collect();
    let text: Vec<Json> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let start = if i == 0 { column(lo) } else { 1 };
            let end = if i + 1 == lines.len() {
                column(hi)
            } else {
                line.chars().count() + 1
            };
            json!({ "text": line, "highlight_start": start, "highlight_end": end })
        })
        .collect();
    json!({
        "file_name": file,
        "byte_start": lo,
        "byte_end": hi,
        "line_start": line_of(lo),
        "line_end": line_of(hi),
        "column_start": column(lo),
        "column_end": column(hi),
        "is_primary": true,
        "text": text,
        "label": null,
        "suggested_replacement": null,
        "suggestion_applicability": null,
        "expansion": null,
    })
}

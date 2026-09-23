//! `unused_pub`'s findings, decided once cargo has built the whole run, the
//! way hawk decides its own: every item a unit of the run recorded, against
//! every use any unit of the run recorded. No one compilation saw the whole
//! run, so this prints them, as rustc would have: rendered for a person, or
//! as cargo's JSON messages, held to the baseline when one is configured.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};
use serde_json::{Value as Json, json};

use crate::Output;
use crate::baseline_file;
use crate::over_baseline;
use crate::records::{self, Def, Note, Unit};

const LINT: &str = "unused_pub";

const HELP: &str = "remove it; if code under a `cfg`, target or feature not compiled here uses it, \
     gate the item the same way";

/// A unit of the run, as cargo's artifact message named it.
pub(crate) struct RunUnit {
    pub(crate) unit: Unit,
    pub(crate) package_id: String,
    pub(crate) manifest_path: String,
    pub(crate) target: Json,
}

struct Finding<'a> {
    def: Def,
    /// The baseline section of the compilation that defined the item.
    section: String,
    unit: &'a RunUnit,
}

/// What the run's records say.
enum Records<'a> {
    /// These units left no record, and judging without one would call
    /// everything they use unused.
    Missing(Vec<&'a Path>),
    Judged {
        unused: Vec<Finding<'a>>,
        /// The section of every unit that defines items, found unused or
        /// not: the ones whose baseline entries this run decides.
        sections: BTreeSet<String>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Severity {
    Warning,
    Error,
}

impl Severity {
    fn of(level: &str) -> Severity {
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

/// `judged` names the members whose items the run can judge. Every unit's
/// uses count, whatever member it is of.
fn judge<'a>(facts: &Path, units: &'a [RunUnit], judged: &HashSet<&str>) -> Records<'a> {
    let mut refs = HashSet::new();
    let mut found = Vec::new();
    let mut sections = BTreeSet::new();
    let mut missing = Vec::new();
    for run_unit in units {
        let unit = &run_unit.unit;
        // A test target with `harness = false` is built without `--test`.
        let recorded = records::read_refs(&unit.refs(facts)).or_else(|| {
            let plain = Unit {
                src: unit.src.clone(),
                test: false,
                extra_filename: unit.extra_filename.clone(),
            };
            if unit.test {
                records::read_refs(&plain.refs(facts))
            } else {
                None
            }
        });
        match recorded {
            Some(unit_refs) => refs.extend(unit_refs),
            None => missing.push(unit.src.as_path()),
        }
        if judged.contains(run_unit.package_id.as_str())
            && let Some(defs) = records::read_defs(&unit.defs(facts))
        {
            sections.insert(defs.section.clone());
            found.extend(defs.defs.into_iter().map(|def| Finding {
                def,
                section: defs.section.clone(),
                unit: run_unit,
            }));
        }
    }
    if !missing.is_empty() {
        return Records::Missing(missing);
    }
    // A run over several targets records one item once per target, and not
    // always under one key: the definition path numbers the `impl` blocks of
    // a module, and a `cfg(windows)` block above an item gives it
    // `{impl#10}` on Windows and `{impl#9}` elsewhere. Its position, the
    // crate plus where its name is, is the same in every unit. So an item is
    // used when a use of any of its keys was recorded, and it is one finding.
    let used: HashSet<String> = found
        .iter()
        .filter(|f| refs.contains(&f.def.key) || refs.contains(&f.def.position_key()))
        .map(|f| f.def.position_key())
        .collect();
    found.retain(|f| !used.contains(&f.def.position_key()));
    // An item of a type or trait that is itself unused goes with it. Keys,
    // since `parent` is the key the item's own unit gave the type or trait.
    let keys: HashSet<String> = found.iter().map(|f| f.def.key.clone()).collect();
    found.retain(|f| !keys.contains(&f.def.parent));
    // By key last, so which unit's record is kept does not depend on the
    // order cargo finished the units in.
    found.sort_by(|a, b| {
        (&a.def.file, a.def.lo, &a.def.key).cmp(&(&b.def.file, b.def.lo, &b.def.key))
    });
    found.dedup_by(|a, b| a.def.position_key() == b.def.position_key());
    Records::Judged {
        unused: found,
        sections,
    }
}

/// Judges and prints the run's `unused_pub` findings. `baseline` is the file
/// name the configuration gives, if it gives one. Whether any finding was an
/// error, and a line for `over-baseline.txt` for each section whose
/// `unused_pub` findings are over its baseline count.
pub(crate) fn report(
    root: &Path,
    facts: &Path,
    units: &[RunUnit],
    judged: &HashSet<&str>,
    output: &Output,
    styled: bool,
    baseline: Option<&str>,
) -> (bool, Vec<over_baseline::OverBaselineLine>) {
    let mut printer = Printer::new(root, output, styled, units.first());
    let (unused, sections) = match judge(facts, units, judged) {
        Records::Missing(units) => {
            // Cargo can build one target more than once in a run.
            let mut named: Vec<String> = Vec::new();
            for src in units {
                let name = format!("`{}`", src.strip_prefix(root).unwrap_or(src).display());
                if !named.contains(&name) {
                    named.push(name);
                }
            }
            // An error: a run that judged nothing must not pass for a run
            // that found nothing, least of all where `over-baseline.txt`
            // staying empty is what CI tests.
            printer.plain(
                Severity::Error,
                format!(
                    "mordant: `unused_pub` did not judge the workspace: {} left no record of \
                     what it uses; remove `{}`, which holds those records and the build `cargo \
                     mordant` reuses, then run again",
                    join(&named),
                    facts.parent().unwrap_or(facts).display(),
                ),
            );
            return (true, Vec::new());
        }
        Records::Judged { unused, sections } => (unused, sections),
    };
    let record = baseline_file::write_mode();
    let Some((_, path)) = baseline.and_then(|name| baseline_file::find(root, name, record)) else {
        for finding in &unused {
            let severity = Severity::of(&finding.def.level);
            printer.finding(finding, severity, None);
        }
        return (printer.tally(), Vec::new());
    };
    if record {
        write(&path, &sections, &unused);
        return (false, Vec::new());
    }
    let recorded = baseline_file::recorded(&path);
    let allowed = |file: &str| {
        recorded
            .get(&(LINT.to_string(), file.to_string()))
            .copied()
            .unwrap_or(0)
    };
    let mut found: HashMap<&str, usize> = HashMap::new();
    for finding in &unused {
        *found.entry(finding.def.file.as_str()).or_default() += 1;
    }
    // A file over its count shows every finding, as `baseline::print_over`
    // does: the count does not say which of them is the new one.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut over: HashMap<&str, usize> = HashMap::new();
    let mut files: Vec<&str> = Vec::new();
    for finding in &unused {
        let file = finding.def.file.as_str();
        let limit = allowed(file);
        if found[file] <= limit {
            continue;
        }
        let n = seen.entry(file).or_default();
        *n += 1;
        // The ones past the count are the crate's to answer for in
        // `over-baseline.txt`; a file two targets of a package share has
        // findings under two sections.
        if *n > limit {
            *over.entry(finding.section.as_str()).or_default() += 1;
        }
        if *n == 1 {
            files.push(file);
        }
        let note = format!("`{LINT}` over the mordant baseline ({limit} recorded for {file})");
        printer.finding(finding, Severity::Warning, Some(note));
    }
    for file in files {
        printer.plain(
            Severity::Warning,
            baseline_file::over_message(LINT, file, found[file], allowed(file)),
        );
    }
    let mut lines = Vec::new();
    for (section, n) in over {
        let line = over_baseline::OverBaselineLine::new(section, n);
        printer.plain(Severity::Warning, line.summary());
        lines.push(line);
    }
    (printer.tally(), lines)
}

/// Prints one error that is about no one item, as `report` prints its error
/// for missing records: on stderr in human and short output, and as a
/// `compiler-message` on stdout, attributed to `unit`, in JSON output.
pub(crate) fn print_error(
    root: &Path,
    output: &Output,
    styled: bool,
    unit: Option<&RunUnit>,
    message: String,
) {
    Printer::new(root, output, styled, unit).plain(Severity::Error, message);
}

/// Rewrites `unused_pub`'s entries in the sections this run judged, and
/// leaves every other entry as the compilations wrote it.
fn write(path: &Path, sections: &BTreeSet<String>, unused: &[Finding<'_>]) {
    baseline_file::update(path, |doc| {
        for section in sections {
            if let Some(entries) = doc.get_mut(section) {
                entries.retain(|key, _| !key.starts_with(&format!("{LINT}:")));
            }
        }
        for finding in unused {
            *doc.entry(finding.section.clone())
                .or_default()
                .entry(baseline_file::entry(LINT, &finding.def.file))
                .or_default() += 1;
        }
        doc.retain(|_, entries| !entries.is_empty());
    });
}

/// `a`, `a and b`, `a, b and c`.
pub(crate) fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Prints findings in the form `--message-format` asked for.
struct Printer<'a> {
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
    fn new(
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

    /// One finding. `over` is the baseline's note, for a finding printed
    /// because it goes over the recorded count, which no level can raise.
    fn finding(&mut self, finding: &Finding<'_>, severity: Severity, over: Option<String>) {
        let def = &finding.def;
        let message = format!(
            "{} `{}` is public, but nothing in the workspace uses it",
            def.descr, def.path
        );
        let Some(source) = self.source(&def.file) else {
            self.plain(
                Severity::Warning,
                format!("mordant: {message} ({} could not be read)", def.file),
            );
            return;
        };
        // Over the baseline, it is the baseline's warning, not the lint.
        let code = over.is_none().then_some(LINT);
        let notes: Vec<Note> = match over {
            Some(note) => vec![Note {
                help: false,
                message: note,
                at: None,
            }],
            None => def.notes.clone(),
        };
        let sources: Vec<Option<String>> = notes
            .iter()
            .map(|n| n.at.as_ref().and_then(|(file, ..)| self.source(file)))
            .collect();
        let report = |renderer: &Renderer| {
            let span = def.lo as usize..def.hi as usize;
            let mut main = severity.level().primary_title(message.as_str()).element(
                Snippet::source(source.as_str())
                    .path(def.file.as_str())
                    .annotation(AnnotationKind::Primary.span(span)),
            );
            main = main.element(Level::HELP.message(HELP));
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
        let mut children = vec![child("help", HELP, Vec::new())];
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
            "spans": [span(&def.file, &source, def.lo, def.hi)],
            "children": children,
            "rendered": rendered,
        });
        emit(finding.unit, diagnostic);
    }

    /// A message about no one item: `mordant: ...`, with no code and no span.
    fn plain(&mut self, severity: Severity, message: String) {
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

    /// The closing count, as rustc prints its own; whether any finding was
    /// an error.
    fn tally(&self) -> bool {
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
                Level::WARNING.primary_title(format!("`{LINT}` generated {}", plural(w, "warning")))
            }
            (e, 0) => {
                Level::ERROR.primary_title(format!("`{LINT}` generated {}", plural(e, "error")))
            }
            (e, w) => Level::ERROR.primary_title(format!(
                "`{LINT}` generated {} and {}",
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

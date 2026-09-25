//! `unused_pub`'s findings, decided once cargo has built the whole run, the
//! way hawk decides its own: every item a unit of the run recorded, against
//! every use any unit of the run recorded. No one compilation saw the whole
//! run, so this prints them, as rustc would have: rendered for a person, or
//! as cargo's JSON messages, held to the baseline when one is configured.
//!
//! Each member is held to the baseline file its own compilation finds, so a
//! member's findings count against the same file as its other lints.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::Output;
use crate::baseline_file;
use crate::over_baseline;
use crate::printer::{Located, Printer, Severity, join};
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
        /// not, with the manifest path of its member: the ones whose
        /// baseline entries this run decides.
        sections: BTreeSet<(&'a str, String)>,
    },
}

/// What one baseline file holds this run to: the findings of the members
/// that use it, and the sections of those members.
struct Governed<'a> {
    /// The directory the baseline file is in, which its file names start from.
    dir: &'a Path,
    sections: BTreeSet<&'a str>,
    findings: Vec<&'a Finding<'a>>,
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
            sections.insert((run_unit.manifest_path.as_str(), defs.section.clone()));
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
    let locations: HashMap<&str, Option<(PathBuf, PathBuf)>> = units
        .iter()
        .map(|unit| {
            (
                unit.manifest_path.as_str(),
                baseline_of(unit, baseline, record),
            )
        })
        .collect();
    let mut governed: BTreeMap<&Path, Governed<'_>> = BTreeMap::new();
    for (manifest, section) in &sections {
        if let Some((dir, path)) = &locations[manifest] {
            governed
                .entry(path)
                .or_insert_with(|| Governed::new(dir))
                .sections
                .insert(section);
        }
    }
    for finding in &unused {
        match &locations[finding.unit.manifest_path.as_str()] {
            Some((dir, path)) => governed
                .entry(path)
                .or_insert_with(|| Governed::new(dir))
                .findings
                .push(finding),
            // No baseline file governs it: an ordinary lint.
            None => show(
                &mut printer,
                finding,
                Severity::of(&finding.def.level),
                None,
            ),
        }
    }
    if record {
        for (path, group) in &governed {
            write(path, group);
        }
        return (printer.tally(LINT), Vec::new());
    }
    let mut over: BTreeMap<String, usize> = BTreeMap::new();
    for (path, group) in &governed {
        hold(&mut printer, path, group, &mut over);
    }
    let mut lines = Vec::new();
    for (section, n) in over {
        let line = over_baseline::OverBaselineLine::new(&section, n);
        printer.plain(Severity::Warning, line.summary());
        lines.push(line);
    }
    (printer.tally(LINT), lines)
}

impl<'a> Governed<'a> {
    fn new(dir: &'a Path) -> Self {
        Governed {
            dir,
            sections: BTreeSet::new(),
            findings: Vec::new(),
        }
    }

    /// A finding's file as the baseline file names it.
    fn file(&self, finding: &Finding<'_>) -> String {
        baseline_file::relative(self.dir, Path::new(&finding.def.file))
    }
}

/// The baseline file `unit`'s own compilation finds, as `(the directory it
/// is in, its path)`.
fn baseline_of(unit: &RunUnit, name: Option<&str>, record: bool) -> Option<(PathBuf, PathBuf)> {
    baseline_file::find(Path::new(&unit.manifest_path).parent()?, name?, record)
}

/// Holds the findings one baseline file governs to its counts. A file over
/// its count shows every finding, as `baseline::print_over` does: the count
/// does not say which of them is the new one. `over` counts the findings past
/// the count for each section.
fn hold(
    printer: &mut Printer<'_>,
    path: &Path,
    group: &Governed<'_>,
    over: &mut BTreeMap<String, usize>,
) {
    let recorded = baseline_file::recorded(path);
    let allowed = |file: &str| {
        recorded
            .get(&(LINT.to_string(), file.to_string()))
            .copied()
            .unwrap_or(0)
    };
    let files: Vec<String> = group.findings.iter().map(|f| group.file(f)).collect();
    let mut found: HashMap<&str, usize> = HashMap::new();
    for file in &files {
        *found.entry(file.as_str()).or_default() += 1;
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut shown: Vec<&str> = Vec::new();
    for (finding, file) in group.findings.iter().zip(&files) {
        let file = file.as_str();
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
            *over.entry(finding.section.clone()).or_default() += 1;
        }
        if *n == 1 {
            shown.push(file);
        }
        let note = format!("`{LINT}` over the mordant baseline ({limit} recorded for {file})");
        show(printer, finding, Severity::Warning, Some(note));
    }
    for file in shown {
        printer.plain(
            Severity::Warning,
            baseline_file::over_message(LINT, file, found[file], allowed(file)),
        );
    }
}

/// Prints one finding. `over` is the baseline's note, for a finding printed
/// because it goes over the recorded count, which no level can raise.
fn show(
    printer: &mut Printer<'_>,
    finding: &Finding<'_>,
    severity: Severity,
    over: Option<String>,
) {
    let def = &finding.def;
    // Over the baseline, it is the baseline's warning, not the lint.
    let (code, notes) = match over {
        Some(message) => (
            None,
            vec![Note {
                help: false,
                message,
                at: None,
            }],
        ),
        None => (Some(LINT), def.notes.clone()),
    };
    let located = Located {
        message: format!(
            "{} `{}` is public, but nothing in the workspace uses it",
            def.descr, def.path
        ),
        file: &def.file,
        lo: def.lo,
        hi: def.hi,
        code,
        help: HELP,
        notes: &notes,
        unit: finding.unit,
    };
    printer.finding(located, severity);
}

/// Rewrites `unused_pub`'s entries in the sections this run judged, and
/// leaves every other entry as the compilations wrote it.
fn write(path: &Path, group: &Governed<'_>) {
    baseline_file::update(path, |doc| {
        for section in &group.sections {
            if let Some(entries) = doc.get_mut(*section) {
                entries.retain(|key, _| !key.starts_with(&format!("{LINT}:")));
            }
        }
        for finding in &group.findings {
            *doc.entry(finding.section.clone())
                .or_default()
                .entry(baseline_file::entry(LINT, &group.file(finding)))
                .or_default() += 1;
        }
        doc.retain(|_, entries| !entries.is_empty());
    });
}

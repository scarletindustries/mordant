//! `<target>/mordant/over-baseline.txt`, which `cargo mordant` writes once
//! the run is judged. It holds one `<section> <count>` line for each
//! compilation whose findings went over the baseline count, and one for each
//! section whose `unused_pub` findings did, so a section can have two lines.
//! A run that writes any line also exits with status 101, so the file says
//! which crates went over and the exit status says whether any did.
//!
//! The `.over` files are read after cargo has finished, so a second
//! `cargo mordant` run sharing the target directory can rewrite one of them
//! in between, and this run then reports that run's count for the unit.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::baseline_file::write_mode;
use crate::records::{self, Over, Unit};
use crate::unused_pub::RunUnit;

/// Reads the `.over` file of every unit in `units` and of every build
/// script in `build_scripts`, from `over_baseline_counts` beside `facts`,
/// and returns a line for each one that holds a count of findings over the
/// baseline. `Err` lists the source file of each unit or build script whose
/// `.over` file is missing or holds something else, each source file once.
///
/// Cargo builds a test target with `harness = false` without `--test`, so
/// when a test unit has no `.over` file, this reads the one written under
/// the same unit with `test` false. Cargo can build one target more than
/// once in a run, so each `.over` file adds its line once.
pub(crate) fn read_counts(
    facts: &Path,
    units: &[RunUnit],
    build_scripts: &[Unit],
) -> Result<Vec<OverBaselineLine>, Vec<PathBuf>> {
    let mut lines = Vec::new();
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut read = HashSet::new();
    for unit in units
        .iter()
        .map(|run_unit| &run_unit.unit)
        .chain(build_scripts)
    {
        let path = unit.over(facts);
        let found = match records::read_over(&path) {
            Some(over) => Some((path, over)),
            None if unit.test => {
                let plain = Unit {
                    src: unit.src.clone(),
                    test: false,
                    extra_filename: unit.extra_filename.clone(),
                };
                let path = plain.over(facts);
                records::read_over(&path).map(|over| (path, over))
            }
            None => None,
        };
        match found {
            Some((path, over)) => {
                if read.insert(path)
                    && let Over::Count { section, count } = over
                {
                    lines.push(OverBaselineLine::new(&section, count));
                }
            }
            None => {
                if !missing.contains(&unit.src) {
                    missing.push(unit.src.clone());
                }
            }
        }
    }
    if missing.is_empty() {
        Ok(lines)
    } else {
        Err(missing)
    }
}

/// `over-baseline.txt`'s lines, sorted by section then count, so the same
/// counts always give the same bytes.
struct OverBaselineLines(Vec<OverBaselineLine>);

impl OverBaselineLines {
    fn new(lines: impl IntoIterator<Item = OverBaselineLine>) -> Self {
        let mut lines: Vec<_> = lines.into_iter().collect();
        lines.sort();
        Self(lines)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes of `over-baseline.txt`: one `<section> <count>` line per
    /// entry, each ending in `\n`.
    fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.0 {
            out.push_str(&line.section);
            out.push(' ');
            out.push_str(&line.count.to_string());
            out.push('\n');
        }
        out
    }
}

/// One `<section> <count>` line of `over-baseline.txt`: a section and how
/// many of its findings are over its baseline count.
#[derive(Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct OverBaselineLine {
    section: String,
    count: usize,
}

impl OverBaselineLine {
    /// The text of the summary warning for this line's section, which the
    /// caller prints:
    /// `mordant: <count> finding(s) over the baseline in <section>`.
    pub(crate) fn summary(&self) -> String {
        format!(
            "mordant: {} finding(s) over the baseline in {}",
            self.count, self.section
        )
    }

    pub(crate) fn new(section: &str, count: usize) -> Self {
        Self {
            section: section.to_string(),
            count,
        }
    }
}

/// The text of the closing error for a run where `lines` is not empty: the
/// count of findings over the baseline, all lines together, and each section
/// once, as `mordant: <count> finding(s) over the baseline in <sections>`.
pub(crate) fn run_summary(lines: &[OverBaselineLine]) -> String {
    let count: usize = lines.iter().map(|line| line.count).sum();
    let mut sections: Vec<String> = Vec::new();
    for line in lines {
        let named = format!("`{}`", line.section);
        if !sections.contains(&named) {
            sections.push(named);
        }
    }
    sections.sort();
    format!(
        "mordant: {count} finding(s) over the baseline in {}",
        crate::unused_pub::join(&sections)
    )
}

/// Writes `lines`, sorted, to `<target_dir>/mordant/<file_name>`, through a
/// temporary file renamed into place.
///
/// Writes nothing when `MORDANT_BASELINE_WRITE` is set: the file on disk
/// stays as it was. When `lines` is empty it deletes the file. When the file
/// already holds the same bytes it leaves the file, and its modification
/// time, unchanged.
pub(crate) fn write_over_baseline(
    target_dir: &Path,
    file_name: &str,
    lines: impl IntoIterator<Item = OverBaselineLine>,
) -> Result<(), std::io::Error> {
    if write_mode() {
        return Ok(());
    }
    let path = target_dir.join("mordant").join(file_name);
    let lines = OverBaselineLines::new(lines);
    if lines.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(at_path(&path, err)),
            Ok(()) => Ok(()),
        };
    }
    let text = lines.text();
    if same_contents(&path, &text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| at_path(&path, err))?;
    }
    let tmp = temp_file(&path)?;
    std::fs::write(&tmp.0, text.as_bytes()).map_err(|err| at_path(&path, err))?;
    std::fs::rename(&tmp.0, &path).map_err(|err| at_path(&path, err))?;
    Ok(())
}

fn at_path(path: &Path, err: std::io::Error) -> std::io::Error {
    std::io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

/// Whether the file at `path` holds exactly `text`. It reads the file in
/// 8 KiB chunks and compares each chunk with the matching part of `text`, so
/// the whole file is never held in memory. A file that cannot be opened or
/// read counts as different.
fn same_contents(path: &Path, text: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let expected = text.as_bytes();
    let mut offset = 0;
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return offset == expected.len(),
            Ok(n) => n,
            Err(_) => return false,
        };
        let end = offset + n;
        if end > expected.len() || buf[..n] != expected[offset..end] {
            return false;
        }
        offset = end;
    }
}

/// `.<file name>.<process id>.tmp` next to `path`, deleted when the returned
/// value is dropped unless it was renamed away first. It returns an
/// `InvalidInput` error when `path` has no UTF-8 file name.
fn temp_file(path: &Path) -> Result<DeleteOnDrop, std::io::Error> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path must have a file name",
        ));
    };
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    Ok(DeleteOnDrop(tmp))
}

struct DeleteOnDrop(PathBuf);
impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{OverBaselineLine, write_over_baseline};
    use std::path::PathBuf;

    #[test]
    fn the_same_lines_leave_over_baseline_txt_untouched() {
        let dir =
            std::env::temp_dir().join(format!("mordant-the-same-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveDir(dir.clone());
        let path = dir.join("mordant").join("over-baseline.txt");
        let lines = || {
            [
                OverBaselineLine::new("crate1", 1),
                OverBaselineLine::new("crate2", 2),
            ]
        };
        write_over_baseline(&dir, "over-baseline.txt", lines()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "crate1 1\ncrate2 2\n"
        );
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_over_baseline(&dir, "over-baseline.txt", lines()).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn new_counts_replace_over_baseline_txt() {
        let dir = std::env::temp_dir().join(format!("mordant-new-counts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveDir(dir.clone());
        let path = dir.join("mordant").join("over-baseline.txt");
        write_over_baseline(
            &dir,
            "over-baseline.txt",
            [
                OverBaselineLine::new("crate_a", 1),
                OverBaselineLine::new("crate_b", 2),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "crate_a 1\ncrate_b 2\n"
        );
        write_over_baseline(
            &dir,
            "over-baseline.txt",
            [
                OverBaselineLine::new("crate_a", 3),
                OverBaselineLine::new("crate_b", 4),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "crate_a 3\ncrate_b 4\n"
        );
    }

    #[test]
    fn no_lines_delete_over_baseline_txt() {
        let dir =
            std::env::temp_dir().join(format!("mordant-no-lines-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveDir(dir.clone());
        let path = dir.join("mordant").join("over-baseline.txt");
        write_over_baseline(
            &dir,
            "over-baseline.txt",
            [
                OverBaselineLine::new("crate_a", 1),
                OverBaselineLine::new("crate_b", 2),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "crate_a 1\ncrate_b 2\n"
        );
        write_over_baseline(&dir, "over-baseline.txt", Vec::<OverBaselineLine>::new()).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn no_lines_and_no_file_write_nothing() {
        let dir =
            std::env::temp_dir().join(format!("mordant-no-lines-no-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveDir(dir.clone());
        let path = dir.join("mordant").join("over-baseline.txt");
        write_over_baseline(&dir, "over-baseline.txt", Vec::<OverBaselineLine>::new()).unwrap();
        assert!(!path.exists());
    }

    struct RemoveDir(PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

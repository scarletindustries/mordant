//! The records each compilation of a member writes under
//! `<target>/mordant/unused_pub/`, named for its [`Unit`], and `cargo
//! mordant` reads once the run is built. `<unit>.refs` has one line per
//! workspace item the compilation uses. `<unit>.defs` has one line per `pub`
//! item it defines, written by a library's or a binary's own build and not
//! by its test build, after a first line naming the baseline section its
//! findings belong to. Items are keyed by crate name plus definition path,
//! which reads the same from every crate that depends on the one defining
//! them. A crate's use of its own items is keyed by [`position_key`]
//! instead. A build script writes neither file. Beside that directory, in
//! `<target>/mordant/over_baseline_counts/`, every compilation, a build
//! script's included, writes `<unit>.over`. It is one `<section>\t<count>`
//! line when the compilation has findings over its baseline count, and
//! empty otherwise: no baseline found, write mode, or nothing over. A file
//! is written whole under a temporary name and renamed, so a reader never
//! sees half of one.
//!
//! Shared by the library and `cargo-mordant`, which includes this file by
//! path, so nothing here may depend on the compiler.

use std::collections::{BTreeSet, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

/// One `pub` item, as its defining crate describes it for `cargo mordant`
/// to report.
pub struct Def {
    pub key: String,
    /// Source file as rustc named it, relative to the workspace root for
    /// a member.
    pub file: String,
    /// The item's name, as byte offsets into `file`.
    pub lo: u32,
    pub hi: u32,
    /// "function", "struct", ...
    pub descr: String,
    /// `crate::module::Item`, for the message.
    pub path: String,
    /// Key of the trait or type the item belongs to, or empty: when that
    /// is reported, the item is not.
    pub parent: String,
    /// `unused_pub`'s level at the item, as rustc spells it: `warn`,
    /// `deny`, `forbid` or `force-warn`, with the command line and every
    /// attribute around it applied. An allowed item is not recorded.
    pub level: String,
    /// What rustc would say under the finding about where that level comes
    /// from.
    pub notes: Vec<Note>,
}

impl Def {
    /// What a use of this item from its own crate is recorded as.
    pub fn position_key(&self) -> String {
        let krate = self.key.split("::").next().unwrap_or(&self.key);
        position_key(krate, &self.file, self.lo)
    }
}

/// How a crate records a use of one of its own items: the crate, as it
/// starts the item's key, and where the item's name is. The definition path
/// will not do. A crate is compiled once on its own and once with `--test`,
/// and the two number its `impl` blocks apart when one sits under
/// `cfg(test)`: `{impl#1}::get` in the test build is `{impl#0}::get` in the
/// build that records the item, and can be another type's `get` there. The
/// name is at the same place in the same file in both.
pub fn position_key(krate: &str, file: &str, lo: u32) -> String {
    format!("{krate}@{file}:{lo}")
}

/// One line under a finding: a note, or a help, optionally pointing at the
/// source (`file`, `lo`, `hi`) like `Def`'s own position.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub help: bool,
    pub message: String,
    pub at: Option<(String, u32, u32)>,
}

/// A unit's items and the baseline section a finding among them counts in.
pub struct Defs {
    pub section: String,
    pub defs: Vec<Def>,
}

/// One compilation cargo runs: a target's crate root, whether it is built as
/// a test harness, and cargo's `-C extra-filename` for that unit.
/// `cargo mordant` names the same units from cargo's own report of the run.
pub struct Unit {
    pub src: PathBuf,
    pub test: bool,
    /// Required so that two compiles of the same crate root (e.g. targeting
    /// different platforms) do not overwrite the same `.refs` and `.defs` files.
    pub extra_filename: String,
}

impl Unit {
    /// The stem both files of this unit are named with. The path is
    /// canonical, so rustc's relative path and cargo's absolute one agree.
    fn stem(&self) -> String {
        let src = std::fs::canonicalize(&self.src).unwrap_or_else(|_| self.src.clone());
        let mut hasher = DefaultHasher::new();
        src.hash(&mut hasher);
        self.test.hash(&mut hasher);
        self.extra_filename.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    pub fn defs(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.defs", self.stem()))
    }

    pub fn refs(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.refs", self.stem()))
    }

    /// `dir` is the directory of the `.refs` and `.defs` files; the `.over`
    /// file is in `over_baseline_counts` beside it.
    pub fn over(&self, dir: &Path) -> PathBuf {
        dir.with_file_name("over_baseline_counts")
            .join(format!("{}.over", self.stem()))
    }
}

pub fn write_defs<'a>(path: &Path, section: &str, defs: impl Iterator<Item = &'a Def>) {
    let mut out = format!("{section}\n");
    for d in defs {
        // Compact JSON has no tab or newline outside its escaped strings.
        let notes = serde_json::to_string(&d.notes).unwrap_or_default();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{notes}\n",
            d.key, d.file, d.lo, d.hi, d.descr, d.path, d.parent, d.level
        ));
    }
    write_whole(path, &out);
}

/// `None` if the file is missing.
pub fn read_defs(path: &Path) -> Option<Defs> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let section = lines.next()?.to_string();
    let defs = lines
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(Def {
                key: f.next()?.into(),
                file: f.next()?.into(),
                lo: f.next()?.parse().ok()?,
                hi: f.next()?.parse().ok()?,
                descr: f.next()?.into(),
                path: f.next()?.into(),
                parent: f.next()?.into(),
                level: f.next()?.into(),
                notes: serde_json::from_str(f.next()?).ok()?,
            })
        })
        .collect();
    Some(Defs { section, defs })
}

pub fn write_refs(path: &Path, refs: &BTreeSet<String>) {
    let mut out = String::new();
    for r in refs {
        out.push_str(r);
        out.push('\n');
    }
    write_whole(path, &out);
}

/// Writes `section` and its number of findings over the baseline count to the
/// `.over` file at `path`, or an empty file when there are none.
pub fn write_over(path: &Path, over: Option<(&str, usize)>) {
    let out = match over {
        Some((section, n)) => format!("{section}\t{n}\n"),
        None => String::new(),
    };
    write_whole(path, &out);
}

/// What a `.over` file says.
pub enum Over {
    /// The file is empty: the compilation had no findings over its baseline
    /// count.
    Nothing,
    /// The file is one `<section>\t<count>` line: the compilation had `count`
    /// findings over the baseline count in `section`.
    Count { section: String, count: usize },
}

/// What the `.over` file at `path` says, or `None` if it is missing or holds
/// anything but what `write_over` writes.
pub fn read_over(path: &Path) -> Option<Over> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.is_empty() {
        return Some(Over::Nothing);
    }
    let (section, count) = text.strip_suffix('\n')?.split_once('\t')?;
    Some(Over::Count {
        section: section.to_string(),
        count: count.parse().ok()?,
    })
}

/// The keys a `.refs` file lists; `None` if it is missing.
pub fn read_refs(path: &Path) -> Option<HashSet<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

/// Every key any `.defs` file in `dir` lists: the items a use is worth
/// recording for.
pub fn all_def_keys(dir: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashSet::new();
    };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("defs"))
        .filter_map(|p| read_defs(&p))
        .flat_map(|d| d.defs.into_iter().map(|d| d.key))
        .collect()
}

fn write_whole(path: &Path, contents: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, contents).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

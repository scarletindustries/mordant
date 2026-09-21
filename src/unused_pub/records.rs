//! The records each compilation of a member writes under
//! `<target>/mordant/unused_pub/`, named for its [`Unit`], and `cargo
//! mordant` reads once the run is built. `<unit>.refs` has one line per
//! workspace item the compilation uses. `<unit>.defs` has one line per `pub`
//! item it defines, written by a library's or a binary's own build and not
//! by its test build, after a first line naming the baseline section its
//! findings belong to. Items are keyed by crate name plus definition path,
//! which reads the same from every crate. A file is written whole under a
//! temporary name and renamed, so a reader never sees half of one.
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

/// One compilation cargo runs: a target's crate root, and whether it is
/// built as a test harness. `cargo mordant` names the same units from
/// cargo's own report of the run.
pub struct Unit {
    pub src: PathBuf,
    pub test: bool,
}

impl Unit {
    /// The stem both files of this unit are named with. The path is
    /// canonical, so rustc's relative path and cargo's absolute one agree.
    fn stem(&self) -> String {
        let src = std::fs::canonicalize(&self.src).unwrap_or_else(|_| self.src.clone());
        let mut hasher = DefaultHasher::new();
        src.hash(&mut hasher);
        self.test.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    pub fn defs(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.defs", self.stem()))
    }

    pub fn refs(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.refs", self.stem()))
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

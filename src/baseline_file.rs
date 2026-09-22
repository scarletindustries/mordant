//! The baseline file and `over-baseline.txt` on disk: where they are, and
//! reading and writing them. The ratchet itself is `baseline`'s, per
//! compilation, and `cargo mordant`'s, for the findings it reports once the
//! run is built. Shared by the library and `cargo-mordant`, which includes
//! this file by path, so nothing here may depend on the compiler.
//!
//! The file is TOML: one table per section, each crate compilation's own,
//! holding `"<lint>:<file>" = <count>`.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

pub type Doc = BTreeMap<String, BTreeMap<String, u64>>;

/// Set, the run rewrites the baseline instead of being held to it.
pub const WRITE_ENV: &str = "MORDANT_BASELINE_WRITE";

pub fn write_mode() -> bool {
    std::env::var_os(WRITE_ENV).is_some()
}

pub fn read_doc(text: &str) -> Doc {
    toml::from_str(text).unwrap_or_default()
}

/// The key a finding is counted under.
pub fn entry(lint: &str, file: &str) -> String {
    format!("{lint}:{file}")
}

/// The baseline named `file_name`, as `(the directory it is in, its path)`:
/// the first one found from `dir` upward. In write mode it may not exist
/// yet, and then belongs beside the `mordant.toml` that named it.
pub fn find(dir: &Path, file_name: &str, record: bool) -> Option<(PathBuf, PathBuf)> {
    let mut dir = dir.to_path_buf();
    loop {
        let cand = dir.join(file_name);
        if cand.exists() || (record && dir.join("mordant.toml").exists()) {
            return Some((dir, cand));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `(lint, file) -> count`, summed over every section, since a key can
/// appear under more than one (a file shared by a lib and a bin target).
pub fn recorded(path: &Path) -> HashMap<(String, String), usize> {
    let doc = std::fs::read_to_string(path)
        .map(|s| read_doc(&s))
        .unwrap_or_default();
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for section in doc.values() {
        for (key, n) in section {
            if let Some((lint, file)) = key.split_once(':') {
                *counts
                    .entry((lint.to_string(), file.to_string()))
                    .or_default() += *n as usize;
            }
        }
    }
    counts
}

/// Rewrites the file with `edit` applied to what it holds now. Parallel
/// compilations write concurrently, so the read-modify-write happens under
/// an exclusive lock.
pub fn update(path: &Path, edit: impl FnOnce(&mut Doc)) {
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        // Not `truncate`: the file is read first, then rewritten in place
        // under the lock via `set_len(0)`.
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
    else {
        return;
    };
    let _ = f.lock();
    let mut existing = String::new();
    let _ = f.read_to_string(&mut existing);
    let mut doc = read_doc(&existing);
    edit(&mut doc);
    if let Ok(out) = toml::to_string_pretty(&doc) {
        let _ = f.set_len(0);
        let _ = f.rewind();
        let _ = f.write_all(out.as_bytes());
    }
    let _ = f.unlock();
}

/// `CARGO_TARGET_DIR` when it names a directory; cargo treats an empty
/// value as unset, so that is `None` here too.
pub fn cargo_target_dir() -> Option<PathBuf> {
    std::env::var_os("CARGO_TARGET_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// `<target>/mordant/over-baseline.txt`, where the target is
/// `${CARGO_TARGET_DIR or <root>/target}`, a relative `CARGO_TARGET_DIR`
/// taken from the workspace root as cargo does.
pub fn status_file(root: &Path, target_dir: Option<&Path>) -> PathBuf {
    match target_dir {
        Some(dir) => root.join(dir),
        None => root.join("target"),
    }
    .join("mordant")
    .join("over-baseline.txt")
}

/// What a run says once for a lint and a file that are over the baseline,
/// under the findings it shows for them.
pub fn over_message(lint: &str, file: &str, found: usize, allowed: usize) -> String {
    format!(
        "mordant: {found} `{lint}` findings in {file}, and the baseline allows {allowed}. The \
         baseline holds a count, not which findings, so all {found} are shown: any of them can \
         be the new one"
    )
}

/// Records that `name` went `over` its baseline. Appended to, never
/// truncated: every crate is its own rustc process, so no process knows it
/// is the first. CI removes the file before the run and tests it is empty
/// or absent after.
pub fn append_status(path: &Path, name: &str, over: usize) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = f.lock();
    let _ = f.write_all(format!("{name} {over}\n").as_bytes());
    let _ = f.unlock();
}

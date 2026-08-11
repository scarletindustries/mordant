//! Ratchet mode. A baseline file records the accepted finding count per
//! (lint, file); runs suppress up to that many findings and surface only the
//! overflow, so mordant can gate CI on a brownfield codebase from day one.
//!
//! Regeneration: `MORDANT_BASELINE_WRITE=1 cargo dylint --all` emits nothing
//! and rewrites each compiled crate's section instead. Sections are keyed by
//! crate so parallel rustc processes never clobber another crate's entries;
//! the file itself is serialized with an exclusive file lock.
//!
//! Counts, not spans: line numbers drift with every edit, so a per-file count
//! is the only key that survives normal development. Moving a finding between
//! files consumes allowance in one file and overflows in the other, which is
//! the desired ratchet behavior.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use clippy_utils::diagnostics::span_lint_and_help;
use rustc_lint::{LateContext, LateLintPass, Lint};
use rustc_span::{FileName, Span};

pub struct Baseline {
    root: PathBuf,
    path: PathBuf,
    /// (lint, relative file) -> remaining suppressions this run.
    allowance: Mutex<HashMap<(String, String), usize>>,
}

static STATE: OnceLock<Option<Baseline>> = OnceLock::new();
static RECORDED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub fn write_mode() -> bool {
    std::env::var_os("MORDANT_BASELINE_WRITE").is_some()
}

type Doc = BTreeMap<String, BTreeMap<String, u64>>;

fn read_doc(bytes: &str) -> Doc {
    toml::from_str(bytes).unwrap_or_default()
}

/// Called once from `register_lints`.
pub fn setup(file_name: &Option<String>) {
    let _ = STATE.set(file_name.as_ref().and_then(|f| init(f)));
}

fn init(file_name: &str) -> Option<Baseline> {
    let mut dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    loop {
        let cand = dir.join(file_name);
        // In write mode the file may not exist yet; anchor at the workspace
        // root, which is where dylint.toml (the config that named us) lives.
        if cand.exists() || (write_mode() && dir.join("dylint.toml").exists()) {
            let doc = std::fs::read_to_string(&cand)
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
            return Some(Baseline {
                root: dir,
                path: cand,
                allowance: Mutex::new(counts),
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn rel_file(cx: &LateContext<'_>, b: &Baseline, span: Span) -> Option<String> {
    let FileName::Real(real) = cx.tcx.sess.source_map().span_to_filename(span) else {
        return None;
    };
    let path = real.local_path()?.to_path_buf();
    let rel = path.strip_prefix(&b.root).unwrap_or(&path);
    Some(rel.to_string_lossy().into_owned())
}

/// True when this finding is consumed by the baseline (or recorded, in write
/// mode) and must not be emitted.
pub fn suppressed(cx: &LateContext<'_>, lint: &'static Lint, span: Span) -> bool {
    let Some(Some(b)) = STATE.get().map(Option::as_ref) else {
        return false;
    };
    let Some(file) = rel_file(cx, b, span) else {
        return false;
    };
    let key = (lint.name_lower(), file);
    if write_mode() {
        RECORDED.lock().unwrap().push(key);
        return true;
    }
    let mut allowance = b.allowance.lock().unwrap();
    match allowance.get_mut(&key) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

/// Every mordant lint reports through here so the ratchet sees all of them.
pub fn emit(
    cx: &LateContext<'_>,
    lint: &'static Lint,
    span: Span,
    msg: impl Into<String>,
    help: &'static str,
) {
    if suppressed(cx, lint, span) {
        return;
    }
    span_lint_and_help(cx, lint, span, msg.into(), None, help);
}

// Registered last: flushes write-mode recordings for this crate into the
// baseline file, replacing only this crate's section.
rustc_session::declare_lint! {
    /// Never fires; the pass exists for its `check_crate_post` hook. Declared
    /// `Warn` because rustc skips a pass whose lints are all allowed, and this
    /// pass must run.
    pub MORDANT_BASELINE,
    Warn,
    "internal pass that writes the ratchet baseline"
}

rustc_session::declare_lint_pass!(BaselineWriter => [MORDANT_BASELINE]);

impl<'tcx> LateLintPass<'tcx> for BaselineWriter {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        if !write_mode() {
            return;
        }
        let Some(Some(b)) = STATE.get().map(Option::as_ref) else {
            return;
        };
        let recorded: Vec<_> = std::mem::take(&mut *RECORDED.lock().unwrap());
        let mut section: BTreeMap<String, u64> = BTreeMap::new();
        for (lint, file) in recorded {
            *section.entry(format!("{lint}:{file}")).or_default() += 1;
        }
        // One crate name can cover several targets (lib + bin); suffix bins so
        // sections never clobber each other.
        let mut name = cx
            .tcx
            .crate_name(rustc_hir::def_id::LOCAL_CRATE)
            .to_string();
        if let Some(bin) = std::env::var_os("CARGO_BIN_NAME") {
            name = format!("{name} (bin {})", bin.to_string_lossy());
        }
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            // Not `truncate`: the file is read first, then rewritten in place
            // under the lock via `set_len(0)`.
            .truncate(false)
            .read(true)
            .write(true)
            .open(&b.path)
        else {
            return;
        };
        // Parallel rustc processes write concurrently; the lock serializes the
        // read-modify-write.
        let _ = f.lock();
        let mut existing = String::new();
        let _ = f.read_to_string(&mut existing);
        let mut doc = read_doc(&existing);
        if section.is_empty() {
            doc.remove(&name);
        } else {
            doc.insert(name, section);
        }
        if let Ok(out) = toml::to_string_pretty(&doc) {
            let _ = f.set_len(0);
            let _ = f.rewind();
            let _ = f.write_all(out.as_bytes());
        }
        let _ = f.unlock();
    }
}

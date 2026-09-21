//! rustc with mordant's lints registered. `cargo mordant` runs `cargo check`
//! with this as `RUSTC_WORKSPACE_WRAPPER`, so cargo invokes it as
//! `mordant-driver <path to rustc> <rustc args>` for every workspace member,
//! and plain rustc for everything else. The ui tests run it as rustc itself.
//!
//! Shaped after clippy's `src/driver.rs` at the `clippy_utils` rev this pack
//! pins, which is written against the same nightly: when a nightly bump
//! breaks this file, that file at the new rev shows the fix.

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_session;
extern crate rustc_span;

// Override the C allocator in the same way that the `rustc` binary would do.
rustc_driver::override_c_allocator_in_binary!();

use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rustc_interface::interface;
use rustc_session::config::ErrorOutputType;
use rustc_session::{EarlyDiagCtxt, Session};
use rustc_span::Symbol;

use mordant::BASELINE_WRITE_ENV;
use mordant::protocol::{CONFIG_ENV, FACTS_ENV, LIST_ARG};

/// Extra rustc flags for the linted crates only, split on whitespace:
/// `MORDANT_RUSTFLAGS="-D warnings"`. `RUSTFLAGS` would also rebuild every
/// dependency with them.
const RUSTFLAGS_ENV: &str = "MORDANT_RUSTFLAGS";

const BUG_REPORT_URL: &str = "https://github.com/scarletindustries/mordant/issues/new";

/// `dylint_lib = "mordant"` is the cfg dylint's driver set, and consumers
/// already gate `#[cfg_attr(dylint_lib = "mordant", allow(..))]` on it.
const LINTING_CFG: &str = r#"--cfg=dylint_lib="mordant""#;

fn main() -> ExitCode {
    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());
    rustc_driver::init_rustc_env_logger(&early_dcx);
    rustc_driver::install_ice_hook(BUG_REPORT_URL, |dcx| {
        dcx.handle().note(format!(
            "mordant {} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("MORDANT_SOURCE_REV")
        ));
    });
    rustc_driver::catch_with_exit_code(move || {
        let mut args = rustc_driver::args::raw_args(&early_dcx);
        if args.len() == 2 && args[1] == LIST_ARG {
            print_lints();
            return ExitCode::SUCCESS;
        }
        // A workspace wrapper gets rustc's path first; run as rustc, it does not.
        if args.get(1).map(Path::new).and_then(Path::file_stem) == Some("rustc".as_ref()) {
            args.remove(1);
        }
        if is_info_query(&args) {
            rustc_driver::run_compiler(&args, &mut PlainCallbacks);
        } else {
            args.push(LINTING_CFG.to_string());
            // A test build is compiled only for `unused_pub` to see what the
            // tests use. Its warnings are the crate's own test job's to
            // report, from the crate's own toolchain, not this nightly's.
            if args.iter().any(|a| a == "--test") {
                args.extend(["--cap-lints".to_string(), "allow".to_string()]);
            }
            if let Ok(flags) = env::var(RUSTFLAGS_ENV) {
                args.extend(flags.split_whitespace().map(String::from));
            }
            rustc_driver::run_compiler(&args, &mut MordantCallbacks);
        }
        ExitCode::SUCCESS
    })
}

fn print_lints() {
    let mut lints = mordant::lints();
    lints.sort_by_key(|lint| lint.name);
    let name_width = lints.iter().map(|l| l.name.len()).max().unwrap_or(0);
    let level_width = lints
        .iter()
        .map(|l| l.default_level.as_str().len())
        .max()
        .unwrap_or(0);
    println!("mordant");
    for lint in lints {
        println!(
            "    {:<name_width$}    {:<level_width$}    {}",
            lint.name_lower(),
            lint.default_level.as_str(),
            lint.desc
        );
    }
}

/// Cargo asks the wrapper for `-vV` and `--print` output to learn about the
/// compiler; those compile nothing, so no lint needs registering, and a bad
/// `dylint.toml` must not end up in the answers cargo caches. The exception,
/// as in clippy, is `--print crate-root-lint-levels`, whose answer depends
/// on which lints are registered.
fn is_info_query(args: &[String]) -> bool {
    args.iter().enumerate().any(|(i, arg)| {
        let printed = match arg.strip_prefix("--print") {
            Some("") => args.get(i + 1).map(String::as_str),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        arg == "-vV" || printed.is_some_and(|p| p != "crate-root-lint-levels")
    })
}

struct PlainCallbacks;

impl rustc_driver::Callbacks for PlainCallbacks {}

struct MordantCallbacks;

impl rustc_driver::Callbacks for MordantCallbacks {
    fn config(&mut self, config: &mut interface::Config) {
        let previous = config.register_lints.take();
        config.track_state = Some(Box::new(track_state));
        config.register_lints = Some(Box::new(move |sess, lint_store| {
            if let Some(previous) = &previous {
                previous(sess, lint_store);
            }
            mordant::register_lints(sess, lint_store);
        }));
        // `clippy_utils::sym` interns its own symbols after rustc's; they
        // resolve only in a session that registers the same list.
        config.extra_symbols = clippy_utils::sym::EXTRA_SYMBOLS.into();
        // The MIR the lints read is unoptimized, as under clippy and dylint.
        config.opts.unstable_opts.mir_opt_level = Some(0);
        if let Some(dir) = &mut config.opts.incremental {
            *dir = incremental_dir(dir);
        }
    }
}

/// Inputs rustc does not see that change what the lints report, written to
/// the dep-info file so cargo reruns a crate when one changes: the
/// configuration, the extra flags, where `unused_pub` keeps its records,
/// the baseline and whether this run writes it, and this binary, which a
/// reinstall or a rebuild replaces with one carrying different lints.
///
/// Without the baseline among them, a run that writes it over a build an
/// earlier run left recorded nothing, and a baseline that changed was not
/// read: cargo replayed the warnings weighed against the old one.
fn track_state(sess: &Session) {
    let mut env_depinfo = sess.env_depinfo.borrow_mut();
    for var in [CONFIG_ENV, RUSTFLAGS_ENV, FACTS_ENV, BASELINE_WRITE_ENV] {
        env_depinfo.insert((
            Symbol::intern(var),
            env::var(var).ok().map(|value| Symbol::intern(&value)),
        ));
    }
    let mut file_depinfo = sess.file_depinfo.borrow_mut();
    for file in [env::current_exe().ok(), mordant::baseline_path()] {
        if let Some(file) = file.as_deref().and_then(Path::to_str) {
            file_depinfo.insert(Symbol::intern(file));
        }
    }
}

/// Cargo's incremental directory for the crate, one level down in a
/// directory named for this build of the driver. The lints are part of what
/// the incremental cache records: a query such as `shallow_lint_levels_on`
/// reads the lint store, and nothing in rustc's dependency graph says the
/// store changed when the driver did, so a cache written by another build
/// of it is reused as current and the compiler ICEs (trailofbits/dylint#2010).
/// The directories of other builds are removed as this one starts. Cargo
/// holds the build directory's lock for the whole run, so every rustc
/// running alongside this one uses the same build of the driver.
fn incremental_dir(dir: &Path) -> PathBuf {
    let own = format!("mordant-driver-{:016x}", build_id());
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|n| n.starts_with("mordant-driver-") && n != own)
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    dir.join(own)
}

/// Identifies this binary by its size and modification time, which every
/// rebuild and reinstall changes. A binary whose metadata cannot be read
/// gets one fixed id, which still keeps its cache apart from rustc's own.
fn build_id() -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(meta) = env::current_exe().and_then(fs::metadata) {
        meta.len().hash(&mut hasher);
        meta.modified().ok().hash(&mut hasher);
    }
    hasher.finish()
}

//! `cargo mordant` end to end: the binaries `cargo test` builds, run the way
//! a user runs them, over a one-package workspace each test writes afresh.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CARGO_MORDANT: &str = env!("CARGO_BIN_EXE_cargo-mordant");

const MAIN: &str = "fn fallible() -> Result<u32, u32> {\n    Err(1)\n}\n\n\
                    fn main() {\n    fallible().ok();\n}\n";

const MAIN_TWO_TO_FIX: &str = r#"fn main() {
    "1".parse::<u32>().ok();
    let _ = "1".parse::<u32>().map_err(|e| e.to_string());
}
"#;

/// A package `demo` holding `files`, its own workspace.
fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("create the workspace");
    fs::create_dir_all(root.join("tests")).expect("create the workspace");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("write Cargo.toml");
    for (path, text) in files {
        fs::write(root.join(path), text).expect("write a source file");
    }
    root
}

fn cargo_mordant(root: &Path) -> Output {
    cargo_mordant_with(root, &[], &[])
}

/// `cargo mordant <args>` in `root`, with `env` added.
fn cargo_mordant_with(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(CARGO_MORDANT)
        .arg("mordant")
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("MORDANT_TOML")
        .env_remove("MORDANT_RUSTFLAGS")
        .envs(env.iter().copied())
        .output()
        .expect("run cargo-mordant")
}

fn stderr(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The stderr of a run where a crate went over its baseline count: it
/// exits with status 101 and prints one closing `error: mordant:` line.
fn stderr_over(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(101), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("error: mordant: ")
                && line.contains(" finding(s) over the baseline in ")),
        "the run exited with 101 but printed no closing error for the findings over the baseline:\n{stderr}"
    );
    stderr
}

/// A `mordant.toml` that names `mordant-baseline.toml` as the baseline file.
const MORDANT_TOML: &str = "[mordant]\nbaseline = \"mordant-baseline.toml\"\n";

/// A package `demo` holding `files`, plus `mordant.toml` naming
/// `mordant-baseline.toml` as the baseline file and an empty
/// `mordant-baseline.toml`. The empty file gives every lint in every file a
/// baseline count of zero, the number of findings the baseline allows.
fn baseline_workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let baseline_files = [
        ("mordant.toml", MORDANT_TOML),
        ("mordant-baseline.toml", ""),
    ];
    let files: Vec<_> = files.iter().copied().chain(baseline_files).collect();
    workspace(name, &files)
}

/// A package with `main` as `src/main.rs`, whose `discarded_error` findings are
/// over a baseline count of zero.
fn over_baseline_workspace(name: &str, main: &str) -> PathBuf {
    baseline_workspace(name, &[("src/main.rs", main)])
}

/// The `<section> <count>` lines of `over-baseline.txt` for the
/// `warning: mordant: <count> finding(s) over the baseline in <section>`
/// lines in `stderr`, sorted as `over-baseline.txt` sorts them: by section,
/// then by count.
fn over_baseline_from_warnings(stderr: &str) -> String {
    let mut pairs: Vec<(&str, usize)> = stderr
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("warning: mordant: ")?;
            let (count, section) = rest.split_once(" finding(s) over the baseline in ")?;
            Some((section, count.parse().ok()?))
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(section, count)| format!("{section} {count}\n"))
        .collect()
}

/// The configuration reaches the lints, and a change to it reruns them on a
/// crate cargo would otherwise have left alone.
#[test]
fn mordant_toml_is_read_and_a_change_to_it_rechecks() {
    let root = workspace("config", &[("src/main.rs", MAIN)]);
    let first = stderr(&cargo_mordant(&root));
    assert!(first.contains("#[warn(discarded_error)]"), "{first}");

    fs::write(
        root.join("mordant.toml"),
        "[mordant]\ndisabled = [\"discarded_error\"]\n",
    )
    .expect("write mordant.toml");
    let disabled = stderr(&cargo_mordant(&root));
    assert!(disabled.contains("Checking demo"), "{disabled}");
    assert!(!disabled.contains("discarded_error"), "{disabled}");

    fs::remove_file(root.join("mordant.toml")).expect("remove mordant.toml");
    let removed = stderr(&cargo_mordant(&root));
    assert!(removed.contains("#[warn(discarded_error)]"), "{removed}");
}

/// The baseline is an input of every compilation too. A run that writes it
/// reruns the lints on a crate cargo would otherwise have left alone, and so
/// does a change to the file.
#[test]
fn a_baseline_write_and_a_change_to_the_baseline_recheck() {
    let root = workspace(
        "baseline_inputs",
        &[
            ("src/main.rs", MAIN),
            (
                "mordant.toml",
                "[mordant]\nbaseline = \"mordant-baseline.toml\"\n",
            ),
        ],
    );
    let unheld = stderr(&cargo_mordant(&root));
    assert!(unheld.contains("#[warn(discarded_error)]"), "{unheld}");

    stderr(&cargo_mordant_with(
        &root,
        &[],
        &[("MORDANT_BASELINE_WRITE", "1")],
    ));
    let baseline =
        fs::read_to_string(root.join("mordant-baseline.toml")).expect("baseline written");
    assert!(
        baseline.contains("\"discarded_error:src/main.rs\" = 1"),
        "{baseline}"
    );
    let held = stderr(&cargo_mordant(&root));
    assert!(!held.contains("discarded_error"), "{held}");

    fs::write(root.join("mordant-baseline.toml"), "").expect("empty the baseline");
    let over = stderr_over(&cargo_mordant(&root));
    assert!(
        over.contains("`discarded_error` over the mordant baseline (0 recorded for src/main.rs)"),
        "{over}"
    );
}

const TWO_DISCARDS: &str = "fn fallible() -> Result<u32, u32> {\n    Err(1)\n}\n\n\
                            fn new() {\n    fallible().ok();\n}\n\n\
                            fn old() {\n    fallible().ok();\n}\n\n\
                            fn main() {\n    new();\n    old();\n}\n";

/// The baseline holds a count for a lint and a file, not which findings. A
/// file that goes over shows every finding of that lint, and says how many
/// the baseline allows: hiding the first ones in the file would hide a new
/// finding written above the old ones, and show an old one in its place.
#[test]
fn a_file_over_the_baseline_shows_every_finding_of_the_lint() {
    let root = workspace(
        "baseline_shows_all",
        &[
            (
                "src/main.rs",
                &TWO_DISCARDS
                    .replace("fn new() {\n    fallible().ok();\n}\n\n", "")
                    .replace("    new();\n", ""),
            ),
            (
                "mordant.toml",
                "[mordant]\nbaseline = \"mordant-baseline.toml\"\n",
            ),
        ],
    );
    stderr(&cargo_mordant_with(
        &root,
        &[],
        &[("MORDANT_BASELINE_WRITE", "1")],
    ));
    let held = stderr(&cargo_mordant(&root));
    assert!(!held.contains("discarded_error"), "{held}");

    fs::write(root.join("src/main.rs"), TWO_DISCARDS).expect("add a finding above the old one");
    let over = stderr_over(&cargo_mordant(&root));
    // The new one, in `new`, and the old one, in `old`.
    for shown in ["--> src/main.rs:6:5", "--> src/main.rs:10:5"] {
        assert!(over.contains(shown), "{shown}: {over}");
    }
    assert_eq!(
        over.matches("`discarded_error` over the mordant baseline (1 recorded for src/main.rs)")
            .count(),
        2,
        "{over}"
    );
    assert!(
        over.contains("2 `discarded_error` findings in src/main.rs, and the baseline allows 1"),
        "{over}"
    );
    assert!(
        over.contains("warning: mordant: 1 finding(s) over the baseline in demo (bin demo)"),
        "{over}"
    );
    assert!(
        over.contains("error: mordant: 1 finding(s) over the baseline in `demo (bin demo)`"),
        "{over}"
    );
    let status = fs::read_to_string(root.join("target/mordant/over-baseline.txt")).expect("status");
    assert_eq!(status, "demo (bin demo) 1\n");
}

/// A second `cargo mordant` run that does not compile the crate again still
/// prints the compiler warning cargo saved from the last `mordant-driver`
/// run and `over-baseline.txt` is written even if it was deleted.
#[test]
fn over_baseline_is_rewritten_when_the_crate_is_not_compiled_again() {
    let root = over_baseline_workspace("over_baseline_cached", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    let expected = over_baseline_from_warnings(&first);
    assert!(!expected.is_empty(), "{first}");
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "{first}"
    );

    fs::remove_file(&over_baseline).expect("delete over-baseline.txt");
    let second = stderr_over(&cargo_mordant(&root));
    assert!(
        !second.contains("Checking demo"),
        "cargo compiled the crate again, so this run does not show that `cargo mordant` reads the counts of a crate it did not compile again:\n{second}"
    );
    let replayed = over_baseline_from_warnings(&second);
    assert_eq!(replayed, expected, "{second}");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        replayed,
        "the compiler warning says this crate is over its baseline count, but over-baseline.txt was not written:\n{second}"
    );
}

/// The compiler warnings come before cargo's `Finished` line on stderr, on a
/// run that compiles the crate and on one that replays the saved warnings
/// alike. That is where cargo itself prints them.
#[test]
fn warnings_come_before_the_finished_line_on_a_cold_and_a_warm_run() {
    let root = over_baseline_workspace("over_baseline_ordering", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    assert!(
        first.contains("Checking demo"),
        "cargo did not compile the crate:\n{first}"
    );
    assert_warnings_before_finished("the run that compiled the crate", &first);

    let second = stderr_over(&cargo_mordant(&root));
    assert!(
        !second.contains("Checking demo"),
        "cargo compiled the crate again, so this does not show a replayed run:\n{second}"
    );
    assert_warnings_before_finished("the run that replayed the saved warnings", &second);
}

/// The first line starting `warning:` in `stderr` comes before the line
/// holding `Finished`.
fn assert_warnings_before_finished(run: &str, stderr: &str) {
    let warning = stderr
        .lines()
        .find(|line| line.starts_with("warning:"))
        .and_then(|line| stderr.find(line));
    let finished = stderr.find("Finished");
    assert!(
        warning.is_some(),
        "{run} printed no `warning:` line:\n{stderr}"
    );
    assert!(
        finished.is_some(),
        "{run} printed no `Finished` line:\n{stderr}"
    );
    assert!(
        warning < finished,
        "{run} printed `Finished` before the first `warning:` line:\n{stderr}"
    );
}

/// Fixing all findings and running `cargo mordant` again deletes
/// `over-baseline.txt`. The `<section> <count>` line from the earlier run does
/// not stay behind.
#[test]
fn over_baseline_is_removed_when_the_finding_is_fixed() {
    let root = over_baseline_workspace("over_baseline_fixed", MAIN_TWO_TO_FIX);
    let first = stderr_over(&cargo_mordant(&root));
    assert!(!over_baseline_from_warnings(&first).is_empty(), "{first}");
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert!(
        over_baseline.is_file(),
        "the first run did not write over-baseline.txt:\n{first}"
    );

    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("fix the finding");
    let fixed = stderr(&cargo_mordant(&root));
    assert!(
        fixed.contains("Checking demo"),
        "cargo did not compile the crate again:\n{fixed}"
    );
    assert!(
        over_baseline_from_warnings(&fixed).is_empty(),
        "the fixed crate is still over its baseline count:\n{fixed}"
    );
    let left = fs::read_to_string(&over_baseline).ok();
    assert!(
        left.is_none(),
        "over-baseline.txt still holds {left:?} after the finding was fixed"
    );
}

/// Fixing one finding and running `cargo mordant` again replaces
/// `over-baseline.txt` with the crate's new `<section> <count>` line, whose
/// count is now 1.
#[test]
fn over_baseline_is_replaced_when_one_finding_is_fixed() {
    let root = over_baseline_workspace("over_baseline_one_fixed", MAIN_TWO_TO_FIX);
    let first = stderr_over(&cargo_mordant(&root));
    assert!(first.contains("2 finding(s) over the baseline"), "{first}");
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert!(
        over_baseline.is_file(),
        "the first run did not write over-baseline.txt:\n{first}"
    );

    fs::write(root.join("src/main.rs"), MAIN).expect("fix one of two findings");
    let fixed = stderr_over(&cargo_mordant(&root));
    assert!(
        fixed.contains("Checking demo"),
        "cargo did not compile the crate again:\n{fixed}"
    );
    assert!(fixed.contains("1 finding(s) over the baseline"), "{fixed}");
    let replayed = over_baseline_from_warnings(&fixed);
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        replayed,
        "compiling again appended another line to over-baseline.txt:\n{fixed}"
    );
}

/// Compiling the crate again replaces `over-baseline.txt` with this run's
/// `<section> <count>` line. It does not append a second copy of that line.
#[test]
fn over_baseline_is_replaced_when_the_crate_is_compiled_again() {
    let root = over_baseline_workspace("over_baseline_replaced", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    let expected = over_baseline_from_warnings(&first);
    assert!(
        !expected.is_empty(),
        "no crate was reported over its baseline count, so this run does not show what over-baseline.txt holds:\n{first}"
    );
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "{first}"
    );

    fs::write(
        root.join("src/main.rs"),
        format!("// the finding is unchanged\n{MAIN}"),
    )
    .expect("edit the source without removing the finding");
    let second = stderr_over(&cargo_mordant(&root));
    assert!(
        second.contains("Checking demo"),
        "cargo did not compile the crate again:\n{second}"
    );
    let replayed = over_baseline_from_warnings(&second);
    assert_eq!(replayed, expected, "{second}");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        replayed,
        "compiling again appended another line to over-baseline.txt:\n{second}"
    );
}

/// A build script over its baseline count gets a `build_script_build <count>`
/// line in `target/mordant/over-baseline.txt`, like any other crate. That
/// happens on the run that compiles the build script, and again on a run that
/// does not compile it after `over-baseline.txt` was deleted.
#[test]
fn over_baseline_holds_a_build_script_past_its_baseline_count() {
    let root = baseline_workspace(
        "over_baseline_build_script",
        &[("src/main.rs", "fn main() {}\n"), ("build.rs", MAIN)],
    );
    let first = stderr_over(&cargo_mordant(&root));
    assert!(
        first.contains("1 finding(s) over the baseline in build_script_build"),
        "the build script was not reported over its baseline count:\n{first}"
    );
    let expected = over_baseline_from_warnings(&first);
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "the build script is over its baseline count, but over-baseline.txt does not say so:\n{first}"
    );

    fs::remove_file(&over_baseline).expect("delete over-baseline.txt");
    let second = stderr_over(&cargo_mordant(&root));
    assert!(
        !second.contains("Compiling demo") && !second.contains("Checking demo"),
        "cargo compiled the package again, so this does not show a run that skips the build script:\n{second}"
    );
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "a run that did not compile the build script did not write its line to over-baseline.txt:\n{second}"
    );
}

/// A baseline file in a member's directory, and not at the workspace root,
/// still puts that member's `<section> <count>` line in
/// `target/mordant/over-baseline.txt` under the workspace root. The
/// compilation of `demo` finds `demo/mordant-baseline.toml` by searching
/// upward from `demo/`, and `cargo mordant` has to write the count that
/// compilation found.
#[test]
fn over_baseline_holds_a_member_whose_baseline_file_is_below_the_workspace_root() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("over_baseline_member_baseline");
    let _ = fs::remove_dir_all(&root);
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"demo\"]\nresolver = \"2\"\n",
        ),
        ("mordant.toml", MORDANT_TOML),
        (
            "demo/Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("demo/src/main.rs", MAIN),
        ("demo/mordant-baseline.toml", ""),
    ];
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a file in the workspace"))
            .expect("create the member");
        fs::write(path, text).expect("write a workspace file");
    }

    let out = stderr_over(&cargo_mordant(&root));
    assert!(
        out.contains("1 finding(s) over the baseline in demo"),
        "the compilation of demo did not find demo/mordant-baseline.toml and report demo over its baseline count:\n{out}"
    );
    let expected = over_baseline_from_warnings(&out);
    assert!(
        !expected.is_empty(),
        "no crate was reported over its baseline count, so this run does not show what over-baseline.txt holds:\n{out}"
    );
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "demo is over its baseline count, but over-baseline.txt does not say so:\n{out}"
    );
}

/// Under `--all-targets`, cargo compiles each test build (the library's
/// unit tests, the binary's unit tests and `tests/it.rs`) as its own unit.
/// Each of those units writes its `.over` file to
/// `target/mordant/over_baseline_counts`, as the library and the binary do,
/// so `cargo mordant` finds a `.over` file for every unit and writes
/// `target/mordant/over-baseline.txt`. With `MORDANT_BASELINE_WRITE=1`, the
/// library's unit-test build shares the `[demo]` section with the library
/// build but runs only `unused_pub`, so it leaves that section alone, and
/// `mordant-baseline.toml` keeps the library's `discarded_error` entry. A
/// write-mode run of `--lib --profile test` compiles only that unit-test
/// build, so nothing else rewrites the section after it.
#[test]
fn test_builds_write_their_over_files_and_leave_the_baseline_section_alone() {
    let root = baseline_workspace(
        "over_baseline_test_builds",
        &[
            (
                "src/lib.rs",
                "fn fallible() -> Result<u32, u32> {\n    Err(1)\n}\n\n\
                 pub fn run() {\n    fallible().ok();\n}\n\n\
                 #[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
            ),
            ("src/main.rs", "fn main() {}\n"),
            ("tests/it.rs", "#[test]\nfn t() {\n    demo::run();\n}\n"),
        ],
    );
    let over_baseline = root.join("target/mordant/over-baseline.txt");

    let ratchet = cargo_mordant_with(&root, &["--all-targets"], &[]);
    let ratchet_stderr = String::from_utf8_lossy(&ratchet.stderr).into_owned();
    assert!(
        !ratchet_stderr.contains("was not written"),
        "a unit wrote no .over file, so over-baseline.txt was not written:\n{ratchet_stderr}"
    );
    let ratchet_stderr = stderr_over(&ratchet);
    assert!(
        ratchet_stderr.contains("1 finding(s) over the baseline in demo"),
        "the library was not reported over its baseline count:\n{ratchet_stderr}"
    );
    // Exactly the library's line: a test build that also reported a count
    // would print its own summary warning, so comparing with the warnings
    // would not catch it.
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        "demo 1\n",
        "over-baseline.txt should hold only the library's line, not one from a test build:\n{ratchet_stderr}"
    );
    let over_files = fs::read_dir(root.join("target/mordant/over_baseline_counts"))
        .expect("read target/mordant/over_baseline_counts")
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.path().extension().is_some_and(|ext| ext == "over"))
        })
        .count();
    // The library and the binary, then the library's unit tests, the
    // binary's unit tests and `tests/it.rs`.
    assert!(
        over_files >= 5,
        "only {over_files} .over files for 2 non-test units and 3 test units, \
         so a test build did not write its .over file:\n{ratchet_stderr}"
    );

    let write_stderr = stderr(&cargo_mordant_with(
        &root,
        &["--all-targets"],
        &[("MORDANT_BASELINE_WRITE", "1")],
    ));
    let baseline =
        fs::read_to_string(root.join("mordant-baseline.toml")).expect("read mordant-baseline.toml");
    assert!(
        baseline.contains("[demo]") && baseline.contains("\"discarded_error:src/lib.rs\" = 1"),
        "mordant-baseline.toml lost the library's discarded_error entry, \
         so a test build replaced the [demo] section:\n{baseline}\n{write_stderr}"
    );

    let test_only_stderr = stderr(&cargo_mordant_with(
        &root,
        &["--lib", "--profile", "test"],
        &[("MORDANT_BASELINE_WRITE", "1")],
    ));
    assert!(
        test_only_stderr.contains("Checking demo"),
        "cargo did not compile the library's unit-test build:\n{test_only_stderr}"
    );
    let baseline =
        fs::read_to_string(root.join("mordant-baseline.toml")).expect("read mordant-baseline.toml");
    assert!(
        baseline.contains("[demo]") && baseline.contains("\"discarded_error:src/lib.rs\" = 1"),
        "the library's unit-test build replaced the [demo] section in \
         mordant-baseline.toml:\n{baseline}\n{test_only_stderr}"
    );

    let held_stderr = stderr(&cargo_mordant_with(&root, &["--all-targets"], &[]));
    assert!(
        !over_baseline.exists(),
        "the library is within its baseline count, but over-baseline.txt is still there:\n{held_stderr}"
    );
}

/// After a crate's diagnostics, cargo prints its per-crate count line on
/// stderr: `warning: `demo` (bin "demo") generated N warning(s)`. That line
/// is printed on a run that compiles the crate.
#[test]
fn cargo_prints_its_warning_count_line_after_the_crates_diagnostics() {
    let root = over_baseline_workspace("warning_count_line", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    assert!(
        first.contains("Checking demo"),
        "cargo did not compile the crate:\n{first}"
    );
    assert!(
        first.lines().any(|line| {
            line.starts_with("warning: `demo` (bin \"demo\") generated ")
                && line.contains(" warning")
        }),
        "cargo's `warning: `demo` (bin \"demo\") generated N warning(s)` line is missing from stderr:\n{first}"
    );
}

/// `mordant-action` reads lint names off this, one indented
/// `name  level  description` line per lint under a `mordant` heading.
#[test]
fn list_prints_each_lint_with_its_level() {
    let out = Command::new(CARGO_MORDANT)
        .arg("--list")
        .output()
        .expect("run cargo-mordant --list");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("the list is UTF-8");
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("mordant"));
    let names: Vec<&str> = lines
        .map(|line| {
            assert!(line.starts_with("    "), "{line:?}");
            let mut words = line.split_whitespace();
            let name = words.next().expect("a name");
            assert!(
                matches!(words.next(), Some("warn" | "allow" | "deny" | "forbid")),
                "{line:?}"
            );
            name
        })
        .collect();
    assert!(names.contains(&"discarded_error"), "{names:?}");
    assert!(names.contains(&"unused_pub"), "{names:?}");
}

/// `cargo new` names a package's library and binary alike, and what the
/// binary calls in the library is used.
#[test]
fn unused_pub_counts_uses_from_a_binary_named_like_its_library() {
    let root = workspace(
        "same_name",
        &[
            (
                "src/lib.rs",
                "pub fn called() {}\n\npub fn never_called() {}\n",
            ),
            ("src/main.rs", "fn main() {\n    demo::called();\n}\n"),
        ],
    );
    let out = stderr(&cargo_mordant(&root));
    assert!(out.contains("`demo::never_called` is public"), "{out}");
    assert!(!out.contains("`demo::called` is public"), "{out}");
}

const LIB: &str = "pub fn by_unit_test() {}\n\npub fn by_integration_test() {}\n\n\
                   pub fn by_nothing() {}\n\n\
                   #[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        \
                   super::by_unit_test();\n    }\n}\n";

const BIN: &str = "pub fn by_bin_test() {}\n\nfn main() {}\n\n\
                   #[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        \
                   super::by_bin_test();\n    }\n}\n";

const INTEGRATION_TEST: &str = "#[test]\nfn t() {\n    demo::by_integration_test();\n}\n";

fn tested(name: &str) -> PathBuf {
    workspace(
        name,
        &[
            ("src/lib.rs", LIB),
            ("src/main.rs", BIN),
            ("tests/it.rs", INTEGRATION_TEST),
        ],
    )
}

/// With `--all-targets` the tests are part of the run, and what they use is
/// used. Without it they are not, and the records they left in the run
/// before are not read.
#[test]
fn unused_pub_counts_what_tests_use_when_they_are_built() {
    let root = tested("tests_count");
    let all = stderr(&cargo_mordant_with(&root, &["--all-targets"], &[]));
    assert!(all.contains("`demo::by_nothing` is public"), "{all}");
    for used in ["by_unit_test", "by_integration_test", "by_bin_test"] {
        assert!(
            !all.contains(&format!("::{used}` is public")),
            "{used}: {all}"
        );
    }

    let plain = stderr(&cargo_mordant(&root));
    for unused in [
        "by_nothing",
        "by_unit_test",
        "by_integration_test",
        "by_bin_test",
    ] {
        assert!(
            plain.contains(&format!("::{unused}` is public")),
            "{unused}: {plain}"
        );
    }
}

/// A `cfg(test)` impl block gives the impl blocks after it one number in the
/// test build and another in the crate's own build. A unit test's use still
/// counts for the method it calls, and not for the method of the same name
/// in the next impl block.
#[test]
fn unused_pub_matches_a_unit_tests_use_past_a_cfg_test_impl() {
    let root = workspace(
        "cfg_test_impl",
        &[(
            "src/lib.rs",
            "pub struct A;\n\n#[cfg(test)]\nimpl A {\n    fn only_in_tests(&self) {}\n}\n\n\
             impl A {\n    pub fn get(&self) -> B {\n        B\n    }\n}\n\n\
             pub struct B;\n\nimpl B {\n    pub fn get(&self) {}\n}\n\n\
             #[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        \
             super::A.only_in_tests();\n        let _ = super::A.get();\n    }\n}\n",
        )],
    );
    let out = stderr(&cargo_mordant_with(&root, &["--all-targets"], &[]));
    assert!(out.contains("`demo::B::get` is public"), "{out}");
    assert!(!out.contains("`demo::A::get` is public"), "{out}");
}

/// A crate that cargo compiles more than once with can have different pub
// function usages in different builds. Adding an integration test makes
/// cargo compile with panic = "unwind" when --all-targets is used.
/// When checking for unused pub functions, we must check all builds.
#[test]
fn unused_pub_unions_uses_from_abort_and_unwind_builds() {
    let root = workspace(
        "abort_and_unwind_builds",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n\n\
                 [profile.dev]\npanic = \"abort\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn by_abort() {}\n\
                 pub fn by_unwind() {}\n\
                 pub fn start() {\n    #[cfg(panic = \"abort\")]\n    by_abort();\n    \
                 #[cfg(panic = \"unwind\")]\n    by_unwind();\n}\n",
            ),
            ("tests/it.rs", "#[test]\nfn t() { demo::start(); }\n"),
        ],
    );
    let out = cargo_mordant_with(
        &root,
        &["--all-targets"],
        &[("MORDANT_RUSTFLAGS", "-D warnings")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("`demo::by_abort` is public"), "{stderr}");
    assert!(!stderr.contains("`demo::by_unwind` is public"), "{stderr}");
}

/// A crate that cargo compiles more than once in a run with different compile
/// time targets enabled can have different pub function usages in different
/// builds. When checking for unused pub functions, we must check all builds.
#[test]
fn unused_pub_checks_all_builds() {
    let root = workspace(
        "two_build_targets",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n\n\
                 [[test]]\nname = \"with_unix\"\npath = \"tests/start.rs\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn by_unix() {}\n\
                pub fn by_windows() {}\n\
                pub fn start() {\n    #[cfg(unix)]\n    by_unix();\n    \
                #[cfg(windows)]\n    by_windows();\n}\n",
            ),
            ("tests/start.rs", "#[test]\nfn t() { demo::start(); }\n"),
        ],
    );
    let out = cargo_mordant_with(
        &root,
        &[
            "--all-targets",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--target",
            "x86_64-pc-windows-msvc",
        ],
        &[("MORDANT_RUSTFLAGS", "-D warnings")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("`demo::by_unix` is public"), "{stderr}");
    assert!(!stderr.contains("`demo::by_windows` is public"), "{stderr}");
}

/// A `cfg(windows)` impl block gives the impl blocks after it one number on
/// Windows and another elsewhere, so one item has two keys in a run over both
/// targets. A use under either target still counts for it, and an unused
/// item is one finding.
#[test]
fn unused_pub_knows_one_item_under_the_keys_of_two_targets() {
    let root = members(
        "two_target_keys",
        "pub struct A;\n\n#[cfg(windows)]\nimpl A {\n    pub fn only_there(&self) {}\n}\n\n\
         impl A {\n    pub fn by_windows(&self) {}\n\n    pub fn by_nothing(&self) {}\n}\n",
        "fn main() {\n    let a = a::A;\n    #[cfg(windows)]\n    {\n        \
         a.only_there();\n        a.by_windows();\n    }\n    let _ = a;\n}\n",
    );
    let out = stderr(&cargo_mordant_with(
        &root,
        &[
            "--workspace",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--target",
            "x86_64-pc-windows-msvc",
        ],
        &[],
    ));
    assert_eq!(
        out.matches("`a::A::by_nothing` is public").count(),
        1,
        "{out}"
    );
    assert!(!out.contains("`a::A::by_windows` is public"), "{out}");
    assert!(!out.contains("`a::A::only_there` is public"), "{out}");
}

/// A workspace of three members: the library `a`; the binary `b`, which
/// depends on it; and `c`, which depends on it too and whose only target
/// wants a feature that is off, so `--workspace` selects it and builds
/// nothing of it.
fn members(name: &str, a_lib: &str, b_main: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&root);
    let package = |name: &str, rest: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{rest}")
    };
    let on_a = "[dependencies]\na = { path = \"../a\" }\n";
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n".to_string(),
        ),
        ("a/Cargo.toml", package("a", "")),
        ("a/src/lib.rs", a_lib.to_string()),
        ("b/Cargo.toml", package("b", on_a)),
        ("b/src/main.rs", b_main.to_string()),
        (
            "c/Cargo.toml",
            package(
                "c",
                &format!(
                    "[features]\nextra = []\n\n[[bin]]\nname = \"c\"\npath = \"src/main.rs\"\n\
                     required-features = [\"extra\"]\n\n{on_a}"
                ),
            ),
        ),
        ("c/src/main.rs", "fn main() {}\n".to_string()),
    ];
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a file in the workspace"))
            .expect("create the member");
        fs::write(path, text).expect("write a workspace file");
    }
    root
}

/// A run of part of the workspace cannot see what the rest of it uses, so it
/// leaves alone a member that something outside the run depends on. A run
/// of the whole workspace judges it, also when it builds nothing of one of
/// its dependents.
#[test]
fn unused_pub_judges_a_member_only_with_its_dependents_in_the_run() {
    let root = members(
        "partial",
        "pub fn by_b() {}\n\npub fn by_nothing() {}\n",
        "fn main() {\n    a::by_b();\n}\n",
    );
    for run in [&["-p", "a"][..], &["--workspace", "--exclude", "b"][..]] {
        let out = stderr(&cargo_mordant_with(&root, run, &[]));
        assert!(!out.contains("is public, but"), "{run:?}: {out}");
    }

    let whole = stderr(&cargo_mordant_with(&root, &["--workspace"], &[]));
    assert!(whole.contains("`a::by_nothing` is public"), "{whole}");
    assert!(!whole.contains("`a::by_b` is public"), "{whole}");
}

/// Asked for JSON, cargo's messages come through and the findings arrive as
/// cargo would have printed them, for the package whose file they are in.
#[test]
fn unused_pub_findings_are_compiler_messages_in_json() {
    let root = tested("json");
    let out = cargo_mordant_with(&root, &["--message-format=json"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let messages: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
        .collect();
    assert!(messages.iter().any(|m| m["reason"] == "compiler-artifact"));
    let finding = messages
        .iter()
        .find(|m| m["reason"] == "compiler-message" && m["message"]["code"]["code"] == "unused_pub")
        .expect("an unused_pub compiler-message");
    assert!(
        finding["package_id"]
            .as_str()
            .is_some_and(|id| id.contains("demo")),
        "{finding}"
    );
    assert_eq!(finding["message"]["spans"][0]["file_name"], "src/lib.rs");
}

/// `MORDANT_RUSTFLAGS="-D warnings"` fails the run on an unused item as on
/// any other finding.
#[test]
fn unused_pub_findings_fail_the_run_under_deny_warnings() {
    let root = tested("deny");
    let out = cargo_mordant_with(
        &root,
        &["--all-targets"],
        &[("MORDANT_RUSTFLAGS", "-D warnings")],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: function `demo::by_nothing` is public"),
        "{stderr}"
    );
}

/// A unit cargo does not rebuild is judged from what it recorded before;
/// with those records gone, `unused_pub` says so instead of calling
/// everything unused, and the run fails: it judged nothing, which is not the
/// same as finding nothing.
#[test]
fn unused_pub_names_the_units_whose_records_are_missing() {
    let root = tested("missing");
    stderr(&cargo_mordant_with(&root, &["--all-targets"], &[]));
    fs::remove_dir_all(root.join("target/mordant/unused_pub")).expect("remove the records");
    let out = cargo_mordant_with(&root, &["--all-targets"], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("error: mordant: `unused_pub` did not judge the workspace"),
        "{stderr}"
    );
    // Named once, though the run built it twice: on its own and as a test.
    assert_eq!(stderr.matches("`src/lib.rs`").count(), 1, "{stderr}");
    assert!(!stderr.contains("is public, but"), "{stderr}");
}

/// Disabling unused_pub in the config means unused pub items
/// must not fail a run and unused pub items must not be reported.
#[test]
fn unused_pub_disabled() {
    let root = workspace(
        "unused_pub_disabled",
        &[
            ("src/lib.rs", "pub fn unused() {}\n"),
            ("mordant.toml", "[mordant]\ndisabled = [\"unused_pub\"]\n"),
        ],
    );
    let out = cargo_mordant_with(&root, &[], &[("MORDANT_RUSTFLAGS", "-D warnings")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("is public"), "{stderr}");
    // when unused_pub is disabled, the "did not judge" error should not be reported
    assert!(!stderr.contains("did not judge the workspace"), "{stderr}");
}

/// Running with unused_pub enabled, then immediately disabling
/// unused_pub in the config. In this situation, unused pub items
/// must not be reported and must not fail a run.
#[test]
fn unused_pub_disabled_after_enabled() {
    let root = workspace(
        "unused_pub_disabled_after_enabled",
        &[
            ("src/lib.rs", "pub fn unused() {}\n"),
            ("mordant.toml", "[mordant]\n"),
        ],
    );
    let out = cargo_mordant_with(&root, &[], &[("MORDANT_RUSTFLAGS", "-D warnings")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("is public"), "{stderr}");
    fs::write(
        root.join("mordant.toml"),
        "[mordant]\ndisabled = [\"unused_pub\"]\n",
    )
    .expect("write the config");
    let out = cargo_mordant_with(&root, &[], &[("MORDANT_RUSTFLAGS", "-D warnings")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("is public"), "{stderr}");
    // when unused_pub is disabled, the "did not judge" error should not be reported
    assert!(!stderr.contains("did not judge the workspace"), "{stderr}");
    // If the records are removed, the "did not judge" error should still be skipped
    // unlike unused_pub_names_the_units_whose_records_are_missing
    fs::remove_dir_all(root.join("target/mordant/unused_pub")).expect("remove the records");
    let out = cargo_mordant_with(&root, &[], &[("MORDANT_RUSTFLAGS", "-D warnings")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("is public"), "{stderr}");
    assert!(!stderr.contains("did not judge the workspace"), "{stderr}");
}

/// With `unused_pub` disabled in `mordant.toml`, `cargo mordant` still writes
/// the `demo 1` line for the `discarded_error` finding over the baseline count
/// to `target/mordant/over-baseline.txt`. It writes the same line again on a
/// run that does not compile the crate, after `over-baseline.txt` was deleted.
#[test]
fn over_baseline_is_written_when_unused_pub_is_disabled() {
    let root = workspace(
        "over_baseline_unused_pub_disabled",
        &[
            ("src/main.rs", MAIN),
            (
                "mordant.toml",
                "[mordant]\nbaseline = \"mordant-baseline.toml\"\ndisabled = [\"unused_pub\"]\n",
            ),
            ("mordant-baseline.toml", ""),
        ],
    );
    let first = stderr_over(&cargo_mordant(&root));
    assert!(
        first.contains("1 finding(s) over the baseline in demo"),
        "{first}"
    );
    let expected = over_baseline_from_warnings(&first);
    assert!(!expected.is_empty(), "{first}");
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "{first}"
    );

    fs::remove_file(&over_baseline).expect("delete over-baseline.txt");
    let second = stderr_over(&cargo_mordant(&root));
    assert!(
        !second.contains("Checking demo"),
        "cargo compiled the crate again, so this run does not show that `cargo mordant` reads the counts of a crate it did not compile again:\n{second}"
    );
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        expected,
        "over-baseline.txt was not written again with `unused_pub` disabled:\n{second}"
    );
}

/// Each compilation writes its count of findings over the baseline count to
/// `target/mordant/over_baseline_counts`. With that directory removed and the
/// crate not compiled again, `cargo mordant` fails, prints on stderr that
/// `over-baseline.txt` was not written and which source file left no count,
/// and leaves `over-baseline.txt` as the first run wrote it.
#[test]
fn over_baseline_is_not_written_when_the_counts_are_missing() {
    let root = over_baseline_workspace("over_baseline_counts_missing", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    let written = fs::read_to_string(&over_baseline);
    assert!(
        written.is_ok(),
        "the first run did not write over-baseline.txt:\n{first}"
    );

    fs::remove_dir_all(root.join("target/mordant/over_baseline_counts"))
        .expect("remove the over-baseline counts");
    let out = cargo_mordant(&root);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Checking demo"),
        "cargo compiled the crate again, so its count was rewritten and this does not show a missing count:\n{stderr}"
    );
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("error: mordant: `over-baseline.txt` was not written"),
        "{stderr}"
    );
    assert!(stderr.contains("`src/main.rs`"), "{stderr}");
    assert!(
        !stderr.contains("did not judge the workspace"),
        "the unused_pub records are still there, but unused_pub said they were missing:\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&over_baseline).ok(),
        written.ok(),
        "over-baseline.txt changed although it was not written:\n{stderr}"
    );
}

/// Each compilation writes its count of findings over the baseline count to a
/// `.over` file in `target/mordant/over_baseline_counts`. With the count in
/// that file replaced by text that is not a number, and the crate not
/// compiled again, `cargo mordant` treats the count as missing: it fails,
/// prints on stderr that `over-baseline.txt` was not written and which source
/// file left no count, and leaves `over-baseline.txt` as the first run wrote
/// it. It leaves the `.over` file as it found it, for someone to inspect.
#[test]
fn over_baseline_is_not_written_when_a_count_is_not_a_number() {
    let root = over_baseline_workspace("over_baseline_count_not_a_number", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    let over_baseline = root.join("target/mordant/over-baseline.txt");
    let written = fs::read_to_string(&over_baseline).unwrap_or_default();
    assert!(
        !written.is_empty(),
        "the first run did not write the crate into over-baseline.txt:\n{first}"
    );

    let counts: Vec<_> = fs::read_dir(root.join("target/mordant/over_baseline_counts"))
        .expect("read the over-baseline counts")
        .map(|entry| entry.expect("an over-baseline count").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "over"))
        .collect();
    assert_eq!(
        counts.len(),
        1,
        "the first run did not write exactly one .over file: {counts:?}\n{first}"
    );
    let over = &counts[0];
    let count = fs::read_to_string(over).expect("read the .over file");
    let (section, _) = count
        .split_once('\t')
        .unwrap_or_else(|| panic!("the .over file holds no section: {count:?}\n{first}"));
    let corrupt = format!("{section}\tnot a number\n");
    fs::write(over, &corrupt).expect("overwrite the .over file");

    let out = cargo_mordant(&root);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Checking demo"),
        "cargo compiled the crate again, so its count was rewritten and this does not show a count that is not a number:\n{stderr}"
    );
    assert!(
        !out.status.success(),
        "the run succeeded, so the count that is not a number was read as nothing over the baseline:\n{stderr}"
    );
    assert!(
        stderr.contains("error: mordant: `over-baseline.txt` was not written"),
        "{stderr}"
    );
    assert!(stderr.contains("`src/main.rs`"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&over_baseline).unwrap_or_default(),
        written,
        "over-baseline.txt changed although it was not written:\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(over).ok().as_deref(),
        Some(corrupt.as_str()),
        "the .over file holding a count that is not a number was changed or removed:\n{stderr}"
    );
}

/// With `--message-format json`, the error that `over-baseline.txt` was not
/// written arrives on stdout as a `compiler-message` at level `error`, like
/// `unused_pub`'s findings, and comes before cargo's closing
/// `build-finished` line, which says `"success": false`. The count is made
/// missing as in `over_baseline_is_not_written_when_the_counts_are_missing`.
#[test]
fn over_baseline_not_written_is_a_compiler_message_in_json() {
    let root = over_baseline_workspace("over_baseline_counts_missing_json", MAIN);
    let first = stderr_over(&cargo_mordant(&root));
    assert!(
        root.join("target/mordant/over-baseline.txt").exists(),
        "the first run did not write over-baseline.txt, stderr:\n{first}"
    );

    fs::remove_dir_all(root.join("target/mordant/over_baseline_counts"))
        .expect("remove the over-baseline counts");
    let out = cargo_mordant_with(&root, &["--message-format=json"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let messages: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
        .collect();
    assert!(
        !messages
            .iter()
            .any(|m| m["reason"] == "compiler-artifact" && m["fresh"] == false),
        "stdout shows cargo compiled a crate again, so its count was rewritten and this does not show a missing count:\n{stdout}"
    );
    assert!(
        !out.status.success(),
        "the run succeeded with a count missing, stderr:\n{stderr}"
    );
    let error = messages.iter().position(|m| {
        m["reason"] == "compiler-message"
            && m["message"]["level"] == "error"
            && m["message"]["message"]
                .as_str()
                .is_some_and(|text| text.contains("`over-baseline.txt` was not written"))
    });
    let finished = messages
        .iter()
        .position(|m| m["reason"] == "build-finished");
    let (Some(error), Some(finished)) = (error, finished) else {
        panic!(
            "stdout lacks an error compiler-message saying over-baseline.txt was not written, or a build-finished line:\n{stdout}\nstderr:\n{stderr}"
        );
    };
    assert_eq!(
        messages[finished]["success"], false,
        "the build-finished line on stdout does not say the run failed:\n{stdout}"
    );
    assert!(
        error < finished,
        "on stdout, the compiler-message saying over-baseline.txt was not written comes after build-finished:\n{stdout}"
    );
}

/// With `--message-format json`, when `cargo mordant` cannot write
/// `over-baseline.txt`, it prints `error: mordant: could not write ...` on
/// stderr and exits with code 1, and cargo's closing `build-finished` line
/// on stdout says `"success": false`. A directory made at
/// `target/mordant/over-baseline.txt` before the run stops the write: the
/// temporary file cannot be renamed onto it, and it cannot be deleted as a
/// file when there are no lines to write.
#[test]
fn over_baseline_that_cannot_be_written_is_a_failed_build_in_json() {
    let root = over_baseline_workspace("over_baseline_cannot_be_written_json", MAIN);
    fs::create_dir_all(root.join("target/mordant/over-baseline.txt"))
        .expect("make a directory where over-baseline.txt goes");

    let out = cargo_mordant_with(&root, &["--message-format=json"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the run succeeded although over-baseline.txt could not be written, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("error: mordant: could not write"),
        "stderr does not say over-baseline.txt could not be written:\n{stderr}"
    );
    let finished = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("every stdout line is JSON")
        })
        .find(|m| m["reason"] == "build-finished")
        .unwrap_or_else(|| panic!("stdout has no build-finished line:\n{stdout}"));
    assert_eq!(
        finished["success"], false,
        "the build-finished line on stdout does not say the run failed:\n{stdout}"
    );
}

/// With `--message-format json`, a crate over its baseline count fails the
/// run the same way: the closing error is a `compiler-message` at level
/// `error` on stdout, before cargo's `build-finished` line, which says
/// `"success": false`, and the exit status is 101. A second run that
/// compiles nothing again fails the same way.
#[test]
fn a_crate_over_the_baseline_fails_the_run_in_json() {
    let root = over_baseline_workspace("over_baseline_fails_json", MAIN);
    for run in [
        "the run that compiled the crate",
        "the run that compiled nothing again",
    ] {
        let out = cargo_mordant_with(&root, &["--message-format=json"], &[]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(101), "{run}, stderr:\n{stderr}");
        let messages: Vec<serde_json::Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
            .collect();
        let error = messages.iter().position(|m| {
            m["reason"] == "compiler-message"
                && m["message"]["level"] == "error"
                && m["message"]["message"]
                    .as_str()
                    .is_some_and(|text| text.contains("finding(s) over the baseline in"))
        });
        let finished = messages
            .iter()
            .position(|m| m["reason"] == "build-finished");
        let (Some(error), Some(finished)) = (error, finished) else {
            panic!(
                "{run}: stdout lacks the closing error as a compiler-message, or a build-finished line:\n{stdout}\nstderr:\n{stderr}"
            );
        };
        assert!(
            error < finished,
            "{run}: the closing error comes after build-finished:\n{stdout}"
        );
        assert_eq!(
            messages[finished]["success"], false,
            "{run}: the build-finished line does not say the run failed:\n{stdout}"
        );
        assert_eq!(
            fs::read_to_string(root.join("target/mordant/over-baseline.txt")).unwrap_or_default(),
            "demo (bin demo) 1\n",
            "{run}, stderr:\n{stderr}"
        );
    }
}

/// Test code is where the crate is exercised, not what the lints are about:
/// a test build runs `unused_pub` alone, so a finding in a `#[cfg(test)]`
/// module or an integration test is not reported, while the same finding
/// in the crate's own code is, once.
#[test]
fn only_unused_pub_runs_on_test_code() {
    let dropped = "std::fs::read(\"x\").ok();";
    let root = workspace(
        "test_code",
        &[
            (
                "src/lib.rs",
                &format!(
                    "pub fn run() {{\n    {dropped}\n}}\n\n\
                     #[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{\n        \
                     {dropped}\n        super::run();\n    }}\n}}\n"
                ),
            ),
            (
                "tests/it.rs",
                &format!("#[test]\nfn t() {{\n    {dropped}\n    demo::run();\n}}\n"),
            ),
        ],
    );
    let out = stderr(&cargo_mordant_with(&root, &["--all-targets"], &[]));
    assert_eq!(out.matches("`.ok();` converts").count(), 1, "{out}");
    assert!(out.contains("--> src/lib.rs:2:5"), "{out}");
}

/// Nor are rustc's own warnings in test code, which the nightly mordant
/// builds with can raise where the workspace's toolchain does not: under
/// `-D warnings` they would fail a run over code no mordant lint looked at.
#[test]
fn rustc_warnings_in_test_builds_do_not_fail_the_run() {
    let root = workspace(
        "test_warnings",
        &[
            ("src/lib.rs", "pub fn run() {}\n"),
            (
                "tests/it.rs",
                "#[test]\nfn t() {\n    let unused = 1;\n    demo::run();\n}\n",
            ),
        ],
    );
    let out = cargo_mordant_with(
        &root,
        &["--all-targets"],
        &[("MORDANT_RUSTFLAGS", "-D warnings")],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("unused variable"), "{stderr}");
}

/// Allowing `unused_pub` in a crate hides that crate's items, not its uses:
/// what it calls elsewhere in the workspace is still used.
#[test]
fn a_crate_allowing_unused_pub_still_records_its_uses() {
    let root = workspace(
        "allowed",
        &[
            (
                "src/lib.rs",
                "pub fn called() {}\n\npub fn never_called() {}\n",
            ),
            (
                "src/main.rs",
                "#![allow(unknown_lints, unused_pub)]\n\npub fn hidden() {}\n\n\
                 fn main() {\n    demo::called();\n}\n",
            ),
        ],
    );
    let out = stderr(&cargo_mordant(&root));
    assert!(out.contains("`demo::never_called` is public"), "{out}");
    assert!(!out.contains("`demo::called` is public"), "{out}");
    assert!(!out.contains("hidden"), "{out}");
    assert!(!out.contains("did not judge"), "{out}");
}

/// With a baseline, `MORDANT_BASELINE_WRITE=1` records the unused items
/// under the section of the crate that defines them, a later run is held to
/// that count, and one past it is reported as the baseline's warning and
/// listed in `over-baseline.txt`. Every finding of the file is shown then,
/// since any of them can be the new one.
#[test]
fn unused_pub_is_held_to_the_baseline() {
    let root = workspace(
        "baseline",
        &[
            ("src/lib.rs", "pub fn old() {}\n"),
            (
                "mordant.toml",
                "[mordant]\nbaseline = \"mordant-baseline.toml\"\n",
            ),
        ],
    );
    let write = cargo_mordant_with(&root, &[], &[("MORDANT_BASELINE_WRITE", "1")]);
    let written = stderr(&write);
    assert!(!written.contains("is public"), "{written}");
    let baseline =
        fs::read_to_string(root.join("mordant-baseline.toml")).expect("baseline written");
    assert!(baseline.contains("[demo]"), "{baseline}");
    assert!(
        baseline.contains("\"unused_pub:src/lib.rs\" = 1"),
        "{baseline}"
    );

    let held = stderr(&cargo_mordant(&root));
    assert!(!held.contains("is public"), "{held}");

    // Above the recorded one: the baseline holds a count and not which
    // findings, so it cannot take the first one in the file for the old one.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn new() {}\n\npub fn old() {}\n",
    )
    .expect("add an item");
    let over = stderr_over(&cargo_mordant(&root));
    assert!(
        over.contains("`unused_pub` over the mordant baseline (1 recorded for src/lib.rs)"),
        "{over}"
    );
    for shown in ["`demo::new` is public", "`demo::old` is public"] {
        assert!(over.contains(shown), "{shown}: {over}");
    }
    assert!(
        over.contains("2 `unused_pub` findings in src/lib.rs, and the baseline allows 1"),
        "{over}"
    );
    assert!(
        over.contains("1 finding(s) over the baseline in demo"),
        "{over}"
    );
    let status = fs::read_to_string(root.join("target/mordant/over-baseline.txt")).expect("status");
    assert_eq!(status, "demo 1\n");
}

/// A level set in the source is the item's, and the finding says where.
#[test]
fn unused_pub_reports_a_level_set_in_the_source() {
    let root = workspace(
        "attribute",
        &[(
            "src/lib.rs",
            "#![cfg_attr(mordant, deny(unused_pub))]\n\npub fn unused() {}\n",
        )],
    );
    let out = cargo_mordant(&root);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("error: function `demo::unused` is public"),
        "{stderr}"
    );
    assert!(
        stderr.contains("note: the lint level is defined here"),
        "{stderr}"
    );
}

/// A proc-macro another member expands is built in full, as a `.dylib` or
/// `.so` with no `.rmeta` beside it, and under `-p` cargo builds nothing
/// else of it. That build is still a unit of the run: what the macro's code
/// uses counts, and the library it uses is judged.
#[test]
fn unused_pub_counts_a_proc_macro_built_only_for_its_dependent() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proc_macro");
    let _ = fs::remove_dir_all(&root);
    let package = |name: &str, rest: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n{rest}")
    };
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"demo\", \"demo_macros\", \"app\"]\nresolver = \"2\"\n"
                .to_string(),
        ),
        ("demo/Cargo.toml", package("demo", "")),
        (
            "demo/src/lib.rs",
            "pub fn used_by_macro() {}\n\npub fn unused() {}\n".to_string(),
        ),
        (
            "demo_macros/Cargo.toml",
            package(
                "demo_macros",
                "[lib]\nproc-macro = true\n\n[dependencies]\ndemo = { path = \"../demo\" }\n",
            ),
        ),
        (
            "demo_macros/src/lib.rs",
            "use proc_macro::TokenStream;\n\n#[proc_macro]\n\
             pub fn m(input: TokenStream) -> TokenStream {\n    demo::used_by_macro();\n    input\n}\n"
                .to_string(),
        ),
        (
            "app/Cargo.toml",
            package("app", "[dependencies]\ndemo_macros = { path = \"../demo_macros\" }\n"),
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    demo_macros::m!();\n}\n".to_string(),
        ),
    ];
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a file in the workspace"))
            .expect("create the member");
        fs::write(path, text).expect("write a workspace file");
    }
    for run in [&["-p", "app", "-p", "demo"][..], &["--workspace"][..]] {
        let out = stderr(&cargo_mordant_with(&root, run, &[]));
        assert!(out.contains("`demo::unused` is public"), "{run:?}: {out}");
        assert!(
            !out.contains("`demo::used_by_macro` is public"),
            "{run:?}: {out}"
        );
    }
}

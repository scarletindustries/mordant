//! `cargo mordant` end to end: the binaries `cargo test` builds, run the way
//! a user runs them, over a one-package workspace each test writes afresh.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CARGO_MORDANT: &str = env!("CARGO_BIN_EXE_cargo-mordant");

const MAIN: &str = "fn fallible() -> Result<u32, u32> {\n    Err(1)\n}\n\n\
                    fn main() {\n    fallible().ok();\n}\n";

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

/// The configuration reaches the lints, and a change to it reruns them on a
/// crate cargo would otherwise have left alone.
#[test]
fn dylint_toml_is_read_and_a_change_to_it_rechecks() {
    let root = workspace("config", &[("src/main.rs", MAIN)]);
    let first = stderr(&cargo_mordant(&root));
    assert!(first.contains("#[warn(discarded_error)]"), "{first}");

    fs::write(
        root.join("dylint.toml"),
        "[mordant]\ndisabled = [\"discarded_error\"]\n",
    )
    .expect("write dylint.toml");
    let disabled = stderr(&cargo_mordant(&root));
    assert!(disabled.contains("Checking demo"), "{disabled}");
    assert!(!disabled.contains("discarded_error"), "{disabled}");

    fs::remove_file(root.join("dylint.toml")).expect("remove dylint.toml");
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
                "dylint.toml",
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
    let over = stderr(&cargo_mordant(&root));
    assert!(
        over.contains("`discarded_error` over the mordant baseline (0 recorded for src/main.rs)"),
        "{over}"
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
/// listed in `over-baseline.txt`.
#[test]
fn unused_pub_is_held_to_the_baseline() {
    let root = workspace(
        "baseline",
        &[
            ("src/lib.rs", "pub fn old() {}\n"),
            (
                "dylint.toml",
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

    fs::write(
        root.join("src/lib.rs"),
        "pub fn old() {}\n\npub fn new() {}\n",
    )
    .expect("add an item");
    let over = stderr(&cargo_mordant(&root));
    assert!(
        over.contains("`unused_pub` over the mordant baseline (1 recorded for src/lib.rs)"),
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
            "#![cfg_attr(dylint_lib = \"mordant\", deny(unused_pub))]\n\npub fn unused() {}\n",
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

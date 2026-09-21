//! The ui suites. Every `.rs` in a suite's directory is compiled by
//! `mordant-driver` with the suite's `mordant.toml`, and what the compiler
//! prints must match the `.stderr` file beside it.

use std::sync::{Mutex, PoisonError};

// By path: depending on the library would link this test against the
// compiler, which a crate without `rustc_private` cannot do.
#[path = "../src/protocol.rs"]
#[allow(dead_code, reason = "the suites set only `CONFIG_ENV`")]
mod protocol;

/// The suites share this process's environment, which carries each one's
/// configuration to the compiler, so they run one at a time.
static ENV: Mutex<()> = Mutex::new(());

fn run(src_base: &str, config: &str) {
    let _env = ENV.lock().unwrap_or_else(PoisonError::into_inner);
    // SAFETY: every test in this binary that touches the environment holds
    // `ENV` while it does, and compiletest reads it only to spawn compilers
    // within this call.
    unsafe { std::env::set_var(protocol::CONFIG_ENV, config) };
    compiletest_rs::run_tests(&compiletest_rs::Config {
        mode: compiletest_rs::common::Mode::Ui,
        rustc_path: env!("CARGO_BIN_EXE_mordant-driver").into(),
        src_base: src_base.into(),
        target_rustcflags: Some("--emit=metadata -Zui-testing".to_string()),
        ..compiletest_rs::Config::default()
    });
}

#[test]
fn ui() {
    run(
        "ui",
        r#"
            [mordant]
            # Its fixtures are full of `pub` items nothing calls; it has its own suite.
            disabled = ["unused_pub"]
            key-not-identity-types = ["Span"]
            key-not-identity-forms = ["to-bits", "ptr-cast"]
            key-not-identity-methods = ["Value::to_raw"]
            key-not-identity-composite = true
            key-not-identity-fixes = ["FileId"]
            stale-across-reentry-callees = ["Vm::run_callback", "dispatch*", "Worker::run_job", "Runner::schedule"]
            defaulted-failure-callees = ["from_str_radix", "listed_by_config"]
            defaulted-failure-ignored-errors = ["Pending"]
            bool-cluster-enabled = true
            stale-safety-comment-enabled = true
            unchecked-input-len-enabled = true
            parallel-params-enabled = true
            some-still-unchecked-enabled = true
            generic-body-not-generic-enabled = true

            [[mordant.forbidden-reach]]
            from = "hot_path"
            never = ["std::vec::Vec::push"]

            [[mordant.forbidden-reach]]
            from = "two_bans"
            never = ["std::vec::Vec::push", "Option::expect"]

            [[mordant.forbidden-reach]]
            from = "one_ban_twice"
            never = ["std::vec::Vec::push"]

            [[mordant.forbidden-reach]]
            from = "index_root"
            never = ["panic_bounds_check"]

            [[mordant.forbidden-reach]]
            from = "add_overflow_root"
            never = ["panic_const_add_overflow"]

            [[mordant.forbidden-reach]]
            from = "div_zero_root"
            never = ["panic_const_div_by_zero"]

            [[mordant.forbidden-reach]]
            from = "rem_zero_root"
            never = ["panic_const_rem_by_zero"]

            [[mordant.forbidden-reach]]
            from = "neg_overflow_root"
            never = ["panic_const_neg_overflow"]

            # The two controls: a live ban on a family their bodies do not
            # reach, so a silent run here is the lint discriminating and not
            # the rule failing to resolve.
            [[mordant.forbidden-reach]]
            from = "wrong_family_root"
            never = ["panic_const_add_overflow"]

            [[mordant.forbidden-reach]]
            from = "no_assert_root"
            never = ["panic_const_add_overflow"]
            "#,
    );
}

/// The `ui` fixtures for the opt-in lints run with their keys on; these
/// re-run the same shapes with the keys absent and expect nothing.
#[test]
fn ui_opt_in_lints_are_off_without_their_key() {
    run("ui_off", "[mordant]\ndisabled = [\"unused_pub\"]\n");
}

/// `unused_pub` alone, since every other fixture is made of unused items.
#[test]
fn ui_unused_pub() {
    run("ui_unused_pub", "[mordant]\n");
}

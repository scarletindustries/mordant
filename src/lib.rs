#![feature(rustc_private)]
#![warn(unused_extern_crates)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

dylint_linting::dylint_library!();

mod asymmetric_guard;
mod baseline;
mod bypassed_validator;
mod claims;
mod discarded_error;
mod exclusive_options;
mod forbidden_reach;
mod guard_flag;
mod lock_order;
mod nonidentity_key;
mod parallel_bools;
mod stale_panic_message;
mod stale_safety_comment;
mod stringified_error;
mod stringly_error;
mod unit_mismatch;
mod unread_error_variant;
mod wildcard_local_enum;

/// Read from `dylint.toml` under `[mordant]` in the linted workspace root.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct MordantConfig {
    /// Fully qualified paths of types that are never a valid map key in this
    /// project (e.g. a span type with no file identity). Empty means silent.
    pub nonidentity_key_types: Vec<String>,
    /// Opt-in key-expression forms: "to-bits", "ptr-cast". Both are legitimate
    /// in interning code, so neither is on by default.
    pub nonidentity_key_forms: Vec<String>,
    /// Also flag `Box<dyn Error>` as a stringly error type.
    pub stringly_error_include_box_dyn: bool,
    /// Fully qualified method paths that never produce a valid map key (e.g. a
    /// NaN-boxing `Value::to_bits`). Checked in key position of `insert`/`entry`.
    pub nonidentity_key_methods: Vec<String>,
    /// Opt-in: also flag composite keys (tuples, structs one level deep) that
    /// carry a denied type without one of the fixing types beside it.
    pub nonidentity_key_composite: bool,
    /// Types whose presence in a composite key restores identity (e.g. the
    /// file id that gives a span a coordinate space).
    pub nonidentity_key_fixes: Vec<String>,
    /// Minimum Option fields for `exclusive_options` to consider a struct.
    #[serde(default = "default_min_fields")]
    pub exclusive_options_min_fields: usize,
    /// `wildcard_local_enum` stays silent above this many variants.
    #[serde(default = "default_max_variants")]
    pub wildcard_local_enum_max_variants: usize,
    /// Ratchet file name, resolved upward from each crate's manifest dir. Runs
    /// suppress up to the recorded count per (lint, file) and surface only new
    /// findings. Regenerate with `MORDANT_BASELINE_WRITE=1`.
    pub baseline: Option<String>,
    /// Reachability bans: from each matching root, no call path may reach a
    /// banned definition. Findings carry the witness path.
    pub forbidden_reach: Vec<forbidden_reach::ReachRule>,
}

fn default_min_fields() -> usize {
    2
}

fn default_max_variants() -> usize {
    12
}

#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    let config: MordantConfig = dylint_linting::config_or_default(env!("CARGO_PKG_NAME"));
    baseline::setup(&config.baseline);
    lint_store.register_lints(&[
        baseline::MORDANT_BASELINE,
        stringly_error::STRINGLY_ERROR,
        nonidentity_key::NONIDENTITY_KEY,
        stringified_error::STRINGIFIED_ERROR,
        exclusive_options::EXCLUSIVE_OPTIONS,
        parallel_bools::PARALLEL_BOOLS,
        bypassed_validator::BYPASSED_VALIDATOR,
        bypassed_validator::PUB_INVARIANT_FIELDS,
        unread_error_variant::UNREAD_ERROR_VARIANT,
        asymmetric_guard::ASYMMETRIC_GUARD,
        stale_safety_comment::STALE_SAFETY_COMMENT,
        unit_mismatch::UNIT_MISMATCH,
        stale_panic_message::STALE_PANIC_MESSAGE,
        lock_order::LOCK_ORDER,
        forbidden_reach::FORBIDDEN_REACH,
        guard_flag::GUARD_FLAG,
        wildcard_local_enum::WILDCARD_LOCAL_ENUM,
        discarded_error::DISCARDED_ERROR,
    ]);
    let c1 = config.clone();
    let c2 = config.clone();
    let c3 = config.clone();
    lint_store.register_late_pass(move |_| Box::new(stringly_error::StringlyError::new(&c1)));
    lint_store.register_late_pass(move |_| Box::new(nonidentity_key::NonidentityKey::new(&c2)));
    lint_store.register_late_pass(|_| Box::new(stringified_error::StringifiedError));
    lint_store.register_late_pass(move |_| Box::new(exclusive_options::ExclusiveOptions::new(&c3)));
    lint_store.register_late_pass(|_| Box::new(parallel_bools::ParallelBools::new()));
    lint_store.register_late_pass(|_| Box::new(bypassed_validator::BypassedValidator::new()));
    lint_store.register_late_pass(|_| Box::new(unread_error_variant::UnreadErrorVariant::new()));
    lint_store.register_late_pass(|_| Box::new(asymmetric_guard::AsymmetricGuard::new()));
    lint_store.register_late_pass(|_| Box::new(stale_safety_comment::StaleSafetyComment::new()));
    lint_store.register_late_pass(|_| Box::new(unit_mismatch::UnitMismatch));
    lint_store.register_late_pass(|_| Box::new(stale_panic_message::StalePanicMessage::new()));
    lint_store.register_late_pass(|_| Box::new(lock_order::LockOrder::new()));
    let c4 = config.clone();
    lint_store.register_late_pass(move |_| Box::new(forbidden_reach::ForbiddenReach::new(&c4)));
    lint_store.register_late_pass(|_| Box::new(guard_flag::GuardFlag::new()));
    lint_store.register_late_pass(move |_| {
        Box::new(wildcard_local_enum::WildcardLocalEnum::new(&config))
    });
    lint_store.register_late_pass(|_| Box::new(discarded_error::DiscardedError));
    // Last, so its check_crate_post flushes after every lint has recorded.
    lint_store.register_late_pass(|_| Box::new(baseline::BaselineWriter));
}

#[test]
fn ui() {
    dylint_testing::ui::Test::src_base(env!("CARGO_PKG_NAME"), "ui")
        .dylint_toml(
            r#"
            [mordant]
            nonidentity-key-types = ["Span"]
            nonidentity-key-forms = ["to-bits", "ptr-cast"]
            nonidentity-key-methods = ["Value::to_raw"]
            nonidentity-key-composite = true
            nonidentity-key-fixes = ["FileId"]

            [[mordant.forbidden-reach]]
            from = "hot_path"
            never = ["std::vec::Vec::push"]
            "#,
        )
        .run();
}

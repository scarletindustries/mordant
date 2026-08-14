#![feature(rustc_private)]
#![warn(unused_extern_crates)]

extern crate rustc_abi;
extern crate rustc_ast;
extern crate rustc_data_structures;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_index;
extern crate rustc_lint;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

dylint_linting::dylint_library!();

mod adt_facts;
mod asymmetric_guard;
mod baseline;
mod bypassed_validator;
mod claims;
mod ctor_flow;
mod defaulted_failure;
mod discarded_error;
mod enum_facts;
mod exclusive_options;
mod flag_cluster;
mod forbidden_reach;
mod guard_flag;
mod hir_shapes;
mod insert_then_unwrap;
mod lock_order;
mod mir_flow;
mod narrowed_return;
mod nonidentity_key;
mod overwide_parameter;
mod parallel_bools;
mod stale_across_reentry;
mod stale_panic_message;
mod stale_safety_comment;
mod stored_projection;
mod stringified_error;
mod stringly_error;
mod unchecked_input_len;
mod unit_mismatch;
mod unread_error_variant;
mod unread_none;
mod variant_flow;
mod wildcard_local_enum;

/// Read from `dylint.toml` under `[mordant]` in the linted workspace root.
///
/// `flag_cluster` is allowed on this one struct, and it is the lawful-lattice
/// case the lint's own help describes rather than an exemption from it. The
/// bools are opt-ins belonging to *different* lints: every combination is
/// reachable from a `dylint.toml` and each means what it says, so there is no
/// invariant between them for a type to carry. The field set is also this
/// pack's public interface — every field is a TOML key, and Scarlet's `xtask`
/// gate reads these field names out of the pinned source to decide which keys
/// it may set — so grouping them into sub-structs would rename user-visible
/// keys to satisfy a lint about internal invariants.
#[derive(serde::Deserialize)]
#[serde(rename_all = "kebab-case", default)]
#[cfg_attr(dylint_lib = "mordant", allow(flag_cluster))]
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
    /// Error types that mean "the environment refused" (allocation, IO,
    /// syscall), added to the built-in std list. A constructor exit failing
    /// with one of these is never treated as validating a field.
    pub validator_resource_errors: Vec<String>,
    /// Minimum Option fields for `exclusive_options` to consider a struct.
    #[serde(default = "default_min_fields")]
    pub exclusive_options_min_fields: usize,
    /// `wildcard_local_enum` stays silent above this many variants.
    #[serde(default = "default_max_variants")]
    pub wildcard_local_enum_max_variants: usize,
    /// Bool fields at which `flag_cluster` fires.
    #[serde(default = "default_min_bools")]
    pub flag_cluster_min_bools: usize,
    /// Construction sites at which `stored_projection` will read a
    /// correspondence between two fields.
    #[serde(default = "default_min_sites")]
    pub stored_projection_min_sites: usize,
    /// Ratchet file name, resolved upward from each crate's manifest dir. Runs
    /// suppress up to the recorded count per (lint, file) and surface only new
    /// findings. Regenerate with `MORDANT_BASELINE_WRITE=1`.
    pub baseline: Option<String>,
    /// Reachability bans: from each matching root, no call path may reach a
    /// banned definition. Findings carry the witness path.
    pub forbidden_reach: Vec<forbidden_reach::ReachRule>,
    /// This project's own re-entry points, for `stale_across_reentry`, on top
    /// of the built-in closure / fn-pointer / `dyn` / `.await` set: paths
    /// matched by `::`-segment suffix (`Vm::run_callback`, `run_callback`;
    /// a method of a trait impl matches under its type or its trait,
    /// `Worker::run_job` or `Runner::run_job`), with a trailing `*` on the
    /// last segment matching by prefix (`dispatch*`).
    pub stale_across_reentry_callees: Vec<String>,
    /// Callees `defaulted_failure` takes as rejecting their argument without
    /// reading their body: parsers in other crates, or local ones whose
    /// failure is built by combinators. Spelled like
    /// `validator-resource-errors` (a full path, `crate::name`, or a bare
    /// name). Empty by default; the lint then reports only callees whose
    /// body it can see the check in.
    pub defaulted_failure_callees: Vec<String>,
    /// Opt-in: run `flag_cluster`. Off by default because most structs it
    /// names are option bags; the state machines among them are found by
    /// running it once, not by gating on it.
    pub flag_cluster_enabled: bool,
    /// Opt-in: run `stale_safety_comment`. Off by default because a name a
    /// crate cannot see is usually defined in C++, a script, or a downstream
    /// crate; the stale ones are found by running it once.
    pub stale_safety_comment_enabled: bool,
    /// Error types whose failure `defaulted_failure` does not count, on top
    /// of `validator-resource-errors`: errors that are already recorded
    /// somewhere else by the time they are returned (a "JS exception is
    /// pending" marker, a diagnostic already pushed to a log), so defaulting
    /// them hides nothing. Same spellings as `validator-resource-errors`.
    pub defaulted_failure_ignored_errors: Vec<String>,
    /// Opt-in: run `unchecked_input_len`. Off by default because most of
    /// what it names on a codebase whose callers vouch for their lengths is a
    /// value the function also uses as some other value's limit (TRIAGE.md),
    /// which nothing inside the function tells from a missed check; run it
    /// once over parsing code and read the list.
    pub unchecked_input_len_enabled: bool,
}

fn default_min_fields() -> usize {
    2
}

fn default_max_variants() -> usize {
    12
}

fn default_min_bools() -> usize {
    3
}

fn default_min_sites() -> usize {
    2
}

/// `dylint_linting::config_or_default` returns this when the linted
/// workspace has no `dylint.toml`. Field-level `serde(default = ...)` covers
/// a present file that omits a key; both paths call the same fns, so the
/// two cannot disagree.
impl Default for MordantConfig {
    fn default() -> Self {
        Self {
            nonidentity_key_types: Vec::new(),
            nonidentity_key_forms: Vec::new(),
            stringly_error_include_box_dyn: false,
            nonidentity_key_methods: Vec::new(),
            nonidentity_key_composite: false,
            nonidentity_key_fixes: Vec::new(),
            validator_resource_errors: Vec::new(),
            exclusive_options_min_fields: default_min_fields(),
            wildcard_local_enum_max_variants: default_max_variants(),
            flag_cluster_min_bools: default_min_bools(),
            stored_projection_min_sites: default_min_sites(),
            baseline: None,
            forbidden_reach: Vec::new(),
            stale_across_reentry_callees: Vec::new(),
            defaulted_failure_callees: Vec::new(),
            flag_cluster_enabled: false,
            stale_safety_comment_enabled: false,
            defaulted_failure_ignored_errors: Vec::new(),
            unchecked_input_len_enabled: false,
        }
    }
}

#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    dylint_linting::init_config(sess);
    let config: MordantConfig = dylint_linting::config_or_default(env!("CARGO_PKG_NAME"));
    let config: &'static MordantConfig = Box::leak(Box::new(config));
    baseline::setup(&config.baseline);
    lint_store.register_lints(&[
        stringly_error::STRINGLY_ERROR,
        nonidentity_key::NONIDENTITY_KEY,
        stringified_error::STRINGIFIED_ERROR,
        exclusive_options::EXCLUSIVE_OPTIONS,
        parallel_bools::PARALLEL_BOOLS,
        flag_cluster::FLAG_CLUSTER,
        bypassed_validator::BYPASSED_VALIDATOR,
        unread_error_variant::UNREAD_ERROR_VARIANT,
        asymmetric_guard::ASYMMETRIC_GUARD,
        stale_safety_comment::STALE_SAFETY_COMMENT,
        unit_mismatch::UNIT_MISMATCH,
        stale_panic_message::STALE_PANIC_MESSAGE,
        lock_order::LOCK_ORDER,
        forbidden_reach::FORBIDDEN_REACH,
        guard_flag::GUARD_FLAG,
        unread_none::UNREAD_NONE,
        insert_then_unwrap::INSERT_THEN_UNWRAP,
        overwide_parameter::OVERWIDE_PARAMETER,
        narrowed_return::NARROWED_RETURN,
        wildcard_local_enum::WILDCARD_LOCAL_ENUM,
        discarded_error::DISCARDED_ERROR,
        stored_projection::STORED_PROJECTION,
        stale_across_reentry::STALE_ACROSS_REENTRY,
        defaulted_failure::DEFAULTED_FAILURE,
        unchecked_input_len::UNCHECKED_INPUT_LEN,
    ]);
    lint_store.register_late_pass(move |_| Box::new(stringly_error::StringlyError::new(config)));
    lint_store.register_late_pass(move |_| Box::new(nonidentity_key::NonidentityKey::new(config)));
    lint_store.register_late_pass(|_| Box::new(stringified_error::StringifiedError));
    lint_store
        .register_late_pass(move |_| Box::new(exclusive_options::ExclusiveOptions::new(config)));
    lint_store.register_late_pass(|_| Box::new(parallel_bools::ParallelBools::new()));
    // The opt-in lints stay registered above so `allow(..)` / `-A` of them
    // still resolve; only their passes are skipped.
    if config.flag_cluster_enabled {
        lint_store.register_late_pass(move |_| Box::new(flag_cluster::FlagCluster::new(config)));
    }
    lint_store
        .register_late_pass(move |_| Box::new(bypassed_validator::BypassedValidator::new(config)));
    lint_store.register_late_pass(|_| Box::new(unread_error_variant::UnreadErrorVariant::new()));
    lint_store.register_late_pass(|_| Box::new(asymmetric_guard::AsymmetricGuard::new()));
    if config.stale_safety_comment_enabled {
        lint_store
            .register_late_pass(|_| Box::new(stale_safety_comment::StaleSafetyComment::new()));
    }
    lint_store.register_late_pass(|_| Box::new(unit_mismatch::UnitMismatch));
    lint_store.register_late_pass(|_| Box::new(stale_panic_message::StalePanicMessage::new()));
    lint_store.register_late_pass(|_| Box::new(lock_order::LockOrder::new()));
    lint_store.register_late_pass(move |_| Box::new(forbidden_reach::ForbiddenReach::new(config)));
    lint_store.register_late_pass(|_| Box::new(guard_flag::GuardFlag::new()));
    lint_store.register_late_pass(|_| Box::new(unread_none::UnreadNone::new()));
    lint_store.register_late_pass(|_| Box::new(insert_then_unwrap::InsertThenUnwrap));
    lint_store.register_late_pass(|_| Box::new(overwide_parameter::OverwideParameter::new()));
    lint_store.register_late_pass(|_| Box::new(narrowed_return::NarrowedReturn::new()));
    lint_store
        .register_late_pass(move |_| Box::new(wildcard_local_enum::WildcardLocalEnum::new(config)));
    lint_store.register_late_pass(|_| Box::new(discarded_error::DiscardedError));
    lint_store
        .register_late_pass(move |_| Box::new(stored_projection::StoredProjection::new(config)));
    lint_store.register_late_pass(move |_| {
        Box::new(stale_across_reentry::StaleAcrossReentry::new(config))
    });
    lint_store
        .register_late_pass(move |_| Box::new(defaulted_failure::DefaultedFailure::new(config)));
    if config.unchecked_input_len_enabled {
        lint_store.register_late_pass(|_| Box::new(unchecked_input_len::UncheckedInputLen));
    }
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
            stale-across-reentry-callees = ["Vm::run_callback", "dispatch*", "Worker::run_job", "Runner::schedule"]
            defaulted-failure-callees = ["from_str_radix", "listed_by_config"]
            defaulted-failure-ignored-errors = ["Pending"]
            flag-cluster-enabled = true
            stale-safety-comment-enabled = true
            unchecked-input-len-enabled = true

            [[mordant.forbidden-reach]]
            from = "hot_path"
            never = ["std::vec::Vec::push"]

            [[mordant.forbidden-reach]]
            from = "two_bans"
            never = ["std::vec::Vec::push", "Option::expect"]

            [[mordant.forbidden-reach]]
            from = "one_ban_twice"
            never = ["std::vec::Vec::push"]
            "#,
        )
        .run();
}

/// The `ui` fixtures for the opt-in lints run with their keys on; these
/// re-run the same shapes with the keys absent and expect nothing.
#[test]
fn ui_opt_in_lints_are_off_without_their_key() {
    dylint_testing::ui::Test::src_base(env!("CARGO_PKG_NAME"), "ui_off")
        .dylint_toml("[mordant]\n")
        .run();
}

/// `config_or_default` returns `Default` when the linted workspace has no
/// `dylint.toml`. A derived `Default` yields 0 for every `usize`, which
/// turns `wildcard_local_enum` off (`n > 0` for every enum) and makes
/// `exclusive_options` consider every struct.
#[test]
fn config_default_thresholds_match_docs() {
    let c = MordantConfig::default();
    assert_eq!(c.exclusive_options_min_fields, 2);
    assert_eq!(c.wildcard_local_enum_max_variants, 12);
    assert_eq!(c.flag_cluster_min_bools, 3);
    assert_eq!(c.stored_projection_min_sites, 2);
    assert!(!c.flag_cluster_enabled);
    assert!(!c.stale_safety_comment_enabled);
    assert!(!c.unchecked_input_len_enabled);
}

/// An empty table (file present, keys omitted) must not drift from
/// `Default`. Field-level `serde(default = ...)` and this impl share the
/// same fns.
#[test]
fn config_omitted_toml_keys_use_the_same_thresholds() {
    let parsed: MordantConfig = toml::from_str("").expect("empty document is an empty table");
    let d = MordantConfig::default();
    assert_eq!(
        parsed.exclusive_options_min_fields,
        d.exclusive_options_min_fields
    );
    assert_eq!(
        parsed.wildcard_local_enum_max_variants,
        d.wildcard_local_enum_max_variants
    );
    assert_eq!(parsed.flag_cluster_min_bools, d.flag_cluster_min_bools);
    assert_eq!(
        parsed.stored_projection_min_sites,
        d.stored_projection_min_sites
    );
    assert_eq!(parsed.flag_cluster_enabled, d.flag_cluster_enabled);
    assert_eq!(
        parsed.stale_safety_comment_enabled,
        d.stale_safety_comment_enabled
    );
    assert_eq!(
        parsed.unchecked_input_len_enabled,
        d.unchecked_input_len_enabled
    );
}

#[test]
fn config_explicit_zero_thresholds_are_honored() {
    let parsed: MordantConfig = toml::from_str(
        "exclusive-options-min-fields = 0\n\
         wildcard-local-enum-max-variants = 0\n\
         flag-cluster-min-bools = 0\n",
    )
    .expect("explicit zeros parse");
    assert_eq!(parsed.exclusive_options_min_fields, 0);
    assert_eq!(parsed.wildcard_local_enum_max_variants, 0);
    assert_eq!(parsed.flag_cluster_min_bools, 0);
}

#![feature(rustc_private)]
#![feature(default_field_values)]
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

use rustc_data_structures::sync;

mod adt_facts;
mod asymmetric_guard;
mod baseline;
mod bool_beside_option;
mod bool_params;
mod bypassed_conversion;
mod bypassed_validator;
mod claims;
mod collapsed_error;
mod crossed_alias;
mod crossed_index;
mod ctor_flow;
mod defaulted_failure;
mod dependent_field;
mod discarded_error;
mod enum_facts;
mod exclusive_options;
mod flag_cluster;
mod forbidden_reach;
mod guard_flag;
mod hir_clone;
mod hir_shapes;
mod insert_then_unwrap;
mod lock_order;
mod mir_flow;
mod misbound_arg;
mod narrowed_return;
mod nonidentity_key;
mod overwide_parameter;
mod parallel_bools;
mod parallel_params;
mod parallel_vecs;
mod reimplemented_helper;
mod same_match_twice;
mod sentinel_int;
mod stale_across_reentry;
mod stale_panic_message;
mod stale_safety_comment;
mod stored_projection;
mod stringified_error;
mod stringly_error;
mod stringly_state;
mod unchecked_input_len;
mod uneven_narrowing;
mod unit_mismatch;
mod unnamed_tuple;
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
///
/// `dylint_linting::config_or_default` returns `Default` when the linted
/// workspace has no `dylint.toml`; the container-level `serde(default)`
/// fills any omitted key from it too.
#[derive(Default, serde::Deserialize)]
#[cfg_attr(test, derive(Debug, PartialEq))]
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
    pub exclusive_options_min_fields: usize = 2,
    /// `wildcard_local_enum` stays silent above this many variants.
    pub wildcard_local_enum_max_variants: usize = 12,
    /// Bool fields at which `flag_cluster` fires.
    pub flag_cluster_min_bools: usize = 3,
    /// Construction sites at which `stored_projection` will read a
    /// correspondence between two fields.
    pub stored_projection_min_sites: usize = 2,
    /// Expression nodes below which `reimplemented_helper` does not compare
    /// a body, so one-line accessors and constructors never pair up.
    pub reimplemented_helper_min_nodes: usize = 12,
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
    /// Opt-in: run `parallel_params`. Off by default because a buffer and a
    /// cursor into it, or a precedence level and the flags in force at it,
    /// pass between functions together by design, and nothing in the
    /// signatures tells those from a value nobody declared; run it once and
    /// read the list.
    pub parallel_params_enabled: bool,
    /// Functions a parameter group must pass between, unchanged, before
    /// `parallel_params` names it.
    pub parallel_params_min_fns: usize = 3,
}

#[expect(clippy::no_mangle_with_rust_abi)]
#[unsafe(no_mangle)]
pub fn register_lints(sess: &rustc_session::Session, s: &mut rustc_lint::LintStore) {
    use {
        asymmetric_guard::AsymmetricGuard, baseline::BaselineWriter, bool_params::BoolParams,
        bypassed_conversion::BypassedConversion, bypassed_validator::BypassedValidator,
        collapsed_error::CollapsedError, crossed_alias::CrossedAlias, crossed_index::CrossedIndex,
        defaulted_failure::DefaultedFailure, dependent_field::DependentField,
        discarded_error::DiscardedError, exclusive_options::ExclusiveOptions,
        flag_cluster::FlagCluster, forbidden_reach::ForbiddenReach, guard_flag::GuardFlag,
        insert_then_unwrap::InsertThenUnwrap, lock_order::LockOrder, misbound_arg::MisboundArg,
        narrowed_return::NarrowedReturn, nonidentity_key::NonidentityKey,
        overwide_parameter::OverwideParameter, parallel_bools::ParallelBools,
        parallel_params::ParallelParams, parallel_vecs::ParallelVecs, sentinel_int::SentinelInt,
        stale_across_reentry::StaleAcrossReentry, stale_panic_message::StalePanicMessage,
        stale_safety_comment::StaleSafetyComment, stored_projection::StoredProjection,
        stringified_error::StringifiedError, stringly_error::StringlyError,
        stringly_state::StringlyState, unchecked_input_len::UncheckedInputLen,
        uneven_narrowing::UnevenNarrowing, unit_mismatch::UnitMismatch,
        unnamed_tuple::UnnamedTuple, unread_error_variant::UnreadErrorVariant,
        unread_none::UnreadNone, wildcard_local_enum::WildcardLocalEnum,
    };
    dylint_linting::init_config(sess);
    let config: MordantConfig = dylint_linting::config_or_default(env!("CARGO_PKG_NAME"));
    let config: &'static MordantConfig = Box::leak(Box::new(config));
    baseline::setup(&config.baseline);
    add(s, true, move || StringlyError { config });
    add(s, true, move || NonidentityKey::new(config));
    add(s, true, || StringifiedError);
    add(s, true, move || ExclusiveOptions::new(config));
    add(s, true, ParallelBools::default);
    add(s, config.flag_cluster_enabled, move || FlagCluster {
        config,
    });
    add(s, true, move || BypassedValidator::new(config));
    add(s, true, UnreadErrorVariant::default);
    add(s, true, AsymmetricGuard::default);
    add(
        s,
        config.stale_safety_comment_enabled,
        StaleSafetyComment::default,
    );
    add(s, true, || UnitMismatch);
    add(s, true, StalePanicMessage::default);
    add(s, true, LockOrder::default);
    add(s, true, move || ForbiddenReach::new(config));
    add(s, true, GuardFlag::default);
    add(s, true, UnreadNone::default);
    add(s, true, || InsertThenUnwrap);
    add(s, true, OverwideParameter::default);
    add(s, true, NarrowedReturn::default);
    add(s, true, move || WildcardLocalEnum { config });
    add(s, true, || DiscardedError);
    add(s, true, move || StoredProjection::new(config));
    add(s, true, move || StaleAcrossReentry { config });
    add(s, true, move || DefaultedFailure::new(config));
    add(s, config.unchecked_input_len_enabled, || UncheckedInputLen);
    add(s, true, || MisboundArg);
    add(s, true, move || BypassedConversion::new(config));
    add(s, true, same_match_twice::SameMatchTwice::default);
    add(s, true, move || {
        reimplemented_helper::ReimplementedHelper::new(config)
    });
    add(s, true, DependentField::default);
    add(s, true, CollapsedError::default);
    add(s, true, UnevenNarrowing::default);
    add(s, true, || CrossedIndex);
    add(s, true, ParallelVecs::default);
    add(s, true, bool_beside_option::BoolBesideOption::default);
    add(s, true, SentinelInt::default);
    add(s, true, StringlyState::default);
    add(s, config.parallel_params_enabled, move || {
        ParallelParams::new(config)
    });
    add(s, true, BoolParams::default);
    add(s, true, UnnamedTuple::default);
    add(s, true, || CrossedAlias);
    // Last, so its check_crate_post flushes after every lint has recorded.
    add(s, true, || BaselineWriter);
}

/// Registers the pass's lints unconditionally, so `allow(..)` / `-A` of an
/// opt-in lint still resolves when only its pass is skipped.
fn add<T: for<'tcx> rustc_lint::LateLintPass<'tcx> + 'static>(
    s: &mut rustc_lint::LintStore,
    enabled: bool,
    make: impl Fn() -> T + sync::DynSend + sync::DynSync + 'static,
) {
    s.register_lints(&make().get_lints());
    if enabled {
        s.register_late_pass(move |_| Box::new(make()));
    }
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
            parallel-params-enabled = true

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
/// `dylint.toml`. A threshold that lost its `= N` would default to 0, which
/// turns `wildcard_local_enum` off (`n > 0` for every enum) and makes
/// `exclusive_options` consider every struct.
#[test]
fn config_default_thresholds_match_docs() {
    let c = MordantConfig::default();
    assert_eq!(c.exclusive_options_min_fields, 2);
    assert_eq!(c.wildcard_local_enum_max_variants, 12);
    assert_eq!(c.flag_cluster_min_bools, 3);
    assert_eq!(c.stored_projection_min_sites, 2);
    assert_eq!(c.reimplemented_helper_min_nodes, 12);
    assert!(!c.flag_cluster_enabled);
    assert!(!c.stale_safety_comment_enabled);
    assert!(!c.unchecked_input_len_enabled);
    assert!(!c.parallel_params_enabled);
    assert_eq!(c.parallel_params_min_fns, 3);
}

/// An empty table (file present, keys omitted) must not drift from
/// `Default`.
#[test]
fn config_omitted_toml_keys_use_the_same_thresholds() {
    let parsed: MordantConfig = toml::from_str("").expect("empty document is an empty table");
    assert_eq!(parsed, MordantConfig::default());
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

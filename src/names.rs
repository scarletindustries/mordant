//! Names lints of this pack used to go by, and the families they belong to.
//! `RENAMED` drives every place an old name can still appear: `#[allow(..)]`
//! in linted code (through `LintStore::register_renamed`), the `disabled`
//! list in `dylint.toml`, and the `lint:file` keys of a baseline written
//! before the rename. `GROUPS` drives `LintStore::register_group` and
//! `group:<name>` entries in `disabled`.

/// (old, new) for every renamed lint.
pub const RENAMED: &[(&str, &str)] = &[
    ("exclusive_options", "options_as_enum"),
    ("flag_cluster", "bool_cluster"),
    ("guard_flag", "runtime_typestate"),
    ("unread_none", "always_unwrapped_option"),
    ("stored_projection", "derived_field"),
    ("dependent_field", "field_valid_only_when"),
    ("unnamed_tuple", "tuple_wants_struct"),
    ("some_if", "some_still_unchecked"),
    ("bypassed_validator", "unchecked_construction"),
    ("asymmetric_guard", "guard_blind_to_action"),
    ("collapsed_error", "error_collapsed_to_bool"),
    ("uneven_narrowing", "narrowed_two_ways"),
    ("bypassed_conversion", "cast_bypasses_from"),
    ("sentinel_int", "sentinel_integer"),
    ("wildcard_local_enum", "wildcard_over_own_enum"),
    ("overwide_parameter", "param_wider_than_callers"),
    ("narrowed_return", "return_wider_than_body"),
    ("bool_params", "bare_bool_args"),
    ("misbound_arg", "arg_named_like_other_param"),
    ("crossed_alias", "interchangeable_aliases"),
    ("crossed_index", "index_of_other_kind"),
    ("nonidentity_key", "key_not_identity"),
];

/// The name `name` goes by now: itself, unless it is an old name.
pub fn current(name: &str) -> &str {
    RENAMED
        .iter()
        .find(|(old, _)| *old == name)
        .map_or(name, |(_, new)| new)
}

/// Every lint of the pack, in exactly one family. A family is a lint group
/// to rustc (`-A mordant_naming`, `#![warn(mordant_state)]`; see `group_id`)
/// and a `group:naming` entry in `disabled`.
pub const GROUPS: &[(&str, &[&str])] = &[
    (
        "state",
        &[
            "options_as_enum",
            "parallel_bools",
            "bool_cluster",
            "runtime_typestate",
            "always_unwrapped_option",
            "derived_field",
            "field_valid_only_when",
            "bool_beside_option",
            "parallel_vecs",
            "parallel_params",
            "stringly_state",
            "tuple_wants_struct",
            "some_still_unchecked",
        ],
    ),
    (
        "checks",
        &[
            "unchecked_construction",
            "defaulted_failure",
            "unchecked_input_len",
            "guard_blind_to_action",
            "stale_across_reentry",
            "error_collapsed_to_bool",
            "narrowed_two_ways",
            "cast_bypasses_from",
            "sentinel_integer",
        ],
    ),
    (
        "errors",
        &[
            "stringly_error",
            "stringified_error",
            "discarded_error",
            "unread_error_variant",
        ],
    ),
    (
        "enums",
        &[
            "wildcard_over_own_enum",
            "param_wider_than_callers",
            "return_wider_than_body",
        ],
    ),
    ("duplication", &["same_match_twice", "reimplemented_helper"]),
    (
        "naming",
        &[
            "bare_bool_args",
            "arg_named_like_other_param",
            "interchangeable_aliases",
            "index_of_other_kind",
            "unit_mismatch",
        ],
    ),
    (
        "keys_locks",
        &["key_not_identity", "insert_then_unwrap", "lock_order"],
    ),
    ("comments", &["stale_safety_comment", "stale_panic_message"]),
    ("custom", &["forbidden_reach"]),
];

/// The group's name to rustc. Every library dylint loads shares one flat
/// namespace of lints and groups with rustc itself, and a bare `errors` or
/// `naming` in it would read as anyone's, so the id carries the pack's name.
pub fn group_id(group: &str) -> String {
    format!("mordant_{group}")
}

/// The lints a `disabled` entry spelled `group:<name>` stands for; `None`
/// for any other entry, including a `group:` naming no family.
pub fn group_members(entry: &str) -> Option<&'static [&'static str]> {
    let name = entry.strip_prefix("group:")?;
    GROUPS
        .iter()
        .find(|(g, _)| *g == name)
        .map(|(_, members)| *members)
}

#[cfg(test)]
mod tests {
    use super::{GROUPS, RENAMED, current, group_members};

    #[test]
    fn group_names_are_unique() {
        let names: Vec<&str> = GROUPS.iter().map(|(g, _)| *g).collect();
        for (i, g) in names.iter().enumerate() {
            assert!(!names[i + 1..].contains(g), "{g} twice");
        }
    }

    #[test]
    fn group_members_reads_only_the_group_spelling() {
        assert_eq!(
            group_members("group:duplication"),
            Some(&["same_match_twice", "reimplemented_helper"][..])
        );
        assert_eq!(group_members("group:nope"), None);
        assert_eq!(group_members("duplication"), None);
        assert_eq!(group_members("lock_order"), None);
    }

    #[test]
    fn renamed_names_are_unique_and_never_chain() {
        let olds: Vec<&str> = RENAMED.iter().map(|(o, _)| *o).collect();
        let news: Vec<&str> = RENAMED.iter().map(|(_, n)| *n).collect();
        for (i, o) in olds.iter().enumerate() {
            assert!(!olds[i + 1..].contains(o), "{o} renamed twice");
            assert!(!news.contains(o), "{o} is both an old and a new name");
        }
        for (i, n) in news.iter().enumerate() {
            assert!(!news[i + 1..].contains(n), "two lints renamed to {n}");
        }
    }

    #[test]
    fn current_maps_old_names_and_passes_everything_else_through() {
        assert_eq!(current("guard_flag"), "runtime_typestate");
        assert_eq!(current("runtime_typestate"), "runtime_typestate");
        assert_eq!(current("lock_order"), "lock_order");
        assert_eq!(current("clippy::unwrap_used"), "clippy::unwrap_used");
    }
}

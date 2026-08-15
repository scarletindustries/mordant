//! Names lints of this pack used to go by. One table drives every place an
//! old name can still appear: `#[allow(..)]` in linted code (through
//! `LintStore::register_renamed`), the `disabled` list in `dylint.toml`, and
//! the `lint:file` keys of a baseline written before the rename.

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

#[cfg(test)]
mod tests {
    use super::{RENAMED, current};

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

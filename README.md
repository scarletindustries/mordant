<img width="128" src="https://github.com/scarletindustries.png" />

### Mordant

Lints that find code where the type system is not enforcing the invariants the code depends on.

[Documentation](https://scarlet.industries/docs/mordant)

---

Mordant is a lint pack for Rust. It finds rules your code depends on but its types do not enforce, such as two flags that must never both be true, and suggests the type that would. A lint that cannot prove its claim stays silent.

## Install

Mordant lints stable Rust projects. It is built with the nightly pinned in [`rust-toolchain`](rust-toolchain), and your own toolchain does not change.

```sh
rustup toolchain install nightly-2026-09-01 --component rustc-dev --component llvm-tools-preview
cargo +nightly-2026-09-01 install --locked --git https://github.com/scarletindustries/mordant
```

Add `--rev <commit>` to pin the lints to one version.

## Run

```sh
cargo mordant --workspace --all-targets
cargo mordant --workspace --fix
```

`--fix` applies the fixes Mordant is sure of, and every other flag goes to `cargo check`. `cargo mordant --list` prints every lint.

## Configure

Settings go in a `[mordant]` table in `mordant.toml` at the workspace root:

```toml
[mordant]
disabled = ["unit_mismatch", "group:duplication"]
bool-cluster-enabled = true

[[mordant.forbidden-reach]]
from = "sched::pick"
never = ["std::vec::Vec::push"]
```

`disabled` takes a lint name or a whole family. A few lints are off until their `*-enabled` key is set, and the [lint reference](https://scarlet.industries/docs/mordant/lints) names each one.

## Baseline

A baseline records how many findings each lint has in each file, so a run says nothing about a file until it has more than that:

```toml
[mordant]
baseline = "mordant-baseline.toml"
```

```sh
MORDANT_BASELINE_WRITE=1 cargo mordant --workspace
```

The baseline holds a count, not which findings. So a file that goes over shows every finding of that lint, with the count the baseline allows: the new one is among them.

Regenerate and commit it after fixing a finding. A crate that goes over the baseline is listed in `target/mordant/over-baseline.txt`, so a CI job can fail on that file:

```sh
rm -f target/mordant/over-baseline.txt
cargo mordant --workspace --keep-going
test ! -s target/mordant/over-baseline.txt
```

## Lints

There are 44, in families. Each family is a lint group, so `#![allow(mordant_naming)]` silences every naming lint. The [lint reference](https://scarlet.industries/docs/mordant/lints) has an example and a fix for each.

| Family         | Group                 | Lints |
| -------------- | --------------------- | ----- |
| State          | `mordant_state`       | `options_as_enum` `parallel_bools` `bool_cluster` `runtime_typestate` `always_unwrapped_option` `derived_field` `field_valid_only_when` `bool_beside_option` `parallel_vecs` `parallel_params` `stringly_state` `tuple_wants_struct` `some_still_unchecked` |
| Checks         | `mordant_checks`      | `unchecked_construction` `defaulted_failure` `unchecked_input_len` `guard_blind_to_action` `stale_across_reentry` `error_collapsed_to_bool` `narrowed_two_ways` `cast_bypasses_from` `sentinel_integer` |
| Errors         | `mordant_errors`      | `stringly_error` `stringified_error` `discarded_error` `unread_error_variant` |
| Enums          | `mordant_enums`       | `wildcard_over_own_enum` `param_wider_than_callers` `return_wider_than_body` |
| Duplication    | `mordant_duplication` | `same_match_twice` `reimplemented_helper` `generic_body_not_generic` |
| Naming         | `mordant_naming`      | `bare_bool_args` `arg_named_like_other_param` `interchangeable_aliases` `index_of_other_kind` `unit_mismatch` |
| Keys and locks | `mordant_keys_locks`  | `key_not_identity` `insert_then_unwrap` `lock_order` |
| Comments       | `mordant_comments`    | `stale_safety_comment` `stale_panic_message` |
| Unused         | `mordant_unused`      | `unused_pub` |
| Custom         | `mordant_custom`      | `forbidden_reach` |

## Develop

CI runs these four, and the last one is Mordant linting itself:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
target/debug/cargo-mordant mordant --all-targets
```

Each `ui/*.rs` fixture must print exactly the `.stderr` file beside it, so a change to what a lint reports means updating that file.

## Name

Stroud dyed wool scarlet, and a mordant is the compound that binds the dye to the fiber so it holds.

## License

MIT or Apache-2.0, at your option.

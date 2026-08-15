<img width="128" src="https://github.com/scarletindustries.png" />

### Mordant

Lints that find code where the type system is not enforcing the invariants the code depends on.

[Documentation](https://scarlet.industries/docs/mordant) • [Dylint](https://github.com/trailofbits/dylint)

---

Mordant is a lint pack for Rust. It looks for places where an invariant lives in a convention or a runtime check when the type system could hold it instead. A struct with three booleans has eight states, and if the code only handles four of them, the type permits four states nobody wrote.

Mordant will not find every defect, but what it reports is real: a lint that cannot prove its claim from the code stays silent, and anything heuristic is off until your config turns it on.

## Lints

| lint                   | flags                                                                                                                                                                     |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `stringly_error`       | `Result<T, String>` in a public signature, where a caller has no variants to match on                                                                                     |
| `stringified_error`    | the destruction site: `.map_err(\|e\| e.to_string())` on a typed error                                                                                                    |
| `exclusive_options`    | a struct whose `Option` fields are never populated together, so the valid combinations are really an enum                                                                 |
| `parallel_bools`       | bool fields only ever assigned as a pair, which together encode a state machine                                                                                           |
| `flag_cluster`         | opt-in via `flag-cluster-enabled`: a named-field struct with several independent bools, 2^n representable states; if fewer are legal an enum names the ones that are      |
| `nonidentity_key`      | a map keyed on something that is not the canonical identity of what it names: a span, a pointer's bits, an unresolved path                                                |
| `bypassed_validator`   | a literal, a write to a checked field, or `mem::zeroed`/`transmute` outside a validated type's module and impls, none of which runs the constructor's check               |
| `guard_flag`           | a bool field that several methods test and bail on at entry, enforcing an ordering invariant at runtime                                                                   |
| `wildcard_local_enum`  | a `_` arm over a small crate-local enum, which absorbs every future variant without a compile error                                                                       |
| `discarded_error`      | `.ok();` in statement position, which reads like handling and makes the error unobservable                                                                                |
| `unread_error_variant` | a private enum variant that is constructed but never named by a pattern outside the enum's own impls, so its structure is never read                                      |
| `asymmetric_guard`     | `self.can_x()` gating a mutation that touches state the guard never reads, so the guard cannot be sound                                                                   |
| `stale_safety_comment` | opt-in via `stale-safety-comment-enabled`: a `SAFETY:` comment naming an identifier that no longer exists in the file or any linked crate                                 |
| `unit_mismatch`        | `timeout_ms + deadline_ns`: addition or comparison between names that claim different units                                                                               |
| `stale_panic_message`  | a panic, assert, or `expect` message naming an identifier that no longer exists                                                                                           |
| `lock_order`           | two locks the crate acquires in both orders, with both locations named: the shape of a deadlock                                                                           |
| `forbidden_reach`      | a config-declared ban ("from `sched::pick`, never reach `Vec::push`") violated by a concrete call path, printed as a witness chain                                        |
| `unread_none`          | an `Option` field every reader unwraps and no reader handles: a state nobody survives, usually a two-phase object wanting two types                                       |
| `insert_then_unwrap`   | `map.get(&k).unwrap()` re-fetching what `map.insert(k, ..)` just proved present, with nothing in between that could disturb either                                        |
| `stored_projection`    | two fields whose constant values agree one-for-one at every construction site: one is a projection of the other, so the type admits pairings the constructors never make  |
| `overwide_parameter`   | a panicking arm for a variant no existing call site passes: the parameter type is wider than the function's domain, and narrowing it turns the panic into a compile error |
| `narrowed_return`      | a panicking arm for a variant the callee provably never constructs: the return type promises more than the function delivers                                              |
| `stale_across_reentry` | a length, flag, or pointer read off a field of `self`, then a call that can re-enter (closure, fn pointer, `dyn`, `.await`, configured), then the field used through it   |
| `defaulted_failure`    | `f(x).unwrap_or(0)` or `let Ok(v) = f(x) else { return Ok(()) }` where `f`'s own body rejects some of `x`: the rejection becomes a value and processing carries on        |
| `unchecked_input_len`  | opt-in via `unchecked-input-len-enabled`: a received integer bounded on one path and turned into memory (`split_at`, `set_len`, `ptr.add`) on a path no check dominates   |
| `misbound_arg`         | `resize(height, width)` against `fn resize(width: u32, height: u32)`: an argument named as another parameter of the same type, so only its position says which it is      |
| `bypassed_conversion`  | `mem::transmute` or a pointer cast into a type outside its own module and impls, when a `From`/`TryFrom` impl or constructor already converts that same source into it    |
| `same_match_twice`     | the same `match` over one enum written out arm for arm in two places: a mapping the enum should state once as a method, kept in step by hand instead                      |
| `reimplemented_helper` | a function whose signature and body repeat another function in the crate under a different name: one helper written twice, so a fix to one copy misses the other          |
| `dependent_field`      | a field every reader tests a sibling for one value before touching, and every other construction fills with a placeholder: an enum payload stored flat beside its tag     |
| `collapsed_error`      | `f(x);` or `let _ = f(x)` on a crate fn whose `false`/`None` is the bare `Err` arm of a `Result` it held: the typed error became one bit, and this call drops the bit     |
| `uneven_narrowing`     | an integer field or local converted with `try_from` at one site and a bare `as` at another: the check says the value may not fit, and `as` wraps silently when it doesn't |
| `crossed_index`        | `parts[source_index]` in a function that indexes `parts` by `part_index` and `sources` by `source_index`: two index kinds cross, and both are plain integers              |
| `parallel_vecs`        | sequence fields of one struct that only change length side by side and are read at one index: element `i` of each is one record, so the type lets the lengths differ      |
| `bool_beside_option`   | a bool field written only beside an `Option` field, `true` with `Some(..)` and `false` with `None`: it is that field's `is_some()` stored twice, kept equal only by habit |
| `sentinel_int`         | an integer field one function tests against `MAX`, `-1` or an `INVALID` constant and another indexes with or offsets a pointer by untested: `Option` spelled as an int    |
| `stringly_state`       | a string field or local only ever storing one of a closed set of literals and then compared against them: an undeclared enum, so a misspelt state still compiles          |
| `parallel_params`      | opt-in via `parallel-params-enabled`: parameters several functions declare alike and hand each other unchanged in one call: one value with no type, passable by halves    |
| `bool_params`          | a crate-private fn with two or more `bool` parameters that a call fills with bare `true`/`false`: `f(x, true, false)` names neither flag, and the swapped call compiles   |
| `unnamed_tuple`        | a private fn's tuple return with two members of one type that every caller destructures under the same names: only the type lacks them, and it accepts them transposed    |
| `crossed_alias`        | a `DependencyId` value passed, stored, bound, returned or compared where a `PackageId` is declared, both aliasing one integer: two id kinds only the aliases tell apart   |

Each diagnostic states what the lint found, why the type is wrong, and the type that replaces it.

## Run

Mordant runs against stable Rust projects. The lints build against a pinned nightly, which dylint fetches on its own; your toolchain does not change.

```sh
cargo install cargo-dylint dylint-link
```

Add the library to your workspace `Cargo.toml`:

```toml
[workspace.metadata.dylint]
libraries = [{ git = "https://github.com/scarletindustries/mordant" }]
```

Run the lints:

```sh
cargo dylint --all
```

Some lints carry machine-applicable fixes. `wildcard_local_enum` rewrites each catch-all arm into the variants it was hiding, in the same path style the file already uses:

```sh
cargo dylint --all --fix
```

Configure per project in `dylint.toml` at the workspace root:

```toml
[mordant]
nonidentity-key-types = ["my_crate::span::Span"]
nonidentity-key-forms = ["ptr-cast"]
nonidentity-key-methods = ["my_crate::value::Value::to_bits"]
wildcard-local-enum-max-variants = 12
exclusive-options-min-fields = 2
flag-cluster-min-bools = 3
stored-projection-min-sites = 2
reimplemented-helper-min-nodes = 12
parallel-params-min-fns = 3

# Opt-in: also count `Box<dyn Error>` as a stringly error type.
stringly-error-include-box-dyn = true

# Opt-in: `flag_cluster`, `stale_safety_comment`, `unchecked_input_len` and
# `parallel_params` are surveys to run once over a codebase (most of what they
# name is legitimate once the real cases are fixed; for the third, a length the
# caller vouches for that the function also uses as some other value's limit;
# for the last, a buffer and a cursor into it, passed along together by
# design), so they are off until turned on here.
flag-cluster-enabled = true
stale-safety-comment-enabled = true
unchecked-input-len-enabled = true
parallel-params-enabled = true

# Opt-in: flag composite keys (tuples, structs one level deep) that carry a
# denied type unless one of the fixing types sits beside it. With these two
# lines, (Span, u32) is flagged and (FileId, Span) is accepted.
nonidentity-key-composite = true
nonidentity-key-fixes = ["my_crate::span::FileId"]

# Error types that mean "the environment refused" (allocation, IO, syscall),
# on top of the std ones. A constructor failing with one of these is not
# treated as validating any field it stores.
validator-resource-errors = ["my_alloc::AllocError", "my_sys::Error"]

# This project's own re-entry points for `stale_across_reentry`, on top of the
# built-in set (calls through closures, fn pointers, and `dyn`, and `.await`).
# Matched by `::`-segment suffix; a trailing `*` matches the rest of the name.
# A method of a trait impl is matched under its type or its trait
# (`Worker::run_job`, `Runner::run_job`) as well as by bare name.
stale-across-reentry-callees = ["Vm::run_callback", "dispatch*"]

# Callees `defaulted_failure` reports without reading their body: parsers in
# other crates, or local ones returning Option or building their failure with
# combinators. A local Result-returning callee whose body shows the check
# needs no entry. Error types that are already recorded by the time they are
# returned (an "exception pending" marker) are not worth reporting a default
# of; both keys are spelled like validator-resource-errors, which this lint
# honours too.
defaulted-failure-callees = ["toml::from_str", "from_str_radix"]
defaulted-failure-ignored-errors = ["my_jsc::JsError"]

# Reachability bans. A finding prints the concrete call chain; dynamic
# dispatch is invisible to the walk, so a clean run proves nothing, but every
# finding is a path that exists.
[[mordant.forbidden-reach]]
from = "sched::pick"
never = ["std::vec::Vec::push", "core::panicking"]
```

## Ratchet

A baseline accepts the findings you already have, so mordant can gate CI on an existing codebase from the first day. Point the config at a file:

```toml
[mordant]
baseline = "mordant-baseline.toml"
```

Generate or regenerate it:

```sh
MORDANT_BASELINE_WRITE=1 cargo dylint --all
```

The file records a count per lint and file. A run suppresses that many findings and reports anything beyond them, so new problems surface while the existing ones stay recorded. When you fix a finding, regenerate and commit the file; the count falls and stays down.

## Name

Stroud dyed wool scarlet, and a mordant is the compound that binds the dye to the fiber so it holds.

Mordant is built by Scarlet Industries.

## License

MIT or Apache-2.0, at your option.

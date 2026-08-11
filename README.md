<img width="128" src="https://github.com/scarletindustries.png" />

### Mordant

Lints that find code where the type system is not enforcing the invariants the code depends on.

[Documentation](https://scarlet.industries/docs/mordant) • [Dylint](https://github.com/trailofbits/dylint)

---

Mordant is a lint pack for Rust. It looks for places where an invariant lives in a convention or a runtime check when the type system could hold it instead. A struct with three booleans has eight states, and if the code only handles four of them, the type permits four states nobody wrote.

Mordant will not find every defect, but what it reports is real: a lint that cannot prove its claim from the code stays silent, and anything heuristic is off until your config turns it on.

## Lints

| lint | flags |
|---|---|
| `stringly_error` | `Result<T, String>` in a public signature, where a caller has no variants to match on |
| `stringified_error` | the destruction site: `.map_err(\|e\| e.to_string())` on a typed error |
| `exclusive_options` | a struct whose `Option` fields are never populated together, so the valid combinations are really an enum |
| `parallel_bools` | bool fields only ever assigned as a pair, which together encode a state machine |
| `nonidentity_key` | a map keyed on something that is not the canonical identity of what it names: a span, a pointer's bits, an unresolved path |
| `bypassed_validator` | a struct literal that skips the type's own `Result<Self, _>` constructor |
| `guard_flag` | a bool field that several methods test and bail on at entry, enforcing an ordering invariant at runtime |
| `wildcard_local_enum` | a `_` arm over a small crate-local enum, which absorbs every future variant without a compile error |
| `discarded_error` | `.ok();` in statement position, which reads like handling and makes the error unobservable |
| `unread_error_variant` | a private enum variant that is constructed but never named by a pattern outside the enum's own impls, so its structure is never read |
| `pub_invariant_fields` | a field of a validated type that is visible outside its module, so any holder can assign around the constructor's check |
| `asymmetric_guard` | `self.can_x()` gating a mutation that touches state the guard never reads, so the guard cannot be sound |
| `stale_safety_comment` | a `SAFETY:` comment naming an identifier that no longer exists in the file or any linked crate |
| `unit_mismatch` | `timeout_ms + deadline_ns`: addition or comparison between names that claim different units |
| `stale_panic_message` | a panic, assert, or `expect` message naming an identifier that no longer exists |
| `lock_order` | two locks the crate acquires in both orders, with both locations named: the shape of a deadlock |
| `forbidden_reach` | a config-declared ban ("from `sched::pick`, never reach `Vec::push`") violated by a concrete call path, printed as a witness chain |

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

# Opt-in: flag composite keys (tuples, structs one level deep) that carry a
# denied type unless one of the fixing types sits beside it. With these two
# lines, (Span, u32) is flagged and (FileId, Span) is accepted.
nonidentity-key-composite = true
nonidentity-key-fixes = ["my_crate::span::FileId"]

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

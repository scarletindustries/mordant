# Mordant v0.1 specification

Internal engineering spec. The README is the public surface; this file is not.

## Mission

Mordant mechanizes a type-system-hardening audit: find every place the program
relies on convention, runtime checks, or prose where the type system could
enforce the invariant instead. String errors that should be sum types, structs
whose field combinations encode one state, booleans that are a state machine,
identities held by the wrong key, validators that can be bypassed, typed errors
collapsed to strings at a boundary. The aim is volume WITH the no-false-positive
bar: many lints, each of which only ever states something provable. Findings
scale by adding lints, never by loosening one.

## Design principle: no false positives

Precision over recall, always. A lint fires only when it can prove the finding from the code it can see; any heuristic that merely suggests a smell stays silent. Missing a real defect is acceptable. Flagging correct code is not, because one false positive teaches an operator to ignore the tool. Every lint below is scoped to what is provable, and every known uncertain case is an explicit skip with a negative test.

## Shape

One dylint library crate named `mordant` registering all lints, at the repo root. One pinned nightly (`rust-toolchain.toml`), one `clippy_utils` pin to match. All lints are `LateLintPass` so they see resolved types, not written syntax. Default level for every lint: `warn`.

Per-project config via `dylint.toml` in the consumer workspace root.

## Lints

### `mordant::stringly_error`

Flags fallible signatures whose error type carries no structure.

- Detect: any `fn` in a public API (public fn, public trait method, public inherent method) whose return type resolves to `Result<_, E>` where `E` is `String`, `&str`, or `Cow<str>`. Resolution happens on `rustc_middle::ty`, so `type Alias = String` and generic defaults are caught.
- Config: `stringly-error-include-box-dyn = true|false` (default false) additionally flags `Box<dyn Error>`.
- Diagnostic: name the function, state that a string error has no variants to match on, suggest a crate-local error enum. Suggestion is not machine-applicable; inventing variant names is the author's job.
- Known false positive to allow for: `fn main() -> Result<(), String>` and doc-example code. Skip `main` and `#[cfg(test)]` items.

### `mordant::exclusive_options`

Flags structs where several `Option` fields together encode one state.

- Detect: struct with >= 2 `Option<_>` fields where every construction site in the crate sets at most one of those fields to `Some`. Construction sites are struct literal expressions and `Default` + field assignment chains within the crate.
- If any constructor sets two of the fields to `Some`, the fields are independent and the lint stays silent. Whole-crate analysis only; cross-crate constructors are invisible and out of scope for v0.1. The struct must be private to the crate (no `pub` visibility beyond it); a struct constructible elsewhere is unprovable and skipped.
- Requires >= 2 distinct construction sites; one constructor is not a pattern.
- The diagnostic claims only what is proved: the type permits states the crate never constructs. That claim is checkable by the reader.
- Config: `exclusive-options-min-fields` (default 2).
- Diagnostic: list the fields, state that the struct represents N x M states of which K are constructed, propose an enum with one variant per field plus an empty variant if all-`None` is constructed.

### `mordant::parallel_bools`

Flags boolean fields that change together. This is distinct from clippy's `struct_excessive_bools`, which counts fields; this lint tracks co-assignment.

- Detect: struct with >= 2 `bool` fields, private to the crate, where every write to any field in the pair occurs in the same statement block as a write to the other, across >= 2 distinct functions. If even one lone write to either field exists, the fields are independent and the lint stays silent.
- The proved claim: these fields are never assigned separately in this crate, so together they encode one state.
- Diagnostic: name the fields and the functions that co-assign them, propose an enum, enumerate the observed assignment combinations as candidate variants.

### `mordant::nonidentity_key`

Flags maps keyed on values that are not the canonical identity of the thing they name. Motivated by 5 real bugs in the Scarlet compiler: span-keyed type maps, written-path module keys, `to_bits()` constant-pool dedup.

- Detect, on `HashMap`/`BTreeMap`/`HashSet`/`IndexMap` key type positions and `insert` call sites:
  1. Key type appears in the configured deny list (`nonidentity-key-types`, a list of fully qualified paths; e.g. a `Span` type). The operator declares project law here, so a hit is correct by definition. This is the zero-false-positive core of the lint.
  2. Opt-in (`nonidentity-key-forms = ["to_bits", "ptr-cast"]`, default empty): key expression at an insert site is a call to `f32::to_bits`/`f64::to_bits`, or a raw-pointer-to-usize cast. Opt-in because both forms are legitimate in float-interning and pointer-identity caches; a project that enables them asserts they are never legitimate locally.
- Config: `nonidentity-key-types` (default empty), `nonidentity-key-forms` (default empty). With no config the lint is silent; it has no built-in universal rule because none exists.
- Diagnostic: state what the key is, why it is not an identity (spans have no file identity; pointer bits name an allocation, not a value), and instruct the author to key on the canonical identity.

### `mordant::stringified_error`

Flags the site where a typed error is collapsed into a string, complementing
`stringly_error` (which flags the signature that demands it).

- Detect: `map_err`/`unwrap_or_else`-style closure whose body is `e.to_string()`,
  `format!(...)` mentioning the error, or `String::from(e)`, where the closure
  parameter's type is a crate-visible non-string error type.
- v0.1 scope: `map_err` with a closure body that is exactly a `to_string()` or
  `format!` of the parameter. The proved claim: this expression had a typed
  error and returns prose.
- No config.

### `mordant::bypassed_validator`

- Detect: an inherent-impl associated fn returning `Result<Self, _>` or
  `Option<Self>` marks the struct as validated. Any struct literal of that type
  whose enclosing item chain is not one of the type's own impls is a bypass.
- Trait impls do not register validators (the trait dictated the signature),
  but literals inside them (e.g. `Default`) count as the type's own code.
- The proved claim is per-site: this literal runs no validation. No visibility
  requirement; the claim does not depend on seeing every construction.

### `mordant::guard_flag`

- Detect: a method whose first statement is `if self.flag { return .. }` (or
  negated) where `flag` is a bool field of the crate-local receiver struct and
  the then-branch ends in `return`. Two or more such methods on one field fire
  the lint at the field definition.
- The proved claim: these methods enforce an ordering invariant at runtime.

### `mordant::wildcard_local_enum`

- Detect: a `_` or bare-binding arm without a guard, in a `match` with >= 2
  arms, over a crate-local enum with <= `wildcard-local-enum-max-variants`
  variants (default 12) that is not `#[non_exhaustive]`.
- The proved claim: adding a variant will not surface here at compile time.

### `mordant::discarded_error`

- Detect: statement-position `.ok();` on a `Result`. The error value is
  unobservable from that point.
- `let _ =` is deliberately not flagged: it is the idiom that states the
  discard.

### `mordant::unread_error_variant`

- Detect: a crate-private enum variant constructed somewhere in the crate but
  never named by a pattern (match, if-let, matches!) outside impls of the enum
  itself. The enum's own `Display`/`Debug`/`From` impls are excluded because
  they must match every variant to exist; patterns elsewhere are what show the
  crate consuming the structure.
- Requires at least one variant of the enum to be pattern-named outside the
  enum's impls; if none are, matching is not how the enum is consumed and
  "never matched" proves nothing about any single variant.
- The proved claim: the variant's payload and identity only ever reach anyone
  through a catch-all or a string rendering.

### `mordant::pub_invariant_fields`

- Detect: a struct with a validating constructor (same detection as
  `bypassed_validator`, and implemented in the same pass) whose field
  visibility is anything broader than private to the struct's own module.
- The proved claim: the field is assignable outside the module, so the
  constructor's check holds only until the first write.

### `nonidentity_key` composite keys

- Opt-in via `nonidentity-key-composite = true`: a key type that is a tuple or
  a struct (one level of fields) carrying a denied type is flagged unless one
  of the `nonidentity-key-fixes` types sits beside it. `(Span, u32)` is
  flagged; `(FileId, Span)` passes when `FileId` is declared as fixing.
- Opt-in because whether a component restores identity is a per-project fact;
  the operator declares it, keeping the zero-false-positive core.

## Roadmap lints (v0.4+)

Same bar, one provable claim each:

- `late_init`: a field assigned immediately after every construction site;
  it belongs in the constructor (or the type has a half-built state).
- `stringly_match`: `match` / chained `==` over a `&str` where every arm is a
  literal from a closed set the crate itself produces; the set is an enum.
- `prose_invariant`: a doc comment stating an obligation ("must", "caller
  must", "before calling") that only prose enforces; audit tier.

## Ratchet

`baseline = "<file>"` in the `[mordant]` config enables it; every lint reports
through `baseline::emit`, so the ratchet covers all of them. The file maps
`"lint:relative/file.rs"` to an accepted count, in per-crate TOML sections so
parallel rustc processes rewrite only their own section (serialized with an
exclusive file lock). Counts rather than spans or fingerprints: line numbers
drift with every edit; a per-file count is the identity that survives normal
development. `MORDANT_BASELINE_WRITE=1` switches every lint from emitting to
recording, and a dedicated final pass flushes the crate's section. That pass's
internal lint is `Warn` (never fired) because rustc skips passes whose lints
are all allowed.

## Autofix

`wildcard_local_enum` carries a `MachineApplicable` suggestion: the catch-all
is replaced with the or-pattern of exactly the uncovered variants
(`V`, `V(..)`, or `V { .. }` by ctor shape), prefixed in the same path style
the sibling arms use, `name @ (...)` when the arm bound the value. Offered
only when every sibling arm is a plain variant head or or-pattern thereof;
anything fancier and the warning ships without a fix rather than risk a wrong
one. Guarded arms cover nothing.

## Testing

Dylint's `ui_test` harness. One `ui/` directory per lint with `.rs` inputs and `.stderr` snapshots. Every documented false-positive case gets a negative test.

## Toolchain policy

The nightly pin lives in this repo. Consumers never see it; `cargo dylint` fetches the pinned nightly to build the library and lints the consumer's stable project. CI has a weekly job that bumps the pin and runs the suite, so breakage from `clippy_utils` churn surfaces on our side within a week.

## Dogfood target

First consumer is the Scarlet compiler workspace (`~/code/al`). `nonidentity_key` configured with the `Span` type on day 1. v0.1 ships when all four lints run clean or produce accepted findings there.

## Out of scope for v0.1

- Primitive obsession detection (needs value-flow analysis to avoid drowning in false positives).
- Parse-don't-validate violations (needs constructor-reachability analysis).
- Machine-applicable fixes. All suggestions are advisory.

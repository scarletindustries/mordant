//! What `cargo-mordant` hands `mordant-driver` through the environment.
//! Shared by the library, both binaries and the ui tests, so the name each
//! side reads is the name the other side sets. `cargo-mordant` includes this
//! file by path rather than depending on the library, which would link it
//! against `librustc_driver`.

/// The text of the linted workspace's `dylint.toml`. `cargo-mordant` reads
/// the file once and passes its contents, so each crate's compile neither
/// looks for the workspace root nor rereads the file, and cargo, which
/// tracks the variable's value, reruns the lints when the file changes,
/// appears or goes away. Unset means no file: every key takes its default.
pub const CONFIG_ENV: &str = "MORDANT_TOML";

/// Passed alone, asks `mordant-driver` to print every lint, one per line
/// under a `mordant` heading, indented and as `name  level  description`:
/// the shape `cargo dylint list` printed, which `mordant-action` parses.
pub const LIST_ARG: &str = "--mordant-list";

/// The directory where each compilation under `cargo mordant` records the
/// `pub` items it defines and the workspace items it uses, for
/// `unused_pub` to judge once the whole run is done. Unset, each crate is
/// judged alone, against its own uses.
pub const FACTS_ENV: &str = "MORDANT_UNUSED_PUB_FACTS";

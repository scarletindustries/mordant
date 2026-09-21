//! `cargo mordant`: `cargo check` with `mordant-driver` compiling the
//! workspace's own crates, so mordant's lints run over them. Dependencies
//! build with plain rustc from the toolchain the driver was built with,
//! since the driver can only read metadata that compiler wrote. All of it
//! goes to `<target>/mordant/check`, so a run leaves the workspace's usual
//! builds alone.
//!
//! `unused_pub` needs every crate of the run before it can say an item is
//! unused, so its findings come last: cargo reports the units it built, and
//! one more compilation, of an empty crate, judges their records together.
//!
//! This binary does not link the compiler, so it starts, and can say what is
//! missing, where the toolchain it was built with is gone.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

#[path = "../protocol.rs"]
mod protocol;

/// The empty crate the last compilation builds. Its findings are the
/// workspace's, and the baseline keeps them under this name.
const REPORT_CRATE: &str = "mordant_workspace";

const USAGE: &str = "\
Run mordant's lints over a cargo workspace.

Usage: cargo mordant [--fix] [<cargo check options>]

Options:
      --fix      Run `cargo fix` instead of `cargo check`, applying the fixes
                 the lints suggest
      --list     Print every lint, with its default level and description
  -V, --version  Print the version, the commit it was built from, and its rustc
  -h, --help     Print this help

Every other option goes to `cargo check` as written: `--workspace`,
`--all-targets`, `-p <package>`, `--keep-going` and so on. `unused_pub`
counts a use from every target the run builds, so pass `--all-targets` for
what tests, benches and examples use to count.

The configuration is the `[mordant]` table of `dylint.toml` in the workspace
root, or the text of MORDANT_TOML when that is set. MORDANT_RUSTFLAGS adds
rustc flags for the linted crates only, as in MORDANT_RUSTFLAGS=\"-D warnings\".";

#[derive(serde::Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    workspace_members: Vec<String>,
    packages: Vec<Package>,
}

#[derive(serde::Deserialize)]
struct Package {
    id: String,
    manifest_path: PathBuf,
    targets: Vec<serde_json::Value>,
}

/// One line of cargo's JSON output. Only a `compiler-artifact` is read: one
/// arrives for every unit the run builds, a fresh one included.
#[derive(serde::Deserialize)]
struct CargoMessage {
    reason: String,
    #[serde(default)]
    package_id: String,
    target: Option<ArtifactTarget>,
    profile: Option<ArtifactProfile>,
}

#[derive(serde::Deserialize)]
struct ArtifactTarget {
    kind: Vec<String>,
    src_path: PathBuf,
}

#[derive(serde::Deserialize)]
struct ArtifactProfile {
    test: bool,
}

/// What `--message-format` asked for.
enum Output {
    Human,
    Short,
    /// JSON on stdout, with the value asked for: cargo's messages pass
    /// through, and the last compilation's join them as `compiler-message`s.
    Json(String),
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    // Cargo runs `cargo mordant ..` as `cargo-mordant mordant ..`.
    if args.first().is_some_and(|a| a == "mordant") {
        args.remove(0);
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!(
            "mordant {} ({}, {})",
            env!("CARGO_PKG_VERSION"),
            env!("MORDANT_SOURCE_REV"),
            env!("MORDANT_RUSTC_VERSION")
        );
        return ExitCode::SUCCESS;
    }
    let before = args.len();
    args.retain(|a| a != "--fix");
    let subcommand = if args.len() < before { "fix" } else { "check" };

    let Ok(exe) = env::current_exe() else {
        return fail("could not find this executable's path");
    };
    let driver = exe.with_file_name(format!("mordant-driver{}", env::consts::EXE_SUFFIX));
    if !driver.is_file() {
        return fail(format_args!(
            "no `mordant-driver` beside {}; install both binaries from one build",
            exe.display()
        ));
    }
    let sysroot = Path::new(env!("MORDANT_SYSROOT"));
    let rustc = sysroot
        .join("bin")
        .join(format!("rustc{}", env::consts::EXE_SUFFIX));
    if !rustc.is_file() {
        return fail(format_args!(
            "mordant was built with the toolchain at {}, which is not installed any more; \
             install it again, or rebuild mordant",
            sysroot.display()
        ));
    }
    if args.iter().any(|a| a == "--list") {
        return exit_code(
            Command::new(&driver).arg(protocol::LIST_ARG).status(),
            "mordant-driver",
        );
    }

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let Some(meta) = metadata(&cargo, option_value(&args, "--manifest-path")) else {
        return fail("could not read the workspace from `cargo metadata`");
    };
    let config = if env::var_os(protocol::CONFIG_ENV).is_some() {
        None
    } else {
        let path = meta.workspace_root.join("dylint.toml");
        match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return fail(format_args!("could not read {}: {err}", path.display())),
        }
    };
    let output = Output::take(&mut args);
    let color = option_value(&args, "--color").map(OsStr::to_os_string);
    let facts = meta.target_directory.join("mordant").join("unused_pub");

    let mut command = Command::new(&cargo);
    command.arg(subcommand);
    if option_value(&args, "--target-dir").is_none() {
        command
            .arg("--target-dir")
            .arg(meta.target_directory.join("mordant").join("check"));
    }
    command
        .arg(format!("--message-format={}", output.for_cargo()))
        .args(&args)
        .env("RUSTC", &rustc)
        .env("RUSTC_WORKSPACE_WRAPPER", &driver)
        .env(protocol::FACTS_ENV, &facts)
        .stdout(Stdio::piped());
    if let Some(text) = &config {
        command.env(protocol::CONFIG_ENV, text);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return fail(format_args!("could not run cargo: {err}")),
    };
    let mut units = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut out = io::stdout().lock();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if matches!(output, Output::Json(_)) {
                let _ = writeln!(out, "{line}");
            }
            units.extend(unit_of(&line, &meta.workspace_members));
        }
    }
    match child.wait() {
        Ok(status) if status.success() => {}
        status => return exit_code(status, "cargo"),
    }

    let mut last = match report_command(&driver, &meta, &facts, &units, &output) {
        Ok(command) => command,
        Err(err) => {
            return fail(format_args!(
                "could not write to {}: {err}",
                facts.display()
            ));
        }
    };
    if let Some(color) = color {
        last.arg("--color").arg(color);
    }
    if let Some(text) = &config {
        last.env(protocol::CONFIG_ENV, text);
    }
    if !matches!(output, Output::Json(_)) {
        return exit_code(last.status(), "mordant-driver");
    }
    last.stderr(Stdio::piped());
    let mut child = match last.spawn() {
        Ok(child) => child,
        Err(err) => return fail(format_args!("could not run mordant-driver: {err}")),
    };
    if let Some(stderr) = child.stderr.take() {
        let mut out = io::stdout().lock();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(message) = compiler_message(&line, &meta) {
                let _ = writeln!(out, "{message}");
            }
        }
    }
    exit_code(child.wait(), "mordant-driver")
}

impl Output {
    /// Takes every `--message-format` out of `args`, since cargo is given
    /// its own.
    fn take(args: &mut Vec<OsString>) -> Output {
        let mut values = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].to_string_lossy().into_owned();
            if arg == "--message-format" && i + 1 < args.len() {
                values.push(args.remove(i + 1).to_string_lossy().into_owned());
                args.remove(i);
            } else if let Some(value) = arg.strip_prefix("--message-format=") {
                values.push(value.to_string());
                args.remove(i);
            } else {
                i += 1;
            }
        }
        let joined = values.join(",");
        if joined.split(',').any(|v| v.starts_with("json")) {
            Output::Json(joined)
        } else if joined.split(',').any(|v| v == "short") {
            Output::Short
        } else {
            Output::Human
        }
    }

    /// Always JSON, so the run's units can be read off cargo's artifact
    /// messages; cargo still renders diagnostics the way that was asked.
    fn for_cargo(&self) -> &str {
        match self {
            Output::Human => "json-render-diagnostics",
            Output::Short => "json-diagnostic-short,json-render-diagnostics",
            Output::Json(value) => value,
        }
    }

    /// The same choice, in rustc's spelling, for the last compilation.
    fn for_rustc(&self) -> Vec<&'static str> {
        match self {
            Output::Human => Vec::new(),
            Output::Short => vec!["--error-format=short"],
            Output::Json(value) if value.contains("rendered-ansi") => {
                vec!["--error-format=json", "--json=diagnostic-rendered-ansi"]
            }
            Output::Json(value) if value.contains("diagnostic-short") => {
                vec!["--error-format=json", "--json=diagnostic-short"]
            }
            Output::Json(_) => vec!["--error-format=json"],
        }
    }
}

/// A unit of the run, as `(test build, crate root)`, if `line` is cargo's
/// artifact message for a workspace member other than a build script.
fn unit_of(line: &str, members: &[String]) -> Option<(bool, PathBuf)> {
    let message: CargoMessage = serde_json::from_str(line).ok()?;
    let (target, profile) = (message.target?, message.profile?);
    (message.reason == "compiler-artifact"
        && members.contains(&message.package_id)
        && !target.kind.iter().any(|k| k == "custom-build"))
    .then_some((profile.test, target.src_path))
}

/// The last compilation: an empty crate, under the environment that makes
/// `unused_pub` judge the run's units from their records in `facts`.
fn report_command(
    driver: &Path,
    meta: &Metadata,
    facts: &Path,
    units: &[(bool, PathBuf)],
    output: &Output,
) -> io::Result<Command> {
    fs::create_dir_all(facts)?;
    let listed: String = units
        .iter()
        .map(|(test, src)| format!("{}\t{}\n", u8::from(*test), src.display()))
        .collect();
    let units_file = facts.join("units");
    fs::write(&units_file, listed)?;
    let src = facts.join(format!("{}.rs", REPORT_CRATE));
    fs::write(&src, "")?;
    let mut command = Command::new(driver);
    command
        .arg(&src)
        .args(["--crate-name", REPORT_CRATE, "--crate-type", "lib"])
        .args(["--emit=metadata", "-o"])
        .arg(facts.join(format!("lib{}.rmeta", REPORT_CRATE)))
        .args(output.for_rustc())
        .current_dir(&meta.workspace_root)
        .env(protocol::FACTS_ENV, facts)
        .env(protocol::UNITS_ENV, &units_file)
        // Where the baseline is looked for, as a member's build would.
        .env("CARGO_MANIFEST_DIR", &meta.workspace_root)
        .env_remove("CARGO_BIN_NAME");
    Ok(command)
}

/// A diagnostic of the last compilation, as cargo would have printed it:
/// wrapped in a `compiler-message` of the package whose file it points at.
/// rustc's own tally of the warnings it emitted is cargo's to make.
fn compiler_message(line: &str, meta: &Metadata) -> Option<String> {
    let diagnostic: serde_json::Value = serde_json::from_str(line).ok()?;
    if diagnostic["$message_type"] != "diagnostic" {
        return None;
    }
    let spans = diagnostic["spans"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let text = diagnostic["message"].as_str().unwrap_or("");
    if spans.is_empty() && (text.ends_with(" emitted") || text.starts_with("aborting due to")) {
        return None;
    }
    let file = spans
        .iter()
        .find(|s| s["is_primary"] == true)
        .and_then(|s| s["file_name"].as_str())
        .map(|f| meta.workspace_root.join(f));
    let package = meta
        .packages
        .iter()
        .filter(|p| {
            let dir = p.manifest_path.parent().unwrap_or(&p.manifest_path);
            file.as_ref().is_some_and(|f| f.starts_with(dir))
        })
        .max_by_key(|p| p.manifest_path.as_os_str().len())
        .or_else(|| meta.packages.first())?;
    Some(
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": package.id,
            "manifest_path": package.manifest_path,
            "target": package.targets.first(),
            "message": diagnostic,
        })
        .to_string(),
    )
}

/// The exit status of the program this run hands over to, as its own.
fn exit_code(status: io::Result<ExitStatus>, program: &str) -> ExitCode {
    match status {
        Ok(status) => ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ),
        Err(err) => fail(format_args!("could not run {program}: {err}")),
    }
}

fn fail(message: impl Display) -> ExitCode {
    eprintln!("error: mordant: {message}");
    ExitCode::FAILURE
}

/// The workspace `cargo check` will build: the one `--manifest-path` names,
/// or else the one around the current directory. When cargo fails, its own
/// error is printed first.
fn metadata(cargo: &OsStr, manifest_path: Option<&OsStr>) -> Option<Metadata> {
    let mut command = Command::new(cargo);
    command.args(["metadata", "--no-deps", "--format-version", "1"]);
    if let Some(path) = manifest_path {
        command.arg("--manifest-path").arg(path);
    }
    let out = command.output().ok()?;
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The value of `--name <value>` or `--name=<value>` in cargo's arguments.
fn option_value<'a>(args: &'a [OsString], name: &str) -> Option<&'a OsStr> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().map(OsString::as_os_str);
        }
        if let Some(value) = arg
            .to_str()
            .and_then(|a| a.strip_prefix(name))
            .and_then(|a| a.strip_prefix('='))
        {
            return Some(OsStr::new(value));
        }
    }
    None
}

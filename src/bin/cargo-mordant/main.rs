//! `cargo mordant`: `cargo check` with `mordant-driver` compiling the
//! workspace's own crates, so mordant's lints run over them. Dependencies
//! build with plain rustc from the toolchain the driver was built with,
//! since the driver can only read metadata that compiler wrote. All of it
//! goes to `<target>/mordant/check`, so a run leaves the workspace's usual
//! builds alone.
//!
//! `unused_pub` needs every crate of the run before it can say an item is
//! unused, so its findings come last: the compilations record what they
//! define and use, cargo reports the units it built, and [`unused_pub`]
//! judges their records together and prints what it finds.
//!
//! This binary does not link the compiler, so it starts, and can say what is
//! missing, where the toolchain it was built with is gone.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use serde_json::Value as Json;

mod unused_pub;

#[path = "../../baseline_file.rs"]
mod baseline_file;
#[path = "../../protocol.rs"]
mod protocol;
#[path = "../../unused_pub/records.rs"]
#[allow(dead_code, reason = "the library writes what this binary only reads")]
mod records;

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
}

/// What `--message-format` asked for.
enum Output {
    Human,
    Short,
    /// JSON on stdout, with the value asked for: cargo's messages pass
    /// through, and `unused_pub`'s findings join them as `compiler-message`s.
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
    // The text to hand the compilations, unless they inherit it.
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
    let baseline = env::var(protocol::CONFIG_ENV)
        .ok()
        .or_else(|| config.clone())
        .and_then(|text| baseline_name(&text));
    let output = Output::take(&mut args);
    let styled = styled(option_value(&args, "--color"));
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
    // Held back, so the findings printed after the build come before it.
    let mut finished = None;
    if let Some(stdout) = child.stdout.take() {
        let mut out = io::stdout().lock();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let message: Option<Json> = serde_json::from_str(&line).ok();
            if let Some(message) = &message {
                if message["reason"] == "build-finished" {
                    finished = Some(message.clone());
                    continue;
                }
                units.extend(unit_of(message, &meta.workspace_members));
            }
            if matches!(output, Output::Json(_)) {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    match child.wait() {
        Ok(status) if status.success() => {}
        status => {
            print_finished(&output, finished);
            return exit_code(status, "cargo");
        }
    }
    let errors = unused_pub::report(
        &meta.workspace_root,
        &facts,
        &units,
        &output,
        styled,
        baseline.as_deref(),
    );
    if let Some(finished) = &mut finished {
        finished["success"] = Json::Bool(!errors);
    }
    print_finished(&output, finished);
    // What cargo exits with when a crate fails to compile.
    if errors {
        ExitCode::from(101)
    } else {
        ExitCode::SUCCESS
    }
}

/// Cargo's closing `build-finished` message, in JSON output.
fn print_finished(output: &Output, finished: Option<Json>) {
    if let (Output::Json(_), Some(finished)) = (output, finished) {
        let _ = writeln!(io::stdout().lock(), "{finished}");
    }
}

/// The baseline file the configuration names, if it names one.
fn baseline_name(config: &str) -> Option<String> {
    let table: toml::Table = toml::from_str(config).ok()?;
    let name = table.get("mordant")?.get("baseline")?.as_str()?;
    Some(name.to_string())
}

/// Whether findings are printed in colour: as `--color` says, or as cargo's
/// `CARGO_TERM_COLOR` does, or when stderr is a terminal.
fn styled(color: Option<&OsStr>) -> bool {
    let setting = color
        .map(OsStr::to_os_string)
        .or_else(|| env::var_os("CARGO_TERM_COLOR"));
    match setting.as_ref().and_then(|s| s.to_str()) {
        Some("always") => true,
        Some("never") => false,
        _ => io::stderr().is_terminal(),
    }
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
}

/// A unit of the run, if `message` is cargo's artifact message for a
/// workspace member other than a build script. One arrives for every unit
/// the run builds, a fresh one included.
fn unit_of(message: &Json, members: &[String]) -> Option<unused_pub::RunUnit> {
    if message["reason"] != "compiler-artifact" {
        return None;
    }
    let package_id = message["package_id"].as_str()?;
    let target = &message["target"];
    let kinds = target["kind"].as_array()?;
    if !members.iter().any(|m| m == package_id) || kinds.iter().any(|k| k == "custom-build") {
        return None;
    }
    Some(unused_pub::RunUnit {
        unit: records::Unit {
            src: PathBuf::from(target["src_path"].as_str()?),
            test: message["profile"]["test"].as_bool()?,
        },
        package_id: package_id.to_string(),
        manifest_path: message["manifest_path"].as_str()?.to_string(),
        target: target.clone(),
    })
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

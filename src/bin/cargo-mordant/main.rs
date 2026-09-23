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

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use serde_json::Value as Json;

mod over_baseline;
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

The configuration is the `[mordant]` table of `mordant.toml` in the workspace
root, or the text of MORDANT_TOML when that is set. MORDANT_RUSTFLAGS adds
rustc flags for the linted crates only, as in MORDANT_RUSTFLAGS=\"-D warnings\".";

#[derive(serde::Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    workspace_members: Vec<String>,
    packages: Vec<Package>,
}

/// A member, as `cargo metadata --no-deps` lists it.
#[derive(serde::Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<Dependency>,
}

#[derive(serde::Deserialize)]
struct Dependency {
    /// The package's name, whatever the manifest renames it to.
    name: String,
    /// Set for a path dependency, which is how one member names another.
    path: Option<PathBuf>,
}

impl Metadata {
    /// The members whose items this run can judge: those that no member
    /// the run leaves out depends on, directly or through others, for any
    /// kind of target. A member left out, as the rest of the workspace is
    /// under `-p`, may hold the only use of an item, and its records are not
    /// read.
    ///
    /// An edge is a path dependency on the member's directory, or failing a
    /// path, a dependency of the member's name: one more edge than there is
    /// only keeps an item from being judged, and one fewer would call it
    /// unused.
    fn judged(&self, args: &[OsString], units: &[unused_pub::RunUnit]) -> HashSet<&str> {
        let mut direct: HashMap<&str, Vec<&str>> = HashMap::new();
        for package in &self.packages {
            for dep in &package.dependencies {
                let on = self.packages.iter().find(|p| match &dep.path {
                    Some(path) => p.manifest_path.parent() == Some(path.as_path()),
                    None => p.name == dep.name,
                });
                if let Some(on) = on {
                    direct.entry(&on.id).or_default().push(&package.id);
                }
            }
        }
        let left_out = self.left_out(args, units);
        let judged = |package: &&Package| {
            let mut seen: HashSet<&str> = HashSet::new();
            let mut queue = vec![package.id.as_str()];
            while let Some(id) = queue.pop() {
                for dependent in direct.get(id).into_iter().flatten() {
                    if left_out.contains(dependent) {
                        return false;
                    }
                    if seen.insert(dependent) {
                        queue.push(dependent);
                    }
                }
            }
            true
        };
        self.packages
            .iter()
            .filter(judged)
            .map(|p| p.id.as_str())
            .collect()
    }

    /// The members the run does not select. Under `--workspace` they are
    /// the ones `--exclude` names. A member the run selects and builds
    /// nothing of (its only target wants a feature that is off) is not left
    /// out: there is no use in it to miss. Any other selection is read off
    /// the units cargo built, which is every member the run selected and a
    /// few it only depends on.
    fn left_out(&self, args: &[OsString], units: &[unused_pub::RunUnit]) -> HashSet<&str> {
        let excluded = option_values(args, "--exclude");
        let named = |spec: &&OsStr| {
            spec.to_str().is_some_and(|s| {
                s.chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            })
        };
        if args.iter().any(|a| a == "--workspace" || a == "--all") && excluded.iter().all(named) {
            return self
                .packages
                .iter()
                .filter(|p| excluded.iter().any(|spec| *spec == p.name.as_str()))
                .map(|p| p.id.as_str())
                .collect();
        }
        let built: HashSet<&str> = units.iter().map(|u| u.package_id.as_str()).collect();
        self.packages
            .iter()
            .map(|p| p.id.as_str())
            .filter(|id| !built.contains(id))
            .collect()
    }
}

/// The configuration for the mordant linting tool.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct MordantTable {
    baseline: Option<String>,
    disabled: Vec<String>,
}

impl MordantTable {
    fn unused_pub_off(&self) -> bool {
        self.disabled
            .iter()
            .any(|name| name == "unused_pub" || name == "group:unused")
    }
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
        let path = meta.workspace_root.join("mordant.toml");
        match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return fail(format_args!("could not read {}: {err}", path.display())),
        }
    };
    let mordant_config = env::var(protocol::CONFIG_ENV)
        .ok()
        .or_else(|| config.clone())
        .map(|text| mordant_config_table(&text))
        .unwrap_or_default();
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
    let mut build_scripts = Vec::new();
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
                build_scripts.extend(build_script_of(message, &meta.workspace_members));
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
    let (errors, mut over_baseline_lines) = if mordant_config.unused_pub_off() {
        (false, Vec::new())
    } else {
        unused_pub::report(
            &meta.workspace_root,
            &facts,
            &units,
            &meta.judged(&args, &units),
            &output,
            styled,
            mordant_config.baseline.as_deref(),
        )
    };
    match over_baseline::read_counts(&facts, &units, &build_scripts) {
        Ok(lines) => over_baseline_lines.extend(lines),
        Err(missing) => {
            let root = &meta.workspace_root;
            let named: Vec<String> = missing
                .iter()
                .map(|src| format!("`{}`", src.strip_prefix(root).unwrap_or(src).display()))
                .collect();
            unused_pub::print_error(
                root,
                &output,
                styled,
                units.first(),
                format!(
                    "mordant: `over-baseline.txt` was not written: {} left no count of findings \
                     over the baseline; remove `{}`, which holds those counts and the build \
                     `cargo mordant` reuses, then run again",
                    unused_pub::join(&named),
                    facts.parent().unwrap_or(&facts).display(),
                ),
            );
            if let Some(finished) = &mut finished {
                finished["success"] = Json::Bool(false);
            }
            print_finished(&output, finished);
            // What cargo exits with when a crate fails to compile, as for
            // `unused_pub`'s errors below.
            return ExitCode::from(101);
        }
    }
    if let Some(finished) = &mut finished {
        finished["success"] = Json::Bool(!errors);
    }
    // When `unused_pub` printed an error, `over-baseline.txt` stays as it was.
    if !errors
        && let Err(err) = over_baseline::write_over_baseline(
            &meta.target_directory,
            "over-baseline.txt",
            over_baseline_lines,
        )
    {
        if let Some(finished) = &mut finished {
            finished["success"] = Json::Bool(false);
        }
        print_finished(&output, finished);
        return fail(format_args!("could not write {err}"));
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

/// Parses the configuration table for the mordant linting tool.
fn mordant_config_table(config: &str) -> MordantTable {
    let mut table: toml::Table = toml::from_str(config).unwrap_or_default();
    table
        .remove("mordant")
        .and_then(|value| value.try_into().ok())
        .unwrap_or_default()
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
/// workspace member other than a build script. One arrives for every unit the
/// run builds, a fresh one included. [`build_script_of`] reads a build
/// script's message.
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
            extra_filename: extra_filename(message)?,
        },
        package_id: package_id.to_string(),
        manifest_path: message["manifest_path"].as_str()?.to_string(),
        target: target.clone(),
    })
}

/// A workspace member's build script, if `message` is cargo's artifact
/// message for one. Cargo sends it on a run that does not compile the build
/// script again too. `cargo mordant` reads only the build script's `.over`
/// file: no other crate can use a build script's items, so `unused_pub` does
/// not judge them.
fn build_script_of(message: &Json, members: &[String]) -> Option<records::Unit> {
    if message["reason"] != "compiler-artifact" {
        return None;
    }
    let package_id = message["package_id"].as_str()?;
    let target = &message["target"];
    let kinds = target["kind"].as_array()?;
    if !members.iter().any(|m| m == package_id) || !kinds.iter().any(|k| k == "custom-build") {
        return None;
    }
    Some(records::Unit {
        src: PathBuf::from(target["src_path"].as_str()?),
        test: false,
        extra_filename: build_script_extra_filename(message)?,
    })
}

/// Cargo's `-C extra-filename` for a build script, read off the one path in
/// its artifact message's `filenames`. Cargo lays out its build directory in
/// one of two ways:
///
/// - `<build dir>/<package>-<hash>/build-script-build`: a copy of rustc's
///   `build_script_build-<hash>`, which rustc wrote under
///   `-C extra-filename=-<hash>`.
/// - `<build dir>/<package>/<hash>/out/build_script_build`: rustc's own
///   file, named by the crate name and its extra-filename, which cargo
///   leaves empty.
///
/// Either may end in `.exe`.
fn build_script_extra_filename(message: &Json) -> Option<String> {
    let file = Path::new(message["filenames"].as_array()?.first()?.as_str()?);
    let name = file.file_name()?.to_str()?;
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name == "build-script-build" {
        let dir = file.parent()?.file_name()?.to_str()?;
        let (_, hash) = dir.rsplit_once('-')?;
        return Some(format!("-{hash}"));
    }
    name.strip_prefix("build_script_build").map(str::to_string)
}

/// Cargo's `-C extra-filename` for this artifact, read off the name of any
/// file it lists: `libdemo-<hash>.rmeta`, or `libdemo_macros-<hash>.dylib`
/// for a proc-macro cargo builds in full for a dependent and never checks.
fn extra_filename(message: &Json) -> Option<String> {
    let crate_name = message["target"]["name"].as_str()?.replace('-', "_");
    let prefixes = [format!("lib{crate_name}"), crate_name];
    message["filenames"].as_array()?.iter().find_map(|file| {
        let name = Path::new(file.as_str()?).file_name()?.to_str()?;
        prefixes.iter().find_map(|prefix| {
            // The hash runs to the first `.`: `.rmeta`, `.so`, `.dll.lib`.
            let (extra, _) = name.strip_prefix(prefix.as_str())?.split_once('.')?;
            (extra.is_empty() || extra.starts_with('-')).then(|| extra.to_string())
        })
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
    option_values(args, name).into_iter().next()
}

/// Every value `--name` is given.
fn option_values<'a>(args: &'a [OsString], name: &str) -> Vec<&'a OsStr> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            values.extend(iter.next().map(OsString::as_os_str));
        } else if let Some(value) = arg
            .to_str()
            .and_then(|a| a.strip_prefix(name))
            .and_then(|a| a.strip_prefix('='))
        {
            values.push(OsStr::new(value));
        }
    }
    values
}

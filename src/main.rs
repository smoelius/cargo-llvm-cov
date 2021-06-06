#![forbid(unsafe_code)]
#![warn(future_incompatible, rust_2018_idioms, single_use_lifetimes, unreachable_pub)]
#![warn(clippy::default_trait_access, clippy::wildcard_imports)]

// Refs:
// - https://doc.rust-lang.org/nightly/unstable-book/compiler-flags/instrument-coverage.html
// - https://llvm.org/docs/CommandGuide/llvm-profdata.html
// - https://llvm.org/docs/CommandGuide/llvm-cov.html

#[macro_use]
mod trace;

#[macro_use]
mod process;

mod fs;

#[cfg(test)]
mod tests;

use std::{
    env,
    ffi::OsString,
    ops,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, format_err, Context as _, Error, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use structopt::{clap::AppSettings, StructOpt};

use crate::process::ProcessBuilder;

#[derive(Debug, StructOpt)]
#[structopt(
    bin_name = "cargo",
    rename_all = "kebab-case",
    setting = AppSettings::DeriveDisplayOrder,
    setting = AppSettings::UnifiedHelpMessage,
)]
enum Opts {
    /// A wrapper for source based code coverage (-Zinstrument-coverage).
    LlvmCov(Args),
}

#[derive(Debug, StructOpt)]
#[structopt(
    rename_all = "kebab-case",
    setting = AppSettings::DeriveDisplayOrder,
    setting = AppSettings::UnifiedHelpMessage,
)]
struct Args {
    /// Export coverage data in "json" format
    ///
    /// If --output-path is not specified, the report will be printed to stdout.
    ///
    /// This internally calls `llvm-cov export -format=text`.
    /// See <https://llvm.org/docs/CommandGuide/llvm-cov.html#llvm-cov-export> for more.
    #[structopt(long)]
    json: bool,
    /// Export coverage data in "lcov" format.
    ///
    /// If --output-path is not specified, the report will be printed to stdout.
    ///
    /// This internally calls `llvm-cov export -format=lcov`.
    /// See <https://llvm.org/docs/CommandGuide/llvm-cov.html#llvm-cov-export> for more.
    #[structopt(long, conflicts_with = "json")]
    lcov: bool,

    /// Generate coverage reports in “text” format.
    ///
    /// If --output-path or --output-dir is not specified, the report will be printed to stdout.
    ///
    /// This internally calls `llvm-cov show -format=text`.
    /// See <https://llvm.org/docs/CommandGuide/llvm-cov.html#llvm-cov-show> for more.
    #[structopt(long, conflicts_with_all = &["json", "lcov"])]
    text: bool,
    /// Generate coverage reports in "html" format.
    ////
    /// If --output-dir is not specified, the report will be generated in `target/llvm-cov` directory.
    ///
    /// This internally calls `llvm-cov show -format=html`.
    /// See <https://llvm.org/docs/CommandGuide/llvm-cov.html#llvm-cov-show> for more.
    #[structopt(long, conflicts_with_all = &["json", "lcov", "text"])]
    html: bool,
    /// Generate coverage reports in "html" format and open them in a browser after the operation.
    ///
    /// See --html for more.
    #[structopt(long, conflicts_with_all = &["json", "lcov", "text"])]
    open: bool,

    /// Export only summary information for each file in the coverage data.
    ///
    /// This flag can only be used together with either --json or --lcov.
    // If the format flag is not specified, this flag is no-op because the only summary is displayed anyway.
    #[structopt(long, conflicts_with_all = &["text", "html", "open"])]
    summary_only: bool,
    /// Specify a file to write coverage data into.
    ///
    /// This flag can only be used together with --json, --lcov, or --text.
    /// See --output-dir for --html and --open.
    #[structopt(long, value_name = "PATH", conflicts_with_all = &["html", "open"])]
    output_path: Option<PathBuf>,
    /// Specify a directory to write coverage reports into (default to `target/llvm-cov`).
    ///
    /// This flag can only be used together with --text, --html, or --open.
    /// See also --output-path.
    // If the format flag is not specified, this flag is no-op.
    #[structopt(long, value_name = "DIRECTORY", conflicts_with_all = &["json", "lcov", "output-path"])]
    output_dir: Option<PathBuf>,

    /// Skip source code files with file paths that match the given regular expression.
    #[structopt(long, value_name = "PATTERN")]
    ignore_filename_regex: Option<String>,
    // For debugging (unstable)
    #[structopt(long, hidden = true)]
    disable_default_ignore_filename_regex: bool,

    // https://doc.rust-lang.org/nightly/unstable-book/compiler-flags/instrument-coverage.html#including-doc-tests
    /// Including doc tests (unstable)
    #[structopt(long)]
    doctests: bool,

    // =========================================================================
    // `cargo test` options
    // https://doc.rust-lang.org/cargo/commands/cargo-test.html
    /// Run all tests regardless of failure
    #[structopt(long)]
    no_fail_fast: bool,
    // TODO: --package doesn't work properly, use --manifest-path instead for now.
    // /// Package to run tests for
    // #[structopt(short, long, value_name = "SPEC")]
    // package: Vec<String>,
    /// Test all packages in the workspace
    #[structopt(long, visible_alias = "all")]
    workspace: bool,
    /// Exclude packages from the test
    #[structopt(long, value_name = "SPEC")]
    exclude: Vec<String>,
    // TODO: Should this only work for cargo's --jobs? Or should it also work
    //       for llvm-cov's -num-threads?
    // /// Number of parallel jobs, defaults to # of CPUs
    // #[structopt(short, long, value_name = "N")]
    // jobs: Option<u64>,
    /// Build artifacts in release mode, with optimizations
    #[structopt(long)]
    release: bool,
    /// Space or comma separated list of features to activate
    #[structopt(long, value_name = "FEATURES")]
    features: Vec<String>,
    /// Activate all available features
    #[structopt(long)]
    all_features: bool,
    /// Do not activate the `default` feature
    #[structopt(long)]
    no_default_features: bool,
    /// Build for the target triple
    #[structopt(long, value_name = "TRIPLE")]
    target: Option<String>,
    // TODO: Currently, we are using a subdirectory of the target directory as
    //       the actual target directory. What effect should this option have
    //       on its behavior?
    // /// Directory for all generated artifacts
    // #[structopt(long, value_name = "DIRECTORY", parse(from_os_str))]
    // target_dir: Option<PathBuf>,
    /// Path to Cargo.toml
    #[structopt(long, value_name = "PATH", parse(from_os_str))]
    manifest_path: Option<PathBuf>,
    /// Use verbose output (-vv very verbose/build.rs output)
    #[structopt(short, long, parse(from_occurrences))]
    verbose: u8,
    /// Coloring: auto, always, never
    // This flag will be propagated to both cargo and llvm-cov.
    #[structopt(long, value_name = "WHEN")]
    color: Option<Coloring>,
    /// Require Cargo.lock and cache are up to date
    #[structopt(long)]
    frozen: bool,
    /// Require Cargo.lock is up to date
    #[structopt(long)]
    locked: bool,

    /// Unstable (nightly-only) flags to Cargo
    #[structopt(short = "Z", value_name = "FLAG")]
    unstable_flags: Vec<String>,

    /// Arguments for the test binary
    #[structopt(last = true, parse(from_os_str))]
    args: Vec<OsString>,
}

impl Args {
    fn show(&self) -> bool {
        self.text || self.html
    }
}

#[derive(Debug, Clone, Copy)]
enum Coloring {
    Auto,
    Always,
    Never,
}

impl Coloring {
    fn cargo_color(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

impl FromStr for Coloring {
    type Err = Error;

    fn from_str(color: &str) -> Result<Self, Self::Err> {
        match color {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            other => bail!("must be auto, always, or never, but found `{}`", other),
        }
    }
}

fn main() -> Result<()> {
    trace::init();

    run(env::args_os())
}

fn run(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> Result<()> {
    let cx = &Context::new(args)?;

    fs::create_dir_all(&cx.target_dir)?;
    if let Some(output_dir) = &cx.output_dir {
        fs::remove_dir_all(output_dir)?;
        fs::create_dir_all(output_dir)?;
    }
    for path in glob::glob(cx.target_dir.join("*.profraw").as_str())?.filter_map(Result::ok) {
        fs::remove_file(path)?;
    }

    // https://doc.rust-lang.org/nightly/unstable-book/compiler-flags/instrument-coverage.html#including-doc-tests
    let doctests_dir = &cx.target_dir.join("doctestbins");
    if cx.doctests {
        fs::remove_dir_all(doctests_dir)?;
        fs::create_dir(doctests_dir)?;
    }

    let package_name = cx.metadata.workspace_root.file_stem().unwrap();
    let profdata_file = &cx.target_dir.join(format!("{}.profdata", package_name));
    fs::remove_file(profdata_file)?;
    let llvm_profile_file = cx.target_dir.join(format!("{}-%m.profraw", package_name));

    let rustflags = &mut match env::var_os("RUSTFLAGS") {
        Some(rustflags) => rustflags,
        None => OsString::new(),
    };
    debug!(RUSTFLAGS = ?rustflags);
    // --remap-path-prefix for Sometimes macros are displayed with abs path
    rustflags.push(format!(
        " -Zinstrument-coverage --remap-path-prefix {}/=",
        cx.metadata.workspace_root
    ));

    let rustdocflags = &mut env::var_os("RUSTDOCFLAGS");
    debug!(RUSTDOCFLAGS = ?rustdocflags);
    if cx.doctests {
        let flags = rustdocflags.get_or_insert_with(OsString::new);
        flags.push(format!(
            " -Zinstrument-coverage -Zunstable-options --persist-doctests {}",
            doctests_dir
        ));
    }

    let mut cargo = cx.process(&cx.cargo);
    if !cx.nightly {
        cargo.arg("+nightly");
    }

    cargo.env("RUSTFLAGS", rustflags);
    cargo.env("LLVM_PROFILE_FILE", &*llvm_profile_file);
    if let Some(rustdocflags) = rustdocflags {
        cargo.env("RUSTDOCFLAGS", rustdocflags);
    }

    cargo.args(&["test", "--target-dir"]).arg(&cx.target_dir);
    append_args(cx, &mut cargo);
    cargo.stdout_to_stderr().run()?;
    cargo.stdout_to_stderr = false;

    if let Some(verbose) = &cx.verbose {
        cargo.args.remove(cargo.args.iter().position(|arg| *arg == **verbose).unwrap());
    }
    let output = cargo
        .arg("--no-run")
        .arg("--message-format=json")
        .stdout_capture()
        .stderr_capture()
        .read()?;
    let mut files = vec![];
    for (_, s) in output.lines().filter(|s| !s.is_empty()).enumerate() {
        let ar = serde_json::from_str::<Artifact>(s)?;
        if ar.profile.map_or(false, |p| p.test) {
            files.extend(ar.filenames.into_iter().filter(|s| !s.ends_with("dSYM")));
        }
    }
    if cx.doctests {
        for f in glob::glob(doctests_dir.join("*/rust_out").as_str())?.filter_map(Result::ok) {
            if is_executable::is_executable(&f) {
                files.push(f.to_string_lossy().into_owned())
            }
        }
    }
    trace!(objects = ?files);

    // Convert raw profile data.
    cx.process(&cx.llvm_profdata)
        .args(&["merge", "-sparse"])
        .args(
            glob::glob(cx.target_dir.join(format!("{}-*.profraw", package_name)).as_str())?
                .filter_map(Result::ok),
        )
        .arg("-o")
        .arg(profdata_file)
        .run()?;

    let format = Format::from_args(cx);
    format.run(cx, profdata_file, &files)?;

    if format == Format::Html {
        Format::None.run(cx, profdata_file, &files)?;

        if cx.open {
            open::that(Path::new(cx.output_dir.as_ref().unwrap()).join("index.html"))?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `llvm-cov report`
    None,
    /// `llvm-cov export -format=text`
    Json,
    /// `llvm-cov export -format=lcov`
    LCov,
    /// `llvm-cov show -format=text`
    Text,
    /// `llvm-cov show -format=html`
    Html,
}

impl Format {
    fn from_args(args: &Args) -> Self {
        if args.json {
            Self::Json
        } else if args.lcov {
            Self::LCov
        } else if args.text {
            Self::Text
        } else if args.html {
            Self::Html
        } else {
            Self::None
        }
    }

    fn llvm_cov_args(self) -> &'static [&'static str] {
        match self {
            Self::None => &["report"],
            Self::Json => &["export", "-format=text"],
            Self::LCov => &["export", "-format=lcov"],
            Self::Text => &["show", "-format=text"],
            Self::Html => &["show", "-format=html"],
        }
    }

    fn use_color(self, color: Option<Coloring>) -> Option<&'static str> {
        if matches!(self, Self::Json | Self::LCov) {
            // `llvm-cov export` doesn't have `-use-color` flag.
            // https://llvm.org/docs/CommandGuide/llvm-cov.html#llvm-cov-export
            return None;
        }
        match color {
            Some(Coloring::Auto) | None => None,
            Some(Coloring::Always) => Some("-use-color=1"),
            Some(Coloring::Never) => Some("-use-color=0"),
        }
    }

    fn run(self, cx: &Context, profdata_file: &Utf8Path, files: &[String]) -> Result<()> {
        const DEFAULT_IGNORE_FILENAME_REGEX: &str =
            r"rustc/|.cargo/(registry|git)/|.rustup/toolchains/|test(s)?/|target/llvm-cov-target/";

        let mut cmd = cx.process(&cx.llvm_cov);

        cmd.args(self.llvm_cov_args())
            .args(self.use_color(cx.color))
            .arg(format!("-instr-profile={}", profdata_file))
            .args(files.iter().flat_map(|f| vec!["-object", f]));

        // TODO: Currently, there is a problem that excluded crates are
        // incorrectly shown in the coverage report if they are path
        // dependencies of other crates.
        if cx.disable_default_ignore_filename_regex {
            if let Some(ignore_filename_regex) = &cx.ignore_filename_regex {
                cmd.arg("-ignore-filename-regex");
                cmd.arg(ignore_filename_regex);
            }
        } else {
            cmd.arg("-ignore-filename-regex");
            if let Some(ignore_filename_regex) = &cx.ignore_filename_regex {
                cmd.arg(format!("{}|{}", ignore_filename_regex, DEFAULT_IGNORE_FILENAME_REGEX));
            } else {
                cmd.arg(DEFAULT_IGNORE_FILENAME_REGEX);
            }
        }

        match self {
            Self::Text | Self::Html => {
                cmd.args(&[
                    "-show-instantiations",
                    "-show-line-counts-or-regions",
                    "-show-expansions",
                    "-Xdemangler=rustfilt",
                ]);
                if let Some(output_dir) = &cx.output_dir {
                    cmd.arg(&format!("-output-dir={}", output_dir.display()));
                }
            }
            Self::Json | Self::LCov => {
                if cx.summary_only {
                    cmd.arg("-summary-only");
                }
            }
            Self::None => {}
        }

        if let Some(output_path) = &cx.output_path {
            let out = cmd.stdout_capture().read()?;
            fs::write(output_path, out)?;
            return Ok(());
        }

        cmd.run()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Artifact {
    profile: Option<Profile>,
    #[serde(default)]
    filenames: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    test: bool,
}

struct Context {
    args: Args,
    verbose: Option<String>,
    metadata: cargo_metadata::Metadata,
    manifest_path: PathBuf,
    target_dir: Utf8PathBuf,
    llvm_cov: Utf8PathBuf,
    llvm_profdata: Utf8PathBuf,
    cargo: OsString,
    nightly: bool,
}

impl Context {
    fn new(args_raw: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> Result<Self> {
        let matches = Opts::clap().get_matches_from(args_raw);
        let Opts::LlvmCov(mut args) = StructOpt::from_clap(&matches);
        let verbose = if args.verbose == 0 {
            None
        } else {
            Some(format!("-{}", "v".repeat(args.verbose as _)))
        };
        debug!(?args);
        args.html |= args.open;
        if args.output_dir.is_some() && !args.show() {
            // If the format flag is not specified, this flag is no-op.
            args.output_dir = None;
        }
        if args.color.is_none() {
            // https://doc.rust-lang.org/cargo/reference/config.html#termcolor
            args.color = env::var("CARGO_TERM_COLOR").ok().map(|s| s.parse()).transpose()?;
            debug!(?args.color);
        }

        let package_root = if let Some(manifest_path) = &args.manifest_path {
            manifest_path.clone()
        } else {
            process!("cargo", "locate-project", "--message-format", "plain")
                .stdout_capture()
                .read()?
                .into()
        };

        let metadata =
            cargo_metadata::MetadataCommand::new().manifest_path(&package_root).exec()?;
        let cargo_target_dir = &metadata.target_directory;
        debug!(?package_root, ?metadata.workspace_root, ?metadata.target_directory);

        if args.output_dir.is_none() && args.html {
            args.output_dir = Some(cargo_target_dir.join("llvm-cov").into());
        }

        // If we change RUSTFLAGS, all dependencies will be recompiled. Therefore,
        // use a subdirectory of the target directory as the actual target directory.
        let target_dir = cargo_target_dir.join("llvm-cov-target");

        let mut cargo = cargo();
        let version =
            process!(&cargo, "version").dir(&metadata.workspace_root).stdout_capture().read()?;
        let nightly = version.contains("-nightly") || version.contains("-dev");
        if !nightly {
            cargo = "cargo".into();
        }

        let sysroot: Utf8PathBuf = sysroot(nightly)?.into();
        // https://github.com/rust-lang/rust/issues/85658
        // https://github.com/rust-lang/rust/blob/595088d602049d821bf9a217f2d79aea40715208/src/bootstrap/dist.rs#L2009
        let rustlib = sysroot.join(format!("lib/rustlib/{}/bin", host()?));
        let llvm_cov = rustlib.join(format!("{}{}", "llvm-cov", env::consts::EXE_SUFFIX));
        let llvm_profdata = rustlib.join(format!("{}{}", "llvm-profdata", env::consts::EXE_SUFFIX));

        debug!(?llvm_cov, ?llvm_profdata, ?cargo, ?nightly);

        // Check if required tools are installed.
        if !llvm_cov.exists() || !llvm_profdata.exists() {
            bail!(
                "failed to find llvm-tools-preview, please install llvm-tools-preview with `rustup component add llvm-tools-preview{}`",
                if !nightly { " --toolchain nightly" } else { "" }
            );
        }
        if args.show() {
            process!("rustfilt", "-V").stdout_capture().run().with_context(|| {
                format!(
                    "{} flag requires rustfilt, please install rustfilt with `cargo install rustfilt`",
                    if args.html { "--html" } else { "--text" }
                )
            })?;
        }

        Ok(Self {
            args,
            verbose,
            metadata,
            manifest_path: package_root,
            target_dir,
            llvm_cov,
            llvm_profdata,
            cargo,
            nightly,
        })
    }

    fn process(&self, program: impl Into<OsString>) -> ProcessBuilder {
        let mut cmd = process!(program);
        cmd.dir(&self.metadata.workspace_root);
        if self.verbose.is_some() {
            cmd.display_env_vars();
        }
        cmd
    }
}

impl ops::Deref for Context {
    type Target = Args;

    fn deref(&self) -> &Self::Target {
        &self.args
    }
}

fn sysroot(nightly: bool) -> Result<String> {
    Ok(if nightly {
        process!(rustc(), "--print", "sysroot")
    } else {
        process!("rustup", "run", "nightly", "rustc", "--print", "sysroot")
    }
    .stdout_capture()
    .read()
    .context("failed to find sysroot")?
    .trim()
    .into())
}

fn host() -> Result<String> {
    let rustc = &rustc();
    let output = process!(rustc, "--version", "--verbose").stdout_capture().read()?;
    output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| {
            format_err!("unexpected version output from `{}`: {}", rustc.to_string_lossy(), output)
        })
        .map(ToString::to_string)
}

fn rustc() -> OsString {
    env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"))
}

fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn append_args(cx: &Context, cmd: &mut ProcessBuilder) {
    if cx.no_fail_fast {
        cmd.arg("--no-fail-fast");
    }
    if cx.workspace {
        cmd.arg("--workspace");
    }
    for exclude in &cx.exclude {
        cmd.arg("--exclude");
        cmd.arg(exclude);
    }
    if cx.release {
        cmd.arg("--release");
    }
    for features in &cx.features {
        cmd.arg("--features");
        cmd.arg(features);
    }
    if cx.all_features {
        cmd.arg("--all-features");
    }
    if cx.no_default_features {
        cmd.arg("--no-default-features");
    }
    if let Some(target) = &cx.target {
        cmd.arg("--target");
        cmd.arg(target);
    }

    cmd.arg("--manifest-path");
    cmd.arg(&cx.manifest_path);

    if let Some(color) = cx.color {
        cmd.arg("--color");
        cmd.arg(color.cargo_color());
    }
    if cx.frozen {
        cmd.arg("--frozen");
    }
    if cx.locked {
        cmd.arg("--locked");
    }

    if let Some(verbose) = &cx.verbose {
        cmd.arg(verbose);
    }

    for unstable_flag in &cx.unstable_flags {
        cmd.arg("-Z");
        cmd.arg(unstable_flag);
    }

    if !cx.args.args.is_empty() {
        cmd.arg("--");
        cmd.args(&cx.args.args);
    }
}

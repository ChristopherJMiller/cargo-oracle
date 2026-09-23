//! `cargo oracle`: symbol-level test attribution for Rust.
//!
//! `lint` is entirely static: it parses the workspace, works out which tests
//! claim which symbols, and reports the oracles that cannot discriminate. No
//! build, no test run, no instrumentation.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use oracle_core::attribution;
use oracle_core::claims::ClaimMap;
use oracle_core::coverage::{self, CoverageMap};
use oracle_core::fastmutate::{self, FastOutcome, Plan};
use oracle_core::inventory::walk_workspace;
use oracle_core::lint::{Rule, Severity};
use oracle_core::mutation::{self, MutationMap};
use oracle_core::report::{self, ColorChoice, Report, Styles};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "cargo-oracle",
    version,
    about = "Which tests verify which symbols, not just which lines ran",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to a Cargo.toml or the directory containing one.
    #[arg(long, global = true, value_name = "PATH")]
    manifest_path: Option<PathBuf>,

    /// Output format, following cargo's convention.
    #[arg(long, global = true, value_enum, default_value_t = MessageFormat::Human, value_name = "FMT")]
    message_format: MessageFormat,

    /// Coloring: auto, always, never.
    #[arg(long, global = true, value_enum, default_value_t = ColorArg::Auto, value_name = "WHEN")]
    color: ColorArg,

    /// Show every test, plus the reasoning behind each finding.
    #[arg(short, long, global = true)]
    verbose: bool,
}

/// Cargo names these `human`, `short` and `json`; so do we.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum MessageFormat {
    /// rustc-style diagnostics with source windows.
    Human,
    /// One line per finding: `file:line:col: warning: message`.
    Short,
    /// Machine-readable.
    Json,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ColorArg {
    Auto,
    Always,
    Never,
}

impl From<ColorArg> for ColorChoice {
    fn from(c: ColorArg) -> Self {
        match c {
            ColorArg::Auto => ColorChoice::Auto,
            ColorArg::Always => ColorChoice::Always,
            ColorArg::Never => ColorChoice::Never,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum DenyLevel {
    Low,
    Medium,
    High,
    Never,
}

#[derive(Subcommand)]
enum Command {
    /// Audit test oracles statically (the default).
    Lint {
        /// Exit non-zero when a finding at or above this severity is reported.
        #[arg(long, value_enum, default_value_t = DenyLevel::Never)]
        deny: DenyLevel,
    },
    /// List testable symbols and the oracle shape each one requires.
    Inventory,
    /// Show which tests claim which symbols, and what nothing claims.
    Claims,
    /// Join `cargo llvm-cov` output onto the inventory: which symbols run.
    Coverage {
        /// Use an existing llvm-cov JSON report instead of producing one.
        #[arg(long, value_name = "PATH")]
        coverage_json: Option<PathBuf>,
        /// Extra arguments forwarded to `cargo llvm-cov`, after `--`.
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
    /// Per-test attribution: which test executes which symbol.
    ///
    /// Profiles the suite once per test, so cost is O(tests). Scope it with
    /// --tests on anything large.
    Attribute {
        /// Only profile tests whose name contains this substring.
        #[arg(long, value_name = "SUBSTRING")]
        tests: Option<String>,
        /// List what would be profiled, and stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Mutation verification: which tests would fail if a symbol broke.
    ///
    /// Recompiles once per mutant, so scope it with --in-diff on anything
    /// larger than a small crate.
    Verify {
        /// Read an existing mutants.out directory instead of running.
        #[arg(long, value_name = "DIR")]
        mutants_out: Option<PathBuf>,
        /// Only mutate regions changed in this diff file.
        #[arg(long, value_name = "FILE", conflicts_with = "since")]
        in_diff: Option<PathBuf>,
        /// Only mutate regions changed since this git ref, e.g. `origin/main`.
        ///
        /// Diffs the working tree against the ref, so the new side always
        /// matches what is on disk -- which is what cargo-mutants requires.
        #[arg(long, value_name = "REF")]
        since: Option<String>,
        /// Also profile per-test attribution, so the report can say
        /// `executes N, verifies M`. Costs one instrumented run per test.
        #[arg(long)]
        with_attribution: bool,
        /// Extra arguments forwarded to `cargo mutants`, after `--`.
        #[arg(last = true)]
        cargo_args: Vec<String>,
    },
    /// Experimental: single-compile mutation.
    ///
    /// Rewrites the crate once with every mutation behind a runtime switch,
    /// builds once, then runs the suite once per mutant. Trades cargo-mutants'
    /// N rebuilds for N test runs, which is the better deal in Rust where the
    /// build dominates. Narrower than `verify`: body replacement only.
    Fastverify {
        /// Where to write the rewritten tree.
        #[arg(long, value_name = "DIR")]
        scratch: Option<PathBuf>,
        /// Show the plan, including what is skipped and why, then stop.
        #[arg(long)]
        dry_run: bool,
        /// Path to the `oracle-switch` crate. Defaults to the copy that was
        /// built alongside this binary.
        #[arg(long, value_name = "DIR")]
        switch_path: Option<PathBuf>,
    },
    /// Explain a lint rule: what it means, why it matters, how to fix it.
    ///
    /// With no argument, lists every rule.
    Explain {
        /// Rule id (`ORC002`) or name (`discriminant-only`).
        #[arg(value_name = "RULE")]
        rule: Option<String>,
    },
    /// The full audit: inventory, claims, and oracle findings.
    Report {
        #[arg(long, value_enum, default_value_t = DenyLevel::Never)]
        deny: DenyLevel,
    },
}

fn main() -> Result<()> {
    // Invoked as `cargo oracle ...`, argv[1] is the subcommand name itself.
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    if argv.get(1).map(|a| a == "oracle").unwrap_or(false) {
        argv.remove(1);
    }
    let cli = Cli::parse_from(argv);

    let dir = resolve_dir(cli.manifest_path.as_deref())?;
    let inventory = walk_workspace(&dir)?;

    let exit = match cli.command.unwrap_or(Command::Lint {
        deny: DenyLevel::Never,
    }) {
        Command::Lint { deny } | Command::Report { deny } => {
            let report = Report::build(&inventory);
            match cli.message_format {
                // Machine formats go to stdout so they can be piped; human
                // diagnostics go to stderr, as clippy and rustc do.
                MessageFormat::Json => println!("{}", report.to_json()),
                MessageFormat::Short => eprint!("{}", report.to_short()),
                MessageFormat::Human => eprint!(
                    "{}",
                    report.to_text(cli.verbose, Styles::resolve(cli.color.into()))
                ),
            }
            match floor(deny) {
                Some(floor) => report.findings_at_least(floor).count(),
                None => 0,
            }
        }

        Command::Inventory => {
            match cli.message_format {
                MessageFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&inventory.symbols)?)
                }
                //  has no distinct shape for a listing.
                _ => {
                    println!("{:<52} {:<12} {:<13} NOTES", "SYMBOL", "KIND", "NEEDS");
                    for s in &inventory.symbols {
                        if !cli.verbose && !s.triviality.is_scorable() {
                            continue;
                        }
                        let mut notes = Vec::new();
                        if !s.triviality.is_scorable() {
                            notes.push(format!("{:?}", s.triviality).to_lowercase());
                        }
                        if s.likely_unviable_to_mutate() {
                            notes.push("type-enforced?".into());
                        }
                        if s.doctests > 0 {
                            notes.push(format!("{} doctest(s)", s.doctests));
                        }
                        println!(
                            "{:<52} {:<12} {:<13} {}",
                            truncate(&s.path, 52),
                            format!("{:?}", s.kind).to_lowercase(),
                            format!("{:?}", s.required_oracle).to_lowercase(),
                            notes.join(", ")
                        );
                    }
                }
            }
            0
        }

        Command::Coverage {
            coverage_json,
            cargo_args,
        } => {
            let root = PathBuf::from(&inventory.root);
            let path = match coverage_json {
                Some(path) => path,
                None => {
                    let out = root.join("target/oracle/coverage.json");
                    eprintln!("running `cargo llvm-cov` (builds and runs the test suite)...");
                    coverage::run_llvm_cov(&dir, &out, &cargo_args)?;
                    out
                }
            };
            let data = coverage::load(&path, &root)?;
            let map = CoverageMap::join(&inventory, &data);
            match cli.message_format {
                MessageFormat::Json => println!("{}", serde_json::to_string_pretty(&map)?),
                // A listing has no distinct short form.
                _ => {
                    print!("{}", report::render_coverage(&inventory, &map, cli.verbose))
                }
            }
            0
        }

        Command::Attribute { tests, dry_run } => {
            let listed = attribution::list_tests(&dir)?;
            let selected: Vec<_> = listed
                .into_iter()
                .filter(|t| tests.as_ref().is_none_or(|f| t.testcase.contains(f)))
                .collect();

            if selected.is_empty() {
                eprintln!("no tests matched");
                return Ok(());
            }
            if dry_run {
                for t in &selected {
                    println!("{}  {}", t.binary_id, t.testcase);
                }
                println!("\n{} test(s) would be profiled", selected.len());
                return Ok(());
            }

            eprintln!(
                "profiling {} test(s), one instrumented run each; this is the expensive stage",
                selected.len()
            );
            let scratch = PathBuf::from(&inventory.root).join("target/oracle");
            let map = attribution::build(&inventory, &dir, &scratch, &selected, |i, n, t| {
                eprintln!("  [{i}/{n}] {}", t.testcase);
            })?;

            match cli.message_format {
                MessageFormat::Json => println!("{}", serde_json::to_string_pretty(&map)?),
                // A listing has no distinct short form.
                _ => print!(
                    "{}",
                    report::render_attribution(&inventory, &map, cli.verbose)
                ),
            }
            0
        }

        Command::Verify {
            mutants_out,
            in_diff,
            since,
            with_attribution,
            mut cargo_args,
        } => {
            let root = PathBuf::from(&inventory.root);
            // Kept so the map can record which regions the run actually
            // examined -- see MutationMap::with_scope.
            let mut scope_diff: Option<String> = None;

            let out_dir = match mutants_out {
                Some(dir) => dir,
                None => {
                    let diff_file = match (&since, &in_diff) {
                        (Some(base), _) => {
                            let diff = mutation::diff_since(&root, base)?;
                            if !mutation::diff_touches_rust(&diff) {
                                // A docs-only change has nothing to mutate.
                                // That is a pass, not an empty report -- a CI
                                // gate must not fail a README edit.
                                println!("no Rust source changed since {base}; nothing to verify");
                                return Ok(());
                            }
                            eprintln!(
                                "scoped to {} changed Rust file(s) since {base}",
                                mutation::files_in_diff(&diff)
                                    .iter()
                                    .filter(|f| f.ends_with(".rs"))
                                    .count()
                            );
                            let path = root.join("target/oracle/since.diff");
                            if let Some(parent) = path.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&path, &diff)?;
                            scope_diff = Some(diff);
                            Some(path)
                        }
                        (None, Some(path)) => {
                            scope_diff = std::fs::read_to_string(path).ok();
                            Some(path.clone())
                        }
                        (None, None) => None,
                    };

                    if let Some(path) = diff_file {
                        cargo_args.push("--in-diff".into());
                        cargo_args.push(path.display().to_string());
                    }

                    eprintln!(
                        "running `cargo mutants` -- one rebuild per mutant, so this is the \
                         expensive stage"
                    );
                    mutation::run_mutants(&dir, &cargo_args)?
                }
            };

            let mutants = mutation::load(&out_dir)?;
            let mut map = MutationMap::build(&inventory, &mutants);
            if let Some(diff) = &scope_diff {
                map = map.with_scope(mutation::DiffScope::parse(diff));
            }

            let attribution = if with_attribution {
                let tests = attribution::list_tests(&dir)?;
                eprintln!("profiling {} test(s) for attribution...", tests.len());
                let scratch = root.join("target/oracle");
                Some(attribution::build(
                    &inventory,
                    &dir,
                    &scratch,
                    &tests,
                    |i, n, t| eprintln!("  [{i}/{n}] {}", t.testcase),
                )?)
            } else {
                None
            };

            match cli.message_format {
                MessageFormat::Json => println!("{}", serde_json::to_string_pretty(&map)?),
                // A listing has no distinct short form.
                _ => print!(
                    "{}",
                    report::render_verification(
                        &inventory,
                        &map,
                        attribution.as_ref(),
                        cli.verbose
                    )
                ),
            }
            0
        }

        Command::Fastverify {
            scratch,
            dry_run,
            switch_path,
        } => {
            let root = PathBuf::from(&inventory.root);
            let plan = Plan::build(&inventory);

            if dry_run {
                for mutant in &plan.mutants {
                    println!(
                        "{:>4}  {:<52} {}",
                        mutant.id, mutant.path, mutant.replacement
                    );
                }
                let mut reasons: std::collections::BTreeMap<&str, usize> = Default::default();
                for (_, reason) in &plan.skipped {
                    *reasons.entry(reason.label()).or_default() += 1;
                }
                println!("\n{} mutant(s) planned", plan.mutants.len());
                for (reason, n) in reasons {
                    println!("  {n} skipped: {reason}");
                }
                return Ok(());
            }
            if plan.mutants.is_empty() {
                println!("nothing to mutate");
                return Ok(());
            }

            let scratch = scratch.unwrap_or_else(|| root.join("target/oracle/fast"));
            // The rewritten tree needs an absolute path to the switch crate.
            // It lives next to this binary's own crate in the cargo-oracle
            // workspace, which is where it is by default; an installed binary
            // has no such sibling, so --switch-path overrides.
            let switch = switch_path.unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .map(|p| p.join("oracle-switch"))
                    .unwrap_or_default()
            });
            if !switch.join("Cargo.toml").is_file() {
                anyhow::bail!(
                    "no oracle-switch crate at {}. Pass --switch-path to point at it.",
                    switch.display()
                );
            }

            eprintln!(
                "rewriting {} function(s) into {}",
                plan.mutants.len(),
                scratch.display()
            );
            let manifest_text = std::fs::read_to_string(root.join("Cargo.toml"))?;
            if fastmutate::is_virtual_workspace(&manifest_text) {
                anyhow::bail!(
                    "fastverify does not yet support virtual workspaces: every member \
                     needs the switch dependency added separately. Point \
                     --manifest-path at a single crate."
                );
            }

            fastmutate::rewrite_tree(&root, &scratch, &inventory, &plan)?;
            fastmutate::patch_manifest(&scratch.join("Cargo.toml"), &switch)?;

            eprintln!("building once, and checking the baseline is green...");
            fastmutate::run_baseline(&scratch)?;

            let log = scratch.join("oracle-notes.log");
            let mut outcomes = Vec::new();
            for mutant in &plan.mutants {
                eprint!(
                    "  [{}/{}] {} ... ",
                    mutant.id + 1,
                    plan.mutants.len(),
                    mutant.path
                );
                let outcome = fastmutate::run_mutant(&scratch, mutant.id, &log)?;
                eprintln!("{}", outcome.label());
                outcomes.push((mutant, outcome));
            }

            println!();
            for (mutant, outcome) in &outcomes {
                if *outcome == FastOutcome::Caught {
                    continue;
                }
                println!("  {:<52} {}", mutant.path, outcome.label());
            }

            let count = |want: FastOutcome| outcomes.iter().filter(|(_, o)| *o == want).count();
            println!(
                "\nsummary\n  caught      {}\n  SURVIVED    {}\n  inert       {} (no Default for the return type)\n  unreached   {} (no test calls it)",
                count(FastOutcome::Caught),
                count(FastOutcome::Survived),
                count(FastOutcome::Inert),
                count(FastOutcome::Unreached),
            );
            println!(
                "\nexperimental: body replacement only, and `unreached` is a distinction\n`cargo oracle verify` cannot make. Use verify for the authoritative answer."
            );
            0
        }

        Command::Explain { rule } => {
            let Some(needle) = rule else {
                println!("{:<9} {:<26} SEVERITY", "ID", "NAME");
                for r in Rule::ALL {
                    println!(
                        "{:<9} {:<26} {}",
                        r.id(),
                        r.name(),
                        format!("{:?}", r.severity()).to_lowercase()
                    );
                }
                println!("\ncargo oracle explain <ID|NAME>   for detail on one rule");
                return Ok(());
            };

            let Some(r) = Rule::find(&needle) else {
                anyhow::bail!("no rule `{needle}`. Run `cargo oracle explain` to list them all.");
            };

            println!(
                "{}  {}   severity: {}\n",
                r.id(),
                r.name(),
                format!("{:?}", r.severity()).to_lowercase()
            );
            println!("why it matters\n  {}\n", wrap_text(r.why(), 2, 76));
            println!("how to fix it\n  {}", wrap_text(r.fix(), 2, 76));
            0
        }

        Command::Claims => {
            let claims = ClaimMap::build(&inventory);
            match cli.message_format {
                MessageFormat::Json => println!("{}", serde_json::to_string_pretty(&claims)?),
                // A listing has no distinct short form.
                _ => {
                    for symbol in inventory.scorable() {
                        let claimants = claims.claimants(&symbol.id);
                        if claimants.is_empty() {
                            println!("{:<52} UNCLAIMED", truncate(&symbol.path, 52));
                            continue;
                        }
                        println!("{}", symbol.path);
                        for (test, kind) in
                            claimants
                                .iter()
                                .take(if cli.verbose { usize::MAX } else { 4 })
                        {
                            println!(
                                "    {:<22} {}",
                                format!("{kind:?}").to_lowercase(),
                                test.path
                            );
                        }
                        if !cli.verbose && claimants.len() > 4 {
                            println!("    ... and {} more", claimants.len() - 4);
                        }
                    }
                }
            }
            0
        }
    };

    if exit > 0 {
        eprintln!("\n{exit} finding(s) at or above the --deny threshold");
        std::process::exit(1);
    }
    Ok(())
}

fn floor(deny: DenyLevel) -> Option<Severity> {
    match deny {
        DenyLevel::Low => Some(Severity::Low),
        DenyLevel::Medium => Some(Severity::Medium),
        DenyLevel::High => Some(Severity::High),
        DenyLevel::Never => None,
    }
}

fn resolve_dir(manifest_path: Option<&Path>) -> Result<PathBuf> {
    let Some(path) = manifest_path else {
        return std::env::current_dir().context("reading the current directory");
    };
    if path.is_dir() {
        return Ok(path.to_path_buf());
    }
    path.parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no parent directory", path.display()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}~")
}

/// Wrap prose to `width`, indenting continuation lines by `indent`.
fn wrap_text(text: &str, indent: usize, width: usize) -> String {
    let pad = " ".repeat(indent);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.len() + word.len() + 1 > width - indent {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.join(&format!("\n{pad}"))
}

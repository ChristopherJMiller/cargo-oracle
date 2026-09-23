//! `cargo oracle` — symbol-level test attribution for Rust.
//!
//! Slice v0 is entirely static: it parses the workspace, works out which tests
//! claim which symbols, and reports the oracles that cannot discriminate. No
//! build, no test run, no instrumentation.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use oracle_core::claims::ClaimMap;
use oracle_core::inventory::walk_workspace;
use oracle_core::lint::Severity;
use oracle_core::report::Report;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "cargo-oracle",
    version,
    about = "Which tests verify which symbols — not just which lines ran",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to a Cargo.toml or the directory containing one.
    #[arg(long, global = true, value_name = "PATH")]
    manifest_path: Option<PathBuf>,

    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Show every test, plus the reasoning behind each finding.
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
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
            match cli.format {
                Format::Json => println!("{}", report.to_json()),
                Format::Text => print!("{}", report.to_text(cli.verbose)),
            }
            match floor(deny) {
                Some(floor) => report.findings_at_least(floor).count(),
                None => 0,
            }
        }

        Command::Inventory => {
            match cli.format {
                Format::Json => println!("{}", serde_json::to_string_pretty(&inventory.symbols)?),
                Format::Text => {
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

        Command::Claims => {
            let claims = ClaimMap::build(&inventory);
            match cli.format {
                Format::Json => println!("{}", serde_json::to_string_pretty(&claims)?),
                Format::Text => {
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

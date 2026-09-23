//! The v0 static audit, driven from the library rather than the CLI.
//!
//! Parses a workspace, works out which tests claim which symbols, and prints
//! the tests whose oracles cannot discriminate. No build, no test run.
//!
//! ```sh
//! cargo run --example static_audit -- path/to/crate
//! ```

use oracle_core::claims::ClaimMap;
use oracle_core::inventory::walk_workspace;
use oracle_core::lint::{self, OracleStrength};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".to_string())
        .into();

    let inv = walk_workspace(&dir)?;
    println!(
        "{} symbols ({} scorable), {} tests across {} files\n",
        inv.symbols.len(),
        inv.scorable().count(),
        inv.tests.len(),
        inv.files.len()
    );

    // Which tests could not fail even if the code were wrong?
    let oracles = lint::analyze(&inv);
    let weak: Vec<_> = oracles
        .iter()
        .filter(|o| o.strength <= OracleStrength::Weak)
        .collect();

    if weak.is_empty() {
        println!("every test has at least a partial oracle.");
    } else {
        println!("tests whose strongest oracle is weak or absent:");
        for o in &weak {
            let rules: Vec<&str> = o.findings.iter().map(|f| f.rule.id()).collect();
            println!("  {:<60} {}", o.test.path, rules.join(" "));
        }
    }

    // ORC010 needs the claim map and the oracle analysis together: it asks
    // whether a test's oracles have the right *shape* for what they claim.
    let claims = ClaimMap::build(&inv);
    for finding in lint::shape_mismatches(&inv, &claims, &oracles) {
        println!(
            "\n{} {}\n  on {}\n  {}",
            finding.rule.id(),
            finding.rule.name(),
            finding.symbol.as_deref().unwrap_or("?"),
            finding.snippet
        );
    }

    // A coverage proxy that needs no build: nothing even nominally speaks for
    // these, so no amount of execution data will change their status.
    let unclaimed = claims.unclaimed(&inv, true);
    if !unclaimed.is_empty() {
        println!("\n{} symbol(s) no test directly claims:", unclaimed.len());
        for symbol in unclaimed.iter().take(10) {
            println!("  {}", symbol.path);
        }
    }

    Ok(())
}

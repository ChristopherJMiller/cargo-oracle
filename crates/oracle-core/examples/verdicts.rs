//! Read an existing `mutants.out` and print the verdict for every symbol.
//!
//! Assumes `cargo mutants` has already run, so this costs nothing:
//!
//! ```sh
//! cargo mutants --test-tool nextest
//! cargo run --example verdicts -- . mutants.out
//! ```

use oracle_core::inventory::walk_workspace;
use oracle_core::mutation::{self, MutationMap, Verification};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir: PathBuf = args.next().unwrap_or_else(|| ".".into()).into();
    let out: PathBuf = args.next().unwrap_or_else(|| "mutants.out".into()).into();

    let inv = walk_workspace(&dir)?;
    let mutants = mutation::load(&out)?;
    let map = MutationMap::build(&inv, &mutants);

    for symbol in inv.scorable() {
        // `verification_of` distinguishes "examined and produced nothing" from
        // "never looked at" -- at file granularity, and at hunk granularity for
        // a --in-diff run. Plain `verification` cannot tell them apart.
        let state = map.verification_of(symbol);
        if state == Verification::OutOfScope {
            continue;
        }

        let verdict = map.verdict(&symbol.id);
        let detail = match state {
            Verification::Verified => {
                let who: Vec<&str> = verdict.killed_by.iter().map(|t| t.path.as_str()).collect();
                format!("killed by {}", who.join(", "))
            }
            Verification::PseudoTested => {
                format!(
                    "{} survived: {}",
                    verdict.missed,
                    verdict.survivors.join(", ")
                )
            }
            _ => String::new(),
        };
        println!("{:<50} {:<18} {}", symbol.path, state.label(), detail);
    }

    println!(
        "\n{} mutants evaluated across {} symbols",
        map.total_mutants,
        map.verdicts.len()
    );
    Ok(())
}

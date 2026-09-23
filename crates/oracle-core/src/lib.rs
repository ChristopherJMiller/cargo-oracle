#![warn(missing_docs)]
//! Symbol-level test attribution for Rust: **would any test fail if this symbol
//! stopped working?**
//!
//! Coverage answers *did this line run*. It structurally cannot answer *did
//! anything check the result*. `oracle-core` is the library behind the
//! `cargo oracle` subcommand, and it answers the second question in four
//! independently useful slices.
//!
//! | Slice | Question | Evidence | Cost |
//! |---|---|---|---|
//! | [`lint`] | Can this test's oracles fail at all? | [`syn`] parse | milliseconds, no build |
//! | [`coverage`] | Which symbols run? | `cargo llvm-cov --json` | one instrumented build |
//! | [`attribution`] | Which *test* runs which symbol? | `cargo nextest`, one profile per test | O(tests) |
//! | [`mutation`] | Which test *fails* when a symbol breaks? | `cargo mutants` | one rebuild per mutant |
//!
//! # Symbol identity is a span, not a name
//!
//! Every tool this library joins against reports `file:line:col`, and none of
//! them agree on names — llvm-cov emits mangled, monomorphized symbols;
//! cargo-mutants emits unmangled source names; [`syn`] sees only what was
//! written. So names are never compared. Each report is placed by *containment*
//! in a [`symbol::LineSpan`], which collapses monomorphized instantiations onto
//! their one source definition for free and filters out macro output, which has
//! no source span of its own.
//!
//! ```
//! use oracle_core::symbol::LineSpan;
//!
//! let parse = LineSpan { start: 38, end: 42 };
//!
//! // A coverage entry or mutant reported at line 40 belongs to `parse` --
//! // even a closure defined inside its body.
//! assert!(parse.contains(40));
//! assert!(!parse.contains(43));
//! ```
//!
//! # Honesty about evidence
//!
//! Each layer claims only what its evidence supports. [`lint`] and [`coverage`]
//! never report a symbol as verified. [`mutation`] distinguishes three separate
//! ways to be *unscorable*, none of which mean verified — see
//! [`mutation::Verification`].
//!
//! # Running the whole pipeline
//!
//! ```no_run
//! use oracle_core::{claims::ClaimMap, inventory, lint, report::Report};
//! # fn main() -> anyhow::Result<()> {
//! let inv = inventory::walk_workspace(std::path::Path::new("."))?;
//!
//! // v0: static, no build. Which oracles cannot discriminate?
//! let report = Report::build(&inv);
//! println!("{}", report.to_text(false));
//!
//! // Which symbols does nothing even claim?
//! let claims = ClaimMap::build(&inv);
//! for symbol in claims.unclaimed(&inv, true) {
//!     println!("unclaimed: {}", symbol.path);
//! }
//!
//! // ORC010 needs the claim map and the oracle analysis together.
//! let oracles = lint::analyze(&inv);
//! for finding in lint::shape_mismatches(&inv, &claims, &oracles) {
//!     println!("{} {}", finding.rule.id(), finding.snippet);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! See `examples/` for the same thing as a runnable program.

pub mod attribution;
pub mod claims;
pub mod coverage;
pub mod inventory;
pub mod lint;
pub mod mutation;
pub mod report;
pub mod symbol;

pub use inventory::{Inventory, TestItem, TestKind};
pub use symbol::{Symbol, SymbolId};
